//! Operating-system-independent TUN/TAP domain types for Tunnel Lattice.
//!
//! This crate models data and contracts; it never inspects or mutates the
//! host system — mirrors `net-lattice-model`'s separation in the sibling
//! Lattice ecosystem. See ARCHITECTURE.md for the full rationale.

#![warn(missing_docs)]

mod device;

pub use device::{
    AdminState, DesiredAdminState, Device, DeviceConfig, DeviceConfigPatch, DeviceId, DeviceKind,
};
