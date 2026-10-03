//! Support API for Tunnel Lattice backend crates. Not covered by any
//! stability promise; may change in any minor release.
//!
//! This module holds the pieces of a TUN/TAP packet path that do not depend
//! on any one backend: they are pure, safe Rust with no I/O and no operating
//! system calls, so every backend (the `tun-rs`-backed one, the native Linux
//! one, and the Windows and macOS ones to come) shares one implementation and
//! one set of tests instead of keeping its own copy. Applications never need
//! it: the `tunnel-lattice` facade does not enable the `backend` feature and
//! does not re-export this module.
//!
//! - [`Step`], [`recv_step`], [`send_step`] and [`tap_abort_retries`] are the
//!   decision a blocking or async `recv`/`send` loop takes after one native
//!   attempt: retry, or finish with a result. What counts as transient or
//!   fatal is the caller's rule, passed in as a closure, so these functions
//!   name no backend and no error type.
//! - [`DrainStep`], [`batch_capacity`], [`DeferredTooSmall`], [`drain_plain`]
//!   and [`recv_batch_blocking`] are the `recv_batch` drain: after the first
//!   packet, further packets are read without waiting, and a failed read
//!   either repeats or ends the batch. They are generic over the error type
//!   of the read (`std::io::Error` for one backend, a raw `errno` for
//!   another) with the classifier passed as a closure.
//! - [`errno`] holds the raw error codes the backends compare against, as
//!   plain integers (so this crate needs no `libc`), and the Linux table that
//!   maps an `errno` and the operation that produced it to an outcome.
//! - `offload`, behind the additional `offload` feature, is the Linux
//!   segmentation-offload codec (the virtio-net header, the receive split,
//!   the send coalescing) and the queue-level engine around it. Its reads,
//!   writes and error tables are supplied by the backend.
//!
//! # Draining a batch
//!
//! A `recv_batch` receives its first packet (position 0) exactly as `recv`
//! does. Once it holds that packet it never waits again: each further
//! position `k >= 1` is one non-waiting read, and its failure is classified
//! by the caller's [`DrainStep`] rule instead of the `recv` rules. A plain
//! packet longer than the buffer of position `k >= 1` is already consumed
//! when the read reports it, so the batch ends with the packets received so
//! far and the pending [`Error::BufferTooSmall`] is stored in the queue's
//! [`DeferredTooSmall`] and returned by the next `recv` or `recv_batch` on
//! that queue.

use std::result::Result as StdResult;
use std::sync::atomic::{AtomicBool, Ordering};

use tunnel_lattice_core::{Error, Result};

pub mod errno;
#[cfg(feature = "offload")]
#[cfg_attr(docsrs, doc(cfg(feature = "offload")))]
pub mod offload;

/// What a `recv`/`send` loop does with one native attempt's result.
#[derive(Debug)]
pub enum Step {
    /// A transient condition: make the native call again.
    Retry,
    /// The call is finished with this result.
    Done(Result<usize>),
}

/// Maps one native `recv` attempt that read into a `buf_len`-byte buffer.
///
/// A length past `buf_len` is a packet that did not fit (the caller read
/// into the buffer plus a one-byte sentinel, which the packet spilled into):
/// [`Error::BufferTooSmall`]. Any other success is the packet length.
/// `on_error` classifies a native failure, as [`Step::Retry`] for a
/// transient one or [`Step::Done`] with the error to report.
pub fn recv_step<E>(
    buf_len: usize,
    result: StdResult<usize, E>,
    on_error: impl FnOnce(E) -> Step,
) -> Step {
    match result {
        Ok(n) if n > buf_len => Step::Done(Err(Error::BufferTooSmall)),
        Ok(n) => Step::Done(Ok(n)),
        Err(err) => on_error(err),
    }
}

/// Maps one native `send` attempt: a success is the number of bytes sent,
/// and `on_error` classifies a native failure as [`Step::Retry`] for a
/// transient one or [`Step::Done`] with the error to report.
pub fn send_step<E>(result: StdResult<usize, E>, on_error: impl FnOnce(E) -> Step) -> Step {
    match result {
        Ok(n) => Step::Done(Ok(n)),
        Err(err) => on_error(err),
    }
}

