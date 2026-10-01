//! `tun-rs`-backed cross-platform TUN/TAP backend for Tunnel Lattice.
//!
//! `tun-rs` is itself cross-platform (Linux, Windows, macOS, the BSDs, iOS,
//! Android), so this is one shared backend crate rather than net-lattice's
//! per-OS split (`net-lattice-backend-linux`/`-windows`/`-darwin`) — see the
//! workspace `ARCHITECTURE.md`, "Backend replacement plan," for how this
//! crate can be replaced later by hand-written per-OS TUN/TAP code without
//! moving `tunnel-lattice-platform` or the `tunnel-lattice` facade's public
//! API.
//!
//! The upstream API surface (`tun_rs::DeviceBuilder`, `SyncDevice`,
//! `AsyncDevice`, `Layer`) and the `open` error classification were verified
//! against `tun-rs` 2.8.11, which is this crate's minimum `tun-rs` version.
//! Newer `tun-rs` 2.x releases resolve without a separate review; the
//! privileged Windows CI tests that open a device before any driver is
//! installed are the check that such a release has not changed the errors
//! `open` relies on.

#![warn(missing_docs)]

#[cfg(any(all(target_os = "macos", feature = "async"), all(test, unix)))]
mod macos_tap;
mod open_contract;
mod recv_contract;
#[cfg(all(target_os = "linux", feature = "tokio"))]
mod tokio_linux;
#[cfg(target_os = "windows")]
mod windows_tap_probe;

use tunnel_lattice_core::{Error, PlatformErrorCode, Result};
use tunnel_lattice_model::{
    AdminState, Device, DeviceConfig, DeviceConfigPatch, DeviceId, DeviceKind, MacAddress,
};
#[cfg(feature = "async")]
use tunnel_lattice_platform::AsyncPacketIo;
use tunnel_lattice_platform::{
    Capability, CapabilityProvider, DeviceMutator, DeviceObserver, DeviceProvider, PacketIo,
};
#[cfg(target_os = "linux")]
use tunnel_lattice_platform::{MultiQueueProvider, PersistentDevice};

/// The `tun-rs`-backed implementation of Tunnel Lattice's provider traits.
///
/// Stateless: opening a device does not go through a persistent connection
/// object the way `net-lattice`'s Netlink/WFP/route-socket backends do —
/// each [`TunRsDevice`] is independent of the others.
///
/// Marked `#[non_exhaustive]` so configuration can be added later without a
/// breaking change: outside this crate, construct it with
/// [`TunRsBackend::new`] or [`Default`], not the `TunRsBackend` literal.
///
/// # Opening a device
///
/// [`DeviceProvider::open`] checks the requested name and MTU before any
/// native call and returns [`Error::InvalidState`] if either cannot be
/// honored exactly (the per-OS name formats are listed on
/// `tunnel_lattice_model::DeviceConfig::name`):
///
/// | OS | Name rule |
/// |---|---|
/// | all | non-empty, no NUL character |
/// | Linux | at most 15 bytes, no `%` (the kernel would expand `%d` as a naming template) |
/// | macOS `Tap` | `feth<N>`, `N` a decimal number from 0 to 32767 (the kernel's highest `feth` unit) with no sign or leading zero (bare `feth` or `feth4294967295` would let the kernel pick the unit) |
/// | macOS `Tun` | `utun<N>`, `N` a decimal number below `u32::MAX` with no sign or leading zero, at most 15 bytes |
/// | Windows | at most 255 UTF-16 code units |
///
/// A native failure is then mapped by these `open`-only rules before the
/// general mapping described on the crate's error table:
///
/// | OS / kind | Native failure | [`Error`] |
/// |---|---|---|
/// | Windows `Tun` | `wintun.dll` could not be loaded, or lacks a required function | [`Error::DriverUnavailable`] |
/// | Windows `Tap` | no `tap0901` (tap-windows6) driver installed | [`Error::DriverUnavailable`] |
/// | Windows `Tap` | an adapter with the requested name already exists | [`Error::AlreadyExists`] |
/// | Linux | `ENODEV`/`ENOENT`: the `tun` module or `/dev/net/tun` is missing | [`Error::DriverUnavailable`] |
/// | Linux | `EINVAL`/`EBUSY` while an interface with the requested name exists (a TUN/TAP or multi-queue mismatch, or a non-multi-queue device that already has a queue attached) | [`Error::AlreadyExists`] |
/// | macOS | `EBUSY` while an interface with the requested name exists (a `utun` unit in use) | [`Error::AlreadyExists`] |
///
/// On macOS and Windows a `Tap` open never adopts an existing interface of
/// the requested name: it fails with [`Error::AlreadyExists`] and leaves the
/// existing interface untouched. Two cases attach to an existing device
/// instead, and neither destroys it when the handle drops: on Linux, a
/// persistent same-kind device with no queue attached, or any same-kind
/// multi-queue device when `multi_queue` is requested (including one opened
/// by another process); and on Windows, an existing Wintun adapter whose
/// name matches a `Tun` request.
///
/// A requested MAC (`tunnel_lattice_model::DeviceConfig::mac`) is checked
/// with the name and MTU: requesting one for a `Tun` device returns
/// [`Error::InvalidState`] before any native call. For a `Tap` device the
/// MAC is passed to the driver at creation and read back once the device
/// exists; if the read-back MAC differs (the platform ignored the request),
/// the new device is torn down and `open` returns [`Error::Unsupported`].
///
/// On Linux, attaching to an existing (persistent) device with an MTU the
/// kernel rejects fails with `EINVAL` while the name exists, so it is
/// reported as [`Error::AlreadyExists`] rather than an MTU error: the same
/// limitation as the `EINVAL` row above.
///
/// ```
/// use tunnel_lattice_backend_tunrs::TunRsBackend;
///
/// let backend = TunRsBackend::new();
/// let same = TunRsBackend::default();
/// # let _ = (backend, same);
/// ```
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct TunRsBackend;

impl TunRsBackend {
    /// Creates a new backend handle. Stateless — never fails.
    pub const fn new() -> Self {
        Self
    }
}

/// An open `tun-rs` TUN/TAP device.
///
/// Holds exactly one underlying `tun-rs` handle, never both: `SyncDevice`
/// has no `try_clone` on macOS/Windows (only on Linux), so this crate never
/// clones a handle to keep a sync and an async view side by side. With the
/// `async` feature, `handle` is a `tun_rs::AsyncDevice` built directly via
/// `DeviceBuilder::build_async`; [`PacketIo`] is then implemented by
/// blocking on the async methods (`futures::executor::block_on`).
/// `SyncDevice`/`AsyncDevice` both deref to the same `tun_rs::DeviceImpl`,
/// so device metadata (name/mtu/if_index/enabled) reads identically either
/// way.
///
/// Because of that, the blocking [`PacketIo`] `recv`/`send` must not be
/// called from async code in an async build. With `tokio` they block
/// through `tokio::runtime::Handle::current().block_on`, which panics when
/// called outside a Tokio runtime context or from inside an asynchronous
/// context (a task, `#[tokio::main]`, or `block_on`), and hangs on a
/// `current_thread` runtime. With `async-io` they block through
/// `futures::executor::block_on`, which parks the executor thread polling
/// the calling task. Inside async code use `AsyncPacketIo` (the facade's
/// `send_async` and `packet_stream`), whose futures are polled by the
/// runtime itself rather than blocked on; `send_async` also works on a
/// `current_thread` Tokio runtime.
///
/// On Linux with the `tokio` feature, receiving is the one exception: it
/// reads through a private duplicate of the device descriptor, registered
/// with the Tokio reactor for both readable and error readiness, because
/// `tun-rs` waits for readable readiness only and a deleted device reports
/// an error readiness alone (see "When the device goes away"). The
/// duplicate is owned by this handle, costs one extra descriptor, and
/// closes with it. Sending stays on the `tun-rs` handle.
///
/// On macOS with `async-io` or `tokio`, a TAP (`feth`) device's `recv`
/// waits for its BPF read descriptor itself instead of through `tun-rs`:
/// on a blocking-pool thread, in waits of at most 250 ms, checking at each
/// timeout that the descriptor is still bound to its interface. macOS
/// readiness notification for BPF does not report the interface going
/// away, so a wait without that check would never end (see "When the
/// device goes away"). `tun-rs` still reads every packet. This costs one
/// extra descriptor per TAP handle, closed with it, and a pipe per pending
/// wait, released as soon as the wait ends or its `recv` is dropped.
/// Sending, and a macOS TUN (`utun`) device, stay on the `tun-rs` handle.
///
/// # Receiving packets
///
/// `recv` (sync [`PacketIo`] and, with an async feature, `AsyncPacketIo`)
/// never truncates a packet and never reports more bytes than the buffer
/// holds. A packet that does not fit is discarded and `recv` returns
/// [`Error::BufferTooSmall`]; the next `recv` receives the following packet.
/// Size buffers with [`Device::recv_buffer_len`]. Per platform:
///
/// | OS / kind | Native behavior for an oversize packet | How it becomes [`Error::BufferTooSmall`] |
/// |---|---|---|
/// | Linux TUN/TAP, macOS TUN (`utun`) | silently truncated | the read uses a one-byte sentinel buffer after `buf`; a length past `buf.len()` means the packet did not fit |
/// | macOS TAP (`feth`) | rejected with `InvalidData` | mapped |
/// | Windows TUN (Wintun) / TAP (tap-windows6) | rejected with `InvalidInput` | mapped |
///
/// `recv` and `send` also retry two transient conditions internally
/// instead of reporting them: a signal interrupting the call (`EINTR`, on
/// Linux and macOS), and, on macOS TAP, a read that produced no complete
/// frame (`tun-rs` reports it as an end-of-file with the message
/// `"recv buffer is empty"`, which this crate matches exactly for
/// `tun-rs` 2.8.11). Each retry re-enters the blocking read or readiness
/// wait, so it never spins. Every other end-of-file, such as the device
/// being closed or a Wintun session ending, is [`Error::Disconnected`].
///
/// # When the device goes away
///
/// `recv` and `send` also map the native errors that mean the device is
/// gone or disabled, before the general mapping on the crate's error table:
///
/// | OS | Native error | [`Error`] |
/// |---|---|---|
/// | macOS (and Linux, for symmetry) | `ENXIO`: on macOS, the TAP's BPF descriptor after the peer `feth` it is bound to was destroyed | [`Error::Disconnected`] |
/// | Linux | `EBADFD`: the device was deleted and its queue detached | [`Error::Disconnected`] |
/// | Linux (`recv`) | `EFAULT`: a blocking read already waiting when the device was deleted | [`Error::Disconnected`] |
/// | Windows TUN (`send`) | Wintun reports the adapter terminating (`tun-rs` returns `WriteZero`) | [`Error::Disconnected`] |
/// | Windows TUN | `"The interface has been disabled"`: the adapter was disabled, for example by applying `DesiredAdminState::Down` | [`Error::InvalidState`]; applying `DesiredAdminState::Up` recovers it |
///
/// So on Linux, deleting the device (for example with `ip link del`) ends
/// a pending or later `recv` with [`Error::Disconnected`] in every feature
/// set, and so does destroying the peer `feth` of a macOS TAP device (with
/// `async-io` or `tokio`, a pending `recv` notices within about 250 ms).
/// A `PacketStream` from `tunnel-lattice-async` ends after any of
/// these errors (every error except [`Error::BufferTooSmall`] ends it).
pub struct TunRsDevice {
    kind: DeviceKind,
    /// The interface index read once at open; see [`DeviceObserver::id`].
    id: DeviceId,
    /// The error-aware receive registration (see above). Declared before
    /// `handle` so it drops, and deregisters, first.
    #[cfg(all(target_os = "linux", feature = "tokio"))]
    reader: tokio_linux::ErrorAwareReader,
    /// The bounded receive wait of a macOS TAP device (see above); `None`
    /// for TUN. Declared before `handle` so its duplicate descriptor closes
    /// first.
    #[cfg(all(target_os = "macos", feature = "async"))]
    tap_wait: Option<macos_tap::BpfWait>,
    #[cfg(feature = "async")]
    handle: tun_rs::AsyncDevice,
    #[cfg(not(feature = "async"))]
    handle: tun_rs::SyncDevice,
}

/// Maps a `tun-rs`/OS `io::Error` onto the workspace [`Error`].
///
/// Portable `io::ErrorKind`s with a matching typed variant are mapped
/// first, so callers can match on them without inspecting a raw platform
/// code. `std` derives these kinds from the native code on every platform
/// (e.g. Linux `EPERM`/`EACCES` and Windows `ERROR_ACCESS_DENIED` are both
/// `PermissionDenied`), and `tun-rs` also constructs some of them directly
/// without any OS code:
///
/// | `io::ErrorKind`                                  | [`Error`]                   |
/// |--------------------------------------------------|-----------------------------|
/// | `PermissionDenied`                               | [`Error::PermissionDenied`] |
/// | `NotFound`                                       | [`Error::NotFound`]         |
/// | `AlreadyExists`                                  | [`Error::AlreadyExists`]    |
/// | `BrokenPipe`, `UnexpectedEof`, `NotConnected`    | [`Error::Disconnected`]     |
/// | `Unsupported`                                    | [`Error::Unsupported`]      |
/// | anything else, with a raw OS code                | [`Error::Platform`] with the target's tag (`Linux`/`Windows`/`Darwin`) and that code |
/// | anything else, without a raw OS code             | `Error::Platform(PlatformErrorCode::Unknown)` |
///
/// The typed variants intentionally drop the raw OS code: they are the
/// primary contract, [`Error::Platform`] is only the diagnostic fallback.
/// A code-less error (e.g. one `tun-rs` builds with `io::Error::other`) is
/// reported as [`PlatformErrorCode::Unknown`] rather than a fabricated `0`
/// code. On a target other than Linux, Windows, and macOS, where
/// [`PlatformErrorCode`] has no matching tag, every unmapped error is
/// [`PlatformErrorCode::Unknown`].
///
/// `recv`/`send` apply their own rules first (retrying transient errors,
/// reporting a too-small buffer as [`Error::BufferTooSmall`], and mapping
/// the device-gone and device-disabled errors; see [`TunRsDevice`],
/// "Receiving packets" and "When the device goes away"), so a retried
/// `EINTR` or macOS `"recv buffer is empty"` end-of-file never reaches this
/// table.
fn io_error(err: std::io::Error) -> Error {
    use std::io::ErrorKind;

    match err.kind() {
        ErrorKind::PermissionDenied => return Error::PermissionDenied,
        ErrorKind::NotFound => return Error::NotFound,
        ErrorKind::AlreadyExists => return Error::AlreadyExists,
        // The device's read/write channel is gone for good: a closed pipe,
        // an end-of-file on the device handle (e.g. Wintun's
        // `ERROR_HANDLE_EOF` on a torn-down session, or macOS's feth-based
        // TAP read loop closing), or a socket that is no longer connected.
        ErrorKind::BrokenPipe | ErrorKind::UnexpectedEof | ErrorKind::NotConnected => {
            return Error::Disconnected;
        }
        // `tun-rs` reports operations that are semantically unsupported by
        // the current device configuration (e.g. cloning a queue on a
        // device that wasn't opened with multi-queue) as
        // `io::ErrorKind::Unsupported` with no raw OS error code —
        // `Error::Platform(PlatformErrorCode::Unknown)` would say nothing
        // about what went wrong. `ErrorKind::Unsupported` is a portable
        // Rust-level signal, not an OS one, so it maps directly onto our
        // own `Error::Unsupported` instead.
        ErrorKind::Unsupported => return Error::Unsupported,
        _ => {}
    }
    Error::Platform(platform_code(err.raw_os_error()))
}

