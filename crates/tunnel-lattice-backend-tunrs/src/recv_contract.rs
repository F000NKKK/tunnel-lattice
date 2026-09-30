//! The `recv`/`send` error contract: the transient conditions retried
//! inside the backend, the too-small-buffer normalization, and the
//! device-lifecycle errors, applied in front of the generic [`io_error`]
//! mapping.
//!
//! Like [`crate::open_contract`], the rules take an explicit [`HostOs`]
//! instead of `#[cfg]` blocks, so every per-OS rule is exercised by the
//! ordinary unit tests on every host. Rules on OS error codes compare the
//! raw code only, never `kind()` of a raw-coded error: `std` decodes a raw
//! code with the *running* host's table (code 6 is `ENXIO` on unix but
//! `ERROR_INVALID_HANDLE` on Windows), so a kind check would make a rule
//! behave differently depending on where the tests run.
//!
//! | OS | Native signal | Result |
//! |---|---|---|
//! | Linux, macOS | raw `EINTR` (4) | retried (re-enters the blocking read or readiness wait) |
//! | macOS | code-less `UnexpectedEof`, message exactly `"recv buffer is empty"` (feth TAP: a BPF read with no complete frame) | retried |
//! | Linux, macOS (recv) | `Ok(n)` with `n > buf.len()` from the sentinel read | [`Error::BufferTooSmall`] |
//! | macOS (recv) | code-less `InvalidData` (feth TAP: frame larger than the buffer) | [`Error::BufferTooSmall`] |
//! | Windows (recv) | code-less `InvalidInput` (wintun / tap-windows: packet larger than the buffer) | [`Error::BufferTooSmall`] |
//! | Linux, macOS | raw `ENXIO` (6): on macOS the feth TAP's BPF descriptor after the interface is destroyed; on Linux listed for symmetry only (`tun.c` returns it only on its XDP transmit path) | [`Error::Disconnected`] |
//! | Linux | raw `EBADFD` (77): the tun file was detached because the device was deleted | [`Error::Disconnected`] |
//! | Linux (recv) | raw `EFAULT` (14): a read already blocked when the device was deleted | [`Error::Disconnected`] |
//! | Windows (send) | code-less `WriteZero` (Wintun `ERROR_HANDLE_EOF`: the adapter is terminating) | [`Error::Disconnected`] |
//! | Windows | code-less `Other`, message exactly `"The interface has been disabled"` (Wintun session ended by `apply(Down)`; `apply(Up)` recovers it) | [`Error::InvalidState`] |
//! | anything else | | [`io_error`], so every other `UnexpectedEof` (the shutdown pipe's `"close"`, Wintun's `ERROR_HANDLE_EOF` on receive) stays [`Error::Disconnected`] and a code-less `Interrupted` (`"cancel"`) is not retried |
//!
//! `WriteZero` is remapped on `send` only, so the same kind from any other
//! operation keeps its generic meaning.
//!
//! Linux TUN/TAP and macOS utun truncate an oversize packet silently, so on
//! unix the backend reads into `[buf, 1-byte sentinel]` with `readv`: a
//! packet that does not fit in `buf` spills into the sentinel, and the
//! reported length exceeds `buf.len()`. The packet is then already consumed
//! (discarded); only its first `buf.len()` bytes were written.
//!
//! The matched messages and the error kinds come from `tun-rs` 2.8.11, the
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

/// `tun-rs` 2.8.11 `platform/windows/tun/mod.rs` (`State::check`, the
/// `WinTunAdapter` receive/send methods once the session is gone, and both
/// Wintun readiness waits when the adapter's shutdown event fires): the
/// message of the code-less `io::Error::other` returned after the Wintun
/// adapter was disabled (`enabled(false)`, which `apply(Down)` calls).
/// Recoverable with `apply(Up)`: mapped to [`Error::InvalidState`].
pub(crate) const WINDOWS_TUN_DISABLED_MESSAGE: &str = "The interface has been disabled";

/// `EINTR`, identical on Linux and macOS (asserted against `libc` by the
/// unit tests on those targets). Written out so the rules compile, and are
/// tested, on every host.
const EINTR: i32 = 4;

