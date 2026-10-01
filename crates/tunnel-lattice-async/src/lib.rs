//! Runtime-agnostic async adapters for Tunnel Lattice.
//!
//! Two ways to get a `futures::Stream` of received packets, both producing
//! the same [`PacketStream`] type, whose items are [`PacketBuf`] views into
//! a [`PacketPool`]:
//!
//! - [`from_async_device`] wraps a backend's own
//!   [`tunnel_lattice_platform::AsyncPacketIo`] directly — no worker thread,
//!   no polling loop. Dropping the stream drops the in-flight `recv` future,
//!   which is genuinely, immediately cancelled the same way dropping any
//!   other future is; there is nothing left running to leak. Prefer this
//!   whenever the backend reports `Capability::NATIVE_ASYNC`.
//! - [`from_device`] bridges a synchronous
//!   [`tunnel_lattice_platform::PacketIo`] device instead, mirroring
//!   `net-lattice-async::from_receiver`'s worker-thread bridge: it spawns
//!   one blocking worker thread per device, since a device's blocking
//!   `recv` has no waker-registration mechanism a direct `Stream`
//!   implementation could poll. This is the fallback for a backend with no
//!   native async path at all — see [`PacketStream`]'s docs for the real
//!   shutdown limitation this variant has that `from_async_device` does not.
//!
//! Both build a [`PacketPool`] sized for `buf_len` and receive every packet
//! straight into one of its slots, so the steady state allocates nothing and
//! copies nothing at this layer. [`from_async_device_with_pool`] and
//! [`from_device_with_pool`] take a pool you built instead, for example to
//! choose the slot count or to share one pool between several streams.
//!
//! No Tokio, async-std, or smol dependency is imposed by this crate itself.

#![warn(missing_docs)]

mod pool;

pub use pool::{PacketBuf, PacketPool};

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, Waker};
use std::thread;

use futures::Stream;
pub use tunnel_lattice_core::{Error, Result};
use tunnel_lattice_platform::{AsyncPacketIo, PacketIo};

use pool::{Acquirer, SlotGuard, drop_guarded};

/// A runtime-agnostic asynchronous stream of received packets.
///
/// Constructed by [`from_async_device`] / [`from_async_device_with_pool`]
/// (native async, genuinely cancellable on drop) or [`from_device`] /
/// [`from_device_with_pool`] (thread bridge, best-effort shutdown only) —
/// see each function's docs for which to prefer.
///
/// # Items
///
/// Each `Ok` item is exactly one packet, as [`PacketIo::recv`] and
/// [`AsyncPacketIo::recv`] frame it: one raw IP packet for a TUN device, or
/// one full Ethernet frame for a TAP device. It never carries a prefix
/// header, is never several packets coalesced into one, and is never a
/// fragment of one. Its length is at most the `buf_len` of the stream's
/// [`PacketPool`], which is the size of the buffer each `recv` receives
/// into.
///
/// An item is a [`PacketBuf`]: a view into one slot of the stream's pool.
/// It dereferences to `[u8]`; call `to_vec()` for an owned copy. The slot
/// returns to the pool when the item is dropped.
///
/// # Back-pressure
///
/// The stream receives only while its pool has a free slot. Items the
/// consumer still holds keep their slots (and so do items the thread bridge
/// has received but the consumer has not polled yet); once every slot is
/// taken, the stream returns `Poll::Pending` without calling `recv` until an
/// item is dropped. Running out of slots is never an error. A stream that is
/// the only user of its pool always makes progress once its consumer drops
/// an item; streams that share a pool through
/// [`from_async_device_with_pool`] or [`from_device_with_pool`] share its
/// slots and can starve each other.
///
/// # End of life
///
/// Both variants behave the same way on errors:
///
/// - A packet larger than the receive buffer yields
///   `Err(Error::BufferTooSmall)` (the packet is discarded, never
///   truncated) and the stream keeps receiving. This is the only
///   non-terminal error.
/// - Every other error is yielded once, and then the stream ends
///   (`poll_next` returns `Poll::Ready(None)`, and keeps returning it if
///   polled again). This includes
///   `Err(Error::Disconnected)` (the device is gone for good) and errors a
///   caller can recover from, such as `Err(Error::InvalidState)` for an
///   administratively disabled interface. After recovering, create a new
///   stream (for a facade handle, `Handle::packet_stream` again).
///
/// Ending on the first such error keeps a device whose `recv` fails
/// immediately and repeatedly (for example after the interface was deleted
/// or disabled) from turning the stream into a busy loop of error items,
/// and keeps the [`from_device`] worker from queueing them:
/// that worker exits right after forwarding the error. A worker thread that
/// panics (for example inside a device's `recv`) also ends the stream.
///
/// The stream never yields `Err(Error::InvalidState)` for its own reasons: a
/// rejected `buf_len` is reported only by the constructor. With the tun-rs
/// backend, an `InvalidState` item means the Windows (Wintun) interface is
/// currently disabled, which is recoverable; other backends define their
/// own meaning.
///
/// Transient native conditions (a signal interrupting the call, for
/// example) are retried inside a conforming backend's `recv` and never
/// reach the stream.
///
/// # Panics in the consumer's waker
///
/// If cloning the consumer's own `Waker` panics during `poll_next`, the
/// panic propagates out of that call. A thread-bridge stream stays usable
/// afterwards. A native stream panics on every later poll and must be
/// dropped; dropping it returns its slot to the pool. Wakers run from any
/// other thread (the bridge worker, or a release on another stream's
/// thread) are called with their panics caught and discarded, so they never
/// end a stream.
pub struct PacketStream {
    inner: Inner,
    /// Set once the stream has ended; `poll_next` then returns `None`
    /// without polling `inner` again.
    done: bool,
}

enum Inner {
    /// Backed by a boxed native async stream built directly from
    /// `AsyncPacketIo::recv` — dropping this drops the in-flight future,
    /// which is complete, immediate cancellation. No worker thread exists
    /// for this variant.
    Native(Pin<Box<dyn Stream<Item = Result<PacketBuf>> + Send>>),
    /// Backed by a worker thread bridging a blocking `PacketIo::recv` loop —
    /// see [`from_device`]'s docs for its shutdown limitation.
    ThreadBridge {
        rx: Receiver<Result<PacketBuf>>,
        waker: Arc<BridgeWaker>,
        stop: Arc<AtomicBool>,
        pool: PacketPool,
    },
}

/// Whether `e` ends a stream. Every error except `BufferTooSmall` does.
///
/// The only place terminality is decided: called by the native path's
/// switch to its final state, by the bridge worker's exit after sending,
/// and by `poll_next` for the bridge (so the bridge ends right after a
/// terminal item, whatever the worker's timing).
#[inline]
fn is_terminal(e: &Error) -> bool {
    !matches!(e, Error::BufferTooSmall)
}

/// Maps one `recv` result on `slot` to one stream item.
///
/// `Ok(n)` becomes a view of the first `n` bytes of the slot. A length past
/// the slot's `buf_len` (which a conforming device never reports) becomes
/// `Err(BufferTooSmall)` and releases the slot, as does any error.
#[inline(always)]
fn map_recv(slot: SlotGuard, result: Result<usize>) -> Result<PacketBuf> {
    match result {
        Ok(n) => slot.into_buf(n).ok_or(Error::BufferTooSmall),
        Err(e) => {
            drop(slot);
            Err(e)
        }
    }
}

