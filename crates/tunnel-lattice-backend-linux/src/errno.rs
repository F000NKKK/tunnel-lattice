//! Raw `errno` to [`Error`] mapping.
//!
//! Every native failure in this crate is classified from its raw `errno` and
//! the operation that produced it, never from `std::io::ErrorKind`: the same
//! errno means different things in different places (`ENODEV` from opening
//! `/dev/net/tun` is a missing driver, from a netlink request by ifindex it
//! is a missing device), and `ErrorKind` folds distinctions this backend's
//! contract depends on.
//!
//! The table itself (`Op`, `Class`, `classify`, `classify_io`,
//! `set_iff_einval` and `platform`) is shared with the other backends and
//! lives in `tunnel_lattice_platform::backend::errno::linux`, where it is
//! documented and tested; this module re-exports it and adds the one error
//! that is specific to this crate's netlink decoding.

use tunnel_lattice_core::{Error, PlatformErrorCode};

pub(crate) use tunnel_lattice_platform::backend::errno::linux::{
    Class, Op, classify, classify_io, platform, set_iff_einval,
};

/// The error for a reply that does not follow the netlink protocol (a
/// truncated or undecodable message). It carries no errno, so it is
/// `Platform(Unknown)`.
pub(crate) fn malformed() -> Error {
    Error::Platform(PlatformErrorCode::Unknown)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_malformed_reply_carries_no_errno() {
        assert!(matches!(
            malformed(),
            Error::Platform(PlatformErrorCode::Unknown)
        ));
    }
}
