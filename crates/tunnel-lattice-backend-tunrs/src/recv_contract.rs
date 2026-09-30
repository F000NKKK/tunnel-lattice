//! The `recv`/`send` error contract: the transient conditions retried
//! inside the backend and the too-small-buffer normalization, applied in
//! front of the generic [`io_error`] mapping.
//!
//! Like [`crate::open_contract`], the rules take an explicit [`HostOs`]
//! instead of `#[cfg]` blocks, so every per-OS rule is exercised by the
//! ordinary unit tests on every host.
//!
//! | OS | Native signal | Result |
//! |---|---|---|
//! | Linux, macOS | raw `EINTR` (`Interrupted`) | retried (re-enters the blocking read or readiness wait) |
//! | macOS | code-less `UnexpectedEof`, message exactly `"recv buffer is empty"` (feth TAP: a BPF read with no complete frame) | retried |
//! | Linux, macOS (recv) | `Ok(n)` with `n > buf.len()` from the sentinel read | [`Error::BufferTooSmall`] |
//! | macOS (recv) | code-less `InvalidData` (feth TAP: frame larger than the buffer) | [`Error::BufferTooSmall`] |
//! | Windows (recv) | code-less `InvalidInput` (wintun / tap-windows: packet larger than the buffer) | [`Error::BufferTooSmall`] |
//! | anything else | | [`io_error`], so every other `UnexpectedEof` (the shutdown pipe's `"close"`, Wintun's `ERROR_HANDLE_EOF`) stays [`Error::Disconnected`] and a code-less `Interrupted` (`"cancel"`) is not retried |
//!
//! Linux TUN/TAP and macOS utun truncate an oversize packet silently, so on
//! unix the backend reads into `[buf, 1-byte sentinel]` with `readv`: a
//! packet that does not fit in `buf` spills into the sentinel, and the
//! reported length exceeds `buf.len()`. The packet is then already consumed
//! (discarded); only its first `buf.len()` bytes were written.
//!
//! The retried message and the error kinds come from `tun-rs` 2.8.11, the
//! workspace's minimum `tun-rs` requirement; re-verify them whenever the
//! resolved `tun-rs` changes. The unit tests pin this crate's own
//! constants, not the upstream strings.

use std::future::Future;
use std::io;

use tunnel_lattice_core::{Error, Result};

use crate::io_error;
use crate::open_contract::HostOs;

/// `tun-rs` 2.8.11 `platform/macos/tap/mod.rs` (`recv`, `recv_uninit`,
/// `recv_vectored`): the message of the code-less `UnexpectedEof` returned
/// when a BPF read produced no complete frame. Transient: retried.
pub(crate) const MACOS_TAP_EMPTY_READ_MESSAGE: &str = "recv buffer is empty";

/// `EINTR`, identical on Linux and macOS (asserted against `libc` by the
/// unit tests on those targets). Written out so the rules compile, and are
/// tested, on every host.
const EINTR: i32 = 4;

/// What a `recv`/`send` loop does with one native attempt's result.
#[derive(Debug)]
pub(crate) enum Step {
    /// A transient condition: make the native call again.
    Retry,
    /// The call is finished with this result.
    Done(Result<usize>),
}

/// Returns `true` for a transient native error that `recv`/`send` retry
/// instead of reporting (see the module table).
pub(crate) fn is_transient(os: HostOs, err: &io::Error) -> bool {
    // Matched on the raw code alone: on Linux and macOS code 4 is always
    // `EINTR` (which std decodes as `Interrupted`), whereas `kind()` is
    // decoded with the *running* host's table, so a kind check would make
    // the Linux/macOS rule fail when the tests run on Windows.
    let eintr = matches!(os, HostOs::Linux | HostOs::Macos) && err.raw_os_error() == Some(EINTR);
    let feth_empty_read = os == HostOs::Macos
        && err.kind() == io::ErrorKind::UnexpectedEof
        && err.raw_os_error().is_none()
        && err.to_string() == MACOS_TAP_EMPTY_READ_MESSAGE;
    eintr || feth_empty_read
}