/// Wraps a backend's native [`AsyncPacketIo`] as a [`PacketStream`] — no
/// worker thread, no polling loop.
///
/// Builds the stream's pool with [`PacketPool::with_buf_len`]. `buf_len` is
/// the per-packet receive buffer size; for a device opened through the
/// `tunnel-lattice` facade, use `handle.snapshot()?.recv_buffer_len()`,
/// which fits one packet at the device's current MTU (Ethernet framing
/// included for TAP). A packet larger than `buf_len` is never truncated: it
/// is discarded and the stream yields `Err(Error::BufferTooSmall)`, then
/// keeps receiving. A non-conforming device that reports more bytes than
/// `buf_len` is treated the same way. Every other error is yielded once and
/// then the stream ends; see [`PacketStream`], "End of life".
///
/// Dropping the returned stream drops whatever `AsyncPacketIo::recv` future
/// is currently in flight, the same way dropping any other future cancels
/// it — this genuinely stops the operation immediately, unlike
/// [`from_device`]'s worker thread, which can outlive the stream that
/// spawned it. Prefer this over `from_device` whenever the backend reports
/// `Capability::NATIVE_ASYNC`; `tunnel_lattice::Handle::packet_stream`
/// already does this automatically.
///
/// # Errors
///
/// [`Error::InvalidState`] if [`PacketPool::with_buf_len`] rejects
/// `buf_len` (zero, or too large for a pool slot). This is checked before
/// anything is allocated and without touching the device; `device` is
/// dropped. There is no other error.
///
/// # Aborts
///
/// If the pool's slab allocation fails, as [`PacketPool::new`] does.
pub fn from_async_device<D>(device: Arc<D>, buf_len: usize) -> Result<PacketStream>
where
    D: AsyncPacketIo + Send + Sync + 'static,
{
    let pool = PacketPool::with_buf_len(buf_len)?;
    Ok(from_async_device_with_pool(device, pool))
}

/// Like [`from_async_device`], but receives into `pool`, which may be
/// shared with other streams (see [`PacketStream`], "Back-pressure").
///
/// Every packet is received into one of `pool`'s slots, so `pool.buf_len()`
/// is the receive buffer size and bounds every item's length. Any valid
/// pool is accepted.
pub fn from_async_device_with_pool<D>(device: Arc<D>, pool: PacketPool) -> PacketStream
where
    D: AsyncPacketIo + Send + Sync + 'static,
{
    enum State<D> {
        Live(Arc<D>, Acquirer),
        Done,
    }

    // The stream's only acquirer; exclusive if `pool` is the only handle.
    let acq = Acquirer::new(pool);
    let stream = futures::stream::unfold(State::Live(device, acq), |state| async move {
        let State::Live(device, mut acq) = state else {
            return None;
        };
        let mut slot = match acq.try_acquire() {
            Some(slot) => slot,
            None => acq.acquire().await,
        };
        let result = device.recv(slot.as_mut_slice()).await;
        let item = map_recv(slot, result);
        // Terminal: yield the error, then end (dropping the device).
        let next = if item.as_ref().is_err_and(is_terminal) {
            State::Done
        } else {
            State::Live(device, acq)
        };
        Some((item, next))
    });
    PacketStream {
        inner: Inner::Native(Box::pin(stream)),
        done: false,
    }
}

/// Bridges a synchronous [`PacketIo`] device to a waker-aware stream of
/// received packets.
///
/// Builds the stream's pool with [`PacketPool::with_buf_len`]. `buf_len`
/// has the same meaning, and the stream the same too-small-buffer and
/// end-of-stream behavior, as for [`from_async_device`]: use
/// `handle.snapshot()?.recv_buffer_len()`; an oversize packet yields
/// `Err(Error::BufferTooSmall)` and the stream keeps receiving; every other
/// error is yielded once and then the stream ends (see [`PacketStream`],
/// "End of life"). The worker thread stops calling `recv` and exits as soon
/// as it has forwarded that error.
///
/// The worker receives only while the pool has a free slot and queues at
/// most one item per slot, so a consumer that stops polling stops the
/// worker too instead of letting a queue grow.
///
/// **Shutdown limitation**: dropping the returned stream signals the worker
/// thread to stop but does **not** join it: [`PacketIo::recv`] has no
/// timeout/cancellation contract, so a worker parked inside a blocking
/// `recv` with no further packets arriving cannot be woken by this adapter
/// alone — it exits only once the underlying device itself unblocks the
/// call (a packet arrives, or every other handle to the device is closed
/// and the OS returns an error). A worker waiting for a free slot, or for
/// room to queue an item, exits promptly. Use [`from_async_device`] instead
/// whenever the backend implements `AsyncPacketIo`, which has no such
/// limitation.
///
/// # Errors
///
/// [`Error::InvalidState`] if [`PacketPool::with_buf_len`] rejects
/// `buf_len`. This is checked before anything is allocated and without
/// touching the device; no thread is spawned and `device` is dropped. There
/// is no other error.
///
/// # Panics
///
/// If the OS cannot spawn the worker thread, as [`std::thread::spawn`]
/// does. Nothing is leaked in that case.
///
/// # Aborts
///
/// If the pool's slab allocation fails, as [`PacketPool::new`] does.
pub fn from_device<D>(device: Arc<D>, buf_len: usize) -> Result<PacketStream>
where
    D: PacketIo + Send + Sync + 'static,
{
    let pool = PacketPool::with_buf_len(buf_len)?;
    Ok(from_device_with_pool(device, pool))
}

/// Like [`from_device`], but receives into `pool`, which may be shared with
/// other streams (see [`PacketStream`], "Back-pressure").
///
/// Every packet is received into one of `pool`'s slots, so `pool.buf_len()`
/// is the receive buffer size and bounds every item's length. Any valid
/// pool is accepted. The worker queues at most `pool.slots()` items.
///
/// # Panics
///
/// If the OS cannot spawn the worker thread, as [`std::thread::spawn`]
/// does. Nothing is leaked in that case.
pub fn from_device_with_pool<D>(device: Arc<D>, pool: PacketPool) -> PacketStream
where
    D: PacketIo + Send + Sync + 'static,
{
    // The worker's acquirer is built first, from the caller's handle, so it
    // is exclusive if that was the only one; the stream's own clone (used
    // only to stop the worker) is made afterwards.
    let acq = Acquirer::new(pool);
    let pool = acq.pool().clone();
    let (tx, rx) = sync_channel(pool.slots());
    let waker = Arc::new(BridgeWaker::new());
    let stop = Arc::new(AtomicBool::new(false));
    let worker_waker = Arc::clone(&waker);
    let worker_stop = Arc::clone(&stop);
    thread::spawn(move || {
        let exit = WorkerExit {
            tx: Some(tx),
            waker: Some(worker_waker),
            acq,
            stop: worker_stop,
            device,
        };
        worker(exit);
    });
    PacketStream {
        inner: Inner::ThreadBridge {
            rx,
            waker,
            stop,
            pool,
        },
        done: false,
    }
}

