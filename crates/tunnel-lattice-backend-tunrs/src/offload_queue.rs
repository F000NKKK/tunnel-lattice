//! Segmentation offload at the queue level for the `tun-rs` backend: the
//! open-time check of what framing a Linux TUN queue really has, and the
//! adapters that bind the shared engine to this backend's reads, writes and
//! error rules. Linux only.
//!
//! The codec (the virtio-net header, the receive split, the send coalescing)
//! and the engine (the per-queue receive staging, the batch receive drain,
//! the batch send) live in `tunnel_lattice_platform::backend::offload`, shared
//! by every backend. What stays here is what only this backend can know:
//! the `ioctl`s that observe the queue, the native reads and writes, and
//! [`TunRsRules`], which maps a failed `tun-rs` read or write through
//! `recv_contract`.
//!
//! # Framing follows the device, not the request
//!
//! `IFF_VNET_HDR` is a device-wide flag, and the kernel does not apply a
//! queue's requested flags when it attaches to a multi-queue device that
//! already has queues. So after every Linux TUN open (and every added
//! queue), [`verify_queue`] reads the device's flags (`TUNGETIFF`) and, if
//! the flag is set, its header size (`TUNGETVNETHDRSZ`), and
//! `decide_framing` picks the framing:
//!
//! | Observed | Framing |
//! |---|---|
//! | `IFF_VNET_HDR` set, header size 10 | offload: a staging buffer is allocated and the handle reports `SEGMENTATION_OFFLOAD` |
//! | `IFF_VNET_HDR` set, any other header size | `open` fails with `Unsupported` |
//! | `IFF_VNET_HDR` clear | plain; if `tun-rs` set an offload mask for this queue, it is cleared with `TUNSETOFFLOAD(0)`, since a device without the header must not produce super-packets |
//!
//! # Receiving and sending on an offload-framed queue
//!
//! A `recv` serves the next pending segment of the last frame read, or
//! reads one frame into the staging buffer and serves its first segment; a
//! frame that fails validation, and a raw `EINVAL` read, are dropped and the
//! read retried (see `recv_contract`). A `recv_batch` fills position 0 the
//! same way, then drains with a *non-waiting* read; every failed drain read
//! other than a raw `EINTR` or `EINVAL` ends the batch (see `recv_contract`,
//! "Draining a batch"). The drain takes its read as a closure, so the
//! blocking build (`preadv2` with `RWF_NOWAIT`), `async-io` (`try_recv`) and
//! Tokio (`try_io`) share it, and so do the scripted-read unit tests.
//!
//! A `send_batch` coalesces runs of one flow into single gather writes. A
//! raw `EINVAL` on a run's write means the kernel refused the header and
//! wrote nothing (a kernel without USO, for example), so the run's packets
//! are written again one by one.

use std::io::{self, IoSlice};
#[cfg(feature = "async")]
use std::sync::MutexGuard;

use tunnel_lattice_core::Result;
use tunnel_lattice_model::DeviceKind;
#[cfg(feature = "async")]
use tunnel_lattice_platform::backend::offload::Staging;
use tunnel_lattice_platform::backend::offload::{self as engine, OffloadRules};

use crate::open_contract::HostOs;
use crate::recv_contract::{self, DrainStep, Step};

/// This backend's answers to the shared engine's questions about a failed
/// read or write: the `recv_contract` rules for `os` and `kind`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct TunRsRules {
    os: HostOs,
    kind: DeviceKind,
}

impl TunRsRules {
    /// The rules of a `kind` handle on `os`.
    pub(crate) fn new(os: HostOs, kind: DeviceKind) -> Self {
        Self { os, kind }
    }
}

impl OffloadRules<io::Error> for TunRsRules {
    fn read_error(&self, err: io::Error) -> Step {
        recv_contract::offload_recv_error_step(self.os, self.kind, err)
    }

    fn drain_error(&self, err: &io::Error) -> DrainStep {
        recv_contract::drain_error_step(self.os, true, err)
    }

    fn write_error(&self, err: io::Error) -> Step {
        recv_contract::send_step(self.os, self.kind, Err(err))
    }

    fn run_refused(&self, err: &io::Error) -> bool {
        recv_contract::is_refused_offload_write(self.os, err)
    }
}

/// Checks the framing of the Linux TUN queue on `fd` (see the module docs)
/// and returns its receive staging when it is offload-framed. `mask_set`
/// is whether `tun-rs` set an offload mask for this queue.
///
/// Errors from the read-only ioctls, and from the mask repair, are mapped
/// like any other native error; the caller drops the handle.
#[cfg(target_os = "linux")]
pub(crate) fn verify_queue(fd: std::os::fd::RawFd, mask_set: bool) -> Result<Option<OffloadRx>> {
    use crate::io_error;

    let vnet_hdr = tun_flags(fd).map_err(io_error)? & libc::IFF_VNET_HDR != 0;
    let hdr_size = if vnet_hdr {
        Some(vnet_hdr_size(fd).map_err(io_error)?)
    } else {
        None
    };
    let framing = engine::decide_framing(vnet_hdr, hdr_size, mask_set)?;
    if framing.clear_mask {
        clear_offload_mask(fd).map_err(io_error)?;
    }
    Ok(framing.offload.then(OffloadRx::new))
}

