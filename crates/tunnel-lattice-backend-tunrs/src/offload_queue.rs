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
//!
//! # Sending a batch on an offload-framed queue
//!
//! [`send_batch_blocking`] and `send_batch_async` send at most
//! [`MAX_SEGMENTS`] packets per call, walking them in order. At each
//! position, [`plan_run`] looks for a run of adjacent packets of one flow
//! that the kernel will split back into exactly those packets (UDP only
//! when the queue has USO); a run goes out as one gather write of the run
//! header and each packet's payload, with no payload copied, and anything
//! else as one packet behind an all-zero header. Results follow the
//! `send_batch` prefix contract: `Ok(n)` means the first `n` packets were
//! written whole, in order, and nothing after them; an error on the first
//! write is returned as is; an error after `k > 0` packets gives `Ok(k)`.
//!
//! A raw `EINVAL` on a run's write means the kernel refused the header and
//! wrote nothing (a kernel without USO, for example), so the run's packets
//! are written again one by one. A write that reports fewer bytes than it
//! was given is an error like any other failed write: the kernel's TUN
//! write takes the whole frame or nothing, so this never happens on a real
//! queue, but if it did, no packet of that write is counted as sent.

use std::io::{self, IoSlice};
use std::sync::{Mutex, MutexGuard};

use tunnel_lattice_core::{Error, Result};
use tunnel_lattice_model::DeviceKind;

use crate::offload::{
    MAX_SEGMENTS, Run, STAGING_LEN, Segment, SplitCursor, VNET_HDR_LEN, VNET_HDR_NONE, plan_run,
};
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

/// The most I/O slices one batch write uses: a run's header, then one
/// payload per packet of the run.
const BATCH_IOV: usize = MAX_SEGMENTS + 1;

/// The error a batch write that reported fewer bytes than it was given
/// becomes, before the `send` error rules map it (see the module docs).
const SHORT_WRITE: &str = "short write to an offload-framed queue";

/// One native write of a send batch.
#[derive(Clone, Copy, Debug)]
enum BatchWrite {
    /// The next `run.count()` packets as one super-packet.
    Run(Run),
    /// The next packet alone, behind [`VNET_HDR_NONE`].
    Single,
}

impl BatchWrite {
    /// How many packets the write carries.
    fn count(&self) -> usize {
        match self {
            Self::Run(run) => run.count(),
            Self::Single => 1,
        }
    }

    /// The length of the frame the write carries for the head of `rest`,
    /// header included: what a complete write reports.
    fn frame_len(&self, rest: &[&[u8]]) -> usize {
        let packet_len = match self {
            Self::Run(run) => run.total_len(),
            Self::Single => rest.first().map_or(0, |packet| packet.len()),
        };
        VNET_HDR_LEN.saturating_add(packet_len)
    }

    /// Fills `iov` with what to write for the head of `rest` and returns
    /// how many slices it used: the run header and each packet's payload,
    /// or the all-zero header and the packet.
    fn iovecs<'s>(&'s self, rest: &[&'s [u8]], iov: &mut [IoSlice<'s>; BATCH_IOV]) -> usize {
        let mut used = 0;
        let mut push = |slice: &'s [u8]| {
            if let Some(entry) = iov.get_mut(used) {
                *entry = IoSlice::new(slice);
                used += 1;
            }
        };
        match self {
            Self::Run(run) => {
                push(run.header());
                for &packet in rest.iter().take(run.count()) {
                    push(run.payload(packet));
                }
            }
            Self::Single => {
                push(&VNET_HDR_NONE);
                push(rest.first().copied().unwrap_or_default());
            }
        }
        used
    }
}

/// The progress of one `send_batch` call on an offload-framed queue.
struct BatchSend<'a, 'p> {
    /// The packets this call may send: at most [`MAX_SEGMENTS`].
    packets: &'a [&'p [u8]],
    /// Whether the queue has USO, so UDP runs may be coalesced.
    udp_gso: bool,
    /// How many packets from the head were written whole.
    sent: usize,
    /// How many of the next packets go out one by one, because the kernel
    /// refused their coalesced write.
    singles: usize,
}