/// The consumer's waker for the thread bridge: a poison-tolerant slot that
/// never runs foreign code (a waker clone, wake, or drop) under its lock.
struct BridgeWaker(Mutex<Option<Waker>>);

impl BridgeWaker {
    fn new() -> Self {
        let this = Self(Mutex::new(None));
        // Eager initialisation: where the std mutex boxes its pthread object
        // lazily (macOS), pay for it here, not on the first packet.
        drop(this.lock());
        this
    }

    fn lock(&self) -> MutexGuard<'_, Option<Waker>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Stores the consumer's current waker. Runs on the consumer's thread,
    /// inside its `poll_next`.
    fn register(&self, waker: &Waker) {
        let waker = waker.clone();
        let mut slot = self.lock();
        let unused = match &mut *slot {
            Some(stored) if stored.will_wake(&waker) => Some(waker),
            other => other.replace(waker),
        };
        drop(slot);
        drop(unused);
    }

    /// Takes the stored waker, if any.
    fn take(&self) -> Option<Waker> {
        self.lock().take()
    }

    /// Wakes the consumer from the worker thread; a panic in the consumer's
    /// waker is caught and discarded.
    fn wake(&self) {
        if let Some(waker) = self.take() {
            let _ = catch_unwind(AssertUnwindSafe(move || waker.wake()));
        }
    }
}

/// Everything the bridge worker owns. Its `Drop` runs on every exit path,
/// including a panic in the device's `recv`: it disconnects the channel,
/// then wakes the consumer so it sees the end.
///
/// The field order matters: `device` drops last, so once the device is
/// released, the sender, the waker slot, and the worker's acquirer (with
/// its pool handle) are already gone.
struct WorkerExit<D> {
    tx: Option<SyncSender<Result<PacketBuf>>>,
    waker: Option<Arc<BridgeWaker>>,
    acq: Acquirer,
    stop: Arc<AtomicBool>,
    device: Arc<D>,
}

impl<D> Drop for WorkerExit<D> {
    fn drop(&mut self) {
        // 1. Disconnect, so the consumer's next `try_recv` sees the end.
        drop(self.tx.take());
        #[cfg(test)]
        if let Some(hooks) = self.acq.pool().hooks() {
            pool::run_hook(&hooks.after_tx_drop);
        }
        // 2. Wake the consumer, and release this `Arc<BridgeWaker>`, which
        //    may be the last one and hold a foreign waker, under a guard.
        let waker = self.waker.take();
        #[cfg(test)]
        let pool = self.acq.pool();
        let _ = catch_unwind(AssertUnwindSafe(move || {
            if let Some(waker) = waker {
                waker.wake();
                #[cfg(test)]
                if let Some(hooks) = pool.hooks() {
                    pool::run_hook(&hooks.after_worker_wake);
                }
                drop(waker);
            }
        }));
        // 3. `acq`, `stop`, and `device` drop after this, in that order.
    }
}

/// The bridge worker loop: waits for a free slot, receives into it, and
/// queues the item, until stopped, disconnected, or after a terminal error.
fn worker<D: PacketIo>(mut exit: WorkerExit<D>) {
    let (Some(tx), Some(waker)) = (&exit.tx, &exit.waker) else {
        return;
    };
    while let Some(mut slot) = exit.acq.acquire_blocking(&exit.stop) {
        let result = exit.device.recv(slot.as_mut_slice());
        let item = map_recv(slot, result);
        let terminal = item.as_ref().is_err_and(is_terminal);
        if tx.send(item).is_err() {
            // The stream was dropped.
            break;
        }
        waker.wake();
        if terminal {
            #[cfg(test)]
            if let Some(hooks) = exit.acq.pool().hooks() {
                pool::run_hook(&hooks.after_terminal_send);
            }
            break;
        }
    }
}

impl Stream for PacketStream {
    type Item = Result<PacketBuf>;

    #[inline]
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.done {
            return Poll::Ready(None);
        }
        let polled = match &mut this.inner {
            Inner::Native(stream) => {
                // The native stream itself switches to its final state
                // after a terminal item and returns `None` on the next poll,
                // so only `None` needs recording here.
                let polled = stream.as_mut().poll_next(cx);
                if let Poll::Ready(None) = polled {
                    this.done = true;
                }
                return polled;
            }
            Inner::ThreadBridge { rx, waker, .. } => {
                waker.register(cx.waker());
                match rx.try_recv() {
                    Ok(item) => Poll::Ready(Some(item)),
                    Err(TryRecvError::Empty) => Poll::Pending,
                    Err(TryRecvError::Disconnected) => Poll::Ready(None),
                }
            }
        };
        // Bridge only: end right after a terminal item, even while the
        // worker has not disconnected yet.
        if let Poll::Ready(item) = &polled
            && item
                .as_ref()
                .is_none_or(|item| item.as_ref().is_err_and(is_terminal))
        {
            this.done = true;
        }
        polled
    }
}