/// The device's TUN flags (`ifr_flags` of `TUNGETIFF`).
#[cfg(target_os = "linux")]
fn tun_flags(fd: std::os::fd::RawFd) -> io::Result<libc::c_int> {
    // `TUNGETIFF` copies out a whole `struct ifreq`: the name, then the
    // flags as a `short` right after the `IFNAMSIZ`-byte name.
    let mut req = [0u8; size_of::<libc::ifreq>()];
    // SAFETY: `fd` is the caller's open `/dev/net/tun` descriptor, valid
    // for the call. `TUNGETIFF` writes exactly `sizeof(struct ifreq)` bytes
    // through the pointer, and `req` is that many writable bytes that live
    // across the call; the kernel copies them out byte-wise, so no
    // alignment is needed. Nothing else is read or written.
    let rc = unsafe { libc::ioctl(fd, libc::TUNGETIFF, req.as_mut_ptr()) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    let flags = req
        .get(libc::IFNAMSIZ..libc::IFNAMSIZ + 2)
        .and_then(|bytes| <[u8; 2]>::try_from(bytes).ok())
        .ok_or_else(|| io::Error::other("short ifreq"))?;
    Ok(libc::c_int::from(u16::from_ne_bytes(flags)))
}

/// The device's virtio-net header size (`TUNGETVNETHDRSZ`).
#[cfg(target_os = "linux")]
fn vnet_hdr_size(fd: std::os::fd::RawFd) -> io::Result<i32> {
    let mut size: libc::c_int = 0;
    // SAFETY: `fd` is the caller's open `/dev/net/tun` descriptor, valid
    // for the call. `TUNGETVNETHDRSZ` writes one `int` through the
    // pointer, which points at `size`, a live, aligned `c_int`. Nothing
    // else is read or written.
    let rc = unsafe { libc::ioctl(fd, libc::TUNGETVNETHDRSZ, &raw mut size) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(size)
}

/// Clears the device's offload mask with `TUNSETOFFLOAD(0)`. The mask is
/// device-wide, so this affects every queue of the device; it is only
/// issued on a device that has no header framing, where any mask is wrong
/// for every user.
#[cfg(target_os = "linux")]
fn clear_offload_mask(fd: std::os::fd::RawFd) -> io::Result<()> {
    let none: libc::c_ulong = 0;
    // SAFETY: `fd` is the caller's open `/dev/net/tun` descriptor, valid
    // for the call. `TUNSETOFFLOAD` takes its argument as an integer by
    // value and reads no memory through it; `0` enables no offload. The
    // call has no effect on any Rust-owned memory.
    let rc = unsafe { libc::ioctl(fd, libc::TUNSETOFFLOAD, none) };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// The receive staging of one offload-framed queue: the shared engine's
/// staging, with this backend's blocking and async receive loops around it.
/// Allocated only for such a queue, owned by its handle, never shared with
/// another queue.
pub(crate) struct OffloadRx(engine::OffloadRx);

impl OffloadRx {
    /// A fresh staging buffer with nothing pending.
    pub(crate) fn new() -> Self {
        Self(engine::OffloadRx::new())
    }

    /// Locks the staging (see `engine::OffloadRx::lock`).
    #[cfg(feature = "async")]
    pub(crate) fn lock(&self) -> MutexGuard<'_, Staging> {
        self.0.lock()
    }

    /// How many frames this queue has split (a test diagnostic).
    #[cfg(test)]
    pub(crate) fn split_frames(&self) -> usize {
        self.0.split_frames()
    }

    /// Blocking `recv`: a batch of one, which therefore never drains.
    #[cfg_attr(
        all(feature = "async", not(test)),
        expect(dead_code, reason = "async builds receive through an async loop")
    )]
    pub(crate) fn recv_blocking(
        &self,
        os: HostOs,
        kind: DeviceKind,
        out: &mut [u8],
        read: impl FnMut(&mut [u8]) -> io::Result<usize>,
    ) -> Result<usize> {
        self.0.recv_blocking(out, read, &TunRsRules::new(os, kind))
    }

    /// Blocking `recv_batch` (see the module docs): position 0 serves a
    /// pending segment, or reads frames with `read` (one blocking native
    /// read into the given buffer) until one yields a packet or an error;
    /// then the staging drains with `read_nowait`, which must never wait.
    /// Holds the lock across `read`, which is what a blocking caller does
    /// anyway.
    #[cfg_attr(
        all(feature = "async", not(test)),
        expect(dead_code, reason = "async builds receive through an async loop")
    )]
    pub(crate) fn recv_batch_blocking(
        &self,
        os: HostOs,
        kind: DeviceKind,
        bufs: &mut [&mut [u8]],
        lens: &mut [usize],
        read: impl FnMut(&mut [u8]) -> io::Result<usize>,
        read_nowait: impl FnMut(&mut [u8]) -> io::Result<usize>,
    ) -> Result<usize> {
        self.0
            .recv_batch_blocking(bufs, lens, read, read_nowait, &TunRsRules::new(os, kind))
    }

    /// The `async-io` `recv`: a batch of one through
    /// [`Self::recv_batch_async_io`], which therefore never drains.
    #[cfg(all(target_os = "linux", feature = "async", not(feature = "tokio")))]
    pub(crate) async fn recv_async_io(
        &self,
        os: HostOs,
        kind: DeviceKind,
        handle: &tun_rs::AsyncDevice,
        out: &mut [u8],
    ) -> Result<usize> {
        let mut lens = [0];
        self.recv_batch_async_io(os, kind, handle, &mut [out], &mut lens)
            .await
            .map(|_| lens[0])
    }

    /// The `async-io` receive loop: does one non-blocking read into the
    /// staging buffer first, and waits for readiness on the `tun-rs` handle
    /// only after that read found the queue empty (`WouldBlock`), as
    /// `async-io`'s own `read_with` and the plain receive path do. Once
    /// position 0 holds a packet, the staging drains the rest with
    /// `try_recv`, which never waits.
    ///
    /// The read must come first: a fresh `async-io` readiness wait never
    /// completes on its first poll, only once the reactor has delivered an
    /// event, so waiting before every read would cost a reactor round trip
    /// per frame even while frames are queued.
    #[cfg(all(target_os = "linux", feature = "async", not(feature = "tokio")))]
    pub(crate) async fn recv_batch_async_io(
        &self,
        os: HostOs,
        kind: DeviceKind,
        handle: &tun_rs::AsyncDevice,
        bufs: &mut [&mut [u8]],
        lens: &mut [usize],
    ) -> Result<usize> {
        if recv_contract::batch_capacity(bufs, lens) == 0 {
            return Ok(0);
        }
        let rules = TunRsRules::new(os, kind);
        let mut read_nowait = |buf: &mut [u8]| handle.try_recv(buf);
        // The outcome of the last readiness wait; an error is handled
        // like a failed read.
        let mut ready = Ok(());
        loop {
            // From the lock to the return, or to the readiness wait, nothing
            // awaits: the lock is never held across an `.await`, and a
            // frame that was read is in the staging buffer before this
            // future can be dropped.
            {
                let mut staging = self.lock();
                if let Some(result) = staging.pending_batch(bufs, lens, &mut read_nowait, &rules) {
                    return result;
                }
                let result = match std::mem::replace(&mut ready, Ok(())) {
                    Ok(()) => handle.try_recv(staging.read_buf()),
                    Err(err) => Err(err),
                };
                match result {
                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => {}
                    result => {
                        if let Some(result) =
                            staging.read_batch(result, bufs, lens, &mut read_nowait, &rules)
                        {
                            return result;
                        }
                        continue;
                    }
                }
            }
            // The queue is empty: `async-io` re-arms its readiness
            // registration, and the next frame wakes this wait.
            ready = handle.readable().await;
        }
    }
}

