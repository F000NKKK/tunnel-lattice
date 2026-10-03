//! The `DeviceProvider::open` contract: the open-only error classifier that
//! runs before the generic [`io_error`] mapping. The name and parameter
//! prechecks that run before any native call are shared with the other
//! backends in `tunnel_lattice_model::backend`.
//!
//! The classifier is written against an explicit [`HostOs`] parameter
//! instead of `#[cfg]` blocks, so every per-OS rule is exercised by the
//! ordinary unit tests on every host. Only the two genuinely native pieces — the
//! `libloading::Error` downcast (Windows) and the `if_nametoindex` lookup
//! (Linux/macOS) — are target-gated.
//!
//! Every message-matched string below was taken from `tun-rs` 2.8.11, the
//! workspace's minimum `tun-rs` requirement; re-verify them whenever the
//! resolved `tun-rs` changes. The unit tests only guard this crate's own
//! constants. Against the real `tun-rs`, the "no TAP driver" message and the
//! wintun load failure are checked end to end by the Windows missing-driver
//! tests in `privileged_tests`, which the privileged CI job runs before any
//! driver is installed; the "adapter already exists" message has no such
//! end-to-end check.

use std::io;

use tunnel_lattice_core::Error;
use tunnel_lattice_model::DeviceKind;
pub(crate) use tunnel_lattice_model::backend::HostOs;

use crate::io_error;

/// The target this crate was compiled for.
pub(crate) const HOST_OS: HostOs = HostOs::CURRENT;

/// `tun-rs` 2.8.11 `windows/tap/iface.rs`: the message of the code-less
/// `ErrorKind::NotFound` returned when no `tap0901` driver is installed.
pub(crate) const WINDOWS_TAP_NO_DRIVER_MESSAGE: &str = "No driver found";

/// `tun-rs` 2.8.11 `windows/device.rs`: prefix of the code-less
/// `io::Error::other` returned for an existing TAP adapter name when
/// `reuse_dev(false)` is set (`"The network adapter [<name>] already
/// exists."`).
pub(crate) const WINDOWS_TAP_EXISTS_PREFIX: &str = "The network adapter [";

/// Suffix matching [`WINDOWS_TAP_EXISTS_PREFIX`].
pub(crate) const WINDOWS_TAP_EXISTS_SUFFIX: &str = "] already exists.";

// POSIX errno values, identical on Linux and macOS (asserted against `libc`
// by the unit tests on those targets). Written out rather than taken from
// `libc` so the classifier compiles, and is tested, on every host.
const ENOENT: i32 = 2;
const EBUSY: i32 = 16;
const ENODEV: i32 = 19;
const EINVAL: i32 = 22;

/// Maps an error from `tun-rs`'s device build step (inside
/// `DeviceProvider::open`) onto [`Error`].
///
/// Applies the open-only rules first and falls back to the generic
/// [`io_error`] mapping for anything they do not match:
///
/// | OS / kind | Match | Result |
/// |---|---|---|
/// | Windows `Tun` | the error wraps a `libloading::Error` from loading `wintun.dll` or resolving one of its functions | [`Error::DriverUnavailable`] |
/// | Windows `Tap` | `NotFound`, no OS code, message exactly [`WINDOWS_TAP_NO_DRIVER_MESSAGE`] | [`Error::DriverUnavailable`] |
/// | Windows `Tap` | `Other`, no OS code, message [`WINDOWS_TAP_EXISTS_PREFIX`]…[`WINDOWS_TAP_EXISTS_SUFFIX`] | [`Error::AlreadyExists`] |
/// | Linux | raw `ENODEV` or `ENOENT` (the `tun` module or `/dev/net/tun` is missing) | [`Error::DriverUnavailable`] |
/// | Linux | raw `EINVAL` or `EBUSY`, a name was requested, and an interface with that name exists | [`Error::AlreadyExists`] |
/// | macOS | raw `EBUSY`, a name was requested, and an interface with that name exists | [`Error::AlreadyExists`] |
///
/// `name_exists` is consulted only for the `EINVAL`/`EBUSY` rows, after the
/// build has already failed.
pub(crate) fn classify_open_error(
    os: HostOs,
    kind: DeviceKind,
    name: Option<&str>,
    err: io::Error,
    name_exists: impl Fn(&str) -> bool,
) -> Error {
    classify_open_only(os, kind, name, &err, name_exists).unwrap_or_else(|| io_error(err))
}

