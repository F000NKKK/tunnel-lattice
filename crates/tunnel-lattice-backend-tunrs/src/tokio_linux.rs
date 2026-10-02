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
use tunnel_lattice_core::Result;
use tunnel_lattice_model::{AdminState, DeviceKind};

use crate::offload_queue::OffloadRx;
use crate::open_contract::HostOs;
use crate::recv_contract::{self, DeferredTooSmall, Step};

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

    /// `recv` on an offload-framed queue: a batch of one through
    /// [`Self::recv_offload_batch`], which therefore never drains.
    pub(crate) async fn recv_offload(
        &self,
        os: HostOs,
        kind: DeviceKind,
        rx: &OffloadRx,
        out: &mut [u8],
    ) -> Result<usize> {
        let mut lens = [0];
        self.recv_offload_batch(os, kind, rx, &mut [out], &mut lens)
            .await
            .map(|_| lens[0])
    }

    /// `recv_batch` on an offload-framed queue: position 0 serves the next
    /// pending segment of `rx`, or waits for readable or error readiness and
    /// reads one frame into `rx`'s staging buffer (see `offload_queue`);
    /// then the staging drains, reading further frames with
    /// `AsyncFd::try_io`, which never waits (a `WouldBlock` clears the
    /// readiness it saw, as `try_io` on a guard does).
    ///
    /// The cooperative budget is spent before anything is served or read,
    /// so a yield never happens after a segment was taken or a frame was
    /// read. The staging lock is taken only between awaits, and nothing
    /// awaits between a successful read and the return, so a dropped
    /// future leaves every unserved segment pending for the next call.
    pub(crate) async fn recv_offload_batch(
        &self,
        os: HostOs,
        kind: DeviceKind,
        rx: &OffloadRx,
        bufs: &mut [&mut [u8]],
        lens: &mut [usize],
    ) -> Result<usize> {
        if recv_contract::batch_capacity(bufs, lens) == 0 {
            return Ok(0);
        }
        let mut read_nowait = |buf: &mut [u8]| {
            self.0.try_io(Interest::READABLE, |file| {
                let mut file: &File = file;
                file.read(buf)
            })
        };
        tokio::task::coop::consume_budget().await;
        loop {
            if let Some(result) = rx.lock().pending_batch(os, bufs, lens, &mut read_nowait) {
                return result;
            }
            let ready = self.0.ready(Interest::READABLE | Interest::ERROR).await;
            let mut staging = rx.lock();
            if let Some(result) = staging.pending_batch(os, bufs, lens, &mut read_nowait) {
                return result;
            }
            let result = match ready {
                Ok(mut guard) => match guard.try_io(|fd| {
                    let mut file: &File = fd.get_ref();
                    file.read(staging.read_buf())
                }) {
                    Ok(result) => result,
                    // As in `recv_native`: the readiness was cleared, wait
                    // again.
                    Err(_would_block) => continue,
                },
                Err(err) => Err(err),
            };
            if let Some(result) = staging.read_batch(os, kind, result, bufs, lens, &mut read_nowait)
            {
                return result;
            }
        }
    }

    /// `recv_batch` on a plainly framed queue. Position 0 is exactly
    /// `recv`: [`recv_contract::recv_async`] over this reader's `recv_native`,
    /// with the cooperative budget spent before each native attempt and
    /// the readable-or-error wait. The positions after it read
    /// `[bufs[k], sentinel]` with `try_io` on the readiness guard position
    /// 0 read with, never waiting again: a `WouldBlock` there clears the
    /// readiness and ends the batch (see `recv_contract`, "Draining a
    /// batch"). Nothing awaits after position 0's read, so a dropped future
    /// has taken no packet. The caller has checked the capacity and taken
    /// any deferred error.
    pub(crate) async fn recv_batch_plain<O>(
        &self,
        os: HostOs,
        kind: DeviceKind,
        oper: &O,
        bufs: &mut [&mut [u8]],
        lens: &mut [usize],
        deferred: &DeferredTooSmall,
    ) -> Result<usize>
    where
        O: Fn() -> io::Result<AdminState> + Sync,
    {
        let mut retried = false;
        'native: loop {
            // `recv_native`'s budget, spent before every native attempt.
            tokio::task::coop::consume_budget().await;
            let (Some(buf), Some(len)) = (bufs.first_mut(), lens.first_mut()) else {
                return Ok(0);
            };
            let buf_len = buf.len();
            loop {
                let mut guard = match self.0.ready(Interest::READABLE | Interest::ERROR).await {
                    Ok(guard) => guard,
                    Err(err) => match recv_contract::recv_step(
                        os,
                        kind,
                        buf_len,
                        Err(err),
                        &mut retried,
                        oper,
                    ) {
                        Step::Retry => continue 'native,
                        Step::Done(result) => return result.map(|_| 0),
                    },
                };
                let result = match guard.try_io(|fd| sentinel_read(fd.get_ref(), buf)) {
                    Ok(result) => result,
                    // As in `recv_native`: wait again.
                    Err(_would_block) => continue,
                };
                match recv_contract::recv_step(os, kind, buf_len, result, &mut retried, oper) {
                    Step::Retry => continue 'native,
                    Step::Done(result) => *len = result?,
                }
                return Ok(recv_contract::drain_plain(
                    os,
                    bufs,
                    lens,
                    1,
                    deferred,
                    |buf| match guard.try_io(|fd| sentinel_read(fd.get_ref(), buf)) {
                        Ok(result) => result,
                        Err(_would_block) => Err(io::ErrorKind::WouldBlock.into()),
                    },
                ));
            }
        }
    }
}

