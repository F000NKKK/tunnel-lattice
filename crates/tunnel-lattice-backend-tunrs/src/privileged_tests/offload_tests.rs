//! Privileged Linux tests of segmentation offload on real TUN queues.
//!
//! Like the rest of `privileged_tests`, each test creates its own
//! non-persistent device, which goes away with its last handle, and changes
//! nothing else on the host: the address it assigns lives on that device.
//! Every receive runs on its own thread under a deadline; on timeout the
//! device is deleted (`ip link del`), which ends the waiting `recv`, so a
//! failing test cannot hang or leave a device behind.
//!
//! Traffic comes from a UDP socket with `UDP_SEGMENT` set, so the host
//! stack builds UDP GSO super-packets and routes them into the device. On
//! a kernel with USO (6.2 and later) an offload-framed queue receives them
//! whole and the backend splits them; on an older kernel the stack
//! segments them first. Either way `recv` must return each segment as its
//! own MTU-sized packet, in order, with valid checksums.

use std::net::{SocketAddr, UdpSocket};
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tunnel_lattice_platform::{
    CapabilityProvider, DeviceObserver, DeviceProvider, MultiQueueProvider, PacketIo,
};

use super::*;

/// The device MTU every test uses.
const MTU: u16 = 1400;
/// The `UDP_SEGMENT` size: the UDP payload of every segment.
const GSO_SIZE: usize = 1000;
/// Segments per burst.
const SEGMENTS: usize = 16;
/// IPv4 (20) plus UDP (8) header bytes of every segment.
const HDR_LEN: usize = 28;
/// The kernel's `IFF_VNET_HDR`, as `tun_flags` in sysfs reports it.
const VNET_HDR_FLAG: u32 = 0x4000;
/// How long a receive may take before the test fails.
const DEADLINE: Duration = Duration::from_secs(60);

/// Enters the Tokio runtime the `tokio` build needs to open a device (see
/// `enter_tokio_runtime`); nothing in the other builds.
macro_rules! enter_runtime {
    () => {
        #[cfg(feature = "tokio")]
        let _runtime = enter_tokio_runtime();
        #[cfg(feature = "tokio")]
        let _entered = _runtime.enter();
    };
}

fn open(config: DeviceConfig) -> TunRsDevice {
    let label = format!("{config:?}");
    TunRsBackend::new()
        .open(config.with_mtu(u32::from(MTU)))
        .unwrap_or_else(|err| panic!("open {label}: {err:?}"))
}

fn has_offload(device: &TunRsDevice) -> bool {
    device
        .capabilities()
        .contains(Capability::SEGMENTATION_OFFLOAD)
}

fn vnet_hdr_flag(name: &str) -> bool {
    linux_tun_flags(name) & VNET_HDR_FLAG != 0
}

/// An offload request on a Linux TUN device gives an offload-framed queue
/// that reports `SEGMENTATION_OFFLOAD`; without the request, and for TAP,
/// the queue is plain and does not. The backend never reports the flag.
#[test]
#[ignore = "requires CAP_NET_ADMIN to open a TUN/TAP device"]
fn offload_framing_follows_the_request_and_the_kind() {
    enter_runtime!();

    assert!(
        !TunRsBackend::new()
            .capabilities()
            .contains(Capability::SEGMENTATION_OFFLOAD)
    );
    let offload = open(DeviceConfig::new(DeviceKind::Tun).with_offload(true));
    let name = offload.snapshot().expect("snapshot").name;
    assert!(
        has_offload(&offload),
        "an offload TUN queue reports offload"
    );
    assert!(vnet_hdr_flag(&name), "an offload TUN queue is framed");

    let plain = open(DeviceConfig::new(DeviceKind::Tun));
    let name = plain.snapshot().expect("snapshot").name;
    assert!(!has_offload(&plain));
    assert!(!vnet_hdr_flag(&name));

    let tap = open(DeviceConfig::new(DeviceKind::Tap).with_offload(true));
    let name = tap.snapshot().expect("snapshot").name;
    assert!(!has_offload(&tap), "the request is ignored for TAP");
    assert!(!vnet_hdr_flag(&name));
}

