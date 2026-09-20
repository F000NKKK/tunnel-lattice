use std::fmt;

/// The single error type surfaced across the Tunnel Lattice workspace.
///
/// Provider trait methods return `Result<T, Error>` — never a raw OS error
/// type (`std::io::Error`, a bare `errno`, a Windows `DWORD`), matching
/// `net-lattice-core::Error`'s contract.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// The operation requires privileges the caller does not have (creating
    /// or configuring a TUN/TAP device generally requires `CAP_NET_ADMIN` on
    /// Linux, Administrator on Windows, or root on macOS/BSD).
    PermissionDenied,
    /// The referenced device does not exist.
    NotFound,
    /// A device with the same identity already exists.
    AlreadyExists,
    /// The operation has no meaning on this backend at all, as opposed to a
    /// `Capability` being merely absent at runtime.
    Unsupported,
    /// The operation is not valid given the device's current state (e.g.
    /// writing to a device that has already been closed).
    InvalidState,
    /// The device's read/write channel has shut down — no further packets
    /// will ever arrive or be delivered. Distinct from a timeout: this means
    /// the device is gone for good, typically because the kernel torn it
    /// down or the owning handle was dropped.
    Disconnected,
    /// Escape hatch preserving the raw backend-specific error for
    /// diagnostics. Not the primary way consumers are expected to match on
    /// failures.
    Platform(PlatformErrorCode),
}

/// A platform-tagged raw error code.
///
/// Linux errno is a signed `i32`, Windows error codes are an unsigned
/// `DWORD` (`u32`); collapsing both into one untyped integer would either
/// truncate one of them or imply the two are comparable, which they are not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlatformErrorCode {
    /// A Linux `errno` value.
    Linux(i32),
    /// A Windows error code (`DWORD`).
    Windows(u32),
    /// A Darwin (macOS) `errno` value.
    Darwin(i32),
}

impl Error {
    /// Returns `true` if this is [`Error::PermissionDenied`].
    pub const fn is_permission_denied(&self) -> bool {
        matches!(self, Error::PermissionDenied)
    }

    /// Returns `true` if this is [`Error::NotFound`].
    pub const fn is_not_found(&self) -> bool {
        matches!(self, Error::NotFound)
    }

    /// Returns `true` if this is [`Error::AlreadyExists`].
    pub const fn is_already_exists(&self) -> bool {
        matches!(self, Error::AlreadyExists)
    }

    /// Returns `true` if this is [`Error::Unsupported`].
    pub const fn is_unsupported(&self) -> bool {
        matches!(self, Error::Unsupported)
    }

    /// Returns `true` if this is [`Error::InvalidState`].
    pub const fn is_invalid_state(&self) -> bool {
        matches!(self, Error::InvalidState)
    }

    /// Returns `true` if this is [`Error::Disconnected`].
    pub const fn is_disconnected(&self) -> bool {
        matches!(self, Error::Disconnected)
    }

    /// Returns `true` if this is [`Error::Platform`].
    pub const fn is_platform(&self) -> bool {
        matches!(self, Error::Platform(_))
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::PermissionDenied => write!(f, "permission denied"),
            Error::NotFound => write!(f, "not found"),
            Error::AlreadyExists => write!(f, "already exists"),
            Error::Unsupported => write!(f, "unsupported operation"),
            Error::InvalidState => write!(f, "invalid state"),
            Error::Disconnected => write!(f, "device channel disconnected"),
            Error::Platform(code) => write!(f, "platform error: {code:?}"),
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_public_error_has_a_stable_display_message() {
        let cases = [
            (Error::PermissionDenied, "permission denied"),
            (Error::NotFound, "not found"),
            (Error::AlreadyExists, "already exists"),
            (Error::Unsupported, "unsupported operation"),
            (Error::InvalidState, "invalid state"),
            (Error::Disconnected, "device channel disconnected"),
        ];
        for (error, expected) in cases {
            assert_eq!(error.to_string(), expected);
        }
        assert_eq!(
            Error::Platform(PlatformErrorCode::Linux(-1)).to_string(),
            "platform error: Linux(-1)"
        );
    }

    #[test]
    fn platform_error_codes_preserve_their_platform_and_value() {
        assert_eq!(PlatformErrorCode::Linux(-1), PlatformErrorCode::Linux(-1));
        assert_ne!(PlatformErrorCode::Windows(1), PlatformErrorCode::Darwin(1));
    }

    #[test]
    fn is_helpers_match_only_their_own_variant() {
        assert!(Error::PermissionDenied.is_permission_denied());
        assert!(!Error::NotFound.is_permission_denied());

        assert!(Error::NotFound.is_not_found());
        assert!(!Error::AlreadyExists.is_not_found());

        assert!(Error::AlreadyExists.is_already_exists());
        assert!(!Error::Unsupported.is_already_exists());

        assert!(Error::Unsupported.is_unsupported());
        assert!(!Error::InvalidState.is_unsupported());

        assert!(Error::InvalidState.is_invalid_state());
        assert!(!Error::Disconnected.is_invalid_state());

        assert!(Error::Disconnected.is_disconnected());
        assert!(!Error::Platform(PlatformErrorCode::Linux(-1)).is_disconnected());

        assert!(Error::Platform(PlatformErrorCode::Linux(-1)).is_platform());
        assert!(!Error::PermissionDenied.is_platform());
    }
}
