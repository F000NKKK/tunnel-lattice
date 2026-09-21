//! The contract between the model and platform backends.
//!
//! `tunnel-lattice-platform` depends only on `tunnel-lattice-core` — never on
//! `tunnel-lattice-model` — the same boundary `net-lattice-platform` holds in
//! the sibling Lattice ecosystem. Its provider traits describe the *shape*
//! of a backend, not the *content* of the model, via associated types; the
//! `tunnel-lattice` facade is what binds those associated types to the
//! concrete `tunnel_lattice_model` types. See ARCHITECTURE.md, "Backend
//! replacement plan," for why this boundary exists: it is what lets a
//! `tun-rs`-backed implementation be replaced later by hand-written
//! per-OS TUN/TAP code without moving this crate or the facade's public API.
//!
//! Provides [`DeviceProvider`] for opening a device, [`PacketIo`] for
//! synchronous packet transfer on an open device, [`DeviceMutator`] for
//! post-open MTU/administrative-state changes gated by `Capability`,
//! [`CapabilityProvider`] for the runtime feature-flag contract,
//! [`PersistentDevice`] for marking a device to survive process exit
//! (`Capability::PERSISTENT_DEVICES`), and [`MultiQueueProvider`] for
//! duplicating a hardware-scheduled queue (`Capability::MULTI_QUEUE`). The
//! `async` feature additionally provides [`AsyncPacketIo`] for a native
//! async transfer path — see that trait's docs for when a backend should
//! implement it instead of relying on `tunnel-lattice-async`'s thread-based
//! adapter over [`PacketIo`].

#![warn(missing_docs)]

#[cfg(feature = "async")]
mod async_packet_io;
mod capability;
mod device_mutator;
mod device_observer;
mod device_provider;
mod multi_queue_provider;
mod packet_io;
mod persistent_device;

#[cfg(feature = "async")]
pub use async_packet_io::AsyncPacketIo;
pub use capability::{Capability, CapabilityProvider};
pub use device_mutator::DeviceMutator;
pub use device_observer::DeviceObserver;
pub use device_provider::DeviceProvider;
pub use multi_queue_provider::MultiQueueProvider;
pub use packet_io::PacketIo;
pub use persistent_device::PersistentDevice;
