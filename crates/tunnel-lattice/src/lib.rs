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
//! - `async-io`/`tokio`: mutually exclusive, matching `tun-rs`'s own two
//!   async backends (enabling both is a compile error). Either one adds
//!   `Handle::packet_stream`, a `futures::Stream` of received packets. Uses
//!   a backend's native async I/O path when it reports
//!   `Capability::NATIVE_ASYNC`; otherwise falls back to
//!   `tunnel-lattice-async`'s thread-based adapter. No async runtime is
//!   forced on a caller that enables neither feature. `packet_stream` is
//!   referenced here as plain text, not an intra-doc link, because it only
//!   exists under these features and this crate's default `cargo doc`
//!   build (no features beyond `tun-rs`) cannot resolve it.

#![warn(missing_docs)]

pub use tunnel_lattice_core::{Error, Result};
pub use tunnel_lattice_model::{
    AdminState, DesiredAdminState, Device, DeviceConfig, DeviceConfigPatch, DeviceId, DeviceKind,
};
#[cfg(feature = "async")]
use tunnel_lattice_platform::AsyncPacketIo;
pub use tunnel_lattice_platform::{
    Capability, CapabilityProvider, MultiQueueProvider, PersistentDevice,
};
use tunnel_lattice_platform::{DeviceMutator, DeviceObserver, DeviceProvider, PacketIo};

/// An open device handle bound to this facade's concrete model types.
///
/// A single named bound for what `Tunnel::open` accepts and `Handle` wraps,
/// instead of repeating the same four-trait list on both `impl` blocks —
/// blanket-implemented for every type that satisfies it, so no backend ever
/// implements this trait by name.
pub trait ConnectedDevice:
    PacketIo
    + DeviceObserver<Device = Device>
    + DeviceMutator<DeviceConfigPatch = DeviceConfigPatch>
    + CapabilityProvider
{
}

impl<T> ConnectedDevice for T where
    T: PacketIo
        + DeviceObserver<Device = Device>
        + DeviceMutator<DeviceConfigPatch = DeviceConfigPatch>
        + CapabilityProvider
{
}

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
    B::Device: ConnectedDevice,
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
/// ## Ownership and concurrency contract
///
/// `Handle<D>` wraps the backend's device handle in an `Arc<D>` — there is
/// no single owner in the usual sense; the underlying device stays open for
/// as long as *any* clone of the `Arc` is alive, and `Handle` is [`Clone`]
/// specifically to make that sharing explicit and deliberate rather than an
/// implementation detail only `packet_stream` uses internally.
///
/// - **Multiple readers/writers are always safe on every backend.**
///   [`PacketIo::recv`]/[`PacketIo::send`] take `&self`, not `&mut self` —
///   this is a property of the trait, not an accident, and every backend
///   this crate ships (`tunnel-lattice-backend-tunrs`, on Linux, macOS, and
///   Windows) is verified safe for concurrent `recv`/`send` from multiple
///   threads sharing one `Handle` clone. This "naive" multiplexing needs no
///   feature or capability check; see `ARCHITECTURE.md`'s async design
///   notes for why `tun-rs`'s own `recv`/`send` signatures already commit
///   to this.
/// - **`additional_queue` (with `D: MultiQueueProvider`) is a different,
///   stronger thing**: it returns an independent `Handle` over a *second*
///   OS-level queue on the same device (Linux `IFF_MULTI_QUEUE` only —
///   `Capability::MULTI_QUEUE`), for hardware-scheduled per-CPU
///   distribution instead of every thread contending on one queue. The two
///   returned handles do not share an `Arc`: dropping one does not affect
///   the other, and each closes only its own queue on drop.
/// - **Drop closes the device once every clone is gone.** `D`'s own `Drop`
///   impl (e.g. `tun_rs::SyncDevice`/`AsyncDevice`'s, which close the
///   underlying file descriptor) runs when the last `Arc<D>` referencing it
///   is dropped — which may be a `Handle` clone, a live `PacketStream`,
///   or both, in any order. No `Handle` method explicitly "closes" a
///   device; there is nothing to call beyond letting every reference drop.
/// - **`packet_stream` holds its own `Arc` clone**, independent of the
///   `Handle` it was created from — dropping the original `Handle` while a
///   `PacketStream` is still alive does not close the device early, and
///   vice versa. See `tunnel_lattice_async::PacketStream`'s own docs for
///   its worker-thread shutdown caveat on `Drop` (a known limitation, not
///   related to this ownership model).
///
/// Referenced as plain text, not an intra-doc link, for the same reason as
/// the crate-level docs above (`packet_stream` only exists under the
/// `async-io`/`tokio` features).
pub struct Handle<D> {
    device: std::sync::Arc<D>,
}

impl<D> Clone for Handle<D> {
    /// Cheap: clones the underlying `Arc<D>`, not the device itself — see
    /// the type's docs on what sharing a clone means for concurrent access
    /// and `Drop`.
    fn clone(&self) -> Self {
        Handle {
            device: std::sync::Arc::clone(&self.device),
        }
    }
}

