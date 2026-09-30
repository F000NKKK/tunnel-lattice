//! The Linux `tokio` receive path: an error-aware readiness wait on a
//! duplicate of the device descriptor.
//!
//! After a Linux TUN/TAP device is deleted, the kernel's `tun_chr_poll`
//! reports the detached file as `EPOLLERR` only (no `EPOLLIN`, no
//! `EPOLLHUP`). Tokio turns that event into `Ready::ERROR` alone, and a
//! wait on `Interest::READABLE` (which is what `tun-rs` 2.8.11's
//! `AsyncDevice::recv`/`recv_vectored` use) is never woken by it, so a
//! pending `recv` would hang forever instead of reading the `EBADFD` that
//! reports the deletion.
//!
//! [`ErrorAwareReader`] therefore receives through its own duplicate of the
//! device descriptor, registered with the current Tokio reactor with
//! `Interest::READABLE | Interest::ERROR`, and waits with
//! `AsyncFd::ready` plus `AsyncFdReadyGuard::try_io`, the combined-interest
//! pattern Tokio documents. Sending, and every other device operation,
//! stays on the `tun-rs` handle.
//!
//! A second registration of the *same* descriptor number fails with
//! `EEXIST`, which is why the reader owns a duplicate: the duplicate is a
//! separate epoll registration of the same open file description, so both
//! registrations are notified.

use std::fs::File;
use std::io::{self, IoSliceMut, Read};
use std::os::fd::{AsFd, OwnedFd};

use tokio::io::Interest;
use tokio::io::unix::AsyncFd;

use crate::recv_contract;

/// A duplicate of a device descriptor, registered with the current Tokio
/// reactor for readable and error readiness.
///
/// Owned by one `TunRsDevice` next to its `tun-rs` handle and never shared
/// beyond it. It drops before that handle (it is declared first), which
/// deregisters it from the reactor and closes the duplicate; the kernel
/// releases the device file only when the last of the two descriptors
/// closes, so teardown timing is unchanged.
pub(crate) struct ErrorAwareReader(AsyncFd<File>);

impl ErrorAwareReader {
    /// Duplicates `fd` (`F_DUPFD_CLOEXEC`) and registers the duplicate with
    /// the current Tokio reactor.
    ///
    /// Invariant: `fd`'s open file description is `O_NONBLOCK`. `tun-rs`
    /// sets it when it builds an `AsyncDevice`, and the duplicate shares
    /// it, so a read with no packet queued returns `EAGAIN` instead of
    /// blocking a runtime thread.
    ///
    /// Must be called inside a Tokio runtime context, like building the
    /// `tun-rs` `AsyncDevice` itself.
    pub(crate) fn new(fd: impl AsFd) -> io::Result<Self> {
        Self::from_owned(fd.as_fd().try_clone_to_owned()?)
    }

    /// Registers an already-owned non-blocking descriptor.
    fn from_owned(fd: OwnedFd) -> io::Result<Self> {
        AsyncFd::with_interest(File::from(fd), Interest::READABLE | Interest::ERROR).map(Self)
    }
}

/// The sentinel read of `recv_contract` (`readv` into `[buf, 1-byte
/// sentinel]`), woken by readable *or* error readiness.
impl recv_contract::AsyncRecvSource for ErrorAwareReader {
    async fn recv_native(&self, buf: &mut [u8]) -> io::Result<usize> {
        // `AsyncFd::ready` does not take part in Tokio's cooperative
        // budget, so a busy device could otherwise starve other tasks. The
        // budget is spent *before* the read, as Tokio's own I/O does: a
        // yield after `readv` had dequeued a packet would lose that packet
        // if the caller then cancelled the `recv`.
        tokio::task::coop::consume_budget().await;
        let mut sentinel = [0u8; 1];
        let mut bufs = [IoSliceMut::new(buf), IoSliceMut::new(&mut sentinel)];
        loop {
            let mut guard = self.0.ready(Interest::READABLE | Interest::ERROR).await?;
            let result = match guard.try_io(|fd| {
                let mut file: &File = fd.get_ref();
                file.read_vectored(&mut bufs)
            }) {
                Ok(result) => result,
                // `try_io` saw `WouldBlock` and cleared the readiness it
                // observed (tick-checked, so a newer event is kept): wait
                // again.
                Err(_would_block) => continue,
            };
            return result;
        }
    }
}

