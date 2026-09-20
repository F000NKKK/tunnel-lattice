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
pub struct TunRsDevice {
    kind: DeviceKind,
    sync: tun_rs::SyncDevice,
    #[cfg(feature = "async")]
    async_device: tun_rs::AsyncDevice,
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

        let sync = builder.build_sync().map_err(io_error)?;
        #[cfg(feature = "async")]
        let async_device = {
            let cloned = sync.try_clone().map_err(io_error)?;
            tun_rs::AsyncDevice::new(cloned).map_err(io_error)?
        };

        Ok(TunRsDevice {
            kind: config.kind,
            sync,
            #[cfg(feature = "async")]
            async_device,
        })
    }
}

impl PacketIo for TunRsDevice {
    fn recv(&self, buf: &mut [u8]) -> Result<usize> {
        self.sync.recv(buf).map_err(io_error)
    }

    fn send(&self, buf: &[u8]) -> Result<usize> {
        self.sync.send(buf).map_err(io_error)
    }
}

#[cfg(feature = "async")]
impl AsyncPacketIo for TunRsDevice {
    async fn recv(&self, buf: &mut [u8]) -> Result<usize> {
        self.async_device.recv(buf).await.map_err(io_error)
    }

    async fn send(&self, buf: &[u8]) -> Result<usize> {
        self.async_device.send(buf).await.map_err(io_error)
    }
}

impl DeviceObserver for TunRsDevice {
    type Device = Device;

    fn snapshot(&self) -> Result<Device> {
        let id = DeviceId::new(u64::from(self.sync.if_index().map_err(io_error)?));
        let name = self.sync.name().map_err(io_error)?;
        let mtu = u32::from(self.sync.mtu().map_err(io_error)?);
        let admin_state = if self.sync.is_running().map_err(io_error)? {
            AdminState::Up
        } else {
            AdminState::Down
        };

        Ok(Device::new(id, name, self.kind, mtu, admin_state))
    }
}

impl DeviceMutator for TunRsDevice {
    type DeviceConfigPatch = DeviceConfigPatch;

    fn apply(&self, patch: Self::DeviceConfigPatch) -> Result<()> {
        if let Some(mtu) = patch.mtu() {
            let mtu = u16::try_from(mtu).map_err(|_| Error::InvalidState)?;
            self.sync.set_mtu(mtu).map_err(io_error)?;
        }
        if let Some(admin_state) = patch.admin_state() {
            use tunnel_lattice_model::DesiredAdminState;
            let enable = matches!(admin_state, DesiredAdminState::Up);
            self.sync.enabled(enable).map_err(io_error)?;
        }
        Ok(())
    }
}

impl CapabilityProvider for TunRsDevice {
    fn capabilities(&self) -> Capability {
        #[allow(unused_mut)]
        let mut caps = Capability::DEVICE_MUTATION | Capability::TAP_DEVICES;
        #[cfg(feature = "async")]
        {
            caps |= Capability::NATIVE_ASYNC;
        }
        caps
    }
}
