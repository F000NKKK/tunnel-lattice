//! Segmentation offload at the queue level: which open requests reach
//! `tun-rs`, the open-time check of what framing a Linux TUN queue really
//! has, and the per-queue receive staging that turns one kernel read into
//! one IP packet per `recv`.
//!
//! # Framing follows the device, not the request
//!
//! `IFF_VNET_HDR` is a device-wide flag, and the kernel does not apply a
//! queue's requested flags when it attaches to a multi-queue device that
//! already has queues. So after every Linux TUN open (and every added
//! queue), [`verify_queue`] reads the device's flags (`TUNGETIFF`) and, if
//! the flag is set, its header size (`TUNGETVNETHDRSZ`), and
//! [`decide_framing`] picks the framing:
//!
//! | Observed | Framing |
//! |---|---|
//! | `IFF_VNET_HDR` set, header size 10 | offload: a staging buffer is allocated and the handle reports `SEGMENTATION_OFFLOAD` |
//! | `IFF_VNET_HDR` set, any other header size | `open` fails with `Unsupported` |
//! | `IFF_VNET_HDR` clear | plain; if `tun-rs` set an offload mask for this queue, it is cleared with `TUNSETOFFLOAD(0)`, since a device without the header must not produce super-packets |
//!
//! # Receiving on an offload-framed queue
//!
//! [`OffloadRx`] owns a staging buffer of [`STAGING_LEN`] bytes (header,
//! the largest super-packet, one sentinel byte) and a [`SplitCursor`],
//! behind a mutex. A `recv` first serves the next pending segment of the
//! last frame read, if any; otherwise it reads one frame into the staging
//! buffer, validates it, and serves its first segment. A segment that does
//! not fit the caller's buffer is dropped alone, as
//! [`Error::BufferTooSmall`]. A frame that fails validation (or filled the
//! sentinel), and a raw `EINVAL` read, are dropped and the read retried
//! (see `recv_contract`).
//!
//! The async receive loops never hold the mutex across an `.await`, and
//! never await between a successful read and the return: a dropped `recv`
//! future therefore never loses a frame it read, because its remaining
//! segments stay in the staging buffer for the next `recv`.

use std::io;
use std::sync::{Mutex, MutexGuard};

use tunnel_lattice_core::{Error, Result};
use tunnel_lattice_model::DeviceKind;

use crate::offload::{STAGING_LEN, Segment, SplitCursor, VNET_HDR_LEN};
use crate::open_contract::HostOs;
use crate::recv_contract::{self, Step};

/// Whether `open` passes a segmentation-offload request on to `tun-rs`:
/// only for a Linux TUN device. Off Linux and for TAP the request is
/// ignored (no native call, no error, plain framing), like `multi_queue`
/// off Linux; the TAP split would have to parse Ethernet, which it does
/// not.
pub(crate) const fn offload_request(os: HostOs, kind: DeviceKind, requested: bool) -> bool {
    requested && matches!(os, HostOs::Linux) && matches!(kind, DeviceKind::Tun)
}

/// The framing [`decide_framing`] chose for one queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Framing {
    /// Every packet on the queue carries a 10-byte virtio-net header.
    pub(crate) offload: bool,
    /// The device has no header framing but carries an offload mask that
    /// `tun-rs` set for this queue: clear it with `TUNSETOFFLOAD(0)`.
    pub(crate) clear_mask: bool,
}