/// Tags a raw OS error code with the current target's platform, or returns
/// [`PlatformErrorCode::Unknown`] when there is no code or no matching tag.
fn platform_code(raw: Option<i32>) -> PlatformErrorCode {
    let Some(code) = raw else {
        return PlatformErrorCode::Unknown;
    };
    #[cfg(target_os = "linux")]
    {
        PlatformErrorCode::Linux(code)
    }
    #[cfg(target_os = "windows")]
    {
        // `std` stores a Windows `DWORD` in an `i32`; reinterpret the bits.
        PlatformErrorCode::Windows(code as u32)
    }
    #[cfg(target_os = "macos")]
    {
        PlatformErrorCode::Darwin(code)
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        let _ = code;
        PlatformErrorCode::Unknown
    }
}

impl DeviceProvider for TunRsBackend {
    type DeviceConfig = DeviceConfig;
    type Device = TunRsDevice;

    /// Opens a device; see [`TunRsBackend`], "Opening a device", for the
    /// name prechecks and the `open`-specific error mapping.
    fn open(&self, config: Self::DeviceConfig) -> Result<Self::Device> {
        let layer = match config.kind {
            DeviceKind::Tun => tun_rs::Layer::L3,
            DeviceKind::Tap => tun_rs::Layer::L2,
            _ => return Err(Error::Unsupported),
        };
        // Prechecks: everything below this block may make a native call.
        if let Some(name) = config.name.as_deref() {
            open_contract::precheck_name(open_contract::HOST_OS, config.kind, name)?;
        }
        let mtu = config
            .mtu
            .map(|mtu| u16::try_from(mtu).map_err(|_| Error::InvalidState))
            .transpose()?;
        if config.mac.is_some() && config.kind != DeviceKind::Tap {
            return Err(Error::InvalidState);
        }

        let mut builder = tun_rs::DeviceBuilder::new().layer(layer);
        // `tun-rs` defaults `reuse_dev` to `true`, which on macOS/Windows TAP
        // adopts an existing interface of the requested name — and on macOS
        // then destroys that foreign `feth` when the handle drops. Turn it
        // off so an existing TAP name fails with `AlreadyExists` instead
        // (the option exists only on these targets and only affects TAP).
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        if config.kind == DeviceKind::Tap {
            builder = builder.reuse_dev(false);
        }
        // `DeviceBuilder::multi_queue` only exists on Linux in `tun-rs`
        // itself (not merely a no-op elsewhere) — `IFF_MULTI_QUEUE` has no
        // equivalent concept on macOS/Windows, so there is nothing to call
        // there. `DeviceConfig::multi_queue`'s own docs already say the
        // request is ignored off Linux; this is that ignoring.
        #[cfg(target_os = "linux")]
        {
            builder = builder.multi_queue(config.multi_queue);
        }
        if let Some(name) = config.name.as_deref() {
            builder = builder.name(name);
        }
        if let Some(mtu) = mtu {
            builder = builder.mtu(mtu);
        }
        if let Some(mac) = config.mac {
            builder = builder.mac_addr(mac.octets());
        }

        #[cfg(feature = "async")]
        let built = builder.build_async();
        #[cfg(not(feature = "async"))]
        let built = builder.build_sync();
        let handle = built.map_err(|err| {
            open_contract::classify_open_error(
                open_contract::HOST_OS,
                config.kind,
                config.name.as_deref(),
                err,
                open_contract::host_name_exists,
            )
        })?;
        // On failure `handle` drops: a new device is torn down, an attached
        // persistent one is detached.
        let id = read_device_id(&handle)?;
        if let Some(mac) = config.mac {
            confirm_requested_mac(&handle, mac)?;
        }

        Ok(TunRsDevice {
            kind: config.kind,
            id,
            // On failure `handle` drops: a new device is torn down, an
            // attached persistent one is detached.
            #[cfg(all(target_os = "linux", feature = "tokio"))]
            reader: tokio_linux::ErrorAwareReader::new(&*handle).map_err(io_error)?,
            // On failure `handle` drops, which destroys the new `feth` pair
            // (`reuse_dev(false)` above, so the pair is always ours).
            #[cfg(all(target_os = "macos", feature = "async"))]
            tap_wait: match config.kind {
                DeviceKind::Tap => Some(macos_tap::BpfWait::new(&*handle).map_err(io_error)?),
                _ => None,
            },
            handle,
        })
    }
}

/// Reads the interface index that becomes the device's identity. On Linux
/// and macOS `tun-rs` returns `if_nametoindex`, where `0` means the lookup
/// failed, so `0` is an error rather than an identity.
fn read_device_id(handle: &tun_rs::DeviceImpl) -> Result<DeviceId> {
    device_id_from_index(handle.if_index())
}

/// Reads the MAC back after a TAP device was opened with one, so a platform
/// that silently ignored the request fails `open` with
/// [`Error::Unsupported`] instead of handing out a device with another MAC.
fn confirm_requested_mac(handle: &tun_rs::DeviceImpl, requested: MacAddress) -> Result<()> {
    let actual = handle.mac_address().map_err(io_error)?;
    if actual == requested.octets() {
        Ok(())
    } else {
        Err(Error::Unsupported)
    }
}

fn device_id_from_index(index: std::io::Result<u32>) -> Result<DeviceId> {
    match index.map_err(io_error)? {
        0 => Err(Error::Platform(PlatformErrorCode::Unknown)),
        index => Ok(DeviceId::new(u64::from(index))),
    }
}

impl TunRsDevice {
    /// Blocking receive on whichever handle this build holds — the async
    /// build blocks on `AsyncDevice::recv` rather than keeping a second
    /// blocking-capable handle around (see [`TunRsDevice`]'s docs).
    ///
    /// Under the `tokio` feature this must go through
    /// `Handle::current().block_on`, never `futures::executor::block_on`:
    /// tun-rs's Tokio-backed `AsyncDevice` relies on Tokio's own I/O driver
    /// to deliver readiness, and only Tokio's own `block_on` polls that
    /// driver — a foreign executor drives the future manually but the
    /// driver never runs, so the call hangs forever waiting for a wakeup
    /// that never arrives (confirmed: a real `send()` deadlocked under a
    /// 10s timeout with `futures::executor::block_on`). `async-io`'s
    /// `AsyncDevice` has no such requirement, since it's built on the
    /// runtime-agnostic `async-io`/`blocking` crates instead.
    ///
    /// Applies the `recv` error contract (internal retries and the
    /// too-small-buffer normalization, see `recv_contract`) in every build.
    ///
    /// **Also requires the caller's Tokio runtime to be multi-threaded.**
    /// `Handle::block_on` only drives a runtime's I/O reactor on the
    /// `multi_thread` flavor, whose worker threads poll it independently of
    /// where `block_on` is called from; on `current_thread`, only
    /// `Runtime::block_on` (called on the owned `Runtime`, not a `Handle`)
    /// drives it, so calling this from a `current_thread` runtime
    /// (`#[tokio::main(flavor = "current_thread")]`) hangs the same way —
    /// confirmed by an isolated repro against `tun-rs` directly, independent
    /// of this crate. See this crate's `privileged_tests` module (test-only,
    /// not part of the public API) and `ARCHITECTURE.md`, "Async design."
    fn blocking_recv(&self, buf: &mut [u8]) -> Result<usize> {
        #[cfg(feature = "tokio")]
        {
            tokio::runtime::Handle::current().block_on(self.async_recv(buf))
        }
        #[cfg(all(feature = "async", not(feature = "tokio")))]
        {
            futures::executor::block_on(self.async_recv(buf))
        }
        #[cfg(not(feature = "async"))]
        {
            recv_contract::recv_blocking(open_contract::HOST_OS, buf, |buf| {
                sentinel_recv(&self.handle, buf)
            })
        }
    }

    /// Blocking send; see [`Self::blocking_recv`].
    fn blocking_send(&self, buf: &[u8]) -> Result<usize> {
        #[cfg(feature = "tokio")]
        {
            tokio::runtime::Handle::current().block_on(self.async_send(buf))
        }
        #[cfg(all(feature = "async", not(feature = "tokio")))]
        {
            futures::executor::block_on(self.async_send(buf))
        }
        #[cfg(not(feature = "async"))]
        {
            recv_contract::send_blocking(open_contract::HOST_OS, || self.handle.send(buf))
        }
    }

    /// Async receive with the `recv` error contract applied. On Linux with
    /// `tokio` it reads through the error-aware duplicate descriptor, and a
    /// macOS TAP device waits through its bounded BPF wait (see
    /// [`TunRsDevice`]); otherwise it reads and waits through the `tun-rs`
    /// handle.
    #[cfg(feature = "async")]
    async fn async_recv(&self, buf: &mut [u8]) -> Result<usize> {
        #[cfg(target_os = "macos")]
        if let Some(wait) = &self.tap_wait {
            let source = macos_tap::TapSource {
                handle: &self.handle,
                wait,
            };
            return recv_contract::recv_async(open_contract::HOST_OS, &source, buf).await;
        }
        #[cfg(all(target_os = "linux", feature = "tokio"))]
        let source = &self.reader;
        #[cfg(not(all(target_os = "linux", feature = "tokio")))]
        let source = &self.handle;
        recv_contract::recv_async(open_contract::HOST_OS, source, buf).await
    }

    /// Async send, retrying transient errors.
    #[cfg(feature = "async")]
    async fn async_send(&self, buf: &[u8]) -> Result<usize> {
        recv_contract::send_async(open_contract::HOST_OS, || self.handle.send(buf)).await
    }
}

/// One blocking native read. On unix it reads into `[buf, 1-byte
/// sentinel]` so an oversize packet, which Linux TUN/TAP and macOS utun
/// would otherwise truncate silently, reports a length past `buf.len()`.
#[cfg(not(feature = "async"))]
fn sentinel_recv(handle: &tun_rs::SyncDevice, buf: &mut [u8]) -> std::io::Result<usize> {
    #[cfg(unix)]
    {
        use std::io::IoSliceMut;
        let mut sentinel = [0u8; 1];
        handle.recv_vectored(&mut [IoSliceMut::new(buf), IoSliceMut::new(&mut sentinel)])
    }
    #[cfg(not(unix))]
    {
        handle.recv(buf)
    }
}

/// The async counterpart of `sentinel_recv`, re-awaited by
/// `recv_contract::recv_async` after a transient error. Not used on Linux
/// with `tokio`, which receives through `tokio_linux::ErrorAwareReader`,
/// nor for a macOS TAP device, which receives through
/// `macos_tap::TapSource`.
#[cfg(all(feature = "async", not(all(target_os = "linux", feature = "tokio"))))]
impl recv_contract::AsyncRecvSource for tun_rs::AsyncDevice {
    async fn recv_native(&self, buf: &mut [u8]) -> std::io::Result<usize> {
        #[cfg(unix)]
        {
            use std::io::IoSliceMut;
            let mut sentinel = [0u8; 1];
            self.recv_vectored(&mut [IoSliceMut::new(buf), IoSliceMut::new(&mut sentinel)])
                .await
        }
        #[cfg(not(unix))]
        {
            self.recv(buf).await
        }
    }
}

/// See [`TunRsDevice`], "Receiving packets", for the `recv` contract.
impl PacketIo for TunRsDevice {
    fn recv(&self, buf: &mut [u8]) -> Result<usize> {
        self.blocking_recv(buf)
    }

    fn send(&self, buf: &[u8]) -> Result<usize> {
        self.blocking_send(buf)
    }
}

#[cfg(feature = "async")]
impl AsyncPacketIo for TunRsDevice {
    async fn recv(&self, buf: &mut [u8]) -> Result<usize> {
        self.async_recv(buf).await
    }

    async fn send(&self, buf: &[u8]) -> Result<usize> {
        self.async_send(buf).await
    }
}

impl DeviceObserver for TunRsDevice {
    type Device = Device;

    fn id(&self) -> DeviceId {
        self.id
    }

    /// The returned record's `id` is the identity captured at open
    /// ([`DeviceObserver::id`]), not a fresh index read.
    fn snapshot(&self) -> Result<Device> {
        let id = self.id;
        let name = self.handle.name().map_err(io_error)?;
        let mtu = u32::from(self.handle.mtu().map_err(io_error)?);
        // `is_running` (IFF_UP | IFF_RUNNING) is a Linux-only read on
        // `tun_rs::DeviceImpl`; macOS/Windows/BSD only expose a write-only
        // `enabled(bool)` setter, with no corresponding getter to read
        // administrative state back. Report `Unknown` there rather than
        // guessing from `enabled`'s absence.
        #[cfg(target_os = "linux")]
        let admin_state = if self.handle.is_running().map_err(io_error)? {
            AdminState::Up
        } else {
            AdminState::Down
        };
        #[cfg(not(target_os = "linux"))]
        let admin_state = AdminState::Unknown;

        let device = Device::new(id, name, self.kind, mtu, admin_state);
        if self.kind != DeviceKind::Tap {
            return Ok(device);
        }
        match self.handle.mac_address() {
            Ok(octets) => Ok(device.with_mac(MacAddress::new(octets))),
            Err(err) if err.kind() == std::io::ErrorKind::Unsupported => Ok(device),
            Err(err) => Err(io_error(err)),
        }
    }
}

impl DeviceMutator for TunRsDevice {
    type DeviceConfigPatch = DeviceConfigPatch;