impl Drop for PacketStream {
    fn drop(&mut self) {
        // `Inner::Native` needs no explicit action: dropping `self.inner`
        // right after this drops the boxed stream (and whatever acquire or
        // `AsyncPacketIo::recv` future it was polling, with its slot), which
        // is itself the cancellation — see `from_async_device`'s docs.
        if let Inner::ThreadBridge {
            waker, stop, pool, ..
        } = &self.inner
        {
            // Best-effort only — see `from_device`'s docs on why this
            // cannot join the worker thread. Wakes a worker waiting for a
            // slot; one waiting to queue an item wakes when `rx` drops.
            pool.stop_worker(stop);
            // A waker registered after the worker's final wake would
            // otherwise be dropped unguarded with the last `Arc`.
            if let Some(waker) = waker.take() {
                drop_guarded(waker);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::mem::{Discriminant, discriminant};
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::task::Wake;
    use std::thread::ThreadId;
    use std::time::{Duration, Instant};

    use futures::{FutureExt, StreamExt};

    use super::*;
    use crate::pool::{CloneSwitch, TestHooks};

    const DEADLINE: Duration = Duration::from_secs(60);

    fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
        let start = Instant::now();
        while !cond() {
            assert!(start.elapsed() < DEADLINE, "timed out waiting for {what}");
            thread::yield_now();
        }
    }

    /// Waits until the test holds the only handle to `device`: a bridge
    /// worker releases its handle last, so this means it has exited.
    fn wait_released<T>(device: &Arc<T>) {
        wait_until("the device to be released", || {
            Arc::strong_count(device) == 1
        });
    }

    fn poll_with(stream: &mut PacketStream, waker: &Waker) -> Poll<Option<Result<PacketBuf>>> {
        Pin::new(stream).poll_next(&mut Context::from_waker(waker))
    }

    /// Counts wakes.
    #[derive(Default)]
    struct Counter(AtomicUsize);

    impl Counter {
        fn waker() -> (Arc<Self>, Waker) {
            let counter = Arc::new(Self::default());
            (Arc::clone(&counter), Waker::from(counter))
        }

        fn count(&self) -> usize {
            self.0.load(Ordering::SeqCst)
        }
    }

    impl Wake for Counter {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Panics when woken.
    struct PanicOnWake;

    impl Wake for PanicOnWake {
        fn wake(self: Arc<Self>) {
            panic!("test waker panics on wake");
        }
    }

    /// Panics when its last clone is dropped.
    struct PanicOnDrop;

    impl Wake for PanicOnDrop {
        // Not a no-op waker: dropping `self` here may drop the last clone,
        // which panics.
        fn wake(self: Arc<Self>) {
            drop(self);
        }
    }

    impl Drop for PanicOnDrop {
        fn drop(&mut self) {
            panic!("test waker panics on drop");
        }
    }

    /// Panics when woken from any thread other than `owner`.
    struct PanicOffThread {
        owner: ThreadId,
    }

    impl Wake for PanicOffThread {
        fn wake(self: Arc<Self>) {
            assert_eq!(
                thread::current().id(),
                self.owner,
                "test waker panics when woken off its thread"
            );
        }
    }

    /// What the consumer saw, comparable across paths.
    #[derive(Debug, PartialEq)]
    enum Seen {
        Data(Vec<u8>),
        Fail(Discriminant<Error>),
    }

    fn seen(item: Result<PacketBuf>) -> Seen {
        match item {
            Ok(buf) => Seen::Data(buf.to_vec()),
            Err(e) => Seen::Fail(discriminant(&e)),
        }
    }

    fn fail(e: Error) -> Seen {
        Seen::Fail(discriminant(&e))
    }

    fn data(bytes: &[u8]) -> Seen {
        Seen::Data(bytes.to_vec())
    }

    /// Collects every item until the stream ends, blocking on each (the
    /// thread bridge delivers from another thread), then checks that the
    /// ended stream keeps returning `None`.
    fn collect(mut stream: PacketStream) -> Vec<Seen> {
        let mut items = Vec::new();
        while let Some(item) = futures::executor::block_on(stream.next()) {
            items.push(seen(item));
        }
        assert!(futures::executor::block_on(stream.next()).is_none());
        assert!(futures::executor::block_on(stream.next()).is_none());
        items
    }

    /// Every current `Error` variant.
    fn every_error() -> [fn() -> Error; 9] {
        [
            || Error::PermissionDenied,
            || Error::NotFound,
            || Error::AlreadyExists,
            || Error::Unsupported,
            || Error::InvalidState,
            || Error::Disconnected,
            || Error::DriverUnavailable,
            || Error::BufferTooSmall,
            || Error::Platform(tunnel_lattice_core::PlatformErrorCode::Unknown),
        ]
    }

    /// A mock `AsyncPacketIo` whose `recv` never resolves — models a real
    /// device with no traffic arriving. No real device or privilege needed:
    /// this tests `from_async_device`'s cancellation contract in isolation.
    struct BlocksForever;

    impl AsyncPacketIo for BlocksForever {
        fn recv(&self, _buf: &mut [u8]) -> impl Future<Output = Result<usize>> + Send {
            std::future::pending()
        }

        fn send(&self, _buf: &[u8]) -> impl Future<Output = Result<usize>> + Send {
            std::future::pending()
        }
    }

    #[test]
    fn from_async_device_drop_completes_immediately_even_mid_recv() {
        let mut stream = from_async_device(Arc::new(BlocksForever), 1500).unwrap();

        // Poll once to put the boxed `recv` future in flight (mirrors a
        // real caller awaiting the stream), without a runtime: `now_or_never`
        // polls exactly once and asserts it doesn't complete yet.
        assert!(
            stream.next().now_or_never().is_none(),
            "a stream over a device with no traffic must not resolve immediately"
        );

        // Dropping a boxed future is synchronous and unconditional, so a
        // hang here would mean this test itself never finishes.
        drop(stream);
    }

    /// Test 4: cancelling mid-`recv` returns the slot the `recv` held.
    #[test]
    fn native_cancellation_mid_recv_returns_the_slot() {
        let slots = PacketPool::new(2, 64).unwrap();
        let mut stream = from_async_device_with_pool(Arc::new(BlocksForever), slots.clone());
        assert!(stream.next().now_or_never().is_none());
        assert_eq!(slots.available(), 1, "the in-flight recv holds one slot");
        drop(stream);
        assert_eq!(slots.available(), 2);
    }

    /// One scripted `recv` outcome.
    #[derive(Clone, Copy)]
    enum Step {
        /// A packet that fits.
        Packet(&'static [u8]),
        /// A non-conforming device reporting one byte more than the buffer.
        Overlong,
        /// An error.
        Fail(fn() -> Error),
    }

    const TOO_SMALL: Step = Step::Fail(|| Error::BufferTooSmall);
    const DISCONNECTED: Step = Step::Fail(|| Error::Disconnected);
    const INVALID_STATE: Step = Step::Fail(|| Error::InvalidState);

    /// A mock device implementing both `PacketIo` and `AsyncPacketIo`,
    /// replaying `steps` in order; each resolves immediately.
    struct Scripted {
        steps: Mutex<std::collections::VecDeque<Step>>,
    }

    impl Scripted {
        fn new(steps: &[Step]) -> Arc<Self> {
            Arc::new(Self {
                steps: Mutex::new(steps.iter().copied().collect()),
            })
        }

        fn next(&self, buf: &mut [u8]) -> Result<usize> {
            let step = self
                .steps
                .lock()
                .unwrap()
                .pop_front()
                .expect("the stream received past the end of the script");
            match step {
                Step::Packet(bytes) => {
                    buf[..bytes.len()].copy_from_slice(bytes);
                    Ok(bytes.len())
                }
                Step::Overlong => Ok(buf.len() + 1),
                Step::Fail(error) => Err(error()),
            }
        }

        fn remaining(&self) -> usize {
            self.steps.lock().unwrap().len()
        }
    }

    impl PacketIo for Scripted {
        fn recv(&self, buf: &mut [u8]) -> Result<usize> {
            self.next(buf)
        }

        fn send(&self, buf: &[u8]) -> Result<usize> {
            Ok(buf.len())
        }
    }

    impl AsyncPacketIo for Scripted {
        fn recv(&self, buf: &mut [u8]) -> impl Future<Output = Result<usize>> + Send {
            let result = self.next(buf);
            async move { result }
        }

        async fn send(&self, buf: &[u8]) -> Result<usize> {
            Ok(buf.len())
        }
    }

    #[test]
    fn from_async_device_terminates_after_disconnected() {
        let device = Scripted::new(&[Step::Packet(b"abc"), DISCONNECTED]);
        let mut stream = from_async_device(device, 1500).unwrap();

        let first = stream
            .next()
            .now_or_never()
            .expect("first recv resolves immediately in this mock")
            .expect("stream yields an item");
        assert_eq!(first.unwrap(), *b"abc");

        let second = stream
            .next()
            .now_or_never()
            .expect("second recv resolves immediately in this mock")
            .expect("stream yields the disconnect error before ending");
        assert!(matches!(second, Err(Error::Disconnected)));

        let third = stream.next().now_or_never().expect("stream has ended");
        assert!(third.is_none(), "stream must end after Disconnected");
    }

    /// Test 2 (native): a reported and an implied oversize packet each yield
    /// `BufferTooSmall` without ending the stream, one item per `recv`; the
    /// stream ends right after `Disconnected` and every slot is back.
    #[test]
    fn native_buffer_too_small_is_not_terminal() {
        let slots = PacketPool::new(4, 8).unwrap();
        let device = Scripted::new(&[
            TOO_SMALL,
            Step::Overlong,
            Step::Packet(b"data"),
            DISCONNECTED,
        ]);
        let items = collect(from_async_device_with_pool(device, slots.clone()));
        assert_eq!(
            items,
            [
                fail(Error::BufferTooSmall),
                fail(Error::BufferTooSmall),
                data(b"data"),
                fail(Error::Disconnected),
            ]
        );
        assert_eq!(slots.available(), slots.slots());
    }

    /// Test 2 (bridge).
    #[test]
    fn bridge_buffer_too_small_is_not_terminal() {
        let slots = PacketPool::new(4, 8).unwrap();
        let device = Scripted::new(&[Step::Overlong, Step::Packet(b"data"), DISCONNECTED]);
        let items = collect(from_device_with_pool(Arc::clone(&device), slots.clone()));
        assert_eq!(
            items,
            [
                fail(Error::BufferTooSmall),
                data(b"data"),
                fail(Error::Disconnected),
            ]
        );
        wait_released(&device);
        assert_eq!(slots.available(), slots.slots());
    }

    /// Test 2: `map_recv` accepts `0..=buf_len` and releases the slot for a
    /// longer length and for an error.
    #[test]
    fn map_recv_bounds_the_view_by_the_slot() {
        let slots = PacketPool::new(1, 8).unwrap();
        let never = AtomicBool::new(false);
        let take = || slots.acquire_blocking(&never).unwrap();

        let empty = map_recv(take(), Ok(0)).unwrap();
        assert!(empty.is_empty());
        drop(empty);
        let full = map_recv(take(), Ok(8)).unwrap();
        assert_eq!(full.len(), 8);
        drop(full);
        assert!(matches!(
            map_recv(take(), Ok(9)),
            Err(Error::BufferTooSmall)
        ));
        assert_eq!(slots.available(), 1, "an overlong length releases the slot");
        assert!(matches!(
            map_recv(take(), Err(Error::Disconnected)),
            Err(Error::Disconnected)
        ));
        assert_eq!(slots.available(), 1, "an error releases the slot");
    }

    /// Test 3b: `is_terminal` classifies every current variant, and only
    /// `BufferTooSmall` is non-terminal.
    #[test]
    fn is_terminal_matches_the_end_of_life_table() {
        for error in every_error() {
            let error = error();
            assert_eq!(
                is_terminal(&error),
                !matches!(error, Error::BufferTooSmall),
                "{error:?}"
            );
        }
    }

    /// Test 3b: for every current variant, both paths yield the same
    /// sequence; `BufferTooSmall` continues and every other variant ends
    /// the stream right after it, without another `recv`.
    #[test]
    fn every_error_ends_both_streams_except_buffer_too_small() {
        for error in every_error() {
            let script = [
                TOO_SMALL,
                Step::Packet(b"abc"),
                Step::Fail(error),
                Step::Packet(b"next"),
                DISCONNECTED,
            ];
            let expected = if is_terminal(&error()) {
                vec![fail(Error::BufferTooSmall), data(b"abc"), fail(error())]
            } else {
                vec![
                    fail(Error::BufferTooSmall),
                    data(b"abc"),
                    fail(Error::BufferTooSmall),
                    data(b"next"),
                    fail(Error::Disconnected),
                ]
            };
            let native = Scripted::new(&script);
            let bridged = Scripted::new(&script);
            let native_items = collect(from_async_device(Arc::clone(&native), 8).unwrap());
            let bridged_items = collect(from_device(Arc::clone(&bridged), 8).unwrap());
            assert_eq!(native_items, expected, "{:?}", error());
            assert_eq!(bridged_items, expected, "{:?}", error());
            wait_released(&bridged);
            assert_eq!(native.remaining(), script.len() - expected.len());
            assert_eq!(bridged.remaining(), script.len() - expected.len());
        }
    }

    /// Test 3a: the bridge ends right after a terminal error, independently
    /// of the worker's timing: the stream yields `None` while the worker is
    /// still held after sending the error.
    #[test]
    fn bridge_ends_after_a_terminal_error_before_the_worker_exits() {
        let hooks = Arc::new(TestHooks::default());
        let slots = PacketPool::with_hooks(2, 8, Arc::clone(&hooks)).unwrap();
        let (release, held) = mpsc::channel::<()>();
        TestHooks::set(&hooks.after_terminal_send, move || {
            let _ = held.recv();
        });
        let device = Scripted::new(&[Step::Packet(b"abc"), INVALID_STATE, Step::Packet(b"never")]);
        let mut stream = from_device_with_pool(Arc::clone(&device), slots);
        let mut next = || futures::executor::block_on(stream.next());
        assert_eq!(seen(next().unwrap()), data(b"abc"));
        assert_eq!(seen(next().unwrap()), fail(Error::InvalidState));
        assert!(next().is_none());
        assert!(Arc::strong_count(&device) > 1, "the worker is still held");
        release.send(()).unwrap();
        wait_released(&device);
        assert_eq!(device.remaining(), 1, "recv was called after the error");
    }

    /// Test 7: a rejected `buf_len` is a constructor error on both paths,
    /// and the device is released without being touched.
    #[test]
    fn constructors_reject_a_zero_buf_len() {
        let native = Scripted::new(&[]);
        let bridged = Scripted::new(&[]);
        assert!(matches!(
            from_async_device(Arc::clone(&native), 0),
            Err(Error::InvalidState)
        ));
        assert!(matches!(
            from_device(Arc::clone(&bridged), 0),
            Err(Error::InvalidState)
        ));
        assert_eq!(Arc::strong_count(&native), 1);
        assert_eq!(Arc::strong_count(&bridged), 1);
    }

    /// Test 7, non-confusion: a device's first `InvalidState` is a stream
    /// item, not a constructor error.
    #[test]
    fn a_first_invalid_state_from_the_device_is_an_item() {
        let script = [INVALID_STATE, Step::Packet(b"never")];
        for stream in [
            from_async_device(Scripted::new(&script), 8),
            from_device(Scripted::new(&script), 8),
        ] {
            let items = collect(stream.expect("a valid buf_len"));
            assert_eq!(items, [fail(Error::InvalidState)]);
        }
    }

    /// A device whose `recv` fails immediately on every call, as a deleted
    /// or disabled interface does, counting the calls.
    struct AlwaysFails {
        calls: AtomicUsize,
    }

    impl AlwaysFails {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicUsize::new(0),
            })
        }

        fn fail(&self) -> Result<usize> {
            self.calls.fetch_add(1, Ordering::AcqRel);
            Err(Error::InvalidState)
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::Acquire)
        }
    }

