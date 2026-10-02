//! The facade driven through a deterministic in-memory backend, using only
//! this crate's public API — the same path a backend written outside this
//! workspace takes through `Tunnel::new`. No privilege, no real device.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tunnel_lattice::{
    AdminState, Capability, CapabilityProvider, DesiredAdminState, Device, DeviceConfig,
    DeviceConfigPatch, DeviceId, DeviceKind, DeviceMutator, DeviceObserver, DeviceProvider, Error,
    MultiQueueProvider, PacketIo, PersistentDevice, PlatformErrorCode, Result, Tunnel,
};

const ID: DeviceId = DeviceId::new(7);

/// The device's state, shared by every queue and visible to the test.
#[derive(Default)]
struct State {
    mtu: Mutex<u32>,
    admin: Mutex<Option<AdminState>>,
    inbox: Mutex<VecDeque<Vec<u8>>>,
    /// Packets written through `PacketIo::send`.
    sent: Mutex<Vec<Vec<u8>>>,
    /// The length of every batch passed to either `send_batch` override,
    /// so a test can tell the facade reached the device's own method.
    batches: Mutex<Vec<usize>>,
    /// The capacity of every batch passed to `PacketIo::recv_batch`.
    recv_batches: Mutex<Vec<usize>>,
    /// The capacity of every batch passed to `AsyncPacketIo::recv_batch`.
    #[cfg(feature = "async")]
    async_recv_batches: Mutex<Vec<usize>>,
    /// Packets written through `AsyncPacketIo::send`, kept apart so a test
    /// can tell which path a facade method took.
    #[cfg(feature = "async")]
    async_sent: Mutex<Vec<Vec<u8>>>,
    /// Both send paths fail with `Error::Disconnected`.
    fail_send: AtomicBool,
    /// An async send stays `Pending` while this is set.
    #[cfg(feature = "async")]
    hold_async_send: AtomicBool,
    persistent: AtomicBool,
    fail_admin: AtomicBool,
    native_async: bool,
    /// Device queues closed so far.
    closed: AtomicUsize,
}

struct MockBackend {
    state: Arc<State>,
    fail_open: bool,
}

impl MockBackend {
    fn new() -> Self {
        Self::with_state(State::default())
    }

    fn with_state(state: State) -> Self {
        Self {
            state: Arc::new(state),
            fail_open: false,
        }
    }
}

impl CapabilityProvider for MockBackend {
    fn capabilities(&self) -> Capability {
        Capability::DEVICE_MUTATION | Capability::TAP_DEVICES
    }
}

impl DeviceProvider for MockBackend {
    type DeviceConfig = DeviceConfig;
    type Device = MockDevice;

    fn open(&self, config: DeviceConfig) -> Result<MockDevice> {
        if self.fail_open {
            return Err(Error::PermissionDenied);
        }
        *self.state.mtu.lock().unwrap() = config.mtu.unwrap_or(1500);
        *self.state.admin.lock().unwrap() = Some(AdminState::Down);
        Ok(MockDevice {
            kind: config.kind,
            state: Arc::clone(&self.state),
        })
    }
}

struct MockDevice {
    kind: DeviceKind,
    state: Arc<State>,
}

impl Drop for MockDevice {
    fn drop(&mut self) {
        self.state.closed.fetch_add(1, Ordering::SeqCst);
    }
}

impl PacketIo for MockDevice {
    fn recv(&self, buf: &mut [u8]) -> Result<usize> {
        let packet = self
            .state
            .inbox
            .lock()
            .unwrap()
            .pop_front()
            .ok_or(Error::Disconnected)?;
        let slot = buf.get_mut(..packet.len()).ok_or(Error::BufferTooSmall)?;
        slot.copy_from_slice(&packet);
        Ok(packet.len())
    }

    fn send(&self, buf: &[u8]) -> Result<usize> {
        if self.state.fail_send.load(Ordering::SeqCst) {
            return Err(Error::Disconnected);
        }
        self.state.sent.lock().unwrap().push(buf.to_vec());
        Ok(buf.len())
    }