/// Maps one native `recv` attempt that read into a `buf_len`-byte buffer.
pub(crate) fn recv_step(os: HostOs, buf_len: usize, result: io::Result<usize>) -> Step {
    match result {
        Ok(n) if n > buf_len => Step::Done(Err(Error::BufferTooSmall)),
        Ok(n) => Step::Done(Ok(n)),
        Err(err) if is_transient(os, &err) => Step::Retry,
        Err(err) => Step::Done(Err(recv_error(os, err))),
    }
}

/// Maps one native `send` attempt.
pub(crate) fn send_step(os: HostOs, result: io::Result<usize>) -> Step {
    match result {
        Ok(n) => Step::Done(Ok(n)),
        Err(err) if is_transient(os, &err) => Step::Retry,
        Err(err) => Step::Done(Err(io_error(err))),
    }
}

/// Maps a non-transient `recv` error: the too-small-buffer signals first,
/// then [`io_error`].
fn recv_error(os: HostOs, err: io::Error) -> Error {
    let too_small = err.raw_os_error().is_none()
        && match os {
            HostOs::Macos => err.kind() == io::ErrorKind::InvalidData,
            HostOs::Windows => err.kind() == io::ErrorKind::InvalidInput,
            HostOs::Linux | HostOs::Other => false,
        };
    if too_small {
        Error::BufferTooSmall
    } else {
        io_error(err)
    }
}

/// Blocking `recv`: calls `read(buf)` until it returns something other
/// than a transient error.
#[cfg_attr(
    all(feature = "async", not(test)),
    expect(dead_code, reason = "async builds block on `recv_async` instead")
)]
pub(crate) fn recv_blocking(
    os: HostOs,
    buf: &mut [u8],
    mut read: impl FnMut(&mut [u8]) -> io::Result<usize>,
) -> Result<usize> {
    let buf_len = buf.len();
    loop {
        if let Step::Done(result) = recv_step(os, buf_len, read(buf)) {
            return result;
        }
    }
}

/// Blocking `send`: calls `write()` until it returns something other than
/// a transient error.
#[cfg_attr(
    all(feature = "async", not(test)),
    expect(dead_code, reason = "async builds block on `send_async` instead")
)]
pub(crate) fn send_blocking(
    os: HostOs,
    mut write: impl FnMut() -> io::Result<usize>,
) -> Result<usize> {
    loop {
        if let Step::Done(result) = send_step(os, write()) {
            return result;
        }
    }
}

/// One native async read attempt, re-awaited by [`recv_async`] after a
/// transient error.
pub(crate) trait AsyncRecvSource {
    /// Reads one packet into `buf`. On unix this is the sentinel read
    /// described in the module docs, so the result may exceed `buf.len()`.
    fn recv_native(&self, buf: &mut [u8]) -> impl Future<Output = io::Result<usize>> + Send;
}

/// Async `recv`: re-awaits `source.recv_native(buf)` until it returns
/// something other than a transient error. Each retry re-enters the native
/// readiness wait, so it never spins.
#[cfg_attr(
    all(not(feature = "async"), not(test)),
    expect(dead_code, reason = "only async builds hold an async handle")
)]
pub(crate) async fn recv_async<S: AsyncRecvSource + ?Sized>(
    os: HostOs,
    source: &S,
    buf: &mut [u8],
) -> Result<usize> {
    let buf_len = buf.len();
    loop {
        if let Step::Done(result) = recv_step(os, buf_len, source.recv_native(buf).await) {
            return result;
        }
    }
}