fn classify_open_only(
    os: HostOs,
    kind: DeviceKind,
    name: Option<&str>,
    err: &io::Error,
    name_exists: impl Fn(&str) -> bool,
) -> Option<Error> {
    let requested_name_exists = || name.is_some_and(&name_exists);
    match os {
        HostOs::Windows => match kind {
            DeviceKind::Tun if is_wintun_load_failure(err) => Some(Error::DriverUnavailable),
            DeviceKind::Tap if is_tap_driver_missing(err) => Some(Error::DriverUnavailable),
            DeviceKind::Tap if is_tap_adapter_exists(err) => Some(Error::AlreadyExists),
            _ => None,
        },
        HostOs::Linux => match err.raw_os_error()? {
            ENODEV | ENOENT => Some(Error::DriverUnavailable),
            EINVAL | EBUSY if requested_name_exists() => Some(Error::AlreadyExists),
            _ => None,
        },
        HostOs::Macos => match err.raw_os_error()? {
            EBUSY if requested_name_exists() => Some(Error::AlreadyExists),
            _ => None,
        },
        HostOs::Other => None,
    }
}

/// `tun-rs` wraps the `libloading::Error` from `wintun_raw::wintun::new`
/// with `io::Error::other`; no element of that chain carries an OS code
/// (`libloading`'s `WindowsError` field is private), so the match is a
/// structural downcast rather than a code or a (localized) message.
///
/// Requires the backend's `libloading` dependency to be the same major
/// version `tun-rs` uses — CI guards this with `cargo tree -i libloading`.
#[cfg(target_os = "windows")]
fn is_wintun_load_failure(err: &io::Error) -> bool {
    use libloading::Error as LoadError;

    matches!(
        err.get_ref().and_then(|e| e.downcast_ref::<LoadError>()),
        Some(
            LoadError::LoadLibraryExW { .. }
                | LoadError::LoadLibraryExWUnknown
                | LoadError::GetProcAddress { .. }
                | LoadError::GetProcAddressUnknown
        )
    )
}

/// `libloading` is a Windows-only dependency; `tun-rs` never produces a
/// wintun load error on another target.
#[cfg(not(target_os = "windows"))]
fn is_wintun_load_failure(_err: &io::Error) -> bool {
    false
}

fn is_tap_driver_missing(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::NotFound
        && err.raw_os_error().is_none()
        && err.to_string() == WINDOWS_TAP_NO_DRIVER_MESSAGE
}

fn is_tap_adapter_exists(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::Other
        && err.raw_os_error().is_none()
        && err
            .to_string()
            .strip_prefix(WINDOWS_TAP_EXISTS_PREFIX)
            .is_some_and(|rest| rest.ends_with(WINDOWS_TAP_EXISTS_SUFFIX))
}

/// Whether an interface named `name` currently exists on this host.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn host_name_exists(name: &str) -> bool {
    let Ok(name) = std::ffi::CString::new(name) else {
        return false;
    };
    // SAFETY: `name` is a valid NUL-terminated C string that outlives the
    // call, and `if_nametoindex` only reads it. A return of 0 means "no such
    // interface" (or failure), never a valid index.
    unsafe { libc::if_nametoindex(name.as_ptr()) != 0 }
}

/// Only the Linux/macOS rules consult interface existence.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn host_name_exists(_name: &str) -> bool {
    false
}

/// Ordinary (non-privileged, deterministic) tests of every classifier row,
/// on every host.
#[cfg(test)]
mod tests {
    use super::*;

    const KINDS: [DeviceKind; 2] = [DeviceKind::Tun, DeviceKind::Tap];

    // The name and parameter prechecks are tested next to their shared
    // implementation in `tunnel_lattice_model::backend`.

    #[test]
    fn host_os_is_the_compilation_target() {
        assert_eq!(HOST_OS, HostOs::CURRENT);
    }

    // ---- classifier ------------------------------------------------------

    /// A `name_exists` that must not be consulted for the row under test.
    fn never_consulted(name: &str) -> bool {
        panic!("name_exists({name}) should not be consulted for this error")
    }

