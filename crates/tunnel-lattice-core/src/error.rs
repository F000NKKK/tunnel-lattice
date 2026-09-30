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
    /// The OS driver or user-mode runtime needed to create this kind of
    /// device is not installed or could not be loaded (for example
    /// `wintun.dll`, the tap-windows6 `tap0901` driver, or the Linux `tun`
    /// module).
    ///
    /// Returned only by `DeviceProvider::open`. Distinct from
    /// [`Error::Unsupported`] (the operation has no meaning on this backend
    /// at all) and from [`Error::NotFound`] (a referenced device is
    /// missing): installing or loading the driver makes the same `open`
    /// call succeed.
    DriverUnavailable,
    /// The buffer passed to `recv` was smaller than the next packet; that
    /// packet was discarded.
    ///
    /// `recv` never truncates a packet silently and never reports more
    /// bytes than the buffer holds. The device stays usable: the next
    /// `recv` with a large enough buffer receives the following packet.
    /// Size buffers with `tunnel_lattice_model::Device::recv_buffer_len`.
    BufferTooSmall,
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
///
/// Marked `#[non_exhaustive]`: new platform tags may be added in a minor
/// release, so a `match` outside this crate needs a wildcard arm.
///
/// ```
/// use tunnel_lattice_core::PlatformErrorCode;
///
/// fn describe(code: PlatformErrorCode) -> String {
///     match code {
///         PlatformErrorCode::Linux(errno) => format!("Linux errno {errno}"),
///         PlatformErrorCode::Windows(code) => format!("Windows error {code}"),
///         PlatformErrorCode::Darwin(errno) => format!("Darwin errno {errno}"),
///         PlatformErrorCode::Unknown => "no OS error code".to_owned(),
///         // Required: the enum is `#[non_exhaustive]`.
///         _ => "unrecognized platform".to_owned(),
///     }
/// }
///
/// assert_eq!(describe(PlatformErrorCode::Unknown), "no OS error code");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PlatformErrorCode {
    /// A Linux `errno` value.
    Linux(i32),
    /// A Windows error code (`DWORD`).
    Windows(u32),
    /// A Darwin (macOS) `errno` value.
    Darwin(i32),
    /// A native failure that carried no OS error code, or occurred on a
    /// target this enum has no tag for. Carries no payload.
    ///
    /// Used instead of fabricating a `0` code (which on Linux/macOS and
    /// Windows alike means "success", not "unknown").
    Unknown,
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

    /// Returns `true` if this is [`Error::DriverUnavailable`].
    pub const fn is_driver_unavailable(&self) -> bool {
        matches!(self, Error::DriverUnavailable)
    }

    /// Returns `true` if this is [`Error::BufferTooSmall`].
    pub const fn is_buffer_too_small(&self) -> bool {
        matches!(self, Error::BufferTooSmall)
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
            Error::DriverUnavailable => write!(f, "required driver or runtime is unavailable"),
            Error::BufferTooSmall => write!(f, "receive buffer too small for the packet"),
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
            (
                Error::DriverUnavailable,
                "required driver or runtime is unavailable",
            ),
            (
                Error::BufferTooSmall,
                "receive buffer too small for the packet",
            ),
        ];
        for (error, expected) in cases {
            assert_eq!(error.to_string(), expected);
        }
        assert_eq!(
            Error::Platform(PlatformErrorCode::Linux(-1)).to_string(),
            "platform error: Linux(-1)"
        );
        assert_eq!(
            Error::Platform(PlatformErrorCode::Unknown).to_string(),
            "platform error: Unknown"
        );
    }

    #[test]
    fn platform_error_codes_preserve_their_platform_and_value() {
        assert_eq!(PlatformErrorCode::Linux(-1), PlatformErrorCode::Linux(-1));
        assert_ne!(PlatformErrorCode::Windows(1), PlatformErrorCode::Darwin(1));
    }

    #[test]
    fn unknown_platform_code_is_distinct_from_every_zero_code() {
        let unknown = PlatformErrorCode::Unknown;
        let copied = unknown;
        assert_eq!(unknown, copied);
        assert_ne!(unknown, PlatformErrorCode::Linux(0));
        assert_ne!(unknown, PlatformErrorCode::Windows(0));
        assert_ne!(unknown, PlatformErrorCode::Darwin(0));
        assert!(Error::Platform(unknown).is_platform());
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

        assert!(Error::DriverUnavailable.is_driver_unavailable());
        assert!(!Error::Unsupported.is_driver_unavailable());
        assert!(!Error::NotFound.is_driver_unavailable());
        assert!(!Error::Platform(PlatformErrorCode::Unknown).is_driver_unavailable());

        assert!(Error::BufferTooSmall.is_buffer_too_small());
        assert!(!Error::InvalidState.is_buffer_too_small());
        assert!(!Error::Disconnected.is_buffer_too_small());
        assert!(!Error::BufferTooSmall.is_invalid_state());

        assert!(Error::Platform(PlatformErrorCode::Linux(-1)).is_platform());
        assert!(!Error::PermissionDenied.is_platform());
    }
}
