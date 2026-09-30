//! The macOS TAP (`feth`) receive path in async builds: a bounded wait for
//! the BPF read descriptor that also notices the interface going away.
//!
//! A macOS TAP device from `tun-rs` reads through a BPF descriptor bound to
//! the peer of a `feth` pair. In async builds, `tun-rs` 2.8.11 waits for
//! that descriptor with `poll(..., -1)` on a blocking-pool thread. When the
//! peer is destroyed, the kernel unbinds the descriptor and wakes its
//! waiters, but the BPF readiness filter only reports buffered data: it
//! never reports the unbinding, so a `poll` that is already sleeping keeps
//! sleeping forever. A plain read of the unbound descriptor, by contrast,
//! fails with `ENXIO` at once, even when the descriptor is non-blocking.
//! The only waiters that ever notice are therefore a read, or a new `poll`
//! registration, which the kernel refuses for an unbound descriptor.
//!
//! This module replaces only the wait. `tun-rs` still does every read (its
//! `try_recv_vectored`), so its frame queue, its frame unpacking and its
//! error signals are unchanged:
//!
//! - [`recv_with_bounded_wait`] tries the read first, and waits only when
//!   the read would block;
//! - [`wait_readable_or_detached`] polls the descriptor with a finite
//!   timeout ([`DETACH_CHECK_INTERVAL`]) and, at each timeout, asks the
//!   descriptor which interface it is bound to (`BIOCGETIF`), which fails
//!   once the interface is gone. The read that follows then returns the
//!   `ENXIO` that `recv_contract` maps to `Disconnected`.
//!
//! The host-generic core also compiles into the unit tests on every unix
//! host, so it is exercised on Linux as well; the BPF-specific items are
//! macOS-only.

use std::future::Future;
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::time::Duration;

use libc::{c_int, c_short};

/// How long one `poll` of the BPF descriptor sleeps before the wait checks
/// the descriptor's binding. Bounds how long after the interface goes away
/// a pending `recv` returns.
pub(crate) const DETACH_CHECK_INTERVAL: Duration = Duration::from_millis(250);

/// Message of the error returned when the BPF descriptor cannot be polled
/// and the read that follows still would block (unreachable per the XNU
/// source: a descriptor the kernel refuses to poll is unbound, and an
/// unbound descriptor's read fails with `ENXIO`).
pub(crate) const UNPOLLABLE_MESSAGE: &str = "BPF descriptor cannot be polled";

/// Why a wait for the BPF descriptor ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Wake {
    /// The descriptor has data to read.
    Readable,
    /// `poll` reported an error condition on the descriptor (for example,
    /// the kernel refused to register it because it is no longer bound).
    Invalid,
    /// The descriptor is no longer bound to an interface.
    Detached,
    /// The waiting future was dropped (its end of the cancel pipe closed).
    Cancelled,
}

/// Whether a BPF descriptor is still bound to an interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Binding {
    /// Bound to an interface.
    Attached,
    /// Unbound (its interface is gone) or being closed.
    Detached,
}

/// Classifies the result of `BIOCGETIF` (`Err` holds the raw `errno`):
/// success is [`Binding::Attached`]; `EINVAL` (the descriptor is unbound)
/// and `ENXIO` (the descriptor is being closed) are [`Binding::Detached`];
/// any other code is returned as an error.
pub(crate) fn classify_binding(result: Result<(), i32>) -> io::Result<Binding> {
    match result {
        Ok(()) => Ok(Binding::Attached),
        Err(libc::EINVAL | libc::ENXIO) => Ok(Binding::Detached),
        Err(code) => Err(io::Error::from_raw_os_error(code)),
    }
}

/// Classifies the `revents` of one `poll` over `[bpf, cancel]`: any event
/// on the cancel pipe (its writer closing reports end-of-file) is
/// [`Wake::Cancelled`]; `POLLIN` on the BPF descriptor is
/// [`Wake::Readable`]; any other BPF event (`POLLNVAL`, `POLLERR`,
/// `POLLHUP`, ...) is [`Wake::Invalid`]; no event at all is `None`.
pub(crate) fn classify_revents(bpf: c_short, cancel: c_short) -> Option<Wake> {
    if cancel != 0 {
        Some(Wake::Cancelled)
    } else if bpf & libc::POLLIN != 0 {
        Some(Wake::Readable)
    } else if bpf != 0 {
        Some(Wake::Invalid)
    } else {
        None
    }
}