    fn classify(
        os: HostOs,
        kind: DeviceKind,
        name: Option<&str>,
        err: io::Error,
        exists: bool,
    ) -> Error {
        classify_open_error(os, kind, name, err, |_| exists)
    }

    fn not_found(message: &str) -> io::Error {
        io::Error::new(io::ErrorKind::NotFound, message)
    }

    /// Guards this crate's own copies of the `tun-rs` 2.8.11 strings the
    /// Windows rules match against accidental edits. It cannot see `tun-rs`
    /// itself: a `tun-rs` release that rewords the "no driver" message is
    /// detected instead by the ignored Windows missing-driver tests in
    /// `privileged_tests`, which the privileged CI job runs before any
    /// driver is installed.
    #[test]
    fn pinned_tun_rs_2_8_11_messages() {
        assert_eq!(WINDOWS_TAP_NO_DRIVER_MESSAGE, "No driver found");
        assert_eq!(WINDOWS_TAP_EXISTS_PREFIX, "The network adapter [");
        assert_eq!(WINDOWS_TAP_EXISTS_SUFFIX, "] already exists.");
        // The exact shape `tun-rs` formats: `format!("The network adapter
        // [{name}] already exists.")`.
        let native = io::Error::other(format!(
            "{WINDOWS_TAP_EXISTS_PREFIX}tap-test{WINDOWS_TAP_EXISTS_SUFFIX}"
        ));
        assert_eq!(
            native.to_string(),
            "The network adapter [tap-test] already exists."
        );
    }

    #[test]
    fn windows_tap_no_driver_is_driver_unavailable() {
        let mapped = classify_open_error(
            HostOs::Windows,
            DeviceKind::Tap,
            None,
            not_found(WINDOWS_TAP_NO_DRIVER_MESSAGE),
            never_consulted,
        );
        assert!(mapped.is_driver_unavailable(), "got {mapped:?}");
    }

    /// The creation-time MAC path produces two other code-less `NotFound`s
    /// that must stay `NotFound`.
    #[test]
    fn other_windows_tap_not_found_messages_stay_not_found() {
        for message in [
            "Device not found",
            "Registry entry not found for given adapter GUID",
            "No driver found.",
            "no driver found",
        ] {
            let mapped = classify_open_error(
                HostOs::Windows,
                DeviceKind::Tap,
                Some("tap0"),
                not_found(message),
                never_consulted,
            );
            assert!(mapped.is_not_found(), "{message}: got {mapped:?}");
        }
    }

    /// The Windows TAP rule is kind- and OS-specific.
    #[test]
    fn no_driver_message_outside_windows_tap_keeps_the_generic_mapping() {
        for (os, kind) in [
            (HostOs::Windows, DeviceKind::Tun),
            (HostOs::Linux, DeviceKind::Tap),
            (HostOs::Macos, DeviceKind::Tap),
            (HostOs::Other, DeviceKind::Tap),
        ] {
            let mapped = classify(
                os,
                kind,
                None,
                not_found(WINDOWS_TAP_NO_DRIVER_MESSAGE),
                false,
            );
            assert!(mapped.is_not_found(), "{os:?}/{kind:?}: got {mapped:?}");
        }
        // Same message, wrong kind: not a driver error.
        let mapped = classify_open_error(
            HostOs::Windows,
            DeviceKind::Tap,
            None,
            io::Error::other(WINDOWS_TAP_NO_DRIVER_MESSAGE),
            never_consulted,
        );
        assert!(
            matches!(
                mapped,
                Error::Platform(tunnel_lattice_core::PlatformErrorCode::Unknown)
            ),
            "got {mapped:?}"
        );
    }

    #[test]
    fn windows_tap_existing_adapter_is_already_exists() {
        let mapped = classify_open_error(
            HostOs::Windows,
            DeviceKind::Tap,
            Some("tap-test"),
            io::Error::other("The network adapter [tap-test] already exists."),
            never_consulted,
        );
        assert!(mapped.is_already_exists(), "got {mapped:?}");
    }