    impl PacketIo for AlwaysFails {
        fn recv(&self, _buf: &mut [u8]) -> Result<usize> {
            self.fail()
        }

        fn send(&self, buf: &[u8]) -> Result<usize> {
            Ok(buf.len())
        }
    }

    impl AsyncPacketIo for AlwaysFails {
        fn recv(&self, _buf: &mut [u8]) -> impl Future<Output = Result<usize>> + Send {
            let result = self.fail();
            async move { result }
        }

        async fn send(&self, buf: &[u8]) -> Result<usize> {
            Ok(buf.len())
        }
    }

    /// The native stream yields the repeating error once, ends, and releases
    /// the device; it never polls `recv` again.
    #[test]
    fn from_async_device_ends_on_a_repeating_error_without_polling_again() {
        let device = AlwaysFails::new();
        let mut stream = from_async_device(Arc::clone(&device), 8).unwrap();
        let first = stream
            .next()
            .now_or_never()
            .expect("ready")
            .expect("an item");
        assert!(matches!(first, Err(Error::InvalidState)));
        assert!(stream.next().now_or_never().expect("ready").is_none());
        assert!(stream.next().now_or_never().expect("ready").is_none());
        assert_eq!(device.calls(), 1);
        assert_eq!(
            Arc::strong_count(&device),
            1,
            "the ended stream holds no device"
        );
    }