/// Blocks until `bpf` is readable, `cancel` reports any event, or `probe`
/// finds `bpf` unbound.
///
/// Each `poll` over `[bpf, cancel]` sleeps at most `interval`. On a timeout
/// `probe(bpf)` is asked for the binding: [`Binding::Attached`] polls
/// again, [`Binding::Detached`] returns [`Wake::Detached`], and an error is
/// returned as is. A `poll` interrupted by a signal is repeated; any other
/// `poll` failure is returned. A `poll` that reports events is classified
/// by [`classify_revents`], and a positive count with no classifiable event
/// is [`Wake::Invalid`], so this never loops on a ready descriptor.
pub(crate) fn wait_readable_or_detached(
    bpf: BorrowedFd<'_>,
    cancel: BorrowedFd<'_>,
    interval: Duration,
    probe: impl Fn(BorrowedFd<'_>) -> io::Result<Binding>,
) -> io::Result<Wake> {
    // At least 1 ms, so a zero interval cannot turn this into a busy loop.
    let timeout = c_int::try_from(interval.as_millis())
        .unwrap_or(c_int::MAX)
        .max(1);
    loop {
        let mut fds = [
            libc::pollfd {
                fd: bpf.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: cancel.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: `fds` is an array of two initialized `pollfd`s and the
        // count passed is its length, so the kernel reads and writes only
        // within it. Both descriptors are borrowed (`BorrowedFd`) for the
        // whole call, so neither can be closed or reused while it runs.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
        if ready < 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(err);
        }
        if ready == 0 {
            match probe(bpf)? {
                Binding::Attached => continue,
                Binding::Detached => return Ok(Wake::Detached),
            }
        }
        return Ok(classify_revents(fds[0].revents, fds[1].revents).unwrap_or(Wake::Invalid));
    }
}

/// One `recv`: calls `try_recv` until it returns something other than
/// `WouldBlock`, and awaits `wait()` after each `WouldBlock`.
///
/// It always tries before it waits: frames already queued inside `tun-rs`
/// leave the BPF descriptor not readable, so waiting first could sleep on a
/// frame that is already there. After a [`Wake::Readable`],
/// [`Wake::Detached`] or [`Wake::Cancelled`] it tries again (a detached
/// descriptor's read then fails with `ENXIO`). After a [`Wake::Invalid`], a
/// read that still would block is reported as an error with
/// [`UNPOLLABLE_MESSAGE`] instead of waiting again, so it never spins.
///
/// Cancel safety: no await sits between a read and its return, and the only
/// await is `wait()`, which reads nothing. Dropping the returned future
/// therefore never loses a packet; frames queued inside `tun-rs` stay there
/// for the next call.
pub(crate) async fn recv_with_bounded_wait<R, W, F>(
    mut try_recv: R,
    mut wait: W,
) -> io::Result<usize>
where
    R: FnMut() -> io::Result<usize>,
    W: FnMut() -> F,
    F: Future<Output = io::Result<Wake>>,
{
    let mut previous_wait_invalid = false;
    loop {
        // Keep this read and the `return` below free of any await (see the
        // cancel-safety note above).
        match try_recv() {
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {}
            result => return result,
        }
        if previous_wait_invalid {
            return Err(io::Error::other(UNPOLLABLE_MESSAGE));
        }
        previous_wait_invalid = wait().await? == Wake::Invalid;
    }
}

/// Asks the kernel which interface the BPF descriptor `fd` is bound to
/// (`BIOCGETIF`), classified by [`classify_binding`]. Needs no privilege
/// beyond holding the descriptor, and reads nothing from it.
#[cfg(target_os = "macos")]
pub(crate) fn bpf_binding(fd: BorrowedFd<'_>) -> io::Result<Binding> {
    // SAFETY: `ifreq` is a plain C struct (a name array and a union of
    // plain C values), for which all-zero bytes are a valid value.
    let mut ifr: libc::ifreq = unsafe { std::mem::zeroed() };
    // SAFETY: `BIOCGETIF` is `_IOR('B', 107, struct ifreq)`, so the kernel
    // writes at most `size_of::<ifreq>()` bytes to the pointer, which points
    // to `ifr`, live and exclusively borrowed for the call. `fd` is borrowed
    // for the call, so it cannot be closed or reused while it runs.
    let rc = unsafe { libc::ioctl(fd.as_raw_fd(), libc::BIOCGETIF, &raw mut ifr) };
    classify_binding(if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error().raw_os_error().unwrap_or(0))
    })
}

