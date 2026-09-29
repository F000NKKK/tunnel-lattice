//! The `DeviceProvider::open` contract: name prechecks that run before any
//! native call, and the open-only error classifier that runs before the
//! generic [`io_error`] mapping.
//!
//! Both are written against an explicit [`HostOs`] parameter instead of
//! `#[cfg]` blocks, so every per-OS rule is exercised by the ordinary unit
//! tests on every host. Only the two genuinely native pieces — the
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

use crate::io_error;

/// The operating system whose `open` rules apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostOs {
    Linux,
    Macos,
    Windows,
    /// Any other target: no name rules beyond the portable ones, and no
    /// open-only classification.
    Other,
}

/// The target this crate was compiled for.
pub(crate) const HOST_OS: HostOs = if cfg!(target_os = "linux") {
    HostOs::Linux
} else if cfg!(target_os = "macos") {
    HostOs::Macos
} else if cfg!(target_os = "windows") {
    HostOs::Windows
} else {
    HostOs::Other
};

/// Longest Linux/macOS interface name in bytes: `IFNAMSIZ` (16) minus the
/// terminating NUL (`tun-rs` `linux/device.rs`, `macos/tuntap.rs`,
/// `macos/tap/mod.rs`).
const UNIX_MAX_NAME_BYTES: usize = 15;

/// Longest Windows adapter name in UTF-16 code units: `tun-rs`'s
/// `MAX_POOL` (256) minus the terminating NUL that its `encode_utf16`
/// appends (`windows/tun/mod.rs`, `windows/ffi.rs`).
const WINDOWS_MAX_NAME_UTF16_UNITS: usize = 255;

/// Required prefix of a macOS TAP (`feth`) name (`macos/tap/mod.rs`).
const MACOS_TAP_PREFIX: &str = "feth";

/// Highest `feth` unit the XNU cloner creates: `FETH_MAXUNIT`, which is
/// `IF_MAXUNIT` (`0x7fff`) in `bsd/net/if_fake.c` / `if_private.h`. Larger
/// units fail natively with `ENXIO`, and `u32::MAX` is XNU's wildcard unit
/// (`if_clone_create` then picks the lowest free unit and renames the
/// device).
const MACOS_FETH_MAX_UNIT: u32 = 0x7fff;

/// Required prefix of a macOS TUN (`utun`) name (`macos/tuntap.rs`).
const MACOS_TUN_PREFIX: &str = "utun";

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

/// Validates a requested device name before any native call.
///
/// Returns [`Error::InvalidState`] when `name` cannot be honored exactly
/// on `os`, instead of letting `tun-rs` fail with a code-less error (or, for
/// some formats, silently open a differently named device):
///
/// | OS | Rule |
/// |---|---|
/// | all | non-empty, no NUL character |
/// | Linux | at most 15 bytes, no `%` (the kernel expands `%d` as a naming template) |
/// | macOS `Tap` | `feth` followed by a canonical decimal number from 0 to 32767 (no sign, no leading zero) |
/// | macOS `Tun` | `utun` followed by a canonical decimal number below `u32::MAX` (no sign, no leading zero), at most 15 bytes |
/// | Windows `Tun`/`Tap` | at most 255 UTF-16 code units |
pub(crate) fn precheck_name(os: HostOs, kind: DeviceKind, name: &str) -> Result<(), Error> {
    if name.is_empty() || name.contains('\0') {
        return Err(Error::InvalidState);
    }
    let valid = match os {
        HostOs::Linux => name.len() <= UNIX_MAX_NAME_BYTES && !name.contains('%'),
        HostOs::Macos => {
            name.len() <= UNIX_MAX_NAME_BYTES
                && match kind {
                    DeviceKind::Tap => is_canonical_feth_name(name),
                    DeviceKind::Tun => is_canonical_utun_name(name),
                    // `open` rejects unknown kinds before prechecking.
                    _ => true,
                }
        }
        HostOs::Windows => name.encode_utf16().count() <= WINDOWS_MAX_NAME_UTF16_UNITS,
        HostOs::Other => true,
    };
    if valid {
        Ok(())
    } else {
        Err(Error::InvalidState)
    }
}

