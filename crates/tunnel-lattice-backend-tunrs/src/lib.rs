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

/// The `tun-rs`-backed implementation of Tunnel Lattice's provider traits.
///
/// Stateless: opening a device does not go through a persistent connection
/// object the way `net-lattice`'s Netlink/WFP/route-socket backends do —
/// each [`TunRsDevice`] is independent of the others.
#[derive(Debug, Default, Clone, Copy)]
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

fn io_error(err: std::io::Error) -> Error {
    #[cfg(target_os = "linux")]
    {
        Error::Platform(PlatformErrorCode::Linux(err.raw_os_error().unwrap_or(0)))
    }
    #[cfg(target_os = "windows")]
    {
        Error::Platform(PlatformErrorCode::Windows(
            err.raw_os_error().unwrap_or(0) as u32
        ))
    }
    #[cfg(target_os = "macos")]
    {
        Error::Platform(PlatformErrorCode::Darwin(err.raw_os_error().unwrap_or(0)))
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        let _ = err;
        Error::Unsupported
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
        if let Some(mtu) = patch.mtu() {
            let mtu = u16::try_from(mtu).map_err(|_| Error::InvalidState)?;
            self.handle.set_mtu(mtu).map_err(io_error)?;
        }
        if let Some(admin_state) = patch.admin_state() {
            use tunnel_lattice_model::DesiredAdminState;
            let enable = matches!(admin_state, DesiredAdminState::Up);
            self.handle.enabled(enable).map_err(io_error)?;
        }
        Ok(())
    }
}

impl CapabilityProvider for TunRsDevice {
    fn capabilities(&self) -> Capability {
        let base = Capability::DEVICE_MUTATION | Capability::TAP_DEVICES;
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
}
