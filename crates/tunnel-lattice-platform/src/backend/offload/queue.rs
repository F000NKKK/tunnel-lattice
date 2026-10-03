//! The queue-level engine of segmentation offload: the framing decision, the
//! receive staging that turns one kernel read into one IP packet per
//! `recv`, and the batch send.
//!
//! The engine does no I/O of its own. Every read and write is a closure (or
//! an [`AsyncGatherWrite`]) supplied by the backend, and the backend's own
//! rules for what a failed read or write means are an [`OffloadRules`]
//! value, so the engine names no operating system, no device kind and no
//! error type.

use std::io::IoSlice;
use std::result::Result as StdResult;
use std::sync::{Mutex, MutexGuard};

use tunnel_lattice_core::{Error, PlatformErrorCode, Result};

use super::codec::{
    MAX_SEGMENTS, Run, STAGING_LEN, Segment, SplitCursor, VNET_HDR_LEN, VNET_HDR_NONE, plan_run,
};
use crate::backend::{DrainStep, Step, batch_capacity};

/// The framing [`decide_framing`] chose for one queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Framing {
    /// Every packet on the queue carries a 10-byte virtio-net header.
    pub offload: bool,
    /// The device has no header framing but carries an offload mask that
    /// the backend's device library set for this queue: the backend clears
    /// it with `TUNSETOFFLOAD(0)`.
    pub clear_mask: bool,
}