impl<'a, 'p> BatchSend<'a, 'p> {
    fn new(packets: &'a [&'p [u8]], udp_gso: bool) -> Self {
        Self {
            packets: packets.get(..MAX_SEGMENTS).unwrap_or(packets),
            udp_gso,
            sent: 0,
            singles: 0,
        }
    }

    /// The packets not written yet.
    fn rest(&self) -> &'a [&'p [u8]] {
        self.packets.get(self.sent..).unwrap_or_default()
    }

    /// The next write, or `None` once every packet of the call is written.
    fn next_write(&self) -> Option<BatchWrite> {
        let rest = self.rest();
        if rest.is_empty() {
            return None;
        }
        if self.singles > 0 {
            return Some(BatchWrite::Single);
        }
        Some(plan_run(rest, self.udp_gso).map_or(BatchWrite::Single, BatchWrite::Run))
    }

    /// Takes the `result` of `write`, which was given `expected` bytes.
    /// `None` means "go on with the next write" (the same one again after
    /// a transient error); `Some` is the call's result.
    fn finish(
        &mut self,
        os: HostOs,
        kind: DeviceKind,
        write: &BatchWrite,
        expected: usize,
        result: io::Result<usize>,
    ) -> Option<Result<usize>> {
        let failure = match result {
            Ok(written) if written == expected => {
                self.sent = self.sent.saturating_add(write.count());
                self.singles = self.singles.saturating_sub(write.count());
                return None;
            }
            Ok(_) => io::Error::new(io::ErrorKind::WriteZero, SHORT_WRITE),
            Err(err) => {
                if let BatchWrite::Run(run) = write
                    && recv_contract::is_refused_offload_write(os, &err)
                {
                    self.singles = run.count();
                    return None;
                }
                err
            }
        };
        match recv_contract::send_step(os, kind, Err(failure)) {
            Step::Retry => None,
            Step::Done(Err(err)) if self.sent == 0 => Some(Err(err)),
            Step::Done(_) => Some(Ok(self.sent)),
        }
    }
}

/// Blocking `send_batch` on an offload-framed queue (see the module docs):
/// `writev` is one blocking gather write to the queue.
#[cfg_attr(
    all(target_os = "linux", feature = "async", not(test)),
    expect(dead_code, reason = "async builds block on `send_batch_async`")
)]
pub(crate) fn send_batch_blocking(
    os: HostOs,
    kind: DeviceKind,
    packets: &[&[u8]],
    udp_gso: bool,
    mut writev: impl FnMut(&[IoSlice<'_>]) -> io::Result<usize>,
) -> Result<usize> {
    let mut batch = BatchSend::new(packets, udp_gso);
    while let Some(write) = batch.next_write() {
        let mut iov = [IoSlice::new(&[]); BATCH_IOV];
        let used = write.iovecs(batch.rest(), &mut iov);
        let expected = write.frame_len(batch.rest());
        let result = writev(iov.get(..used).unwrap_or_default());
        if let Some(result) = batch.finish(os, kind, &write, expected, result) {
            return result;
        }
    }
    Ok(batch.sent)
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

#[cfg(all(target_os = "linux", feature = "async"))]
impl AsyncGatherWrite for tun_rs::AsyncDevice {
    fn write_gather(
        &self,
        bufs: &[IoSlice<'_>],
    ) -> impl std::future::Future<Output = io::Result<usize>> + Send {
        self.send_vectored(bufs)
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
    let mut batch = BatchSend::new(packets, udp_gso);
    while let Some(write) = batch.next_write() {
        let mut iov = [IoSlice::new(&[]); BATCH_IOV];
        let used = write.iovecs(batch.rest(), &mut iov);
        let expected = write.frame_len(batch.rest());
        let result = queue
            .write_gather(iov.get(..used).unwrap_or_default())
            .await;
        if let Some(result) = batch.finish(os, kind, &write, expected, result) {
            return result;
        }
    }
    Ok(batch.sent)
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

    /// TCP ACK, the flag every [`tcp4_packet`] of a run carries.
    pub(crate) const TCP_ACK: u8 = 0x10;
    /// TCP PSH, which ends a coalesced run.
    pub(crate) const TCP_PSH: u8 = 0x08;

    /// Fills in the IPv4 total length and header checksum and the L4
    /// checksum of an IPv4 packet with a 20-byte header.
    fn finish_ipv4(mut packet: Vec<u8>, csum_offset: usize) -> Vec<u8> {
        use crate::offload::{checksum, sum_words};

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
        use crate::offload::{Segment, SplitCursor};

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

/// The batch send over a scripted queue, through both the blocking and the
/// async driver.
#[cfg(test)]
mod batch_tests {
    use std::collections::VecDeque;
    use std::future::Future;
    use std::pin::pin;
    use std::sync::Mutex;
    use std::task::{Context, Poll, Waker};

    use tunnel_lattice_core::PlatformErrorCode;

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
}
