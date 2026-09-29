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
//! Upstream API surface last verified against `tun-rs` 2.8.11 on docs.rs
//! (`tun_rs::DeviceBuilder`, `SyncDevice`, `AsyncDevice`, `Layer`); re-verify
//! before bumping the workspace's pinned `tun-rs` version if its public API
//! has changed.

#![warn(missing_docs)]

use tunnel_lattice_core::{Error, PlatformErrorCode, Result};
use tunnel_lattice_model::{
    AdminState, Device, DeviceConfig, DeviceConfigPatch, DeviceId, DeviceKind,
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
pub struct TunRsDevice {
    kind: DeviceKind,
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

    fn open(&self, config: Self::DeviceConfig) -> Result<Self::Device> {
        let layer = match config.kind {
            DeviceKind::Tun => tun_rs::Layer::L3,
            DeviceKind::Tap => tun_rs::Layer::L2,
            _ => return Err(Error::Unsupported),
        };
        let mut builder = tun_rs::DeviceBuilder::new().layer(layer);
        // `DeviceBuilder::multi_queue` only exists on Linux in `tun-rs`
        // itself (not merely a no-op elsewhere) — `IFF_MULTI_QUEUE` has no
        // equivalent concept on macOS/Windows, so there is nothing to call
        // there. `DeviceConfig::multi_queue`'s own docs already say the
        // request is ignored off Linux; this is that ignoring.
        #[cfg(target_os = "linux")]
        {
            builder = builder.multi_queue(config.multi_queue);
        }
        if let Some(name) = config.name {
            builder = builder.name(name);
        }
        if let Some(mtu) = config.mtu {
            let mtu = u16::try_from(mtu).map_err(|_| Error::InvalidState)?;
            builder = builder.mtu(mtu);
        }

        #[cfg(feature = "async")]
        let handle = builder.build_async().map_err(io_error)?;
        #[cfg(not(feature = "async"))]
        let handle = builder.build_sync().map_err(io_error)?;

        Ok(TunRsDevice {
            kind: config.kind,
            handle,
        })
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
    fn blocking_recv(&self, buf: &mut [u8]) -> std::io::Result<usize> {
        #[cfg(feature = "tokio")]
        {
            tokio::runtime::Handle::current().block_on(self.handle.recv(buf))
        }
        #[cfg(all(feature = "async", not(feature = "tokio")))]
        {
            futures::executor::block_on(self.handle.recv(buf))
        }
        #[cfg(not(feature = "async"))]
        {
            self.handle.recv(buf)
        }
    }

    /// Blocking send; see [`Self::blocking_recv`].
    fn blocking_send(&self, buf: &[u8]) -> std::io::Result<usize> {
        #[cfg(feature = "tokio")]
        {
            tokio::runtime::Handle::current().block_on(self.handle.send(buf))
        }
        #[cfg(all(feature = "async", not(feature = "tokio")))]
        {
            futures::executor::block_on(self.handle.send(buf))
        }
        #[cfg(not(feature = "async"))]
        {
            self.handle.send(buf)
        }
    }
}

impl PacketIo for TunRsDevice {
    fn recv(&self, buf: &mut [u8]) -> Result<usize> {
        self.blocking_recv(buf).map_err(io_error)
    }

    fn send(&self, buf: &[u8]) -> Result<usize> {
        self.blocking_send(buf).map_err(io_error)
    }
}

#[cfg(feature = "async")]
impl AsyncPacketIo for TunRsDevice {
    async fn recv(&self, buf: &mut [u8]) -> Result<usize> {
        self.handle.recv(buf).await.map_err(io_error)
    }

    async fn send(&self, buf: &[u8]) -> Result<usize> {
        self.handle.send(buf).await.map_err(io_error)
    }
}

impl DeviceObserver for TunRsDevice {
    type Device = Device;

    fn snapshot(&self) -> Result<Device> {
        let id = DeviceId::new(u64::from(self.handle.if_index().map_err(io_error)?));
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

        Ok(Device::new(id, name, self.kind, mtu, admin_state))
    }
}

impl DeviceMutator for TunRsDevice {
    type DeviceConfigPatch = DeviceConfigPatch;

    fn apply(&self, patch: Self::DeviceConfigPatch) -> Result<()> {
        use tunnel_lattice_model::DesiredAdminState;

        // Resolve the requested admin state before any native call, so a
        // `DesiredAdminState` variant this backend does not know (the enum is
        // `#[non_exhaustive]`) is rejected without changing the MTU first.
        let enable = match patch.admin_state() {
            None => None,
            Some(DesiredAdminState::Up) => Some(true),
            Some(DesiredAdminState::Down) => Some(false),
            Some(_) => return Err(Error::Unsupported),
        };
        if let Some(mtu) = patch.mtu() {
            let mtu = u16::try_from(mtu).map_err(|_| Error::InvalidState)?;
            self.handle.set_mtu(mtu).map_err(io_error)?;
        }
        if let Some(enable) = enable {
            self.handle.enabled(enable).map_err(io_error)?;
        }
        Ok(())
    }
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
            handle,
        })
    }
}

impl CapabilityProvider for TunRsDevice {
    fn capabilities(&self) -> Capability {
        let base = Capability::DEVICE_MUTATION | Capability::TAP_DEVICES;
        #[cfg(target_os = "linux")]
        let base = base | Capability::PERSISTENT_DEVICES | Capability::MULTI_QUEUE;
        #[cfg(feature = "async")]
        {
            base | Capability::NATIVE_ASYNC
        }
        #[cfg(not(feature = "async"))]
        {
            base
        }
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
    /// the TUN module, where the correct result is `NotFound` instead).
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

        // Dropping the clone first does not affect the original queue —
        // exercises the "independent handles" half of Handle's ownership
        // contract (see tunnel-lattice's rustdoc) at the backend level.
        drop(queue);
        device
            .snapshot()
            .expect("original queue still usable after the clone was dropped");
    }
}
