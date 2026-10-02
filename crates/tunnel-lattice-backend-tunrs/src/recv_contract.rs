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
//! | Linux (send) | raw `EIO` (5): the device is administratively down (`apply(Up)` recovers it) | [`Error::InvalidState`] |
//! | Linux, offload-framed TUN queue only (recv) | raw `EINVAL` (22): the kernel could not put a packet behind a virtio-net header (an unexpected GSO type) and has already freed it | retried ([`offload_recv_error_step`]) |
//! | Linux, offload-framed TUN queue only (recv) | a frame that fails the virtio-net header and super-packet validation, or that filled the staging sentinel (longer than 64 KiB) | dropped, and the read retried (see `offload_queue`) |
//! | Windows, TAP only (recv) | the first raw 995 (`ERROR_OPERATION_ABORTED`) in a call while the adapter's operational status reads `Up`: a read cancelled because the thread that issued it exited, on a healthy adapter | retried once ([`tap_abort_retries`]) |
//! | Windows, TAP only | raw 995 (`ERROR_OPERATION_ABORTED`) otherwise (on `recv`: the status is not `Up`, the status read fails, or the call already retried): a call made while the tap-windows adapter's media is disconnected, which `apply(Down)` does (`apply(Up)` recovers it; a read already waiting is not ended by it), or the adapter was disabled outside this crate | [`Error::InvalidState`] |
//! | anything else | | [`io_error`], so every other `UnexpectedEof` (the shutdown pipe's `"close"`, Wintun's `ERROR_HANDLE_EOF` on receive) stays [`Error::Disconnected`] and a code-less `Interrupted` (`"cancel"`) is not retried |
//!
//! `WriteZero` and `EIO` are remapped on `send` only, so the same signal
//! from any other operation keeps its generic meaning. The `EINVAL` retry
//! applies to a read on an offload-framed queue only: on a plain queue, and
//! on every `send`, `EINVAL` keeps its generic meaning. Neither offload
//! retry ever spins: each one is a packet the kernel produced and the read
//! consumed, and the next read waits as usual. The Windows 995 rule
//! takes the [`DeviceKind`] as well: it applies to a TAP handle only, on
//! both directions, and a Wintun (TUN) 995 keeps its generic meaning.
//!
//! The Windows TAP `recv` retry exists because a healthy adapter also
//! reports 995: an async `recv` issues its overlapped read on the thread
//! that polls it, and if that future is dropped while the read is pending
//! and the thread then exits, Windows cancels the read, and the next
//! `recv` collects the 995. While the adapter reads `Up`, the first 995 in
//! a call is retried with a fresh read, which waits as usual, so the
//! caller sees nothing. The status is read at most once per call, and a
//! call retries at most once, so it never spins. One case remains: a
//! single `recv` call that meets two reads cancelled this way (for
//! example, it collects one left by an earlier dropped `recv`, and then a
//! thread that polled it exits while it is still pending) reports
//! [`Error::InvalidState`] once while the adapter is up, and the next call
//! works. `send` has no such retry: `tun-rs` discards a cancelled pending
//! write, so a `send` 995 always comes from the driver refusing a fresh
//! write.
//!
//! [`Error::InvalidState`] from `recv`/`send` means the device exists but
//! is not passing packets because it is down or disabled. When applying
//! `DesiredAdminState::Down` caused it, applying `DesiredAdminState::Up`
//! on the same handle recovers it. A down Linux device does not fail
//! `recv` at all: the read waits until the device is up and traffic
//! arrives. On Windows TAP the media disconnect fails only the calls made
//! while it lasts: a `recv` whose read was already waiting when
//! `apply(Down)` ran is not ended by it, nor by a later `apply(Up)`, and
//! keeps waiting as on a down Linux device. Disabling the adapter outside
//! this API (`Disable-NetAdapter`) does end a waiting `recv`, with the
//! same raw 995 and so also with [`Error::InvalidState`], but only
//! re-enabling the adapter outside this API recovers that one.
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
use tunnel_lattice_model::{AdminState, DeviceKind};

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

/// Linux `EIO` (asserted against `libc` by the unit tests on Linux).
/// Mapped on Linux `send` only; see [`send_error`].
const EIO: i32 = 5;

/// Linux `EINVAL` (asserted against `libc` by the unit tests on Linux).
/// Retried on a read from an offload-framed queue only; see
/// [`offload_recv_error_step`].
const EINVAL: i32 = 22;

