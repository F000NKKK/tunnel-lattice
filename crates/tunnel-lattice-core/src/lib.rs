//! Foundational types with no tunnel-specific semantics of their own.
//!
//! `tunnel-lattice-core` carries no OS dependency and no TUN/TAP-specific
//! types — those belong to `tunnel-lattice-model`. Mirrors `net-lattice-core`
//! in the sibling Lattice ecosystem so both share one error/result shape.

#![warn(missing_docs)]

mod error;
mod id;

pub use error::{Error, PlatformErrorCode};
pub use id::Id;

/// A result returned by Tunnel Lattice operations.
pub type Result<T> = core::result::Result<T, Error>;