    /// The thread-bridge worker forwards the repeating error once and exits:
    /// the stream ends after exactly one item, `recv` was called once, and
    /// the worker releases its device handle.
    #[test]
    fn from_device_worker_exits_on_a_repeating_error() {
        let device = AlwaysFails::new();
        let items = collect(from_device(Arc::clone(&device), 8).unwrap());
        assert_eq!(items, [fail(Error::InvalidState)]);
        wait_released(&device);
        assert_eq!(device.calls(), 1, "the worker called recv after the error");
    }

    /// Test 1 (native): a single stream on a 1-slot pool waits while its
    /// consumer holds the item and is woken when the item drops.
    #[test]
    fn native_exhausted_stream_waits_then_wakes_on_release() {
        let slots = PacketPool::new(1, 8).unwrap();
        let device = Scripted::new(&[Step::Packet(b"a"), Step::Packet(b"b"), DISCONNECTED]);
        let mut stream = from_async_device_with_pool(device, slots.clone());
        let (counter, waker) = Counter::waker();

        let Poll::Ready(Some(Ok(first))) = poll_with(&mut stream, &waker) else {
            panic!("the first packet is ready");
        };
        assert!(poll_with(&mut stream, &waker).is_pending());
        assert!(poll_with(&mut stream, &waker).is_pending());
        assert_eq!(counter.count(), 0);
        drop(first);
        assert_eq!(counter.count(), 1, "releasing the item wakes the stream");
        let Poll::Ready(Some(Ok(second))) = poll_with(&mut stream, &waker) else {
            panic!("the second packet is ready");
        };
        assert_eq!(second, *b"b");
    }

    /// r6-6: every receive on an exhausted native stream goes through the
    /// release slow path (a live waiter drained).
    #[test]
    fn native_exhausted_stream_takes_the_drain_path_per_packet() {
        const PACKETS: usize = 20;
        let hooks = Arc::new(TestHooks::default());
        let slots = PacketPool::with_hooks(1, 8, Arc::clone(&hooks)).unwrap();
        let device = Scripted::new(&[Step::Packet(b"p"); PACKETS]);
        let mut stream = from_async_device_with_pool(device, slots);
        let (counter, waker) = Counter::waker();

        let Poll::Ready(Some(Ok(mut held))) = poll_with(&mut stream, &waker) else {
            panic!("the first packet is ready");
        };
        for _ in 1..PACKETS {
            assert!(poll_with(&mut stream, &waker).is_pending());
            drop(held);
            let Poll::Ready(Some(Ok(next))) = poll_with(&mut stream, &waker) else {
                panic!("the next packet is ready after the release");
            };
            held = next;
        }
        drop(held);
        assert_eq!(counter.count(), PACKETS - 1);
        assert_eq!(hooks.drain_count.load(Ordering::SeqCst), PACKETS - 1);
    }

    /// Test 1 (bridge): the worker waits for the held slot, and the consumer
    /// is woken once the item drops and the worker has queued the next.
    #[test]
    fn bridge_exhausted_stream_waits_then_wakes_on_release() {
        let slots = PacketPool::new(1, 8).unwrap();
        let device = Scripted::new(&[Step::Packet(b"a"), Step::Packet(b"b"), DISCONNECTED]);
        let mut stream = from_device_with_pool(Arc::clone(&device), slots.clone());
        let first = futures::executor::block_on(stream.next()).unwrap().unwrap();
        assert_eq!(first, *b"a");
        wait_until("the worker to wait for a slot", || slots.blocked() == 1);

        let (counter, waker) = Counter::waker();
        assert!(poll_with(&mut stream, &waker).is_pending());
        drop(first);
        wait_until("the wake", || counter.count() >= 1);
        let Poll::Ready(Some(Ok(second))) = poll_with(&mut stream, &waker) else {
            panic!("the second packet is ready after the wake");
        };
        assert_eq!(second, *b"b");
        drop(second);
        assert_eq!(collect(stream), [fail(Error::Disconnected)]);
        wait_released(&device);
    }

    /// Test 5a: dropping the stream makes a worker that waits for a free
    /// slot exit.
    #[test]
    fn bridge_drop_stops_a_worker_waiting_for_a_slot() {
        let slots = PacketPool::new(1, 8).unwrap();
        let device = Scripted::new(&[Step::Packet(b"a"), Step::Packet(b"never")]);
        let mut stream = from_device_with_pool(Arc::clone(&device), slots.clone());
        let held = futures::executor::block_on(stream.next()).unwrap().unwrap();
        wait_until("the worker to wait for a slot", || slots.blocked() == 1);
        drop(stream);
        wait_released(&device);
        assert_eq!(device.remaining(), 1);
        drop(held);
        assert_eq!(slots.available(), 1);
    }

    /// A device whose every packet is too long for the buffer, counting its
    /// calls: each yields `BufferTooSmall`, which holds no slot, so the
    /// worker fills the queue and then waits to send.
    struct AlwaysOverlong {
        calls: AtomicUsize,
    }

    impl PacketIo for AlwaysOverlong {
        fn recv(&self, buf: &mut [u8]) -> Result<usize> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(buf.len() + 1)
        }