/// Windows `ERROR_OPERATION_ABORTED` (asserted against `windows-sys` by the
/// unit tests on Windows). Mapped for a TAP handle only; see
/// [`lifecycle_error`].
const ERROR_OPERATION_ABORTED: i32 = 995;

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

/// Whether a Windows TAP `recv` retries a raw 995 instead of reporting
/// [`Error::InvalidState`]: only the first 995 within one call (`retried`
/// is still `false`), and only when the adapter's operational status,
/// read by `oper`, is `Up`. A status that is not `Up`, or a failed read,
/// is not retried, so a vanished or disabled adapter never leads to a
/// retry. Once the call has retried, `oper` is not called again.
///
/// A healthy adapter reports 995 when a read was cancelled because the
/// thread that issued it exited (an async `recv` issues its read on the
/// thread that polls it); the retry issues a fresh read, which waits as
/// usual. A call that already retried reports its next 995, so a read
/// that fails again at once is reported instead of retried forever.
/// This is the path of a `recv` started in the few milliseconds after
/// `apply(Down)` returns, before the operational status reads `Down`: its
/// fresh read fails at once, is retried once, and the second 995 is
/// reported.
pub(crate) fn tap_abort_retries(
    retried: bool,
    oper: impl FnOnce() -> io::Result<AdminState>,
) -> bool {
    !retried && matches!(oper(), Ok(AdminState::Up))
}

/// Maps one native `recv` attempt on a `kind` handle that read into a
/// `buf_len`-byte buffer. `retried` is the call's state for the Windows
/// TAP 995 retry (see [`tap_abort_retries`], which `oper` feeds); it is
/// `false` at the start of each call and set when that retry is taken.
pub(crate) fn recv_step(
    os: HostOs,
    kind: DeviceKind,
    buf_len: usize,
    result: io::Result<usize>,
    retried: &mut bool,
    oper: impl FnOnce() -> io::Result<AdminState>,
) -> Step {
    if let Err(err) = &result
        && os == HostOs::Windows
        && kind == DeviceKind::Tap
        && err.raw_os_error() == Some(ERROR_OPERATION_ABORTED)
        && tap_abort_retries(*retried, oper)
    {
        *retried = true;
        return Step::Retry;
    }
    match result {
        Ok(n) if n > buf_len => Step::Done(Err(Error::BufferTooSmall)),
        Ok(n) => Step::Done(Ok(n)),
        Err(err) if is_transient(os, &err) => Step::Retry,
        Err(err) => Step::Done(Err(recv_error(os, kind, err))),
    }
}

/// Maps one failed native read on an offload-framed Linux TUN queue (a
/// queue that reads through a virtio-net header into the backend's own
/// staging buffer).
///
/// A raw `EINVAL` there means the kernel's `tun_put_user` could not
/// express one packet behind the header (an unexpected GSO type) or was
/// handed a buffer shorter than the header, which the staging buffer never
/// is. Either way the kernel has already freed that packet, so the read is
/// retried and the packet is lost, like a malformed super-packet. Every
/// other error goes through the ordinary `recv` rules ([`recv_step`]); a
/// read into the staging buffer can never be too small for a packet, so
/// [`Error::BufferTooSmall`] does not come from here.
pub(crate) fn offload_recv_error_step(os: HostOs, kind: DeviceKind, err: io::Error) -> Step {
    if os == HostOs::Linux && err.raw_os_error() == Some(EINVAL) {
        return Step::Retry;
    }
    recv_step(os, kind, usize::MAX, Err(err), &mut false, || {
        Ok(AdminState::Unknown)
    })
}

/// Maps one native `send` attempt on a `kind` handle.
pub(crate) fn send_step(os: HostOs, kind: DeviceKind, result: io::Result<usize>) -> Step {
    match result {
        Ok(n) => Step::Done(Ok(n)),
        Err(err) if is_transient(os, &err) => Step::Retry,
        Err(err) => Step::Done(Err(send_error(os, kind, err))),
    }
}