/// Async `send`: re-awaits `write()` until it returns something other than
/// a transient error.
#[cfg_attr(
    all(not(feature = "async"), not(test)),
    expect(dead_code, reason = "only async builds hold an async handle")
)]
pub(crate) async fn send_async<F, Fut>(os: HostOs, mut write: F) -> Result<usize>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = io::Result<usize>>,
{
    loop {
        if let Step::Done(result) = send_step(os, write().await) {
            return result;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::pin::pin;
    use std::task::{Context, Poll, Waker};

    use tunnel_lattice_core::PlatformErrorCode;

    use super::*;

    const ALL_OSES: [HostOs; 4] = [HostOs::Linux, HostOs::Macos, HostOs::Windows, HostOs::Other];

    fn eintr() -> io::Error {
        io::Error::from_raw_os_error(EINTR)
    }

    fn feth_empty_read() -> io::Error {
        io::Error::new(io::ErrorKind::UnexpectedEof, MACOS_TAP_EMPTY_READ_MESSAGE)
    }

    fn close() -> io::Error {
        io::Error::new(io::ErrorKind::UnexpectedEof, "close")
    }

    fn cancel() -> io::Error {
        io::Error::new(io::ErrorKind::Interrupted, "cancel")
    }

    fn recv_result(os: HostOs, buf_len: usize, result: io::Result<usize>) -> Option<Result<usize>> {
        match recv_step(os, buf_len, result) {
            Step::Retry => None,
            Step::Done(result) => Some(result),
        }
    }

    /// Drives a future that is ready without a real waker (every mock
    /// below completes on its first poll).
    fn ready<F: Future>(future: F) -> F::Output {
        let mut future = pin!(future);
        let mut cx = Context::from_waker(Waker::noop());
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(output) => output,
            Poll::Pending => panic!("mock futures complete on the first poll"),
        }
    }

    /// Pins the strings this module matches on to `tun-rs` 2.8.11.
    #[test]
    fn pinned_tun_rs_2_8_11_recv_messages() {
        assert_eq!(MACOS_TAP_EMPTY_READ_MESSAGE, "recv buffer is empty");
        // A custom error's Display is exactly its message, which is what
        // `is_transient` compares.
        assert_eq!(feth_empty_read().to_string(), "recv buffer is empty");
        assert_eq!(close().to_string(), "close");
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn eintr_matches_libc() {
        assert_eq!(EINTR, libc::EINTR);
    }

    #[test]
    fn raw_eintr_is_retried_on_linux_and_macos_only() {
        for os in [HostOs::Linux, HostOs::Macos] {
            assert!(is_transient(os, &eintr()), "{os:?}");
            assert!(recv_result(os, 64, Err(eintr())).is_none(), "{os:?}");
            assert!(matches!(send_step(os, Err(eintr())), Step::Retry), "{os:?}");
        }
        for os in [HostOs::Windows, HostOs::Other] {
            assert!(!is_transient(os, &eintr()), "{os:?}");
        }
    }

    /// The code-less `Interrupted "cancel"` is a deliberate cancellation,
    /// not a signal: never retried, and mapped through `io_error`.
    #[test]
    fn code_less_interrupted_is_not_retried() {
        for os in ALL_OSES {
            assert!(!is_transient(os, &cancel()), "{os:?}");
            assert!(
                matches!(
                    recv_result(os, 64, Err(cancel())),
                    Some(Err(Error::Platform(PlatformErrorCode::Unknown)))
                ),
                "{os:?}"
            );
            assert!(matches!(
                send_step(os, Err(cancel())),
                Step::Done(Err(Error::Platform(PlatformErrorCode::Unknown)))
            ));
        }
    }

    #[test]
    fn feth_empty_read_is_retried_on_macos_only() {
        assert!(is_transient(HostOs::Macos, &feth_empty_read()));
        assert!(recv_result(HostOs::Macos, 64, Err(feth_empty_read())).is_none());
        for os in [HostOs::Linux, HostOs::Windows, HostOs::Other] {
            assert!(
                matches!(
                    recv_result(os, 64, Err(feth_empty_read())),
                    Some(Err(Error::Disconnected))
                ),
                "{os:?}"
            );
        }
    }

    /// Genuine end-of-life stays `Disconnected`: the shutdown pipe's
    /// `"close"`, Wintun's `ERROR_HANDLE_EOF` (a bare `UnexpectedEof`), and
    /// any other `UnexpectedEof` message.
    #[test]
    fn every_other_unexpected_eof_stays_disconnected() {
        for os in ALL_OSES {
            for err in [
                close(),
                io::Error::from(io::ErrorKind::UnexpectedEof),
                io::Error::new(io::ErrorKind::UnexpectedEof, "recv buffer is empty!"),
            ] {
                assert!(!is_transient(os, &err), "{os:?} {err}");
                assert!(
                    matches!(
                        recv_result(os, 64, Err(err)),
                        Some(Err(Error::Disconnected))
                    ),
                    "{os:?}"
                );
            }
        }
    }

    #[test]
    fn a_length_past_the_buffer_is_buffer_too_small() {
        for os in ALL_OSES {
            assert!(matches!(recv_result(os, 64, Ok(64)), Some(Ok(64))));
            assert!(matches!(recv_result(os, 64, Ok(0)), Some(Ok(0))));
            assert!(matches!(
                recv_result(os, 64, Ok(65)),
                Some(Err(Error::BufferTooSmall))
            ));
            assert!(matches!(
                recv_result(os, 0, Ok(1)),
                Some(Err(Error::BufferTooSmall))
            ));
        }
    }

    #[test]
    fn feth_invalid_data_is_buffer_too_small_on_macos_only() {
        let err = || io::Error::new(io::ErrorKind::InvalidData, "buffer too small");
        assert!(matches!(
            recv_result(HostOs::Macos, 64, Err(err())),
            Some(Err(Error::BufferTooSmall))
        ));
        for os in [HostOs::Linux, HostOs::Windows, HostOs::Other] {
            assert!(
                matches!(
                    recv_result(os, 64, Err(err())),
                    Some(Err(Error::Platform(_)))
                ),
                "{os:?}"
            );
        }
    }

    #[test]
    fn windows_invalid_input_is_buffer_too_small_on_windows_recv_only() {
        for message in ["destination buffer too small", "receive buffer too small"] {
            let err = || io::Error::new(io::ErrorKind::InvalidInput, message);
            assert!(matches!(
                recv_result(HostOs::Windows, 64, Err(err())),
                Some(Err(Error::BufferTooSmall))
            ));
            // On send, `InvalidInput` means an oversize packet to write,
            // not a receive buffer: it is not remapped.
            assert!(matches!(
                send_step(HostOs::Windows, Err(err())),
                Step::Done(Err(Error::Platform(PlatformErrorCode::Unknown)))
            ));
            for os in [HostOs::Linux, HostOs::Macos, HostOs::Other] {
                assert!(
                    matches!(
                        recv_result(os, 64, Err(err())),
                        Some(Err(Error::Platform(_)))
                    ),
                    "{os:?}"
                );
            }
        }
    }

    /// A native error that carries an OS code is never taken for the
    /// code-less too-small signals (e.g. Windows `ERROR_INVALID_PARAMETER`,
    /// which `std` also classifies as `InvalidInput`).
    #[test]
    fn an_os_coded_error_is_not_buffer_too_small() {
        const ERROR_INVALID_PARAMETER: i32 = 87;
        let err = io::Error::from_raw_os_error(ERROR_INVALID_PARAMETER);
        let result = recv_result(HostOs::Windows, 64, Err(err));
        assert!(
            !matches!(result, Some(Err(Error::BufferTooSmall))),
            "{result:?}"
        );
    }

    /// A scripted native read: pops one result per call and writes
    /// `payload` into the buffer on success.
    struct Script {
        results: RefCell<VecDeque<io::Result<usize>>>,
        payload: &'static [u8],
        calls: RefCell<usize>,
    }

    impl Script {
        fn new(results: Vec<io::Result<usize>>, payload: &'static [u8]) -> Self {
            Self {
                results: RefCell::new(results.into()),
                payload,
                calls: RefCell::new(0),
            }
        }

        fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
            *self.calls.borrow_mut() += 1;
            let result = self
                .results
                .borrow_mut()
                .pop_front()
                .expect("the loop called the native read too often");
            if let Ok(n) = result {
                let written = n.min(buf.len()).min(self.payload.len());
                buf[..written].copy_from_slice(&self.payload[..written]);
            }
            result
        }

        fn calls(&self) -> usize {
            *self.calls.borrow()
        }
    }

    /// Async view of a [`Script`]: each attempt does its work up front and
    /// returns an already-resolved `Send` future; the loop under test is
    /// what re-awaits it.
    struct AsyncScript(std::sync::Mutex<Script>);

    impl AsyncRecvSource for AsyncScript {
        fn recv_native(&self, buf: &mut [u8]) -> impl Future<Output = io::Result<usize>> + Send {
            let result = self.0.lock().unwrap().read(buf);
            async move { result }
        }
    }

    #[test]
    fn blocking_recv_retries_eintr_and_the_caller_sees_only_the_data() {
        let script = Script::new(vec![Err(eintr()), Ok(3)], b"abc");
        let mut buf = [0u8; 8];
        let n = recv_blocking(HostOs::Linux, &mut buf, |buf| script.read(buf)).unwrap();
        assert_eq!(&buf[..n], b"abc");
        assert_eq!(script.calls(), 2);
    }

    #[test]
    fn blocking_recv_retries_the_feth_empty_read_then_reports_close() {
        let script = Script::new(
            vec![Err(feth_empty_read()), Err(eintr()), Err(close())],
            b"",
        );
        let mut buf = [0u8; 8];
        let result = recv_blocking(HostOs::Macos, &mut buf, |buf| script.read(buf));
        assert!(matches!(result, Err(Error::Disconnected)), "{result:?}");
        assert_eq!(script.calls(), 3);
    }

    #[test]
    fn blocking_recv_does_not_retry_code_less_interrupted() {
        let script = Script::new(vec![Err(cancel()), Ok(3)], b"abc");
        let mut buf = [0u8; 8];
        let result = recv_blocking(HostOs::Macos, &mut buf, |buf| script.read(buf));
        assert!(matches!(
            result,
            Err(Error::Platform(PlatformErrorCode::Unknown))
        ));
        assert_eq!(script.calls(), 1);
    }

    /// An oversize packet is reported once and the next call receives the
    /// following packet normally.
    #[test]
    fn blocking_recv_reports_an_oversize_packet_and_stays_usable() {
        let script = Script::new(vec![Ok(9), Ok(3)], b"abcdefghi");
        let mut buf = [0u8; 8];
        let first = recv_blocking(HostOs::Linux, &mut buf, |buf| script.read(buf));
        assert!(matches!(first, Err(Error::BufferTooSmall)));
        let second = recv_blocking(HostOs::Linux, &mut buf, |buf| script.read(buf));
        assert_eq!(second.unwrap(), 3);
    }

    #[test]
    fn blocking_send_retries_eintr_only() {
        let script = Script::new(vec![Err(eintr()), Ok(5)], b"");
        let n = send_blocking(HostOs::Linux, || script.read(&mut [])).unwrap();
        assert_eq!(n, 5);
        assert_eq!(script.calls(), 2);

        let script = Script::new(vec![Err(cancel()), Ok(5)], b"");
        assert!(send_blocking(HostOs::Linux, || script.read(&mut [])).is_err());
        assert_eq!(script.calls(), 1);
    }

    #[test]
    fn async_recv_re_awaits_eintr_and_the_caller_sees_only_the_data() {
        let source = AsyncScript(std::sync::Mutex::new(Script::new(
            vec![Err(eintr()), Err(feth_empty_read()), Ok(3)],
            b"abc",
        )));
        let mut buf = [0u8; 8];
        let n = ready(recv_async(HostOs::Macos, &source, &mut buf)).unwrap();
        assert_eq!(&buf[..n], b"abc");
        assert_eq!(source.0.lock().unwrap().calls(), 3);
    }

    #[test]
    fn async_recv_reports_close_and_code_less_interrupted_without_retrying() {
        for (err, expected_disconnected) in [(close(), true), (cancel(), false)] {
            let source = AsyncScript(std::sync::Mutex::new(Script::new(
                vec![Err(err), Ok(3)],
                b"abc",
            )));
            let mut buf = [0u8; 8];
            let result = ready(recv_async(HostOs::Linux, &source, &mut buf));
            assert_eq!(
                matches!(result, Err(Error::Disconnected)),
                expected_disconnected,
                "{result:?}"
            );
            assert!(result.is_err());
            assert_eq!(source.0.lock().unwrap().calls(), 1);
        }
    }

    #[test]
    fn async_recv_reports_an_oversize_packet_and_stays_usable() {
        let source = AsyncScript(std::sync::Mutex::new(Script::new(
            vec![Ok(9), Ok(3)],
            b"abcdefghi",
        )));
        let mut buf = [0u8; 8];
        let first = ready(recv_async(HostOs::Linux, &source, &mut buf));
        assert!(matches!(first, Err(Error::BufferTooSmall)));
        let second = ready(recv_async(HostOs::Linux, &source, &mut buf));
        assert_eq!(second.unwrap(), 3);
    }

    #[test]
    fn async_send_retries_eintr() {
        let script = Script::new(vec![Err(eintr()), Ok(5)], b"");
        let n = ready(send_async(HostOs::Macos, || {
            let result = script.read(&mut []);
            async move { result }
        }))
        .unwrap();
        assert_eq!(n, 5);
        assert_eq!(script.calls(), 2);
    }
}
