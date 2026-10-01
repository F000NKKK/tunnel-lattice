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
//!     // Fits one packet at the current MTU; a larger packet is reported as
//!     // `Error::BufferTooSmall`, never truncated.
//!     let mut buf = vec![0u8; device.snapshot()?.recv_buffer_len()];
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
//!   `Handle::packet_stream` and `Handle::packet_stream_with_pool`, a
//!   `futures::Stream` of received packets, each a `PacketBuf` view into a
//!   `PacketPool` slot (both re-exported under these features). Uses
//!   a backend's native async I/O path when it reports
//!   `Capability::NATIVE_ASYNC`; otherwise falls back to
//!   `tunnel-lattice-async`'s thread-based adapter. Either feature also adds
//!   `Handle::send_async`, which returns the backend's own async send
//!   future for use inside async code, where the blocking `Handle::send`
//!   must not be called. No async runtime is forced on a caller that
//!   enables neither feature. `packet_stream` and `send_async` are
//!   referenced here as plain text, not intra-doc links, because they only
//!   exist under these features and this crate's default `cargo doc` build
//!   (no features beyond `tun-rs`) cannot resolve them.

#![warn(missing_docs)]

#[cfg(feature = "async")]
pub use tunnel_lattice_async::{PacketBuf, PacketPool, PacketStream};
#[cfg(feature = "tun-rs")]
pub use tunnel_lattice_backend_tunrs::{TunRsBackend, TunRsDevice};
pub use tunnel_lattice_core::{Error, PlatformErrorCode, Result};
pub use tunnel_lattice_model::{
    AdminState, DesiredAdminState, Device, DeviceConfig, DeviceConfigPatch, DeviceId, DeviceKind,
    MacAddress,
};
#[cfg(feature = "async")]
pub use tunnel_lattice_platform::AsyncPacketIo;
pub use tunnel_lattice_platform::{
    Capability, CapabilityProvider, DeviceMutator, DeviceObserver, DeviceProvider,
    MultiQueueProvider, PacketIo, PersistentDevice,
};

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

impl<B> Tunnel<B> {
    /// Wraps any backend, for example one implemented outside this
    /// workspace or a test double.
    ///
    /// `open` is available once the backend implements [`DeviceProvider`]
    /// with this crate's [`DeviceConfig`] and a device type satisfying
    /// [`ConnectedDevice`]; other methods need only their own bound:
    ///
    /// ```
    /// use tunnel_lattice::{Capability, CapabilityProvider, Tunnel};
    ///
    /// /// A backend that reports no capabilities.
    /// struct Minimal;
    ///
    /// impl CapabilityProvider for Minimal {
    ///     fn capabilities(&self) -> Capability {
    ///         Capability::empty()
    ///     }
    /// }
    ///
    /// let tunnel = Tunnel::new(Minimal);
    /// assert!(tunnel.capabilities().is_empty());
    /// ```
    pub const fn new(backend: B) -> Self {
        Self { backend }
    }
}

impl<B> Tunnel<B>
where
    B: CapabilityProvider,
{
    /// Returns the capabilities the backend reports for this host before
    /// any device is opened.
    ///
    /// A capability that depends on the host (for example a driver that
    /// may not be installed) is reported only if the backend detected it.
    /// `open` stays authoritative: it is never refused in advance because
    /// of this answer. A [`Handle`]'s own [`Handle::capabilities`] can add
    /// flags that depend on the opened device.
    pub fn capabilities(&self) -> Capability {
        self.backend.capabilities()
    }
}

impl<B> Tunnel<B>
where
    B: DeviceProvider<DeviceConfig = DeviceConfig>,
    B::Device: ConnectedDevice,
{
    /// Opens a new device matching `config`.
    pub fn open(&self, config: DeviceConfig) -> Result<Handle<B::Device>> {
        let kind = config.kind;
        Ok(Handle {
            device: std::sync::Arc::new(self.backend.open(config)?),
            kind,
        })
    }
}