/// Chooses a queue's framing from what the device reports: whether
/// `IFF_VNET_HDR` is set, its header size (`None` when it was not read
/// because the flag is clear), and whether an offload mask was set for this
/// queue.
///
/// | Observed | Framing |
/// |---|---|
/// | `IFF_VNET_HDR` set, header size 10 | offload |
/// | `IFF_VNET_HDR` set, any other header size | an error |
/// | `IFF_VNET_HDR` clear | plain; `clear_mask` is `mask_set`, since a device without the header must not produce super-packets |
///
/// # Errors
///
/// [`Error::Unsupported`] when the flag is set and the header size is not
/// [`VNET_HDR_LEN`] (or was not read).
pub fn decide_framing(vnet_hdr: bool, hdr_size: Option<i32>, mask_set: bool) -> Result<Framing> {
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

/// What a backend decides about the failures of an offload-framed queue.
///
/// `E` is the error type of the backend's reads and writes (`std::io::Error`
/// for one backend, a raw `errno` for another). The engine only asks these
/// four questions; the answers are the backend's own error tables.
pub trait OffloadRules<E> {
    /// Classifies a failed read of a frame into the staging buffer, by a
    /// waiting `recv`: [`Step::Retry`] to read again (a signal, or a packet
    /// the kernel could not frame and has already freed), or [`Step::Done`]
    /// with the error to report. A read into the staging buffer can never be
    /// too small for a packet.
    fn read_error(&self, err: E) -> Step;

    /// Classifies a failed non-waiting read of a drain: [`DrainStep::Repeat`]
    /// reads again (the failure consumed a signal or a packet),
    /// [`DrainStep::End`] ends the batch with the packets received so far.
    fn drain_error(&self, err: &E) -> DrainStep;

    /// Classifies a failed write of a send batch: [`Step::Retry`] repeats
    /// the same write, [`Step::Done`] with the error to report ends the
    /// batch (see [`send_batch_blocking`] for how a prefix is reported).
    fn write_error(&self, err: E) -> Step;

    /// Whether a failed write of a coalesced run was refused whole: the
    /// kernel rejected the run's virtio-net header and wrote nothing, so
    /// the run's packets may be written again one by one. Asked only for a
    /// run's write, never for a single packet's.
    fn run_refused(&self, err: &E) -> bool;
}

/// The receive staging of one offload-framed queue: allocated only for
/// such a queue, owned by its handle, never shared with another queue.
///
/// It owns a staging buffer of [`STAGING_LEN`] bytes (header, the largest
/// super-packet, one sentinel byte) and a [`SplitCursor`], behind a mutex.
/// A `recv` first serves the next pending segment of the last frame read, if
/// any; otherwise it reads one frame into the staging buffer, validates it,
/// and serves its first segment. A segment that does not fit the caller's
/// buffer is dropped alone, as [`Error::BufferTooSmall`]. A frame that fails
/// validation (or filled the sentinel) is dropped and the read retried.
///
/// An async receive loop must never hold the lock across an `.await`, and
/// never await between a successful read and the return: a dropped `recv`
/// future then never loses a frame it read, because its remaining segments
/// stay in the staging buffer for the next `recv`.
pub struct OffloadRx(Mutex<Staging>);

/// The last frame read and how far it has been served.
///
/// Reached through [`OffloadRx::lock`]. Its methods are the building blocks
/// of a receive loop: serve a pending segment ([`Self::pending_batch`]),
/// hand the staging buffer to one native read ([`Self::read_buf`]), and
/// take that read's result ([`Self::read_batch`]).
pub struct Staging {
    /// Header, packet and sentinel byte of the last read.
    buf: Box<[u8]>,
    /// How many bytes of `buf` the last read filled.
    frame_len: usize,
    /// The segments of that frame still to serve; idle when none are.
    cursor: SplitCursor,
    /// How many frames read so far carried more than one segment, so a
    /// test can tell a real split from packets the kernel delivered one by
    /// one.
    split_frames: usize,
}

impl OffloadRx {
    /// A fresh staging buffer with nothing pending.
    #[must_use]
    pub fn new() -> Self {
        Self(Mutex::new(Staging {
            buf: vec![0u8; STAGING_LEN].into_boxed_slice(),
            frame_len: 0,
            cursor: SplitCursor::IDLE,
            split_frames: 0,
        }))
    }

    /// How many frames this queue has read that carried more than one
    /// segment (super-packets that were split). A diagnostic for tests that
    /// must tell a real split from packets delivered one by one. Takes the
    /// staging lock, so call it between receives, not during one.
    #[doc(hidden)]
    #[must_use]
    pub fn split_frames(&self) -> usize {
        self.lock().split_frames
    }

    /// Locks the staging. The split code cannot panic, so the mutex is
    /// never poisoned by it; if it ever is, the pending segments are
    /// discarded and the staging is used again.
    pub fn lock(&self) -> MutexGuard<'_, Staging> {
        self.0.lock().unwrap_or_else(|poisoned| {
            let mut staging = poisoned.into_inner();
            staging.cursor = SplitCursor::IDLE;
            self.0.clear_poison();
            staging
        })
    }

    /// Blocking `recv`: serves the pending segment, or reads frames with
    /// `read` (one blocking native read into the given buffer) until one
    /// yields a packet or an error, which `rules` classifies. Holds the lock
    /// across `read`, which is what a blocking caller does anyway. Never
    /// drains: it is a batch of one.
    ///
    /// # Errors
    ///
    /// [`Error::BufferTooSmall`] when the next segment does not fit `out`
    /// (that segment alone is dropped), and whatever `rules` reports for a
    /// failed read.
    pub fn recv_blocking<E>(
        &self,
        out: &mut [u8],
        mut read: impl FnMut(&mut [u8]) -> StdResult<usize, E>,
        rules: &impl OffloadRules<E>,
    ) -> Result<usize> {
        let mut staging = self.lock();
        loop {
            if let Some(result) = staging.next_pending(out) {
                return result;
            }
            let result = read(staging.read_buf());
            if let Some(result) = staging.finish_read(result, out, rules) {
                return result;
            }
        }
    }

    /// Blocking `recv_batch`: position 0 serves a pending segment, or reads
    /// frames with `read` (one blocking native read into the given buffer)
    /// until one yields a packet or an error; then [`Staging::drain`] fills
    /// the rest with `read_nowait`, which must never wait. Holds the lock
    /// across `read`. Returns the number of packets received; their lengths
    /// are in `lens`.
    ///
    /// # Errors
    ///
    /// What position 0 reports (see [`Self::recv_blocking`]); the drain
    /// itself never fails.
    pub fn recv_batch_blocking<E>(
        &self,
        bufs: &mut [&mut [u8]],
        lens: &mut [usize],
        mut read: impl FnMut(&mut [u8]) -> StdResult<usize, E>,
        mut read_nowait: impl FnMut(&mut [u8]) -> StdResult<usize, E>,
        rules: &impl OffloadRules<E>,
    ) -> Result<usize> {
        if batch_capacity(bufs, lens) == 0 {
            return Ok(0);
        }
        let mut staging = self.lock();
        loop {
            if let Some(result) = staging.pending_batch(bufs, lens, &mut read_nowait, rules) {
                return result;
            }
            let result = read(staging.read_buf());
            if let Some(result) = staging.read_batch(result, bufs, lens, &mut read_nowait, rules) {
                return result;
            }
        }
    }
}

impl Default for OffloadRx {
    fn default() -> Self {
        Self::new()
    }
}

impl Staging {
    /// Position 0 of a batch from a pending segment, then the drain:
    /// `Some(result)` for the call, or `None` when nothing is pending (or
    /// the batch has no capacity, which callers check first).
    pub fn pending_batch<E>(
        &mut self,
        bufs: &mut [&mut [u8]],
        lens: &mut [usize],
        read_nowait: impl FnMut(&mut [u8]) -> StdResult<usize, E>,
        rules: &impl OffloadRules<E>,
    ) -> Option<Result<usize>> {
        let first = self.next_pending(bufs.first_mut()?)?;
        Some(self.complete_batch(first, bufs, lens, read_nowait, rules))
    }