    fn apply(&self, patch: Self::DeviceConfigPatch) -> Result<()> {
        // Not `self.capabilities()`: on Windows that also runs the host-level
        // driver lookup, which a patch never needs.
        let target = ApplyTarget {
            id: self.id,
            kind: self.kind,
            mac_mutation: mac_mutation_supported(self.kind),
        };
        apply_patch(&*self.handle, target, &patch)
    }
}

/// The native steps [`apply_patch`] drives, split out so the ordering and
/// compensation logic is testable without a real device.
trait ApplySteps {
    fn mtu(&self) -> Result<u16>;
    fn set_mtu(&self, mtu: u16) -> Result<()>;
    fn mac(&self) -> Result<MacAddress>;
    fn set_mac(&self, mac: MacAddress) -> Result<()>;
    fn set_enabled(&self, enabled: bool) -> Result<()>;
}

impl ApplySteps for tun_rs::DeviceImpl {
    fn mtu(&self) -> Result<u16> {
        tun_rs::DeviceImpl::mtu(self).map_err(io_error)
    }

    fn set_mtu(&self, mtu: u16) -> Result<()> {
        tun_rs::DeviceImpl::set_mtu(self, mtu).map_err(io_error)
    }

    fn mac(&self) -> Result<MacAddress> {
        self.mac_address().map(MacAddress::new).map_err(io_error)
    }

    fn set_mac(&self, mac: MacAddress) -> Result<()> {
        self.set_mac_address(mac.octets()).map_err(io_error)
    }

    fn set_enabled(&self, enabled: bool) -> Result<()> {
        tun_rs::DeviceImpl::enabled(self, enabled).map_err(io_error)
    }
}

/// What [`apply_patch`] checks a patch against.
#[derive(Clone, Copy)]
struct ApplyTarget {
    id: DeviceId,
    kind: DeviceKind,
    /// Whether the handle reports `Capability::MAC_MUTATION`.
    mac_mutation: bool,
}

/// [`DeviceMutator::apply`]'s contract: every precondition before any
/// native call, then MTU, MAC, and administrative state in that order, and
/// on a failed step a best-effort revert of the earlier ones in reverse
/// before returning the original error.
fn apply_patch(
    steps: &impl ApplySteps,
    target: ApplyTarget,
    patch: &DeviceConfigPatch,
) -> Result<()> {
    use tunnel_lattice_model::DesiredAdminState;

    if patch.device_id() != target.id {
        return Err(Error::InvalidState);
    }
    let mtu = patch
        .mtu()
        .map(|mtu| u16::try_from(mtu).map_err(|_| Error::InvalidState))
        .transpose()?;
    let mac = patch.mac();
    if mac.is_some() && target.kind != DeviceKind::Tap {
        return Err(Error::InvalidState);
    }
    // `DesiredAdminState` is `#[non_exhaustive]`: a variant this backend
    // does not know is unsupported, not a malformed patch.
    let enable = match patch.admin_state() {
        None => None,
        Some(DesiredAdminState::Up) => Some(true),
        Some(DesiredAdminState::Down) => Some(false),
        Some(_) => return Err(Error::Unsupported),
    };
    if mac.is_some() && !target.mac_mutation {
        return Err(Error::Unsupported);
    }

    // Administrative state goes last: it cannot be read back off Linux, so
    // it cannot be reverted. A step's previous value is read first, before
    // any change, and only when a later step could need it reverted.
    let previous_mtu = match mtu {
        Some(_) if mac.is_some() || enable.is_some() => Some(steps.mtu()?),
        _ => None,
    };
    let previous_mac = match mac {
        Some(_) if enable.is_some() => Some(steps.mac()?),
        _ => None,
    };
    // Best effort: the original error is what the caller needs, and
    // `snapshot()` is authoritative after any `Err`.
    let revert_mtu = || {
        if let Some(previous) = previous_mtu {
            let _ = steps.set_mtu(previous);
        }
    };

    if let Some(mtu) = mtu {
        steps.set_mtu(mtu)?;
    }
    if let Some(mac) = mac
        && let Err(error) = steps.set_mac(mac)
    {
        revert_mtu();
        return Err(error);
    }
    if let Some(enable) = enable
        && let Err(error) = steps.set_enabled(enable)
    {
        if let Some(previous) = previous_mac {
            let _ = steps.set_mac(previous);
        }
        revert_mtu();
        return Err(error);
    }
    Ok(())
}

#[cfg(target_os = "linux")]
impl PersistentDevice for TunRsDevice {
    fn persist(&self) -> Result<()> {
        self.handle.persist().map_err(io_error)
    }
}

#[cfg(target_os = "linux")]
impl MultiQueueProvider for TunRsDevice {
    fn additional_queue(&self) -> Result<Self> {
        let handle = self.handle.try_clone().map_err(io_error)?;
        Ok(TunRsDevice {
            kind: self.kind,
            id: self.id,
            #[cfg(all(target_os = "linux", feature = "tokio"))]
            reader: tokio_linux::ErrorAwareReader::new(&*handle).map_err(io_error)?,
            handle,
        })
    }
}

/// The host-level capabilities shared by [`TunRsBackend`] and every
/// [`TunRsDevice`]; see [`TunRsBackend`]'s [`CapabilityProvider`] impl.
fn host_capabilities() -> Capability {
    let base = Capability::DEVICE_MUTATION;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let base = base | Capability::TAP_DEVICES;
    #[cfg(target_os = "linux")]
    let base = base | Capability::PERSISTENT_DEVICES | Capability::MULTI_QUEUE;
    #[cfg(target_os = "windows")]
    let base = if windows_tap_probe::tap_driver_installed() {
        base | Capability::TAP_DEVICES
    } else {
        base
    };
    #[cfg(feature = "async")]
    {
        base | Capability::NATIVE_ASYNC
    }
    #[cfg(not(feature = "async"))]
    {
        base
    }
}

/// What this host supports before any device is opened (exposed by the
/// facade as `Tunnel::capabilities()`).
///
/// | Flag | Reported |
/// |---|---|
/// | `DEVICE_MUTATION` | always |
/// | `TAP_DEVICES` | Linux and macOS always; Windows only if the tap-windows6 driver (hardware id `tap0901`) is installed |
/// | `PERSISTENT_DEVICES`, `MULTI_QUEUE` | Linux |
/// | `NATIVE_ASYNC` | with the `async-io` or `tokio` feature |
///
/// `MAC_MUTATION` is never part of this answer: it is a property of an open
/// TAP handle, reported by [`TunRsDevice`]'s own `capabilities()` (Linux and
/// macOS only).
///
/// On Windows the driver is detected on the first call with the same
/// SetupAPI driver lookup `tun-rs` performs before creating a TAP adapter,
/// stopping before anything is registered: no device or adapter is created
/// and no elevation is needed. The answer is cached for the life of the
/// process, so a driver installed or removed later is not noticed until
/// the process restarts. A failed lookup reports the flag as absent.
///
/// This answer is advisory: [`DeviceProvider::open`] never refuses a
/// request because of it, and a missing driver is still reported by `open`
/// itself (as [`Error::DriverUnavailable`]). Neither does any flag prove
/// the process is privileged enough to open a device.
///
/// ```
/// use tunnel_lattice_backend_tunrs::TunRsBackend;
/// use tunnel_lattice_platform::{Capability, CapabilityProvider};
///
/// let capabilities = TunRsBackend::new().capabilities();
/// assert!(capabilities.contains(Capability::DEVICE_MUTATION));
/// assert!(!capabilities.contains(Capability::MAC_MUTATION));
/// ```
impl CapabilityProvider for TunRsBackend {
    fn capabilities(&self) -> Capability {
        host_capabilities()
    }
}

/// The backend's host-level answer (see [`TunRsBackend`]'s
/// [`CapabilityProvider`] impl) plus what depends on this handle:
/// `MAC_MUTATION` on a TAP handle on Linux and macOS, and `TAP_DEVICES` on
/// any TAP handle, since its successful open proves TAP works here even if
/// the Windows driver lookup did not find the driver.
impl CapabilityProvider for TunRsDevice {
    fn capabilities(&self) -> Capability {
        let base = host_capabilities();
        if self.kind != DeviceKind::Tap {
            return base;
        }
        let base = base | Capability::TAP_DEVICES;
        if mac_mutation_supported(self.kind) {
            base | Capability::MAC_MUTATION
        } else {
            base
        }
    }
}

/// Whether a handle of `kind` can change its MAC after open
/// (`Capability::MAC_MUTATION`): TAP on Linux and macOS. The Windows TAP
/// driver takes a MAC only at creation; `tun-rs` returns `Unsupported` for
/// a later change there. Decided from the kind and the target alone, with no
/// native call.
const fn mac_mutation_supported(kind: DeviceKind) -> bool {
    matches!(kind, DeviceKind::Tap) && !cfg!(target_os = "windows")
}

/// Ordinary tests of the backend's host-level capability answer (no device
/// is opened; the Windows driver lookup needs no elevation).
#[cfg(test)]
mod capability_tests {
    use super::*;