    #[test]
    fn near_miss_existing_adapter_messages_are_not_already_exists() {
        for err in [
            io::Error::other("The network adapter [tap-test] is busy."),
            io::Error::other("A network adapter [tap-test] already exists."),
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "The network adapter [tap-test] already exists.",
            ),
        ] {
            let mapped = classify_open_error(
                HostOs::Windows,
                DeviceKind::Tap,
                Some("tap-test"),
                err,
                never_consulted,
            );
            assert!(!mapped.is_already_exists(), "got {mapped:?}");
            assert!(mapped.is_platform(), "got {mapped:?}");
        }
        // Only produced on the TAP path.
        let mapped = classify_open_error(
            HostOs::Windows,
            DeviceKind::Tun,
            Some("tap-test"),
            io::Error::other("The network adapter [tap-test] already exists."),
            never_consulted,
        );
        assert!(!mapped.is_already_exists(), "got {mapped:?}");
    }

    /// Code-less `Other` errors `tun-rs` builds on the wintun path that are
    /// not a missing runtime (name/description/ring-capacity checks) must
    /// not become `DriverUnavailable`.
    #[test]
    fn code_less_windows_tun_errors_are_not_driver_unavailable() {
        for message in [
            "name too long",
            "tunnel type too long",
            "ring capacity 1 not in [131072,67108864]",
        ] {
            let mapped = classify_open_error(
                HostOs::Windows,
                DeviceKind::Tun,
                None,
                io::Error::other(message),
                never_consulted,
            );
            assert!(
                matches!(
                    mapped,
                    Error::Platform(tunnel_lattice_core::PlatformErrorCode::Unknown)
                ),
                "{message}: got {mapped:?}"
            );
        }
    }

    /// `libloading`'s unit variants are the only ones constructible outside
    /// that crate (`WindowsError`'s field is private); the struct variants
    /// share the same match arm.
    #[test]
    #[cfg(target_os = "windows")]
    fn windows_tun_libloading_failures_are_driver_unavailable() {
        for load_error in [
            libloading::Error::LoadLibraryExWUnknown,
            libloading::Error::GetProcAddressUnknown,
        ] {
            let mapped = classify_open_error(
                HostOs::Windows,
                DeviceKind::Tun,
                None,
                io::Error::other(load_error),
                never_consulted,
            );
            assert!(mapped.is_driver_unavailable(), "got {mapped:?}");
        }
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn unrelated_libloading_failures_are_not_driver_unavailable() {
        for load_error in [
            libloading::Error::FreeLibraryUnknown,
            libloading::Error::GetModuleHandleExWUnknown,
        ] {
            let mapped = classify_open_error(
                HostOs::Windows,
                DeviceKind::Tun,
                None,
                io::Error::other(load_error),
                never_consulted,
            );
            assert!(!mapped.is_driver_unavailable(), "got {mapped:?}");
        }
        // A wintun load error is never expected on the TAP path.
        let mapped = classify_open_error(
            HostOs::Windows,
            DeviceKind::Tap,
            None,
            io::Error::other(libloading::Error::LoadLibraryExWUnknown),
            never_consulted,
        );
        assert!(!mapped.is_driver_unavailable(), "got {mapped:?}");
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn wintun_downcast_never_matches_off_windows() {
        assert!(!is_wintun_load_failure(&io::Error::other("wintun.dll")));
    }

    #[test]
    fn linux_missing_tun_driver_is_driver_unavailable() {
        for kind in KINDS {
            for code in [ENODEV, ENOENT] {
                for name in [None, Some("tl0")] {
                    let mapped = classify_open_error(
                        HostOs::Linux,
                        kind,
                        name,
                        io::Error::from_raw_os_error(code),
                        never_consulted,
                    );
                    assert!(mapped.is_driver_unavailable(), "{code}: got {mapped:?}");
                }
            }
        }
    }

    /// `ENODEV`/`ENOENT` are Linux-only rows.
    #[test]
    fn missing_device_codes_elsewhere_are_not_driver_unavailable() {
        for os in [HostOs::Macos, HostOs::Windows, HostOs::Other] {
            for code in [ENODEV, ENOENT] {
                let mapped = classify(
                    os,
                    DeviceKind::Tun,
                    Some("tl0"),
                    io::Error::from_raw_os_error(code),
                    true,
                );
                assert!(
                    !mapped.is_driver_unavailable(),
                    "{os:?}/{code}: got {mapped:?}"
                );
            }
        }
    }

    #[test]
    fn linux_einval_or_ebusy_on_an_existing_name_is_already_exists() {
        for kind in KINDS {
            for code in [EINVAL, EBUSY] {
                let mapped = classify(
                    HostOs::Linux,
                    kind,
                    Some("tl0"),
                    io::Error::from_raw_os_error(code),
                    true,
                );
                assert!(mapped.is_already_exists(), "{code}: got {mapped:?}");
            }
        }
    }

    /// Without an existing interface (or without a requested name) the same
    /// codes keep their raw platform mapping: `EINVAL` is also returned for
    /// flag-validation failures.
    #[test]
    fn linux_einval_or_ebusy_without_an_existing_name_stays_a_platform_error() {
        for code in [EINVAL, EBUSY] {
            let absent = classify(
                HostOs::Linux,
                DeviceKind::Tun,
                Some("tl0"),
                io::Error::from_raw_os_error(code),
                false,
            );
            assert!(absent.is_platform(), "{code}: got {absent:?}");

            let unnamed = classify_open_error(
                HostOs::Linux,
                DeviceKind::Tun,
                None,
                io::Error::from_raw_os_error(code),
                never_consulted,
            );
            assert!(unnamed.is_platform(), "{code}: got {unnamed:?}");
        }
    }

    #[test]
    fn name_existence_is_looked_up_for_the_requested_name() {
        let mapped = classify_open_error(
            HostOs::Linux,
            DeviceKind::Tun,
            Some("tl-exists"),
            io::Error::from_raw_os_error(EBUSY),
            |name| name == "tl-exists",
        );
        assert!(mapped.is_already_exists(), "got {mapped:?}");
    }

    #[test]
    fn macos_ebusy_on_an_existing_name_is_already_exists() {
        for kind in KINDS {
            let mapped = classify(
                HostOs::Macos,
                kind,
                Some("utun7"),
                io::Error::from_raw_os_error(EBUSY),
                true,
            );
            assert!(mapped.is_already_exists(), "{kind:?}: got {mapped:?}");
        }
        let absent = classify(
            HostOs::Macos,
            DeviceKind::Tun,
            Some("utun7"),
            io::Error::from_raw_os_error(EBUSY),
            false,
        );
        assert!(absent.is_platform(), "got {absent:?}");
    }

    /// macOS has no `EINVAL` existing-name row.
    #[test]
    fn macos_einval_is_not_already_exists() {
        let mapped = classify(
            HostOs::Macos,
            DeviceKind::Tun,
            Some("utun7"),
            io::Error::from_raw_os_error(EINVAL),
            true,
        );
        assert!(!mapped.is_already_exists(), "got {mapped:?}");
    }

    /// Anything the open-only rules do not match goes through the generic
    /// mapping unchanged (`EPERM` stays `PermissionDenied`, even when the
    /// name exists).
    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn unmatched_errors_fall_through_to_the_generic_mapping() {
        for os in [HostOs::Linux, HostOs::Macos] {
            let mapped = classify(
                os,
                DeviceKind::Tun,
                Some("tl0"),
                io::Error::from_raw_os_error(libc::EPERM),
                true,
            );
            assert!(mapped.is_permission_denied(), "{os:?}: got {mapped:?}");
        }
        let mapped = classify_open_error(
            HostOs::Other,
            DeviceKind::Tun,
            Some("tl0"),
            io::Error::from_raw_os_error(EBUSY),
            never_consulted,
        );
        assert!(mapped.is_platform(), "got {mapped:?}");
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn errno_constants_match_libc() {
        assert_eq!(ENOENT, libc::ENOENT);
        assert_eq!(EBUSY, libc::EBUSY);
        assert_eq!(ENODEV, libc::ENODEV);
        assert_eq!(EINVAL, libc::EINVAL);
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn host_name_exists_uses_the_live_interface_table() {
        // The loopback interface exists in every network namespace.
        #[cfg(target_os = "linux")]
        assert!(host_name_exists("lo"));
        #[cfg(target_os = "macos")]
        assert!(host_name_exists("lo0"));
        assert!(!host_name_exists("tl-no-such-if"));
        assert!(!host_name_exists("lo\0"));
    }
}