/// Maps a non-transient `recv` error: the too-small-buffer signals first,
/// then the Linux receive-only `EFAULT` rule, then the device-lifecycle
/// rules, then [`io_error`].
fn recv_error(os: HostOs, kind: DeviceKind, err: io::Error) -> Error {
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
    // faults, which a safe `&mut [u8]` cannot cause. That still holds on an
    // offload-framed queue, which reads the virtio-net header and the
    // packet into the backend's own staging buffer, always valid memory of
    // the full size, so an `EFAULT` there is the teardown too. Packet
    // information is never enabled; re-examine this rule if it ever is.
    // Matched on the raw code only, never on `kind()`: `std`
    // decodes the code with the running host's table. Not applied to
    // `send` (an `EFAULT` there is only a copy fault; teardown is
    // `EBADFD`) nor to macOS (a BPF `EFAULT` is a `copyout` fault).
    if os == HostOs::Linux && err.raw_os_error() == Some(EFAULT) {
        return Error::Disconnected;
    }
    lifecycle_error(os, kind, &err).unwrap_or_else(|| io_error(err))
}

/// Maps a non-transient `send` error: Wintun's send-side end-of-life
/// signal, then the Linux send-only `EIO` rule, then the device-lifecycle
/// rules shared with `recv`, then [`io_error`].
fn send_error(os: HostOs, kind: DeviceKind, err: io::Error) -> Error {
    // `tun-rs` reports Wintun `ERROR_HANDLE_EOF` as `WriteZero` on send
    // (and as `UnexpectedEof`, already `Disconnected`, on receive).
    let wintun_send_eof = os == HostOs::Windows
        && err.raw_os_error().is_none()
        && err.kind() == io::ErrorKind::WriteZero;
    if wintun_send_eof {
        return Error::Disconnected;
    }
    // Linux `tun.c` has exactly one `EIO` on its write path: `tun_get_user`
    // refuses a packet while the device is administratively down (the
    // `!(dev->flags & IFF_UP)` check, `tun.c:1895` in Linux 7.0). The check
    // is on the common path, so TUN and TAP behave alike, and applying `Up`
    // on the same handle makes it send again: `InvalidState`, like a
    // disabled Wintun adapter. It holds on an offload-framed queue too: the
    // kernel rejects a virtio-net header it cannot accept with `EINVAL`,
    // never `EIO`. Re-examine this rule if napi frags or an XDP program is
    // ever enabled, or a kernel adds another `EIO` to that path. Matched on the raw code only; never on
    // `recv` (the read path has no `EIO`: a down device makes `recv` wait)
    // nor on macOS (where `EIO` has unrelated meanings).
    if os == HostOs::Linux && err.raw_os_error() == Some(EIO) {
        return Error::InvalidState;
    }
    lifecycle_error(os, kind, &err).unwrap_or_else(|| io_error(err))
}

/// The device-lifecycle rules shared by `recv` and `send` on a `kind`
/// handle (see the module table), or `None` for an error [`io_error`]
/// maps.
fn lifecycle_error(os: HostOs, kind: DeviceKind, err: &io::Error) -> Option<Error> {
    let unix = matches!(os, HostOs::Linux | HostOs::Macos);
    // tap-windows fails every read and write *issued* while the adapter's
    // media is disconnected with `ERROR_OPERATION_ABORTED`; a read already
    // pending when the media is disconnected is not completed and keeps
    // waiting (the Windows privileged tests check it is still waiting 10 s
    // after `apply(Down)`), so `apply(Down)` never brings it here, while
    // disabling the adapter completes it with 995. `apply(Down)` (`tun-rs`
    // `set_status(false)`, the `TAP_IOCTL_SET_MEDIA_STATUS` ioctl)
    // disconnects the media, and
    // `apply(Up)` on the same handle makes both directions work again
    // (checked by the Windows privileged tests on every feature set). An
    // adapter disabled outside this API fails the same way, so it reads as
    // `InvalidState` too. Wintun never reports this code for a disabled
    // adapter (it uses `WINDOWS_TUN_DISABLED_MESSAGE`), so the rule is
    // limited to TAP. A healthy adapter also reports 995 for a read that
    // was cancelled because the thread that issued it exited; on `recv`
    // that case never gets here while the adapter reads `Up`, because
    // `recv_step` retries it first (`tap_abort_retries`). On `send` a 995
    // is never such a cancellation: `tun-rs` 2.8.11 logs and discards a
    // cancelled pending write (`platform/windows/tap/overlapped.rs`, the
    // `finish_pending_*` path), so a `send` 995 comes from a fresh write
    // the driver refused.
    let tap_media_down = os == HostOs::Windows && kind == DeviceKind::Tap;
    match err.raw_os_error() {
        Some(ENXIO) if unix => Some(Error::Disconnected),
        Some(EBADFD) if os == HostOs::Linux => Some(Error::Disconnected),
        Some(ERROR_OPERATION_ABORTED) if tap_media_down => Some(Error::InvalidState),
        Some(_) => None,
        None => (os == HostOs::Windows
            && err.kind() == io::ErrorKind::Other
            && err.to_string() == WINDOWS_TUN_DISABLED_MESSAGE)
            .then_some(Error::InvalidState),
    }
}