/// Blocking `send_batch` on an offload-framed queue (see the module docs):
/// `writev` is one blocking gather write to the queue.
#[cfg_attr(
    all(feature = "async", not(test)),
    expect(dead_code, reason = "async builds block on `send_batch_async`")
)]
pub(crate) fn send_batch_blocking(
    os: HostOs,
    kind: DeviceKind,
    packets: &[&[u8]],
    udp_gso: bool,
    writev: impl FnMut(&[IoSlice<'_>]) -> io::Result<usize>,
) -> Result<usize> {
    engine::send_batch_blocking(packets, udp_gso, writev, &TunRsRules::new(os, kind))
}

/// One async gather write to a queue, as [`send_batch_async`] uses it.
/// Compiled where an async handle exists, and for the unit tests.
#[cfg(any(feature = "async", test))]
pub(crate) trait AsyncGatherWrite {
    /// Writes `bufs` as one frame, waiting for the queue to be writable.
    /// Dropping the future before it completes writes nothing.
    fn write_gather(
        &self,
        bufs: &[IoSlice<'_>],
    ) -> impl std::future::Future<Output = io::Result<usize>> + Send;
}

#[cfg(feature = "async")]
impl AsyncGatherWrite for tun_rs::AsyncDevice {
    fn write_gather(
        &self,
        bufs: &[IoSlice<'_>],
    ) -> impl std::future::Future<Output = io::Result<usize>> + Send {
        self.send_vectored(bufs)
    }
}

/// Binds this backend's [`AsyncGatherWrite`] to the shared engine's trait
/// of the same name, whose `Error` is `io::Error` here.
#[cfg(any(feature = "async", test))]
struct Gather<'w, W: ?Sized>(&'w W);

#[cfg(any(feature = "async", test))]
impl<W: AsyncGatherWrite + ?Sized> engine::AsyncGatherWrite for Gather<'_, W> {
    type Error = io::Error;

    fn write_gather(
        &self,
        bufs: &[IoSlice<'_>],
    ) -> impl std::future::Future<Output = io::Result<usize>> + Send {
        self.0.write_gather(bufs)
    }
}

/// Async `send_batch` on an offload-framed queue (see the module docs).
///
/// The only await is each write itself, and nothing is locked. A future
/// dropped before it completes has written an unknown prefix of
/// `packets`: each write wrote its packets whole or not at all, and no
/// packet was written twice.
#[cfg(any(feature = "async", test))]
pub(crate) async fn send_batch_async<W>(
    os: HostOs,
    kind: DeviceKind,
    packets: &[&[u8]],
    udp_gso: bool,
    queue: &W,
) -> Result<usize>
where
    W: AsyncGatherWrite + Sync + ?Sized,
{
    engine::send_batch_async(packets, udp_gso, &Gather(queue), &TunRsRules::new(os, kind)).await
}

/// Hand-built offload frames shared by the receive tests here and in the
/// async receive paths.
#[cfg(test)]
pub(crate) mod test_frames {
    use tunnel_lattice_platform::backend::offload::{
        F_NEEDS_CSUM, GSO_UDP_L4, VNET_HDR_LEN, VirtioNetHdr,
    };

    /// The payload of segment `k` of a [`udp4_frame`], `len` bytes long.
    pub(crate) fn segment_payload(k: usize, len: usize) -> Vec<u8> {
        (0..len).map(|i| (k * 31 + i) as u8).collect()
    }

    /// A USO super-frame as the kernel hands it over: vnet header, IPv4
    /// (20 bytes) and UDP (8 bytes) headers, and `count` payloads of
    /// `gso_size` bytes (the last `last` bytes long). The split recomputes
    /// every length and checksum, so the input carries zeros there.
    pub(crate) fn udp4_frame(gso_size: usize, count: usize, last: usize) -> Vec<u8> {
        let hdr = VirtioNetHdr {
            flags: F_NEEDS_CSUM,
            gso_type: GSO_UDP_L4,
            hdr_len: 28,
            gso_size: u16::try_from(gso_size).expect("a gso_size that fits"),
            csum_start: 20,
            csum_offset: 6,
        };
        let mut frame = hdr.encode().to_vec();
        frame.extend_from_slice(&[0x45, 0, 0, 0, 0, 7, 0x40, 0, 64, 17, 0, 0]);
        frame.extend_from_slice(&[10, 0, 0, 1, 10, 0, 0, 2]);
        frame.extend_from_slice(&[0x9c, 0x40, 0, 9, 0, 0, 0, 0]);
        for k in 0..count {
            let len = if k + 1 == count { last } else { gso_size };
            frame.extend_from_slice(&segment_payload(k, len));
        }
        let total = u16::try_from(frame.len() - VNET_HDR_LEN).expect("a frame that fits");
        frame[VNET_HDR_LEN + 2..VNET_HDR_LEN + 4].copy_from_slice(&total.to_be_bytes());
        frame
    }

    /// Checks `packet` is segment `k` of a [`udp4_frame`], with a `len`-byte
    /// payload.
    pub(crate) fn assert_segment(packet: &[u8], k: usize, len: usize) {
        assert_eq!(packet.len(), 28 + len, "segment {k}");
        assert_eq!(&packet[28..], segment_payload(k, len), "segment {k}");
        let id = u16::from_be_bytes([packet[4], packet[5]]);
        assert_eq!(id, 7u16.wrapping_add(k as u16), "segment {k}");
    }

    /// TCP ACK, the flag every [`tcp4_packet`] of a run carries.
    pub(crate) const TCP_ACK: u8 = 0x10;
    /// TCP PSH, which ends a coalesced run.
    pub(crate) const TCP_PSH: u8 = 0x08;

    /// Fills in the IPv4 total length and header checksum and the L4
    /// checksum of an IPv4 packet with a 20-byte header.
    fn finish_ipv4(mut packet: Vec<u8>, csum_offset: usize) -> Vec<u8> {
        use tunnel_lattice_platform::backend::offload::{checksum, sum_words};

        let total = u16::try_from(packet.len()).expect("a short packet");
        packet[2..4].copy_from_slice(&total.to_be_bytes());
        let ip_sum = checksum(&packet[..20], 0);
        packet[10..12].copy_from_slice(&ip_sum.to_be_bytes());
        let l4_len = u16::try_from(packet.len() - 20).expect("a short packet");
        let [l0, l1] = l4_len.to_be_bytes();
        let pseudo = sum_words(&[0, packet[9], l0, l1], sum_words(&packet[12..20], 0));
        let l4_sum = match checksum(&packet[20..], pseudo) {
            0 if packet[9] == 17 => 0xffff,
            sum => sum,
        };
        packet[20 + csum_offset..22 + csum_offset].copy_from_slice(&l4_sum.to_be_bytes());
        packet
    }

    /// An IPv4/UDP packet 10.0.0.1:`sport` → 10.0.0.2:4789 with IPv4 id
    /// `id`, carrying `payload`, with valid checksums.
    pub(crate) fn udp4_packet(id: u16, sport: u16, payload: &[u8]) -> Vec<u8> {
        let mut packet = vec![0x45, 0, 0, 0];
        packet.extend_from_slice(&id.to_be_bytes());
        packet.extend_from_slice(&[0x40, 0, 64, 17, 0, 0, 10, 0, 0, 1, 10, 0, 0, 2]);
        packet.extend_from_slice(&sport.to_be_bytes());
        packet.extend_from_slice(&4789u16.to_be_bytes());
        let udp_len = u16::try_from(8 + payload.len()).expect("a short payload");
        packet.extend_from_slice(&udp_len.to_be_bytes());
        packet.extend_from_slice(&[0, 0]);
        packet.extend_from_slice(payload);
        finish_ipv4(packet, 6)
    }

    /// An IPv4/TCP packet 10.0.0.1:40000 → 10.0.0.2:5201 (20-byte headers)
    /// with IPv4 id `id`, sequence number `seq` and `flags`, carrying
    /// `payload`, with valid checksums.
    pub(crate) fn tcp4_packet(id: u16, seq: u32, flags: u8, payload: &[u8]) -> Vec<u8> {
        let mut packet = vec![0x45, 0, 0, 0];
        packet.extend_from_slice(&id.to_be_bytes());
        packet.extend_from_slice(&[0x40, 0, 64, 6, 0, 0, 10, 0, 0, 1, 10, 0, 0, 2]);
        packet.extend_from_slice(&40000u16.to_be_bytes());
        packet.extend_from_slice(&5201u16.to_be_bytes());
        packet.extend_from_slice(&seq.to_be_bytes());
        packet.extend_from_slice(&1u32.to_be_bytes());
        packet.extend_from_slice(&[0x50, flags, 0x20, 0, 0, 0, 0, 0]);
        packet.extend_from_slice(payload);
        finish_ipv4(packet, 16)
    }

    /// `count` packets of one TCP flow that coalesce into one run: `len`
    /// payload bytes each, contiguous sequence numbers, consecutive ids,
    /// and PSH on the last one only.
    pub(crate) fn tcp4_flow(count: usize, len: usize) -> Vec<Vec<u8>> {
        (0..count)
            .map(|k| {
                let flags = if k + 1 == count {
                    TCP_ACK | TCP_PSH
                } else {
                    TCP_ACK
                };
                let id = 100u16.wrapping_add(k as u16);
                let seq = 1000u32.wrapping_add((k * len) as u32);
                tcp4_packet(id, seq, flags, &segment_payload(k, len))
            })
            .collect()
    }

    /// Splits one written frame back into its IP packets, as the kernel
    /// would on the receiving side.
    pub(crate) fn split(frame: &[u8]) -> Vec<Vec<u8>> {
        use tunnel_lattice_platform::backend::offload::{Segment, SplitCursor};

        let mut cursor = SplitCursor::new(frame).expect("a valid frame");
        let mut packets = Vec::new();
        let mut out = vec![0u8; 65_536];
        while let Segment::Packet(len) = cursor.write_next(frame, &mut out).expect("a segment") {
            packets.push(out[..len].to_vec());
        }
        packets
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::test_frames::{assert_segment, udp4_frame};
    use tunnel_lattice_core::Error;
    use tunnel_lattice_platform::backend::offload::{MAX_SEGMENTS, STAGING_LEN, VNET_HDR_NONE};

    use super::*;

    const TUN: DeviceKind = DeviceKind::Tun;

    /// Scripted native reads: each copies a frame (or fails).
    struct Reads(VecDeque<io::Result<Vec<u8>>>);

    impl Reads {
        fn new(script: impl IntoIterator<Item = io::Result<Vec<u8>>>) -> Self {
            Self(script.into_iter().collect())
        }

        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let frame = self.0.pop_front().expect("no more scripted reads")?;
            // The kernel truncates silently to the buffer.
            let n = frame.len().min(buf.len());
            buf[..n].copy_from_slice(&frame[..n]);
            Ok(frame.len().min(buf.len()))
        }
    }

    fn recv(rx: &OffloadRx, reads: &mut Reads, out: &mut [u8]) -> Result<usize> {
        rx.recv_blocking(HostOs::Linux, TUN, out, |buf| reads.read(buf))
    }

    #[test]
    fn one_read_serves_every_segment_in_order() {
        let rx = OffloadRx::new();
        let mut reads = Reads::new([Ok(udp4_frame(1000, 5, 300))]);
        let mut out = vec![0u8; 2000];
        for k in 0..5 {
            let len = recv(&rx, &mut reads, &mut out).expect("a segment");
            assert_segment(&out[..len], k, if k == 4 { 300 } else { 1000 });
        }
        assert!(reads.0.is_empty(), "one read for the whole super-packet");
    }

    /// A segment that does not fit is dropped alone; the next `recv` with
    /// a large enough buffer serves the segment after it.
    #[test]
    fn a_too_small_buffer_drops_one_segment_only() {
        let rx = OffloadRx::new();
        let mut reads = Reads::new([Ok(udp4_frame(1000, 3, 1000))]);
        let mut out = vec![0u8; 2000];
        let len = recv(&rx, &mut reads, &mut out).unwrap();
        assert_segment(&out[..len], 0, 1000);
        assert!(matches!(
            recv(&rx, &mut reads, &mut out[..100]),
            Err(Error::BufferTooSmall)
        ));
        let len = recv(&rx, &mut reads, &mut out).unwrap();
        assert_segment(&out[..len], 2, 1000);
    }

    /// A malformed frame, an oversized one (the sentinel byte filled) and
    /// a raw `EINVAL` are dropped and the read retried; `EINTR` too. The
    /// next valid frame is served by the same `recv`.
    #[test]
    fn dropped_frames_and_retried_errors_read_again() {
        let rx = OffloadRx::new();
        let mut bad_type = udp4_frame(1000, 2, 10);
        bad_type[1] = 0x33;
        let oversized = vec![0u8; STAGING_LEN + 10];
        let mut reads = Reads::new([
            Ok(bad_type),
            Ok(vec![0u8; 4]),
            Ok(oversized),
            Err(io::Error::from_raw_os_error(22)),
            Err(io::Error::from_raw_os_error(4)),
            Ok(udp4_frame(500, 2, 500)),
        ]);
        let mut out = vec![0u8; 2000];
        let len = recv(&rx, &mut reads, &mut out).unwrap();
        assert_segment(&out[..len], 0, 500);
        assert!(reads.0.is_empty());
        let len = recv(&rx, &mut reads, &mut out).unwrap();
        assert_segment(&out[..len], 1, 500);
    }

    /// A non-GSO frame is one packet, copied after the header.
    #[test]
    fn a_plain_frame_is_one_packet() {
        let rx = OffloadRx::new();
        let packet = [
            0x45u8, 0, 0, 20, 0, 0, 0, 0, 64, 17, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8,
        ];
        let frame = [VNET_HDR_NONE.as_slice(), &packet].concat();
        let mut reads = Reads::new([Ok(frame)]);
        let mut out = vec![0u8; 64];
        let len = recv(&rx, &mut reads, &mut out).unwrap();
        assert_eq!(&out[..len], packet);
    }

    /// The teardown errors end the call; a zero-length read is passed
    /// through rather than retried.
    #[test]
    fn errors_and_empty_reads_are_reported() {
        let rx = OffloadRx::new();
        let mut reads = Reads::new([
            Err(io::Error::from_raw_os_error(77)),
            Err(io::Error::from_raw_os_error(14)),
            Ok(Vec::new()),
        ]);
        let mut out = vec![0u8; 64];
        assert!(matches!(
            recv(&rx, &mut reads, &mut out),
            Err(Error::Disconnected)
        ));
        assert!(matches!(
            recv(&rx, &mut reads, &mut out),
            Err(Error::Disconnected)
        ));
        assert!(matches!(recv(&rx, &mut reads, &mut out), Ok(0)));
    }

    /// The receive split has no segment cap: a super-packet of more than
    /// the 128 segments a coalesced send may carry is served whole.
    #[test]
    fn more_than_the_coalescing_cap_of_segments_is_served() {
        let count = MAX_SEGMENTS * 4 + 3;
        let rx = OffloadRx::new();
        let mut reads = Reads::new([Ok(udp4_frame(100, count, 40))]);
        let mut out = vec![0u8; 200];
        for k in 0..count {
            let len = recv(&rx, &mut reads, &mut out).expect("a segment");
            assert_segment(&out[..len], k, if k + 1 == count { 40 } else { 100 });
        }
        assert!(reads.0.is_empty());
    }

    /// `count` receive buffers of `len` bytes each.
    struct Bufs(Vec<Vec<u8>>);

    impl Bufs {
        fn new(count: usize, len: usize) -> Self {
            Self(vec![vec![0u8; len]; count])
        }

        fn slices(&mut self) -> Vec<&mut [u8]> {
            self.0.iter_mut().map(Vec::as_mut_slice).collect()
        }
    }

    /// One blocking `recv_batch`: `reads` scripts position 0's waiting
    /// reads, `nowait` the drain's.
    fn batch(
        rx: &OffloadRx,
        reads: &mut Reads,
        nowait: &mut Reads,
        bufs: &mut Bufs,
        lens: &mut [usize],
    ) -> Result<usize> {
        rx.recv_batch_blocking(
            HostOs::Linux,
            TUN,
            &mut bufs.slices(),
            lens,
            |buf| reads.read(buf),
            |buf| nowait.read(buf),
        )
    }

    fn would_block() -> io::Result<Vec<u8>> {
        Err(io::ErrorKind::WouldBlock.into())
    }

    #[test]
    fn a_batch_with_no_capacity_reads_nothing() {
        let rx = OffloadRx::new();
        let (mut reads, mut nowait) = (Reads::new([]), Reads::new([]));
        let mut bufs = Bufs::new(4, 100);
        assert_eq!(
            batch(&rx, &mut reads, &mut nowait, &mut bufs, &mut []).unwrap(),
            0
        );
        let mut none = Bufs::new(0, 0);
        assert_eq!(
            batch(&rx, &mut reads, &mut nowait, &mut none, &mut [0; 4]).unwrap(),
            0
        );
    }

    /// Every segment of one super-frame in one call, with no further read
    /// once the capacity is reached.
    #[test]
    fn one_call_serves_every_segment_of_a_frame() {
        let rx = OffloadRx::new();
        let mut reads = Reads::new([Ok(udp4_frame(1000, 5, 300))]);
        let mut nowait = Reads::new([]);
        let mut bufs = Bufs::new(5, 2000);
        let mut lens = [0; 5];
        assert_eq!(
            batch(&rx, &mut reads, &mut nowait, &mut bufs, &mut lens).unwrap(),
            5
        );
        for (k, (buf, len)) in bufs.0.iter().zip(lens).enumerate() {
            assert_segment(&buf[..len], k, if k == 4 { 300 } else { 1000 });
        }
        assert!(reads.0.is_empty());
    }

    /// A frame larger than the batch leaves its rest pending for the next
    /// call, which is served before any read.
    #[test]
    fn the_rest_of_a_frame_stays_pending_for_the_next_call() {
        let rx = OffloadRx::new();
        let mut reads = Reads::new([Ok(udp4_frame(100, 5, 100))]);
        let mut nowait = Reads::new([would_block()]);
        let mut bufs = Bufs::new(3, 200);
        let mut lens = [0; 3];
        assert_eq!(
            batch(&rx, &mut reads, &mut nowait, &mut bufs, &mut lens).unwrap(),
            3
        );
        assert_eq!(
            batch(&rx, &mut reads, &mut nowait, &mut bufs, &mut lens).unwrap(),
            2
        );
        assert_segment(&bufs.0[0][..lens[0]], 3, 100);
        assert_segment(&bufs.0[1][..lens[1]], 4, 100);
        assert!(reads.0.is_empty() && nowait.0.is_empty());
    }

    /// The drain reads further frames without waiting and stops at the
    /// first empty or unsupported read.
    #[test]
    fn the_drain_spans_frames_until_the_queue_is_empty() {
        let plain = [
            VNET_HDR_NONE.as_slice(),
            &[
                0x45u8, 0, 0, 20, 0, 0, 0, 0, 64, 17, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8,
            ],
        ]
        .concat();
        for end in [
            would_block(),
            Err(io::Error::from_raw_os_error(11)),
            Err(io::Error::from_raw_os_error(95)),
        ] {
            let rx = OffloadRx::new();
            let mut reads = Reads::new([Ok(udp4_frame(100, 2, 100))]);
            let mut nowait = Reads::new([Ok(udp4_frame(200, 2, 50)), Ok(plain.clone()), end]);
            let mut bufs = Bufs::new(8, 300);
            let mut lens = [0; 8];
            assert_eq!(
                batch(&rx, &mut reads, &mut nowait, &mut bufs, &mut lens).unwrap(),
                5
            );
            assert_segment(&bufs.0[0][..lens[0]], 0, 100);
            assert_segment(&bufs.0[1][..lens[1]], 1, 100);
            assert_segment(&bufs.0[2][..lens[2]], 0, 200);
            assert_segment(&bufs.0[3][..lens[3]], 1, 50);
            assert_eq!(lens[4], 20);
            assert!(nowait.0.is_empty(), "the drain stopped at the empty read");
        }
    }

    /// A segment too long for its buffer at a position after the first
    /// stays pending and ends the batch; the next call with a large
    /// enough buffer serves it. At position 0 it is dropped, as by `recv`.
    #[test]
    fn a_too_small_buffer_after_the_first_keeps_the_segment() {
        let rx = OffloadRx::new();
        let mut reads = Reads::new([Ok(udp4_frame(100, 4, 100))]);
        let mut nowait = Reads::new([]);
        let mut bufs = Bufs::new(3, 200);
        bufs.0[1].truncate(50);
        let mut lens = [0; 3];
        assert_eq!(
            batch(&rx, &mut reads, &mut nowait, &mut bufs, &mut lens).unwrap(),
            1
        );
        assert_segment(&bufs.0[0][..lens[0]], 0, 100);

        let mut small = Bufs::new(2, 50);
        assert!(matches!(
            batch(&rx, &mut reads, &mut nowait, &mut small, &mut [0; 2]),
            Err(Error::BufferTooSmall)
        ));
        // Segment 1 was kept, then dropped at position 0; 2 and 3 remain.
        let mut bufs = Bufs::new(3, 200);
        let mut nowait = Reads::new([would_block()]);
        assert_eq!(
            batch(&rx, &mut reads, &mut nowait, &mut bufs, &mut lens).unwrap(),
            2
        );
        assert_segment(&bufs.0[0][..lens[0]], 2, 100);
        assert_segment(&bufs.0[1][..lens[1]], 3, 100);
    }

    /// A frame the split does not accept, a raw `EINVAL` and `EINTR` are
    /// dropped by the drain, which goes on reading; a zero-length read is a
    /// zero-length packet.
    #[test]
    fn the_drain_drops_bad_frames_and_goes_on() {
        let rx = OffloadRx::new();
        let mut bad_type = udp4_frame(100, 2, 10);
        bad_type[1] = 0x33;
        let mut reads = Reads::new([Ok(udp4_frame(100, 1, 100))]);
        let mut nowait = Reads::new([
            Ok(bad_type),
            Err(io::Error::from_raw_os_error(22)),
            Err(io::Error::from_raw_os_error(4)),
            Ok(udp4_frame(100, 1, 60)),
            Ok(Vec::new()),
            would_block(),
        ]);
        let mut bufs = Bufs::new(4, 200);
        let mut lens = [9; 4];
        assert_eq!(
            batch(&rx, &mut reads, &mut nowait, &mut bufs, &mut lens).unwrap(),
            3
        );
        assert_segment(&bufs.0[0][..lens[0]], 0, 100);
        assert_segment(&bufs.0[1][..lens[1]], 0, 60);
        assert_eq!(lens[2], 0);
        assert!(nowait.0.is_empty());
    }

    /// Any other drain error returns the prefix; the next call's own read
    /// reports the device state behind it.
    #[test]
    fn another_drain_error_returns_the_prefix_first() {
        let rx = OffloadRx::new();
        let mut reads = Reads::new([
            Ok(udp4_frame(100, 1, 100)),
            Err(io::Error::from_raw_os_error(77)),
        ]);
        let mut nowait = Reads::new([Err(io::Error::from_raw_os_error(77))]);
        let mut bufs = Bufs::new(4, 200);
        let mut lens = [0; 4];
        assert_eq!(
            batch(&rx, &mut reads, &mut nowait, &mut bufs, &mut lens).unwrap(),
            1
        );
        assert!(matches!(
            batch(&rx, &mut reads, &mut nowait, &mut bufs, &mut lens),
            Err(Error::Disconnected)
        ));
    }

    /// Position 0 waits through dropped frames and retried errors exactly
    /// as `recv` does, then drains.
    #[test]
    fn position_zero_reads_again_like_recv() {
        let rx = OffloadRx::new();
        let mut reads = Reads::new([
            Ok(vec![0u8; 4]),
            Err(io::Error::from_raw_os_error(22)),
            Ok(udp4_frame(100, 2, 100)),
        ]);
        let mut nowait = Reads::new([would_block()]);
        let mut bufs = Bufs::new(4, 200);
        let mut lens = [0; 4];
        assert_eq!(
            batch(&rx, &mut reads, &mut nowait, &mut bufs, &mut lens).unwrap(),
            2
        );
        assert!(reads.0.is_empty() && nowait.0.is_empty());
    }
}

/// The batch send over a scripted queue, through both the blocking and the
/// async driver.
#[cfg(test)]
mod batch_tests {
    use std::collections::VecDeque;
    use std::future::Future;
    use std::pin::pin;
    use std::sync::Mutex;
    use std::task::{Context, Poll, Waker};

    use tunnel_lattice_core::{Error, PlatformErrorCode};
    use tunnel_lattice_platform::backend::offload::{MAX_SEGMENTS, VNET_HDR_LEN, VNET_HDR_NONE};

    use super::test_frames::{
        TCP_ACK, segment_payload, split, tcp4_flow, tcp4_packet, udp4_packet,
    };
    use super::*;

    const TUN: DeviceKind = DeviceKind::Tun;
    const EIO: i32 = 5;
    const EINTR: i32 = 4;
    const EINVAL: i32 = 22;
    const EBADFD: i32 = 77;

    /// What the queue does with one write.
    #[derive(Clone, Copy, Debug)]
    enum Reply {
        /// Takes the whole frame.
        Full,
        /// Reports one byte fewer than it was given, and records nothing.
        Short,
        /// Fails with this raw OS error, writing nothing.
        Fail(i32),
        /// Never completes (async only), writing nothing.
        Pending,
    }

    /// A queue that answers each write with the next scripted reply (then
    /// `Full`), and records every frame it took whole.
    struct Queue {
        replies: Mutex<VecDeque<Reply>>,
        frames: Mutex<Vec<Vec<u8>>>,
        writes: Mutex<usize>,
    }

    impl Queue {
        fn new(replies: impl IntoIterator<Item = Reply>) -> Self {
            Self {
                replies: Mutex::new(replies.into_iter().collect()),
                frames: Mutex::new(Vec::new()),
                writes: Mutex::new(0),
            }
        }

        fn frames(&self) -> Vec<Vec<u8>> {
            self.frames.lock().unwrap().clone()
        }

        fn writes(&self) -> usize {
            *self.writes.lock().unwrap()
        }

        /// One write; `None` while the scripted reply is `Pending`.
        fn write(&self, bufs: &[IoSlice<'_>]) -> Option<io::Result<usize>> {
            let mut replies = self.replies.lock().unwrap();
            let reply = replies.front().copied().unwrap_or(Reply::Full);
            if let Reply::Pending = reply {
                return None;
            }
            replies.pop_front();
            *self.writes.lock().unwrap() += 1;
            let frame: Vec<u8> = bufs.iter().flat_map(|buf| buf.iter().copied()).collect();
            Some(match reply {
                Reply::Full => {
                    let len = frame.len();
                    self.frames.lock().unwrap().push(frame);
                    Ok(len)
                }
                Reply::Short => Ok(frame.len() - 1),
                Reply::Fail(code) => Err(io::Error::from_raw_os_error(code)),
                Reply::Pending => unreachable!(),
            })
        }
    }

    impl AsyncGatherWrite for Queue {
        /// Decides only when polled, so a future dropped while `Pending`
        /// wrote nothing.
        fn write_gather(
            &self,
            bufs: &[IoSlice<'_>],
        ) -> impl std::future::Future<Output = io::Result<usize>> + Send {
            std::future::poll_fn(move |_| match self.write(bufs) {
                Some(result) => Poll::Ready(result),
                None => Poll::Pending,
            })
        }
    }

    fn poll_once<F: Future>(future: F) -> Poll<F::Output> {
        pin!(future).poll(&mut Context::from_waker(Waker::noop()))
    }

    /// Sends `packets` through the blocking driver and, on a fresh copy of
    /// the script, through the async one; checks both agree and returns
    /// the result and the frames written.
    fn send(
        replies: &[Reply],
        packets: &[Vec<u8>],
        udp_gso: bool,
    ) -> (Result<usize>, Vec<Vec<u8>>) {
        let refs: Vec<&[u8]> = packets.iter().map(Vec::as_slice).collect();
        let sync_queue = Queue::new(replies.iter().copied());
        let sync = send_batch_blocking(HostOs::Linux, TUN, &refs, udp_gso, |iov| {
            sync_queue
                .write(iov)
                .expect("no pending reply in a blocking test")
        });
        let async_queue = Queue::new(replies.iter().copied());
        let Poll::Ready(asynchronous) = poll_once(send_batch_async(
            HostOs::Linux,
            TUN,
            &refs,
            udp_gso,
            &async_queue,
        )) else {
            panic!("the scripted queue never waits here");
        };
        assert_eq!(
            format!("{sync:?}"),
            format!("{asynchronous:?}"),
            "both drivers agree"
        );
        assert_eq!(sync_queue.frames(), async_queue.frames());
        assert_eq!(sync_queue.writes(), async_queue.writes());
        (sync, sync_queue.frames())
    }

    /// The packets the frames carry, in order.
    fn unframe(frames: &[Vec<u8>]) -> Vec<Vec<u8>> {
        frames.iter().flat_map(|frame| split(frame)).collect()
    }

    /// One frame per packet, each behind the all-zero header.
    fn singles(packets: &[Vec<u8>]) -> Vec<Vec<u8>> {
        packets
            .iter()
            .map(|packet| [VNET_HDR_NONE.as_slice(), packet].concat())
            .collect()
    }

    /// `count` UDP packets of one flow, `len` payload bytes each.
    fn udp_flow(count: usize, len: usize) -> Vec<Vec<u8>> {
        (0..count)
            .map(|k| udp4_packet(7u16.wrapping_add(k as u16), 40000, &segment_payload(k, len)))
            .collect()
    }

    /// Packets that never coalesce: each from another UDP source port.
    fn mixed_flows(count: usize) -> Vec<Vec<u8>> {
        (0..count)
            .map(|k| udp4_packet(7, 40000 + k as u16, &segment_payload(k, 100)))
            .collect()
    }

    #[test]
    fn an_empty_batch_writes_nothing() {
        let (result, frames) = send(&[Reply::Fail(EIO)], &[], true);
        assert_eq!(result.unwrap(), 0);
        assert!(frames.is_empty());
    }

    /// A TCP run is one write of the run header and the payloads, which the
    /// kernel splits back into exactly the packets sent.
    #[test]
    fn a_tcp_run_is_one_write() {
        let packets = tcp4_flow(6, 700);
        for udp_gso in [false, true] {
            let (result, frames) = send(&[], &packets, udp_gso);
            assert_eq!(result.unwrap(), 6);
            assert_eq!(frames.len(), 1, "one write for the run");
            assert_eq!(frames[0].len(), VNET_HDR_LEN + 40 + 6 * 700);
            assert_eq!(unframe(&frames), packets);
        }
    }

    /// A UDP run is coalesced only on a queue with USO; without it every
    /// datagram goes out alone behind the all-zero header.
    #[test]
    fn a_udp_run_needs_uso() {
        let packets = udp_flow(5, 500);
        let (result, frames) = send(&[], &packets, true);
        assert_eq!(result.unwrap(), 5);
        assert_eq!(frames.len(), 1);
        assert_eq!(unframe(&frames), packets);

        let (result, frames) = send(&[], &packets, false);
        assert_eq!(result.unwrap(), 5);
        assert_eq!(frames, singles(&packets));
    }

    /// Packets that do not coalesce go out one by one, in order, and runs
    /// form again wherever they can.
    #[test]
    fn mixed_flows_go_out_one_by_one() {
        let packets = mixed_flows(6);
        let (result, frames) = send(&[], &packets, true);
        assert_eq!(result.unwrap(), 6);
        assert_eq!(frames, singles(&packets));

        let mut packets = mixed_flows(1);
        packets.extend(tcp4_flow(3, 100));
        packets.extend(mixed_flows(2));
        let (result, frames) = send(&[], &packets, true);
        assert_eq!(result.unwrap(), 6);
        assert_eq!(frames.len(), 4, "single, run of 3, single, single");
        assert_eq!(unframe(&frames), packets);
    }

    /// A call sends at most `MAX_SEGMENTS` packets, coalesced or not; the
    /// caller sends the rest with the next call.
    #[test]
    fn a_call_sends_at_most_max_segments() {
        let total = MAX_SEGMENTS + 72;
        let packets = udp_flow(total, 10);
        let (result, frames) = send(&[], &packets, true);
        assert_eq!(result.unwrap(), MAX_SEGMENTS);
        assert_eq!(frames.len(), 1, "one run of MAX_SEGMENTS");
        assert_eq!(unframe(&frames), packets[..MAX_SEGMENTS]);
        let (result, frames) = send(&[], &packets[MAX_SEGMENTS..], true);
        assert_eq!(result.unwrap(), 72);
        assert_eq!(unframe(&frames), packets[MAX_SEGMENTS..]);

        let packets = mixed_flows(total);
        let (result, frames) = send(&[], &packets, true);
        assert_eq!(result.unwrap(), MAX_SEGMENTS);
        assert_eq!(frames, singles(&packets[..MAX_SEGMENTS]));
    }

    /// A run the kernel refuses (`EINVAL`) wrote nothing: its packets are
    /// written again one by one, and the batch goes on after them.
    #[test]
    fn a_refused_run_is_resent_packet_by_packet() {
        let mut packets = tcp4_flow(4, 300);
        packets.extend(tcp4_flow(3, 200).into_iter().map(|mut packet| {
            packet[23] ^= 1; // another destination port: a second flow
            packet
        }));
        // The changed port invalidates the second flow's checksums, so its
        // packets go out one by one; only the first run is refused.
        let (result, frames) = send(&[Reply::Fail(EINVAL)], &packets, true);
        assert_eq!(result.unwrap(), 7);
        assert_eq!(frames, singles(&packets));

        // Refused, then the first single fails: nothing was sent.
        let packets = tcp4_flow(4, 300);
        let (result, frames) = send(&[Reply::Fail(EINVAL), Reply::Fail(EIO)], &packets, true);
        assert!(matches!(result, Err(Error::InvalidState)), "{result:?}");
        assert!(frames.is_empty());

        // Refused, two singles sent, then a failure: the sent prefix.
        let replies = [
            Reply::Fail(EINVAL),
            Reply::Full,
            Reply::Full,
            Reply::Fail(EBADFD),
        ];
        let (result, frames) = send(&replies, &packets, true);
        assert_eq!(result.unwrap(), 2);
        assert_eq!(frames, singles(&packets[..2]));
    }

    /// A failure on the first write is the call's error, mapped like a
    /// `send` error; after `k` packets were written it is `Ok(k)`, and the
    /// next call, starting at the failed packet, reports the error.
    #[test]
    fn errors_follow_the_prefix_contract() {
        let packets = mixed_flows(4);
        let (result, frames) = send(&[Reply::Fail(EBADFD)], &packets, true);
        assert!(matches!(result, Err(Error::Disconnected)), "{result:?}");
        assert!(frames.is_empty());

        let replies = [Reply::Full, Reply::Full, Reply::Fail(EIO)];
        let (result, frames) = send(&replies, &packets, true);
        assert_eq!(result.unwrap(), 2);
        assert_eq!(frames, singles(&packets[..2]));
        let (result, _) = send(&[Reply::Fail(EIO)], &packets[2..], true);
        assert!(matches!(result, Err(Error::InvalidState)), "{result:?}");

        // A run, then a failure: the run's packets were sent.
        let mut packets = tcp4_flow(3, 100);
        packets.extend(mixed_flows(2));
        let (result, frames) = send(&[Reply::Full, Reply::Fail(EIO)], &packets, true);
        assert_eq!(result.unwrap(), 3);
        assert_eq!(unframe(&frames), packets[..3]);
    }

    /// `EINVAL` on a single packet is not a refused run: it keeps its
    /// generic meaning, and nothing is retried.
    #[test]
    fn einval_on_a_single_packet_is_an_error() {
        let packets = mixed_flows(2);
        let (result, frames) = send(&[Reply::Fail(EINVAL)], &packets, true);
        assert!(matches!(result, Err(Error::Platform(_))), "{result:?}");
        assert!(frames.is_empty());
        let queue = Queue::new([Reply::Fail(EINVAL)]);
        let refs: Vec<&[u8]> = packets.iter().map(Vec::as_slice).collect();
        let _ = send_batch_blocking(HostOs::Linux, TUN, &refs, true, |iov| {
            queue.write(iov).unwrap()
        });
        assert_eq!(queue.writes(), 1);
    }

    /// `EINTR` repeats the same write, for a run and for a single packet.
    #[test]
    fn eintr_repeats_the_write() {
        let mut packets = tcp4_flow(3, 100);
        packets.extend(mixed_flows(1));
        let replies = [Reply::Fail(EINTR), Reply::Full, Reply::Fail(EINTR)];
        let (result, frames) = send(&replies, &packets, true);
        assert_eq!(result.unwrap(), 4);
        assert_eq!(frames.len(), 2);
        assert_eq!(unframe(&frames), packets);
    }

    /// A write that reports fewer bytes than it was given fails like any
    /// other write: none of its packets counts as sent.
    #[test]
    fn a_short_write_is_an_error() {
        let packets = tcp4_flow(3, 100);
        let (result, _) = send(&[Reply::Short], &packets, true);
        assert!(
            matches!(result, Err(Error::Platform(PlatformErrorCode::Unknown))),
            "{result:?}"
        );
        let packets = mixed_flows(3);
        let (result, frames) = send(&[Reply::Full, Reply::Short], &packets, true);
        assert_eq!(result.unwrap(), 1);
        assert_eq!(frames, singles(&packets[..1]));
    }

    /// An async batch dropped while a write waits has written a prefix of
    /// whole packets: here the first packet, alone, then nothing of the
    /// run it was waiting to write.
    #[test]
    fn a_dropped_async_batch_leaves_a_whole_prefix() {
        let mut packets = mixed_flows(1);
        packets.extend(tcp4_flow(3, 100));
        let refs: Vec<&[u8]> = packets.iter().map(Vec::as_slice).collect();
        let queue = Queue::new([Reply::Full, Reply::Pending]);
        let pending = poll_once(send_batch_async(HostOs::Linux, TUN, &refs, true, &queue));
        assert!(pending.is_pending());
        assert_eq!(queue.frames(), singles(&packets[..1]));

        // The caller resumes after the sent prefix; nothing is written twice.
        queue.replies.lock().unwrap().clear();
        let Poll::Ready(result) = poll_once(send_batch_async(
            HostOs::Linux,
            TUN,
            &refs[1..],
            true,
            &queue,
        )) else {
            panic!("the queue no longer waits");
        };
        assert_eq!(result.unwrap(), 3);
        assert_eq!(unframe(&queue.frames()), packets);
    }

    /// The run header and payload slices are passed to the write as they
    /// are: one slice for the header, then one per packet, no copies.
    #[test]
    fn a_run_is_written_as_header_and_payload_slices() {
        let packets = tcp4_flow(3, 100);
        let refs: Vec<&[u8]> = packets.iter().map(Vec::as_slice).collect();
        let mut seen = Vec::new();
        let result = send_batch_blocking(HostOs::Linux, TUN, &refs, true, |iov| {
            seen = iov
                .iter()
                .map(|slice| (slice.as_ptr(), slice.len()))
                .collect();
            Ok(iov.iter().map(|slice| slice.len()).sum())
        });
        assert_eq!(result.unwrap(), 3);
        assert_eq!(seen.len(), 4);
        assert_eq!(seen[0].1, VNET_HDR_LEN + 40);
        for (k, packet) in packets.iter().enumerate() {
            assert_eq!(seen[k + 1], (packet[40..].as_ptr(), 100), "payload {k}");
        }
        // A packet that cannot coalesce is the zero header and the packet.
        let lone = tcp4_packet(1, 1, TCP_ACK, b"x");
        let result = send_batch_blocking(HostOs::Linux, TUN, &[&lone], true, |iov| {
            seen = iov
                .iter()
                .map(|slice| (slice.as_ptr(), slice.len()))
                .collect();
            Ok(iov.iter().map(|slice| slice.len()).sum())
        });
        assert_eq!(result.unwrap(), 1);
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].1, VNET_HDR_LEN);
        assert_eq!(seen[1], (lone.as_ptr(), 41), "the packet itself");
    }
}

/// The batch send through real `tun-rs` handles over a datagram socket
/// standing in for an offload-framed queue (one datagram per write, like a
/// tun write).
#[cfg(all(test, target_os = "linux"))]
mod batch_handle_tests {
    use std::os::fd::IntoRawFd;
    use std::os::unix::net::UnixDatagram;

    use super::test_frames::{split, tcp4_flow, udp4_packet};
    use super::*;

    /// A datagram pair: ours is handed to `tun-rs`, the peer reads.
    fn pair() -> (std::os::fd::RawFd, UnixDatagram) {
        let (ours, theirs) = UnixDatagram::pair().expect("create a datagram pair");
        ours.set_nonblocking(true).expect("make it non-blocking");
        theirs
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .expect("set a read timeout");
        (ours.into_raw_fd(), theirs)
    }

    /// A TCP run then a lone UDP packet: two writes, which the peer reads
    /// as two frames that split back into the packets sent.
    fn batch() -> Vec<Vec<u8>> {
        let mut packets = tcp4_flow(5, 500);
        packets.push(udp4_packet(1, 1, b"lone"));
        packets
    }

    fn assert_received(peer: &UnixDatagram, packets: &[Vec<u8>]) {
        let mut buf = vec![0u8; 65_536];
        let mut received = Vec::new();
        for _ in 0..2 {
            let n = peer.recv(&mut buf).expect("a frame");
            received.extend(split(&buf[..n]));
        }
        assert_eq!(received, packets);
    }

    #[cfg(not(feature = "async"))]
    #[test]
    fn a_sync_handle_writes_runs_and_singles() {
        let (fd, peer) = pair();
        // SAFETY: `fd` is an open descriptor whose sole ownership passes to
        // the handle, which closes it when dropped.
        let device = unsafe { tun_rs::SyncDevice::from_fd(fd) }.expect("wrap the descriptor");
        let packets = batch();
        let refs: Vec<&[u8]> = packets.iter().map(Vec::as_slice).collect();
        let sent = send_batch_blocking(HostOs::Linux, DeviceKind::Tun, &refs, true, |iov| {
            device.send_vectored(iov)
        });
        assert_eq!(sent.unwrap(), packets.len());
        assert_received(&peer, &packets);
    }

    #[cfg(feature = "async")]
    #[test]
    fn an_async_handle_writes_runs_and_singles() {
        let (fd, peer) = pair();
        let packets = batch();
        let send = async {
            // SAFETY: `fd` is an open descriptor whose sole ownership
            // passes to the handle, which closes it when dropped.
            let device = unsafe { tun_rs::AsyncDevice::from_fd(fd) }.expect("wrap the descriptor");
            let refs: Vec<&[u8]> = packets.iter().map(Vec::as_slice).collect();
            send_batch_async(HostOs::Linux, DeviceKind::Tun, &refs, true, &device).await
        };
        #[cfg(feature = "tokio")]
        let sent = tokio::runtime::Builder::new_multi_thread()
            .enable_io()
            .build()
            .expect("build a Tokio runtime")
            .block_on(send);
        #[cfg(not(feature = "tokio"))]
        let sent = futures::executor::block_on(send);
        assert_eq!(sent.unwrap(), packets.len());
        assert_received(&peer, &packets);
    }
}

/// The `async-io` receive loop over a datagram socket standing in for an
/// offload-framed queue (one datagram per frame, like a tun read).
#[cfg(all(test, target_os = "linux", feature = "async", not(feature = "tokio")))]
mod async_io_tests {
    use std::os::fd::IntoRawFd;
    use std::os::unix::net::UnixDatagram;

    use futures::FutureExt;
    use futures::executor::block_on;

    use super::test_frames::{assert_segment, udp4_frame};
    use super::*;

    /// A `tun-rs` async handle reading one end of a datagram pair, and the
    /// other end to send frames with.
    fn queue() -> (tun_rs::AsyncDevice, UnixDatagram) {
        let (ours, theirs) = UnixDatagram::pair().expect("create a datagram pair");
        ours.set_nonblocking(true).expect("make it non-blocking");
        // SAFETY: `into_raw_fd` hands over sole ownership of an open,
        // non-blocking descriptor; the handle closes it when dropped.
        let device = unsafe { tun_rs::AsyncDevice::from_fd(ours.into_raw_fd()) }
            .expect("wrap the descriptor");
        (device, theirs)
    }

    fn recv(rx: &OffloadRx, device: &tun_rs::AsyncDevice, out: &mut [u8]) -> Result<usize> {
        block_on(rx.recv_async_io(HostOs::Linux, DeviceKind::Tun, device, out))
    }

    /// One frame read serves every segment, in order, and the remaining
    /// segments of a frame come before the next frame.
    #[test]
    fn serves_segments_in_order_across_frames() {
        let (device, peer) = queue();
        let rx = OffloadRx::new();
        peer.send(&udp4_frame(300, 3, 300)).expect("send a frame");
        peer.send(&udp4_frame(200, 2, 50)).expect("send a frame");
        let mut out = vec![0u8; 1500];
        for (k, len) in [(0, 300), (1, 300), (2, 300), (0, 200), (1, 50)] {
            let n = recv(&rx, &device, &mut out).expect("a segment");
            assert_segment(&out[..n], k, len);
        }
    }

    /// A `recv` dropped while it waits for readiness, or dropped before it
    /// is polled at all mid-split, loses nothing: the next `recv` serves
    /// the next segment.
    #[test]
    fn a_dropped_recv_loses_no_segment() {
        let (device, peer) = queue();
        let rx = OffloadRx::new();
        let mut out = vec![0u8; 1500];
        {
            let waiting = rx
                .recv_async_io(HostOs::Linux, DeviceKind::Tun, &device, &mut out)
                .now_or_never();
            assert!(waiting.is_none(), "nothing to read yet");
        }
        peer.send(&udp4_frame(100, 3, 100)).expect("send a frame");
        let n = recv(&rx, &device, &mut out).expect("segment 0");
        assert_segment(&out[..n], 0, 100);
        drop(rx.recv_async_io(HostOs::Linux, DeviceKind::Tun, &device, &mut out));
        let n = recv(&rx, &device, &mut out).expect("segment 1");
        assert_segment(&out[..n], 1, 100);
        let n = rx
            .recv_async_io(HostOs::Linux, DeviceKind::Tun, &device, &mut out)
            .now_or_never()
            .expect("a pending segment is served without waiting")
            .expect("segment 2");
        assert_segment(&out[..n], 2, 100);
    }

    /// A frame that is already queued is read on the first poll, with no
    /// readiness wait first: every `recv` here, the first of each frame
    /// included, completes without ever returning `Pending`. A wait on
    /// every read would cost a reactor round trip per frame even on a busy
    /// queue (`async-io` reports readiness only through the reactor, never
    /// from a fresh wait), which starved a forwarder of throughput.
    #[test]
    fn a_queued_frame_is_read_without_waiting_for_readiness() {
        let (device, peer) = queue();
        let rx = OffloadRx::new();
        for _ in 0..3 {
            peer.send(&udp4_frame(100, 2, 60)).expect("send a frame");
        }
        let mut out = vec![0u8; 1500];
        for _ in 0..3 {
            for (k, len) in [(0, 100), (1, 60)] {
                let n = rx
                    .recv_async_io(HostOs::Linux, DeviceKind::Tun, &device, &mut out)
                    .now_or_never()
                    .expect("a queued frame is read on the first poll")
                    .expect("a segment");
                assert_segment(&out[..n], k, len);
            }
        }
        let waiting = rx
            .recv_async_io(HostOs::Linux, DeviceKind::Tun, &device, &mut out)
            .now_or_never();
        assert!(waiting.is_none(), "an empty queue waits for readiness");
    }

    /// A `recv` that found the queue empty waits for readiness and is
    /// woken by a frame that arrives later.
    #[test]
    fn a_recv_on_an_empty_queue_is_woken_by_a_later_frame() {
        let (device, peer) = queue();
        let rx = OffloadRx::new();
        let sender = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            peer.send(&udp4_frame(100, 2, 40)).expect("send a frame");
            peer
        });
        let mut out = vec![0u8; 1500];
        let n = recv(&rx, &device, &mut out).expect("segment 0");
        assert_segment(&out[..n], 0, 100);
        let _peer = sender.join().expect("the sender thread");
        let n = recv(&rx, &device, &mut out).expect("segment 1");
        assert_segment(&out[..n], 1, 40);
    }

    /// One `recv_batch` serves every queued segment, across frames, with
    /// `try_recv` reads that never wait, and returns at the empty queue.
    #[test]
    fn a_batch_drains_every_queued_frame() {
        let (device, peer) = queue();
        let rx = OffloadRx::new();
        peer.send(&udp4_frame(300, 3, 300)).expect("send a frame");
        peer.send(&udp4_frame(200, 2, 50)).expect("send a frame");
        let mut bufs = vec![vec![0u8; 1500]; 8];
        let mut lens = [0; 8];
        let n = block_on(rx.recv_batch_async_io(
            HostOs::Linux,
            DeviceKind::Tun,
            &device,
            &mut bufs.iter_mut().map(Vec::as_mut_slice).collect::<Vec<_>>(),
            &mut lens,
        ))
        .expect("a batch");
        assert_eq!(n, 5);
        for (i, (k, len)) in [(0, 300), (1, 300), (2, 300), (0, 200), (1, 50)]
            .into_iter()
            .enumerate()
        {
            assert_segment(&bufs[i][..lens[i]], k, len);
        }
    }

    /// A batch dropped while it waits takes nothing; a batch never polled
    /// mid-split takes nothing either; the rest of a frame larger than the
    /// batch is served, without waiting, by the next one.
    #[test]
    fn a_dropped_batch_loses_no_segment() {
        let (device, peer) = queue();
        let rx = OffloadRx::new();
        let mut bufs = vec![vec![0u8; 1500]; 2];
        let mut lens = [0; 2];
        let batch = |bufs: &mut Vec<Vec<u8>>, lens: &mut [usize]| {
            let mut slices: Vec<&mut [u8]> = bufs.iter_mut().map(Vec::as_mut_slice).collect();
            rx.recv_batch_async_io(HostOs::Linux, DeviceKind::Tun, &device, &mut slices, lens)
                .now_or_never()
        };
        assert!(batch(&mut bufs, &mut lens).is_none(), "nothing to read yet");
        peer.send(&udp4_frame(100, 5, 100)).expect("send a frame");
        for first in [0, 2] {
            let n = batch(&mut bufs, &mut lens)
                .expect("a queued frame is read on the first poll")
                .expect("a batch");
            assert_eq!(n, 2);
            assert_segment(&bufs[0][..lens[0]], first, 100);
            assert_segment(&bufs[1][..lens[1]], first + 1, 100);
        }
        let n = batch(&mut bufs, &mut lens)
            .expect("a pending segment is served without waiting")
            .expect("segment 4");
        assert_eq!(n, 1);
        assert_segment(&bufs[0][..lens[0]], 4, 100);
    }
}