    /// Records the batch, then sends at most `BATCH_LIMIT` packets: a short
    /// batch the facade must hand back unchanged.
    fn send_batch(&self, packets: &[&[u8]]) -> Result<usize> {
        self.state.batches.lock().unwrap().push(packets.len());
        let limit = packets.len().min(BATCH_LIMIT);
        for packet in &packets[..limit] {
            PacketIo::send(self, packet)?;
        }
        Ok(limit)
    }

    fn recv_batch(&self, bufs: &mut [&mut [u8]], lens: &mut [usize]) -> Result<usize> {
        self.state
            .recv_batches
            .lock()
            .unwrap()
            .push(bufs.len().min(lens.len()));
        self.take_batch(bufs, lens)
    }
}

impl MockDevice {
    /// Follows `recv_batch`'s contract: takes every queued packet that fits,
    /// up to the capacity, and returns a short batch rather than waiting.
    /// A packet too large for `bufs[0]` is discarded as `BufferTooSmall`;
    /// one too large for a later buffer stays queued. An empty inbox after
    /// `k >= 1` packets gives `Ok(k)`, and the next call `Disconnected`.
    fn take_batch(&self, bufs: &mut [&mut [u8]], lens: &mut [usize]) -> Result<usize> {
        let capacity = bufs.len().min(lens.len());
        let mut inbox = self.state.inbox.lock().unwrap();
        let mut received = 0;
        while received < capacity {
            let Some(packet) = inbox.front() else {
                break;
            };
            let Some(slot) = bufs[received].get_mut(..packet.len()) else {
                if received == 0 {
                    inbox.pop_front();
                    return Err(Error::BufferTooSmall);
                }
                break;
            };
            slot.copy_from_slice(packet);
            lens[received] = packet.len();
            inbox.pop_front();
            received += 1;
        }
        if received == 0 && capacity > 0 {
            return Err(Error::Disconnected);
        }
        Ok(received)
    }
}

/// The most packets one mock `send_batch` call sends.
const BATCH_LIMIT: usize = 2;

impl DeviceObserver for MockDevice {
    type Device = Device;

    fn snapshot(&self) -> Result<Device> {
        Ok(Device::new(
            ID,
            "mock0".to_owned(),
            self.kind,
            *self.state.mtu.lock().unwrap(),
            self.state
                .admin
                .lock()
                .unwrap()
                .unwrap_or(AdminState::Unknown),
        ))
    }

    fn id(&self) -> DeviceId {
        ID
    }
}

/// Follows `DeviceMutator::apply`'s documented contract.
impl DeviceMutator for MockDevice {
    type DeviceConfigPatch = DeviceConfigPatch;

    fn apply(&self, patch: DeviceConfigPatch) -> Result<()> {
        if patch.device_id() != self.id() {
            return Err(Error::InvalidState);
        }
        let previous = *self.state.mtu.lock().unwrap();
        if let Some(mtu) = patch.mtu() {
            *self.state.mtu.lock().unwrap() = mtu;
        }
        if let Some(admin) = patch.admin_state() {
            if self.state.fail_admin.load(Ordering::SeqCst) {
                *self.state.mtu.lock().unwrap() = previous;
                return Err(Error::Platform(PlatformErrorCode::Unknown));
            }
            *self.state.admin.lock().unwrap() = Some(match admin {
                DesiredAdminState::Up => AdminState::Up,
                _ => AdminState::Down,
            });
        }
        Ok(())
    }
}

impl CapabilityProvider for MockDevice {
    fn capabilities(&self) -> Capability {
        let base =
            Capability::DEVICE_MUTATION | Capability::PERSISTENT_DEVICES | Capability::MULTI_QUEUE;
        if self.state.native_async {
            base | Capability::NATIVE_ASYNC
        } else {
            base
        }
    }
}

impl PersistentDevice for MockDevice {
    fn persist(&self) -> Result<()> {
        self.state.persistent.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn unpersist(&self) -> Result<()> {
        self.state.persistent.store(false, Ordering::SeqCst);
        Ok(())
    }
}

impl MultiQueueProvider for MockDevice {
    fn additional_queue(&self) -> Result<Self> {
        Ok(MockDevice {
            kind: self.kind,
            state: Arc::clone(&self.state),
        })
    }
}

#[cfg(feature = "async")]
impl tunnel_lattice::AsyncPacketIo for MockDevice {
    fn recv(&self, buf: &mut [u8]) -> impl Future<Output = Result<usize>> + Send {
        let result = PacketIo::recv(self, buf);
        async move { result }
    }