impl<D> Handle<D>
where
    D: ConnectedDevice,
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

impl<D> Handle<D>
where
    D: PersistentDevice,
{
    /// Marks this device persistent — see [`PersistentDevice`]'s docs.
    /// Requires `Capability::PERSISTENT_DEVICES`; only `TunRsDevice` on
    /// Linux implements this today.
    pub fn persist(&self) -> Result<()> {
        self.device.persist()
    }
}

impl<D> Handle<D>
where
    D: MultiQueueProvider,
{
    /// Duplicates this device's hardware-scheduled queue for use from
    /// another thread — see [`MultiQueueProvider`]'s docs. Requires
    /// `Capability::MULTI_QUEUE` and that the device was opened with
    /// `DeviceConfig::with_multi_queue(true)`; only `TunRsDevice` on Linux
    /// implements this today.
    pub fn additional_queue(&self) -> Result<Handle<D>> {
        Ok(Handle {
            device: std::sync::Arc::new(self.device.additional_queue()?),
        })
    }
}

#[cfg(feature = "async")]
impl<D> Handle<D>
where
    D: PacketIo + AsyncPacketIo + CapabilityProvider + Send + Sync + 'static,
{
    /// Returns a `futures::Stream` of received packets.
    ///
    /// `mtu` bounds the per-packet receive buffer. Uses
    /// `tunnel-lattice-async::from_async_device` (no worker thread; dropping
    /// the stream drops the in-flight `recv` future, which is genuine,
    /// immediate cancellation) when the device reports
    /// `Capability::NATIVE_ASYNC`; otherwise falls back to
    /// `from_device`'s thread-based bridge over [`PacketIo`], which cannot
    /// guarantee prompt shutdown — see that function's rustdoc. Every
    /// backend `tunnel-lattice` ships as of this method's `D: AsyncPacketIo`
    /// bound always implements `AsyncPacketIo` whenever this method is
    /// reachable at all (it requires the `async` feature, which is what
    /// makes a backend build its async-capable handle in the first place),
    /// so the fallback path exists for a hypothetical future backend with
    /// no native async support, not for anything shipped today.
    pub fn packet_stream(&self, mtu: usize) -> tunnel_lattice_async::PacketStream {
        if self
            .device
            .capabilities()
            .contains(Capability::NATIVE_ASYNC)
        {
            tunnel_lattice_async::from_async_device(std::sync::Arc::clone(&self.device), mtu)
        } else {
            tunnel_lattice_async::from_device(std::sync::Arc::clone(&self.device), mtu)
        }
    }
}

/// Privileged, `async`-feature-only tests exercising `Handle::packet_stream`
/// against a real device — see `tunnel-lattice-backend-tunrs`'s
/// `privileged_tests` module for why these are `#[ignore]`d and how to run
/// them, and `tunnel-lattice-async`'s own unit tests for the
/// cancellation-semantics proof against a mock (no privilege needed there).
#[cfg(all(test, feature = "async", feature = "tun-rs"))]
mod privileged_tests {
    use futures::FutureExt;

    use super::*;

    #[cfg(feature = "tokio")]
    fn enter_tokio_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .enable_io()
            .build()
            .expect("build a Tokio runtime")
    }

    #[test]
    #[ignore = "requires CAP_NET_ADMIN/Administrator/root to open a TUN device"]
    fn packet_stream_dispatches_to_the_native_no_thread_path() {
        #[cfg(feature = "tokio")]
        let _runtime = enter_tokio_runtime();
        #[cfg(feature = "tokio")]
        let _entered = _runtime.enter();

        let tunnel = Tunnel::connect();
        let device = tunnel
            .open(DeviceConfig::new(DeviceKind::Tun).with_mtu(1400))
            .expect("open a TUN device");

        assert!(
            device.capabilities().contains(Capability::NATIVE_ASYNC),
            "tunnel-lattice-backend-tunrs reports NATIVE_ASYNC whenever \
             an async feature is enabled, which is the only way this test \
             itself gets compiled in"
        );

        let mut stream = device.packet_stream(1400);
        // A freshly created Linux TUN device is not actually silent: the
        // kernel sends IPv6 neighbor-discovery traffic (router
        // solicitation) onto it almost immediately, confirmed by an
        // earlier run of this test printing a real received packet here.
        // So this deliberately does not assert Pending vs. Ready either
        // way — only that whatever comes back is a well-formed item, not
        // an error from the dispatch itself.
        if let Some(item) = futures::StreamExt::next(&mut stream)
            .now_or_never()
            .flatten()
        {
            item.expect("a resolved item from the native path must be Ok, not a dispatch error");
        }
        // Reaching this line at all is the proof: dropping a real device's
        // in-flight (or just-completed) `AsyncPacketIo::recv` future
        // completes synchronously, with no worker thread left parked in a
        // blocking recv the way the pre-0.4 thread-bridge path could leave
        // one behind.
        drop(stream);
    }
}
