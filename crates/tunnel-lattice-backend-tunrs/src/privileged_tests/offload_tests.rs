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
//!
//! In-order arrival alone would also pass if the kernel delivered every
//! segment as its own packet, so wherever the queue has the offload the
//! traffic needs (USO for these bursts, TSO for the TCP test), a test also
//! requires the queue's test-only split counter to show that at least one
//! super-packet was read and split.

use std::io::Write;
use std::net::{SocketAddr, TcpStream, UdpSocket};
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

/// How many super-packets `device` has read and split so far (its staging's
/// test-only counter); 0 for a plainly framed queue. Call it between
/// `recv`s.
fn split_frames(device: &TunRsDevice) -> usize {
    device
        .offload
        .as_ref()
        .map_or(0, crate::offload_queue::OffloadRx::split_frames)
}

/// Whether a test must see a split: logs the decision for the CI output.
fn require_split(what: &str, offload: bool) -> bool {
    if !offload {
        eprintln!("{what}: the queue lacks the offload, so the stack segments first");
    }
    offload
}

/// How many whole bursts a receive reads, at most, waiting for one that was
/// read as a super-packet and split.
const MAX_BURSTS: usize = 20;

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
/// `recv`s return one whole burst segment by segment. With `require_split`,
/// reads on (up to [`MAX_BURSTS`] whole bursts) until one was read as a
/// super-packet and split by the queue.
fn assert_receives_a_burst(device: TunRsDevice, subnet: [u8; 3], require_split: bool) {
    let name = device.snapshot().expect("snapshot").name;
    let device = Arc::new(device);
    let target = route_traffic_into(DeviceKind::Tun, &name, subnet);
    let sender = GsoSender::start(target);
    let port = sender.port;
    let result = within_deadline(&device, &name, move |device| {
        let mut check = BurstCheck::new(port);
        let mut buf = vec![0u8; usize::from(MTU)];
        let mut bursts = 0;
        loop {
            match PacketIo::recv(device, &mut buf) {
                Ok(n) => {
                    if !check.feed(&buf[..n])? {
                        continue;
                    }
                    bursts += 1;
                    let splits = split_frames(device);
                    if !require_split || splits > 0 {
                        eprintln!("{bursts} whole bursts, {splits} super-packets split");
                        return Ok(());
                    }
                    if bursts == MAX_BURSTS {
                        return Err(format!(
                            "{bursts} whole bursts arrived, but no super-packet was split"
                        ));
                    }
                    check = BurstCheck::new(port);
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
/// MTU-sized segments, each with valid checksums, none lost; with USO the
/// queue reads it as one super-packet and splits it.
#[test]
#[ignore = "requires CAP_NET_ADMIN to open and address a TUN device"]
fn a_gso_burst_is_received_segment_by_segment() {
    enter_runtime!();
    let device = open(DeviceConfig::new(DeviceKind::Tun).with_offload(true));
    assert!(has_offload(&device));
    let split = require_split("UDP GSO burst", device.handle.udp_gso());
    assert_receives_a_burst(device, [10, 204, subnet_octet()], split);
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
    ipv4_packet(src, dst, 0, &udp_header(4000, port, payload.len()), payload)
}

/// A UDP header from `sport` to `dport` for a `payload_len`-byte payload,
/// checksum zero (filled in by [`ipv4_packet`]).
fn udp_header(sport: u16, dport: u16, payload_len: usize) -> Vec<u8> {
    let udp_len = u16::try_from(8 + payload_len).expect("a short packet");
    let mut header = sport.to_be_bytes().to_vec();
    header.extend_from_slice(&dport.to_be_bytes());
    header.extend_from_slice(&udp_len.to_be_bytes());
    header.extend_from_slice(&[0, 0]);
    header
}

/// A 20-byte TCP header from `sport` to `dport` with sequence number
/// `seq`, ACK set, checksum zero (filled in by [`ipv4_packet`]).
fn tcp_header(sport: u16, dport: u16, seq: u32) -> Vec<u8> {
    let mut header = sport.to_be_bytes().to_vec();
    header.extend_from_slice(&dport.to_be_bytes());
    header.extend_from_slice(&seq.to_be_bytes());
    header.extend_from_slice(&1u32.to_be_bytes());
    header.extend_from_slice(&[0x50, 0x10, 0x20, 0, 0, 0, 0, 0]);
    header
}

/// An IPv4 packet from `src` to `dst` with IPv4 id `id`, carrying the L4
/// header `l4` (UDP if it is 8 bytes long, TCP otherwise) and `payload`,
/// with valid IPv4 and L4 checksums.
fn ipv4_packet(src: [u8; 4], dst: [u8; 4], id: u16, l4: &[u8], payload: &[u8]) -> Vec<u8> {
    let (proto, csum_at) = if l4.len() == 8 { (17, 26) } else { (6, 36) };
    let total = u16::try_from(20 + l4.len() + payload.len()).expect("a short packet");
    let l4_len = total - 20;
    let mut packet = vec![0x45, 0, 0, 0, 0, 0, 0x40, 0, 64, proto, 0, 0];
    packet[2..4].copy_from_slice(&total.to_be_bytes());
    packet[4..6].copy_from_slice(&id.to_be_bytes());
    packet.extend_from_slice(&src);
    packet.extend_from_slice(&dst);
    packet.extend_from_slice(l4);
    packet.extend_from_slice(payload);
    if packet.len() % 2 == 1 {
        packet.push(0);
    }
    let ip_sum = !ones_complement_sum(&packet[..20]);
    packet[10..12].copy_from_slice(&ip_sum.to_be_bytes());
    let [l0, l1] = l4_len.to_be_bytes();
    let pseudo = [0, proto, l0, l1];
    let sum = u32::from(ones_complement_sum(&packet[12..20]))
        + u32::from(ones_complement_sum(&pseudo))
        + u32::from(ones_complement_sum(&packet[20..]));
    let folded = (sum & 0xffff) + (sum >> 16);
    let folded = (folded & 0xffff) + (folded >> 16);
    let l4_sum = match !u16::try_from(folded).expect("folded into 16 bits") {
        0 if proto == 17 => 0xffff,
        sum => sum,
    };
    packet[csum_at..csum_at + 2].copy_from_slice(&l4_sum.to_be_bytes());
    packet.truncate(usize::from(total));
    packet
}

/// The `send_batch` flavors this build has: blocking, and async with an
/// async feature.
fn batch_modes() -> &'static [bool] {
    if cfg!(feature = "async") {
        &[false, true]
    } else {
        &[false]
    }
}

/// One `send_batch` call, blocking or (with an async feature) async.
fn send_batch_once(
    device: &TunRsDevice,
    packets: &[&[u8]],
    asynchronous: bool,
) -> tunnel_lattice_core::Result<usize> {
    if !asynchronous {
        return PacketIo::send_batch(device, packets);
    }
    #[cfg(feature = "tokio")]
    {
        tokio::runtime::Handle::current().block_on(
            tunnel_lattice_platform::AsyncPacketIo::send_batch(device, packets),
        )
    }
    #[cfg(all(feature = "async", not(feature = "tokio")))]
    {
        futures::executor::block_on(tunnel_lattice_platform::AsyncPacketIo::send_batch(
            device, packets,
        ))
    }
    #[cfg(not(feature = "async"))]
    {
        unreachable!("no async send_batch without an async feature")
    }
}

/// Sends every packet with `send_batch`, starting each call after the
/// packets the previous one sent, and returns each call's count. Fails on
/// an error or an empty call.
fn send_all(device: &TunRsDevice, packets: &[Vec<u8>], asynchronous: bool) -> Vec<usize> {
    let refs: Vec<&[u8]> = packets.iter().map(Vec::as_slice).collect();
    let mut counts = Vec::new();
    let mut sent = 0;
    while sent < refs.len() {
        match send_batch_once(device, &refs[sent..], asynchronous) {
            Ok(n) if n > 0 => {
                counts.push(n);
                sent += n;
            }
            other => panic!("send_batch at packet {sent} (async {asynchronous}): {other:?}"),
        }
    }
    counts
}

/// The UDP payload of packet `k` of round `round`: `len` bytes.
fn round_payload(round: u8, k: usize, len: usize) -> Vec<u8> {
    let mut payload = vec![round, u8::try_from(k).expect("fewer than 256 packets")];
    payload.extend((2..len).map(|i| (i * 13 + k) as u8));
    payload
}

/// Addresses `device` with `subnet.1` and binds a host UDP socket there.
fn host_udp_socket(device: &TunRsDevice, subnet: [u8; 3]) -> UdpSocket {
    let name = device.snapshot().expect("snapshot").name;
    route_traffic_into(DeviceKind::Tun, &name, subnet);
    let [a, b, c] = subnet;
    let host = UdpSocket::bind(format!("{a}.{b}.{c}.1:0")).expect("bind the host socket");
    host.set_read_timeout(Some(Duration::from_millis(500)))
        .expect("set a read timeout");
    host
}

/// Sends rounds of `count` UDP datagrams of `len` payload bytes from
/// `subnet.2` to `host` through `device` with `send_batch`, until one round
/// arrives (the first may be sent before the route is usable). Datagram `k`
/// comes from source port `4000 + k % flows`, so `flows > 1` interleaves
/// flows that cannot be coalesced. Each round's datagrams must arrive
/// whole, in order, and from the right port; `check_counts` sees each
/// round's `send_batch` counts.
fn assert_udp_batch_arrives(
    device: &TunRsDevice,
    host: &UdpSocket,
    subnet: [u8; 3],
    shape: (usize, usize, u16),
    check_counts: impl Fn(&[usize]),
) {
    let (count, len, flows) = shape;
    let [a, b, c] = subnet;
    let port = host.local_addr().expect("the host address").port();
    let mut round: u8 = 0;
    for &asynchronous in batch_modes() {
        let mut arrived = false;
        for _ in 0..10 {
            round = round.wrapping_add(1);
            let packets: Vec<Vec<u8>> = (0..count)
                .map(|k| {
                    let payload = round_payload(round, k, len);
                    let flow = u16::try_from(k % usize::from(flows)).expect("few flows");
                    let id = u16::from(round).wrapping_mul(1000).wrapping_add(k as u16);
                    let l4 = udp_header(4000 + flow, port, len);
                    ipv4_packet([a, b, c, 2], [a, b, c, 1], id, &l4, &payload)
                })
                .collect();
            check_counts(&send_all(device, &packets, asynchronous));
            let mut next = 0;
            let mut buf = vec![0u8; 65_536];
            while next < count {
                let Ok((n, from)) = host.recv_from(&mut buf) else {
                    break;
                };
                if buf.first() != Some(&round) {
                    continue;
                }
                assert_eq!(
                    &buf[..n],
                    round_payload(round, next, len),
                    "round {round} (async {asynchronous}): datagram {next}"
                );
                let flow = u16::try_from(next % usize::from(flows)).expect("few flows");
                assert_eq!(from.port(), 4000 + flow, "datagram {next}");
                next += 1;
            }
            assert!(
                next == 0 || next == count,
                "round {round} (async {asynchronous}): only {next} of {count} datagrams arrived"
            );
            if next == count {
                arrived = true;
                break;
            }
        }
        assert!(arrived, "async {asynchronous}: no round arrived");
    }
}

/// A batch of UDP datagrams of one flow sent with `send_batch` on an
/// offload-framed queue reaches a host socket complete and in order,
/// whether the kernel takes them as one super-packet (with USO) or one by
/// one.
#[test]
#[ignore = "requires CAP_NET_ADMIN to open and address a TUN device"]
fn a_udp_batch_reaches_the_host_whole_and_in_order() {
    enter_runtime!();
    let device = open(DeviceConfig::new(DeviceKind::Tun).with_offload(true));
    assert!(has_offload(&device));
    eprintln!("udp_gso: {}", device.handle.udp_gso());
    let subnet = [10, 210, subnet_octet()];
    let host = host_udp_socket(&device, subnet);
    assert_udp_batch_arrives(&device, &host, subnet, (32, GSO_SIZE, 1), |counts| {
        assert_eq!(counts, [32], "one call sends a batch below the limit");
    });
}

/// A batch of more than 128 packets is sent in short batches of at most
/// 128: the first call reports exactly 128, and every packet arrives.
#[test]
#[ignore = "requires CAP_NET_ADMIN to open and address a TUN device"]
fn a_batch_over_the_limit_is_sent_in_short_batches() {
    enter_runtime!();
    let device = open(DeviceConfig::new(DeviceKind::Tun).with_offload(true));
    let subnet = [10, 211, subnet_octet()];
    let host = host_udp_socket(&device, subnet);
    let count = tunnel_lattice_platform::backend::offload::MAX_SEGMENTS + 12;
    assert_udp_batch_arrives(&device, &host, subnet, (count, 64, 1), |counts| {
        assert_eq!(
            counts,
            [tunnel_lattice_platform::backend::offload::MAX_SEGMENTS, 12]
        );
    });
}

/// Interleaved flows cannot be coalesced, so `send_batch` sends them one
/// by one; they still arrive complete and in order.
#[test]
#[ignore = "requires CAP_NET_ADMIN to open and address a TUN device"]
fn a_batch_of_interleaved_flows_is_sent_packet_by_packet() {
    enter_runtime!();
    let device = open(DeviceConfig::new(DeviceKind::Tun).with_offload(true));
    let subnet = [10, 212, subnet_octet()];
    let host = host_udp_socket(&device, subnet);
    assert_udp_batch_arrives(&device, &host, subnet, (24, 500, 2), |counts| {
        assert_eq!(counts, [24]);
    });
}

/// A raw IPv4 socket for TCP bound to one host address: it receives a
/// copy of every TCP packet delivered to that address, before TCP itself
/// sees it (which answers our unsolicited segments with resets, harmlessly).
struct RawTcp(std::os::fd::OwnedFd);

impl RawTcp {
    fn bind(addr: [u8; 4]) -> Self {
        use std::os::fd::FromRawFd;

        // SAFETY: a plain `socket` call with constant arguments; it reads
        // and writes no memory.
        let fd = unsafe {
            libc::socket(
                libc::AF_INET,
                libc::SOCK_RAW | libc::SOCK_CLOEXEC,
                libc::IPPROTO_TCP,
            )
        };
        assert!(
            fd >= 0,
            "create a raw TCP socket: {}",
            std::io::Error::last_os_error()
        );
        // SAFETY: `fd` was just returned by `socket` and is owned by
        // nothing else; the `OwnedFd` closes it when dropped.
        let socket = Self(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) });
        let sin = libc::sockaddr_in {
            sin_family: libc::sa_family_t::try_from(libc::AF_INET).expect("AF_INET fits"),
            sin_port: 0,
            sin_addr: libc::in_addr {
                s_addr: u32::from_ne_bytes(addr),
            },
            sin_zero: [0; 8],
        };
        // SAFETY: `sin` is a live, initialized `sockaddr_in`, and the length
        // passed is its size; `bind` only reads it.
        let rc = unsafe {
            libc::bind(
                fd,
                (&raw const sin).cast(),
                libc::socklen_t::try_from(size_of::<libc::sockaddr_in>()).expect("a small size"),
            )
        };
        assert_eq!(
            rc,
            0,
            "bind the raw socket: {}",
            std::io::Error::last_os_error()
        );
        let timeout = libc::timeval {
            tv_sec: 0,
            tv_usec: 500_000,
        };
        // SAFETY: `SO_RCVTIMEO` reads one `timeval` from the pointer, which
        // points at `timeout`, a live `timeval`, and the length passed is
        // its size.
        let rc = unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_RCVTIMEO,
                (&raw const timeout).cast(),
                libc::socklen_t::try_from(size_of::<libc::timeval>()).expect("a small size"),
            )
        };
        assert_eq!(rc, 0, "set a timeout: {}", std::io::Error::last_os_error());
        socket
    }

    /// The next IPv4 packet, or `None` after the 500 ms timeout.
    fn recv(&self, buf: &mut [u8]) -> Option<usize> {
        // SAFETY: the descriptor is open for the call, and `buf` is
        // `buf.len()` writable bytes that live across it.
        let n = unsafe { libc::recv(self.0.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
        usize::try_from(n).ok()
    }
}

/// A batch of TCP segments of one flow sent with `send_batch` on an
/// offload-framed queue reaches the host complete and in order: the data
/// a raw socket sees for the flow, packet by packet (as one super-packet
/// or several), is the byte stream sent, with contiguous sequence numbers.
#[test]
#[ignore = "requires CAP_NET_ADMIN to open and address a TUN device, and CAP_NET_RAW for a raw socket"]
fn a_tcp_batch_reaches_the_host_whole_and_in_order() {
    const COUNT: usize = 32;
    const SEGMENT: usize = 1000;
    const PORT: u16 = 9;
    enter_runtime!();
    let device = open(DeviceConfig::new(DeviceKind::Tun).with_offload(true));
    assert!(has_offload(&device));
    let name = device.snapshot().expect("snapshot").name;
    let subnet = [10, 213, subnet_octet()];
    route_traffic_into(DeviceKind::Tun, &name, subnet);
    let [a, b, c] = subnet;
    let raw = RawTcp::bind([a, b, c, 1]);
    let mut round: u8 = 0;
    for &asynchronous in batch_modes() {
        let mut arrived = false;
        for _ in 0..10 {
            round = round.wrapping_add(1);
            let base = u32::from(round) * 1_000_000;
            let stream: Vec<u8> = (0..COUNT)
                .flat_map(|k| round_payload(round, k, SEGMENT))
                .collect();
            let packets: Vec<Vec<u8>> = stream
                .chunks(SEGMENT)
                .enumerate()
                .map(|(k, payload)| {
                    let offset = u32::try_from(k * SEGMENT).expect("a short stream");
                    let l4 = tcp_header(4000, PORT, base + offset);
                    let id = u16::from(round).wrapping_mul(1000).wrapping_add(k as u16);
                    ipv4_packet([a, b, c, 2], [a, b, c, 1], id, &l4, payload)
                })
                .collect();
            assert_eq!(send_all(&device, &packets, asynchronous), [COUNT]);

            let mut received = Vec::new();
            let mut buf = vec![0u8; 70_000];
            while received.len() < stream.len() {
                let Some(n) = raw.recv(&mut buf) else { break };
                let packet = &buf[..n];
                let ihl = usize::from(packet[0] & 0x0f) * 4;
                let ours = packet.len() >= ihl + 20
                    && packet[9] == 6
                    && packet[12..16] == [a, b, c, 2]
                    && packet[ihl..ihl + 2] == 4000u16.to_be_bytes()
                    && packet[ihl + 2..ihl + 4] == PORT.to_be_bytes();
                if !ours {
                    continue;
                }
                let seq = u32::from_be_bytes(packet[ihl + 4..ihl + 8].try_into().unwrap());
                let Some(offset) = seq.checked_sub(base) else {
                    continue; // An earlier round.
                };
                let offset = usize::try_from(offset).expect("a small offset");
                if offset >= stream.len() {
                    continue;
                }
                let total = usize::from(u16::from_be_bytes([packet[2], packet[3]]));
                let data_at = ihl + usize::from(packet[ihl + 12] >> 4) * 4;
                assert_eq!(
                    offset,
                    received.len(),
                    "round {round} (async {asynchronous}): a gap or reordering"
                );
                received.extend_from_slice(&packet[data_at..total.min(n)]);
            }
            assert!(
                received.is_empty() || received == stream,
                "round {round} (async {asynchronous}): {} of {} bytes arrived, or they differ",
                received.len(),
                stream.len()
            );
            if received == stream {
                arrived = true;
                break;
            }
        }
        assert!(arrived, "{name} (async {asynchronous}): no round arrived");
    }
}

/// The port the device side of the TSO test listens on.
const TSO_PORT: u16 = 5201;
/// The MSS the device side announces: the MTU less the IPv4 and TCP
/// headers, so every segment the split produces is exactly MTU-sized.
const TSO_MSS: u16 = MTU - 40;
/// How many in-order stream bytes the TSO test reads before it may finish.
const TSO_BYTES: usize = 256 * 1024;
/// How many in-order stream bytes the TSO test reads, at most, waiting for
/// a super-packet to be split.
const TSO_MAX_BYTES: usize = 16 * 1024 * 1024;
/// The device side's initial sequence number.
const TSO_ISS: u32 = 1_000_000;

/// Byte `offset` of the stream the TSO test's host client sends.
fn tso_stream_byte(offset: usize) -> u8 {
    (offset % 251) as u8
}

/// A host TCP client that connects to `target` through the device
/// (retrying until the route is usable, and again whenever a connection
/// fails) and writes the [`tso_stream_byte`] stream, from offset 0 on each
/// connection, until stopped.
struct TcpClient {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl TcpClient {
    fn start(target: SocketAddr) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut chunk = vec![0u8; 64 * 1024];
                while !stop.load(Ordering::Acquire) {
                    let Ok(stream) = TcpStream::connect_timeout(&target, Duration::from_secs(2))
                    else {
                        std::thread::sleep(Duration::from_millis(100));
                        continue;
                    };
                    // Short timeouts, so a blocked write sees `stop` soon.
                    let _ = stream.set_write_timeout(Some(Duration::from_millis(200)));
                    let mut offset = 0;
                    while !stop.load(Ordering::Acquire) {
                        for (i, byte) in chunk.iter_mut().enumerate() {
                            *byte = tso_stream_byte(offset + i);
                        }
                        match (&stream).write(&chunk) {
                            Ok(n) => offset += n,
                            Err(err)
                                if matches!(
                                    err.kind(),
                                    std::io::ErrorKind::WouldBlock
                                        | std::io::ErrorKind::TimedOut
                                        | std::io::ErrorKind::Interrupted
                                ) => {}
                            Err(_) => break,
                        }
                    }
                }
            })
        };
        Self {
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for TcpClient {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Whether the TCP checksum of the IPv4 `packet` (header length `ihl`)
/// checks out.
fn tcp_checksum_ok(packet: &[u8], ihl: usize) -> bool {
    let Ok(l4_len) = u16::try_from(packet.len() - ihl) else {
        return false;
    };
    let [l0, l1] = l4_len.to_be_bytes();
    let mut l4 = packet[ihl..].to_vec();
    if l4.len() % 2 == 1 {
        l4.push(0);
    }
    checksum_ok(&[&packet[12..20], &[0, 6, l0, l1], &l4])
}

/// The device side of one TCP connection in the TSO test: it answers the
/// client's SYN, then acknowledges every segment, checking that each one is
/// a whole, checksummed, MTU-bounded packet carrying the right bytes.
struct TcpPeer {
    /// The client (`subnet.1`) and device side (`subnet.2`) addresses.
    client: [u8; 4],
    server: [u8; 4],
    /// The client port of the connection being followed, once its SYN came.
    client_port: Option<u16>,
    /// The next stream sequence number expected from the client.
    rcv_nxt: u32,
    /// The in-order stream bytes received on the connection.
    received: usize,
    /// The IPv4 id of the next reply.
    id: u16,
}

impl TcpPeer {
    fn new(subnet: [u8; 3]) -> Self {
        let [a, b, c] = subnet;
        Self {
            client: [a, b, c, 1],
            server: [a, b, c, 2],
            client_port: None,
            rcv_nxt: 0,
            received: 0,
            id: 0,
        }
    }

    /// An IPv4 TCP reply to the client: `flags`, our sequence number `seq`,
    /// acknowledging `rcv_nxt`, a 65535-byte window, and `options`.
    fn reply(&mut self, seq: u32, flags: u8, options: &[u8]) -> Vec<u8> {
        let port = self.client_port.expect("a connection");
        let doff = u8::try_from((20 + options.len()) / 4).expect("a short header") << 4;
        let mut l4 = TSO_PORT.to_be_bytes().to_vec();
        l4.extend_from_slice(&port.to_be_bytes());
        l4.extend_from_slice(&seq.to_be_bytes());
        l4.extend_from_slice(&self.rcv_nxt.to_be_bytes());
        l4.extend_from_slice(&[doff, flags, 0xff, 0xff, 0, 0, 0, 0]);
        l4.extend_from_slice(options);
        self.id = self.id.wrapping_add(1);
        ipv4_packet(self.server, self.client, self.id, &l4, &[])
    }

    /// Feeds one received packet: `Ok(Some(reply))` to send back,
    /// `Ok(None)` for nothing (another flow, or a packet that needs no
    /// answer), `Err` on a malformed or corrupt packet of the flow.
    fn feed(&mut self, packet: &[u8]) -> std::result::Result<Option<Vec<u8>>, String> {
        const SYN: u8 = 0x02;
        const RST: u8 = 0x04;
        const ACK: u8 = 0x10;
        let ihl = usize::from(packet.first().copied().unwrap_or(0) & 0x0f) * 4;
        let ours = packet.len() >= 40
            && packet[0] >> 4 == 4
            && ihl >= 20
            && packet.len() >= ihl + 20
            && packet[9] == 6
            && packet[12..16] == self.client
            && packet[16..20] == self.server
            && packet[ihl + 2..ihl + 4] == TSO_PORT.to_be_bytes();
        if !ours {
            return Ok(None);
        }
        let total = usize::from(u16::from_be_bytes([packet[2], packet[3]]));
        if total != packet.len() {
            return Err(format!(
                "a {}-byte packet with total length {total}",
                packet.len()
            ));
        }
        if packet.len() > usize::from(MTU) {
            return Err(format!("a {}-byte packet over the MTU", packet.len()));
        }
        if !checksum_ok(&[&packet[..ihl]]) {
            return Err("an invalid IPv4 header checksum".to_owned());
        }
        if !tcp_checksum_ok(packet, ihl) {
            return Err(format!("an invalid TCP checksum ({} bytes)", packet.len()));
        }
        let tcp = &packet[ihl..];
        let port = u16::from_be_bytes([tcp[0], tcp[1]]);
        let seq = u32::from_be_bytes([tcp[4], tcp[5], tcp[6], tcp[7]]);
        let flags = tcp[13];
        let data_at = usize::from(tcp[12] >> 4) * 4;
        let Some(data) = tcp.get(data_at..) else {
            return Err("a TCP header past the packet".to_owned());
        };
        if flags & SYN != 0 {
            // A new connection (the client retries until one works):
            // follow it from the start.
            self.client_port = Some(port);
            self.rcv_nxt = seq.wrapping_add(1);
            self.received = 0;
            let [m0, m1] = TSO_MSS.to_be_bytes();
            return Ok(Some(self.reply(TSO_ISS, SYN | ACK, &[2, 4, m0, m1])));
        }
        if self.client_port != Some(port) {
            return Ok(None);
        }
        if flags & RST != 0 {
            self.client_port = None;
            return Ok(None);
        }
        if data.is_empty() {
            return Ok(None);
        }
        if seq == self.rcv_nxt {
            let expected = (self.received..self.received + data.len()).map(tso_stream_byte);
            if !data.iter().copied().eq(expected) {
                return Err(format!(
                    "corrupt stream bytes at offset {} ({} bytes)",
                    self.received,
                    data.len()
                ));
            }
            self.received += data.len();
            self.rcv_nxt = self
                .rcv_nxt
                .wrapping_add(u32::try_from(data.len()).expect("a short segment"));
        }
        // In order or not (a retransmission), acknowledge what we have.
        Ok(Some(self.reply(TSO_ISS.wrapping_add(1), ACK, &[])))
    }
}

/// A host TCP client sending a bulk stream into an offload-framed queue
/// through the device (its route to the peer address) has the stack hand
/// the queue TSO super-packets: `recv` splits them into MTU-sized segments,
/// each with valid checksums, that carry the stream in order, and the
/// queue's split counter shows at least one super-packet was split. The
/// device side is a minimal TCP peer answering through the same queue.
#[test]
#[ignore = "requires CAP_NET_ADMIN to open and address a TUN device"]
fn a_tso_stream_is_received_segment_by_segment() {
    enter_runtime!();
    let device = open(DeviceConfig::new(DeviceKind::Tun).with_offload(true));
    assert!(has_offload(&device));
    let split = require_split("TCP TSO stream", device.handle.tcp_gso());
    let name = device.snapshot().expect("snapshot").name;
    let device = Arc::new(device);
    let subnet = [10, 214, subnet_octet()];
    let target = route_traffic_into(DeviceKind::Tun, &name, subnet);
    let client = TcpClient::start(SocketAddr::new(target.ip(), TSO_PORT));
    let result = within_deadline(&device, &name, move |device| {
        let mut peer = TcpPeer::new(subnet);
        let mut buf = vec![0u8; usize::from(MTU)];
        loop {
            let n = PacketIo::recv(device, &mut buf).map_err(|err| format!("recv: {err:?}"))?;
            let Some(reply) = peer.feed(&buf[..n])? else {
                continue;
            };
            PacketIo::send(device, &reply).map_err(|err| format!("send: {err:?}"))?;
            if peer.received < TSO_BYTES {
                continue;
            }
            let splits = split_frames(device);
            if !split || splits > 0 {
                eprintln!(
                    "{} stream bytes, {splits} super-packets split",
                    peer.received
                );
                return Ok(());
            }
            if peer.received >= TSO_MAX_BYTES {
                return Err(format!(
                    "{} stream bytes arrived, but no super-packet was split",
                    peer.received
                ));
            }
        }
    });
    drop(client);
    drop(device);
    if let Err(message) = result {
        panic!("{name}: {message}");
    }
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
        // No split is required: the attach that did not request offload
        // cleared the device-wide offload mask (`tun-rs` does on every
        // plain open), so the stack segments the bursts before either queue
        // sees them; the framing is what this test checks.
        assert_receives_a_burst(attached, [10, recv_subnet, subnet_octet()], false);
    }
}

/// `additional_queue` on an offload-framed queue gives another
/// offload-framed queue with its own staging, which receives a GSO burst
/// segment by segment once the original queue is gone (with USO, splitting
/// it itself).
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
    let split = require_split("added queue", queue.handle.udp_gso());
    assert_receives_a_burst(queue, [10, 208, subnet_octet()], split);
}

/// An async `recv` dropped after one poll, between the segments of a GSO
/// burst and while waiting for the next frame, loses no segment: every
/// burst is still received whole and in order. With `tokio` the dropped
/// poll is forced to yield by an exhausted cooperative budget; with
/// `async-io` it is dropped whether or not it completed (a completed one's
/// packet is checked too). With USO, at least one of the bursts must have
/// been read as a super-packet, so the drops really fell between segments.
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
    let split = require_split("dropped recv", device.handle.udp_gso());

    let owned = Arc::clone(&device);
    let result = within_deadline(&device, &name, move |_| {
        let receive = async move {
            let device = &*owned;
            // Whether enough whole bursts arrived: three, and with `split`
            // at least one read as a super-packet and split.
            let enough = |bursts: usize| -> std::result::Result<bool, String> {
                if bursts < 3 {
                    return Ok(false);
                }
                let splits = split_frames(device);
                if !split || splits > 0 {
                    eprintln!("{bursts} whole bursts, {splits} super-packets split");
                    return Ok(true);
                }
                if bursts >= MAX_BURSTS {
                    return Err(format!(
                        "{bursts} whole bursts arrived, but no super-packet was split"
                    ));
                }
                Ok(false)
            };
            let mut check = BurstCheck::new(port);
            let mut buf = vec![0u8; usize::from(MTU)];
            let mut bursts = 0;
            loop {
                let n = AsyncPacketIo::recv(device, &mut buf)
                    .await
                    .map_err(|err| format!("recv: {err:?}"))?;
                if check.feed(&buf[..n])? {
                    bursts += 1;
                    if enough(bursts)? {
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
                            if enough(bursts)? {
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

/// How many buffers each `recv_batch` in these tests offers: more than a
/// burst, so one call can take a whole burst.
const BATCH: usize = 2 * SEGMENTS;

/// Tracks what the `recv_batch` tests require: whole bursts in order (one
/// [`BurstCheck`] over every packet of every batch), at least one batch of
/// more than one packet, and with `split` at least one super-packet split.
struct BatchCheck {
    port: u16,
    check: BurstCheck,
    bursts: usize,
    largest: usize,
    split: bool,
}

impl BatchCheck {
    fn new(port: u16, split: bool) -> Self {
        Self {
            port,
            check: BurstCheck::new(port),
            bursts: 0,
            largest: 0,
            split,
        }
    }

    /// Feeds the packets of one batch: `Ok(true)` once enough was seen.
    fn feed(
        &mut self,
        device: &TunRsDevice,
        packets: impl IntoIterator<Item = Vec<u8>>,
    ) -> std::result::Result<bool, String> {
        let mut count = 0;
        let mut whole = false;
        for packet in packets {
            count += 1;
            if self.check.feed(&packet)? {
                self.bursts += 1;
                self.check = BurstCheck::new(self.port);
                whole = true;
            }
        }
        self.largest = self.largest.max(count);
        if !whole {
            return Ok(false);
        }
        let splits = split_frames(device);
        if self.largest > 1 && (!self.split || splits > 0) {
            eprintln!(
                "{} whole bursts, largest batch {}, {splits} super-packets split",
                self.bursts, self.largest
            );
            return Ok(true);
        }
        if self.bursts >= MAX_BURSTS {
            return Err(format!(
                "{} whole bursts, but the largest batch was {} and {splits} super-packets \
                 were split",
                self.bursts, self.largest
            ));
        }
        Ok(false)
    }
}

/// Addresses `device`, sends GSO bursts into it, and checks that blocking
/// `recv_batch` calls return whole bursts in order, some call more than
/// one packet, and (with `require_split`) some burst read as a super-packet.
fn assert_batches_receive_a_burst(device: TunRsDevice, subnet: [u8; 3], require_split: bool) {
    let name = device.snapshot().expect("snapshot").name;
    let device = Arc::new(device);
    let target = route_traffic_into(DeviceKind::Tun, &name, subnet);
    let sender = GsoSender::start(target);
    let port = sender.port;
    let result = within_deadline(&device, &name, move |device| {
        let mut check = BatchCheck::new(port, require_split);
        let mut bufs = vec![vec![0u8; usize::from(MTU)]; BATCH];
        let mut lens = [0usize; BATCH];
        loop {
            let mut slices: Vec<&mut [u8]> = bufs.iter_mut().map(Vec::as_mut_slice).collect();
            let n = PacketIo::recv_batch(device, &mut slices, &mut lens)
                .map_err(|err| format!("recv_batch: {err:?}"))?;
            if n == 0 || n > BATCH {
                return Err(format!("recv_batch returned {n}"));
            }
            let packets = (0..n).map(|i| bufs[i][..lens[i]].to_vec());
            if check.feed(device, packets)? {
                return Ok(());
            }
        }
    });
    drop(sender);
    drop(device);
    if let Err(message) = result {
        panic!("{name}: {message}");
    }
}

/// On a plainly framed queue `recv_batch` waits for the first packet, then
/// drains what the queue already holds: the segments the stack produced
/// for a burst come back several per call, whole and in order.
#[test]
#[ignore = "requires CAP_NET_ADMIN to open and address a TUN device"]
fn a_plain_queue_batch_drains_queued_packets() {
    enter_runtime!();
    let device = open(DeviceConfig::new(DeviceKind::Tun));
    assert!(!has_offload(&device));
    assert_batches_receive_a_burst(device, [10, 215, subnet_octet()], false);
}

/// On an offload-framed queue one `recv_batch` serves the segments of a
/// super-packet together, whole and in order.
#[test]
#[ignore = "requires CAP_NET_ADMIN to open and address a TUN device"]
fn an_offload_queue_batch_serves_a_burst_together() {
    enter_runtime!();
    let device = open(DeviceConfig::new(DeviceKind::Tun).with_offload(true));
    assert!(has_offload(&device));
    let split = require_split("offload batch", device.handle.udp_gso());
    assert_batches_receive_a_burst(device, [10, 216, subnet_octet()], split);
}

/// An added queue drains with `recv_batch` like the first one, from its
/// own staging.
#[test]
#[ignore = "requires CAP_NET_ADMIN to open and address a TUN device"]
fn an_additional_queue_batch_serves_a_burst_together() {
    enter_runtime!();
    let device = open(
        DeviceConfig::new(DeviceKind::Tun)
            .with_multi_queue(true)
            .with_offload(true),
    );
    let queue = device.additional_queue().expect("add a queue");
    assert!(has_offload(&queue));
    drop(device);
    let split = require_split("added queue batch", queue.handle.udp_gso());
    assert_batches_receive_a_burst(queue, [10, 217, subnet_octet()], split);
}

/// An async `recv_batch` dropped before it completes (by an exhausted
/// Tokio budget, or after one poll under `async-io`) loses no segment:
/// the bursts still arrive whole and in order across the calls.
#[cfg(feature = "async")]
#[test]
#[ignore = "requires CAP_NET_ADMIN to open and address a TUN device"]
fn a_dropped_async_recv_batch_loses_no_segment() {
    #[cfg(feature = "tokio")]
    use std::future::Future;
    #[cfg(feature = "tokio")]
    use std::task::Poll;

    use tunnel_lattice_platform::AsyncPacketIo;

    enter_runtime!();
    let device = open(DeviceConfig::new(DeviceKind::Tun).with_offload(true));
    let name = device.snapshot().expect("snapshot").name;
    let device = Arc::new(device);
    let target = route_traffic_into(DeviceKind::Tun, &name, [10, 218, subnet_octet()]);
    let sender = GsoSender::start(target);
    let port = sender.port;
    let split = require_split("dropped recv_batch", device.handle.udp_gso());

    let owned = Arc::clone(&device);
    let result = within_deadline(&device, &name, move |_| {
        let receive = async move {
            let device = &*owned;
            let mut check = BatchCheck::new(port, split);
            let mut bufs = vec![vec![0u8; usize::from(MTU)]; BATCH];
            let mut lens = [0usize; BATCH];
            loop {
                {
                    let mut slices: Vec<&mut [u8]> =
                        bufs.iter_mut().map(Vec::as_mut_slice).collect();
                    let n = AsyncPacketIo::recv_batch(device, &mut slices, &mut lens)
                        .await
                        .map_err(|err| format!("recv_batch: {err:?}"))?;
                    let packets = (0..n).map(|i| bufs[i][..lens[i]].to_vec());
                    if check.feed(device, packets)? {
                        return Ok(());
                    }
                }
                let mut slices: Vec<&mut [u8]> = bufs.iter_mut().map(Vec::as_mut_slice).collect();
                #[cfg(feature = "tokio")]
                {
                    while tokio::task::coop::has_budget_remaining() {
                        tokio::task::coop::consume_budget().await;
                    }
                    let mut dropped =
                        std::pin::pin!(AsyncPacketIo::recv_batch(device, &mut slices, &mut lens));
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
                    let polled_once =
                        AsyncPacketIo::recv_batch(device, &mut slices, &mut lens).now_or_never();
                    drop(slices);
                    if let Some(result) = polled_once {
                        let n = result.map_err(|err| format!("recv_batch: {err:?}"))?;
                        let packets = (0..n).map(|i| bufs[i][..lens[i]].to_vec());
                        if check.feed(device, packets)? {
                            return Ok(());
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