    /// Position 0 of a batch from one native read's `result` (see
    /// [`Self::finish_read`]), then the drain: `Some(result)` for the call,
    /// or `None` to read again.
    pub fn read_batch<E>(
        &mut self,
        result: StdResult<usize, E>,
        bufs: &mut [&mut [u8]],
        lens: &mut [usize],
        read_nowait: impl FnMut(&mut [u8]) -> StdResult<usize, E>,
        rules: &impl OffloadRules<E>,
    ) -> Option<Result<usize>> {
        let first = self.finish_read(result, bufs.first_mut()?, rules)?;
        Some(self.complete_batch(first, bufs, lens, read_nowait, rules))
    }

    /// Records position 0's packet, or returns its error with nothing
    /// received, then drains.
    fn complete_batch<E>(
        &mut self,
        first: Result<usize>,
        bufs: &mut [&mut [u8]],
        lens: &mut [usize],
        read_nowait: impl FnMut(&mut [u8]) -> StdResult<usize, E>,
        rules: &impl OffloadRules<E>,
    ) -> Result<usize> {
        let len = first?;
        let Some(slot) = lens.first_mut() else {
            return Ok(0);
        };
        *slot = len;
        Ok(self.drain(bufs, lens, 1, read_nowait, rules))
    }

    /// Positions `n..` of a batch, never waiting: serves pending segments
    /// into `bufs[n..]`, and when none is pending reads the next frame with
    /// `read_nowait` and splits on, until the capacity is reached or the
    /// batch ends. Returns the new count.
    ///
    /// A segment sized with [`SplitCursor::next_len`] before it is copied:
    /// one longer than its buffer stays pending and ends the batch, so the
    /// next call serves it (or, at position 0, drops it as
    /// [`Error::BufferTooSmall`]). A frame the split does not accept is
    /// dropped, and a failed read follows [`OffloadRules::drain_error`]. A
    /// zero-length read is a zero-length packet, as `recv` passes it through.
    pub fn drain<E>(
        &mut self,
        bufs: &mut [&mut [u8]],
        lens: &mut [usize],
        mut n: usize,
        mut read_nowait: impl FnMut(&mut [u8]) -> StdResult<usize, E>,
        rules: &impl OffloadRules<E>,
    ) -> usize {
        let capacity = batch_capacity(bufs, lens);
        while n < capacity {
            let (Some(out), Some(len)) = (bufs.get_mut(n), lens.get_mut(n)) else {
                break;
            };
            if !self.cursor.is_finished() {
                if self.cursor.next_len().is_some_and(|next| next > out.len()) {
                    break;
                }
                match self.next_pending(out) {
                    Some(Ok(got)) => {
                        *len = got;
                        n += 1;
                    }
                    // Unreachable after the size check: a too-small buffer
                    // is the only error, and it would have dropped the
                    // segment. End the batch rather than go on.
                    Some(Err(_)) => break,
                    // The cursor ended: read the next frame.
                    None => {}
                }
                continue;
            }
            match read_nowait(self.read_buf()) {
                Ok(0) => {
                    *len = 0;
                    n += 1;
                }
                Ok(got) => self.load(got),
                Err(err) => match rules.drain_error(&err) {
                    DrainStep::Repeat => {}
                    DrainStep::End => break,
                },
            }
        }
        n
    }

    /// Takes a frame of `len` bytes that a read put in the staging buffer:
    /// validates it and points the cursor at its first segment, or, when
    /// the split does not accept it, drops it (the cursor stays idle).
    fn load(&mut self, len: usize) {
        self.frame_len = len.min(self.buf.len());
        let frame = self.buf.get(..self.frame_len).unwrap_or_default();
        self.cursor = SplitCursor::new(frame).unwrap_or(SplitCursor::IDLE);
        if self.cursor.segments() > 1 {
            self.split_frames += 1;
        }
    }