/// `ENXIO`, identical on Linux and macOS (asserted against `libc` by the
/// unit tests on those targets).
const ENXIO: i32 = 6;

/// Linux `EBADFD` (asserted against `libc` by the unit tests on Linux).
/// macOS has no `EBADFD`; its code 77 is `ENOLCK`, so this rule is
/// Linux-only.
const EBADFD: i32 = 77;

/// Linux `EFAULT` (asserted against `libc` by the unit tests on Linux).
/// Mapped on Linux `recv` only; see [`recv_error`].
const EFAULT: i32 = 14;

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
        Err(err) => Step::Done(Err(send_error(os, err))),
    }
}

/// Maps a non-transient `recv` error: the too-small-buffer signals first,
/// then the Linux receive-only `EFAULT` rule, then the device-lifecycle
/// rules, then [`io_error`].
fn recv_error(os: HostOs, err: io::Error) -> Error {
    let too_small = err.raw_os_error().is_none()
        && match os {
            HostOs::Macos => err.kind() == io::ErrorKind::InvalidData,
            HostOs::Windows => err.kind() == io::ErrorKind::InvalidInput,
            HostOs::Linux | HostOs::Other => false,
        };
    if too_small {
        return Error::BufferTooSmall;
    }
    // Linux `tun.c` fails a read that is already blocked when the device is
    // deleted with `EFAULT` (the socket's `RCV_SHUTDOWN`, which only the
    // device teardown sets on an attached file); every later read returns
    // `EBADFD`. The read path's only other `EFAULT` sources are user-copy
    // faults, which a safe `&mut [u8]` cannot cause: the packet-information
    // header and the virtio-net header, neither of which this backend
    // enables. Re-examine this rule if packet information or offload is
    // ever enabled. Matched on the raw code only, never on `kind()`: `std`
    // decodes the code with the running host's table. Not applied to
    // `send` (an `EFAULT` there is only a copy fault; teardown is
    // `EBADFD`) nor to macOS (a BPF `EFAULT` is a `copyout` fault).
    if os == HostOs::Linux && err.raw_os_error() == Some(EFAULT) {
        return Error::Disconnected;
    }
    lifecycle_error(os, &err).unwrap_or_else(|| io_error(err))
}

/// Maps a non-transient `send` error: Wintun's send-side end-of-life
/// signal, then the device-lifecycle rules shared with `recv`, then
/// [`io_error`].
fn send_error(os: HostOs, err: io::Error) -> Error {
    // `tun-rs` reports Wintun `ERROR_HANDLE_EOF` as `WriteZero` on send
    // (and as `UnexpectedEof`, already `Disconnected`, on receive).
    let wintun_send_eof = os == HostOs::Windows
        && err.raw_os_error().is_none()
        && err.kind() == io::ErrorKind::WriteZero;
    if wintun_send_eof {
        return Error::Disconnected;
    }
    lifecycle_error(os, &err).unwrap_or_else(|| io_error(err))
}

