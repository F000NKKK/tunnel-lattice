//! Cross-platform Rust library for TUN/TAP tunnel interfaces, designed to
//! compose with the rest of the Lattice networking stack.
//!
//! Start with [`Tunnel::connect`] (available with the default `tun-rs`
//! feature) to open a device:
//!
//! ```no_run
//! use tunnel_lattice::{DeviceConfig, DeviceKind, Result, Tunnel};
//!
//! fn main() -> Result<()> {
//!     let tunnel = Tunnel::connect();
//!     let device = tunnel.open(DeviceConfig::new(DeviceKind::Tun).with_mtu(1500))?;
//!     let mut buf = vec![0u8; 1500];
//!     let len = device.recv(&mut buf)?;
//!     println!("{} bytes", len);
//!     Ok(())
//! }
//! ```
//!
//! This crate carries no OS-addressing responsibility — it creates and
//! configures the virtual interface and transfers packets on it; IP address
//! assignment on the resulting interface is `net-lattice`'s job (see the
//! `net-lattice` crate in the sibling Lattice ecosystem).
//!
//! ## Feature flags
//!
//! - `tun-rs` (default): selects `tunnel-lattice-backend-tunrs`,
//!   implemented on top of the cross-platform `tun-rs` crate. This is a
//!   Cargo feature rather than a `target_os` cfg gate so a future
//!   alternative backend can sit alongside it instead of replacing it — see
//!   the workspace `ARCHITECTURE.md`, "Backend replacement plan."
//! - `async`: adds [`Handle::packet_stream`], a `futures::Stream` of
//!   received packets. Uses a backend's native async I/O path when it
//!   reports `Capability::NATIVE_ASYNC`; otherwise falls back to
//!   `tunnel-lattice-async`'s thread-based adapter. No async runtime is
//!   forced on a caller that does not enable this feature.

#![warn(missing_docs)]

pub use tunnel_lattice_core::{Error, Result};
pub use tunnel_lattice_model::{
    AdminState, DesiredAdminState, Device, DeviceConfig, DeviceConfigPatch, DeviceId, DeviceKind,
};
pub use tunnel_lattice_platform::{Capability, CapabilityProvider};
use tunnel_lattice_platform::{DeviceMutator, DeviceObserver, DeviceProvider, PacketIo};

/// A connected backend for creating and operating TUN/TAP devices.
///
/// Generic over the backend the same way `net_lattice::Lattice<Backend>`
/// is: `tunnel_lattice_platform::DeviceProvider`'s associated types are
/// bound to the concrete `tunnel_lattice_model` types here, at the facade
/// layer, not inside `tunnel-lattice-platform` itself.
pub struct Tunnel<B> {
    backend: B,
}

impl<B> Tunnel<B>
where
    B: DeviceProvider<DeviceConfig = DeviceConfig>,
    B::Device: PacketIo
        + DeviceObserver<Device = Device>
        + DeviceMutator<DeviceConfigPatch = DeviceConfigPatch>
        + CapabilityProvider,
{
    /// Opens a new device matching `config`.
    pub fn open(&self, config: DeviceConfig) -> Result<Handle<B::Device>> {
        Ok(Handle {
            device: std::sync::Arc::new(self.backend.open(config)?),
        })
    }
}

#[cfg(feature = "tun-rs")]
impl Tunnel<tunnel_lattice_backend_tunrs::TunRsBackend> {
    /// Connects the default `tun-rs`-backed backend.
    ///
    /// Stateless and infallible: `tun-rs` has no persistent connection step
    /// analogous to `net_lattice::Lattice::connect`'s Netlink/WFP/
    /// route-socket handshake — the privileged step is opening a device,
    /// not connecting the backend.
    pub fn connect() -> Self {
        Self {
            backend: tunnel_lattice_backend_tunrs::TunRsBackend::new(),
        }
    }
}

/// An open TUN/TAP device.
///
/// Wraps the backend's device handle in an `Arc` unconditionally (not only
/// under the `async` feature) so [`Handle::packet_stream`] can share it with
/// a background worker thread without a separate wrapping step at the call
/// site.
pub struct Handle<D> {
    device: std::sync::Arc<D>,
}

impl<D> Handle<D>
where
    D: PacketIo
        + DeviceObserver<Device = Device>
        + DeviceMutator<DeviceConfigPatch = DeviceConfigPatch>
        + CapabilityProvider,
{
    /// Reads one packet into `buf`, returning the number of bytes written.
    pub fn recv(&self, buf: &mut [u8]) -> Result<usize> {
        self.device.recv(buf)
    }

    /// Writes one packet from `buf`.
    pub fn send(&self, buf: &[u8]) -> Result<usize> {
        self.device.send(buf)
    }

    /// Returns this device's current observed state.
    pub fn snapshot(&self) -> Result<Device> {
        self.device.snapshot()
    }

    /// Applies an MTU or administrative-state patch to this device.
    pub fn apply(&self, patch: DeviceConfigPatch) -> Result<()> {
        self.device.apply(patch)
    }

    /// Returns the runtime-dependent capabilities this device has available.
    pub fn capabilities(&self) -> Capability {
        self.device.capabilities()
    }
}

#[cfg(feature = "async")]
impl<D> Handle<D>
where
    D: PacketIo + Send + Sync + 'static,
{
    /// Returns a `futures::Stream` of received packets, backed by
    /// `tunnel-lattice-async`'s thread-based adapter over [`PacketIo`].
    ///
    /// `mtu` bounds the per-packet receive buffer. Prefer a backend's
    /// native `AsyncPacketIo` path directly when it reports
    /// `Capability::NATIVE_ASYNC` — this method always uses the generic
    /// adapter regardless of that capability; see `tunnel-lattice-async`'s
    /// README for why.
    pub fn packet_stream(&self, mtu: usize) -> tunnel_lattice_async::PacketStream {
        tunnel_lattice_async::from_device(std::sync::Arc::clone(&self.device), mtu)
    }
}