    /// Writes the next pending segment into `out`: `Some(Ok(len))`, or
    /// `Some(Err(BufferTooSmall))` when `out` is too short (that segment is
    /// dropped, and the next call serves the one after it). `None` when no
    /// segment is pending.
    pub fn next_pending(&mut self, out: &mut [u8]) -> Option<Result<usize>> {
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
    pub fn read_buf(&mut self) -> &mut [u8] {
        &mut self.buf
    }

    /// Takes one native read's `result`: on success, validates the frame
    /// and serves its first segment into `out`; on failure, applies
    /// [`OffloadRules::read_error`]. `None` means "read again": the frame
    /// was dropped, or the error is retried.
    ///
    /// A zero-length read carries no frame and is passed through as
    /// `Ok(0)`, as on a plain queue, rather than retried.
    pub fn finish_read<E>(
        &mut self,
        result: StdResult<usize, E>,
        out: &mut [u8],
        rules: &impl OffloadRules<E>,
    ) -> Option<Result<usize>> {
        match result {
            Ok(0) => {
                self.cursor = SplitCursor::IDLE;
                Some(Ok(0))
            }
            Ok(len) => {
                self.load(len);
                self.next_pending(out)
            }
            Err(err) => match rules.read_error(err) {
                Step::Retry => None,
                Step::Done(result) => Some(result),
            },
        }
    }
}

/// The most I/O slices one batch write uses: a run's header, then one
/// payload per packet of the run.
const BATCH_IOV: usize = MAX_SEGMENTS + 1;

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

    /// The call's result for a failure: the error itself when nothing was
    /// sent, otherwise the sent prefix.
    fn fail(&self, err: Error) -> Result<usize> {
        if self.sent == 0 {
            Err(err)
        } else {
            Ok(self.sent)
        }
    }

    /// Takes the `result` of `write`, which was given `expected` bytes.
    /// `None` means "go on with the next write" (the same one again after
    /// a transient error); `Some` is the call's result.
    ///
    /// A write that reports fewer bytes than it was given is a failure like
    /// any other: the kernel's TUN write takes the whole frame or nothing,
    /// so this never happens on a real queue, but if it did, no packet of
    /// that write is counted as sent, and the error is
    /// `Platform(Unknown)`.
    fn finish<E>(
        &mut self,
        write: &BatchWrite,
        expected: usize,
        result: StdResult<usize, E>,
        rules: &impl OffloadRules<E>,
    ) -> Option<Result<usize>> {
        let failure = match result {
            Ok(written) if written == expected => {
                self.sent = self.sent.saturating_add(write.count());
                self.singles = self.singles.saturating_sub(write.count());
                return None;
            }
            Ok(_) => return Some(self.fail(Error::Platform(PlatformErrorCode::Unknown))),
            Err(err) => {
                if let BatchWrite::Run(run) = write
                    && rules.run_refused(&err)
                {
                    self.singles = run.count();
                    return None;
                }
                err
            }
        };
        match rules.write_error(failure) {
            Step::Retry => None,
            Step::Done(Err(err)) => Some(self.fail(err)),
            Step::Done(Ok(_)) => Some(Ok(self.sent)),
        }
    }
}

