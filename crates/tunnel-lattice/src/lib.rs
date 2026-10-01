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

/// Privileged, `async`-feature-only tests exercising `Handle::packet_stream`
/// and `Handle::send_async` against a real device (run in CI with
/// `cargo test -p tunnel-lattice --lib` plus an async feature and
/// `-- --ignored`) — see `tunnel-lattice-backend-tunrs`'s
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
        let mut sum = bytes
            .chunks_exact(2)
            .map(|word| u32::from(u16::from_be_bytes([word[0], word[1]])))
            .sum::<u32>();
        while sum > 0xffff {
            sum = (sum & 0xffff) + (sum >> 16);
        }
        u16::try_from(sum).expect("folded into 16 bits")
    }

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

    #[cfg(not(feature = "tokio"))]
    #[test]
    #[ignore = "requires CAP_NET_ADMIN/Administrator/root to open a TUN device"]
    fn send_async_works_under_a_foreign_executor_with_async_io() {
        let sent = futures::executor::block_on(open_and_send_async());
        assert!(matches!(sent, Ok(32)), "send_async: {sent:?}");
    }
}