#[cfg(all(target_os = "macos", feature = "async"))]
pub(crate) use async_wait::{BpfWait, TapSource};

/// The async side: the per-device wait and its `recv_contract` source.
#[cfg(all(target_os = "macos", feature = "async"))]
mod async_wait {
    use std::io::{self, IoSliceMut};
    use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
    use std::sync::Arc;
    use std::time::Duration;

    use super::{
        Binding, DETACH_CHECK_INTERVAL, Wake, bpf_binding, recv_with_bounded_wait,
        wait_readable_or_detached,
    };
    use crate::recv_contract;

    /// A TAP device's bounded BPF wait: owns a duplicate of the device's
    /// BPF read descriptor and runs [`wait_readable_or_detached`] on it on
    /// the `blocking` thread pool, the same pool `tun-rs` itself waits on.
    /// It needs no reactor and no runtime timer, so it behaves the same
    /// under `async-io`, under `tokio`, and inside a blocking `recv`.
    pub(crate) struct BpfWait {
        /// The duplicate descriptor. Each wait thread holds a clone of the
        /// `Arc`, so a thread that outlives its `recv` future never polls a
        /// closed, or reused, descriptor number.
        bpf: Arc<OwnedFd>,
        probe: fn(BorrowedFd<'_>) -> io::Result<Binding>,
        interval: Duration,
    }

    impl BpfWait {
        /// Duplicates (`F_DUPFD_CLOEXEC`) the TAP's BPF descriptor.
        pub(crate) fn new(bpf: impl AsFd) -> io::Result<Self> {
            Ok(Self::from_owned(
                bpf.as_fd().try_clone_to_owned()?,
                bpf_binding,
                DETACH_CHECK_INTERVAL,
            ))
        }

        /// Builds a wait over an already-owned descriptor, with an explicit
        /// binding probe and interval (the unit tests use a pipe).
        fn from_owned(
            fd: OwnedFd,
            probe: fn(BorrowedFd<'_>) -> io::Result<Binding>,
            interval: Duration,
        ) -> Self {
            Self {
                bpf: Arc::new(fd),
                probe,
                interval,
            }
        }

        /// Waits on a `blocking` pool thread until the descriptor is
        /// readable or unbound. Dropping the returned future ends the
        /// thread's wait at once.
        pub(crate) async fn wait(&self) -> io::Result<Wake> {
            let (cancel_rx, cancel_tx) = std::io::pipe()?;
            let (bpf, probe, interval) = (Arc::clone(&self.bpf), self.probe, self.interval);
            let task = blocking::unblock(move || {
                wait_readable_or_detached(bpf.as_fd(), cancel_rx.as_fd(), interval, probe)
            });
            // The writer must live across the await: dropping this future
            // closes it, the thread's read end then reports end-of-file, and
            // the thread returns at once, releasing its `Arc` and its pipe
            // end. Dropping it any earlier would make every wait return
            // `Cancelled` immediately and the receive loop spin.
            let result = task.await;
            drop(cancel_tx);
            result
        }
    }

    /// The `recv_contract` source of a macOS TAP device in async builds:
    /// `tun-rs` reads (`try_recv_vectored` into `[buf, 1-byte sentinel]`),
    /// and [`BpfWait`] replaces `tun-rs`'s own readiness wait.
    pub(crate) struct TapSource<'a> {
        /// The device's `tun-rs` handle, which does every read.
        pub(crate) handle: &'a tun_rs::AsyncDevice,
        /// The device's bounded wait.
        pub(crate) wait: &'a BpfWait,
    }