/// Blocking `send_batch` on an offload-framed queue: `writev` is one
/// blocking gather write to the queue.
///
/// At most [`MAX_SEGMENTS`] packets are sent per call, walking them in
/// order. At each position, [`plan_run`] looks for a run of adjacent packets
/// of one flow that the kernel will split back into exactly those packets
/// (UDP only when the queue has USO, `udp_gso`); a run goes out as one gather
/// write of the run header and each packet's payload, with no payload
/// copied, and anything else as one packet behind an all-zero header.
///
/// The result follows the `send_batch` prefix contract: `Ok(n)` means the
/// first `n` packets were written whole, in order, and nothing after them;
/// an error on the first write is returned as is; an error after `k > 0`
/// packets gives `Ok(k)`. A failed write is classified by
/// [`OffloadRules::write_error`]. A write the rules call a refused run
/// ([`OffloadRules::run_refused`]) wrote nothing, so the run's packets are
/// written again one by one.
///
/// # Errors
///
/// What [`OffloadRules::write_error`] reports for a failure of the first
/// write, and `Platform(Unknown)` for a first write that reported fewer
/// bytes than it was given.
pub fn send_batch_blocking<E>(
    packets: &[&[u8]],
    udp_gso: bool,
    mut writev: impl FnMut(&[IoSlice<'_>]) -> StdResult<usize, E>,
    rules: &impl OffloadRules<E>,
) -> Result<usize> {
    let mut batch = BatchSend::new(packets, udp_gso);
    while let Some(write) = batch.next_write() {
        let mut iov = [IoSlice::new(&[]); BATCH_IOV];
        let used = write.iovecs(batch.rest(), &mut iov);
        let expected = write.frame_len(batch.rest());
        let result = writev(iov.get(..used).unwrap_or_default());
        if let Some(result) = batch.finish(&write, expected, result, rules) {
            return result;
        }
    }
    Ok(batch.sent)
}

/// One async gather write to a queue, as [`send_batch_async`] uses it.
#[cfg(any(feature = "async", test))]
#[cfg_attr(docsrs, doc(cfg(feature = "async")))]
pub trait AsyncGatherWrite {
    /// The error of a failed write, classified by the caller's
    /// [`OffloadRules`].
    type Error;

    /// Writes `bufs` as one frame, waiting for the queue to be writable.
    /// Dropping the future before it completes writes nothing.
    fn write_gather(
        &self,
        bufs: &[IoSlice<'_>],
    ) -> impl std::future::Future<Output = StdResult<usize, Self::Error>> + Send;
}

/// Async `send_batch` on an offload-framed queue (see
/// [`send_batch_blocking`] for the batching and the result).
///
/// The only await is each write itself, and nothing is locked. A future
/// dropped before it completes has written an unknown prefix of
/// `packets`: each write wrote its packets whole or not at all, and no
/// packet was written twice.
///
/// # Errors
///
/// As [`send_batch_blocking`].
#[cfg(any(feature = "async", test))]
#[cfg_attr(docsrs, doc(cfg(feature = "async")))]
pub async fn send_batch_async<W, R>(
    packets: &[&[u8]],
    udp_gso: bool,
    queue: &W,
    rules: &R,
) -> Result<usize>
where
    W: AsyncGatherWrite + Sync + ?Sized,
    R: OffloadRules<W::Error> + Sync,
{
    let mut batch = BatchSend::new(packets, udp_gso);
    while let Some(write) = batch.next_write() {
        let mut iov = [IoSlice::new(&[]); BATCH_IOV];
        let used = write.iovecs(batch.rest(), &mut iov);
        let expected = write.frame_len(batch.rest());
        let result = queue
            .write_gather(iov.get(..used).unwrap_or_default())
            .await;
        if let Some(result) = batch.finish(&write, expected, result, rules) {
            return result;
        }
    }
    Ok(batch.sent)
}

/// Hand-built offload frames for the engine tests.
#[cfg(test)]
mod test_frames {
    use super::super::codec::{
        F_NEEDS_CSUM, GSO_UDP_L4, VNET_HDR_LEN, VirtioNetHdr, checksum, sum_words,
    };
    use super::super::codec::{Segment, SplitCursor};

    /// The payload of segment `k` of a [`udp4_frame`], `len` bytes long.
    pub(super) fn segment_payload(k: usize, len: usize) -> Vec<u8> {
        (0..len).map(|i| (k * 31 + i) as u8).collect()
    }

    /// A USO super-frame as the kernel hands it over: vnet header, IPv4
    /// (20 bytes) and UDP (8 bytes) headers, and `count` payloads of
    /// `gso_size` bytes (the last `last` bytes long). The split recomputes
    /// every length and checksum, so the input carries zeros there.
    pub(super) fn udp4_frame(gso_size: usize, count: usize, last: usize) -> Vec<u8> {
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
    pub(super) fn assert_segment(packet: &[u8], k: usize, len: usize) {
        assert_eq!(packet.len(), 28 + len, "segment {k}");
        assert_eq!(&packet[28..], segment_payload(k, len), "segment {k}");
        let id = u16::from_be_bytes([packet[4], packet[5]]);
        assert_eq!(id, 7u16.wrapping_add(k as u16), "segment {k}");
    }

    /// TCP ACK, the flag every [`tcp4_packet`] of a run carries.
    pub(super) const TCP_ACK: u8 = 0x10;
    /// TCP PSH, which ends a coalesced run.
    const TCP_PSH: u8 = 0x08;

    /// Fills in the IPv4 total length and header checksum and the L4
    /// checksum of an IPv4 packet with a 20-byte header.
    fn finish_ipv4(mut packet: Vec<u8>, csum_offset: usize) -> Vec<u8> {
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

    /// An IPv4/UDP packet 10.0.0.1:`sport` -> 10.0.0.2:4789 with IPv4 id
    /// `id`, carrying `payload`, with valid checksums.
    pub(super) fn udp4_packet(id: u16, sport: u16, payload: &[u8]) -> Vec<u8> {
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

    /// An IPv4/TCP packet 10.0.0.1:40000 -> 10.0.0.2:5201 (20-byte headers)
    /// with IPv4 id `id`, sequence number `seq` and `flags`, carrying
    /// `payload`, with valid checksums.
    pub(super) fn tcp4_packet(id: u16, seq: u32, flags: u8, payload: &[u8]) -> Vec<u8> {
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
    pub(super) fn tcp4_flow(count: usize, len: usize) -> Vec<Vec<u8>> {
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
    pub(super) fn split(frame: &[u8]) -> Vec<Vec<u8>> {
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
    use std::future::Future;
    use std::pin::pin;
    use std::sync::Arc;
    use std::task::{Context, Poll, Waker};

    use super::test_frames::{
        TCP_ACK, assert_segment, segment_payload, split, tcp4_flow, tcp4_packet, udp4_frame,
        udp4_packet,
    };
    use super::*;

    /// A read or write failure, as the tests' backend sees it: a raw code.
    type Code = i32;

    const EINTR: Code = 4;
    const EIO: Code = 5;
    const EAGAIN: Code = 11;
    const EINVAL: Code = 22;
    const EBADFD: Code = 77;

    /// A backend's rules, as the engine tests pin them: `EINVAL` on a read
    /// and `EINTR` anywhere retry, `EINVAL` on a run's write is a refused
    /// run, `EBADFD` is `Disconnected` and anything else `InvalidState`.
    struct Rules;

    impl OffloadRules<Code> for Rules {
        fn read_error(&self, err: Code) -> Step {
            match err {
                EINTR | EINVAL => Step::Retry,
                EBADFD => Step::Done(Err(Error::Disconnected)),
                _ => Step::Done(Err(Error::InvalidState)),
            }
        }

        fn drain_error(&self, err: &Code) -> DrainStep {
            if matches!(*err, EINTR | EINVAL) {
                DrainStep::Repeat
            } else {
                DrainStep::End
            }
        }

        fn write_error(&self, err: Code) -> Step {
            match err {
                EINTR => Step::Retry,
                EBADFD => Step::Done(Err(Error::Disconnected)),
                _ => Step::Done(Err(Error::InvalidState)),
            }
        }

        fn run_refused(&self, err: &Code) -> bool {
            *err == EINVAL
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
    struct Reads(VecDeque<StdResult<Vec<u8>, Code>>);

    impl Reads {
        fn new(script: impl IntoIterator<Item = StdResult<Vec<u8>, Code>>) -> Self {
            Self(script.into_iter().collect())
        }

        fn read(&mut self, buf: &mut [u8]) -> StdResult<usize, Code> {
            let frame = self.0.pop_front().expect("no more scripted reads")?;
            // The kernel truncates silently to the buffer.
            let n = frame.len().min(buf.len());
            buf[..n].copy_from_slice(&frame[..n]);
            Ok(n)
        }
    }

    fn recv(rx: &OffloadRx, reads: &mut Reads, out: &mut [u8]) -> Result<usize> {
        rx.recv_blocking(out, |buf| reads.read(buf), &Rules)
    }

    /// A plain (non-GSO) frame: the all-zero header, then a 20-byte packet.
    fn plain_frame() -> Vec<u8> {
        [
            VNET_HDR_NONE.as_slice(),
            &[
                0x45u8, 0, 0, 20, 0, 0, 0, 0, 64, 17, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8,
            ],
        ]
        .concat()
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

    /// The test-only split counter counts frames of more than one segment
    /// once each, when they are read: not a plain frame, not a GSO frame
    /// that carries a single segment, and not a frame dropped as malformed.
    #[test]
    fn the_split_counter_counts_multi_segment_frames_only() {
        let rx = OffloadRx::new();
        let mut malformed = udp4_frame(100, 4, 100);
        malformed[1] = 0x33;
        let mut reads = Reads::new([
            Ok(plain_frame()),
            Ok(udp4_frame(100, 1, 60)),
            Ok(malformed),
            Ok(udp4_frame(100, 3, 100)),
            Ok(udp4_frame(200, 2, 10)),
        ]);
        let mut out = vec![0u8; 400];
        assert_eq!(rx.split_frames(), 0);
        recv(&rx, &mut reads, &mut out).expect("the plain packet");
        assert_eq!(rx.split_frames(), 0, "a plain frame is not split");
        recv(&rx, &mut reads, &mut out).expect("the single segment");
        assert_eq!(rx.split_frames(), 0, "one segment is not a split");
        // The malformed frame is dropped; the next frame is split.
        recv(&rx, &mut reads, &mut out).expect("segment 0");
        assert_eq!(rx.split_frames(), 1, "counted when read");
        for _ in 1..3 {
            recv(&rx, &mut reads, &mut out).expect("a pending segment");
        }
        assert_eq!(rx.split_frames(), 1, "pending segments read nothing");
        recv(&rx, &mut reads, &mut out).expect("segment 0");
        assert_eq!(rx.split_frames(), 2);
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
    /// a retried error are dropped and the read retried; the next valid
    /// frame is served by the same `recv`.
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
            Err(EINVAL),
            Err(EINTR),
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
        let frame = plain_frame();
        let mut reads = Reads::new([Ok(frame.clone())]);
        let mut out = vec![0u8; 64];
        let len = recv(&rx, &mut reads, &mut out).unwrap();
        assert_eq!(&out[..len], &frame[VNET_HDR_LEN..]);
    }

    /// The rules decide what a failed read means; a zero-length read is
    /// passed through rather than retried.
    #[test]
    fn errors_and_empty_reads_are_reported() {
        let rx = OffloadRx::new();
        let mut reads = Reads::new([Err(EBADFD), Err(EIO), Ok(Vec::new())]);
        let mut out = vec![0u8; 64];
        assert!(matches!(
            recv(&rx, &mut reads, &mut out),
            Err(Error::Disconnected)
        ));
        assert!(matches!(
            recv(&rx, &mut reads, &mut out),
            Err(Error::InvalidState)
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
        let rx = Arc::new(OffloadRx::new());
        let mut reads = Reads::new([Ok(udp4_frame(100, 3, 100))]);
        let mut out = vec![0u8; 200];
        recv(&rx, &mut reads, &mut out).unwrap();
        let poisoner = Arc::clone(&rx);
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
            &mut bufs.slices(),
            lens,
            |buf| reads.read(buf),
            |buf| nowait.read(buf),
            &Rules,
        )
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
        let mut nowait = Reads::new([Err(EAGAIN)]);
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
    /// first read the rules end the batch on.
    #[test]
    fn the_drain_spans_frames_until_the_queue_is_empty() {
        for end in [EAGAIN, 95] {
            let rx = OffloadRx::new();
            let mut reads = Reads::new([Ok(udp4_frame(100, 2, 100))]);
            let mut nowait = Reads::new([Ok(udp4_frame(200, 2, 50)), Ok(plain_frame()), Err(end)]);
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
            assert!(nowait.0.is_empty(), "the drain stopped at the end read");
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
        let mut nowait = Reads::new([Err(EAGAIN)]);
        assert_eq!(
            batch(&rx, &mut reads, &mut nowait, &mut bufs, &mut lens).unwrap(),
            2
        );
        assert_segment(&bufs.0[0][..lens[0]], 2, 100);
        assert_segment(&bufs.0[1][..lens[1]], 3, 100);
    }

    /// A frame the split does not accept and the reads the rules repeat
    /// are dropped by the drain, which goes on reading; a zero-length read
    /// is a zero-length packet.
    #[test]
    fn the_drain_drops_bad_frames_and_goes_on() {
        let rx = OffloadRx::new();
        let mut bad_type = udp4_frame(100, 2, 10);
        bad_type[1] = 0x33;
        let mut reads = Reads::new([Ok(udp4_frame(100, 1, 100))]);
        let mut nowait = Reads::new([
            Ok(bad_type),
            Err(EINVAL),
            Err(EINTR),
            Ok(udp4_frame(100, 1, 60)),
            Ok(Vec::new()),
            Err(EAGAIN),
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
    /// reports the failure.
    #[test]
    fn another_drain_error_returns_the_prefix_first() {
        let rx = OffloadRx::new();
        let mut reads = Reads::new([Ok(udp4_frame(100, 1, 100)), Err(EBADFD)]);
        let mut nowait = Reads::new([Err(EBADFD)]);
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
        let mut reads = Reads::new([Ok(vec![0u8; 4]), Err(EINVAL), Ok(udp4_frame(100, 2, 100))]);
        let mut nowait = Reads::new([Err(EAGAIN)]);
        let mut bufs = Bufs::new(4, 200);
        let mut lens = [0; 4];
        assert_eq!(
            batch(&rx, &mut reads, &mut nowait, &mut bufs, &mut lens).unwrap(),
            2
        );
        assert!(reads.0.is_empty() && nowait.0.is_empty());
    }

    /// What the scripted queue does with one write.
    #[derive(Clone, Copy, Debug)]
    enum Reply {
        /// Takes the whole frame.
        Full,
        /// Reports one byte fewer than it was given, and records nothing.
        Short,
        /// Fails with this code, writing nothing.
        Fail(Code),
        /// Never completes (async only), writing nothing.
        Pending,
    }

    /// A queue that answers each write with the next scripted reply (then
    /// `Full`), and records every frame it took whole.
    struct Queue {
        replies: std::sync::Mutex<VecDeque<Reply>>,
        frames: std::sync::Mutex<Vec<Vec<u8>>>,
        writes: std::sync::Mutex<usize>,
    }

    impl Queue {
        fn new(replies: impl IntoIterator<Item = Reply>) -> Self {
            Self {
                replies: std::sync::Mutex::new(replies.into_iter().collect()),
                frames: std::sync::Mutex::new(Vec::new()),
                writes: std::sync::Mutex::new(0),
            }
        }

        fn frames(&self) -> Vec<Vec<u8>> {
            self.frames.lock().unwrap().clone()
        }

        fn writes(&self) -> usize {
            *self.writes.lock().unwrap()
        }

        /// One write; `None` while the scripted reply is `Pending`.
        fn write(&self, bufs: &[IoSlice<'_>]) -> Option<StdResult<usize, Code>> {
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
                Reply::Fail(code) => Err(code),
                Reply::Pending => unreachable!(),
            })
        }
    }

    impl AsyncGatherWrite for Queue {
        type Error = Code;

        /// Decides only when polled, so a future dropped while `Pending`
        /// wrote nothing.
        fn write_gather(
            &self,
            bufs: &[IoSlice<'_>],
        ) -> impl Future<Output = StdResult<usize, Code>> + Send {
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
        let sync = send_batch_blocking(
            &refs,
            udp_gso,
            |iov| {
                sync_queue
                    .write(iov)
                    .expect("no pending reply in a blocking test")
            },
            &Rules,
        );
        let async_queue = Queue::new(replies.iter().copied());
        let Poll::Ready(asynchronous) =
            poll_once(send_batch_async(&refs, udp_gso, &async_queue, &Rules))
        else {
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

    /// A run the kernel refuses wrote nothing: its packets are written
    /// again one by one, and the batch goes on after them.
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

    /// A failure on the first write is the call's error, classified by the
    /// rules; after `k` packets were written it is `Ok(k)`, and the next
    /// call, starting at the failed packet, reports the error.
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

    /// A refusal is only asked of a run's write: the same failure on a
    /// single packet is classified by `write_error`, and nothing is retried.
    #[test]
    fn a_refusal_code_on_a_single_packet_is_an_error() {
        let packets = mixed_flows(2);
        let (result, frames) = send(&[Reply::Fail(EINVAL)], &packets, true);
        assert!(matches!(result, Err(Error::InvalidState)), "{result:?}");
        assert!(frames.is_empty());
        let queue = Queue::new([Reply::Fail(EINVAL)]);
        let refs: Vec<&[u8]> = packets.iter().map(Vec::as_slice).collect();
        let _ = send_batch_blocking(&refs, true, |iov| queue.write(iov).unwrap(), &Rules);
        assert_eq!(queue.writes(), 1);
    }

    /// A transient failure repeats the same write, for a run and for a
    /// single packet.
    #[test]
    fn a_retried_failure_repeats_the_write() {
        let mut packets = tcp4_flow(3, 100);
        packets.extend(mixed_flows(1));
        let replies = [Reply::Fail(EINTR), Reply::Full, Reply::Fail(EINTR)];
        let (result, frames) = send(&replies, &packets, true);
        assert_eq!(result.unwrap(), 4);
        assert_eq!(frames.len(), 2);
        assert_eq!(unframe(&frames), packets);
    }

    /// A write that reports fewer bytes than it was given fails like any
    /// other write, as `Platform(Unknown)`: none of its packets counts as
    /// sent.
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
        let pending = poll_once(send_batch_async(&refs, true, &queue, &Rules));
        assert!(pending.is_pending());
        assert_eq!(queue.frames(), singles(&packets[..1]));

        // The caller resumes after the sent prefix; nothing is written twice.
        queue.replies.lock().unwrap().clear();
        let Poll::Ready(result) = poll_once(send_batch_async(&refs[1..], true, &queue, &Rules))
        else {
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
        let result = send_batch_blocking(
            &refs,
            true,
            |iov| -> StdResult<usize, Code> {
                seen = iov
                    .iter()
                    .map(|slice| (slice.as_ptr(), slice.len()))
                    .collect();
                Ok(iov.iter().map(|slice| slice.len()).sum())
            },
            &Rules,
        );
        assert_eq!(result.unwrap(), 3);
        assert_eq!(seen.len(), 4);
        assert_eq!(seen[0].1, VNET_HDR_LEN + 40);
        for (k, packet) in packets.iter().enumerate() {
            assert_eq!(seen[k + 1], (packet[40..].as_ptr(), 100), "payload {k}");
        }
        // A packet that cannot coalesce is the zero header and the packet.
        let lone = tcp4_packet(1, 1, TCP_ACK, b"x");
        let result = send_batch_blocking(
            &[&lone],
            true,
            |iov| -> StdResult<usize, Code> {
                seen = iov
                    .iter()
                    .map(|slice| (slice.as_ptr(), slice.len()))
                    .collect();
                Ok(iov.iter().map(|slice| slice.len()).sum())
            },
            &Rules,
        );
        assert_eq!(result.unwrap(), 1);
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].1, VNET_HDR_LEN);
        assert_eq!(seen[1], (lone.as_ptr(), 41), "the packet itself");
    }
}