/// Whether a `recv` retries a cancelled read once instead of reporting it
/// as a failure: only when the call has not retried yet (`retried` is still
/// `false`), and only when `adapter_up`, which reads the adapter's
/// operational status, says it is up. Once the call has retried,
/// `adapter_up` is not called, so a call retries at most once and a vanished
/// or disabled adapter never leads to a retry.
///
/// A healthy Windows TAP adapter reports a cancelled read when the thread
/// that issued it exited; the retry issues a fresh read, which waits as
/// usual.
pub fn tap_abort_retries(retried: bool, adapter_up: impl FnOnce() -> bool) -> bool {
    !retried && adapter_up()
}

/// What a `recv_batch` drain does after one failed non-waiting read at a
/// position `k >= 1`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DrainStep {
    /// Read again: the failure consumed nothing the caller should see.
    Repeat,
    /// End the batch with the packets received so far.
    End,
}

/// How many packets one `recv_batch` call may receive: the shorter of its
/// two slices.
pub fn batch_capacity(bufs: &[&mut [u8]], lens: &[usize]) -> usize {
    bufs.len().min(lens.len())
}

/// The deferred [`Error::BufferTooSmall`] of one queue: set by a
/// `recv_batch` drain that consumed a plain packet too long for its
/// buffer after it had already received others, and returned once, first,
/// by the next `recv` or `recv_batch` on the same queue, before any native
/// call.
///
/// The flag is per queue (one per device handle, never shared with an
/// additional queue), so every handle clone that reads this queue sees it.
/// The fast path is one relaxed load; the swap happens only when it is set.
/// It is set immediately before the batch returns, with no `.await` after
/// it, so a dropped future can neither lose nor duplicate it.
#[derive(Debug, Default)]
pub struct DeferredTooSmall(AtomicBool);

impl DeferredTooSmall {
    /// No error pending.
    #[must_use]
    pub const fn new() -> Self {
        Self(AtomicBool::new(false))
    }