    impl recv_contract::AsyncRecvSource for TapSource<'_> {
        async fn recv_native(&self, buf: &mut [u8]) -> io::Result<usize> {
            let mut sentinel = [0u8; 1];
            let mut bufs = [IoSliceMut::new(buf), IoSliceMut::new(&mut sentinel)];
            recv_with_bounded_wait(
                || self.handle.try_recv_vectored(&mut bufs),
                || self.wait.wait(),
            )
            .await
        }
    }

    /// Unprivileged tests of the wait over a pipe standing in for the BPF
    /// descriptor, with scripted binding probes.
    #[cfg(test)]
    mod tests {
        use std::future::Future;
        use std::io::Write;
        use std::task::{Context, Waker};
        use std::time::Instant;

        use super::super::tests::with_watchdog;
        use super::*;

        fn attached(_: BorrowedFd<'_>) -> io::Result<Binding> {
            Ok(Binding::Attached)
        }

        fn detached(_: BorrowedFd<'_>) -> io::Result<Binding> {
            Ok(Binding::Detached)
        }

        #[test]
        fn bpf_wait_wakes_on_data() {
            let wake = with_watchdog("BpfWait woken by data", || {
                let (reader, mut writer) = std::io::pipe().expect("create a pipe");
                let wait =
                    BpfWait::from_owned(OwnedFd::from(reader), attached, Duration::from_secs(10));
                let sender = std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(50));
                    writer.write_all(&[1]).expect("write 1 byte");
                    writer
                });
                let wake = futures::executor::block_on(wait.wait());
                drop(sender.join().expect("the sender thread does not panic"));
                wake
            });
            assert_eq!(wake.expect("the wait succeeds"), Wake::Readable);
        }

        #[test]
        fn dropping_a_pending_wait_releases_its_thread() {
            with_watchdog("a dropped wait releases its thread", || {
                let (reader, writer) = std::io::pipe().expect("create a pipe");
                let wait =
                    BpfWait::from_owned(OwnedFd::from(reader), attached, Duration::from_secs(10));
                {
                    let mut pending = std::pin::pin!(wait.wait());
                    let mut cx = Context::from_waker(Waker::noop());
                    assert!(pending.as_mut().poll(&mut cx).is_pending());
                }
                let deadline = Instant::now() + Duration::from_secs(2);
                while Arc::strong_count(&wait.bpf) != 1 {
                    assert!(
                        Instant::now() < deadline,
                        "the wait thread still holds the descriptor 2 s after the drop"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
                drop(writer);
            });
        }

        #[test]
        fn bpf_wait_reports_detached_from_the_probe() {
            let wake = with_watchdog("BpfWait reports Detached", || {
                let (reader, writer) = std::io::pipe().expect("create a pipe");
                let wait =
                    BpfWait::from_owned(OwnedFd::from(reader), detached, Duration::from_millis(10));
                let wake = futures::executor::block_on(wait.wait());
                drop(writer);
                wake
            });
            assert_eq!(wake.expect("the wait succeeds"), Wake::Detached);
        }
    }
}