/// `utun<N>` where `N` is exactly what `tun-rs` will create: `tun-rs`
/// parses `N` with `str::parse::<u32>` (which also accepts `+7` and `007`,
/// both of which would open `utun7` under a different name) and then adds
/// one, so `u32::MAX` itself would overflow.
fn is_canonical_utun_name(name: &str) -> bool {
    name.strip_prefix(MACOS_TUN_PREFIX)
        .and_then(canonical_unit)
        .is_some_and(|n| n < u32::MAX)
}

/// `feth<N>` with an explicit, canonical unit number. `tun-rs` itself only
/// checks the `feth` prefix and hands the name to `SIOCIFCREATE`, but bare
/// `feth` is its own auto-naming template (the kernel picks the unit, so the
/// device would open as some `fethN`), and a non-numeric or non-canonical
/// unit is not a name the `feth` cloner creates as given. The unit is also
/// bounded by [`MACOS_FETH_MAX_UNIT`]: `feth4294967295` would be XNU's
/// wildcard (again a kernel-picked unit) and `feth32768` and above fail
/// natively.
fn is_canonical_feth_name(name: &str) -> bool {
    name.strip_prefix(MACOS_TAP_PREFIX)
        .and_then(canonical_unit)
        .is_some_and(|n| n <= MACOS_FETH_MAX_UNIT)
}

/// Parses a non-empty run of ASCII digits with no sign and no leading zero
/// (except `0` itself) as a `u32`.
fn canonical_unit(unit: &str) -> Option<u32> {
    let canonical = !unit.is_empty()
        && unit.bytes().all(|b| b.is_ascii_digit())
        && (unit == "0" || !unit.starts_with('0'));
    if canonical {
        unit.parse::<u32>().ok()
    } else {
        None
    }
}

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

/// Ordinary (non-privileged, deterministic) tests of every precheck and
/// classifier row, on every host.
#[cfg(test)]
mod tests {
    use super::*;

    const ALL_OSES: [HostOs; 4] = [HostOs::Linux, HostOs::Macos, HostOs::Windows, HostOs::Other];
    const KINDS: [DeviceKind; 2] = [DeviceKind::Tun, DeviceKind::Tap];

    fn rejected(os: HostOs, kind: DeviceKind, name: &str) -> bool {
        matches!(precheck_name(os, kind, name), Err(Error::InvalidState))
    }

    fn accepted(os: HostOs, kind: DeviceKind, name: &str) -> bool {
        precheck_name(os, kind, name).is_ok()
    }

    // ---- prechecks -------------------------------------------------------

    #[test]
    fn empty_and_nul_names_are_rejected_everywhere() {
        for os in ALL_OSES {
            for kind in KINDS {
                assert!(rejected(os, kind, ""), "{os:?}/{kind:?} empty");
                assert!(rejected(os, kind, "feth\0x"), "{os:?}/{kind:?} NUL");
                assert!(rejected(os, kind, "utun1\0"), "{os:?}/{kind:?} NUL");
            }
        }
    }

    #[test]
    fn linux_names_are_limited_to_15_bytes_for_both_kinds() {
        for kind in KINDS {
            assert!(accepted(HostOs::Linux, kind, "tun0"));
            assert!(accepted(HostOs::Linux, kind, &"a".repeat(15)));
            assert!(rejected(HostOs::Linux, kind, &"a".repeat(16)));
            // Bytes, not characters: 8 two-byte characters are 16 bytes.
            assert!(accepted(HostOs::Linux, kind, &"ä".repeat(7)));
            assert!(rejected(HostOs::Linux, kind, &"ä".repeat(8)));
        }
    }