/// Chooses a queue's framing from what the device reports: whether
/// `IFF_VNET_HDR` is set, its header size (`None` when it was not read
/// because the flag is clear), and whether `tun-rs` set an offload mask for
/// this queue (its `tcp_gso()`). See the module table.
pub(crate) fn decide_framing(
    vnet_hdr: bool,
    hdr_size: Option<i32>,
    mask_set: bool,
) -> Result<Framing> {
    if !vnet_hdr {
        return Ok(Framing {
            offload: false,
            clear_mask: mask_set,
        });
    }
    match hdr_size.map(usize::try_from) {
        Some(Ok(VNET_HDR_LEN)) => Ok(Framing {
            offload: true,
            clear_mask: false,
        }),
        _ => Err(Error::Unsupported),
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
    let framing = decide_framing(vnet_hdr, hdr_size, mask_set)?;
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

/// The receive staging of one offload-framed queue: allocated only for
/// such a queue, owned by its handle, never shared with another queue.
pub(crate) struct OffloadRx(Mutex<Staging>);

/// The last frame read and how far it has been served.
pub(crate) struct Staging {
    /// Header, packet and sentinel byte of the last read.
    buf: Box<[u8]>,
    /// How many bytes of `buf` the last read filled.
    frame_len: usize,
    /// The segments of that frame still to serve; idle when none are.
    cursor: SplitCursor,
}

impl OffloadRx {
    /// A fresh staging buffer with nothing pending.
    pub(crate) fn new() -> Self {
        Self(Mutex::new(Staging {
            buf: vec![0u8; STAGING_LEN].into_boxed_slice(),
            frame_len: 0,
            cursor: SplitCursor::IDLE,
        }))
    }

    /// Locks the staging. The split code cannot panic, so the mutex is
    /// never poisoned by it; if it ever is, the pending segments are
    /// discarded and the staging is used again.
    pub(crate) fn lock(&self) -> MutexGuard<'_, Staging> {
        self.0.lock().unwrap_or_else(|poisoned| {
            let mut staging = poisoned.into_inner();
            staging.cursor = SplitCursor::IDLE;
            self.0.clear_poison();
            staging
        })
    }

    /// Blocking `recv`: serves a pending segment, or reads frames with
    /// `read` (one blocking native read into the given buffer) until one
    /// yields a packet or an error. Holds the lock across `read`, which is
    /// what a blocking caller does anyway.
    #[cfg_attr(
        all(target_os = "linux", feature = "async", not(test)),
        expect(dead_code, reason = "async builds receive through an async loop")
    )]
    pub(crate) fn recv_blocking(
        &self,
        os: HostOs,
        kind: DeviceKind,
        out: &mut [u8],
        mut read: impl FnMut(&mut [u8]) -> io::Result<usize>,
    ) -> Result<usize> {
        let mut staging = self.lock();
        loop {
            if let Some(result) = staging.next_pending(out) {
                return result;
            }
            let result = read(staging.read_buf());
            if let Some(result) = staging.finish_read(os, kind, result, out) {
                return result;
            }
        }
    }

    /// The `async-io` receive loop: waits for readiness on the `tun-rs`
    /// handle, then does one non-blocking read into the staging buffer. A
    /// `WouldBlock` read waits again (`async-io` re-arms the readiness
    /// wait).
    #[cfg(all(target_os = "linux", feature = "async", not(feature = "tokio")))]
    pub(crate) async fn recv_async_io(
        &self,
        os: HostOs,
        kind: DeviceKind,
        handle: &tun_rs::AsyncDevice,
        out: &mut [u8],
    ) -> Result<usize> {
        loop {
            if let Some(result) = self.lock().next_pending(out) {
                return result;
            }
            let ready = handle.readable().await;
            // From here to the return, or to the next iteration, nothing
            // awaits: the lock is never held across an `.await`, and a
            // frame that was read is in the staging buffer before this
            // future can be dropped.
            let mut staging = self.lock();
            if let Some(result) = staging.next_pending(out) {
                return result;
            }
            let result = match ready {
                Ok(()) => match handle.try_recv(staging.read_buf()) {
                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => continue,
                    result => result,
                },
                Err(err) => Err(err),
            };
            if let Some(result) = staging.finish_read(os, kind, result, out) {
                return result;
            }
        }
    }
}

impl Staging {
    /// Writes the next pending segment into `out`: `Some(Ok(len))`, or
    /// `Some(Err(BufferTooSmall))` when `out` is too short (that segment is
    /// dropped, and the next call serves the one after it). `None` when no
    /// segment is pending.
    pub(crate) fn next_pending(&mut self, out: &mut [u8]) -> Option<Result<usize>> {
        if self.cursor.is_finished() {
            return None;
        }
        let frame = self.buf.get(..self.frame_len).unwrap_or_default();
        match self.cursor.write_next(frame, out) {
            Ok(Segment::Packet(len)) => Some(Ok(len)),
            Ok(Segment::BufferTooSmall { .. }) => Some(Err(Error::BufferTooSmall)),
            // `Done` cannot happen on an unfinished cursor, and a frame
            // mismatch cannot happen on the frame the cursor validated;
            // either way nothing is pending any more.
            Ok(Segment::Done) | Err(_) => {
                self.cursor = SplitCursor::IDLE;
                None
            }
        }
    }