/// Blocking `recv`: calls `read(buf)` until it returns something other
/// than a transient error. `oper` reads the adapter's operational status
/// for the Windows TAP 995 retry (see [`tap_abort_retries`]); it is
/// called at most once per call, and never on other hosts or kinds.
#[cfg_attr(
    all(feature = "async", not(test)),
    expect(dead_code, reason = "async builds block on `recv_async` instead")
)]
pub(crate) fn recv_blocking<O>(
    os: HostOs,
    kind: DeviceKind,
    buf: &mut [u8],
    oper: &O,
    mut read: impl FnMut(&mut [u8]) -> io::Result<usize>,
) -> Result<usize>
where
    O: Fn() -> io::Result<AdminState>,
{
    let buf_len = buf.len();
    let mut retried = false;
    loop {
        if let Step::Done(result) = recv_step(os, kind, buf_len, read(buf), &mut retried, oper) {
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
    kind: DeviceKind,
    mut write: impl FnMut() -> io::Result<usize>,
) -> Result<usize> {
    loop {
        if let Step::Done(result) = send_step(os, kind, write()) {
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
/// readiness wait, so it never spins. `oper` is the status read of
/// [`recv_blocking`]; it is `Sync` so the returned future stays `Send`.
#[cfg_attr(
    all(not(feature = "async"), not(test)),
    expect(dead_code, reason = "only async builds hold an async handle")
)]
pub(crate) async fn recv_async<S, O>(
    os: HostOs,
    kind: DeviceKind,
    source: &S,
    oper: &O,
    buf: &mut [u8],
) -> Result<usize>
where
    S: AsyncRecvSource + ?Sized,
    O: Fn() -> io::Result<AdminState> + Sync,
{
    let buf_len = buf.len();
    let mut retried = false;
    loop {
        let result = source.recv_native(buf).await;
        if let Step::Done(result) = recv_step(os, kind, buf_len, result, &mut retried, oper) {
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
pub(crate) async fn send_async<F, Fut>(os: HostOs, kind: DeviceKind, mut write: F) -> Result<usize>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = io::Result<usize>>,
{
    loop {
        if let Step::Done(result) = send_step(os, kind, write().await) {
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

    /// The device kind every rule except the Windows TAP 995 rule ignores.
    const TUN: DeviceKind = DeviceKind::Tun;

    fn recv_result(os: HostOs, buf_len: usize, result: io::Result<usize>) -> Option<Result<usize>> {
        recv_result_on(os, TUN, buf_len, result)
    }

    /// One `recv_step` at the start of a call, with a status read that
    /// must not be called.
    fn recv_result_on(
        os: HostOs,
        kind: DeviceKind,
        buf_len: usize,
        result: io::Result<usize>,
    ) -> Option<Result<usize>> {
        match recv_step(os, kind, buf_len, result, &mut false, no_status) {
            Step::Retry => None,
            Step::Done(result) => Some(result),
        }
    }

    /// A status read for a path that must never read the status.
    fn no_status() -> io::Result<AdminState> {
        panic!("the status is read only for a Windows TAP 995 on recv")
    }

    fn aborted() -> io::Error {
        io::Error::from_raw_os_error(ERROR_OPERATION_ABORTED)
    }

    /// A scripted operational-status read that counts its calls.
    struct Status {
        result: fn() -> io::Result<AdminState>,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl Status {
        fn new(result: fn() -> io::Result<AdminState>) -> Self {
            Self {
                result,
                calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }

        fn read(&self) -> io::Result<AdminState> {
            self.calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            (self.result)()
        }

        fn calls(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::Relaxed)
        }
    }

    fn up() -> io::Result<AdminState> {
        Ok(AdminState::Up)
    }

    fn down() -> io::Result<AdminState> {
        Ok(AdminState::Down)
    }

    fn unknown() -> io::Result<AdminState> {
        Ok(AdminState::Unknown)
    }

    fn status_failed() -> io::Result<AdminState> {
        // `ERROR_NOT_FOUND`: `GetIfEntry2` on a vanished interface.
        Err(io::Error::from_raw_os_error(1168))
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

    #[test]
    #[cfg(target_os = "linux")]
    fn eio_matches_libc_on_linux() {
        assert_eq!(EIO, libc::EIO);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn einval_matches_libc_on_linux() {
        assert_eq!(EINVAL, libc::EINVAL);
    }

    fn offload_step(os: HostOs, err: io::Error) -> Option<Result<usize>> {
        match offload_recv_error_step(os, TUN, err) {
            Step::Retry => None,
            Step::Done(result) => Some(result),
        }
    }

    /// On an offload-framed Linux queue a raw `EINVAL` read is a packet the
    /// kernel already dropped: retried. Everywhere else it keeps its
    /// generic meaning, and the ordinary `recv` rules still apply.
    #[test]
    fn offload_read_retries_einval_on_linux_only() {
        let einval = || io::Error::from_raw_os_error(EINVAL);
        assert!(offload_step(HostOs::Linux, einval()).is_none());
        for os in [HostOs::Macos, HostOs::Windows, HostOs::Other] {
            let result = offload_step(os, einval());
            assert!(
                matches!(result, Some(Err(Error::Platform(_)))),
                "{os:?}: {result:?}"
            );
        }
        // The ordinary rules: EINTR is retried, teardown is Disconnected.
        assert!(offload_step(HostOs::Linux, eintr()).is_none());
        for code in [EFAULT, EBADFD] {
            let result = offload_step(HostOs::Linux, io::Error::from_raw_os_error(code));
            assert!(
                matches!(result, Some(Err(Error::Disconnected))),
                "{code}: {result:?}"
            );
        }
        // A plain queue's EINVAL read is not retried.
        assert!(matches!(
            recv_result(HostOs::Linux, 64, Err(einval())),
            Some(Err(Error::Platform(_)))
        ));
    }

    /// A send to an administratively down Linux device fails with raw
    /// `EIO`: `InvalidState` on Linux `send` only. The same raw code on
    /// Linux `recv` or on any other host is not remapped to it.
    #[test]
    fn raw_eio_is_invalid_state_on_linux_send_only() {
        let eio = || io::Error::from_raw_os_error(EIO);
        assert!(matches!(
            send_result(HostOs::Linux, eio()),
            Err(Error::InvalidState)
        ));
        let recv = recv_err(HostOs::Linux, eio());
        assert!(!matches!(recv, Err(Error::InvalidState)), "{recv:?}");
        for os in [HostOs::Macos, HostOs::Windows, HostOs::Other] {
            let send = send_result(os, eio());
            assert!(!matches!(send, Err(Error::InvalidState)), "{os:?} {send:?}");
            let recv = recv_err(os, eio());
            assert!(!matches!(recv, Err(Error::InvalidState)), "{os:?} {recv:?}");
        }
    }

    #[test]
    #[cfg(windows)]
    fn error_operation_aborted_matches_windows_sys() {
        assert_eq!(
            ERROR_OPERATION_ABORTED as u32,
            windows_sys::Win32::Foundation::ERROR_OPERATION_ABORTED
        );
    }

    /// Windows raw 995 (`ERROR_OPERATION_ABORTED`, a tap-windows adapter
    /// whose media `apply(Down)` disconnected) is `InvalidState` on both
    /// directions of a Windows TAP handle only (on `recv`, once the status
    /// reads `Down`). On a Windows TUN handle and on every other host it
    /// stays a generic platform error, and the status is never read.
    #[test]
    fn raw_operation_aborted_is_invalid_state_on_windows_tap_only() {
        let tap = DeviceKind::Tap;
        let status = Status::new(down);
        let recv = recv_step(HostOs::Windows, tap, 64, Err(aborted()), &mut false, || {
            status.read()
        });
        assert!(
            matches!(recv, Step::Done(Err(Error::InvalidState))),
            "{recv:?}"
        );
        assert_eq!(status.calls(), 1);
        let send = send_result_on(HostOs::Windows, tap, aborted());
        assert!(matches!(send, Err(Error::InvalidState)), "{send:?}");
        for os in ALL_OSES {
            for kind in [DeviceKind::Tun, DeviceKind::Tap] {
                if os == HostOs::Windows && kind == DeviceKind::Tap {
                    continue;
                }
                // `recv_err_on` panics if the status is read.
                let recv = recv_err_on(os, kind, aborted());
                assert!(
                    matches!(recv, Err(Error::Platform(_))),
                    "{os:?} {kind:?} {recv:?}"
                );
                let send = send_result_on(os, kind, aborted());
                assert!(
                    matches!(send, Err(Error::Platform(_))),
                    "{os:?} {kind:?} {send:?}"
                );
            }
        }
    }

    /// The retry decision: only a call that has not retried yet, and only
    /// while the status reads `Up`. A call that already retried does not
    /// read the status.
    #[test]
    fn tap_abort_retries_only_the_first_995_of_a_call_while_up() {
        for (result, expected) in [
            (up as fn() -> io::Result<AdminState>, true),
            (down, false),
            (unknown, false),
            (status_failed, false),
        ] {
            let status = Status::new(result);
            assert_eq!(
                tap_abort_retries(false, || status.read()),
                expected,
                "{:?}",
                result()
            );
            assert_eq!(status.calls(), 1);
        }
        for result in [up as fn() -> io::Result<AdminState>, down, status_failed] {
            let status = Status::new(result);
            assert!(!tap_abort_retries(true, || status.read()));
            assert_eq!(status.calls(), 0, "a retried call does not read the status");
        }
    }

    /// A Windows TAP 995 on `recv`, through `recv_step`: retried once
    /// while the status reads `Up` (marking the call), `InvalidState` on
    /// the call's second 995 without a status read.
    #[test]
    fn recv_step_retries_a_windows_tap_995_once_per_call() {
        let status = Status::new(up);
        let mut retried = false;
        let step = |retried: &mut bool| {
            recv_step(
                HostOs::Windows,
                DeviceKind::Tap,
                64,
                Err(aborted()),
                retried,
                || status.read(),
            )
        };
        assert!(matches!(step(&mut retried), Step::Retry));
        assert!(retried);
        assert!(matches!(
            step(&mut retried),
            Step::Done(Err(Error::InvalidState))
        ));
        assert_eq!(status.calls(), 1);
    }

    /// The blocking and async loops on Windows TAP: a 995 then data is
    /// one transparent retry; two 995s, a `Down` status or a failed status
    /// read end the call with `InvalidState`. The status is read at most
    /// once per call.
    #[test]
    fn windows_tap_recv_loops_retry_one_995_while_up() {
        type Case = (
            Vec<io::Result<usize>>,
            fn() -> io::Result<AdminState>,
            Option<usize>,
            usize,
        );
        let cases = || -> Vec<Case> {
            vec![
                (vec![Err(aborted()), Ok(5)], up, Some(5), 2),
                (vec![Err(aborted()), Err(aborted()), Ok(5)], up, None, 2),
                (vec![Err(aborted()), Ok(5)], down, None, 1),
                (vec![Err(aborted()), Ok(5)], unknown, None, 1),
                (vec![Err(aborted()), Ok(5)], status_failed, None, 1),
            ]
        };
        for (results, result, expected, reads) in cases() {
            let script = Script::new(results, b"abcde");
            let status = Status::new(result);
            let mut buf = [0u8; 8];
            let got = recv_blocking(
                HostOs::Windows,
                DeviceKind::Tap,
                &mut buf,
                &|| status.read(),
                |buf| script.read(buf),
            );
            check_tap_recv(&got, expected);
            assert_eq!(script.calls(), reads, "blocking reads, {expected:?}");
            assert_eq!(status.calls(), 1, "blocking status reads, {expected:?}");
        }
        for (results, result, expected, reads) in cases() {
            let source = AsyncScript(std::sync::Mutex::new(Script::new(results, b"abcde")));
            let status = Status::new(result);
            let mut buf = [0u8; 8];
            let got = ready(recv_async(
                HostOs::Windows,
                DeviceKind::Tap,
                &source,
                &|| status.read(),
                &mut buf,
            ));
            check_tap_recv(&got, expected);
            let calls = source.0.lock().unwrap().calls();
            assert_eq!(calls, reads, "async reads, {expected:?}");
            assert_eq!(status.calls(), 1, "async status reads, {expected:?}");
        }
    }

    /// `Some(n)` expects `Ok(n)`; `None` expects `InvalidState`.
    fn check_tap_recv(got: &Result<usize>, expected: Option<usize>) {
        match expected {
            Some(n) => assert!(matches!(got, Ok(m) if *m == n), "{got:?}"),
            None => assert!(matches!(got, Err(Error::InvalidState)), "{got:?}"),
        }
    }

    /// A 995 that is not a Windows TAP `recv` 995 leaves the loops' result
    /// as it was and never reads the status: Windows TUN and every other
    /// host, TUN and TAP.
    #[test]
    fn other_995s_do_not_read_the_status() {
        for os in ALL_OSES {
            for kind in [DeviceKind::Tun, DeviceKind::Tap] {
                if os == HostOs::Windows && kind == DeviceKind::Tap {
                    continue;
                }
                let script = Script::new(vec![Err(aborted()), Ok(3)], b"abc");
                let mut buf = [0u8; 8];
                let got = recv_blocking(os, kind, &mut buf, &no_status, |buf| script.read(buf));
                assert!(matches!(got, Err(Error::Platform(_))), "{os:?} {kind:?}");
                assert_eq!(script.calls(), 1);
                let source = AsyncScript(std::sync::Mutex::new(Script::new(
                    vec![Err(aborted()), Ok(3)],
                    b"abc",
                )));
                let got = ready(recv_async(os, kind, &source, &no_status, &mut buf));
                assert!(matches!(got, Err(Error::Platform(_))), "{os:?} {kind:?}");
                assert_eq!(source.0.lock().unwrap().calls(), 1);
            }
        }
    }

    /// The Windows TAP 995 rule ends the loops: `recv` after its one
    /// retry (status `Up`) or at once (status `Down`), `send` after one
    /// native call (it never retries a 995).
    #[test]
    fn windows_tap_media_down_ends_the_loops() {
        let script = Script::new(vec![Err(aborted()), Err(aborted()), Ok(3)], b"abc");
        let status = Status::new(up);
        let mut buf = [0u8; 8];
        let result = recv_blocking(
            HostOs::Windows,
            DeviceKind::Tap,
            &mut buf,
            &|| status.read(),
            |buf| script.read(buf),
        );
        assert!(matches!(result, Err(Error::InvalidState)), "{result:?}");
        assert_eq!(script.calls(), 2);

        let script = Script::new(vec![Err(aborted()), Ok(3)], b"abc");
        let status = Status::new(down);
        let result = recv_blocking(
            HostOs::Windows,
            DeviceKind::Tap,
            &mut buf,
            &|| status.read(),
            |buf| script.read(buf),
        );
        assert!(matches!(result, Err(Error::InvalidState)), "{result:?}");
        assert_eq!(script.calls(), 1);

        let script = Script::new(vec![Err(aborted()), Ok(5)], b"");
        let result = ready(send_async(HostOs::Windows, DeviceKind::Tap, || {
            let result = script.read(&mut []);
            async move { result }
        }));
        assert!(matches!(result, Err(Error::InvalidState)), "{result:?}");
        assert_eq!(script.calls(), 1);

        let script = Script::new(vec![Err(aborted()), Ok(5)], b"");
        let result = send_blocking(HostOs::Windows, DeviceKind::Tap, || script.read(&mut []));
        assert!(matches!(result, Err(Error::InvalidState)), "{result:?}");
        assert_eq!(script.calls(), 1);
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
        send_result_on(os, TUN, err)
    }

    fn send_result_on(os: HostOs, kind: DeviceKind, err: io::Error) -> Result<usize> {
        match send_step(os, kind, Err(err)) {
            Step::Retry => panic!("{os:?} {kind:?}: a lifecycle error must not be retried"),
            Step::Done(result) => result,
        }
    }

    fn recv_err(os: HostOs, err: io::Error) -> Result<usize> {
        recv_err_on(os, TUN, err)
    }

    fn recv_err_on(os: HostOs, kind: DeviceKind, err: io::Error) -> Result<usize> {
        recv_result_on(os, kind, 64, Err(err)).expect("a lifecycle error must not be retried")
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
            for kind in [DeviceKind::Tun, DeviceKind::Tap] {
                let mapped = lifecycle_error(os, kind, &err);
                assert!(mapped.is_none(), "{os:?} {kind:?} {err}: {mapped:?}");
            }
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
        let result = recv_blocking(HostOs::Linux, TUN, &mut buf, &no_status, |buf| {
            script.read(buf)
        });
        assert!(matches!(result, Err(Error::Disconnected)), "{result:?}");
        assert_eq!(script.calls(), 1);

        let source = AsyncScript(std::sync::Mutex::new(Script::new(
            vec![Err(wintun_disabled()), Ok(3)],
            b"abc",
        )));
        let result = ready(recv_async(
            HostOs::Windows,
            TUN,
            &source,
            &no_status,
            &mut buf,
        ));
        assert!(matches!(result, Err(Error::InvalidState)), "{result:?}");
        assert_eq!(source.0.lock().unwrap().calls(), 1);

        let script = Script::new(
            vec![Err(io::Error::from(io::ErrorKind::WriteZero)), Ok(5)],
            b"",
        );
        let result = send_blocking(HostOs::Windows, TUN, || script.read(&mut []));
        assert!(matches!(result, Err(Error::Disconnected)), "{result:?}");
        assert_eq!(script.calls(), 1);

        let script = Script::new(vec![Err(io::Error::from_raw_os_error(ENXIO)), Ok(5)], b"");
        let result = ready(send_async(HostOs::Macos, TUN, || {
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
            assert!(
                matches!(send_step(os, TUN, Err(eintr())), Step::Retry),
                "{os:?}"
            );
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
                send_step(os, TUN, Err(cancel())),
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
                send_step(HostOs::Windows, TUN, Err(err())),
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
        let n = recv_blocking(HostOs::Linux, TUN, &mut buf, &no_status, |buf| {
            script.read(buf)
        })
        .unwrap();
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
        let result = recv_blocking(HostOs::Macos, TUN, &mut buf, &no_status, |buf| {
            script.read(buf)
        });
        assert!(matches!(result, Err(Error::Disconnected)), "{result:?}");
        assert_eq!(script.calls(), 3);
    }

    #[test]
    fn blocking_recv_does_not_retry_code_less_interrupted() {
        let script = Script::new(vec![Err(cancel()), Ok(3)], b"abc");
        let mut buf = [0u8; 8];
        let result = recv_blocking(HostOs::Macos, TUN, &mut buf, &no_status, |buf| {
            script.read(buf)
        });
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
        let first = recv_blocking(HostOs::Linux, TUN, &mut buf, &no_status, |buf| {
            script.read(buf)
        });
        assert!(matches!(first, Err(Error::BufferTooSmall)));
        let second = recv_blocking(HostOs::Linux, TUN, &mut buf, &no_status, |buf| {
            script.read(buf)
        });
        assert_eq!(second.unwrap(), 3);
    }

    #[test]
    fn blocking_send_retries_eintr_only() {
        let script = Script::new(vec![Err(eintr()), Ok(5)], b"");
        let n = send_blocking(HostOs::Linux, TUN, || script.read(&mut [])).unwrap();
        assert_eq!(n, 5);
        assert_eq!(script.calls(), 2);

        let script = Script::new(vec![Err(cancel()), Ok(5)], b"");
        assert!(send_blocking(HostOs::Linux, TUN, || script.read(&mut [])).is_err());
        assert_eq!(script.calls(), 1);
    }

    #[test]
    fn async_recv_re_awaits_eintr_and_the_caller_sees_only_the_data() {
        let source = AsyncScript(std::sync::Mutex::new(Script::new(
            vec![Err(eintr()), Err(feth_empty_read()), Ok(3)],
            b"abc",
        )));
        let mut buf = [0u8; 8];
        let n = ready(recv_async(
            HostOs::Macos,
            TUN,
            &source,
            &no_status,
            &mut buf,
        ))
        .unwrap();
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
            let result = ready(recv_async(
                HostOs::Linux,
                TUN,
                &source,
                &no_status,
                &mut buf,
            ));
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
        let first = ready(recv_async(
            HostOs::Linux,
            TUN,
            &source,
            &no_status,
            &mut buf,
        ));
        assert!(matches!(first, Err(Error::BufferTooSmall)));
        let second = ready(recv_async(
            HostOs::Linux,
            TUN,
            &source,
            &no_status,
            &mut buf,
        ));
        assert_eq!(second.unwrap(), 3);
    }

    #[test]
    fn async_send_retries_eintr() {
        let script = Script::new(vec![Err(eintr()), Ok(5)], b"");
        let n = ready(send_async(HostOs::Macos, TUN, || {
            let result = script.read(&mut []);
            async move { result }
        }))
        .unwrap();
        assert_eq!(n, 5);
        assert_eq!(script.calls(), 2);
    }
}
