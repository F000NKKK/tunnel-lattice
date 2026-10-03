//! Operating-system-independent TUN/TAP domain types for Tunnel Lattice.
//!
//! This crate models data and contracts; it never inspects or mutates the
//! host system — mirrors `net-lattice-model`'s separation in the sibling
//! Lattice ecosystem. See ARCHITECTURE.md for the full rationale.

#![warn(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]

#[cfg(feature = "backend")]
#[cfg_attr(docsrs, doc(cfg(feature = "backend")))]
pub mod backend;
mod device;
mod mac;

pub use device::{
    AdminState, DesiredAdminState, Device, DeviceConfig, DeviceConfigPatch, DeviceId, DeviceKind,
};
pub use mac::MacAddress;