    /// Records one pending [`Error::BufferTooSmall`]. Several drains that
    /// each set it before any receive collapse into one, which cannot
    /// happen in practice: a drain that sets it returns, and the next call
    /// clears it before reading.
    pub fn set(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    /// Returns the pending [`Error::BufferTooSmall`] once and clears it, or
    /// `Ok(())` when none is pending.
    ///
    /// # Errors
    ///
    /// [`Error::BufferTooSmall`], once, after [`DeferredTooSmall::set`].
    pub fn take(&self) -> Result<()> {
        if self.0.load(Ordering::Relaxed) && self.0.swap(false, Ordering::Relaxed) {
            Err(Error::BufferTooSmall)
        } else {
            Ok(())
        }
    }
}

/// Positions `n..` of a `recv_batch` on a plainly framed queue, after the
/// first `n` (normally one) were received and recorded in `lens`: reads
/// with `read`, which must never wait, into `bufs[k]` until the capacity is
/// reached or a read ends the batch. Returns the new count.
///
/// `read` is a sentinel read (`[bufs[k], 1-byte sentinel]`), so a result
/// past `bufs[k].len()` is a packet that did not fit: it is already
/// consumed, so the batch ends there and `deferred` is set (see
/// [`DeferredTooSmall`]). A failed read is classified by `classify`: a
/// [`DrainStep::Repeat`] reads again, a [`DrainStep::End`] ends the batch.
/// A zero-length read is a zero-length packet, as `recv` passes it through.
pub fn drain_plain<E>(
    bufs: &mut [&mut [u8]],
    lens: &mut [usize],
    mut n: usize,
    deferred: &DeferredTooSmall,
    mut read: impl FnMut(&mut [u8]) -> StdResult<usize, E>,
    mut classify: impl FnMut(&E) -> DrainStep,
) -> usize {
    let capacity = batch_capacity(bufs, lens);
    while n < capacity {
        let (Some(buf), Some(len)) = (bufs.get_mut(n), lens.get_mut(n)) else {
            break;
        };
        match read(buf) {
            Ok(got) if got > buf.len() => {
                deferred.set();
                break;
            }
            Ok(got) => {
                *len = got;
                n += 1;
            }
            Err(err) => match classify(&err) {
                DrainStep::Repeat => {}
                DrainStep::End => break,
            },
        }
    }
    n
}

/// Blocking `recv_batch` on a plainly framed queue: position 0 is `first`
/// (the caller's complete blocking `recv`, with all its retry and error
/// rules), and the rest is [`drain_plain`] with `read_nowait` and
/// `classify`. The caller has checked the capacity and taken any deferred
/// error. Returns the number of packets received; their lengths are in
/// `lens`.
///
/// # Errors
///
/// What `first` returns, when it fails; the drain itself never fails.
pub fn recv_batch_blocking<E>(
    bufs: &mut [&mut [u8]],
    lens: &mut [usize],
    deferred: &DeferredTooSmall,
    first: impl FnOnce(&mut [u8]) -> Result<usize>,
    read_nowait: impl FnMut(&mut [u8]) -> StdResult<usize, E>,
    classify: impl FnMut(&E) -> DrainStep,
) -> Result<usize> {
    let (Some(buf), Some(len)) = (bufs.first_mut(), lens.first_mut()) else {
        return Ok(0);
    };
    *len = first(buf)?;
    Ok(drain_plain(bufs, lens, 1, deferred, read_nowait, classify))
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;

    use super::*;

    /// Four 8-byte buffers for the drain tests.
    struct Batch([[u8; 8]; 4]);

    impl Batch {
        fn new() -> Self {
            Self([[0; 8]; 4])
        }

        fn bufs(&mut self) -> [&mut [u8]; 4] {
            let [a, b, c, d] = &mut self.0;
            [a, b, c, d]
        }
    }

    /// Scripted reads of a drain: `Ok(n)` copies `n` bytes of the payload in
    /// (capped by the buffer) and reports `n`; `Err(code)` fails with that
    /// code. Counts its calls.
    struct Script {
        results: RefCell<VecDeque<StdResult<usize, i32>>>,
        calls: Cell<usize>,
    }

    impl Script {
        fn new(results: Vec<StdResult<usize, i32>>) -> Self {
            Self {
                results: RefCell::new(results.into()),
                calls: Cell::new(0),
            }
        }

        fn read(&self, buf: &mut [u8]) -> StdResult<usize, i32> {
            self.calls.set(self.calls.get() + 1);
            let result = self
                .results
                .borrow_mut()
                .pop_front()
                .expect("read past the script");
            if let Ok(n) = result {
                let take = n.min(buf.len());
                buf[..take].fill(0xAB);
            }
            result
        }

        fn calls(&self) -> usize {
            self.calls.get()
        }
    }

    const REPEAT: i32 = 4;
    const STOP: i32 = 11;

    fn classify(code: &i32) -> DrainStep {
        if *code == REPEAT {
            DrainStep::Repeat
        } else {
            DrainStep::End
        }
    }

    #[test]
    fn recv_step_reports_a_length_past_the_buffer_as_too_small() {
        let step = recv_step::<i32>(4, Ok(5), |_| panic!("not an error"));
        assert!(matches!(step, Step::Done(Err(Error::BufferTooSmall))));
        let step = recv_step::<i32>(4, Ok(4), |_| panic!("not an error"));
        assert!(matches!(step, Step::Done(Ok(4))));
        let step = recv_step::<i32>(0, Ok(0), |_| panic!("not an error"));
        assert!(matches!(step, Step::Done(Ok(0))));
        let step = recv_step::<i32>(0, Ok(1), |_| panic!("not an error"));
        assert!(matches!(step, Step::Done(Err(Error::BufferTooSmall))));
    }

    #[test]
    fn recv_and_send_steps_hand_a_failure_to_the_classifier() {
        let step = recv_step(8, Err(7), |code| {
            assert_eq!(code, 7);
            Step::Retry
        });
        assert!(matches!(step, Step::Retry));
        let step = recv_step(8, Err(7), |_| Step::Done(Err(Error::Disconnected)));
        assert!(matches!(step, Step::Done(Err(Error::Disconnected))));

        let step = send_step::<i32>(Ok(3), |_| panic!("not an error"));
        assert!(matches!(step, Step::Done(Ok(3))));
        let step = send_step(Err(9), |code| {
            assert_eq!(code, 9);
            Step::Done(Err(Error::InvalidState))
        });
        assert!(matches!(step, Step::Done(Err(Error::InvalidState))));
        assert!(matches!(send_step(Err(9), |_| Step::Retry), Step::Retry));
    }

    #[test]
    fn a_send_length_is_never_judged_against_a_buffer() {
        // Unlike a receive, a send's success is passed through whatever it is.
        let step = send_step::<i32>(Ok(usize::MAX), |_| panic!("not an error"));
        assert!(matches!(step, Step::Done(Ok(usize::MAX))));
    }

    #[test]
    fn tap_abort_retries_only_the_first_cancellation_while_up() {
        let asked = Cell::new(0);
        let status = |up: bool| {
            let asked = &asked;
            move || {
                asked.set(asked.get() + 1);
                up
            }
        };
        assert!(tap_abort_retries(false, status(true)));
        assert_eq!(asked.get(), 1);
        assert!(!tap_abort_retries(false, status(false)));
        assert_eq!(asked.get(), 2);
        // After the call retried, the status is not read at all.
        assert!(!tap_abort_retries(true, status(true)));
        assert_eq!(asked.get(), 2);
    }

    #[test]
    fn batch_capacity_is_the_shorter_slice() {
        let (mut a, mut b) = ([0u8; 4], [0u8; 4]);
        let bufs: [&mut [u8]; 2] = [&mut a, &mut b];
        assert_eq!(batch_capacity(&bufs, &[0; 1]), 1);
        assert_eq!(batch_capacity(&bufs, &[0; 3]), 2);
        assert_eq!(batch_capacity(&[], &[0; 3]), 0);
    }

    #[test]
    fn deferred_too_small_is_returned_once() {
        let deferred = DeferredTooSmall::new();
        assert!(deferred.take().is_ok());
        deferred.set();
        deferred.set();
        assert!(matches!(deferred.take(), Err(Error::BufferTooSmall)));
        assert!(deferred.take().is_ok());
    }

    #[test]
    fn drain_fills_the_capacity_without_reading_past_it() {
        let script = Script::new(vec![Ok(3), Ok(0), Ok(4), Ok(5), Ok(6)]);
        let mut batch = Batch::new();
        let mut lens = [5, 0, 0, 0];
        let deferred = DeferredTooSmall::new();
        let n = drain_plain(
            &mut batch.bufs(),
            &mut lens,
            1,
            &deferred,
            |buf| script.read(buf),
            classify,
        );
        assert_eq!(n, 4);
        assert_eq!(lens, [5, 3, 0, 4]);
        assert_eq!(script.calls(), 3, "positions 1..4 only");
        assert!(deferred.take().is_ok());
    }

    #[test]
    fn drain_stops_at_the_shorter_slice() {
        let script = Script::new(vec![Ok(1), Ok(2), Ok(3)]);
        let mut batch = Batch::new();
        let mut lens = [9, 0];
        let n = drain_plain(
            &mut batch.bufs(),
            &mut lens,
            1,
            &DeferredTooSmall::new(),
            |buf| script.read(buf),
            classify,
        );
        assert_eq!(n, 2);
        assert_eq!(script.calls(), 1);

        // No room at all: the read is never made.
        let mut lens = [9; 4];
        let n = drain_plain(
            &mut batch.bufs(),
            &mut lens,
            4,
            &DeferredTooSmall::new(),
            |buf| script.read(buf),
            classify,
        );
        assert_eq!(n, 4);
        assert_eq!(script.calls(), 1);
    }

    #[test]
    fn drain_repeats_what_the_classifier_repeats_and_ends_on_the_rest() {
        let script = Script::new(vec![Err(REPEAT), Err(REPEAT), Ok(2), Err(STOP), Ok(7)]);
        let mut batch = Batch::new();
        let mut lens = [1, 0, 0, 0];
        let n = drain_plain(
            &mut batch.bufs(),
            &mut lens,
            1,
            &DeferredTooSmall::new(),
            |buf| script.read(buf),
            classify,
        );
        assert_eq!(n, 2, "the batch ends at the first non-repeat failure");
        assert_eq!(lens[..2], [1, 2]);
        assert_eq!(script.calls(), 4);
    }

    #[test]
    fn drain_passes_the_failure_itself_to_the_classifier() {
        let seen = RefCell::new(Vec::new());
        let script = Script::new(vec![Err(5), Err(6), Err(STOP)]);
        let mut batch = Batch::new();
        let mut lens = [1, 0, 0, 0];
        let n = drain_plain(
            &mut batch.bufs(),
            &mut lens,
            1,
            &DeferredTooSmall::new(),
            |buf| script.read(buf),
            |code: &i32| {
                seen.borrow_mut().push(*code);
                if *code < 10 {
                    DrainStep::Repeat
                } else {
                    DrainStep::End
                }
            },
        );
        assert_eq!(n, 1);
        assert_eq!(*seen.borrow(), [5, 6, STOP]);
    }

    #[test]
    fn drain_defers_a_packet_that_filled_the_sentinel() {
        let script = Script::new(vec![Ok(2), Ok(9), Ok(1)]);
        let mut batch = Batch::new();
        let mut lens = [4, 0, 0, 0];
        let deferred = DeferredTooSmall::new();
        let n = drain_plain(
            &mut batch.bufs(),
            &mut lens,
            1,
            &deferred,
            |buf| script.read(buf),
            classify,
        );
        assert_eq!(n, 2, "the batch ends at the packet that did not fit");
        assert_eq!(script.calls(), 2);
        assert!(matches!(deferred.take(), Err(Error::BufferTooSmall)));
        assert!(deferred.take().is_ok());
    }

    #[test]
    fn drain_works_over_a_non_io_error_type() {
        // The error type is the caller's: a unit-like marker is enough.
        #[derive(Debug)]
        struct Empty;
        let mut reads = vec![Ok(1), Err(Empty)].into_iter();
        let mut batch = Batch::new();
        let mut lens = [0; 4];
        let n = drain_plain(
            &mut batch.bufs(),
            &mut lens,
            0,
            &DeferredTooSmall::new(),
            |_| reads.next().expect("read past the script"),
            |_: &Empty| DrainStep::End,
        );
        assert_eq!(n, 1);
    }

    #[test]
    fn recv_batch_blocking_waits_only_for_position_zero() {
        let rest = Script::new(vec![Ok(2), Err(STOP)]);
        let mut batch = Batch::new();
        let mut lens = [0; 4];
        let n = recv_batch_blocking(
            &mut batch.bufs(),
            &mut lens,
            &DeferredTooSmall::new(),
            |buf| {
                buf[..3].copy_from_slice(b"abc");
                Ok(3)
            },
            |buf| rest.read(buf),
            classify,
        )
        .unwrap();
        assert_eq!(n, 2);
        assert_eq!(lens[..2], [3, 2]);
        assert_eq!(&batch.0[0][..3], b"abc");
        assert_eq!(rest.calls(), 2);
    }

    #[test]
    fn recv_batch_blocking_reports_a_position_zero_failure_without_draining() {
        let mut batch = Batch::new();
        let mut lens = [0; 4];
        let result = recv_batch_blocking(
            &mut batch.bufs(),
            &mut lens,
            &DeferredTooSmall::new(),
            |_| Err(Error::Disconnected),
            |_| -> StdResult<usize, i32> { panic!("no drain after an error") },
            classify,
        );
        assert!(matches!(result, Err(Error::Disconnected)));
    }

    #[test]
    fn recv_batch_blocking_with_no_capacity_reads_nothing() {
        let mut lens = [0; 4];
        let result = recv_batch_blocking(
            &mut [],
            &mut lens,
            &DeferredTooSmall::new(),
            |_| panic!("no position 0"),
            |_| -> StdResult<usize, i32> { panic!("no drain") },
            classify,
        );
        assert!(matches!(result, Ok(0)));
    }
}