    /// The buffer one native read fills: the whole staging buffer,
    /// sentinel included. Nothing is pending when this is called, so the
    /// read may overwrite the previous frame.
    pub(crate) fn read_buf(&mut self) -> &mut [u8] {
        &mut self.buf
    }

    /// Takes one native read's `result`: on success, validates the frame
    /// and serves its first segment into `out`; on failure, applies the
    /// offload `recv` rules. `None` means "read again": the frame was
    /// dropped, or the error is retried.
    ///
    /// A zero-length read carries no frame and is passed through as
    /// `Ok(0)`, as on a plain queue, rather than retried.
    pub(crate) fn finish_read(
        &mut self,
        os: HostOs,
        kind: DeviceKind,
        result: io::Result<usize>,
        out: &mut [u8],
    ) -> Option<Result<usize>> {
        match result {
            Ok(0) => {
                self.cursor = SplitCursor::IDLE;
                Some(Ok(0))
            }
            Ok(len) => {
                self.frame_len = len.min(self.buf.len());
                let frame = self.buf.get(..self.frame_len).unwrap_or_default();
                self.cursor = SplitCursor::new(frame).unwrap_or(SplitCursor::IDLE);
                self.next_pending(out)
            }
            Err(err) => match recv_contract::offload_recv_error_step(os, kind, err) {
                Step::Retry => None,
                Step::Done(result) => Some(result),
            },
        }
    }
}

/// Hand-built offload frames shared by the receive tests here and in the
/// async receive paths.
#[cfg(test)]
pub(crate) mod test_frames {
    use crate::offload::{F_NEEDS_CSUM, GSO_UDP_L4, VNET_HDR_LEN, VirtioNetHdr};

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
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::test_frames::{assert_segment, udp4_frame};
    use super::*;
    use crate::offload::MAX_SEGMENTS;

    const TUN: DeviceKind = DeviceKind::Tun;
    const ALL_OSES: [HostOs; 4] = [HostOs::Linux, HostOs::Macos, HostOs::Windows, HostOs::Other];

    #[test]
    fn offload_is_requested_for_linux_tun_only() {
        for os in ALL_OSES {
            for kind in [DeviceKind::Tun, DeviceKind::Tap] {
                assert!(!offload_request(os, kind, false), "{os:?} {kind:?}");
                assert_eq!(
                    offload_request(os, kind, true),
                    os == HostOs::Linux && kind == DeviceKind::Tun,
                    "{os:?} {kind:?}"
                );
            }
        }
    }

    #[test]
    fn framing_follows_the_observed_flag_and_header_size() {
        let framing = |offload, clear_mask| Framing {
            offload,
            clear_mask,
        };
        for mask in [false, true] {
            assert_eq!(
                decide_framing(true, Some(10), mask).ok(),
                Some(framing(true, false)),
                "{mask}"
            );
        }
        for size in [None, Some(0), Some(-1), Some(12), Some(20)] {
            for mask in [false, true] {
                assert!(
                    matches!(decide_framing(true, size, mask), Err(Error::Unsupported)),
                    "{size:?} {mask}"
                );
            }
        }
        for size in [None, Some(10)] {
            assert_eq!(
                decide_framing(false, size, false).ok(),
                Some(framing(false, false))
            );
            assert_eq!(
                decide_framing(false, size, true).ok(),
                Some(framing(false, true)),
                "a mask on a device without framing is repaired"
            );
        }
    }

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
        let frame = [crate::offload::VNET_HDR_NONE.as_slice(), &packet].concat();
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

    /// A poisoned staging mutex is recovered with nothing pending.
    #[test]
    fn a_poisoned_staging_is_reset() {
        let rx = std::sync::Arc::new(OffloadRx::new());
        let mut reads = Reads::new([Ok(udp4_frame(100, 3, 100))]);
        let mut out = vec![0u8; 200];
        recv(&rx, &mut reads, &mut out).unwrap();
        let poisoner = std::sync::Arc::clone(&rx);
        let _ = std::thread::spawn(move || {
            let _staging = poisoner.lock();
            panic!("poison the staging");
        })
        .join();
        assert!(rx.0.is_poisoned());
        let mut reads = Reads::new([Ok(udp4_frame(100, 2, 100))]);
        let len = recv(&rx, &mut reads, &mut out).unwrap();
        assert_segment(&out[..len], 0, 100);
        assert!(!rx.0.is_poisoned());
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
}