/// Deterministic, unprivileged tests of the host-generic core, on pipes and
/// scripted closures. Every wait runs under a watchdog, so a regression
/// fails instead of hanging.
#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io::Write;
    use std::os::fd::AsFd;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Mutex, mpsc};

    use super::*;

    /// Runs `f` on its own thread and fails the test if it has not
    /// finished within 10 s.
    pub(super) fn with_watchdog<T: Send + 'static>(
        label: &str,
        f: impl FnOnce() -> T + Send + 'static,
    ) -> T {
        let (done, outcome) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = done.send(f());
        });
        outcome
            .recv_timeout(Duration::from_secs(10))
            .unwrap_or_else(|_| panic!("{label}: did not complete within 10 s"))
    }

    #[test]
    fn detach_check_interval_is_250_ms() {
        assert_eq!(DETACH_CHECK_INTERVAL, Duration::from_millis(250));
    }

    #[test]
    fn binding_classification() {
        assert_eq!(classify_binding(Ok(())).unwrap(), Binding::Attached);
        assert_eq!(
            classify_binding(Err(libc::EINVAL)).unwrap(),
            Binding::Detached
        );
        assert_eq!(
            classify_binding(Err(libc::ENXIO)).unwrap(),
            Binding::Detached
        );
        for code in [libc::ENOTTY, libc::EBADF] {
            let err = classify_binding(Err(code)).expect_err("an unexpected errno is an error");
            assert_eq!(err.raw_os_error(), Some(code));
        }
    }

    #[test]
    fn revents_classification() {
        assert_eq!(
            classify_revents(libc::POLLIN, libc::POLLIN),
            Some(Wake::Cancelled)
        );
        assert_eq!(classify_revents(0, libc::POLLHUP), Some(Wake::Cancelled));
        assert_eq!(classify_revents(libc::POLLIN, 0), Some(Wake::Readable));
        for bpf in [libc::POLLNVAL, libc::POLLERR, libc::POLLHUP] {
            assert_eq!(classify_revents(bpf, 0), Some(Wake::Invalid), "{bpf:#x}");
        }
        assert_eq!(classify_revents(0, 0), None);
    }

    /// Runs the wait on a pipe (the stand-in BPF descriptor) and a cancel
    /// pipe whose writer stays open, with `probe` counted.
    fn wait_on_pipes(
        preload: bool,
        interval: Duration,
        probe: impl Fn(usize) -> io::Result<Binding> + Send + 'static,
    ) -> (io::Result<Wake>, usize) {
        with_watchdog("wait on pipes", move || {
            let (bpf_rx, mut bpf_tx) = std::io::pipe().expect("create the data pipe");
            let (cancel_rx, cancel_tx) = std::io::pipe().expect("create the cancel pipe");
            if preload {
                bpf_tx.write_all(&[1]).expect("write 1 byte");
            }
            let calls = AtomicUsize::new(0);
            let result =
                wait_readable_or_detached(bpf_rx.as_fd(), cancel_rx.as_fd(), interval, |_| {
                    probe(calls.fetch_add(1, Ordering::SeqCst))
                });
            drop((bpf_tx, cancel_tx));
            (result, calls.load(Ordering::SeqCst))
        })
    }

    #[test]
    fn wait_returns_readable_when_data_is_pending() {
        let (result, calls) =
            wait_on_pipes(true, Duration::from_secs(10), |_| Ok(Binding::Attached));
        assert_eq!(result.expect("the wait succeeds"), Wake::Readable);
        assert_eq!(calls, 0, "the probe runs only on a timeout");
    }

    #[test]
    fn wait_returns_cancelled_when_the_writer_drops() {
        let result = with_watchdog("wait cancelled by the writer", || {
            let (bpf_rx, bpf_tx) = std::io::pipe().expect("create the data pipe");
            let (cancel_rx, cancel_tx) = std::io::pipe().expect("create the cancel pipe");
            let closer = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(50));
                drop(cancel_tx);
            });
            let result = wait_readable_or_detached(
                bpf_rx.as_fd(),
                cancel_rx.as_fd(),
                Duration::from_secs(10),
                |_| Ok(Binding::Attached),
            );
            closer.join().expect("the closer thread does not panic");
            drop(bpf_tx);
            result
        });
        assert_eq!(result.expect("the wait succeeds"), Wake::Cancelled);
    }

    #[test]
    fn wait_probes_on_each_timeout_until_detached() {
        let (result, calls) = wait_on_pipes(false, Duration::from_millis(10), |call| {
            Ok(if call < 2 {
                Binding::Attached
            } else {
                Binding::Detached
            })
        });
        assert_eq!(result.expect("the wait succeeds"), Wake::Detached);
        assert_eq!(calls, 3);
    }

    #[test]
    fn wait_returns_the_probe_error() {
        let (result, calls) = wait_on_pipes(false, Duration::from_millis(10), |_| {
            Err(io::Error::from_raw_os_error(libc::EBADF))
        });
        let err = result.expect_err("the probe error is returned");
        assert_eq!(err.raw_os_error(), Some(libc::EBADF));
        assert_eq!(calls, 1);
    }

    fn would_block() -> io::Result<usize> {
        Err(io::ErrorKind::WouldBlock.into())
    }

    /// Runs the receive loop over scripted reads and waits, returning its
    /// result and how many waits it awaited.
    fn run_recv_loop(
        reads: Vec<io::Result<usize>>,
        wakes: Vec<Wake>,
    ) -> (io::Result<usize>, usize) {
        let reads = Mutex::new(VecDeque::from(reads));
        let wakes = Mutex::new(VecDeque::from(wakes));
        let waits = AtomicUsize::new(0);
        let result = futures::executor::block_on(recv_with_bounded_wait(
            || {
                reads
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("the loop read more often than scripted")
            },
            || {
                waits.fetch_add(1, Ordering::SeqCst);
                let wake = wakes
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("the loop waited more often than scripted");
                async move { Ok(wake) }
            },
        ));
        (result, waits.load(Ordering::SeqCst))
    }

    #[test]
    fn recv_loop_tries_before_it_waits() {
        let (result, waits) = run_recv_loop(vec![Ok(3)], vec![]);
        assert_eq!(result.expect("the read succeeds"), 3);
        assert_eq!(waits, 0);
    }

    #[test]
    fn recv_loop_reads_again_after_readable() {
        let (result, waits) = run_recv_loop(vec![would_block(), Ok(5)], vec![Wake::Readable]);
        assert_eq!(result.expect("the read succeeds"), 5);
        assert_eq!(waits, 1);
    }

    #[test]
    fn recv_loop_returns_the_read_error_after_detached() {
        let (result, waits) = run_recv_loop(
            vec![
                would_block(),
                Err(io::Error::from_raw_os_error(libc::ENXIO)),
            ],
            vec![Wake::Detached],
        );
        let err = result.expect_err("the read error is returned");
        assert_eq!(err.raw_os_error(), Some(6));
        assert_eq!(waits, 1);
    }

    #[test]
    fn recv_loop_does_not_spin_after_an_invalid_wait() {
        let (result, waits) =
            run_recv_loop(vec![would_block(), would_block()], vec![Wake::Invalid]);
        let err = result.expect_err("a second WouldBlock after Invalid is an error");
        assert_eq!(err.to_string(), UNPOLLABLE_MESSAGE);
        assert_eq!(err.raw_os_error(), None);
        assert_eq!(waits, 1);
    }

    #[test]
    fn recv_loop_returns_a_wait_error() {
        let reads = Mutex::new(VecDeque::from(vec![would_block()]));
        let result = futures::executor::block_on(recv_with_bounded_wait(
            || reads.lock().unwrap().pop_front().expect("one read"),
            || async { Err(io::Error::from_raw_os_error(libc::EMFILE)) },
        ));
        assert_eq!(
            result
                .expect_err("the wait error is returned")
                .raw_os_error(),
            Some(libc::EMFILE)
        );
    }

    /// `BIOCGETIF` is `_IOR('B', 107, struct ifreq)`: `IOC_OUT`
    /// (`0x4000_0000`), the parameter length in bits 16-28, the group, and
    /// the number.
    #[test]
    #[cfg(target_os = "macos")]
    fn biocgetif_matches_the_xnu_definition() {
        use libc::c_ulong;
        let expected = 0x4000_0000
            | ((std::mem::size_of::<libc::ifreq>() as c_ulong & 0x1fff) << 16)
            | (c_ulong::from(b'B') << 8)
            | 107;
        assert_eq!(libc::BIOCGETIF, expected);
    }

    /// `BIOCGETIF` on a descriptor that is not a BPF device fails with an
    /// OS error (not a binding), so a wrong descriptor is never mistaken
    /// for an attached or detached one.
    #[test]
    #[cfg(target_os = "macos")]
    fn bpf_binding_on_a_non_bpf_descriptor_is_an_error() {
        let (reader, writer) = std::io::pipe().expect("create a pipe");
        let result = bpf_binding(reader.as_fd());
        drop(writer);
        let err = result.expect_err("a pipe has no BPF binding");
        assert!(err.raw_os_error().is_some(), "{err:?}");
    }
}