/// Sends a UDP GSO burst to `target` every 100 ms until stopped. Returns
/// the socket's source port, to tell the bursts from other traffic.
struct GsoSender {
    port: u16,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl GsoSender {
    fn start(target: SocketAddr) -> Self {
        let socket = UdpSocket::bind("0.0.0.0:0").expect("bind a UDP socket");
        let size = libc::c_int::try_from(GSO_SIZE).expect("a small size");
        // SAFETY: the descriptor is the socket's own, open for the call;
        // `UDP_SEGMENT` reads one `int` from the pointer, which points at
        // `size`, a live `c_int`, and the length passed is its size.
        let rc = unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                libc::SOL_UDP,
                libc::UDP_SEGMENT,
                (&raw const size).cast(),
                libc::socklen_t::try_from(size_of::<libc::c_int>()).expect("a small size"),
            )
        };
        assert_eq!(
            rc,
            0,
            "set UDP_SEGMENT: {}",
            std::io::Error::last_os_error()
        );
        let port = socket.local_addr().expect("the local address").port();
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut burst: u16 = 0;
                while !stop.load(Ordering::Acquire) {
                    // Errors are expected until the route is usable.
                    let _ = socket.send_to(&burst_payload(burst), target);
                    burst = burst.wrapping_add(1);
                    std::thread::sleep(Duration::from_millis(100));
                }
            })
        };
        Self {
            port,
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for GsoSender {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The UDP payload of segment `k` of burst `burst`.
fn segment_payload(burst: u16, k: usize) -> Vec<u8> {
    let [b0, b1] = burst.to_be_bytes();
    let mut payload = vec![b0, b1, u8::try_from(k).expect("few segments")];
    payload.extend((3..GSO_SIZE).map(|i| (i * 7 + k) as u8));
    payload
}

/// One `UDP_SEGMENT` send: [`SEGMENTS`] payloads back to back.
fn burst_payload(burst: u16) -> Vec<u8> {
    (0..SEGMENTS)
        .flat_map(|k| segment_payload(burst, k))
        .collect()
}

/// Whether the ones' complement sum over `parts` (each of even length)
/// checks out.
fn checksum_ok(parts: &[&[u8]]) -> bool {
    let sum: u32 = parts
        .iter()
        .map(|part| u32::from(ones_complement_sum(part)))
        .sum();
    let folded = (sum & 0xffff) + (sum >> 16);
    let folded = (folded & 0xffff) + (folded >> 16);
    folded == 0xffff
}

/// Checks that the burst segments arrive whole and in order: once segment
/// 0 of a burst is seen, the next packets of the flow are its segments 1
/// to the last, so a segment lost (or duplicated) by the split fails it.
struct BurstCheck {
    port: u16,
    /// The burst being followed and the next segment expected from it.
    expected: Option<(u16, usize)>,
    packets: usize,
}

impl BurstCheck {
    fn new(port: u16) -> Self {
        Self {
            port,
            expected: None,
            packets: 0,
        }
    }

    /// Feeds one received packet: `Ok(true)` once a whole burst arrived,
    /// `Ok(false)` to read on, `Err` on a missing, malformed or out-of-order
    /// segment.
    fn feed(&mut self, packet: &[u8]) -> std::result::Result<bool, String> {
        self.packets += 1;
        if self.packets > 100_000 {
            return Err("no whole burst within 100000 packets".to_owned());
        }
        let ours = packet.len() >= HDR_LEN
            && packet[0] == 0x45
            && packet[9] == 17
            && packet[20..22] == self.port.to_be_bytes()
            && packet[22..24] == [0, 9];
        if !ours {
            return Ok(false);
        }
        if packet.len() != HDR_LEN + GSO_SIZE {
            return Err(format!("a {}-byte segment", packet.len()));
        }
        if !checksum_ok(&[&packet[..20]]) {
            return Err("an invalid IPv4 header checksum".to_owned());
        }
        let udp_len = u16::try_from(packet.len() - 20).expect("a short packet");
        let [l0, l1] = udp_len.to_be_bytes();
        let pseudo = [0, 17, l0, l1];
        if !checksum_ok(&[&packet[12..20], &pseudo, &packet[20..]]) {
            return Err("an invalid UDP checksum".to_owned());
        }
        let burst = u16::from_be_bytes([packet[28], packet[29]]);
        let k = usize::from(packet[30]);
        if packet[HDR_LEN..] != segment_payload(burst, k) {
            return Err(format!("burst {burst} segment {k}: a corrupt payload"));
        }
        match self.expected {
            Some((b, next)) if (b, next) == (burst, k) => {
                if k + 1 == SEGMENTS {
                    return Ok(true);
                }
                self.expected = Some((b, k + 1));
                Ok(false)
            }
            Some((b, next)) => Err(format!(
                "expected burst {b} segment {next}, got burst {burst} segment {k}"
            )),
            // Joined mid-burst: wait for the start of the next one.
            None if k != 0 => Ok(false),
            None => {
                self.expected = Some((burst, 1));
                Ok(false)
            }
        }
    }
}

/// Runs `receive` on its own thread (inside the test's Tokio runtime under
/// `tokio`) and returns its outcome, or deletes the device to end a
/// waiting `recv` and fails after [`DEADLINE`].
fn within_deadline(
    device: &Arc<TunRsDevice>,
    name: &str,
    receive: impl FnOnce(&TunRsDevice) -> std::result::Result<(), String> + Send + 'static,
) -> std::result::Result<(), String> {
    let (done, outcome) = std::sync::mpsc::channel();
    #[cfg(feature = "tokio")]
    let runtime = tokio::runtime::Handle::current();
    let thread = {
        let device = Arc::clone(device);
        std::thread::spawn(move || {
            #[cfg(feature = "tokio")]
            let _entered = runtime.enter();
            let _ = done.send(receive(&device));
        })
    };
    let result = match outcome.recv_timeout(DEADLINE) {
        Ok(result) => result,
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            Err("the receiver thread panicked".to_owned())
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            release_waiting_recv(device, name, None);
            if outcome.recv_timeout(Duration::from_secs(30)).is_err() {
                return Err("timed out, and the receiver did not return once released".to_owned());
            }
            Err("no whole burst arrived in time".to_owned())
        }
    };
    let _ = thread.join();
    result
}

/// Addresses `device`, sends GSO bursts into it, and checks that blocking
/// `recv`s return one whole burst segment by segment.
fn assert_receives_a_burst(device: TunRsDevice, subnet: [u8; 3]) {
    let name = device.snapshot().expect("snapshot").name;
    let device = Arc::new(device);
    let target = route_traffic_into(DeviceKind::Tun, &name, subnet);
    let sender = GsoSender::start(target);
    let port = sender.port;
    let result = within_deadline(&device, &name, move |device| {
        let mut check = BurstCheck::new(port);
        let mut buf = vec![0u8; usize::from(MTU)];
        loop {
            match PacketIo::recv(device, &mut buf) {
                Ok(n) => {
                    if check.feed(&buf[..n])? {
                        return Ok(());
                    }
                }
                Err(err) => return Err(format!("recv: {err:?}")),
            }
        }
    });
    drop(sender);
    drop(device);
    if let Err(message) = result {
        panic!("{name}: {message}");
    }
}

/// A UDP GSO burst routed into an offload-framed queue is received as
/// MTU-sized segments, each with valid checksums, none lost.
#[test]
#[ignore = "requires CAP_NET_ADMIN to open and address a TUN device"]
fn a_gso_burst_is_received_segment_by_segment() {
    enter_runtime!();
    let device = open(DeviceConfig::new(DeviceKind::Tun).with_offload(true));
    assert!(has_offload(&device));
    assert_receives_a_burst(device, [10, 204, subnet_octet()]);
}

/// A plain send on an offload-framed queue goes out behind an all-zero
/// header: the host receives it as an ordinary UDP datagram.
#[test]
#[ignore = "requires CAP_NET_ADMIN to open and address a TUN device"]
fn a_plain_send_on_an_offload_queue_reaches_the_host() {
    enter_runtime!();
    let device = open(DeviceConfig::new(DeviceKind::Tun).with_offload(true));
    assert_send_reaches_the_host(&device, [10, 205, subnet_octet()]);
}

/// Addresses `device` with `subnet.1`, binds a host socket there, and
/// checks that a UDP packet sent through `device` from `subnet.2` arrives.
fn assert_send_reaches_the_host(device: &TunRsDevice, subnet: [u8; 3]) {
    let name = device.snapshot().expect("snapshot").name;
    route_traffic_into(DeviceKind::Tun, &name, subnet);
    let [a, b, c] = subnet;
    let host = UdpSocket::bind(format!("{a}.{b}.{c}.1:0")).expect("bind the host socket");
    host.set_read_timeout(Some(Duration::from_millis(500)))
        .expect("set a read timeout");
    let port = host.local_addr().expect("the host address").port();
    let packet = udp_packet([a, b, c, 2], [a, b, c, 1], port, b"offload send");

    let mut buf = [0u8; 64];
    for _ in 0..20 {
        let sent = PacketIo::send(device, &packet);
        assert!(
            matches!(sent, Ok(n) if n == packet.len()),
            "send through {name}: {sent:?}"
        );
        if let Ok((n, from)) = host.recv_from(&mut buf) {
            assert_eq!(&buf[..n], b"offload send");
            assert_eq!(from.port(), 4000);
            return;
        }
    }
    panic!("{name}: the host never received the packet");
}

/// An IPv4/UDP packet from `src:4000` to `dst:port` carrying `payload`,
/// with valid IPv4 and UDP checksums.
fn udp_packet(src: [u8; 4], dst: [u8; 4], port: u16, payload: &[u8]) -> Vec<u8> {
    let total = u16::try_from(HDR_LEN + payload.len()).expect("a short packet");
    let udp_len = total - 20;
    let mut packet = vec![0x45, 0, 0, 0, 0, 0, 0x40, 0, 64, 17, 0, 0];
    packet[2..4].copy_from_slice(&total.to_be_bytes());
    packet.extend_from_slice(&src);
    packet.extend_from_slice(&dst);
    packet.extend_from_slice(&4000u16.to_be_bytes());
    packet.extend_from_slice(&port.to_be_bytes());
    packet.extend_from_slice(&udp_len.to_be_bytes());
    packet.extend_from_slice(&[0, 0]);
    packet.extend_from_slice(payload);
    if packet.len() % 2 == 1 {
        packet.push(0);
    }
    let ip_sum = !ones_complement_sum(&packet[..20]);
    packet[10..12].copy_from_slice(&ip_sum.to_be_bytes());
    let [l0, l1] = udp_len.to_be_bytes();
    let pseudo = [0, 17, l0, l1];
    let sum = u32::from(ones_complement_sum(&packet[12..20]))
        + u32::from(ones_complement_sum(&pseudo))
        + u32::from(ones_complement_sum(&packet[20..]));
    let folded = (sum & 0xffff) + (sum >> 16);
    let folded = (folded & 0xffff) + (folded >> 16);
    let udp_sum = match !u16::try_from(folded).expect("folded into 16 bits") {
        0 => 0xffff,
        sum => sum,
    };
    packet[26..28].copy_from_slice(&udp_sum.to_be_bytes());
    packet.truncate(usize::from(total));
    packet
}

/// A queue attached to a multi-queue device that already has a queue takes
/// the device's framing, not its own request: a plain attach to an offload
/// device is offload-framed (and reports it), and an offload attach to a
/// plain device is plain (and does not). Each attached queue sends; once
/// the first queue is gone, each receives a GSO burst whole, so the plain
/// attach's offload mask was cleared.
#[test]
#[ignore = "requires CAP_NET_ADMIN to open and address a TUN device"]
fn an_attached_queue_takes_the_framing_of_its_device() {
    enter_runtime!();
    let mq = |offload: bool, name: &str| {
        DeviceConfig::new(DeviceKind::Tun)
            .with_name(name)
            .with_multi_queue(true)
            .with_offload(offload)
    };
    // Separate subnets for the send and the receive check, since each
    // assigns its own address.
    for (first_offload, send_subnet, recv_subnet) in [(true, 206u8, 216u8), (false, 207, 217)] {
        let name = unique_linux_name(if first_offload { "mqoff" } else { "mqpln" });
        let _guard = delete_on_drop(&name);
        let first = open(mq(first_offload, &name));
        let attached = open(mq(!first_offload, &name));
        assert_eq!(has_offload(&first), first_offload, "{name}: first queue");
        assert_eq!(
            has_offload(&attached),
            first_offload,
            "{name}: the attached queue follows the device"
        );
        assert_eq!(vnet_hdr_flag(&name), first_offload, "{name}: device flag");
        assert_send_reaches_the_host(&attached, [10, send_subnet, subnet_octet()]);
        drop(first);
        assert_receives_a_burst(attached, [10, recv_subnet, subnet_octet()]);
    }
}

/// `additional_queue` on an offload-framed queue gives another
/// offload-framed queue with its own staging, which receives a GSO burst
/// segment by segment once the original queue is gone.
#[test]
#[ignore = "requires CAP_NET_ADMIN to open and address a TUN device"]
fn an_additional_queue_of_an_offload_queue_is_offload_framed() {
    enter_runtime!();
    let device = open(
        DeviceConfig::new(DeviceKind::Tun)
            .with_multi_queue(true)
            .with_offload(true),
    );
    let queue = device.additional_queue().expect("add a queue");
    assert!(has_offload(&queue), "the added queue reports offload");
    drop(device);
    assert_receives_a_burst(queue, [10, 208, subnet_octet()]);
}

/// An async `recv` dropped after one poll, between the segments of a GSO
/// burst and while waiting for the next frame, loses no segment: every
/// burst is still received whole and in order. With `tokio` the dropped
/// poll is forced to yield by an exhausted cooperative budget; with
/// `async-io` it is dropped whether or not it completed (a completed one's
/// packet is checked too).
#[test]
#[cfg(feature = "async")]
#[ignore = "requires CAP_NET_ADMIN to open and address a TUN device"]
fn a_dropped_async_recv_loses_no_segment() {
    #[cfg(feature = "tokio")]
    use std::future::Future;
    #[cfg(feature = "tokio")]
    use std::task::Poll;

    use tunnel_lattice_platform::AsyncPacketIo;

    enter_runtime!();
    let device = open(DeviceConfig::new(DeviceKind::Tun).with_offload(true));
    let name = device.snapshot().expect("snapshot").name;
    let device = Arc::new(device);
    let target = route_traffic_into(DeviceKind::Tun, &name, [10, 209, subnet_octet()]);
    let sender = GsoSender::start(target);
    let port = sender.port;

    let owned = Arc::clone(&device);
    let result = within_deadline(&device, &name, move |_| {
        let receive = async move {
            let device = &*owned;
            let mut check = BurstCheck::new(port);
            let mut buf = vec![0u8; usize::from(MTU)];
            let mut bursts = 0;
            loop {
                let n = AsyncPacketIo::recv(device, &mut buf)
                    .await
                    .map_err(|err| format!("recv: {err:?}"))?;
                if check.feed(&buf[..n])? {
                    bursts += 1;
                    if bursts == 3 {
                        return Ok(());
                    }
                    check = BurstCheck::new(port);
                }
                #[cfg(feature = "tokio")]
                {
                    while tokio::task::coop::has_budget_remaining() {
                        tokio::task::coop::consume_budget().await;
                    }
                    let mut dropped = std::pin::pin!(AsyncPacketIo::recv(device, &mut buf));
                    let pending = std::future::poll_fn(|cx| {
                        Poll::Ready(dropped.as_mut().poll(cx).is_pending())
                    })
                    .await;
                    if !pending {
                        return Err("the exhausted budget did not force a yield".to_owned());
                    }
                }
                #[cfg(not(feature = "tokio"))]
                {
                    use futures::FutureExt;
                    if let Some(result) = AsyncPacketIo::recv(device, &mut buf).now_or_never() {
                        let n = result.map_err(|err| format!("recv: {err:?}"))?;
                        if check.feed(&buf[..n])? {
                            bursts += 1;
                            if bursts == 3 {
                                return Ok(());
                            }
                            check = BurstCheck::new(port);
                        }
                    }
                }
                #[cfg(feature = "tokio")]
                tokio::task::yield_now().await;
            }
        };
        // Spawned, so the task's cooperative budget is in force.
        #[cfg(feature = "tokio")]
        {
            let runtime = tokio::runtime::Handle::current();
            runtime
                .block_on(runtime.spawn(receive))
                .unwrap_or_else(|err| Err(format!("the receive task failed: {err}")))
        }
        #[cfg(not(feature = "tokio"))]
        {
            futures::executor::block_on(receive)
        }
    });
    drop(sender);
    drop(device);
    if let Err(message) = result {
        panic!("{name}: {message}");
    }
}