/// One sentinel read (`readv` into `[buf, 1-byte sentinel]`) on the
/// reader's descriptor, which is non-blocking.
fn sentinel_read(file: &File, buf: &mut [u8]) -> io::Result<usize> {
    let mut sentinel = [0u8; 1];
    let mut file = file;
    file.read_vectored(&mut [IoSliceMut::new(buf), IoSliceMut::new(&mut sentinel)])
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

    /// The offload receive loop over a datagram socket standing in for an
    /// offload-framed queue: segments in order, and a `recv` cancelled by
    /// an exhausted budget, or while it waits for readiness, loses no
    /// segment.
    #[test]
    fn offload_recv_serves_segments_and_survives_cancellation() {
        use crate::offload_queue::test_frames::{assert_segment, udp4_frame};

        let served = with_watchdog("offload recv", || {
            let (ours, theirs) = datagram_pair();
            runtime().block_on(async move {
                tokio::spawn(async move {
                    let reader = ErrorAwareReader::from_owned(OwnedFd::from(ours))
                        .expect("register the socket");
                    let rx = OffloadRx::new();
                    let mut out = vec![0u8; 1500];
                    macro_rules! recv {
                        ($out:expr) => {
                            reader.recv_offload(HostOs::Linux, DeviceKind::Tun, &rx, $out)
                        };
                    }
                    let mut served = Vec::new();

                    // Cancelled while waiting for readiness.
                    {
                        let mut waiting = std::pin::pin!(recv!(&mut out));
                        let pending = std::future::poll_fn(|cx| {
                            Poll::Ready(waiting.as_mut().poll(cx).is_pending())
                        })
                        .await;
                        assert!(pending, "nothing to read yet");
                    }
                    theirs.send(&udp4_frame(100, 3, 100)).expect("send a frame");
                    let n = recv!(&mut out).await.expect("segment 0");
                    served.push(out[..n].to_vec());

                    // Cancelled by the budget, mid-split.
                    while tokio::task::coop::has_budget_remaining() {
                        tokio::task::coop::consume_budget().await;
                    }
                    {
                        let mut yielding = std::pin::pin!(recv!(&mut out));
                        let pending = std::future::poll_fn(|cx| {
                            Poll::Ready(yielding.as_mut().poll(cx).is_pending())
                        })
                        .await;
                        assert!(pending, "the budget did not force a yield");
                    }
                    tokio::task::yield_now().await;
                    for k in 1..3 {
                        let n = recv!(&mut out).await.unwrap_or_else(|err| {
                            panic!("segment {k}: {err:?}");
                        });
                        served.push(out[..n].to_vec());
                    }
                    served
                })
                .await
                .expect("the task does not panic")
            })
        });
        assert_eq!(served.len(), 3);
        for (k, packet) in served.iter().enumerate() {
            assert_segment(packet, k, 100);
        }
    }

    /// A status read for the plain batch tests, which never meet a
    /// Windows TAP 995.
    fn no_status() -> io::Result<AdminState> {
        panic!("the status is read only for a Windows TAP 995")
    }

    /// Polls `future` once and reports whether it is still pending; the
    /// future is dropped either way.
    async fn first_poll_pending<F: Future>(future: F) -> bool {
        let mut future = std::pin::pin!(future);
        std::future::poll_fn(|cx| Poll::Ready(future.as_mut().poll(cx).is_pending())).await
    }

    /// The plain batch: position 0 waits, the rest drains with `try_io` on
    /// the same guard, in order, and stops at the empty socket; a datagram
    /// longer than its buffer after the first ends the batch and is
    /// reported once, through the deferred flag.
    #[test]
    fn a_plain_batch_drains_in_order_and_defers_a_too_long_packet() {
        let (first, second, deferred_err, last) = with_watchdog("plain batch", || {
            let (ours, theirs) = datagram_pair();
            runtime().block_on(async move {
                let reader =
                    ErrorAwareReader::from_owned(OwnedFd::from(ours)).expect("register the socket");
                let deferred = DeferredTooSmall::new();
                let mut bufs = vec![vec![0u8; 8]; 4];
                let mut lens = [0; 4];
                let batch = async |bufs: &mut Vec<Vec<u8>>, lens: &mut [usize]| {
                    let mut slices: Vec<&mut [u8]> =
                        bufs.iter_mut().map(Vec::as_mut_slice).collect();
                    reader
                        .recv_batch_plain(
                            HostOs::Linux,
                            DeviceKind::Tun,
                            &no_status,
                            &mut slices,
                            lens,
                            &deferred,
                        )
                        .await
                };
                for packet in [&b"one"[..], b"two", b"three"] {
                    theirs.send(packet).expect("send a datagram");
                }
                let n = batch(&mut bufs, &mut lens).await.expect("a batch");
                let first: Vec<Vec<u8>> = (0..n).map(|i| bufs[i][..lens[i]].to_vec()).collect();
                for packet in [&b"four"[..], b"123456789", b"last"] {
                    theirs.send(packet).expect("send a datagram");
                }
                let n = batch(&mut bufs, &mut lens).await.expect("a batch");
                let second: Vec<Vec<u8>> = (0..n).map(|i| bufs[i][..lens[i]].to_vec()).collect();
                let deferred_err = deferred.take();
                let n = batch(&mut bufs, &mut lens).await.expect("a batch");
                let last: Vec<Vec<u8>> = (0..n).map(|i| bufs[i][..lens[i]].to_vec()).collect();
                (first, second, deferred_err, last)
            })
        });
        assert_eq!(first, [&b"one"[..], b"two", b"three"]);
        assert_eq!(second, [&b"four"[..]]);
        assert!(matches!(
            deferred_err,
            Err(tunnel_lattice_core::Error::BufferTooSmall)
        ));
        assert_eq!(last, [&b"last"[..]]);
    }

    /// A plain batch dropped while it waits, or because the cooperative
    /// budget forced a yield, has taken no packet.
    #[test]
    fn a_dropped_plain_batch_loses_no_packet() {
        let got = with_watchdog("dropped plain batch", || {
            let (ours, theirs) = datagram_pair();
            runtime().block_on(async move {
                tokio::spawn(async move {
                    let reader = ErrorAwareReader::from_owned(OwnedFd::from(ours))
                        .expect("register the socket");
                    let deferred = DeferredTooSmall::new();
                    let mut a = [0u8; 8];
                    let mut b = [0u8; 8];
                    let mut lens = [0; 2];
                    macro_rules! batch {
                        () => {
                            reader.recv_batch_plain(
                                HostOs::Linux,
                                DeviceKind::Tun,
                                &no_status,
                                &mut [&mut a[..], &mut b[..]],
                                &mut lens,
                                &deferred,
                            )
                        };
                    }
                    assert!(first_poll_pending(batch!()).await, "nothing to read yet");
                    theirs.send(b"keep").expect("send a datagram");
                    theirs.send(b"also").expect("send a datagram");
                    while tokio::task::coop::has_budget_remaining() {
                        tokio::task::coop::consume_budget().await;
                    }
                    assert!(
                        first_poll_pending(batch!()).await,
                        "the budget forced a yield"
                    );
                    tokio::task::yield_now().await;
                    let n = batch!().await.expect("a batch");
                    assert_eq!(n, 2);
                    (a[..lens[0]].to_vec(), b[..lens[1]].to_vec())
                })
                .await
                .expect("the task does not panic")
            })
        });
        assert_eq!(got, (b"keep".to_vec(), b"also".to_vec()));
    }

    /// A plain batch pending when the descriptor becomes error-only (the
    /// Linux device deletion) is woken and returns the kernel's error.
    #[test]
    fn error_only_readiness_wakes_a_pending_plain_batch() {
        let result = with_watchdog("pending batch on an error-only pipe", || {
            let (reader, writer) = io::pipe().expect("create a pipe");
            runtime().block_on(async move {
                let reader_end = ErrorAwareReader::from_owned(OwnedFd::from(writer))
                    .expect("register the write end");
                let closer = std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(100));
                    drop(reader);
                });
                let mut buf = [0u8; 8];
                let result = reader_end
                    .recv_batch_plain(
                        HostOs::Linux,
                        DeviceKind::Tun,
                        &no_status,
                        &mut [&mut buf[..]],
                        &mut [0],
                        &DeferredTooSmall::new(),
                    )
                    .await;
                closer.join().expect("the closer thread does not panic");
                result
            })
        });
        // EBADF has no typed variant: the platform fallback.
        assert!(
            matches!(
                result,
                Err(tunnel_lattice_core::Error::Platform(
                    tunnel_lattice_core::PlatformErrorCode::Linux(libc::EBADF)
                ))
            ),
            "{result:?}"
        );
    }

    /// The offload batch: every queued segment across frames in one call;
    /// a batch dropped while it waits, or by the budget mid-split, loses no
    /// segment.
    #[test]
    fn an_offload_batch_drains_frames_and_survives_cancellation() {
        use crate::offload_queue::test_frames::{assert_segment, udp4_frame};

        let served = with_watchdog("offload batch", || {
            let (ours, theirs) = datagram_pair();
            runtime().block_on(async move {
                tokio::spawn(async move {
                    let reader = ErrorAwareReader::from_owned(OwnedFd::from(ours))
                        .expect("register the socket");
                    let rx = OffloadRx::new();
                    let mut bufs = vec![vec![0u8; 1500]; 8];
                    let mut lens = [0; 8];
                    let mut served = Vec::new();
                    macro_rules! batch {
                        ($count:expr) => {
                            reader.recv_offload_batch(
                                HostOs::Linux,
                                DeviceKind::Tun,
                                &rx,
                                &mut bufs[..$count]
                                    .iter_mut()
                                    .map(Vec::as_mut_slice)
                                    .collect::<Vec<_>>(),
                                &mut lens[..$count],
                            )
                        };
                    }
                    assert!(first_poll_pending(batch!(8)).await, "nothing to read yet");
                    theirs.send(&udp4_frame(100, 3, 100)).expect("send a frame");
                    theirs.send(&udp4_frame(200, 2, 50)).expect("send a frame");
                    let n = batch!(2).await.expect("a batch");
                    served.extend((0..n).map(|i| bufs[i][..lens[i]].to_vec()));
                    while tokio::task::coop::has_budget_remaining() {
                        tokio::task::coop::consume_budget().await;
                    }
                    assert!(
                        first_poll_pending(batch!(8)).await,
                        "the budget forced a yield"
                    );
                    tokio::task::yield_now().await;
                    let n = batch!(8).await.expect("a batch");
                    served.extend((0..n).map(|i| bufs[i][..lens[i]].to_vec()));
                    served
                })
                .await
                .expect("the task does not panic")
            })
        });
        assert_eq!(served.len(), 5);
        for (packet, (k, len)) in
            served
                .iter()
                .zip([(0, 100), (1, 100), (2, 100), (0, 200), (1, 50)])
        {
            assert_segment(packet, k, len);
        }
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