#[cfg(feature = "tun-rs")]
impl Tunnel<TunRsBackend> {
    /// Connects the default `tun-rs`-backed backend; the same as
    /// `Tunnel::new(TunRsBackend::new())`.
    ///
    /// Stateless and infallible: `tun-rs` has no persistent connection step
    /// analogous to `net_lattice::Lattice::connect`'s Netlink/WFP/
    /// route-socket handshake — the privileged step is opening a device,
    /// not connecting the backend.
    ///
    /// [`Tunnel::capabilities`] then reports what this host supports
    /// without opening anything or needing privilege. `TAP_DEVICES` is
    /// always reported on Linux and macOS, and on Windows only if the
    /// tap-windows6 driver is installed (detected once per process; see
    /// `TunRsBackend`'s `CapabilityProvider` impl):
    ///
    /// ```
    /// use tunnel_lattice::{Capability, Tunnel};
    ///
    /// let tunnel = Tunnel::connect();
    /// let host = tunnel.capabilities();
    /// assert!(host.contains(Capability::DEVICE_MUTATION));
    /// // Per-handle flags appear only on an open device's own answer.
    /// assert!(!host.contains(Capability::MAC_MUTATION));
    /// ```
    pub fn connect() -> Self {
        Self::new(TunRsBackend::new())
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
/// - **`send_async` only borrows.** Its future borrows the `Handle` and the
///   packet buffer; it needs no `Arc` clone and keeps nothing alive after
///   it completes or is dropped.
///
/// Referenced as plain text, not intra-doc links, for the same reason as
/// the crate-level docs above (`packet_stream` and `send_async` only exist
/// under the `async-io`/`tokio` features).
pub struct Handle<D> {
    device: std::sync::Arc<D>,
    kind: DeviceKind,
}

impl<D> Clone for Handle<D> {
    /// Cheap: clones the underlying `Arc<D>`, not the device itself — see
    /// the type's docs on what sharing a clone means for concurrent access
    /// and `Drop`.
    fn clone(&self) -> Self {
        Handle {
            device: std::sync::Arc::clone(&self.device),
            kind: self.kind,
        }
    }
}

impl<D> Handle<D>
where
    D: ConnectedDevice,
{
    /// Returns this handle's device identity, captured when the device was
    /// opened. Makes no native call.
    ///
    /// Equal to `snapshot()?.id`. It is not guaranteed to equal the
    /// interface's current OS index (see [`DeviceId`]); to find the same
    /// interface through `net-lattice`, resolve it by `snapshot()?.name`.
    ///
    /// There is deliberately no `name()` shortcut: an interface can be
    /// renamed outside this process, so read the current name with
    /// `snapshot()?.name`.
    pub fn id(&self) -> DeviceId {
        self.device.id()
    }

    /// Returns whether this device is TUN or TAP, as requested at open.
    /// Makes no native call.
    pub fn kind(&self) -> DeviceKind {
        self.kind
    }

    /// Reads one packet into `buf`, returning the number of bytes written.
    ///
    /// Blocks the calling thread until a packet arrives. Inside async code,
    /// receive with `packet_stream` instead (available with the `async-io`
    /// or `tokio` feature).
    ///
    /// With the `async-io` feature and the tun-rs backend, this blocks on
    /// the device's async receive, so called from an async task it parks the
    /// executor thread that polls that task. With the `tokio` feature and
    /// the tun-rs backend, the calling thread must have a multi-threaded
    /// Tokio runtime entered; on a `current_thread` runtime it hangs (see
    /// the backend's documentation).
    ///
    /// # Panics
    ///
    /// With the `tokio` feature and the tun-rs backend: when called outside
    /// a Tokio runtime context, or from inside an asynchronous context (a
    /// task, `#[tokio::main]`, or `block_on`).
    pub fn recv(&self, buf: &mut [u8]) -> Result<usize> {
        self.device.recv(buf)
    }

    /// Writes one packet from `buf`.
    ///
    /// Blocks the calling thread until the packet is written. Inside async
    /// code, use `send_async` instead (available with the `async-io` or
    /// `tokio` feature); it has the same results and errors.
    ///
    /// With the `async-io` feature and the tun-rs backend, this blocks on
    /// the device's async send, so called from an async task it parks the
    /// executor thread that polls that task. With the `tokio` feature and
    /// the tun-rs backend, the calling thread must have a multi-threaded
    /// Tokio runtime entered; on a `current_thread` runtime it hangs (see
    /// the backend's documentation).
    ///
    /// # Panics
    ///
    /// With the `tokio` feature and the tun-rs backend: when called outside
    /// a Tokio runtime context, or from inside an asynchronous context (a
    /// task, `#[tokio::main]`, or `block_on`).
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

/// Persistence, available only when the device type implements
/// [`PersistentDevice`].
///
/// With the tun-rs backend that is Linux only. On macOS and Windows
/// `TunRsDevice` does not implement [`PersistentDevice`], so `persist` and
/// `unpersist` do not exist on its `Handle`: calling them is a compile
/// error, not a runtime `Error::Unsupported`. `Capability::PERSISTENT_DEVICES`
/// is absent there to match.
impl<D> Handle<D>
where
    D: PersistentDevice,
{
    /// Marks this device persistent, so it survives its last handle closing
    /// (including this process exiting) — see [`PersistentDevice`]'s docs.
    /// A later process re-attaches by opening the same name, kind, and
    /// multi-queue setting. Requires `Capability::PERSISTENT_DEVICES`; only
    /// `TunRsDevice` on Linux implements this today, and on other targets
    /// the method does not exist (a compile error, not
    /// `Error::Unsupported`).
    pub fn persist(&self) -> Result<()> {
        self.device.persist()
    }

    /// Clears persistence, so the device is destroyed when its last handle
    /// (in any process) closes — see [`PersistentDevice`]'s docs.
    /// Idempotent, and works from any handle or queue attached to the
    /// device, including one re-attached by name in another process.
    /// Requires `Capability::PERSISTENT_DEVICES`; only `TunRsDevice` on
    /// Linux implements this today, and on other targets the method does
    /// not exist (a compile error, not `Error::Unsupported`).
    pub fn unpersist(&self) -> Result<()> {
        self.device.unpersist()
    }
}

/// Multi-queue, available only when the device type implements
/// [`MultiQueueProvider`].
///
/// With the tun-rs backend that is Linux only. On macOS and Windows
/// `TunRsDevice` does not implement [`MultiQueueProvider`], so
/// `additional_queue` does not exist on its `Handle`: calling it is a
/// compile error, not a runtime `Error::Unsupported`.
/// `Capability::MULTI_QUEUE` is absent there to match. Sharing one device
/// across threads needs no multi-queue at all; clone the `Handle` instead.
impl<D> Handle<D>
where
    D: MultiQueueProvider,
{
    /// Duplicates this device's hardware-scheduled queue for use from
    /// another thread — see [`MultiQueueProvider`]'s docs. Requires
    /// `Capability::MULTI_QUEUE` and that the device was opened with
    /// `DeviceConfig::with_multi_queue(true)` (otherwise
    /// `Error::Unsupported`); only `TunRsDevice` on Linux implements this
    /// today, and on other targets the method does not exist (a compile
    /// error).
    pub fn additional_queue(&self) -> Result<Handle<D>> {
        Ok(Handle {
            device: std::sync::Arc::new(self.device.additional_queue()?),
            kind: self.kind,
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
    /// Each `Ok` item is a [`PacketBuf`] holding exactly one packet as
    /// [`PacketIo::recv`] frames it (one raw IP packet for TUN, one Ethernet
    /// frame for TAP), received straight into a slot of a [`PacketPool`]
    /// built with `PacketPool::with_buf_len(buf_len)`. Dropping the item
    /// returns its slot; while the consumer holds every slot, the stream
    /// waits instead of receiving. Use `to_vec()` for an owned copy.
    ///
    /// `buf_len` is the per-packet receive buffer size; use
    /// `self.snapshot()?.recv_buffer_len()` ([`Device::recv_buffer_len`]),
    /// which fits one packet at the device's current MTU, Ethernet framing
    /// included for TAP. A packet larger than `buf_len` is never truncated:
    /// it is discarded, the stream yields `Err(Error::BufferTooSmall)`, and
    /// it keeps receiving.
    ///
    /// Every other error is yielded once and then the stream ends: both
    /// `Err(Error::Disconnected)` (the device is gone for good) and errors
    /// the device can recover from, such as `Err(Error::InvalidState)`
    /// after the interface was administratively disabled. To keep
    /// receiving after recovering (for example after applying
    /// `DesiredAdminState::Up`), call `packet_stream` again for a new
    /// stream.
    ///
    /// The stream adapter never yields `InvalidState` for its own reasons;
    /// a rejected `buf_len` is reported only by this method's own `Err`.
    /// With the tun-rs backend, an `InvalidState` item means the Windows
    /// (Wintun) interface is currently disabled, which is recoverable; other
    /// backends define their own meaning.
    ///
    /// ```no_run
    /// # #[cfg(any(feature = "async-io", feature = "tokio"))]
    /// # fn example() -> tunnel_lattice::Result<()> {
    /// use tunnel_lattice::{DeviceConfig, DeviceKind, Tunnel};
    ///
    /// let device = Tunnel::connect().open(DeviceConfig::new(DeviceKind::Tap))?;
    /// let stream = device.packet_stream(device.snapshot()?.recv_buffer_len())?;
    /// # let _ = stream;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// Uses
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
    ///
    /// # Errors
    ///
    /// [`Error::InvalidState`] only if `buf_len` is rejected (zero, or too
    /// large for a pool slot; see [`PacketPool::new`]). Nothing is allocated
    /// and the device is not touched in that case.
    ///
    /// # Panics
    ///
    /// On the thread-bridge branch (a device without
    /// `Capability::NATIVE_ASYNC`), if the OS cannot spawn the worker thread.
    ///
    /// # Aborts
    ///
    /// If the pool's slab allocation fails, as [`PacketPool::new`] does.
    pub fn packet_stream(&self, buf_len: usize) -> Result<tunnel_lattice_async::PacketStream> {
        Ok(self.packet_stream_with_pool(PacketPool::with_buf_len(buf_len)?))
    }

    /// Like `packet_stream`, but receives into `pool`, which may be shared
    /// with other streams. Every item's length is at most `pool.buf_len()`.
    ///
    /// Streams that share a pool share its slots: a consumer that holds
    /// many items can starve the other streams on the same pool.
    ///
    /// ```no_run
    /// # #[cfg(any(feature = "async-io", feature = "tokio"))]
    /// # fn example() -> tunnel_lattice::Result<()> {
    /// use tunnel_lattice::{DeviceConfig, DeviceKind, PacketPool, Tunnel};
    ///
    /// let device = Tunnel::connect().open(DeviceConfig::new(DeviceKind::Tun))?;
    /// let pool = PacketPool::new(32, device.snapshot()?.recv_buffer_len())?;
    /// let stream = device.packet_stream_with_pool(pool);
    /// # let _ = stream;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Panics
    ///
    /// On the thread-bridge branch (a device without
    /// `Capability::NATIVE_ASYNC`), if the OS cannot spawn the worker thread.
    pub fn packet_stream_with_pool(&self, pool: PacketPool) -> tunnel_lattice_async::PacketStream {
        let device = std::sync::Arc::clone(&self.device);
        if self
            .device
            .capabilities()
            .contains(Capability::NATIVE_ASYNC)
        {
            tunnel_lattice_async::from_async_device_with_pool(device, pool)
        } else {
            tunnel_lattice_async::from_device_with_pool(device, pool)
        }
    }
}

#[cfg(feature = "async")]
impl<D> Handle<D>
where
    D: AsyncPacketIo,
{
    /// Writes one packet from `buf` without blocking the calling thread.
    ///
    /// The async counterpart of [`Handle::send`]: `buf` holds exactly one
    /// packet, framed as [`PacketIo::send`] expects (one raw IP packet for
    /// TUN, one Ethernet frame for TAP). The returned future is the device's
    /// own [`AsyncPacketIo::send`] future, unchanged: this method adds no
    /// validation, mapping, allocation, or copy. A received [`PacketBuf`]
    /// derefs to `[u8]`, so it can be passed straight through.
    ///
    /// Use this, not `send`, inside async code. With the `tokio` feature
    /// the future must be polled by the Tokio runtime the device was opened
    /// under; it also works on a `current_thread` runtime, where the
    /// blocking `send` hangs. With the `async-io` feature any executor can
    /// poll it.
    ///
    /// There is no capability check and no fallback. A device that does
    /// not report `Capability::NATIVE_ASYNC` may return a future that
    /// blocks the thread polling it while it writes. The tun-rs backend
    /// reports `NATIVE_ASYNC` in every async build.
    ///
    /// ```no_run
    /// # #[cfg(any(feature = "async-io", feature = "tokio"))]
    /// # async fn example() -> tunnel_lattice::Result<()> {
    /// use futures::StreamExt;
    /// use tunnel_lattice::{DeviceConfig, DeviceKind, Tunnel};
    ///
    /// let tunnel = Tunnel::connect();
    /// let inside = tunnel.open(DeviceConfig::new(DeviceKind::Tun))?;
    /// let outside = tunnel.open(DeviceConfig::new(DeviceKind::Tun))?;
    /// let mut packets = inside.packet_stream(inside.snapshot()?.recv_buffer_len())?;
    /// if let Some(packet) = packets.next().await {
    ///     let packet = packet?;
    ///     // Forwards the pooled packet without copying it.
    ///     outside.send_async(&packet).await?;
    /// }
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Whatever the device's [`AsyncPacketIo::send`] returns. With the
    /// tun-rs backend these are the same results and errors as
    /// [`Handle::send`] in the same build: [`Error::Disconnected`] once the
    /// device is gone, [`Error::InvalidState`] while a Windows (Wintun)
    /// interface is disabled, and other native failures as the backend's
    /// error table maps them. An interrupted write is retried, not reported.
    ///
    /// # Cancellation
    ///
    /// Dropping the future before it completes is always safe and never
    /// sends part of a packet. With the tun-rs backend:
    ///
    /// - Linux, and macOS TUN (`utun`) and TAP (`feth`): a future dropped
    ///   before it completed sent nothing.
    /// - Windows (Wintun and tap-windows6): a future dropped before it
    ///   completed may or may not have sent the packet.
    ///
    /// The portable contract is "the whole packet at most once; unknown
    /// after drop". A caller that must know whether a packet went out has
    /// to await the future to completion.
    pub fn send_async(&self, buf: &[u8]) -> impl Future<Output = Result<usize>> + Send {
        AsyncPacketIo::send(&*self.device, buf)
    }
}

/// Mock-based tests of the facade's stream constructors (no privilege, no
/// real device).
#[cfg(all(test, feature = "async"))]
mod stream_tests {
    use std::future::Future;
    use std::sync::Arc;

    use futures::StreamExt;

    use super::*;

    /// A device with one packet, then `Disconnected`, reporting
    /// `NATIVE_ASYNC` or not.
    struct Mock {
        native: bool,
        packets: std::sync::Mutex<Vec<&'static [u8]>>,
    }

    impl Mock {
        fn handle(native: bool) -> Handle<Self> {
            Handle {
                device: Arc::new(Self {
                    native,
                    packets: std::sync::Mutex::new(vec![b"pkt"]),
                }),
                kind: DeviceKind::Tun,
            }
        }

        fn next(&self, buf: &mut [u8]) -> Result<usize> {
            let packet = self
                .packets
                .lock()
                .unwrap()
                .pop()
                .ok_or(Error::Disconnected)?;
            buf[..packet.len()].copy_from_slice(packet);
            Ok(packet.len())
        }
    }

    impl PacketIo for Mock {
        fn recv(&self, buf: &mut [u8]) -> Result<usize> {
            self.next(buf)
        }

        fn send(&self, buf: &[u8]) -> Result<usize> {
            Ok(buf.len())
        }
    }

    impl AsyncPacketIo for Mock {
        fn recv(&self, buf: &mut [u8]) -> impl Future<Output = Result<usize>> + Send {
            let result = self.next(buf);
            async move { result }
        }

        async fn send(&self, buf: &[u8]) -> Result<usize> {
            Ok(buf.len())
        }
    }

    impl CapabilityProvider for Mock {
        fn capabilities(&self) -> Capability {
            if self.native {
                Capability::NATIVE_ASYNC
            } else {
                Capability::empty()
            }
        }
    }

    #[test]
    fn packet_stream_rejects_a_zero_buf_len_on_both_branches() {
        for native in [true, false] {
            let handle = Mock::handle(native);
            assert!(matches!(handle.packet_stream(0), Err(Error::InvalidState)));
            assert_eq!(Arc::strong_count(&handle.device), 1, "native: {native}");
        }
    }

    #[test]
    fn packet_stream_yields_pooled_packets_on_both_branches() {
        for native in [true, false] {
            let handle = Mock::handle(native);
            let pool = PacketPool::new(2, 8).unwrap();
            let mut stream = handle.packet_stream_with_pool(pool.clone());
            let first = futures::executor::block_on(stream.next()).unwrap().unwrap();
            assert_eq!(first, *b"pkt", "native: {native}");
            assert_eq!(pool.available(), 1);
            drop(first);
            assert!(matches!(
                futures::executor::block_on(stream.next()),
                Some(Err(Error::Disconnected))
            ));
            assert!(futures::executor::block_on(stream.next()).is_none());
        }
    }
}

/// Ordinary tests of which platform-specific surfaces the tun-rs backend
/// exposes through the facade on this target (no privilege, no device).
#[cfg(all(test, feature = "tun-rs"))]
mod platform_surface_tests {
    use super::*;

    /// Answers "does `T` implement this trait?" at compile time, for a
    /// concrete `T`: the inherent constant applies only when the bound
    /// holds, and path resolution prefers it over the [`Fallback`] trait's
    /// constant, which applies to every type.
    struct Implements<T>(std::marker::PhantomData<T>);

    #[allow(dead_code)]
    impl<T: PersistentDevice> Implements<T> {
        const PERSISTENT: bool = true;
    }

    #[allow(dead_code)]
    impl<T: MultiQueueProvider> Implements<T> {
        const MULTI_QUEUE: bool = true;
    }

    #[allow(dead_code)]
    trait Fallback {
        const PERSISTENT: bool = false;
        const MULTI_QUEUE: bool = false;
    }

    impl<T> Fallback for T {}

    /// `Handle::persist`/`unpersist` and `Handle::additional_queue` exist
    /// exactly where `TunRsDevice` implements the trait behind them: on
    /// Linux. Elsewhere they are absent at compile time, not methods that
    /// return `Error::Unsupported`.
    #[test]
    fn persistence_and_multi_queue_are_implemented_only_on_linux() {
        let linux = cfg!(target_os = "linux");
        assert_eq!(Implements::<TunRsDevice>::PERSISTENT, linux);
        assert_eq!(Implements::<TunRsDevice>::MULTI_QUEUE, linux);
    }

    #[test]
    #[cfg(not(target_os = "linux"))]
    fn persistence_and_multi_queue_flags_are_absent_off_linux() {
        let host = Tunnel::connect().capabilities();
        assert!(!host.contains(Capability::PERSISTENT_DEVICES), "{host:?}");
        assert!(!host.contains(Capability::MULTI_QUEUE), "{host:?}");
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn persistence_and_multi_queue_flags_are_present_on_linux() {
        let host = Tunnel::connect().capabilities();
        assert!(host.contains(Capability::PERSISTENT_DEVICES), "{host:?}");
        assert!(host.contains(Capability::MULTI_QUEUE), "{host:?}");
    }
}

/// Privileged tests against a real device through the facade (run in CI
/// with `cargo test -p tunnel-lattice --lib` in each feature set and
/// `-- --ignored`): the exact capability sets per OS, concurrent `recv` and
/// `send` through cloned `Handle`s, and, with an async feature,
/// `Handle::packet_stream` and `Handle::send_async`. See
/// `tunnel-lattice-backend-tunrs`'s `privileged_tests` module for why these
/// are `#[ignore]`d and how to run them, and `tunnel-lattice-async`'s own
/// unit tests for the cancellation-semantics proof against a mock (no
/// privilege needed there).
#[cfg(all(test, feature = "tun-rs"))]
mod privileged_tests {
    #[cfg(feature = "async")]
    use futures::FutureExt;

    use super::*;

    #[cfg(feature = "tokio")]
    fn enter_tokio_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .enable_io()
            .build()
            .expect("build a Tokio runtime")
    }

    #[cfg(feature = "async")]
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

        let buf_len = device.snapshot().expect("snapshot").recv_buffer_len();
        assert_eq!(buf_len, 1400, "a TUN buffer is exactly the MTU");
        let mut stream = device.packet_stream(buf_len).expect("a valid buf_len");
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

    /// A well-formed 32-byte IPv4/UDP packet between two TEST-NET-1
    /// addresses (192.0.2.1 -> 192.0.2.2, port 9 "discard"), with a valid
    /// header checksum and the UDP checksum left at zero (allowed for
    /// IPv4). The host drops it after routing; the write itself succeeds.
    #[cfg(feature = "async")]
    fn ipv4_udp_packet() -> [u8; 32] {
        let mut packet = [0u8; 32];
        packet[0] = 0x45; // version 4, header length 5 words
        packet[2..4].copy_from_slice(&32u16.to_be_bytes()); // total length
        packet[8] = 64; // TTL
        packet[9] = 17; // UDP
        packet[12..16].copy_from_slice(&[192, 0, 2, 1]);
        packet[16..20].copy_from_slice(&[192, 0, 2, 2]);
        let checksum = !ones_complement_sum(&packet[..20]);
        packet[10..12].copy_from_slice(&checksum.to_be_bytes());
        packet[20..22].copy_from_slice(&40000u16.to_be_bytes()); // source port
        packet[22..24].copy_from_slice(&9u16.to_be_bytes()); // destination port
        packet[24..26].copy_from_slice(&12u16.to_be_bytes()); // UDP length
        packet[28..32].copy_from_slice(b"ping");
        packet
    }

    /// The 16-bit one's-complement sum of `bytes` (RFC 1071).
    fn ones_complement_sum(bytes: &[u8]) -> u16 {
        let (words, rest) = bytes.as_chunks::<2>();
        assert!(rest.is_empty(), "an even number of bytes");
        let mut sum = words
            .iter()
            .map(|word| u32::from(u16::from_be_bytes(*word)))
            .sum::<u32>();
        while sum > 0xffff {
            sum = (sum & 0xffff) + (sum >> 16);
        }
        u16::try_from(sum).expect("folded into 16 bits")
    }

    #[cfg(feature = "async")]
    #[test]
    fn the_test_packet_has_a_valid_ipv4_header_checksum() {
        let packet = ipv4_udp_packet();
        assert_eq!(
            ones_complement_sum(&packet[..20]),
            0xffff,
            "a header including its checksum sums to 0xffff"
        );
        assert_eq!(
            usize::from(u16::from_be_bytes([packet[2], packet[3]])),
            packet.len()
        );
    }

    /// Opens a TUN device and sends one packet through `send_async`.
    #[cfg(feature = "async")]
    async fn open_and_send_async() -> Result<usize> {
        let device = Tunnel::connect().open(DeviceConfig::new(DeviceKind::Tun).with_mtu(1400))?;
        device.send_async(&ipv4_udp_packet()).await
    }

    /// The case the blocking `Handle::send` panics on ("Cannot start a
    /// runtime from within a runtime"): a send from inside a spawned task.
    #[cfg(feature = "tokio")]
    #[test]
    #[ignore = "requires CAP_NET_ADMIN/Administrator/root to open a TUN device"]
    fn send_async_works_inside_a_task_on_a_multi_thread_tokio_runtime() {
        let runtime = enter_tokio_runtime();
        let sent = runtime
            .block_on(async { tokio::spawn(open_and_send_async()).await })
            .expect("the task does not panic");
        assert!(matches!(sent, Ok(32)), "send_async: {sent:?}");
    }

    /// The blocking `Handle::send` hangs on a `current_thread` runtime;
    /// `send_async` is polled by the runtime itself, so it does not.
    #[cfg(feature = "tokio")]
    #[test]
    #[ignore = "requires CAP_NET_ADMIN/Administrator/root to open a TUN device"]
    fn send_async_works_on_a_current_thread_tokio_runtime() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()
            .expect("build a current_thread Tokio runtime");
        let sent = runtime.block_on(open_and_send_async());
        assert!(matches!(sent, Ok(32)), "send_async: {sent:?}");
    }

    #[cfg(all(feature = "async-io", not(feature = "tokio")))]
    #[test]
    #[ignore = "requires CAP_NET_ADMIN/Administrator/root to open a TUN device"]
    fn send_async_works_under_a_foreign_executor_with_async_io() {
        let sent = futures::executor::block_on(open_and_send_async());
        assert!(matches!(sent, Ok(32)), "send_async: {sent:?}");
    }

    /// The exact host-level set the tun-rs backend reports on this OS and
    /// feature set, written out per OS rather than derived from the
    /// backend's own code. On Windows it assumes the tap-windows6 driver is
    /// staged, as the privileged CI job does before running these tests.
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    fn expected_host_capabilities() -> Capability {
        #[cfg(target_os = "linux")]
        let host = Capability::DEVICE_MUTATION
            | Capability::TAP_DEVICES
            | Capability::PERSISTENT_DEVICES
            | Capability::MULTI_QUEUE;
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        let host = Capability::DEVICE_MUTATION | Capability::TAP_DEVICES;
        if cfg!(any(feature = "async-io", feature = "tokio")) {
            host | Capability::NATIVE_ASYNC
        } else {
            host
        }
    }

    /// `Tunnel::capabilities()` and a real TUN handle's `capabilities()`
    /// are both exactly the expected host set: no per-handle flag on TUN,
    /// `NATIVE_ASYNC` exactly in the async builds, the Linux-only flags
    /// exactly on Linux.
    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    #[ignore = "requires CAP_NET_ADMIN/Administrator/root to open a TUN device, and on Windows the tap-windows6 driver staged"]
    fn tun_capabilities_are_exactly_the_host_set_on_a_real_device() {
        #[cfg(feature = "tokio")]
        let _runtime = enter_tokio_runtime();
        #[cfg(feature = "tokio")]
        let _entered = _runtime.enter();

        let expected = expected_host_capabilities();
        let tunnel = Tunnel::connect();
        let host = tunnel.capabilities();
        eprintln!("host capabilities: {host:?}");
        assert_eq!(host, expected, "Tunnel::capabilities()");

        let device = tunnel
            .open(DeviceConfig::new(DeviceKind::Tun))
            .expect("open a TUN device");
        let handle = device.capabilities();
        eprintln!("TUN handle capabilities: {handle:?}");
        assert_eq!(handle, expected, "a TUN handle's capabilities()");
        assert_eq!(
            device.clone().capabilities(),
            handle,
            "a clone reports the same set"
        );
        assert_eq!(tunnel.capabilities(), expected, "unchanged by an open");
    }

    /// A real TAP handle reports the expected host set plus
    /// `MAC_MUTATION` on Linux and macOS, and exactly the host set on
    /// Windows, where the driver takes a MAC only at creation.
    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    #[ignore = "requires CAP_NET_ADMIN/Administrator/root to open a TAP device, and on Windows the tap-windows6 driver staged"]
    fn tap_capabilities_are_exactly_the_host_set_plus_mac_mutation_where_supported() {
        #[cfg(feature = "tokio")]
        let _runtime = enter_tokio_runtime();
        #[cfg(feature = "tokio")]
        let _entered = _runtime.enter();

        let host = expected_host_capabilities();
        let expected = if cfg!(target_os = "windows") {
            host
        } else {
            host | Capability::MAC_MUTATION
        };
        let tunnel = Tunnel::connect();
        assert_eq!(tunnel.capabilities(), host, "Tunnel::capabilities()");

        let device = tunnel
            .open(DeviceConfig::new(DeviceKind::Tap))
            .expect("open a TAP device");
        assert_eq!(device.kind(), DeviceKind::Tap);
        let handle = device.capabilities();
        eprintln!("TAP handle capabilities: {handle:?}");
        assert_eq!(handle, expected, "a TAP handle's capabilities()");
        assert_eq!(tunnel.capabilities(), host, "unchanged by an open");
    }

    /// The payload of every echo request the concurrency tests send; the
    /// host's echo reply carries it back unchanged.
    const ECHO_PAYLOAD: &[u8; 16] = b"lattice-echo-req";

    /// How long the concurrency tests wait for the echo reply before they
    /// fail, and then for a released receiver to return.
    const REPLY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
    const RELEASE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

    /// An IPv4 ICMP echo request from `source` to `destination` with
    /// identifier `ident`, sequence number 1, and [`ECHO_PAYLOAD`], with
    /// valid IPv4 header and ICMP checksums.
    fn icmp_echo_request(source: [u8; 4], destination: [u8; 4], ident: u16) -> Vec<u8> {
        let total = 20 + 8 + ECHO_PAYLOAD.len();
        let mut packet = vec![0u8; total];
        packet[0] = 0x45; // version 4, header length 5 words
        packet[2..4].copy_from_slice(&u16::try_from(total).expect("fits").to_be_bytes());
        packet[8] = 64; // TTL
        packet[9] = 1; // ICMP
        packet[12..16].copy_from_slice(&source);
        packet[16..20].copy_from_slice(&destination);
        let header = !ones_complement_sum(&packet[..20]);
        packet[10..12].copy_from_slice(&header.to_be_bytes());
        packet[20] = 8; // echo request, code 0
        packet[24..26].copy_from_slice(&ident.to_be_bytes());
        packet[26..28].copy_from_slice(&1u16.to_be_bytes()); // sequence
        packet[28..].copy_from_slice(ECHO_PAYLOAD);
        let icmp = !ones_complement_sum(&packet[20..]);
        packet[22..24].copy_from_slice(&icmp.to_be_bytes());
        packet
    }

    /// Whether `packet` is the IPv4 echo reply from `local` to `peer` that
    /// answers [`icmp_echo_request`]`(peer, local, ident)`: identifier,
    /// sequence number, and payload all match.
    fn is_echo_reply(packet: &[u8], local: [u8; 4], peer: [u8; 4], ident: u16) -> bool {
        if packet.len() < 20 || packet[0] >> 4 != 4 || packet[9] != 1 {
            return false;
        }
        let header_len = usize::from(packet[0] & 0x0f) * 4;
        let Some(icmp) = packet.get(header_len..) else {
            return false;
        };
        packet[12..16] == local
            && packet[16..20] == peer
            && icmp.len() == 8 + ECHO_PAYLOAD.len()
            && icmp[0] == 0 // echo reply
            && icmp[1] == 0
            && icmp[4..6] == ident.to_be_bytes()
            && icmp[6..8] == 1u16.to_be_bytes()
            && icmp[8..] == ECHO_PAYLOAD[..]
    }

    #[test]
    fn the_echo_request_has_valid_checksums_and_its_reply_is_recognized() {
        let (local, peer) = ([10, 202, 7, 1], [10, 202, 7, 2]);
        let request = icmp_echo_request(peer, local, 0x1234);
        assert_eq!(ones_complement_sum(&request[..20]), 0xffff, "IPv4 header");
        assert_eq!(ones_complement_sum(&request[20..]), 0xffff, "ICMP message");
        assert!(!is_echo_reply(&request, local, peer, 0x1234), "a request");
        // What the host answers: addresses swapped, type 0, the same
        // identifier, sequence, and payload.
        let mut reply = icmp_echo_request(local, peer, 0x1234);
        reply[20] = 0;
        assert!(is_echo_reply(&reply, local, peer, 0x1234));
        assert!(!is_echo_reply(&reply, local, peer, 0x1235), "another ident");
        assert!(
            !is_echo_reply(&reply[..30], local, peer, 0x1234),
            "truncated"
        );
    }

    /// A per-process value, so parallel runs on one host use different
    /// subnets and identifiers.
    fn per_process(modulus: u32) -> u32 {
        std::process::id() % modulus
    }

    /// Runs a host network-configuration command on the test's own device,
    /// panicking with its output if it fails.
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

    /// Gives the test's own TUN device the address `subnet.1` with the peer
    /// `subnet.2` reachable through it, and brings it up. Returns
    /// `(local, peer)`. An echo request from the peer written into the
    /// device is answered by the host with an echo reply routed back into
    /// the same device. The address lives on the device and goes away with
    /// it.
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    fn address_tun_device(name: &str, subnet: [u8; 3]) -> ([u8; 4], [u8; 4]) {
        let [a, b, c] = subnet;
        let (local, peer) = ([a, b, c, 1], [a, b, c, 2]);
        let local_text = format!("{a}.{b}.{c}.1");
        #[cfg(target_os = "linux")]
        {
            run_host_command(
                "ip",
                &["addr", "add", &format!("{local_text}/24"), "dev", name],
            );
            run_host_command("ip", &["link", "set", "dev", name, "up"]);
        }
        #[cfg(target_os = "macos")]
        run_host_command(
            "ifconfig",
            &[name, "inet", &local_text, &format!("{a}.{b}.{c}.2"), "up"],
        );
        // As in the backend's tests: a fresh Wintun adapter is not always
        // registered with the IP helper yet, so netsh is retried for a
        // bounded time.
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
                &local_text,
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
        (local, peer)
    }

    /// Makes a `recv` blocked on the test's own TUN device return, best
    /// effort, so a failing test can join its receiver and drop the device
    /// before it panics: Linux deletes the device, Windows disables the
    /// Wintun adapter (which ends its session). A macOS `utun` cannot be
    /// destroyed from outside; like the other two, it goes away when the
    /// test process exits.
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    fn release_blocked_recv(device: &Handle<TunRsDevice>, name: &str) {
        #[cfg(target_os = "linux")]
        {
            let _ = device;
            let _ = std::process::Command::new("ip")
                .args(["link", "del", name])
                .output();
        }
        #[cfg(target_os = "macos")]
        let _ = (device, name);
        #[cfg(target_os = "windows")]
        {
            let _ = name;
            if let Ok(patch) =
                DeviceConfigPatch::new(device.id(), Some(DesiredAdminState::Down), None)
            {
                let _ = device.apply(patch);
            }
        }
    }

    /// Sends `request` through `send` every 200 ms until `stop` is set,
    /// starting 200 ms in so the receiver is already waiting. Repeats
    /// because the host may not answer at once (for example while Windows
    /// still checks the new address for duplicates). Returns how many
    /// requests went out, or the first send error.
    fn send_until_stopped(
        stop: &std::sync::atomic::AtomicBool,
        request: &[u8],
        mut send: impl FnMut(&[u8]) -> Result<usize>,
    ) -> std::result::Result<usize, String> {
        let mut sent = 0;
        loop {
            std::thread::sleep(std::time::Duration::from_millis(200));
            if stop.load(std::sync::atomic::Ordering::Acquire) {
                return Ok(sent);
            }
            match send(request) {
                Ok(n) if n == request.len() => sent += 1,
                other => return Err(format!("send #{}: {other:?}", sent + 1)),
            }
        }
    }

    /// The outcome of one concurrent receive/send check.
    struct Concurrency {
        /// The receiver's result, `None` if it had not returned within
        /// [`REPLY_TIMEOUT`] (it was then released and given
        /// [`RELEASE_TIMEOUT`] more).
        received: Option<std::result::Result<String, String>>,
        /// Whether the receiver returned at all, even after a release.
        receiver_returned: bool,
        /// The sender's result, `None` if it did not stop in time.
        sent: Option<std::result::Result<usize, String>>,
    }

    impl Concurrency {
        /// Panics with every problem found, after the caller dropped the
        /// device.
        fn assert_ok(self, label: &str) {
            eprintln!(
                "{label}: received {:?}, sent {:?}",
                self.received, self.sent
            );
            let mut problems = Vec::new();
            match &self.received {
                Some(Ok(_)) => {}
                Some(Err(err)) => problems.push(format!("receiver failed: {err}")),
                None => problems.push(format!(
                    "no echo reply within {REPLY_TIMEOUT:?} (receiver returned after release: {})",
                    self.receiver_returned
                )),
            }
            match &self.sent {
                Some(Ok(sent)) if *sent > 0 => {}
                Some(Ok(_)) => problems.push("no request was sent".to_owned()),
                Some(Err(err)) => problems.push(format!("sender failed: {err}")),
                None => problems.push(format!(
                    "the sender did not stop within {RELEASE_TIMEOUT:?}: send may be deadlocked"
                )),
            }
            assert!(problems.is_empty(), "{label}: {}", problems.join("; "));
        }
    }

    /// Two clones of one `Handle` on two threads: one blocks in
    /// `Handle::recv` while the other writes ICMP echo requests with
    /// `Handle::send`; the host's echo reply must reach the blocked `recv`
    /// within [`REPLY_TIMEOUT`]. In the `tokio` build both threads enter
    /// the test's multi-thread runtime, as the blocking calls require.
    ///
    /// On a timeout the receiver is released ([`release_blocked_recv`]) and
    /// joined if it returns; the sender is joined once it stops. The device
    /// is dropped before any assertion fails.
    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    #[ignore = "requires CAP_NET_ADMIN/Administrator/root to open and address a TUN device"]
    fn cloned_handles_recv_and_send_concurrently_on_two_threads() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::mpsc;

        #[cfg(feature = "tokio")]
        let runtime = enter_tokio_runtime();
        #[cfg(feature = "tokio")]
        let _entered = runtime.enter();

        let device = Tunnel::connect()
            .open(DeviceConfig::new(DeviceKind::Tun).with_mtu(1400))
            .expect("open a TUN device");
        let snapshot = device.snapshot().expect("snapshot the device");
        let octet = u8::try_from(per_process(200)).expect("below 200") + 20;
        let (local, peer) = address_tun_device(&snapshot.name, [10, 202, octet]);
        let ident = u16::try_from(per_process(0x8000)).expect("below 0x8000");
        let request = icmp_echo_request(peer, local, ident);
        let buf_len = snapshot.recv_buffer_len();

        let (received_tx, received_rx) = mpsc::channel();
        let receiver = {
            let reader = device.clone();
            #[cfg(feature = "tokio")]
            let runtime = runtime.handle().clone();
            std::thread::spawn(move || {
                #[cfg(feature = "tokio")]
                let _entered = runtime.enter();
                let mut buf = vec![0u8; buf_len];
                let mut skipped = 0usize;
                let result = loop {
                    match reader.recv(&mut buf) {
                        Ok(n) if is_echo_reply(&buf[..n], local, peer, ident) => {
                            break Ok(format!("{n}-byte echo reply after {skipped} other packets"));
                        }
                        Ok(_) | Err(Error::BufferTooSmall) => skipped += 1,
                        Err(err) => break Err(format!("recv: {err:?}")),
                    }
                };
                let _ = received_tx.send(result);
            })
        };

        let stop = Arc::new(AtomicBool::new(false));
        let (sent_tx, sent_rx) = mpsc::channel();
        let sender = {
            let writer = device.clone();
            let stop = Arc::clone(&stop);
            #[cfg(feature = "tokio")]
            let runtime = runtime.handle().clone();
            std::thread::spawn(move || {
                #[cfg(feature = "tokio")]
                let _entered = runtime.enter();
                let result = send_until_stopped(&stop, &request, |packet| writer.send(packet));
                let _ = sent_tx.send(result);
            })
        };

        let received = received_rx.recv_timeout(REPLY_TIMEOUT).ok();
        stop.store(true, Ordering::Release);
        let receiver_returned = if received.is_some() {
            true
        } else {
            release_blocked_recv(&device, &snapshot.name);
            received_rx.recv_timeout(RELEASE_TIMEOUT).is_ok()
        };
        let sent = sent_rx.recv_timeout(RELEASE_TIMEOUT).ok();
        if sent.is_some() {
            sender.join().expect("the sender thread does not panic");
        }
        if receiver_returned {
            receiver.join().expect("the receiver thread does not panic");
        }
        drop(device);
        Concurrency {
            received,
            receiver_returned,
            sent,
        }
        .assert_ok("threads, blocking recv/send");
    }

    /// The async counterpart: a `packet_stream` on one clone is polled as
    /// a task (on the Tokio runtime with `tokio`, on its own thread with
    /// `async-io`) while another thread sends the echo requests with
    /// `send_async` through a second clone; the reply must come out of the
    /// stream. On a timeout the stream is cancelled, which drops it and
    /// its in-flight receive.
    #[cfg(feature = "async")]
    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    #[ignore = "requires CAP_NET_ADMIN/Administrator/root to open and address a TUN device"]
    fn cloned_handles_stream_and_send_async_concurrently() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::mpsc;

        use futures::StreamExt;

        #[cfg(feature = "tokio")]
        let runtime = enter_tokio_runtime();
        #[cfg(feature = "tokio")]
        let _entered = runtime.enter();

        let device = Tunnel::connect()
            .open(DeviceConfig::new(DeviceKind::Tun).with_mtu(1400))
            .expect("open a TUN device");
        let snapshot = device.snapshot().expect("snapshot the device");
        let octet = u8::try_from(per_process(200)).expect("below 200") + 20;
        let (local, peer) = address_tun_device(&snapshot.name, [10, 203, octet]);
        let ident = u16::try_from(per_process(0x8000)).expect("below 0x8000") | 0x8000;
        let request = icmp_echo_request(peer, local, ident);

        let mut stream = device
            .clone()
            .packet_stream(snapshot.recv_buffer_len())
            .expect("a valid buf_len");
        let (cancel_tx, cancel_rx) = futures::channel::oneshot::channel::<()>();
        let (received_tx, received_rx) = mpsc::channel();
        let receive = async move {
            let mut skipped = 0usize;
            let wait = async {
                while let Some(item) = stream.next().await {
                    match item {
                        Ok(packet) if is_echo_reply(&packet, local, peer, ident) => {
                            return Ok(format!(
                                "{}-byte echo reply after {skipped} other packets",
                                packet.len()
                            ));
                        }
                        Ok(_) | Err(Error::BufferTooSmall) => skipped += 1,
                        Err(err) => return Err(format!("stream item: {err:?}")),
                    }
                }
                Err("the stream ended".to_owned())
            };
            let result = match futures::future::select(Box::pin(wait), cancel_rx).await {
                futures::future::Either::Left((result, _)) => result,
                futures::future::Either::Right(_) => Err("cancelled".to_owned()),
            };
            let _ = received_tx.send(result);
        };
        #[cfg(feature = "tokio")]
        let receiver = runtime.spawn(receive);
        #[cfg(not(feature = "tokio"))]
        let receiver = std::thread::spawn(move || futures::executor::block_on(receive));

        let stop = Arc::new(AtomicBool::new(false));
        let (sent_tx, sent_rx) = mpsc::channel();
        let sender = {
            let writer = device.clone();
            let stop = Arc::clone(&stop);
            #[cfg(feature = "tokio")]
            let runtime = runtime.handle().clone();
            std::thread::spawn(move || {
                let result = send_until_stopped(&stop, &request, |packet| {
                    #[cfg(feature = "tokio")]
                    {
                        runtime.block_on(writer.send_async(packet))
                    }
                    #[cfg(not(feature = "tokio"))]
                    {
                        futures::executor::block_on(writer.send_async(packet))
                    }
                });
                let _ = sent_tx.send(result);
            })
        };

        let received = received_rx.recv_timeout(REPLY_TIMEOUT).ok();
        stop.store(true, Ordering::Release);
        let receiver_returned = if received.is_some() {
            true
        } else {
            let _ = cancel_tx.send(());
            received_rx.recv_timeout(RELEASE_TIMEOUT).is_ok()
        };
        let sent = sent_rx.recv_timeout(RELEASE_TIMEOUT).ok();
        if sent.is_some() {
            sender.join().expect("the sender thread does not panic");
        }
        if receiver_returned {
            #[cfg(feature = "tokio")]
            runtime
                .block_on(receiver)
                .expect("the receiver task does not panic");
            #[cfg(not(feature = "tokio"))]
            receiver.join().expect("the receiver thread does not panic");
        }
        drop(device);
        Concurrency {
            received,
            receiver_returned,
            sent,
        }
        .assert_ok("packet_stream + send_async");
    }
}