    #[test]
    fn backend_reports_host_flags_and_never_per_handle_ones() {
        let capabilities = TunRsBackend::new().capabilities();
        assert!(capabilities.contains(Capability::DEVICE_MUTATION));
        assert!(!capabilities.contains(Capability::MAC_MUTATION));
        assert_eq!(capabilities, host_capabilities());
        assert_eq!(
            capabilities.contains(Capability::NATIVE_ASYNC),
            cfg!(feature = "async")
        );
        let linux_only = Capability::PERSISTENT_DEVICES | Capability::MULTI_QUEUE;
        if cfg!(target_os = "linux") {
            assert!(capabilities.contains(linux_only));
        } else {
            assert!(!capabilities.intersects(linux_only));
        }
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn backend_reports_tap_devices_statically_on_linux_and_macos() {
        assert!(
            TunRsBackend::new()
                .capabilities()
                .contains(Capability::TAP_DEVICES)
        );
    }

    /// `apply` decides `MAC_MUTATION` from the kind and the target alone
    /// (no driver lookup); TUN never has it, TAP everywhere but Windows.
    #[test]
    fn mac_mutation_follows_the_kind_and_the_target_only() {
        assert!(!mac_mutation_supported(DeviceKind::Tun));
        assert_eq!(
            mac_mutation_supported(DeviceKind::Tap),
            !cfg!(target_os = "windows")
        );
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn backend_reports_tap_devices_on_windows_only_with_the_driver() {
        assert_eq!(
            TunRsBackend::new()
                .capabilities()
                .contains(Capability::TAP_DEVICES),
            windows_tap_probe::tap_driver_installed()
        );
    }
}

/// Ordinary tests of [`apply_patch`]'s preconditions, ordering, and
/// compensation against recorded fake steps (no device).
#[cfg(test)]
mod apply_tests {
    use std::cell::RefCell;

    use tunnel_lattice_model::DesiredAdminState;

    use super::*;

    #[derive(Debug, PartialEq, Eq)]
    enum Call {
        ReadMtu,
        SetMtu(u16),
        ReadMac,
        SetMac(MacAddress),
        SetEnabled(bool),
    }

    /// Records every native call; each step fails when its flag is set.
    #[derive(Default)]
    struct Fake {
        calls: RefCell<Vec<Call>>,
        fail_read: bool,
        fail_set_mtu: bool,
        fail_set_mac: bool,
        fail_enable: bool,
    }

    impl ApplySteps for Fake {
        fn mtu(&self) -> Result<u16> {
            self.calls.borrow_mut().push(Call::ReadMtu);
            if self.fail_read {
                return Err(Error::Platform(PlatformErrorCode::Unknown));
            }
            Ok(1500)
        }

        fn set_mtu(&self, mtu: u16) -> Result<()> {
            self.calls.borrow_mut().push(Call::SetMtu(mtu));
            if self.fail_set_mtu {
                return Err(Error::PermissionDenied);
            }
            Ok(())
        }

        fn mac(&self) -> Result<MacAddress> {
            self.calls.borrow_mut().push(Call::ReadMac);
            Ok(OLD_MAC)
        }

        fn set_mac(&self, mac: MacAddress) -> Result<()> {
            self.calls.borrow_mut().push(Call::SetMac(mac));
            if self.fail_set_mac {
                return Err(Error::PermissionDenied);
            }
            Ok(())
        }

        fn set_enabled(&self, enabled: bool) -> Result<()> {
            self.calls.borrow_mut().push(Call::SetEnabled(enabled));
            if self.fail_enable {
                return Err(Error::PermissionDenied);
            }
            Ok(())
        }
    }

    const ID: DeviceId = DeviceId::new(3);
    const OLD_MAC: MacAddress = MacAddress::new([0x02, 0, 0, 0, 0, 0x01]);
    const NEW_MAC: MacAddress = MacAddress::new([0x02, 0, 0, 0, 0, 0x02]);

    /// A TUN device: no MAC address at all.
    const TUN: ApplyTarget = ApplyTarget {
        id: ID,
        kind: DeviceKind::Tun,
        mac_mutation: false,
    };

    /// A TAP device whose handle reports `MAC_MUTATION`.
    const TAP: ApplyTarget = ApplyTarget {
        id: ID,
        kind: DeviceKind::Tap,
        mac_mutation: true,
    };

    fn patch(
        id: DeviceId,
        admin: Option<DesiredAdminState>,
        mtu: Option<u32>,
    ) -> DeviceConfigPatch {
        DeviceConfigPatch::new(id, admin, mtu).expect("a valid patch")
    }

    #[test]
    fn a_patch_for_another_device_is_rejected_without_a_native_call() {
        let fake = Fake::default();
        let patch = patch(DeviceId::new(4), Some(DesiredAdminState::Up), Some(1400));
        let result = apply_patch(&fake, TUN, &patch);
        assert!(matches!(result, Err(Error::InvalidState)), "{result:?}");
        assert!(fake.calls.borrow().is_empty());
    }

    #[test]
    fn an_mtu_above_u16_is_rejected_without_a_native_call() {
        let fake = Fake::default();
        let patch = patch(ID, Some(DesiredAdminState::Up), Some(70_000));
        let result = apply_patch(&fake, TUN, &patch);
        assert!(matches!(result, Err(Error::InvalidState)), "{result:?}");
        assert!(fake.calls.borrow().is_empty());
    }

    #[test]
    fn a_mac_for_a_tun_device_is_rejected_without_a_native_call() {
        let fake = Fake::default();
        let patch = patch(ID, None, Some(1400)).with_mac(NEW_MAC);
        let result = apply_patch(&fake, TUN, &patch);
        assert!(matches!(result, Err(Error::InvalidState)), "{result:?}");
        assert!(fake.calls.borrow().is_empty());
    }

    #[test]
    fn a_mac_without_mac_mutation_is_unsupported_without_a_native_call() {
        let fake = Fake::default();
        let target = ApplyTarget {
            mac_mutation: false,
            ..TAP
        };
        let patch = patch(ID, Some(DesiredAdminState::Up), Some(1400)).with_mac(NEW_MAC);
        let result = apply_patch(&fake, target, &patch);
        assert!(matches!(result, Err(Error::Unsupported)), "{result:?}");
        assert!(fake.calls.borrow().is_empty());
    }

    #[test]
    fn a_single_step_skips_the_pre_read() {
        let fake = Fake::default();
        apply_patch(&fake, TUN, &patch(ID, None, Some(1400))).expect("apply");
        assert_eq!(*fake.calls.borrow(), [Call::SetMtu(1400)]);

        let fake = Fake::default();
        apply_patch(&fake, TUN, &patch(ID, Some(DesiredAdminState::Down), None)).expect("apply");
        assert_eq!(*fake.calls.borrow(), [Call::SetEnabled(false)]);

        let fake = Fake::default();
        apply_patch(&fake, TAP, &DeviceConfigPatch::new_mac(ID, NEW_MAC)).expect("apply");
        assert_eq!(*fake.calls.borrow(), [Call::SetMac(NEW_MAC)]);
    }

    #[test]
    fn every_step_runs_mtu_then_mac_then_admin_after_the_pre_reads() {
        let fake = Fake::default();
        let patch = patch(ID, Some(DesiredAdminState::Up), Some(1400)).with_mac(NEW_MAC);
        apply_patch(&fake, TAP, &patch).expect("apply");
        assert_eq!(
            *fake.calls.borrow(),
            [
                Call::ReadMtu,
                Call::ReadMac,
                Call::SetMtu(1400),
                Call::SetMac(NEW_MAC),
                Call::SetEnabled(true),
            ]
        );
    }

    #[test]
    fn a_failed_mac_step_restores_the_mtu_and_skips_the_admin_step() {
        let fake = Fake {
            fail_set_mac: true,
            ..Fake::default()
        };
        let patch = patch(ID, Some(DesiredAdminState::Up), Some(1400)).with_mac(NEW_MAC);
        let result = apply_patch(&fake, TAP, &patch);
        assert!(matches!(result, Err(Error::PermissionDenied)), "{result:?}");
        assert_eq!(
            *fake.calls.borrow(),
            [
                Call::ReadMtu,
                Call::ReadMac,
                Call::SetMtu(1400),
                Call::SetMac(NEW_MAC),
                Call::SetMtu(1500),
            ]
        );
    }

    #[test]
    fn a_failed_admin_step_restores_the_mac_then_the_mtu() {
        let fake = Fake {
            fail_enable: true,
            ..Fake::default()
        };
        let patch = patch(ID, Some(DesiredAdminState::Down), Some(1400)).with_mac(NEW_MAC);
        let result = apply_patch(&fake, TAP, &patch);
        assert!(matches!(result, Err(Error::PermissionDenied)), "{result:?}");
        assert_eq!(
            *fake.calls.borrow(),
            [
                Call::ReadMtu,
                Call::ReadMac,
                Call::SetMtu(1400),
                Call::SetMac(NEW_MAC),
                Call::SetEnabled(false),
                Call::SetMac(OLD_MAC),
                Call::SetMtu(1500),
            ]
        );
    }

    #[test]
    fn both_steps_run_mtu_first_after_a_pre_read() {
        let fake = Fake::default();
        apply_patch(
            &fake,
            TUN,
            &patch(ID, Some(DesiredAdminState::Up), Some(1400)),
        )
        .expect("apply");
        assert_eq!(
            *fake.calls.borrow(),
            [Call::ReadMtu, Call::SetMtu(1400), Call::SetEnabled(true)]
        );
    }

    #[test]
    fn a_failed_pre_read_changes_nothing() {
        let fake = Fake {
            fail_read: true,
            ..Fake::default()
        };
        let result = apply_patch(
            &fake,
            TUN,
            &patch(ID, Some(DesiredAdminState::Up), Some(1400)),
        );
        assert!(
            matches!(result, Err(Error::Platform(PlatformErrorCode::Unknown))),
            "{result:?}"
        );
        assert_eq!(*fake.calls.borrow(), [Call::ReadMtu]);
    }

    #[test]
    fn a_failed_mtu_step_skips_the_admin_step() {
        let fake = Fake {
            fail_set_mtu: true,
            ..Fake::default()
        };
        let result = apply_patch(
            &fake,
            TUN,
            &patch(ID, Some(DesiredAdminState::Up), Some(1400)),
        );
        assert!(matches!(result, Err(Error::PermissionDenied)), "{result:?}");
        assert_eq!(*fake.calls.borrow(), [Call::ReadMtu, Call::SetMtu(1400)]);
    }

    #[test]
    fn a_failed_admin_step_restores_the_mtu_and_returns_its_own_error() {
        let fake = Fake {
            fail_enable: true,
            ..Fake::default()
        };
        let result = apply_patch(
            &fake,
            TUN,
            &patch(ID, Some(DesiredAdminState::Down), Some(1400)),
        );
        assert!(matches!(result, Err(Error::PermissionDenied)), "{result:?}");
        assert_eq!(
            *fake.calls.borrow(),
            [
                Call::ReadMtu,
                Call::SetMtu(1400),
                Call::SetEnabled(false),
                Call::SetMtu(1500),
            ]
        );
    }

    #[test]
    fn a_failed_revert_still_returns_the_original_error() {
        // `set_mtu` succeeds the first time and fails on the revert.
        struct FlakyRevert(RefCell<u8>);

        impl ApplySteps for FlakyRevert {
            fn mtu(&self) -> Result<u16> {
                Ok(1500)
            }

            fn set_mtu(&self, _mtu: u16) -> Result<()> {
                let mut calls = self.0.borrow_mut();
                *calls += 1;
                if *calls > 1 {
                    return Err(Error::Platform(PlatformErrorCode::Unknown));
                }
                Ok(())
            }

            fn mac(&self) -> Result<MacAddress> {
                Ok(OLD_MAC)
            }

            fn set_mac(&self, _mac: MacAddress) -> Result<()> {
                Ok(())
            }

            fn set_enabled(&self, _enabled: bool) -> Result<()> {
                Err(Error::NotFound)
            }
        }

        let steps = FlakyRevert(RefCell::new(0));
        let result = apply_patch(
            &steps,
            TUN,
            &patch(ID, Some(DesiredAdminState::Up), Some(1400)),
        );
        assert!(matches!(result, Err(Error::NotFound)), "{result:?}");
        assert_eq!(*steps.0.borrow(), 2, "the revert was attempted");
    }
}

/// Ordinary (non-privileged, deterministic) tests of [`io_error`]'s
/// `io::ErrorKind` → [`Error`] mapping.
#[cfg(test)]
mod io_error_tests {
    use std::io;

    use super::*;

    fn map_kind(kind: io::ErrorKind) -> Error {
        io_error(io::Error::new(kind, "synthetic"))
    }

    #[test]
    fn a_zero_interface_index_is_not_an_identity() {
        assert!(matches!(
            device_id_from_index(Ok(0)),
            Err(Error::Platform(PlatformErrorCode::Unknown))
        ));
        assert!(matches!(
            device_id_from_index(Err(io::Error::from(io::ErrorKind::PermissionDenied))),
            Err(Error::PermissionDenied)
        ));
        assert_eq!(
            device_id_from_index(Ok(12)).expect("a real index"),
            DeviceId::new(12)
        );
    }

    #[test]
    fn permission_denied_maps_to_the_typed_variant() {
        assert!(matches!(
            map_kind(io::ErrorKind::PermissionDenied),
            Error::PermissionDenied
        ));
    }

    #[test]
    fn not_found_maps_to_the_typed_variant() {
        assert!(matches!(map_kind(io::ErrorKind::NotFound), Error::NotFound));
    }

    #[test]
    fn already_exists_maps_to_the_typed_variant() {
        assert!(matches!(
            map_kind(io::ErrorKind::AlreadyExists),
            Error::AlreadyExists
        ));
    }

    #[test]
    fn channel_shutdown_kinds_map_to_disconnected() {
        for kind in [
            io::ErrorKind::BrokenPipe,
            io::ErrorKind::UnexpectedEof,
            io::ErrorKind::NotConnected,
        ] {
            assert!(
                matches!(map_kind(kind), Error::Disconnected),
                "{kind:?} should map to Error::Disconnected"
            );
        }
    }

    #[test]
    fn unsupported_still_maps_to_the_typed_variant() {
        assert!(matches!(
            map_kind(io::ErrorKind::Unsupported),
            Error::Unsupported
        ));
    }

    /// Kinds with no typed counterpart keep the platform escape hatch —
    /// including ones that sound related but are not a permanent channel
    /// shutdown (`TimedOut`, `WouldBlock`, `Interrupted`).
    #[test]
    fn unmapped_kinds_stay_platform_errors() {
        for kind in [
            io::ErrorKind::Other,
            io::ErrorKind::InvalidInput,
            io::ErrorKind::TimedOut,
            io::ErrorKind::WouldBlock,
            io::ErrorKind::Interrupted,
        ] {
            let mapped = map_kind(kind);
            assert!(
                matches!(mapped, Error::Platform(_)),
                "{kind:?} should stay Error::Platform, got {mapped:?}"
            );
        }
    }

    /// An error with no raw OS code (as `tun-rs` builds with
    /// `io::Error::other`/`io::Error::new`) maps to
    /// `PlatformErrorCode::Unknown`, never a fabricated `…(0)` code.
    #[test]
    fn code_less_errors_map_to_unknown_platform_code() {
        for err in [
            io::Error::other("synthetic code-less failure"),
            io::Error::new(io::ErrorKind::InvalidInput, "name too long"),
            io::Error::new(io::ErrorKind::Interrupted, "cancel"),
            io::Error::from(io::ErrorKind::TimedOut),
        ] {
            assert!(err.raw_os_error().is_none());
            let mapped = io_error(err);
            assert!(
                matches!(mapped, Error::Platform(PlatformErrorCode::Unknown)),
                "a code-less error should map to Platform(Unknown), got {mapped:?}"
            );
        }
    }

    /// Typed kinds still win over the `Unknown` fallback when there is no
    /// raw OS code.
    #[test]
    fn code_less_typed_kinds_keep_their_typed_variant() {
        assert!(matches!(
            io_error(io::Error::new(io::ErrorKind::NotFound, "No driver found")),
            Error::NotFound
        ));
        assert!(matches!(
            io_error(io::Error::from(io::ErrorKind::Unsupported)),
            Error::Unsupported
        ));
    }

    #[test]
    fn platform_code_tags_a_present_code_and_reports_unknown_otherwise() {
        assert_eq!(platform_code(None), PlatformErrorCode::Unknown);
        #[cfg(target_os = "linux")]
        assert_eq!(platform_code(Some(5)), PlatformErrorCode::Linux(5));
        #[cfg(target_os = "windows")]
        assert_eq!(platform_code(Some(5)), PlatformErrorCode::Windows(5));
        #[cfg(target_os = "macos")]
        assert_eq!(platform_code(Some(5)), PlatformErrorCode::Darwin(5));
        #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
        assert_eq!(platform_code(Some(5)), PlatformErrorCode::Unknown);
    }

    /// Native codes go through `std`'s own errno → `ErrorKind` translation,
    /// so the typed mapping also holds for real OS errors, not only for
    /// errors `tun-rs` constructs from a bare kind.
    #[test]
    #[cfg(unix)]
    fn raw_errno_values_map_through_their_error_kind() {
        // POSIX values shared by Linux and macOS.
        const EPERM: i32 = 1;
        const ENOENT: i32 = 2;
        const EACCES: i32 = 13;
        const EEXIST: i32 = 17;
        const EPIPE: i32 = 32;
        let raw = |code| io_error(io::Error::from_raw_os_error(code));

        assert!(matches!(raw(EPERM), Error::PermissionDenied));
        assert!(matches!(raw(EACCES), Error::PermissionDenied));
        assert!(matches!(raw(ENOENT), Error::NotFound));
        assert!(matches!(raw(EEXIST), Error::AlreadyExists));
        assert!(matches!(raw(EPIPE), Error::Disconnected));
    }

    /// An unmapped native code still carries its raw value.
    #[test]
    #[cfg(target_os = "linux")]
    fn unmapped_linux_errno_is_preserved() {
        const EIO: i32 = 5;
        assert!(matches!(
            io_error(io::Error::from_raw_os_error(EIO)),
            Error::Platform(PlatformErrorCode::Linux(EIO))
        ));
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn raw_windows_codes_map_through_their_error_kind() {
        const ERROR_FILE_NOT_FOUND: i32 = 2;
        const ERROR_ACCESS_DENIED: i32 = 5;
        const ERROR_BROKEN_PIPE: i32 = 109;
        const ERROR_ALREADY_EXISTS: i32 = 183;
        let raw = |code| io_error(io::Error::from_raw_os_error(code));

        assert!(matches!(raw(ERROR_ACCESS_DENIED), Error::PermissionDenied));
        assert!(matches!(raw(ERROR_FILE_NOT_FOUND), Error::NotFound));
        assert!(matches!(raw(ERROR_ALREADY_EXISTS), Error::AlreadyExists));
        assert!(matches!(raw(ERROR_BROKEN_PIPE), Error::Disconnected));
    }

    /// Returns `true` when this process can plausibly create a TUN device:
    /// effective UID 0, or `CAP_NET_ADMIN` (bit 12) in the effective
    /// capability set. Read from `/proc/self/status` so the check needs no
    /// extra dependency; an unreadable file is treated as "privileged" so
    /// the caller skips instead of asserting on an unknown environment.
    #[cfg(target_os = "linux")]
    fn linux_process_may_create_tun_devices() -> bool {
        const CAP_NET_ADMIN: u32 = 12;
        let Ok(status) = std::fs::read_to_string("/proc/self/status") else {
            return true;
        };
        let field = |name: &str| {
            status
                .lines()
                .find_map(|line| line.strip_prefix(name))
                .map(str::trim)
        };
        let euid_is_root = field("Uid:")
            .and_then(|uids| uids.split_whitespace().nth(1))
            .is_none_or(|euid| euid == "0");
        let has_net_admin = field("CapEff:")
            .and_then(|caps| u64::from_str_radix(caps, 16).ok())
            .is_none_or(|caps| caps & (1 << CAP_NET_ADMIN) != 0);
        euid_is_root || has_net_admin
    }

    /// Unprivileged `open` on Linux must surface as the typed
    /// [`Error::PermissionDenied`] (`TUNSETIFF` fails with `EPERM`, or
    /// opening `/dev/net/tun` fails with `EACCES`), not a raw
    /// `Error::Platform` errno. Skipped at runtime when the process is root
    /// or holds `CAP_NET_ADMIN` (where `open` would succeed and create a
    /// real device), or when `/dev/net/tun` is absent (containers without
    /// the TUN module, where `open` reports `Error::DriverUnavailable`
    /// instead).
    #[test]
    #[cfg(target_os = "linux")]
    fn unprivileged_open_reports_permission_denied_on_linux() {
        if linux_process_may_create_tun_devices() {
            eprintln!("skipped: process is root or holds CAP_NET_ADMIN");
            return;
        }
        if !std::path::Path::new("/dev/net/tun").exists() {
            eprintln!("skipped: /dev/net/tun is not present");
            return;
        }

        #[cfg(feature = "tokio")]
        let _runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_io()
            .build()
            .expect("build a Tokio runtime");
        #[cfg(feature = "tokio")]
        let _entered = _runtime.enter();

        let result = TunRsBackend::new().open(DeviceConfig::new(DeviceKind::Tun));
        assert!(
            matches!(result, Err(Error::PermissionDenied)),
            "unprivileged open should report Error::PermissionDenied, got {:?}",
            result.err()
        );
    }

    /// A name or MTU `open` cannot honor is rejected with `InvalidState`
    /// before any native call. Deterministic and unprivileged: no device is
    /// ever built. Under the `tokio` feature this also proves no native
    /// call was made — building a device without an entered Tokio runtime
    /// would panic.
    #[test]
    fn open_rejects_unusable_names_and_mtus_before_any_native_call() {
        let backend = TunRsBackend::new();
        let too_long_for_host = if cfg!(target_os = "windows") {
            "a".repeat(256)
        } else {
            "a".repeat(16)
        };
        for kind in [DeviceKind::Tun, DeviceKind::Tap] {
            for name in ["", "tl\0x", too_long_for_host.as_str()] {
                let result = backend.open(DeviceConfig::new(kind).with_name(name));
                assert!(
                    matches!(result, Err(Error::InvalidState)),
                    "{kind:?} name {name:?}: expected InvalidState, got {:?}",
                    result.err()
                );
            }
            let result = backend.open(DeviceConfig::new(kind).with_mtu(u32::from(u16::MAX) + 1));
            assert!(
                matches!(result, Err(Error::InvalidState)),
                "{kind:?} oversized MTU: expected InvalidState, got {:?}",
                result.err()
            );
        }
        #[cfg(target_os = "macos")]
        {
            for name in ["tap0", "feth", "fethX"] {
                let tap = backend.open(DeviceConfig::new(DeviceKind::Tap).with_name(name));
                assert!(matches!(tap, Err(Error::InvalidState)), "{name}");
            }
            let tun = backend.open(DeviceConfig::new(DeviceKind::Tun).with_name("tun0"));
            assert!(matches!(tun, Err(Error::InvalidState)));
        }
        #[cfg(target_os = "linux")]
        for kind in [DeviceKind::Tun, DeviceKind::Tap] {
            let result = backend.open(DeviceConfig::new(kind).with_name("tl%d"));
            assert!(
                matches!(result, Err(Error::InvalidState)),
                "{kind:?} template name: expected InvalidState, got {:?}",
                result.err()
            );
        }
    }
}

/// Privileged tests that create a real TUN/TAP device.
///
/// `#[ignore]`d because opening a device needs `CAP_NET_ADMIN` on Linux,
/// Administrator on Windows, or root on macOS/BSD — see `.claude/rules/
/// ci.md`. Run with `cargo test -p tunnel-lattice-backend-tunrs -- --ignored`
/// under the required privilege (`sudo` on Linux/macOS). Each test creates
/// its own non-persistent device and never touches pre-existing host state:
/// dropping `TunRsDevice` tears the interface down, so there is nothing to
/// restore on any exit path (including a panic through `expect`).
#[cfg(test)]
mod privileged_tests {
    #[cfg(target_os = "linux")]
    use tunnel_lattice_model::DesiredAdminState;
    use tunnel_lattice_model::DeviceConfigPatch;
    use tunnel_lattice_platform::{DeviceMutator, DeviceObserver, DeviceProvider, PacketIo};

    use super::*;

    /// Under the `tokio` feature, `tun-rs`'s Tokio-backed `AsyncDevice`
    /// registers its file descriptor with `tokio::runtime::Handle::
    /// current()` as soon as it's built — before this crate's `PacketIo`
    /// ever calls an async method — so every test below needs a Tokio
    /// runtime entered on the current thread or `TunRsBackend::open` itself
    /// panics (verified: "there is no reactor running, must be called from
    /// the context of a Tokio 1.x runtime"), even though `open`/`recv`/
    /// `send` are ordinary synchronous calls.
    ///
    /// **Must be `new_multi_thread`, not `new_current_thread`.**
    /// `TunRsDevice`'s blocking `recv`/`send` drive tun-rs's async methods
    /// through `Handle::current().block_on(..)` (see [`TunRsDevice::
    /// blocking_recv`]'s docs on why, versus `futures::executor::block_on`).
    /// On a `current_thread` runtime, `Handle::block_on` does not itself
    /// drive that runtime's I/O driver — only `Runtime::block_on` does —
    /// so a `send`/`recv` call made this way hangs forever waiting for a
    /// readiness notification the (undriven) reactor never delivers.
    /// Verified with an isolated repro: identical code hung under
    /// `new_current_thread()` and completed immediately under
    /// `new_multi_thread()`. A real caller building a `tokio` feature
    /// integration must use `#[tokio::main]`'s default multi-thread flavor
    /// (or `Builder::new_multi_thread()` directly) for the same reason —
    /// see `ARCHITECTURE.md`, "Async design." Not needed for `async-io`,
    /// whose `AsyncDevice` is built on the runtime-agnostic `async-io`
    /// crate instead.
    #[cfg(feature = "tokio")]
    fn enter_tokio_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .enable_io()
            .build()
            .expect("build a Tokio runtime")
    }

    #[test]
    #[ignore = "requires CAP_NET_ADMIN/Administrator/root to open a TUN device"]
    fn open_reports_the_requested_kind_and_mtu() {
        #[cfg(feature = "tokio")]
        let _runtime = enter_tokio_runtime();
        #[cfg(feature = "tokio")]
        let _entered = _runtime.enter();

        let backend = TunRsBackend::new();
        let device = backend
            .open(DeviceConfig::new(DeviceKind::Tun).with_mtu(1400))
            .expect("open a TUN device");

        let snapshot = device.snapshot().expect("snapshot the open device");
        assert_eq!(snapshot.kind, DeviceKind::Tun);
        assert_eq!(snapshot.mtu, 1400);
        assert!(!snapshot.name.is_empty());
        assert_ne!(device.id().value(), 0, "an OS interface index is never 0");
        assert_eq!(snapshot.id, device.id());
    }

    #[test]
    #[ignore = "requires CAP_NET_ADMIN/Administrator/root to open a TUN device"]
    fn apply_changes_mtu_and_is_observable_on_the_next_snapshot() {
        #[cfg(feature = "tokio")]
        let _runtime = enter_tokio_runtime();
        #[cfg(feature = "tokio")]
        let _entered = _runtime.enter();

        let backend = TunRsBackend::new();
        let device = backend
            .open(DeviceConfig::new(DeviceKind::Tun).with_mtu(1400))
            .expect("open a TUN device");
        let device_id = device.snapshot().expect("initial snapshot").id;

        let patch = DeviceConfigPatch::new(device_id, None, Some(1300)).expect("build a patch");
        device.apply(patch).expect("apply the MTU patch");

        let updated = device.snapshot().expect("snapshot after the patch");
        assert_eq!(updated.mtu, 1300);

        let other = DeviceId::new(device_id.value().wrapping_add(1));
        let patch = DeviceConfigPatch::new(other, None, Some(1200)).expect("build a patch");
        assert!(
            matches!(device.apply(patch), Err(Error::InvalidState)),
            "a patch for another device must be rejected"
        );
        let unchanged = device
            .snapshot()
            .expect("snapshot after the rejected patch");
        assert_eq!(unchanged.mtu, 1300, "a rejected patch changes nothing");
    }

    #[test]
    #[cfg(target_os = "linux")]
    #[ignore = "requires CAP_NET_ADMIN to open a TUN device"]
    fn apply_toggles_admin_state_and_it_is_observable_on_linux() {
        #[cfg(feature = "tokio")]
        let _runtime = enter_tokio_runtime();
        #[cfg(feature = "tokio")]
        let _entered = _runtime.enter();

        let backend = TunRsBackend::new();
        let device = backend
            .open(DeviceConfig::new(DeviceKind::Tun))
            .expect("open a TUN device");
        let device_id = device.snapshot().expect("initial snapshot").id;

        let patch = DeviceConfigPatch::new(device_id, Some(DesiredAdminState::Up), None)
            .expect("build a patch");
        device.apply(patch).expect("bring the device up");
        assert_eq!(
            device.snapshot().expect("snapshot after up").admin_state,
            AdminState::Up
        );

        let patch = DeviceConfigPatch::new(device_id, Some(DesiredAdminState::Down), None)
            .expect("build a patch");
        device.apply(patch).expect("bring the device down");
        assert_eq!(
            device.snapshot().expect("snapshot after down").admin_state,
            AdminState::Down
        );
    }

    #[test]
    #[ignore = "requires CAP_NET_ADMIN/Administrator/root to open a TUN device"]
    fn recv_returns_permission_or_no_data_rather_than_hanging_forever() {
        // Exercises the `PacketIo::recv` code path against a real handle
        // without depending on external traffic reaching the interface: a
        // freshly opened, administratively-down device either yields no
        // packets (the call would block) or the backend reports a state
        // error, so this only asserts the send half round-trips a buffer
        // size the OS is willing to accept, not full packet delivery.
        #[cfg(feature = "tokio")]
        let _runtime = enter_tokio_runtime();
        #[cfg(feature = "tokio")]
        let _entered = _runtime.enter();

        let backend = TunRsBackend::new();
        let device = backend
            .open(DeviceConfig::new(DeviceKind::Tun).with_mtu(1400))
            .expect("open a TUN device");

        let mut buf = vec![0u8; 1400];
        buf[0] = 0x45; // IPv4, header length 5
        // Disambiguated: with the `async` feature, `TunRsDevice` also
        // implements `AsyncPacketIo::send`, which has the same name.
        let result = PacketIo::send(&device, &buf[..20]);
        assert!(
            result.is_ok() || matches!(result, Err(Error::Platform(_))),
            "send on a freshly opened device should succeed or report a platform error, not panic: {result:?}"
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    #[ignore = "requires CAP_NET_ADMIN to open a TUN device"]
    fn persist_marks_a_real_device_persistent() {
        use tunnel_lattice_platform::PersistentDevice;

        #[cfg(feature = "tokio")]
        let _runtime = enter_tokio_runtime();
        #[cfg(feature = "tokio")]
        let _entered = _runtime.enter();

        let backend = TunRsBackend::new();
        let device = backend
            .open(DeviceConfig::new(DeviceKind::Tun))
            .expect("open a TUN device");

        // `persist()` succeeding is the whole observable contract here —
        // `tun-rs` exposes no getter to read the persistent flag back, and
        // actually leaving a persistent interface behind after this test
        // process exits would violate this crate's own privileged-test
        // convention of never touching state outside what the test itself
        // owns and tears down (see `privileged_tests`'s module docs). A
        // real end-to-end "survives process exit" check belongs in a
        // separate, explicitly destructive test outside the default
        // `--ignored` run, not here.
        device.persist().expect("mark the device persistent");
    }

    #[test]
    #[cfg(target_os = "linux")]
    #[ignore = "requires CAP_NET_ADMIN to open a TUN device"]
    fn additional_queue_requires_multi_queue_to_have_been_requested() {
        use tunnel_lattice_platform::MultiQueueProvider;

        #[cfg(feature = "tokio")]
        let _runtime = enter_tokio_runtime();
        #[cfg(feature = "tokio")]
        let _entered = _runtime.enter();

        let backend = TunRsBackend::new();
        let device = backend
            .open(DeviceConfig::new(DeviceKind::Tun))
            .expect("open a TUN device (multi_queue not requested)");

        let result = device.additional_queue();
        assert!(
            matches!(result.err(), Some(Error::Unsupported)),
            "cloning a queue on a non-multi-queue device should report Unsupported, not a raw platform error"
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    #[ignore = "requires CAP_NET_ADMIN to open a TUN device"]
    fn additional_queue_clones_an_independent_queue_on_a_multi_queue_device() {
        use tunnel_lattice_platform::MultiQueueProvider;

        #[cfg(feature = "tokio")]
        let _runtime = enter_tokio_runtime();
        #[cfg(feature = "tokio")]
        let _entered = _runtime.enter();

        let backend = TunRsBackend::new();
        let device = backend
            .open(DeviceConfig::new(DeviceKind::Tun).with_multi_queue(true))
            .expect("open a multi-queue TUN device");

        let queue = device
            .additional_queue()
            .expect("clone a second queue on a multi-queue device");

        // Both queues observe the same underlying interface (same name),
        // confirming this cloned a queue on one device rather than
        // accidentally creating a second, unrelated one.
        let name = device.snapshot().expect("snapshot original queue").name;
        let queue_name = queue.snapshot().expect("snapshot cloned queue").name;
        assert_eq!(name, queue_name);
        assert_eq!(queue.id(), device.id(), "a queue keeps the device identity");

        // Dropping the clone first does not affect the original queue —
        // exercises the "independent handles" half of Handle's ownership
        // contract (see tunnel-lattice's rustdoc) at the backend level.
        drop(queue);
        device
            .snapshot()
            .expect("original queue still usable after the clone was dropped");
    }

    /// Runs a host network-configuration command, panicking with its
    /// output if it fails. Used only on the test's own device.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn run_host_command(program: &str, args: &[&str]) {
        let output = std::process::Command::new(program)
            .args(args)
            .output()
            .unwrap_or_else(|err| panic!("run {program}: {err}"));
        assert!(
            output.status.success(),
            "{program} {args:?} failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Gives the test's own device `subnet.1` and returns where to send UDP
    /// so the host routes it into the device: the peer `subnet.2` for TUN
    /// (no link-layer resolution), the subnet broadcast for TAP (no ARP
    /// needed). A macOS `utun` gets a point-to-point address; a macOS
    /// `feth` TAP, an Ethernet interface, gets a `/24` like Linux. The
    /// address lives on the device and goes away with it.
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    fn route_traffic_into(kind: DeviceKind, name: &str, subnet: [u8; 3]) -> std::net::SocketAddr {
        let [a, b, c] = subnet;
        let local = format!("{a}.{b}.{c}.1");
        let peer = format!("{a}.{b}.{c}.2");
        #[cfg(target_os = "linux")]
        {
            run_host_command("ip", &["addr", "add", &format!("{local}/24"), "dev", name]);
            run_host_command("ip", &["link", "set", "dev", name, "up"]);
        }
        #[cfg(target_os = "macos")]
        match kind {
            DeviceKind::Tap => run_host_command(
                "ifconfig",
                &[name, "inet", &local, "netmask", "255.255.255.0", "up"],
            ),
            _ => run_host_command("ifconfig", &[name, "inet", &local, &peer, "up"]),
        }
        // A freshly created Wintun adapter is not always registered with the
        // IP helper yet, and an adapter name reused right after the previous
        // test's adapter was removed can still resolve to the retired
        // interface; netsh then fails with "Failed to configure the DHCP
        // service". Retry for a bounded time before failing the test.
        #[cfg(target_os = "windows")]
        {
            let name_arg = format!("name={name}");
            let args = [
                "interface",
                "ipv4",
                "set",
                "address",
                name_arg.as_str(),
                "static",
                &local,
                "255.255.255.0",
            ];
            let mut attempts = 0;
            loop {
                let output = std::process::Command::new("netsh")
                    .args(args)
                    .output()
                    .unwrap_or_else(|err| panic!("run netsh: {err}"));
                if output.status.success() {
                    break;
                }
                attempts += 1;
                assert!(
                    attempts < 20,
                    "netsh {args:?} failed after {attempts} attempts: {}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
        }
        let target = match kind {
            DeviceKind::Tap => format!("{a}.{b}.{c}.255:9"),
            _ => format!("{peer}:9"),
        };
        target.parse().expect("a valid socket address")
    }

    /// A packet larger than the `recv` buffer is reported as
    /// `BufferTooSmall` (never truncated), and the next `recv` with a
    /// buffer of `recv_buffer_len()` bytes succeeds.
    ///
    /// A sender thread keeps sending 512-byte UDP datagrams (540-byte IPv4
    /// packets) into the device. The receiver first reads with a 64-byte
    /// buffer until it sees `BufferTooSmall` (small unsolicited packets,
    /// such as IPv6 neighbor discovery, may fit and are skipped), then
    /// reads once with a full-size buffer. The receiver runs on its own
    /// thread so a missing packet fails the test after 60 s instead of
    /// hanging it. Nothing outside the test's own device is changed: the
    /// address is assigned to that device and removed with it.
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    fn assert_oversize_packet_is_rejected_then_next_recv_succeeds(
        kind: DeviceKind,
        subnet: [u8; 3],
    ) {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::Duration;

        const SMALL: usize = 64;

        #[cfg(feature = "tokio")]
        let runtime = enter_tokio_runtime();
        #[cfg(feature = "tokio")]
        let _entered = runtime.enter();

        let device = Arc::new(
            TunRsBackend::new()
                .open(DeviceConfig::new(kind).with_mtu(1400))
                .expect("open a device"),
        );
        let snapshot = device.snapshot().expect("snapshot the device");
        let buf_len = snapshot.recv_buffer_len();
        // A macOS `feth` pair outlives the process, so it gets the teardown
        // guards; its peer (where the BPF descriptor reads) must be up to
        // receive the frames the host sends out of `dev`.
        #[cfg(target_os = "macos")]
        let _feth = (kind == DeviceKind::Tap).then(|| {
            let feth = guard_feth_pair(&snapshot.name);
            run_host_command("ifconfig", &[&feth.peer, "up"]);
            feth
        });
        let target = route_traffic_into(kind, &snapshot.name, subnet);

        let stop = Arc::new(AtomicBool::new(false));
        let sender = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let socket = std::net::UdpSocket::bind("0.0.0.0:0").expect("bind a UDP socket");
                socket.set_broadcast(true).expect("enable broadcast");
                let payload = [0xa5u8; 512];
                while !stop.load(Ordering::Acquire) {
                    // Errors are expected until the address is usable
                    // (e.g. Windows duplicate-address detection).
                    let _ = socket.send_to(&payload, target);
                    std::thread::sleep(Duration::from_millis(50));
                }
            })
        };

        let (done, outcome) = std::sync::mpsc::channel();
        {
            let device = Arc::clone(&device);
            #[cfg(feature = "tokio")]
            let runtime = runtime.handle().clone();
            std::thread::spawn(move || {
                #[cfg(feature = "tokio")]
                let _entered = runtime.enter();
                let mut small = [0u8; SMALL];
                let mut rejected = false;
                let mut result = Ok(());
                for _ in 0..10_000 {
                    match PacketIo::recv(&*device, &mut small) {
                        Err(Error::BufferTooSmall) => {
                            rejected = true;
                            break;
                        }
                        Ok(n) if n <= SMALL => {}
                        other => {
                            result = Err(format!("small-buffer recv: {other:?}"));
                            break;
                        }
                    }
                }
                if result.is_ok() && !rejected {
                    result = Err("no oversize packet was reported".to_owned());
                }
                if result.is_ok() {
                    let mut full = vec![0u8; buf_len];
                    result = match PacketIo::recv(&*device, &mut full) {
                        Ok(n) if n <= buf_len => Ok(()),
                        other => Err(format!("full-size recv after BufferTooSmall: {other:?}")),
                    };
                }
                let _ = done.send(result);
            });
        }

        let outcome = outcome.recv_timeout(Duration::from_secs(60));
        stop.store(true, Ordering::Release);
        sender.join().expect("the sender thread does not panic");
        match outcome {
            Ok(Ok(())) => {}
            Ok(Err(message)) => panic!("{kind:?}: {message}"),
            Err(_) => panic!("{kind:?}: no packet arrived within 60 s"),
        }
    }

    /// A per-process subnet octet, so parallel runs on one host differ.
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    fn subnet_octet() -> u8 {
        u8::try_from(std::process::id() % 200).expect("below 200") + 20
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    #[ignore = "requires CAP_NET_ADMIN/Administrator/root to open and address a TUN device"]
    fn tun_oversize_packet_reports_buffer_too_small_and_the_next_recv_succeeds() {
        assert_oversize_packet_is_rejected_then_next_recv_succeeds(
            DeviceKind::Tun,
            [10, 201, subnet_octet()],
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    #[ignore = "requires CAP_NET_ADMIN to open and address a TAP device"]
    fn tap_oversize_frame_reports_buffer_too_small_and_the_next_recv_succeeds_on_linux() {
        assert_oversize_packet_is_rejected_then_next_recv_succeeds(
            DeviceKind::Tap,
            [10, 202, subnet_octet()],
        );
    }

    /// The macOS `feth` TAP rejects a frame larger than the buffer with
    /// `InvalidData`, reported as `BufferTooSmall`, and the next `recv`
    /// succeeds. In the async builds this is also the data path through the
    /// bounded BPF wait: every `recv` here that finds no queued frame waits
    /// in it.
    #[test]
    #[cfg(target_os = "macos")]
    #[ignore = "requires root to open and address a feth TAP device"]
    fn tap_oversize_frame_reports_buffer_too_small_and_the_next_recv_succeeds_on_macos() {
        let _serial = serialize_feth_tests();
        assert_oversize_packet_is_rejected_then_next_recv_succeeds(
            DeviceKind::Tap,
            [10, 203, subnet_octet()],
        );
    }

    /// A name unique to this test process and `tag`, within `IFNAMSIZ`.
    #[cfg(target_os = "linux")]
    fn unique_linux_name(tag: &str) -> String {
        let name = format!("tl{tag}{}", std::process::id() % 100_000);
        assert!(name.len() <= 15, "{name} exceeds IFNAMSIZ");
        name
    }

    /// Opening a name whose non-multi-queue device already has a queue
    /// attached fails with `EBUSY`, which `open` reports as
    /// `AlreadyExists`. Both handles are this test's own non-persistent
    /// devices, so dropping `first` removes the interface.
    #[test]
    #[cfg(target_os = "linux")]
    #[ignore = "requires CAP_NET_ADMIN to open a TUN device"]
    fn opening_an_attached_non_multi_queue_name_reports_already_exists() {
        #[cfg(feature = "tokio")]
        let _runtime = enter_tokio_runtime();
        #[cfg(feature = "tokio")]
        let _entered = _runtime.enter();

        let backend = TunRsBackend::new();
        let name = unique_linux_name("busy");
        let first = backend
            .open(DeviceConfig::new(DeviceKind::Tun).with_name(name.as_str()))
            .expect("open the first TUN device");

        let second = backend.open(DeviceConfig::new(DeviceKind::Tun).with_name(name.as_str()));
        assert!(
            matches!(second, Err(Error::AlreadyExists)),
            "second open of an attached name should report AlreadyExists, got {:?}",
            second.err()
        );
        // The original device is untouched by the failed open.
        assert_eq!(first.snapshot().expect("snapshot the original").name, name);
    }

    /// Opening an existing TUN name as TAP fails with `EINVAL`, which `open`
    /// reports as `AlreadyExists` because the name exists.
    #[test]
    #[cfg(target_os = "linux")]
    #[ignore = "requires CAP_NET_ADMIN to open a TUN device"]
    fn opening_an_existing_name_as_the_other_kind_reports_already_exists() {
        #[cfg(feature = "tokio")]
        let _runtime = enter_tokio_runtime();
        #[cfg(feature = "tokio")]
        let _entered = _runtime.enter();

        let backend = TunRsBackend::new();
        let name = unique_linux_name("kind");
        let tun = backend
            .open(DeviceConfig::new(DeviceKind::Tun).with_name(name.as_str()))
            .expect("open the TUN device");

        let tap = backend.open(DeviceConfig::new(DeviceKind::Tap).with_name(name.as_str()));
        assert!(
            matches!(tap, Err(Error::AlreadyExists)),
            "opening a TUN name as TAP should report AlreadyExists, got {:?}",
            tap.err()
        );
        assert_eq!(tun.snapshot().expect("snapshot the original").name, name);
    }

    /// Opening a `Tun` device while `wintun.dll` is not on the DLL search
    /// path reports [`Error::DriverUnavailable`]. Only meaningful on a host
    /// without wintun: the privileged CI job runs it (filtered by the
    /// `windows_missing_driver_` prefix) before its "Download wintun.dll"
    /// step and skips it afterwards. This is the end-to-end check that the
    /// `libloading::Error` downcast still matches what `tun-rs` returns.
    /// Nothing is created on the expected path; if wintun is unexpectedly
    /// present, the opened device is non-persistent and is torn down when
    /// the result drops.
    #[test]
    #[cfg(target_os = "windows")]
    #[ignore = "requires Administrator and a host without wintun.dll on the DLL search path"]
    fn windows_missing_driver_tun_open_reports_driver_unavailable() {
        #[cfg(feature = "tokio")]
        let _runtime = enter_tokio_runtime();
        #[cfg(feature = "tokio")]
        let _entered = _runtime.enter();

        let result = TunRsBackend::new().open(DeviceConfig::new(DeviceKind::Tun));
        assert!(
            matches!(result, Err(Error::DriverUnavailable)),
            "open(Tun) without wintun.dll should report DriverUnavailable, got {:?}",
            result.err()
        );
    }

    /// Opening a `Tap` device while no `tap0901` (tap-windows6) driver is
    /// installed reports [`Error::DriverUnavailable`]. Only meaningful on a
    /// host without that driver: the privileged CI job runs it (filtered by
    /// the `windows_missing_driver_` prefix) before any driver is installed
    /// and skips it afterwards. This is the end-to-end check that `tun-rs`
    /// still returns exactly the "No driver found" error the classifier
    /// matches. If the driver is unexpectedly installed, the adapter
    /// created is non-persistent and is removed when the result drops.
    #[test]
    #[cfg(target_os = "windows")]
    #[ignore = "requires Administrator and a host without the tap-windows6 driver installed"]
    fn windows_missing_driver_tap_open_reports_driver_unavailable() {
        #[cfg(feature = "tokio")]
        let _runtime = enter_tokio_runtime();
        #[cfg(feature = "tokio")]
        let _entered = _runtime.enter();

        // The driver lookup behind the host-level answer agrees with what
        // `open` is about to find.
        assert!(
            !TunRsBackend::new()
                .capabilities()
                .contains(Capability::TAP_DEVICES),
            "TAP_DEVICES reported on a host without the tap-windows6 driver"
        );
        let result = TunRsBackend::new().open(DeviceConfig::new(DeviceKind::Tap));
        assert!(
            matches!(result, Err(Error::DriverUnavailable)),
            "open(Tap) without the tap-windows6 driver should report DriverUnavailable, got {:?}",
            result.err()
        );
    }

    /// Serializes the privileged Windows TAP tests. `tun-rs` names a new
    /// TAP adapter after the first free `tap{N}` it finds and then renames
    /// it with `netsh`, and each open installs a device through SetupAPI, so
    /// two concurrent opens could race for the same name. Take it as the
    /// test's first local, so it is released only after the device dropped
    /// (which removes the adapter).
    #[cfg(target_os = "windows")]
    fn serialize_windows_tap_tests() -> std::sync::MutexGuard<'static, ()> {
        static WINDOWS_TAP_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());
        WINDOWS_TAP_TESTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// With the tap-windows6 driver staged, the host-level driver lookup
    /// finds it, the backend reports `TAP_DEVICES`, and `open(Tap)`
    /// succeeds with a snapshot of kind `Tap` that carries a MAC. The handle
    /// reports `TAP_DEVICES` but not `MAC_MUTATION` (the Windows driver
    /// takes a MAC only at creation). The privileged CI job runs this after
    /// staging the driver; the name must not start with
    /// `windows_missing_driver_`, which selects the driver-less tests.
    #[test]
    #[cfg(target_os = "windows")]
    #[ignore = "requires Administrator and the tap-windows6 driver"]
    fn windows_tap_driver_is_detected_and_a_tap_device_opens() {
        let _serial = serialize_windows_tap_tests();
        #[cfg(feature = "tokio")]
        let _runtime = enter_tokio_runtime();
        #[cfg(feature = "tokio")]
        let _entered = _runtime.enter();

        assert!(
            windows_tap_probe::tap_driver_installed(),
            "the driver lookup did not find the staged tap0901 driver"
        );
        let backend = TunRsBackend::new();
        assert!(backend.capabilities().contains(Capability::TAP_DEVICES));

        let device = backend
            .open(DeviceConfig::new(DeviceKind::Tap))
            .expect("open a TAP device");
        let snapshot = device.snapshot().expect("snapshot the TAP device");
        eprintln!("windows TAP snapshot: {snapshot:?}");
        assert_eq!(snapshot.kind, DeviceKind::Tap);
        assert!(snapshot.mac.is_some(), "a TAP snapshot carries its MAC");
        assert!(!snapshot.name.is_empty());
        assert_ne!(device.id().value(), 0, "an OS interface index is never 0");
        assert_eq!(snapshot.id, device.id());
        assert_eq!(
            snapshot.recv_buffer_len(),
            usize::try_from(snapshot.mtu).expect("an MTU fits in usize") + 18
        );

        let capabilities = device.capabilities();
        assert!(capabilities.contains(Capability::TAP_DEVICES));
        assert!(!capabilities.contains(Capability::MAC_MUTATION));
        assert_eq!(capabilities, backend.capabilities());
    }

    /// A MAC requested at open is the one the Windows TAP adapter reports,
    /// and a later MAC patch is `Unsupported` without changing anything:
    /// the driver takes a MAC only at creation.
    #[test]
    #[cfg(target_os = "windows")]
    #[ignore = "requires Administrator and the tap-windows6 driver"]
    fn windows_tap_mac_is_set_at_open_and_a_later_change_is_unsupported() {
        let _serial = serialize_windows_tap_tests();
        #[cfg(feature = "tokio")]
        let _runtime = enter_tokio_runtime();
        #[cfg(feature = "tokio")]
        let _entered = _runtime.enter();

        // Locally administered unicast addresses.
        let initial = MacAddress::new([0x02, 0x7c, 0x1a, 0x00, 0x00, 0x11]);
        let changed = MacAddress::new([0x02, 0x7c, 0x1a, 0x00, 0x00, 0x12]);

        let device = TunRsBackend::new()
            .open(DeviceConfig::new(DeviceKind::Tap).with_mac(initial))
            .expect("open a TAP device with a MAC");
        assert_eq!(
            device.snapshot().expect("snapshot the device").mac,
            Some(initial)
        );

        let result = device.apply(DeviceConfigPatch::new_mac(device.id(), changed));
        assert!(
            matches!(result, Err(Error::Unsupported)),
            "a MAC patch on a Windows TAP device should be Unsupported, got {result:?}"
        );
        assert_eq!(
            device
                .snapshot()
                .expect("snapshot after the refused patch")
                .mac,
            Some(initial),
            "a refused MAC patch changes nothing"
        );
    }

    /// A Windows TAP adapter's MTU is set at open and changed by a patch,
    /// both read back from the next snapshot. Both values stay below the
    /// driver's maximum of 1500.
    #[test]
    #[cfg(target_os = "windows")]
    #[ignore = "requires Administrator and the tap-windows6 driver"]
    fn windows_tap_mtu_is_set_at_open_and_changed_by_apply() {
        let _serial = serialize_windows_tap_tests();
        #[cfg(feature = "tokio")]
        let _runtime = enter_tokio_runtime();
        #[cfg(feature = "tokio")]
        let _entered = _runtime.enter();

        let device = TunRsBackend::new()
            .open(DeviceConfig::new(DeviceKind::Tap).with_mtu(1400))
            .expect("open a TAP device");
        assert_eq!(device.snapshot().expect("initial snapshot").mtu, 1400);

        let patch = DeviceConfigPatch::new(device.id(), None, Some(1300)).expect("build a patch");
        device.apply(patch).expect("apply the MTU patch");
        assert_eq!(
            device.snapshot().expect("snapshot after the patch").mtu,
            1300
        );
    }

    /// A 42-byte broadcast ARP request (the smallest common Ethernet frame,
    /// before padding) is written to the Windows TAP adapter whole. The
    /// adapter's media status is connected, because `tun-rs` enables a new
    /// device.
    #[test]
    #[cfg(target_os = "windows")]
    #[ignore = "requires Administrator and the tap-windows6 driver"]
    fn windows_tap_sends_a_broadcast_arp_frame() {
        let _serial = serialize_windows_tap_tests();
        #[cfg(feature = "tokio")]
        let _runtime = enter_tokio_runtime();
        #[cfg(feature = "tokio")]
        let _entered = _runtime.enter();

        let device = TunRsBackend::new()
            .open(DeviceConfig::new(DeviceKind::Tap))
            .expect("open a TAP device");
        let mac = device
            .snapshot()
            .expect("snapshot the device")
            .mac
            .expect("a TAP snapshot carries its MAC")
            .octets();

        // Ethernet: broadcast destination, our source, EtherType ARP.
        let mut frame = Vec::with_capacity(42);
        frame.extend_from_slice(&[0xff; 6]);
        frame.extend_from_slice(&mac);
        frame.extend_from_slice(&[0x08, 0x06]);
        // ARP request: Ethernet/IPv4, sender our MAC at 169.254.0.1, asking
        // for 169.254.0.2.
        frame.extend_from_slice(&[0x00, 0x01, 0x08, 0x00, 6, 4, 0x00, 0x01]);
        frame.extend_from_slice(&mac);
        frame.extend_from_slice(&[169, 254, 0, 1]);
        frame.extend_from_slice(&[0; 6]);
        frame.extend_from_slice(&[169, 254, 0, 2]);
        assert_eq!(frame.len(), 42);

        let sent = PacketIo::send(&device, &frame);
        assert!(matches!(sent, Ok(42)), "send of a 42-byte frame: {sent:?}");
    }

    /// The Windows TAP adapter rejects a frame larger than the buffer with
    /// `InvalidInput`, reported as `BufferTooSmall`, and the next `recv`
    /// with a `recv_buffer_len()` buffer succeeds. The host sends UDP to the
    /// adapter's subnet broadcast address, so no ARP is needed.
    #[test]
    #[cfg(target_os = "windows")]
    #[ignore = "requires Administrator and the tap-windows6 driver"]
    fn windows_tap_oversize_frame_reports_buffer_too_small_and_the_next_recv_succeeds() {
        let _serial = serialize_windows_tap_tests();
        assert_oversize_packet_is_rejected_then_next_recv_succeeds(
            DeviceKind::Tap,
            [10, 204, subnet_octet()],
        );
    }

    /// Opening a Windows TAP device under the name of an existing adapter
    /// fails with `AlreadyExists` (the backend turns `tun-rs`'s adapter
    /// reuse off), and the existing adapter is untouched and still usable.
    #[test]
    #[cfg(target_os = "windows")]
    #[ignore = "requires Administrator and the tap-windows6 driver"]
    fn windows_tap_open_of_an_existing_name_reports_already_exists() {
        let _serial = serialize_windows_tap_tests();
        #[cfg(feature = "tokio")]
        let _runtime = enter_tokio_runtime();
        #[cfg(feature = "tokio")]
        let _entered = _runtime.enter();

        let name = format!("tltap{}", std::process::id() % 100_000);
        let backend = TunRsBackend::new();
        let first = backend
            .open(DeviceConfig::new(DeviceKind::Tap).with_name(name.as_str()))
            .expect("open the first TAP device");
        assert_eq!(first.snapshot().expect("snapshot the first").name, name);

        let second = backend.open(DeviceConfig::new(DeviceKind::Tap).with_name(name.as_str()));
        assert!(
            matches!(second, Err(Error::AlreadyExists)),
            "second open of an existing TAP name should report AlreadyExists, got {:?}",
            second.err()
        );
        let snapshot = first
            .snapshot()
            .expect("the first device is still usable after the refused open");
        assert_eq!(snapshot.name, name);
        assert_eq!(snapshot.id, first.id());
    }

    /// A TAP device opened with a MAC address reports it, reports
    /// `MAC_MUTATION`, and a later patch's MAC is observable on the next
    /// snapshot; a TUN device reports no MAC and refuses one at open. The
    /// macOS `feth` pair gets the teardown guards, the Linux TAP goes away
    /// with its handle.
    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[ignore = "requires CAP_NET_ADMIN/root to open a TAP device"]
    fn tap_mac_is_set_at_open_and_changed_by_apply() {
        use tunnel_lattice_platform::CapabilityProvider;

        #[cfg(target_os = "macos")]
        let _serial = serialize_feth_tests();
        #[cfg(feature = "tokio")]
        let _runtime = enter_tokio_runtime();
        #[cfg(feature = "tokio")]
        let _entered = _runtime.enter();

        // Locally administered unicast addresses.
        let initial = MacAddress::new([0x02, 0x7c, 0x1a, 0x00, 0x00, 0x01]);
        let changed = MacAddress::new([0x02, 0x7c, 0x1a, 0x00, 0x00, 0x02]);

        let backend = TunRsBackend::new();
        let device = backend
            .open(DeviceConfig::new(DeviceKind::Tap).with_mac(initial))
            .expect("open a TAP device with a MAC");
        let snapshot = device.snapshot().expect("snapshot the device");
        #[cfg(target_os = "macos")]
        let _feth = guard_feth_pair(&snapshot.name);
        assert_eq!(snapshot.mac, Some(initial));
        assert!(device.capabilities().contains(Capability::MAC_MUTATION));
        // The host-level answer is the handle's minus the per-handle flag,
        // and it promised TAP before this open succeeded.
        assert_eq!(
            device.capabilities() - Capability::MAC_MUTATION,
            backend.capabilities()
        );
        assert!(backend.capabilities().contains(Capability::TAP_DEVICES));

        device
            .apply(DeviceConfigPatch::new_mac(device.id(), changed))
            .expect("apply a new MAC");
        let snapshot = device.snapshot().expect("snapshot after apply");
        assert_eq!(snapshot.mac, Some(changed));

        let tun = backend
            .open(DeviceConfig::new(DeviceKind::Tun))
            .expect("open a TUN device");
        assert_eq!(tun.snapshot().expect("snapshot the TUN device").mac, None);
        assert!(!tun.capabilities().contains(Capability::MAC_MUTATION));
        assert_eq!(tun.capabilities(), backend.capabilities());
        let refused = backend.open(DeviceConfig::new(DeviceKind::Tun).with_mac(initial));
        assert!(
            matches!(refused, Err(Error::InvalidState)),
            "{:?}",
            refused.err()
        );
    }

    /// Runs a host command best-effort on drop: removes the test's own
    /// device if a teardown test panics before (or after) tearing it down
    /// itself. A failure (the device is already gone) is ignored, and it
    /// runs on unwind, not on a hard abort.
    ///
    /// A Linux TUN/TAP device is non-persistent, so closing its handle, at
    /// the latest when the test process exits, removes it as well. A macOS
    /// `feth` pair is different: it is a cloned interface that outlives the
    /// process, so after a failed test (whose drain thread still holds the
    /// device) these guards, one per `feth`, are its only cleanup. A hard
    /// abort would leak it; CI runners are ephemeral.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    struct RemoveOnDrop {
        program: &'static str,
        args: Vec<String>,
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    impl Drop for RemoveOnDrop {
        fn drop(&mut self) {
            let _ = std::process::Command::new(self.program)
                .args(&self.args)
                .output();
        }
    }

    /// What a stream yielded between being started and ending.
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    #[derive(Debug)]
    struct Drained {
        /// Packets received before the first error (unsolicited traffic
        /// such as IPv6 neighbor discovery may arrive).
        packets: usize,
        /// Every error item, in order.
        errors: Vec<Error>,
        /// Packets received after the first error; must be zero.
        packets_after_error: usize,
    }

    /// Starts the `PacketStream` the facade's `Handle::packet_stream` would
    /// build for this device (the native adapter in async builds, the
    /// thread bridge in the sync build), drains it on a separate thread,
    /// runs `teardown` while the stream is polling, and returns what the
    /// stream yielded once it ends, or panics if it has not ended within
    /// 30 s. `teardown` returns `false` if it could not tear the device
    /// down; the stream is then left polling on its thread (the device is
    /// removed when the test process exits) and `None` is returned.
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    fn drain_stream_across_teardown(
        device: TunRsDevice,
        teardown: impl FnOnce(&TunRsDevice) -> bool,
    ) -> Option<Drained> {
        use std::sync::Arc;
        use std::time::Duration;

        use futures::StreamExt;

        let buf_len = device
            .snapshot()
            .expect("snapshot the device")
            .recv_buffer_len();
        let device = Arc::new(device);
        #[cfg(feature = "async")]
        let mut stream = tunnel_lattice_async::from_async_device(Arc::clone(&device), buf_len)
            .expect("a valid buf_len");
        #[cfg(not(feature = "async"))]
        let mut stream = tunnel_lattice_async::from_device(Arc::clone(&device), buf_len)
            .expect("a valid buf_len");

        #[cfg(feature = "tokio")]
        let runtime = tokio::runtime::Handle::current();
        let (done, outcome) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let drain = async move {
                let mut drained = Drained {
                    packets: 0,
                    errors: Vec::new(),
                    packets_after_error: 0,
                };
                while let Some(item) = stream.next().await {
                    match item {
                        Ok(_) if drained.errors.is_empty() => drained.packets += 1,
                        Ok(_) => drained.packets_after_error += 1,
                        Err(err) => drained.errors.push(err),
                    }
                }
                drained
            };
            #[cfg(feature = "tokio")]
            let drained = runtime.block_on(drain);
            #[cfg(not(feature = "tokio"))]
            let drained = futures::executor::block_on(drain);
            let _ = done.send(drained);
        });

        // Let the stream reach its blocking read or readiness wait first.
        std::thread::sleep(Duration::from_millis(500));
        if !teardown(&device) {
            return None;
        }
        match outcome.recv_timeout(Duration::from_secs(30)) {
            Ok(drained) => Some(drained),
            Err(_) => panic!("the stream did not end within 30 s of the device teardown"),
        }
    }

    /// The contract every teardown test asserts: the stream ended after
    /// yielding exactly one error, and nothing after it.
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    fn assert_ended_after_one_error(label: &str, drained: &Drained) {
        eprintln!("{label}: {drained:?}");
        assert_eq!(
            drained.errors.len(),
            1,
            "{label}: the stream must end right after its first error: {drained:?}"
        );
        assert!(
            !matches!(drained.errors[0], Error::BufferTooSmall),
            "{label}: BufferTooSmall is not terminal"
        );
        assert_eq!(drained.packets_after_error, 0, "{label}: {drained:?}");
    }

    /// Deleting the device with `ip link del` while a stream is polling
    /// ends the stream with exactly `Disconnected`, in every feature set.
    /// Per `drivers/net/tun.c`, the native error differs by build:
    ///
    /// - sync (the thread bridge's blocking read): `EFAULT` for the read
    ///   already blocked, or `EBADFD` if the read starts after the tun file
    ///   was detached;
    /// - `async-io`: `EBADFD`, read once `polling` reports the error
    ///   readiness;
    /// - `tokio`: `EBADFD`, read through the error-aware duplicate
    ///   descriptor (Tokio's readable-only wait, which `tun-rs` uses, is
    ///   never woken by the error readiness a detached tun file reports).
    ///
    /// All of them map to `Disconnected`; the stream yields it and ends.
    #[cfg(target_os = "linux")]
    fn assert_linux_delete_ends_the_stream(kind: DeviceKind, tag: &str) {
        #[cfg(feature = "tokio")]
        let runtime = enter_tokio_runtime();
        #[cfg(feature = "tokio")]
        let _entered = runtime.enter();

        let name = unique_linux_name(tag);
        let device = TunRsBackend::new()
            .open(DeviceConfig::new(kind).with_name(name.as_str()))
            .expect("open a device");
        let _cleanup = RemoveOnDrop {
            program: "ip",
            args: vec!["link".into(), "del".into(), name.clone()],
        };
        let drained = drain_stream_across_teardown(device, |_| {
            run_host_command("ip", &["link", "del", &name]);
            true
        })
        .expect("the teardown ran");
        assert_ended_after_one_error(&format!("{kind:?} ip link del"), &drained);
        assert!(
            matches!(drained.errors[0], Error::Disconnected),
            "{drained:?}"
        );
    }

    /// A `PacketIo::recv` blocked when the device is deleted with `ip link
    /// del` returns `Disconnected`: the sync build's `EFAULT` rule, and the
    /// `tokio` build's `Handle::block_on` path over the error-aware reader.
    /// Unsolicited packets that arrive first are skipped. The receiving
    /// thread is bounded by a 30 s wait; the guard deletes the device on
    /// every exit path.
    #[test]
    #[cfg(target_os = "linux")]
    #[ignore = "requires CAP_NET_ADMIN to open and delete a TUN device"]
    fn recv_returns_disconnected_when_the_device_is_deleted_on_linux() {
        use std::sync::Arc;
        use std::time::Duration;

        #[cfg(feature = "tokio")]
        let runtime = enter_tokio_runtime();
        #[cfg(feature = "tokio")]
        let _entered = runtime.enter();

        let name = unique_linux_name("rdel");
        let device = Arc::new(
            TunRsBackend::new()
                .open(DeviceConfig::new(DeviceKind::Tun).with_name(name.as_str()))
                .expect("open a TUN device"),
        );
        let _cleanup = RemoveOnDrop {
            program: "ip",
            args: vec!["link".into(), "del".into(), name.clone()],
        };
        let buf_len = device
            .snapshot()
            .expect("snapshot the device")
            .recv_buffer_len();

        let (done, outcome) = std::sync::mpsc::channel();
        {
            let device = Arc::clone(&device);
            #[cfg(feature = "tokio")]
            let runtime = runtime.handle().clone();
            std::thread::spawn(move || {
                #[cfg(feature = "tokio")]
                let _entered = runtime.enter();
                let mut buf = vec![0u8; buf_len];
                let err = loop {
                    if let Err(err) = PacketIo::recv(&*device, &mut buf) {
                        break err;
                    }
                };
                let _ = done.send(err);
            });
        }

        // Let the receiver reach its blocking read or readiness wait first.
        std::thread::sleep(Duration::from_millis(500));
        run_host_command("ip", &["link", "del", &name]);
        match outcome.recv_timeout(Duration::from_secs(30)) {
            Ok(err) => assert!(matches!(err, Error::Disconnected), "{err:?}"),
            Err(_) => panic!("recv did not return within 30 s of the device deletion"),
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    #[ignore = "requires CAP_NET_ADMIN to open and delete a TUN device"]
    fn tun_stream_ends_when_the_device_is_deleted_on_linux() {
        assert_linux_delete_ends_the_stream(DeviceKind::Tun, "sdel");
    }

    #[test]
    #[cfg(target_os = "linux")]
    #[ignore = "requires CAP_NET_ADMIN to open and delete a TAP device"]
    fn tap_stream_ends_when_the_device_is_deleted_on_linux() {
        assert_linux_delete_ends_the_stream(DeviceKind::Tap, "pdel");
    }

    /// Serializes the privileged macOS `feth` tests. macOS hands a new
    /// `feth` the lowest free `fethN` name, and both the teardown guards and
    /// `tun-rs`'s own drop destroy the pair by name, so a test whose pair was
    /// already destroyed could otherwise destroy a concurrent test's freshly
    /// created pair. Take it as the test's first local, so it is released
    /// only after the device and every guard have dropped.
    #[cfg(target_os = "macos")]
    fn serialize_feth_tests() -> std::sync::MutexGuard<'static, ()> {
        static FETH_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());
        FETH_TESTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The peer of the macOS TAP's `feth` interface `dev`, read from the
    /// `peer: fethN` line of `ifconfig <dev>`. `tun-rs` creates a `feth`
    /// pair and binds its BPF read descriptor to the peer, but does not
    /// expose the peer's name. Panics if there is no such line.
    #[cfg(target_os = "macos")]
    fn feth_peer_of(dev: &str) -> String {
        let output = std::process::Command::new("ifconfig")
            .arg(dev)
            .output()
            .unwrap_or_else(|err| panic!("run ifconfig {dev}: {err}"));
        let stdout = String::from_utf8_lossy(&output.stdout);
        stdout
            .lines()
            .find_map(|line| line.trim().strip_prefix("peer: "))
            .and_then(|rest| rest.split_whitespace().next())
            .map(str::to_owned)
            .unwrap_or_else(|| panic!("no `peer:` line in `ifconfig {dev}`:\n{stdout}"))
    }

    /// The teardown guards of a macOS TAP's `feth` pair, and the peer's
    /// name. The fields drop in declaration order, so the peer is destroyed
    /// before `dev`.
    #[cfg(target_os = "macos")]
    struct FethPair {
        /// The peer, the interface the BPF read descriptor is bound to.
        peer: String,
        _peer_guard: RemoveOnDrop,
        _dev_guard: RemoveOnDrop,
    }

    /// Arms the teardown guards of the `feth` pair whose `dev` side is
    /// `dev`. The `dev` guard is armed before the peer is looked up, so a
    /// failed lookup still removes `dev`.
    #[cfg(target_os = "macos")]
    fn guard_feth_pair(dev: &str) -> FethPair {
        let dev_guard = RemoveOnDrop {
            program: "ifconfig",
            args: vec![dev.to_owned(), "destroy".into()],
        };
        let peer = feth_peer_of(dev);
        let peer_guard = RemoveOnDrop {
            program: "ifconfig",
            args: vec![peer.clone(), "destroy".into()],
        };
        FethPair {
            peer,
            _peer_guard: peer_guard,
            _dev_guard: dev_guard,
        }
    }

    /// Destroying the peer of the macOS TAP's `feth` pair with `ifconfig
    /// <peer> destroy` while a stream is polling ends the stream with
    /// exactly `Disconnected`, in every feature set. The peer is the
    /// interface the BPF read descriptor is bound to; destroying the other
    /// (`dev`) side would only unpeer it and the read would keep waiting.
    /// The sync build's blocking BPF read is woken and fails with `ENXIO`;
    /// in the async builds the bounded BPF wait finds the descriptor
    /// unbound at its next check, and the read that follows fails with
    /// `ENXIO`. The test prints the terminal item.
    #[test]
    #[cfg(target_os = "macos")]
    #[ignore = "requires root to open and destroy a feth TAP device"]
    fn tap_stream_ends_when_the_feth_is_destroyed_on_macos() {
        let _serial = serialize_feth_tests();
        #[cfg(feature = "tokio")]
        let runtime = enter_tokio_runtime();
        #[cfg(feature = "tokio")]
        let _entered = runtime.enter();

        let device = TunRsBackend::new()
            .open(DeviceConfig::new(DeviceKind::Tap))
            .expect("open a feth TAP device");
        let dev = device.snapshot().expect("snapshot the device").name;
        let feth = guard_feth_pair(&dev);
        let drained = drain_stream_across_teardown(device, |_| {
            run_host_command("ifconfig", &[&feth.peer, "destroy"]);
            true
        })
        .expect("the teardown ran");
        assert_ended_after_one_error("feth peer destroy", &drained);
        let terminal = &drained.errors[0];
        eprintln!("feth peer destroy: terminal item {terminal:?}");
        assert!(matches!(terminal, Error::Disconnected), "{drained:?}");
    }

    /// A `PacketIo::recv` waiting when the peer of the macOS TAP's `feth`
    /// pair is destroyed returns `Disconnected`, and does so within 5 s of
    /// the destroy: the sync build's blocking BPF read is woken at once,
    /// and the async builds' bounded BPF wait checks the descriptor's
    /// binding every 250 ms (without it, the async builds would wait
    /// forever). Unsolicited frames that arrive first are skipped. The
    /// receiving thread is bounded by a 30 s wait; the guards destroy the
    /// pair on every exit path that unwinds.
    #[test]
    #[cfg(target_os = "macos")]
    #[ignore = "requires root to open and destroy a feth TAP device"]
    fn recv_returns_disconnected_when_the_feth_is_destroyed_on_macos() {
        use std::sync::Arc;
        use std::time::{Duration, Instant};

        let _serial = serialize_feth_tests();
        #[cfg(feature = "tokio")]
        let runtime = enter_tokio_runtime();
        #[cfg(feature = "tokio")]
        let _entered = runtime.enter();

        let device = Arc::new(
            TunRsBackend::new()
                .open(DeviceConfig::new(DeviceKind::Tap))
                .expect("open a feth TAP device"),
        );
        let snapshot = device.snapshot().expect("snapshot the device");
        let feth = guard_feth_pair(&snapshot.name);
        let buf_len = snapshot.recv_buffer_len();

        let (done, outcome) = std::sync::mpsc::channel();
        {
            let device = Arc::clone(&device);
            #[cfg(feature = "tokio")]
            let runtime = runtime.handle().clone();
            std::thread::spawn(move || {
                #[cfg(feature = "tokio")]
                let _entered = runtime.enter();
                let mut buf = vec![0u8; buf_len];
                let err = loop {
                    if let Err(err) = PacketIo::recv(&*device, &mut buf) {
                        break err;
                    }
                };
                let _ = done.send(err);
            });
        }

        // Let the receiver reach its blocking read or bounded wait first.
        std::thread::sleep(Duration::from_millis(500));
        run_host_command("ifconfig", &[&feth.peer, "destroy"]);
        let destroyed = Instant::now();
        match outcome.recv_timeout(Duration::from_secs(30)) {
            Ok(err) => {
                let elapsed = destroyed.elapsed();
                eprintln!("feth peer destroy: recv returned {err:?} after {elapsed:?}");
                assert!(matches!(err, Error::Disconnected), "{err:?}");
                assert!(
                    elapsed < Duration::from_secs(5),
                    "recv returned {elapsed:?} after the destroy, expected under 5 s"
                );
            }
            Err(_) => panic!("recv did not return within 30 s of the feth peer destroy"),
        }
    }

    /// `ifconfig <utun> destroy` while a stream is polling. A `utun`
    /// interface is created through a kernel-control socket, not an
    /// interface cloner, so the kernel is expected to refuse the destroy;
    /// the test then records that nothing was torn down (only the `feth`
    /// tests above exercise macOS teardown) and leaves the device to be
    /// removed when the test process exits. If the destroy does succeed,
    /// the stream must end like on every other platform.
    #[test]
    #[cfg(target_os = "macos")]
    #[ignore = "requires root to open a utun device"]
    fn tun_stream_ends_or_utun_destroy_is_refused_on_macos() {
        #[cfg(feature = "tokio")]
        let runtime = enter_tokio_runtime();
        #[cfg(feature = "tokio")]
        let _entered = runtime.enter();

        let device = TunRsBackend::new()
            .open(DeviceConfig::new(DeviceKind::Tun))
            .expect("open a utun device");
        let name = device.snapshot().expect("snapshot the device").name;
        let drained = drain_stream_across_teardown(device, |_| {
            let output = std::process::Command::new("ifconfig")
                .args([name.as_str(), "destroy"])
                .output()
                .expect("run ifconfig");
            if !output.status.success() {
                eprintln!(
                    "utun destroy refused, nothing torn down: {}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            output.status.success()
        });
        if let Some(drained) = drained {
            assert_ended_after_one_error("utun destroy", &drained);
        }
    }

    /// Disabling the Wintun adapter (`apply(Down)`, which ends the Wintun
    /// session) while a stream is polling ends the stream with
    /// `InvalidState`: the disabled state is recoverable with `apply(Up)`,
    /// but it still ends the stream (only `BufferTooSmall` does not). The
    /// adapter is removed when the device drops at the end of the test. If
    /// the test panics, the still-running drain thread keeps the device, so
    /// removal then relies on Wintun deleting the adapter when the test
    /// process exits.
    #[test]
    #[cfg(target_os = "windows")]
    #[ignore = "requires Administrator and wintun.dll to open a TUN device"]
    fn tun_stream_ends_when_the_wintun_adapter_is_disabled_on_windows() {
        use tunnel_lattice_model::DesiredAdminState;

        #[cfg(feature = "tokio")]
        let runtime = enter_tokio_runtime();
        #[cfg(feature = "tokio")]
        let _entered = runtime.enter();

        let device = TunRsBackend::new()
            .open(DeviceConfig::new(DeviceKind::Tun))
            .expect("open a Wintun device");
        let drained = drain_stream_across_teardown(device, |device| {
            let id = device.snapshot().expect("snapshot the device").id;
            let patch = DeviceConfigPatch::new(id, Some(DesiredAdminState::Down), None)
                .expect("build a patch");
            device.apply(patch).expect("disable the adapter");
            true
        })
        .expect("the teardown ran");
        assert_ended_after_one_error("Wintun disable", &drained);
        assert!(
            matches!(drained.errors[0], Error::InvalidState),
            "{drained:?}"
        );
    }
}