    /// Writes only when polled, and only once `hold_async_send` is clear, so
    /// a future dropped while `Pending` has sent nothing. Borrows `buf`
    /// instead of copying it up front.
    fn send(&self, buf: &[u8]) -> impl Future<Output = Result<usize>> + Send {
        let state = &self.state;
        std::future::poll_fn(move |_| {
            if state.hold_async_send.load(Ordering::SeqCst) {
                return std::task::Poll::Pending;
            }
            if state.fail_send.load(Ordering::SeqCst) {
                return std::task::Poll::Ready(Err(Error::Disconnected));
            }
            state.async_sent.lock().unwrap().push(buf.to_vec());
            std::task::Poll::Ready(Ok(buf.len()))
        })
    }

    /// Records the batch, then sends at most `BATCH_LIMIT` packets through
    /// the async `send`, like the sync override.
    async fn send_batch(&self, packets: &[&[u8]]) -> Result<usize>
    where
        Self: Sync,
    {
        self.state.batches.lock().unwrap().push(packets.len());
        let limit = packets.len().min(BATCH_LIMIT);
        for packet in &packets[..limit] {
            tunnel_lattice::AsyncPacketIo::send(self, packet).await?;
        }
        Ok(limit)
    }

    /// Records the batch, then takes packets like the sync override.
    async fn recv_batch(&self, bufs: &mut [&mut [u8]], lens: &mut [usize]) -> Result<usize>
    where
        Self: Sync,
    {
        self.state
            .async_recv_batches
            .lock()
            .unwrap()
            .push(bufs.len().min(lens.len()));
        self.take_batch(bufs, lens)
    }
}

#[test]
fn open_applies_the_config_and_capabilities_come_from_the_backend() {
    let tunnel = Tunnel::new(MockBackend::new());
    assert_eq!(
        tunnel.capabilities(),
        Capability::DEVICE_MUTATION | Capability::TAP_DEVICES
    );

    let handle = tunnel
        .open(DeviceConfig::new(DeviceKind::Tap).with_mtu(1400))
        .expect("open");
    let snapshot = handle.snapshot().expect("snapshot");
    assert_eq!(snapshot.mtu, 1400);
    assert_eq!(snapshot.kind, DeviceKind::Tap);
    assert!(handle.capabilities().contains(Capability::MULTI_QUEUE));
}

#[test]
fn an_open_error_reaches_the_caller_unchanged() {
    let backend = MockBackend {
        fail_open: true,
        ..MockBackend::new()
    };
    let result = Tunnel::new(backend).open(DeviceConfig::new(DeviceKind::Tun));
    assert!(matches!(result, Err(Error::PermissionDenied)));
}

#[test]
fn handle_identity_matches_the_snapshot_and_the_requested_kind() {
    let tunnel = Tunnel::new(MockBackend::new());
    for kind in [DeviceKind::Tun, DeviceKind::Tap] {
        let handle = tunnel.open(DeviceConfig::new(kind)).expect("open");
        let snapshot = handle.snapshot().expect("snapshot");
        assert_eq!(handle.id(), snapshot.id);
        assert_eq!(handle.kind(), kind);
        assert_eq!(snapshot.kind, kind);
    }
}

#[test]
fn send_and_recv_reach_the_device() {
    let backend = MockBackend::new();
    let state = Arc::clone(&backend.state);
    let handle = Tunnel::new(backend)
        .open(DeviceConfig::new(DeviceKind::Tun))
        .expect("open");

    assert_eq!(handle.send(b"out").expect("send"), 3);
    assert_eq!(*state.sent.lock().unwrap(), [b"out".to_vec()]);

    state.inbox.lock().unwrap().push_back(b"in".to_vec());
    let mut buf = [0u8; 8];
    assert_eq!(handle.recv(&mut buf).expect("recv"), 2);
    assert_eq!(&buf[..2], b"in");

    state.inbox.lock().unwrap().push_back(b"too long".to_vec());
    assert!(matches!(
        handle.recv(&mut [0u8; 4]),
        Err(Error::BufferTooSmall)
    ));
}

const BATCH: [&[u8]; 3] = [b"one", b"two", b"three"];

#[test]
fn send_batch_passes_through_to_the_device_including_a_short_batch() {
    let backend = MockBackend::new();
    let state = Arc::clone(&backend.state);
    let handle = Tunnel::new(backend)
        .open(DeviceConfig::new(DeviceKind::Tun).with_offload(true))
        .expect("open");

    assert_eq!(handle.send_batch(&BATCH).expect("send_batch"), BATCH_LIMIT);
    assert_eq!(*state.batches.lock().unwrap(), [3]);
    assert_eq!(*state.sent.lock().unwrap(), BATCH[..BATCH_LIMIT]);

    assert_eq!(handle.send_batch(&BATCH[BATCH_LIMIT..]).expect("rest"), 1);
    assert_eq!(*state.sent.lock().unwrap(), BATCH);

    state.fail_send.store(true, Ordering::SeqCst);
    assert!(matches!(
        handle.send_batch(&BATCH),
        Err(Error::Disconnected)
    ));
    assert_eq!(*state.batches.lock().unwrap(), [3, 1, 3]);
}

/// Receive buffers of 8 bytes each.
fn recv_storage() -> [[u8; 8]; 4] {
    [[0; 8]; 4]
}

#[test]
fn recv_batch_passes_through_to_the_device_including_a_short_batch() {
    let backend = MockBackend::new();
    let state = Arc::clone(&backend.state);
    let handle = Tunnel::new(backend)
        .open(DeviceConfig::new(DeviceKind::Tun))
        .expect("open");
    state
        .inbox
        .lock()
        .unwrap()
        .extend(BATCH.iter().map(|packet| packet.to_vec()));
    let mut storage = recv_storage();
    let mut bufs: Vec<&mut [u8]> = storage.iter_mut().map(|buf| &mut buf[..]).collect();
    let mut lens = [0usize; 4];

    // Capacity is the shorter of the two slices.
    assert_eq!(
        handle.recv_batch(&mut bufs, &mut lens[..2]).expect("full"),
        2
    );
    assert_eq!(
        (&bufs[0][..lens[0]], &bufs[1][..lens[1]]),
        (BATCH[0], BATCH[1])
    );

    // Fewer packets queued than the capacity: a short batch, no waiting.
    assert_eq!(handle.recv_batch(&mut bufs, &mut lens).expect("short"), 1);
    assert_eq!(&bufs[0][..lens[0]], BATCH[2]);

    // Nothing left: the device's error, with nothing received.
    assert!(matches!(
        handle.recv_batch(&mut bufs, &mut lens),
        Err(Error::Disconnected)
    ));

    // Zero capacity still reaches the device, which returns `Ok(0)`.
    assert_eq!(handle.recv_batch(&mut [], &mut lens).expect("empty"), 0);
    assert_eq!(*state.recv_batches.lock().unwrap(), [2, 4, 4, 0]);
}

#[test]
fn a_recv_batch_failure_after_k_packets_reaches_the_next_call() {
    let backend = MockBackend::new();
    let state = Arc::clone(&backend.state);
    let handle = Tunnel::new(backend)
        .open(DeviceConfig::new(DeviceKind::Tun))
        .expect("open");
    state
        .inbox
        .lock()
        .unwrap()
        .extend([b"one".to_vec(), b"too long!".to_vec(), b"two".to_vec()]);
    let mut storage = recv_storage();
    let mut bufs: Vec<&mut [u8]> = storage.iter_mut().map(|buf| &mut buf[..]).collect();
    let mut lens = [0usize; 4];

    // The oversize packet at position 1 ends the batch and stays queued.
    assert_eq!(handle.recv_batch(&mut bufs, &mut lens).expect("prefix"), 1);
    assert_eq!(&bufs[0][..lens[0]], b"one");
    // The next call reports it at position 0 and discards it.
    assert!(matches!(
        handle.recv_batch(&mut bufs, &mut lens),
        Err(Error::BufferTooSmall)
    ));
    assert_eq!(handle.recv_batch(&mut bufs, &mut lens).expect("next"), 1);
    assert_eq!(&bufs[0][..lens[0]], b"two");
}

#[test]
fn apply_is_observable_on_the_next_snapshot() {
    let handle = Tunnel::new(MockBackend::new())
        .open(DeviceConfig::new(DeviceKind::Tun))
        .expect("open");
    let patch = DeviceConfigPatch::new(handle.id(), Some(DesiredAdminState::Up), Some(1280))
        .expect("patch");
    handle.apply(patch).expect("apply");

    let snapshot = handle.snapshot().expect("snapshot");
    assert_eq!(snapshot.mtu, 1280);
    assert_eq!(snapshot.admin_state, AdminState::Up);
}

#[test]
fn an_empty_patch_is_rejected_before_it_reaches_the_device() {
    assert!(matches!(
        DeviceConfigPatch::new(ID, None, None),
        Err(Error::InvalidState)
    ));
    assert!(matches!(
        DeviceConfigPatch::new(ID, None, Some(0)),
        Err(Error::InvalidState)
    ));
}

#[test]
fn apply_rejects_a_patch_for_another_device_and_changes_nothing() {
    let handle = Tunnel::new(MockBackend::new())
        .open(DeviceConfig::new(DeviceKind::Tun).with_mtu(1500))
        .expect("open");
    let patch = DeviceConfigPatch::new(DeviceId::new(8), None, Some(1280)).expect("patch");
    assert!(matches!(handle.apply(patch), Err(Error::InvalidState)));
    assert_eq!(handle.snapshot().expect("snapshot").mtu, 1500);
}

#[test]
fn a_failed_apply_returns_the_original_error_and_snapshot_is_authoritative() {
    let backend = MockBackend::new();
    backend.state.fail_admin.store(true, Ordering::SeqCst);
    let handle = Tunnel::new(backend)
        .open(DeviceConfig::new(DeviceKind::Tun).with_mtu(1500))
        .expect("open");
    let patch = DeviceConfigPatch::new(handle.id(), Some(DesiredAdminState::Up), Some(1280))
        .expect("patch");
    assert!(matches!(
        handle.apply(patch),
        Err(Error::Platform(PlatformErrorCode::Unknown))
    ));
    let snapshot = handle.snapshot().expect("snapshot");
    assert_eq!(snapshot.mtu, 1500, "the MTU step was reverted");
    assert_eq!(snapshot.admin_state, AdminState::Down);
}

#[test]
fn clones_share_one_device_that_closes_after_the_last_drop() {
    let backend = MockBackend::new();
    let state = Arc::clone(&backend.state);
    let handle = Tunnel::new(backend)
        .open(DeviceConfig::new(DeviceKind::Tun))
        .expect("open");
    let clone = handle.clone();
    assert_eq!((clone.id(), clone.kind()), (handle.id(), handle.kind()));

    clone.send(b"via clone").expect("send");
    assert_eq!(state.sent.lock().unwrap().len(), 1);

    drop(handle);
    assert_eq!(state.closed.load(Ordering::SeqCst), 0, "a clone is alive");
    drop(clone);
    assert_eq!(state.closed.load(Ordering::SeqCst), 1);
}

#[test]
fn an_additional_queue_keeps_the_identity_and_closes_independently() {
    let backend = MockBackend::new();
    let state = Arc::clone(&backend.state);
    let handle = Tunnel::new(backend)
        .open(DeviceConfig::new(DeviceKind::Tap).with_multi_queue(true))
        .expect("open");
    let queue = handle.additional_queue().expect("queue");
    assert_eq!((queue.id(), queue.kind()), (handle.id(), DeviceKind::Tap));

    drop(queue);
    assert_eq!(state.closed.load(Ordering::SeqCst), 1);
    handle.snapshot().expect("the first queue still works");
}

#[test]
fn persist_reaches_the_device() {
    let backend = MockBackend::new();
    let state = Arc::clone(&backend.state);
    let handle = Tunnel::new(backend)
        .open(DeviceConfig::new(DeviceKind::Tun))
        .expect("open");
    handle.persist().expect("persist");
    assert!(state.persistent.load(Ordering::SeqCst));
}

#[test]
fn unpersist_reaches_the_device_from_any_queue_and_is_idempotent() {
    let backend = MockBackend::new();
    let state = Arc::clone(&backend.state);
    let handle = Tunnel::new(backend)
        .open(DeviceConfig::new(DeviceKind::Tun).with_multi_queue(true))
        .expect("open");
    let queue = handle.additional_queue().expect("second queue");
    handle.persist().expect("persist");

    queue.unpersist().expect("unpersist from the second queue");
    assert!(!state.persistent.load(Ordering::SeqCst));
    handle.unpersist().expect("unpersist again");
    assert!(!state.persistent.load(Ordering::SeqCst));
}

#[cfg(feature = "async")]
#[test]
fn packet_stream_works_on_both_dispatch_branches() {
    use futures::StreamExt;

    for native_async in [true, false] {
        let backend = MockBackend::with_state(State {
            native_async,
            ..State::default()
        });
        let state = Arc::clone(&backend.state);
        state.inbox.lock().unwrap().push_back(b"pkt".to_vec());
        let handle = Tunnel::new(backend)
            .open(DeviceConfig::new(DeviceKind::Tun))
            .expect("open");
        assert_eq!(
            handle.capabilities().contains(Capability::NATIVE_ASYNC),
            native_async
        );

        let mut stream = handle.packet_stream(16).expect("a valid buf_len");
        let packet = futures::executor::block_on(stream.next())
            .expect("an item")
            .expect("a packet");
        assert_eq!(*packet, *b"pkt", "native_async: {native_async}");
        drop(packet);
        assert!(matches!(
            futures::executor::block_on(stream.next()),
            Some(Err(Error::Disconnected))
        ));
        assert!(futures::executor::block_on(stream.next()).is_none());
    }
}

/// Opens a TUN handle on a mock that reports `NATIVE_ASYNC` or not.
#[cfg(feature = "async")]
fn async_handle(native_async: bool) -> (tunnel_lattice::Handle<MockDevice>, Arc<State>) {
    let backend = MockBackend::with_state(State {
        native_async,
        ..State::default()
    });
    let state = Arc::clone(&backend.state);
    let handle = Tunnel::new(backend)
        .open(DeviceConfig::new(DeviceKind::Tun))
        .expect("open");
    (handle, state)
}

#[cfg(feature = "async")]
#[test]
fn send_async_takes_the_async_path_whatever_the_capabilities_say() {
    for native_async in [true, false] {
        let (handle, state) = async_handle(native_async);
        assert_eq!(
            handle.capabilities().contains(Capability::NATIVE_ASYNC),
            native_async
        );

        let sent = futures::executor::block_on(handle.send_async(b"async"));
        assert_eq!(sent.expect("send_async"), 5, "native_async: {native_async}");
        assert_eq!(*state.async_sent.lock().unwrap(), [b"async".to_vec()]);
        assert!(state.sent.lock().unwrap().is_empty(), "not PacketIo::send");

        handle.send(b"sync").expect("send");
        assert_eq!(*state.sent.lock().unwrap(), [b"sync".to_vec()]);
        assert_eq!(
            state.async_sent.lock().unwrap().len(),
            1,
            "not AsyncPacketIo::send"
        );
    }
}

#[cfg(feature = "async")]
#[test]
fn a_send_async_error_reaches_the_caller_unchanged() {
    let (handle, state) = async_handle(true);
    state.fail_send.store(true, Ordering::SeqCst);
    assert!(matches!(
        futures::executor::block_on(handle.send_async(b"pkt")),
        Err(Error::Disconnected)
    ));
    assert!(matches!(handle.send(b"pkt"), Err(Error::Disconnected)));
    assert!(state.async_sent.lock().unwrap().is_empty());

    state.fail_send.store(false, Ordering::SeqCst);
    assert_eq!(
        futures::executor::block_on(handle.send_async(b"pkt")).expect("send_async"),
        3
    );
}

#[cfg(feature = "async")]
#[test]
fn send_batch_async_passes_through_to_the_async_device_method() {
    let (handle, state) = async_handle(true);

    let sent = futures::executor::block_on(handle.send_batch_async(&BATCH));
    assert_eq!(sent.expect("send_batch_async"), BATCH_LIMIT);
    assert_eq!(*state.batches.lock().unwrap(), [3]);
    assert_eq!(*state.async_sent.lock().unwrap(), BATCH[..BATCH_LIMIT]);
    assert!(state.sent.lock().unwrap().is_empty(), "not PacketIo");

    state.fail_send.store(true, Ordering::SeqCst);
    assert!(matches!(
        futures::executor::block_on(handle.send_batch_async(&BATCH)),
        Err(Error::Disconnected)
    ));
    assert_eq!(state.async_sent.lock().unwrap().len(), BATCH_LIMIT);
}

#[cfg(feature = "async")]
#[test]
fn recv_batch_async_passes_through_to_the_async_device_method() {
    let (handle, state) = async_handle(true);
    state
        .inbox
        .lock()
        .unwrap()
        .extend(BATCH.iter().map(|packet| packet.to_vec()));
    let mut storage = recv_storage();
    let mut bufs: Vec<&mut [u8]> = storage.iter_mut().map(|buf| &mut buf[..]).collect();
    let mut lens = [0usize; 4];

    let received = futures::executor::block_on(handle.recv_batch_async(&mut bufs, &mut lens));
    assert_eq!(received.expect("short batch"), 3);
    for (i, packet) in BATCH.iter().enumerate() {
        assert_eq!(&bufs[i][..lens[i]], *packet);
    }
    assert!(matches!(
        futures::executor::block_on(handle.recv_batch_async(&mut bufs, &mut lens)),
        Err(Error::Disconnected)
    ));
    assert_eq!(
        futures::executor::block_on(handle.recv_batch_async(&mut [], &mut lens)).expect("empty"),
        0
    );
    assert_eq!(*state.async_recv_batches.lock().unwrap(), [4, 4, 0]);
    assert!(
        state.recv_batches.lock().unwrap().is_empty(),
        "not PacketIo"
    );
}

#[cfg(feature = "async")]
#[test]
fn the_recv_batch_async_future_is_send_and_runs_on_another_thread() {
    fn assert_send<T: Send>(value: T) -> T {
        value
    }

    let (handle, state) = async_handle(true);
    state.inbox.lock().unwrap().push_back(b"moved".to_vec());
    let mut storage = recv_storage();
    let mut bufs: Vec<&mut [u8]> = storage.iter_mut().map(|buf| &mut buf[..]).collect();
    let mut lens = [0usize; 4];
    let future = assert_send(handle.recv_batch_async(&mut bufs, &mut lens));
    let received = std::thread::scope(|scope| {
        scope
            .spawn(move || futures::executor::block_on(future))
            .join()
            .expect("the receiving thread does not panic")
    });
    assert_eq!(received.expect("recv_batch_async"), 1);
    assert_eq!(&bufs[0][..lens[0]], b"moved");
}

#[cfg(feature = "async")]
#[test]
fn the_send_async_future_is_send_and_runs_on_another_thread() {
    fn assert_send<T: Send>(value: T) -> T {
        value
    }

    let (handle, state) = async_handle(true);
    let clone = handle.clone();
    let packet = b"moved".to_vec();
    // Built on this thread, polled to completion on another one.
    let future = assert_send(clone.send_async(&packet));
    let sent = std::thread::scope(|scope| {
        scope
            .spawn(move || futures::executor::block_on(future))
            .join()
            .expect("the sending thread does not panic")
    });
    assert_eq!(sent.expect("send_async"), 5);
    assert_eq!(*state.async_sent.lock().unwrap(), [b"moved".to_vec()]);
}

#[cfg(feature = "async")]
#[test]
fn dropping_a_pending_send_async_sends_nothing_and_the_handle_stays_usable() {
    use std::task::{Context, Waker};

    let (handle, state) = async_handle(true);
    state.hold_async_send.store(true, Ordering::SeqCst);
    {
        let mut future = std::pin::pin!(handle.send_async(b"dropped"));
        let mut cx = Context::from_waker(Waker::noop());
        assert!(future.as_mut().poll(&mut cx).is_pending());
    }
    assert!(state.async_sent.lock().unwrap().is_empty());
    assert!(state.sent.lock().unwrap().is_empty());

    state.hold_async_send.store(false, Ordering::SeqCst);
    let sent = futures::executor::block_on(handle.send_async(b"next"));
    assert_eq!(sent.expect("send_async"), 4);
    assert_eq!(*state.async_sent.lock().unwrap(), [b"next".to_vec()]);
    assert_eq!(
        state.closed.load(Ordering::SeqCst),
        0,
        "the device is still open"
    );
}