    /// Linux expands a `%d` in the requested name as a naming template
    /// (`tl%d` opens as `tl0`), and rejects any other `%` use natively, so
    /// every `%` is rejected up front.
    #[test]
    fn linux_names_containing_a_percent_sign_are_rejected() {
        for kind in KINDS {
            for bad in ["tl%d", "%d", "tun%", "a%sb", "%%"] {
                assert!(rejected(HostOs::Linux, kind, bad), "{kind:?}/{bad}");
            }
        }
        // The `%` rule is Linux-only; Windows adapter names may contain it.
        assert!(accepted(HostOs::Windows, DeviceKind::Tun, "tl%d"));
        assert!(accepted(HostOs::Other, DeviceKind::Tun, "tl%d"));
    }

    #[test]
    fn macos_tap_names_must_be_canonical_feth_units() {
        let tap = DeviceKind::Tap;
        for ok in ["feth0", "feth7", "feth42", "feth32767"] {
            assert!(accepted(HostOs::Macos, tap, ok), "{ok} should be accepted");
        }
        for bad in [
            // `tun-rs`'s own auto-naming template: the kernel picks the unit.
            "feth",
            "fethX",
            "feth1a",
            "feth+5",
            "feth-1",
            "feth07",
            "feth00",
            "feth 1",
            // Past XNU's `IF_MAXUNIT` (0x7fff): fails natively with ENXIO.
            "feth32768",
            "feth65535",
            // XNU's wildcard unit (`u32::MAX`): the kernel would pick the
            // unit and rename the device.
            "feth4294967295",
            // Past `u32` (and within 15 bytes, so only the range rule
            // rejects it).
            "feth4294967296",
            "feth99999999999",
            // 16 bytes.
            "feth111111111111",
            "tap0",
            "fet",
            "utun3",
        ] {
            assert!(
                rejected(HostOs::Macos, tap, bad),
                "{bad} should be rejected"
            );
        }
    }

    #[test]
    fn macos_tun_names_must_be_canonical_utun_units() {
        let tun = DeviceKind::Tun;
        for ok in ["utun0", "utun7", "utun123", "utun4294967294"] {
            assert!(accepted(HostOs::Macos, tun, ok), "{ok} should be accepted");
        }
        for bad in [
            "utun",
            "utunx",
            "utun+5",
            "utun-1",
            "utun07",
            "utun00",
            "utun 1",
            "tun0",
            "feth0",
            // `tun-rs` computes `N + 1`; `u32::MAX` would overflow.
            "utun4294967295",
            // Parses past `u32` (and is 15 bytes, so only the range rule
            // rejects it).
            "utun99999999999",
            // 16 bytes.
            "utun999999999999",
        ] {
            assert!(
                rejected(HostOs::Macos, tun, bad),
                "{bad} should be rejected"
            );
        }
    }

    #[test]
    fn windows_names_are_limited_to_255_utf16_units_for_both_kinds() {
        for kind in KINDS {
            // Longer than any Unix limit, still fine on Windows.
            assert!(accepted(HostOs::Windows, kind, &"a".repeat(100)));
            assert!(accepted(HostOs::Windows, kind, "Tunnel Lattice (test)"));
            assert!(accepted(HostOs::Windows, kind, &"a".repeat(255)));
            assert!(rejected(HostOs::Windows, kind, &"a".repeat(256)));
            // A non-BMP character is two UTF-16 units.
            assert!(accepted(HostOs::Windows, kind, &"\u{1F600}".repeat(127)));
            assert!(rejected(HostOs::Windows, kind, &"\u{1F600}".repeat(128)));
        }
    }

    #[test]
    fn other_targets_only_apply_the_portable_rules() {
        for kind in KINDS {
            assert!(accepted(HostOs::Other, kind, &"a".repeat(300)));
        }
    }

    #[test]
    fn host_os_matches_the_compilation_target() {
        #[cfg(target_os = "linux")]
        assert_eq!(HOST_OS, HostOs::Linux);
        #[cfg(target_os = "macos")]
        assert_eq!(HOST_OS, HostOs::Macos);
        #[cfg(target_os = "windows")]
        assert_eq!(HOST_OS, HostOs::Windows);
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        assert_eq!(HOST_OS, HostOs::Other);
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