        fn send(&self, buf: &[u8]) -> Result<usize> {
            Ok(buf.len())
        }
    }

    /// Test 5a: dropping the stream makes a worker blocked on a full queue
    /// exit.
    #[test]
    fn bridge_drop_stops_a_worker_waiting_to_queue() {
        let slots = PacketPool::new(2, 8).unwrap();
        let device = Arc::new(AlwaysOverlong {
            calls: AtomicUsize::new(0),
        });
        let stream = from_device_with_pool(Arc::clone(&device), slots.clone());
        // Two queued items, plus the third the worker is trying to send.
        wait_until("the queue to fill", || {
            device.calls.load(Ordering::SeqCst) >= 3
        });
        drop(stream);
        wait_released(&device);
        assert_eq!(device.calls.load(Ordering::SeqCst), 3);
        assert_eq!(slots.available(), 2);
    }

    /// Tests 5b and 5c: two bridge streams share a 1-slot pool whose slot
    /// the test holds. Dropping one stops only its worker; the released slot
    /// then reaches the surviving worker.
    #[test]
    fn bridge_shared_slots_drop_one_stream_then_the_other_proceeds() {
        let slots = PacketPool::new(1, 8).unwrap();
        let never = AtomicBool::new(false);
        let held = slots.acquire_blocking(&never).unwrap();
        let dropped = Scripted::new(&[Step::Packet(b"never")]);
        let survivor = Scripted::new(&[Step::Packet(b"x"), DISCONNECTED]);
        let first = from_device_with_pool(Arc::clone(&dropped), slots.clone());
        let second = from_device_with_pool(Arc::clone(&survivor), slots.clone());
        wait_until("both workers to wait", || slots.blocked() == 2);

        drop(first);
        wait_released(&dropped);
        wait_until("one worker to wait", || slots.blocked() == 1);
        assert_eq!(dropped.remaining(), 1, "the stopped worker never received");

        drop(held);
        assert_eq!(collect(second), [data(b"x"), fail(Error::Disconnected)]);
        wait_released(&survivor);
        assert_eq!(slots.available(), 1);
    }

    /// A device whose `recv` waits at a barrier shared with the test, then
    /// panics.
    struct PanicsAfterBarrier {
        barrier: Barrier,
    }

    impl PanicsAfterBarrier {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                barrier: Barrier::new(2),
            })
        }
    }

    impl PacketIo for PanicsAfterBarrier {
        fn recv(&self, _buf: &mut [u8]) -> Result<usize> {
            self.barrier.wait();
            panic!("test device panics in recv");
        }

        fn send(&self, buf: &[u8]) -> Result<usize> {
            Ok(buf.len())
        }
    }

    /// Installs a hook that reports it was entered, then waits for the
    /// test's release. Returns `(entered, release)`.
    fn holding_hook(slot: &pool::Hook) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
        let (entered_tx, entered) = mpsc::channel();
        let (release, release_rx) = mpsc::channel::<()>();
        TestHooks::set(slot, move || {
            entered_tx.send(()).unwrap();
            let _ = release_rx.recv();
        });
        (entered, release)
    }

    /// Test 5d v1: a panicking worker has disconnected before its final
    /// wake; while it is held there, the device is not released yet and a
    /// poll already sees the end.
    #[test]
    fn bridge_worker_panic_disconnects_before_the_final_wake() {
        let hooks = Arc::new(TestHooks::default());
        let slots = PacketPool::with_hooks(1, 8, Arc::clone(&hooks)).unwrap();
        let (entered, release) = holding_hook(&hooks.after_tx_drop);
        let device = PanicsAfterBarrier::new();
        let mut stream = from_device_with_pool(Arc::clone(&device), slots);
        device.barrier.wait();
        entered.recv().unwrap();
        assert!(Arc::strong_count(&device) > 1, "the worker is still held");
        assert!(poll_with(&mut stream, Waker::noop()).is_ready_and_none());
        release.send(()).unwrap();
        wait_released(&device);
    }

    /// Test 5d v2: a pending consumer is woken exactly once by the panicking
    /// worker's exit, and then sees the end.
    #[test]
    fn bridge_worker_panic_wakes_the_consumer_once() {
        let device = PanicsAfterBarrier::new();
        let mut stream = from_device(Arc::clone(&device), 8).unwrap();
        let (counter, waker) = Counter::waker();
        assert!(poll_with(&mut stream, &waker).is_pending());
        device.barrier.wait();
        wait_released(&device);
        assert_eq!(counter.count(), 1);
        assert!(poll_with(&mut stream, &waker).is_ready_and_none());
    }

    /// Test 5d v3: a consumer waker that panics when the exiting worker
    /// wakes it does not abort, and the worker still releases everything.
    #[test]
    fn bridge_worker_exit_contains_a_panicking_consumer_wake() {
        let device = PanicsAfterBarrier::new();
        let mut stream = from_device(Arc::clone(&device), 8).unwrap();
        assert!(poll_with(&mut stream, &Waker::from(Arc::new(PanicOnWake))).is_pending());
        device.barrier.wait();
        wait_released(&device);
        assert!(poll_with(&mut stream, Waker::noop()).is_ready_and_none());
    }

    /// Test 5d v4: a waker whose drop panics, registered while the worker
    /// is between its final wake and its release of the waker slot, is
    /// dropped under a guard when the stream drops.
    #[test]
    fn bridge_drop_contains_a_late_registered_panicking_waker() {
        let hooks = Arc::new(TestHooks::default());
        let slots = PacketPool::with_hooks(1, 8, Arc::clone(&hooks)).unwrap();
        let (entered, release) = holding_hook(&hooks.after_worker_wake);
        let device = PanicsAfterBarrier::new();
        let mut stream = from_device_with_pool(Arc::clone(&device), slots);
        device.barrier.wait();
        entered.recv().unwrap();
        let waker = Waker::from(Arc::new(PanicOnDrop));
        assert!(poll_with(&mut stream, &waker).is_ready_and_none());
        // The stored clone is now the last one.
        drop(waker);
        drop(stream);
        release.send(()).unwrap();
        wait_released(&device);
    }

    /// Test 5d v5: the stream holds the last waker slot with a waker whose
    /// drop panics, and is dropped during unwinding: no abort.
    #[test]
    fn bridge_drop_during_unwind_contains_a_panicking_waker() {
        let device = PanicsAfterBarrier::new();
        let mut stream = from_device(Arc::clone(&device), 8).unwrap();
        device.barrier.wait();
        wait_released(&device);
        let waker = Waker::from(Arc::new(PanicOnDrop));
        assert!(poll_with(&mut stream, &waker).is_ready_and_none());
        drop(waker);
        let unwound = catch_unwind(AssertUnwindSafe(move || {
            let _stream = stream;
            panic!("unwinding with the stream alive");
        }));
        assert!(unwound.is_err());
    }

    /// Test 5e: once the worker has released the device, the pool it held
    /// is gone too (the stream was dropped first, and nothing else keeps a
    /// pool handle).
    #[test]
    fn bridge_worker_releases_its_slab_before_the_device() {
        let hooks = Arc::new(TestHooks::default());
        let slots = PacketPool::with_hooks(2, 8, Arc::clone(&hooks)).unwrap();
        let device = Scripted::new(&[Step::Packet(b"a"), DISCONNECTED]);
        let items = collect(from_device_with_pool(Arc::clone(&device), slots));
        assert_eq!(items, [data(b"a"), fail(Error::Disconnected)]);
        wait_released(&device);
        assert!(hooks.inner_dropped.load(Ordering::SeqCst));
    }

    /// Each stream builds exactly one acquirer. It is exclusive when the
    /// stream got the only handle of its pool, on both paths, and shared
    /// when the caller kept a clone.
    #[test]
    fn streams_acquire_exclusively_only_from_the_sole_handle() {
        let counts = |hooks: &TestHooks| {
            (
                hooks.acquirers.load(Ordering::SeqCst),
                hooks.exclusive_acquirers.load(Ordering::SeqCst),
            )
        };
        for keep_clone in [false, true] {
            let native_hooks = Arc::new(TestHooks::default());
            let pool = PacketPool::with_hooks(2, 8, Arc::clone(&native_hooks)).unwrap();
            let kept = keep_clone.then(|| pool.clone());
            let device = Scripted::new(&[Step::Packet(b"a"), DISCONNECTED]);
            let items = collect(from_async_device_with_pool(device, pool));
            assert_eq!(items, [data(b"a"), fail(Error::Disconnected)]);
            assert_eq!(counts(&native_hooks), (1, usize::from(!keep_clone)));
            drop(kept);

            let bridge_hooks = Arc::new(TestHooks::default());
            let pool = PacketPool::with_hooks(2, 8, Arc::clone(&bridge_hooks)).unwrap();
            let kept = keep_clone.then(|| pool.clone());
            let device = Scripted::new(&[Step::Packet(b"b"), DISCONNECTED]);
            let items = collect(from_device_with_pool(Arc::clone(&device), pool));
            assert_eq!(items, [data(b"b"), fail(Error::Disconnected)]);
            wait_released(&device);
            assert_eq!(counts(&bridge_hooks), (1, usize::from(!keep_clone)));
            drop(kept);
        }
    }

    /// The last item, dropped on another thread after the stream and the
    /// pool handle are gone, frees the pool exactly once.
    #[test]
    fn native_last_item_dropped_elsewhere_frees_the_pool_once() {
        let hooks = Arc::new(TestHooks::default());
        let pool = PacketPool::with_hooks(4, 8, Arc::clone(&hooks)).unwrap();
        let device = Scripted::new(&[Step::Packet(b"a"), Step::Packet(b"b")]);
        let mut stream = from_async_device_with_pool(device, pool);
        let a = futures::executor::block_on(stream.next()).unwrap().unwrap();
        let b = futures::executor::block_on(stream.next()).unwrap().unwrap();
        drop(stream);
        drop(a);
        assert!(!hooks.inner_dropped.load(Ordering::SeqCst));
        thread::spawn(move || assert_eq!(b, *b"b")).join().unwrap();
        assert_eq!(hooks.inner_drops.load(Ordering::SeqCst), 1);
    }

    /// Test 5f: a consumer waker that panics whenever the worker wakes it
    /// (off the consumer's thread) loses no items.
    #[test]
    fn bridge_panicking_off_thread_wakes_lose_no_items() {
        let device = Scripted::new(&[
            Step::Packet(b"1"),
            Step::Packet(b"2"),
            Step::Packet(b"3"),
            Step::Packet(b"4"),
            Step::Packet(b"5"),
            DISCONNECTED,
        ]);
        let mut stream = from_device(Arc::clone(&device), 8).unwrap();
        let waker = Waker::from(Arc::new(PanicOffThread {
            owner: thread::current().id(),
        }));
        let mut items = Vec::new();
        let start = Instant::now();
        loop {
            match poll_with(&mut stream, &waker) {
                Poll::Ready(Some(item)) => items.push(seen(item)),
                Poll::Ready(None) => break,
                Poll::Pending => {
                    assert!(start.elapsed() < DEADLINE, "timed out spinning");
                    thread::yield_now();
                }
            }
        }
        assert_eq!(
            items,
            [
                data(b"1"),
                data(b"2"),
                data(b"3"),
                data(b"4"),
                data(b"5"),
                fail(Error::Disconnected),
            ]
        );
        wait_released(&device);
    }

    /// Test 5g (bridge): a panic from cloning the consumer's waker escapes
    /// that poll, and the next poll works normally.
    #[test]
    fn bridge_survives_a_panicking_waker_clone() {
        let device = Scripted::new(&[Step::Packet(b"a"), Step::Packet(b"b"), DISCONNECTED]);
        let mut stream = from_device(Arc::clone(&device), 8).unwrap();
        let switch = Arc::new(CloneSwitch::default());
        switch.panic_on_clone.store(true, Ordering::SeqCst);
        let waker = switch.waker();
        let polled = catch_unwind(AssertUnwindSafe(|| {
            let _ = poll_with(&mut stream, &waker);
        }));
        assert!(polled.is_err(), "the clone panic escapes poll_next");
        assert_eq!(
            collect(stream),
            [data(b"a"), data(b"b"), fail(Error::Disconnected)]
        );
        wait_released(&device);
    }

    /// Test 5g (native): after a waker-clone panic on an exhausted 1-slot
    /// pool, every later poll panics; dropping the stream and the held item
    /// returns every slot.
    #[test]
    fn native_panicking_waker_clone_poisons_only_that_stream() {
        let slots = PacketPool::new(1, 8).unwrap();
        let device = Scripted::new(&[Step::Packet(b"a"), Step::Packet(b"b")]);
        let mut stream = from_async_device_with_pool(device, slots.clone());
        let Poll::Ready(Some(Ok(held))) = poll_with(&mut stream, Waker::noop()) else {
            panic!("the first packet is ready");
        };
        let switch = Arc::new(CloneSwitch::default());
        switch.panic_on_clone.store(true, Ordering::SeqCst);
        let waker = switch.waker();
        let polled = catch_unwind(AssertUnwindSafe(|| {
            let _ = poll_with(&mut stream, &waker);
        }));
        assert!(polled.is_err(), "the clone panic escapes poll_next");
        let again = catch_unwind(AssertUnwindSafe(|| {
            let _ = poll_with(&mut stream, Waker::noop());
        }));
        assert!(again.is_err(), "a native stream panics after that");
        drop(stream);
        drop(held);
        assert_eq!(slots.available(), slots.slots());
    }

    /// Test 16: dropping a bridge stream discards its queued items; their
    /// release wakes a native stream's panicking waiter on the same pool
    /// without letting the panic escape, and every slot comes back.
    #[test]
    fn bridge_teardown_contains_a_panicking_native_waiter() {
        let slots = PacketPool::new(2, 8).unwrap();
        let bridged = Scripted::new(&[Step::Packet(b"a"), Step::Packet(b"b"), Step::Packet(b"x")]);
        let bridge = from_device_with_pool(Arc::clone(&bridged), slots.clone());
        wait_until("the worker to fill every slot and wait", || {
            slots.available() == 0 && slots.blocked() == 1
        });

        let native = Scripted::new(&[Step::Packet(b"c"), DISCONNECTED]);
        let mut stream = from_async_device_with_pool(native, slots.clone());
        assert!(poll_with(&mut stream, &Waker::from(Arc::new(PanicOnWake))).is_pending());

        drop(bridge);
        wait_released(&bridged);
        assert_eq!(bridged.remaining(), 1);
        assert_eq!(collect(stream), [data(b"c"), fail(Error::Disconnected)]);
        assert_eq!(slots.available(), slots.slots());
    }

    /// Readability helper for `Poll<Option<_>>`.
    trait ReadyNone {
        fn is_ready_and_none(&self) -> bool;
    }

    impl<T> ReadyNone for Poll<Option<T>> {
        fn is_ready_and_none(&self) -> bool {
            matches!(self, Poll::Ready(None))
        }
    }
}