/// Deterministic, unprivileged tests of the reader on plain descriptors: a
/// pipe's write end whose read end is closed is reported by epoll as
/// `EPOLLERR` only, exactly like a detached tun file, and reading it fails
/// with `EBADF`.
#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::os::unix::net::UnixDatagram;
    use std::sync::mpsc;
    use std::task::Poll;
    use std::time::Duration;

    use super::*;
    use crate::recv_contract::AsyncRecvSource;

    /// Runs `f` on its own thread and fails the test if it has not
    /// finished within 10 s, so a regression hangs nothing.
    fn with_watchdog<T: Send + 'static>(label: &str, f: impl FnOnce() -> T + Send + 'static) -> T {
        let (done, outcome) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = done.send(f());
        });
        outcome
            .recv_timeout(Duration::from_secs(10))
            .unwrap_or_else(|_| panic!("{label}: did not complete within 10 s"))
    }

    /// The runtime flavor `TunRsDevice` documents as required.
    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .enable_io()
            .build()
            .expect("build a Tokio runtime")
    }

    fn assert_ebadf(result: io::Result<usize>) {
        match result {
            Err(err) => assert_eq!(err.raw_os_error(), Some(libc::EBADF), "{err:?}"),
            Ok(n) => panic!("expected EBADF, read {n} bytes"),
        }
    }

    fn datagram_pair() -> (UnixDatagram, UnixDatagram) {
        let (ours, theirs) = UnixDatagram::pair().expect("create a datagram pair");
        ours.set_nonblocking(true).expect("make it non-blocking");
        (ours, theirs)
    }

    /// A `recv` already waiting when the descriptor becomes error-only (the
    /// Linux device deletion) is woken and returns the kernel's error.
    #[test]
    fn error_only_readiness_wakes_a_pending_recv() {
        let result = with_watchdog("pending recv on an error-only pipe", || {
            let (reader, writer) = io::pipe().expect("create a pipe");
            runtime().block_on(async move {
                let reader_end = ErrorAwareReader::from_owned(OwnedFd::from(writer))
                    .expect("register the write end");
                let closer = std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(100));
                    drop(reader);
                });
                let mut buf = [0u8; 8];
                let result = reader_end.recv_native(&mut buf).await;
                closer.join().expect("the closer thread does not panic");
                result
            })
        });
        assert_ebadf(result);
    }

    /// A descriptor that is already error-only when it is registered is
    /// reported at once.
    #[test]
    fn error_only_readiness_before_registration_is_seen() {
        let result = with_watchdog("recv on an already error-only pipe", || {
            let (reader, writer) = io::pipe().expect("create a pipe");
            drop(reader);
            runtime().block_on(async move {
                let reader_end = ErrorAwareReader::from_owned(OwnedFd::from(writer))
                    .expect("register the write end");
                let mut buf = [0u8; 8];
                reader_end.recv_native(&mut buf).await
            })
        });
        assert_ebadf(result);
    }

    /// Ordinary data: a datagram that fits is read whole; one that does
    /// not spills into the sentinel, so the reported length exceeds the
    /// buffer (which `recv_contract` turns into `BufferTooSmall`).
    #[test]
    fn reads_datagrams_and_reports_oversize_through_the_sentinel() {
        let (fits, oversize) = with_watchdog("datagram reads", || {
            let (ours, theirs) = datagram_pair();
            runtime().block_on(async move {
                let reader =
                    ErrorAwareReader::from_owned(OwnedFd::from(ours)).expect("register the socket");
                let mut buf = [0u8; 8];
                theirs.send(b"abc").expect("send 3 bytes");
                let fits = reader.recv_native(&mut buf).await.expect("read 3 bytes");
                assert_eq!(&buf[..fits], b"abc");
                theirs.send(b"123456789").expect("send 9 bytes");
                let oversize = reader.recv_native(&mut buf).await.expect("read 9 bytes");
                (fits, oversize)
            })
        });
        assert_eq!(fits, 3);
        assert_eq!(oversize, 9);
    }

    /// A `recv` started before any data arrives completes when it does.
    #[test]
    fn pending_recv_is_woken_by_data() {
        let (n, buf) = with_watchdog("pending recv woken by data", || {
            let (ours, theirs) = datagram_pair();
            runtime().block_on(async move {
                let reader =
                    ErrorAwareReader::from_owned(OwnedFd::from(ours)).expect("register the socket");
                let sender = std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(100));
                    theirs.send(b"late").expect("send 4 bytes");
                });
                let mut buf = [0u8; 8];
                let n = reader.recv_native(&mut buf).await.expect("read 4 bytes");
                sender.join().expect("the sender thread does not panic");
                (n, buf)
            })
        });
        assert_eq!(&buf[..n], b"late");
    }

    /// A `recv` that yields because the task's cooperative budget is spent,
    /// and is then cancelled, has not consumed a packet: the next `recv`
    /// still reads it.
    #[test]
    fn a_recv_cancelled_by_an_exhausted_budget_loses_no_packet() {
        let (first_poll_pending, n, buf) = with_watchdog("recv cancelled by the budget", || {
            let (ours, theirs) = datagram_pair();
            let runtime = runtime();
            runtime.block_on(async move {
                tokio::spawn(async move {
                    let reader = ErrorAwareReader::from_owned(OwnedFd::from(ours))
                        .expect("register the socket");
                    theirs.send(b"keep").expect("send 4 bytes");
                    let mut buf = [0u8; 8];
                    while tokio::task::coop::has_budget_remaining() {
                        tokio::task::coop::consume_budget().await;
                    }
                    let first_poll_pending = {
                        let mut recv = std::pin::pin!(reader.recv_native(&mut buf));
                        std::future::poll_fn(|cx| Poll::Ready(recv.as_mut().poll(cx).is_pending()))
                            .await
                    };
                    // A fresh poll of the task restores its budget.
                    tokio::task::yield_now().await;
                    let n = reader.recv_native(&mut buf).await.expect("read 4 bytes");
                    (first_poll_pending, n, buf)
                })
                .await
                .expect("the task does not panic")
            })
        });
        assert!(first_poll_pending, "the budget did not force a yield");
        assert_eq!(&buf[..n], b"keep");
    }

    /// Canary for the reason this reader exists: a plain `READABLE` wait is
    /// not woken by error-only readiness. If this starts failing, Tokio has
    /// changed that behavior and the reader should be re-evaluated.
    #[test]
    fn canary_tokio_readable_interest_ignores_error_only_readiness() {
        let (reader, writer) = io::pipe().expect("create a pipe");
        drop(reader);
        let runtime = runtime();
        let (registered_tx, registered) = mpsc::channel::<io::Result<()>>();
        let (woken_tx, woken) = mpsc::channel::<()>();
        runtime.spawn(async move {
            let file = File::from(OwnedFd::from(writer));
            let fd = match AsyncFd::with_interest(file, Interest::READABLE) {
                Ok(fd) => fd,
                Err(err) => {
                    let _ = registered_tx.send(Err(err));
                    return;
                }
            };
            let _ = registered_tx.send(Ok(()));
            let _ = fd.readable().await;
            let _ = woken_tx.send(());
        });

        match registered.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(())) => {}
            Ok(Err(err)) => panic!("the canary could not register its descriptor: {err}"),
            Err(_) => panic!("the canary did not register within 10 s"),
        }
        let woken = woken.recv_timeout(Duration::from_millis(300));
        runtime.shutdown_background();
        assert!(
            woken.is_err(),
            "tokio now wakes READABLE on EPOLLERR; re-evaluate the ErrorAwareReader \
             (see this module's docs): {woken:?}"
        );
    }
}