/// The device-lifecycle rules shared by `recv` and `send` (see the module
/// table), or `None` for an error [`io_error`] maps.
fn lifecycle_error(os: HostOs, err: &io::Error) -> Option<Error> {
    let unix = matches!(os, HostOs::Linux | HostOs::Macos);
    match err.raw_os_error() {
        Some(ENXIO) if unix => Some(Error::Disconnected),
        Some(EBADFD) if os == HostOs::Linux => Some(Error::Disconnected),
        Some(_) => None,
        None => (os == HostOs::Windows
            && err.kind() == io::ErrorKind::Other
            && err.to_string() == WINDOWS_TUN_DISABLED_MESSAGE)
            .then_some(Error::InvalidState),
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
    fn eintr_and_enxio_match_libc() {
        assert_eq!(EINTR, libc::EINTR);
        assert_eq!(ENXIO, libc::ENXIO);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn ebadfd_matches_libc_on_linux() {
        assert_eq!(EBADFD, libc::EBADFD);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn efault_matches_libc_on_linux() {
        assert_eq!(EFAULT, libc::EFAULT);
    }

    /// A read blocked across the Linux device deletion fails with raw
    /// `EFAULT`: `Disconnected` on Linux `recv` only. Everywhere else the
    /// same raw code falls through to the generic mapping, which never
    /// yields `Disconnected` for it on any host (code 14 decodes as
    /// `Uncategorized` on unix and as `OutOfMemory` on Windows).
    #[test]
    fn raw_efault_is_disconnected_on_linux_recv_only() {
        let efault = || io::Error::from_raw_os_error(EFAULT);
        assert!(matches!(
            recv_err(HostOs::Linux, efault()),
            Err(Error::Disconnected)
        ));
        let send = send_result(HostOs::Linux, efault());
        assert!(!matches!(send, Err(Error::Disconnected)), "{send:?}");
        for os in [HostOs::Macos, HostOs::Windows, HostOs::Other] {
            let recv = recv_err(os, efault());
            assert!(!matches!(recv, Err(Error::Disconnected)), "{os:?} {recv:?}");
        }
        let send = send_result(HostOs::Macos, efault());
        assert!(!matches!(send, Err(Error::Disconnected)), "{send:?}");
    }

    /// Pins the Wintun "disabled" message to `tun-rs` 2.8.11.
    #[test]
    fn pinned_tun_rs_2_8_11_lifecycle_messages() {
        assert_eq!(
            WINDOWS_TUN_DISABLED_MESSAGE,
            "The interface has been disabled"
        );
        assert_eq!(
            wintun_disabled().to_string(),
            "The interface has been disabled"
        );
    }

    fn wintun_disabled() -> io::Error {
        io::Error::other(WINDOWS_TUN_DISABLED_MESSAGE)
    }

    fn send_result(os: HostOs, err: io::Error) -> Result<usize> {
        match send_step(os, Err(err)) {
            Step::Retry => panic!("{os:?}: a lifecycle error must not be retried"),
            Step::Done(result) => result,
        }
    }

    fn recv_err(os: HostOs, err: io::Error) -> Result<usize> {
        recv_result(os, 64, Err(err)).expect("a lifecycle error must not be retried")
    }

    /// The device-lifecycle table, on every host: raw `ENXIO` on Linux and
    /// macOS and raw `EBADFD` on Linux are `Disconnected` on both `recv`
    /// and `send`; the same raw codes elsewhere (code 6 is Windows
    /// `ERROR_INVALID_HANDLE`, code 77 is macOS `ENOLCK`) fall through to
    /// the generic mapping.
    #[test]
    fn raw_enxio_and_ebadfd_are_disconnected_only_where_they_mean_it() {
        let enxio = || io::Error::from_raw_os_error(ENXIO);
        let ebadfd = || io::Error::from_raw_os_error(EBADFD);
        for os in [HostOs::Linux, HostOs::Macos] {
            assert!(matches!(recv_err(os, enxio()), Err(Error::Disconnected)));
            assert!(matches!(send_result(os, enxio()), Err(Error::Disconnected)));
        }
        assert!(matches!(
            recv_err(HostOs::Linux, ebadfd()),
            Err(Error::Disconnected)
        ));
        assert!(matches!(
            send_result(HostOs::Linux, ebadfd()),
            Err(Error::Disconnected)
        ));
        for (os, err) in [
            (HostOs::Windows, enxio()),
            (HostOs::Other, enxio()),
            (HostOs::Macos, ebadfd()),
            (HostOs::Windows, ebadfd()),
            (HostOs::Other, ebadfd()),
        ] {
            let mapped = lifecycle_error(os, &err);
            assert!(mapped.is_none(), "{os:?} {err}: {mapped:?}");
        }
    }

    /// Wintun's send-side `ERROR_HANDLE_EOF` (`WriteZero`) is
    /// `Disconnected` on Windows `send` only: not on `recv`, not on other
    /// hosts, and not when it carries an OS code.
    #[test]
    fn code_less_write_zero_is_disconnected_on_windows_send_only() {
        let write_zero = || io::Error::from(io::ErrorKind::WriteZero);
        assert!(matches!(
            send_result(HostOs::Windows, write_zero()),
            Err(Error::Disconnected)
        ));
        assert!(matches!(
            recv_err(HostOs::Windows, write_zero()),
            Err(Error::Platform(PlatformErrorCode::Unknown))
        ));
        for os in [HostOs::Linux, HostOs::Macos, HostOs::Other] {
            assert!(
                matches!(
                    send_result(os, write_zero()),
                    Err(Error::Platform(PlatformErrorCode::Unknown))
                ),
                "{os:?}"
            );
        }
        // Wintun's receive-side `ERROR_HANDLE_EOF` is a bare
        // `UnexpectedEof`, which is `Disconnected` on both directions.
        let eof = || io::Error::from(io::ErrorKind::UnexpectedEof);
        assert!(matches!(
            recv_err(HostOs::Windows, eof()),
            Err(Error::Disconnected)
        ));
        assert!(matches!(
            send_result(HostOs::Windows, eof()),
            Err(Error::Disconnected)
        ));
    }

    /// The Wintun "disabled" error is `InvalidState` (recoverable, not
    /// `Disconnected`) on both directions, on Windows only, and only for
    /// the exact code-less message.
    #[test]
    fn wintun_disabled_is_invalid_state_on_windows_only() {
        assert!(matches!(
            recv_err(HostOs::Windows, wintun_disabled()),
            Err(Error::InvalidState)
        ));
        assert!(matches!(
            send_result(HostOs::Windows, wintun_disabled()),
            Err(Error::InvalidState)
        ));
        for os in [HostOs::Linux, HostOs::Macos, HostOs::Other] {
            assert!(
                matches!(
                    recv_err(os, wintun_disabled()),
                    Err(Error::Platform(PlatformErrorCode::Unknown))
                ),
                "{os:?}"
            );
        }
        for near_miss in [
            io::Error::other("The interface has been disabled."),
            io::Error::other("the interface has been disabled"),
            io::Error::new(io::ErrorKind::TimedOut, WINDOWS_TUN_DISABLED_MESSAGE),
        ] {
            assert!(
                matches!(
                    recv_err(HostOs::Windows, near_miss),
                    Err(Error::Platform(PlatformErrorCode::Unknown))
                ),
                "only the exact code-less `Other` message is remapped"
            );
        }
    }

    /// The lifecycle errors are final: the blocking and async loops report
    /// them after one native call instead of retrying.
    #[test]
    fn lifecycle_errors_end_the_loops_after_one_call() {
        let script = Script::new(
            vec![Err(io::Error::from_raw_os_error(EBADFD)), Ok(3)],
            b"abc",
        );
        let mut buf = [0u8; 8];
        let result = recv_blocking(HostOs::Linux, &mut buf, |buf| script.read(buf));
        assert!(matches!(result, Err(Error::Disconnected)), "{result:?}");
        assert_eq!(script.calls(), 1);

        let source = AsyncScript(std::sync::Mutex::new(Script::new(
            vec![Err(wintun_disabled()), Ok(3)],
            b"abc",
        )));
        let result = ready(recv_async(HostOs::Windows, &source, &mut buf));
        assert!(matches!(result, Err(Error::InvalidState)), "{result:?}");
        assert_eq!(source.0.lock().unwrap().calls(), 1);

        let script = Script::new(
            vec![Err(io::Error::from(io::ErrorKind::WriteZero)), Ok(5)],
            b"",
        );
        let result = send_blocking(HostOs::Windows, || script.read(&mut []));
        assert!(matches!(result, Err(Error::Disconnected)), "{result:?}");
        assert_eq!(script.calls(), 1);

        let script = Script::new(vec![Err(io::Error::from_raw_os_error(ENXIO)), Ok(5)], b"");
        let result = ready(send_async(HostOs::Macos, || {
            let result = script.read(&mut []);
            async move { result }
        }));
        assert!(matches!(result, Err(Error::Disconnected)), "{result:?}");
        assert_eq!(script.calls(), 1);
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
