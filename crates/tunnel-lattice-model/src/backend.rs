//! Support API for Tunnel Lattice backend crates. Not covered by any
//! stability promise; may change in any minor release.
//!
//! Applications do not need this module: it is behind the non-default
//! `backend` feature and the `tunnel-lattice` facade neither enables nor
//! re-exports it. It holds the OS-independent rules that every backend
//! (the `tun-rs` backend, the native Linux backend, and the Windows and
//! macOS backends to come) would otherwise copy:
//!
//! - [`HostOs`], the operating system whose rules apply, and the per-OS
//!   interface-name rules ([`precheck_name`] and its constants);
//! - the checks `open` runs before any native call ([`precheck_open`]);
//! - the segmentation-offload request rule ([`offload_requested`]);
//! - the administrative-state read rules ([`admin_from_if_flags`],
//!   [`admin_from_oper_status`]);
//! - the `apply` contract: precondition order, step order, and
//!   compensation ([`apply_patch`], driving a backend's [`ApplySteps`]).
//!
//! Every rule takes the [`HostOs`] (or the data it needs) as a parameter
//! instead of reading `#[cfg]`, so the rules of every operating system are
//! testable on every host. Nothing here performs I/O, makes a system call,
//! or uses `unsafe`.

use tunnel_lattice_core::{Error, Result};

use crate::{
    AdminState, DesiredAdminState, DeviceConfig, DeviceConfigPatch, DeviceId, DeviceKind,
    MacAddress,
};

/// The operating system whose `open` rules apply.
///
/// Passing the value explicitly (rather than branching on `#[cfg]`) lets a
/// test exercise every operating system's rules on any host. Use
/// [`HostOs::CURRENT`] for the target a crate was compiled for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostOs {
    /// Linux.
    Linux,
    /// macOS.
    Macos,
    /// Windows.
    Windows,
    /// Any other target: no name rules beyond the portable ones, and no
    /// open-only classification.
    Other,
}

impl HostOs {
    /// The target this crate was compiled for.
    pub const CURRENT: Self = if cfg!(target_os = "linux") {
        Self::Linux
    } else if cfg!(target_os = "macos") {
        Self::Macos
    } else if cfg!(target_os = "windows") {
        Self::Windows
    } else {
        Self::Other
    };
}

/// Longest Linux/macOS interface name in bytes: `IFNAMSIZ` (16) minus the
/// terminating NUL.
pub const UNIX_MAX_NAME_BYTES: usize = 15;

/// Longest Windows adapter name in UTF-16 code units: a 256-unit buffer
/// minus the terminating NUL.
pub const WINDOWS_MAX_NAME_UTF16_UNITS: usize = 255;

/// Required prefix of a macOS TAP (`feth`) name.
pub const MACOS_TAP_PREFIX: &str = "feth";

/// Highest `feth` unit the XNU cloner creates (`IF_MAXUNIT`, `0x7fff`).
/// Larger units fail natively with `ENXIO`, and `u32::MAX` is XNU's
/// wildcard unit (the kernel would pick the unit and rename the device).
pub const MACOS_FETH_MAX_UNIT: u32 = 0x7fff;

/// Required prefix of a macOS TUN (`utun`) name.
pub const MACOS_TUN_PREFIX: &str = "utun";

/// `IFF_UP`, identical on Linux and macOS.
pub const IFF_UP: u32 = 0x1;

/// `IFF_RUNNING`, identical on Linux and macOS.
pub const IFF_RUNNING: u32 = 0x40;

/// `IfOperStatusUp` from the Windows `ifdef.h`.
pub const IF_OPER_STATUS_UP: i32 = 1;

/// Validates a requested device name before any native call.
///
/// Returns [`Error::InvalidState`] when `name` cannot be honored exactly on
/// `os`:
///
/// | OS | Rule |
/// |---|---|
/// | all | non-empty, no NUL character |
/// | Linux | at most 15 bytes, no `%` (the kernel expands `%d` as a naming template) |
/// | macOS `Tap` | `feth` followed by a canonical decimal number from 0 to 32767 (no sign, no leading zero) |
/// | macOS `Tun` | `utun` followed by a canonical decimal number below `u32::MAX` (no sign, no leading zero), at most 15 bytes |
/// | Windows `Tun`/`Tap` | at most 255 UTF-16 code units |
/// | other | only the portable rule |
///
/// # Errors
///
/// [`Error::InvalidState`] for a name that breaks the rule of `os`.
#[allow(
    unreachable_patterns,
    reason = "`DeviceKind` is `#[non_exhaustive]` for other crates only; the wildcard keeps a future kind safe"
)]
pub fn precheck_name(os: HostOs, kind: DeviceKind, name: &str) -> Result<()> {
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
                    // `precheck_open` rejects unknown kinds before the name.
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

/// `utun<N>` where `N` is exactly what the backend will create: `N` is
/// parsed as a `u32` and one is added, so `u32::MAX` itself would overflow,
/// and a sign or a leading zero would open a differently named device.
fn is_canonical_utun_name(name: &str) -> bool {
    name.strip_prefix(MACOS_TUN_PREFIX)
        .and_then(canonical_unit)
        .is_some_and(|n| n < u32::MAX)
}

/// `feth<N>` with an explicit, canonical unit number. Bare `feth` is the
/// kernel's own auto-naming template, a non-numeric or non-canonical unit is
/// not a name the `feth` cloner creates as given, and the unit is bounded by
/// [`MACOS_FETH_MAX_UNIT`].
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

/// What [`precheck_open`] hands back once every check has passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenRequest {
    /// The requested MTU, narrowed to the `u16` every native device API
    /// takes; `None` when no MTU was requested.
    pub mtu: Option<u16>,
}

/// The checks `open` runs before any native call, in this order:
///
/// 1. the kind must be [`DeviceKind::Tun`] or [`DeviceKind::Tap`], else
///    [`Error::Unsupported`];
/// 2. a requested name must pass [`precheck_name`];
/// 3. a requested MTU must fit `u16`;
/// 4. a requested MAC address needs a [`DeviceKind::Tap`] device.
///
/// Checks 2 to 4 fail with [`Error::InvalidState`].
///
/// # Errors
///
/// As listed above; nothing has been created when this returns an error.
#[allow(
    unreachable_patterns,
    reason = "`DeviceKind` is `#[non_exhaustive]` for other crates only; the wildcard keeps a future kind safe"
)]
pub fn precheck_open(os: HostOs, config: &DeviceConfig) -> Result<OpenRequest> {
    match config.kind {
        DeviceKind::Tun | DeviceKind::Tap => {}
        _ => return Err(Error::Unsupported),
    }
    if let Some(name) = config.name.as_deref() {
        precheck_name(os, config.kind, name)?;
    }
    let mtu = config
        .mtu
        .map(|mtu| u16::try_from(mtu).map_err(|_| Error::InvalidState))
        .transpose()?;
    if config.mac.is_some() && config.kind != DeviceKind::Tap {
        return Err(Error::InvalidState);
    }
    Ok(OpenRequest { mtu })
}

/// Whether segmentation offload is actually requested from the native
/// device: the caller asked for it (`requested`) and the combination
/// supports it, which is a Linux TUN device only. The request is ignored,
/// without an error, everywhere else.
#[must_use]
pub const fn offload_requested(os: HostOs, kind: DeviceKind, requested: bool) -> bool {
    requested && matches!(os, HostOs::Linux) && matches!(kind, DeviceKind::Tun)
}

/// The Linux/macOS administrative-state rule: `Up` iff the interface flags
/// carry both `IFF_UP` and `IFF_RUNNING` (administratively up and able to
/// pass packets). Other flags do not matter.
#[must_use]
pub const fn admin_from_if_flags(flags: u32) -> AdminState {
    if flags & IFF_UP != 0 && flags & IFF_RUNNING != 0 {
        AdminState::Up
    } else {
        AdminState::Down
    }
}

/// The Windows administrative-state rule: `Up` iff the operational status
/// is `IfOperStatusUp` ([`IF_OPER_STATUS_UP`]); every other status (down,
/// dormant, not present, lower layer down, testing, unknown) is `Down`.
#[must_use]
pub const fn admin_from_oper_status(status: i32) -> AdminState {
    if status == IF_OPER_STATUS_UP {
        AdminState::Up
    } else {
        AdminState::Down
    }
}

/// The native steps [`apply_patch`] drives, so the ordering and
/// compensation logic is shared and testable without a device.
///
/// Each method is one native request and reports its own failure as an
/// [`Error`]. A "set" is expected to change only the one property it names.
pub trait ApplySteps {
    /// Reads the current MTU.
    ///
    /// # Errors
    ///
    /// The native failure; no device state has changed.
    fn mtu(&mut self) -> Result<u16>;

    /// Sets the MTU.
    ///
    /// # Errors
    ///
    /// The native failure.
    fn set_mtu(&mut self, mtu: u16) -> Result<()>;

    /// Reads the current MAC address.
    ///
    /// # Errors
    ///
    /// The native failure; no device state has changed.
    fn mac(&mut self) -> Result<MacAddress>;

    /// Sets the MAC address.
    ///
    /// # Errors
    ///
    /// The native failure.
    fn set_mac(&mut self, mac: MacAddress) -> Result<()>;

    /// Sets (`true`) or clears (`false`) the administrative up state.
    ///
    /// # Errors
    ///
    /// The native failure.
    fn set_up(&mut self, up: bool) -> Result<()>;
}

/// What [`apply_patch`] checks a patch against.
#[derive(Debug, Clone, Copy)]
pub struct ApplyTarget {
    /// The handle's device identity.
    pub id: DeviceId,
    /// The device's kind.
    pub kind: DeviceKind,
    /// Whether the handle reports MAC-address mutation as supported.
    pub mac_mutation: bool,
}

/// The `apply` contract: every precondition before any native call, then
/// MTU, MAC address, and administrative state in that order, and on a
/// failed step a best-effort revert of the earlier ones in reverse before
/// returning the failed step's own error.
///
/// Preconditions, in order:
///
/// 1. a patch for another device is [`Error::InvalidState`];
/// 2. an MTU above `u16::MAX` is [`Error::InvalidState`];
/// 3. a MAC address for a device that is not a TAP device is
///    [`Error::InvalidState`];
/// 4. a [`DesiredAdminState`] this module does not know is
///    [`Error::Unsupported`];
/// 5. a MAC address on a target without `mac_mutation` is
///    [`Error::Unsupported`].
///
/// A step's previous value is read before any change, and only when a
/// later step exists that could fail and need it restored; a failed read
/// returns its error with nothing changed. The administrative state goes
/// last, so it never needs reverting. After any `Err`, a fresh snapshot of
/// the device is authoritative.
///
/// # Errors
///
/// A failed precondition (above) or the error of the first failing step.
#[allow(
    unreachable_patterns,
    reason = "`DesiredAdminState` is `#[non_exhaustive]` for other crates only; the wildcard keeps a future state safe"
)]
pub fn apply_patch(
    steps: &mut impl ApplySteps,
    target: ApplyTarget,
    patch: &DeviceConfigPatch,
) -> Result<()> {
    if patch.device_id() != target.id {
        return Err(Error::InvalidState);
    }
    let mtu = patch
        .mtu()
        .map(|mtu| u16::try_from(mtu).map_err(|_| Error::InvalidState))
        .transpose()?;
    let mac = patch.mac();
    if mac.is_some() && target.kind != DeviceKind::Tap {
        return Err(Error::InvalidState);
    }
    // `DesiredAdminState` is `#[non_exhaustive]`: a variant this module does
    // not know is unsupported, not a malformed patch.
    let up = match patch.admin_state() {
        None => None,
        Some(DesiredAdminState::Up) => Some(true),
        Some(DesiredAdminState::Down) => Some(false),
        Some(_) => return Err(Error::Unsupported),
    };
    if mac.is_some() && !target.mac_mutation {
        return Err(Error::Unsupported);
    }

    // Previous values, read before anything changes, only where a later
    // step exists.
    let previous_mtu = match mtu {
        Some(_) if mac.is_some() || up.is_some() => Some(steps.mtu()?),
        _ => None,
    };
    let previous_mac = match mac {
        Some(_) if up.is_some() => Some(steps.mac()?),
        _ => None,
    };

    if let Some(mtu) = mtu {
        steps.set_mtu(mtu)?;
    }
    if let Some(mac) = mac
        && let Err(error) = steps.set_mac(mac)
    {
        revert(steps, previous_mtu, None);
        return Err(error);
    }
    if let Some(up) = up
        && let Err(error) = steps.set_up(up)
    {
        revert(steps, previous_mtu, previous_mac);
        return Err(error);
    }
    Ok(())
}

/// Best-effort compensation, in reverse order (MAC address, then MTU). A
/// failed revert is not reported: the original error is what the caller
/// needs.
fn revert(steps: &mut impl ApplySteps, mtu: Option<u16>, mac: Option<MacAddress>) {
    if let Some(mac) = mac {
        let _ = steps.set_mac(mac);
    }
    if let Some(mtu) = mtu {
        let _ = steps.set_mtu(mtu);
    }
}

#[cfg(test)]
mod tests {
    use tunnel_lattice_core::PlatformErrorCode;

    use super::*;

    const ALL_OSES: [HostOs; 4] = [HostOs::Linux, HostOs::Macos, HostOs::Windows, HostOs::Other];
    const KINDS: [DeviceKind; 2] = [DeviceKind::Tun, DeviceKind::Tap];

    fn rejected(os: HostOs, kind: DeviceKind, name: &str) -> bool {
        matches!(precheck_name(os, kind, name), Err(Error::InvalidState))
    }

    fn accepted(os: HostOs, kind: DeviceKind, name: &str) -> bool {
        precheck_name(os, kind, name).is_ok()
    }

    // ---- names -----------------------------------------------------------

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
    fn linux_short_names_and_old_native_corpus() {
        for ok in ["t", "tun0", "tl-test", "a234567890abcde"] {
            assert!(accepted(HostOs::Linux, DeviceKind::Tun, ok), "{ok}");
        }
        for bad in ["", "a234567890abcdef", "tun%d", "tun\0", "%"] {
            assert!(rejected(HostOs::Linux, DeviceKind::Tun, bad), "{bad:?}");
        }
        assert!(accepted(HostOs::Linux, DeviceKind::Tun, "ééééééé"));
        assert!(rejected(HostOs::Linux, DeviceKind::Tun, "éééééééé"));
    }

    #[test]
    fn macos_tap_names_must_be_canonical_feth_units() {
        let tap = DeviceKind::Tap;
        for ok in ["feth0", "feth7", "feth42", "feth32767"] {
            assert!(accepted(HostOs::Macos, tap, ok), "{ok} should be accepted");
        }
        for bad in [
            // The kernel's own auto-naming template: the kernel picks the unit.
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
            // The backend computes `N + 1`; `u32::MAX` would overflow.
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
    fn name_constants_are_pinned() {
        assert_eq!(UNIX_MAX_NAME_BYTES, 15);
        assert_eq!(WINDOWS_MAX_NAME_UTF16_UNITS, 255);
        assert_eq!(MACOS_TAP_PREFIX, "feth");
        assert_eq!(MACOS_TUN_PREFIX, "utun");
        assert_eq!(MACOS_FETH_MAX_UNIT, 0x7fff);
    }

    #[test]
    fn host_os_matches_the_compilation_target() {
        #[cfg(target_os = "linux")]
        assert_eq!(HostOs::CURRENT, HostOs::Linux);
        #[cfg(target_os = "macos")]
        assert_eq!(HostOs::CURRENT, HostOs::Macos);
        #[cfg(target_os = "windows")]
        assert_eq!(HostOs::CURRENT, HostOs::Windows);
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        assert_eq!(HostOs::CURRENT, HostOs::Other);
    }

    // ---- open prechecks --------------------------------------------------

    const MAC: MacAddress = MacAddress::new([0x02, 0, 0, 0, 0, 1]);

    #[test]
    fn precheck_open_passes_a_valid_config_and_narrows_the_mtu() {
        for os in ALL_OSES {
            assert_eq!(
                precheck_open(os, &DeviceConfig::new(DeviceKind::Tun)).unwrap(),
                OpenRequest { mtu: None }
            );
            let config = DeviceConfig::new(DeviceKind::Tap)
                .with_name("tap0")
                .with_mtu(u32::from(u16::MAX))
                .with_mac(MAC);
            let result = precheck_open(os, &config);
            if os == HostOs::Macos {
                // `tap0` is not a canonical macOS `feth` name.
                assert!(matches!(result, Err(Error::InvalidState)), "{os:?}");
            } else {
                let request = result.expect("accepted");
                assert_eq!(request.mtu, Some(u16::MAX), "{os:?}");
            }
        }
    }

    #[test]
    fn precheck_open_rejects_each_failing_check_with_invalid_state() {
        let os = HostOs::Linux;
        let long = "a".repeat(16);
        let rows = [
            DeviceConfig::new(DeviceKind::Tun).with_name(""),
            DeviceConfig::new(DeviceKind::Tun).with_name(long),
            DeviceConfig::new(DeviceKind::Tun).with_mtu(u32::from(u16::MAX) + 1),
            DeviceConfig::new(DeviceKind::Tun).with_mac(MAC),
        ];
        for config in rows {
            let result = precheck_open(os, &config);
            assert!(matches!(result, Err(Error::InvalidState)), "{result:?}");
        }
    }

    #[test]
    fn precheck_open_checks_the_name_before_the_mtu_and_the_mtu_before_the_mac() {
        // The first failing check decides; all three here are InvalidState,
        // so the order is observed through which inputs are rejected alone.
        let bad_name = DeviceConfig::new(DeviceKind::Tun).with_name("");
        assert!(precheck_open(HostOs::Linux, &bad_name).is_err());
        let ok_name_bad_mtu = DeviceConfig::new(DeviceKind::Tun)
            .with_name("tun0")
            .with_mtu(70_000);
        assert!(precheck_open(HostOs::Linux, &ok_name_bad_mtu).is_err());
        let ok_mtu_bad_mac = DeviceConfig::new(DeviceKind::Tun)
            .with_name("tun0")
            .with_mtu(1500)
            .with_mac(MAC);
        assert!(precheck_open(HostOs::Linux, &ok_mtu_bad_mac).is_err());
    }

    // ---- offload request -------------------------------------------------

    #[test]
    fn the_offload_request_is_honoured_for_linux_tun_only() {
        for os in ALL_OSES {
            for kind in KINDS {
                assert!(!offload_requested(os, kind, false), "{os:?} {kind:?}");
                assert_eq!(
                    offload_requested(os, kind, true),
                    os == HostOs::Linux && kind == DeviceKind::Tun,
                    "{os:?} {kind:?}"
                );
            }
        }
    }

    // ---- administrative state --------------------------------------------

    #[test]
    fn interface_flags_are_up_only_with_up_and_running() {
        assert_eq!(IFF_UP, 0x1);
        assert_eq!(IFF_RUNNING, 0x40);
        assert_eq!(admin_from_if_flags(IFF_UP | IFF_RUNNING), AdminState::Up);
        // Other flags (broadcast, multicast, point-to-point) do not matter.
        assert_eq!(
            admin_from_if_flags(IFF_UP | IFF_RUNNING | 0x2 | 0x8000),
            AdminState::Up
        );
        assert_eq!(admin_from_if_flags(IFF_UP), AdminState::Down);
        assert_eq!(admin_from_if_flags(IFF_RUNNING), AdminState::Down);
        assert_eq!(admin_from_if_flags(0), AdminState::Down);
        assert_eq!(admin_from_if_flags(0x8000_0041), AdminState::Up);
        assert_eq!(admin_from_if_flags(0xffff_ffbe), AdminState::Down);
    }

    #[test]
    fn operational_status_is_up_only_when_up() {
        // `ifdef.h`: Up 1, Down 2, Testing 3, Unknown 4, Dormant 5,
        // NotPresent 6, LowerLayerDown 7.
        assert_eq!(IF_OPER_STATUS_UP, 1);
        assert_eq!(admin_from_oper_status(1), AdminState::Up);
        for status in [2, 3, 4, 5, 6, 7, 0, -1] {
            assert_eq!(admin_from_oper_status(status), AdminState::Down, "{status}");
        }
    }

    // ---- apply -----------------------------------------------------------

    #[derive(Debug, PartialEq, Eq)]
    enum Call {
        ReadMtu,
        SetMtu(u16),
        ReadMac,
        SetMac(MacAddress),
        SetUp(bool),
    }

    /// Records every native call; each step fails when its flag is set.
    #[derive(Default)]
    struct Fake {
        calls: Vec<Call>,
        fail_read: bool,
        fail_set_mtu: bool,
        fail_set_mac: bool,
        fail_set_up: bool,
    }

    impl ApplySteps for Fake {
        fn mtu(&mut self) -> Result<u16> {
            self.calls.push(Call::ReadMtu);
            if self.fail_read {
                return Err(Error::Platform(PlatformErrorCode::Unknown));
            }
            Ok(1500)
        }

        fn set_mtu(&mut self, mtu: u16) -> Result<()> {
            self.calls.push(Call::SetMtu(mtu));
            if self.fail_set_mtu {
                return Err(Error::PermissionDenied);
            }
            Ok(())
        }

        fn mac(&mut self) -> Result<MacAddress> {
            self.calls.push(Call::ReadMac);
            Ok(OLD_MAC)
        }

        fn set_mac(&mut self, mac: MacAddress) -> Result<()> {
            self.calls.push(Call::SetMac(mac));
            if self.fail_set_mac {
                return Err(Error::PermissionDenied);
            }
            Ok(())
        }

        fn set_up(&mut self, up: bool) -> Result<()> {
            self.calls.push(Call::SetUp(up));
            if self.fail_set_up {
                return Err(Error::PermissionDenied);
            }
            Ok(())
        }
    }

    const ID: DeviceId = DeviceId::new(3);
    const OLD_MAC: MacAddress = MacAddress::new([0x02, 0, 0, 0, 0, 0x01]);
    const NEW_MAC: MacAddress = MacAddress::new([0x02, 0, 0, 0, 0, 0x02]);

    /// A TUN device: no MAC address at all.
    const TUN: ApplyTarget = ApplyTarget {
        id: ID,
        kind: DeviceKind::Tun,
        mac_mutation: false,
    };

    /// A TAP device whose handle reports MAC mutation.
    const TAP: ApplyTarget = ApplyTarget {
        id: ID,
        kind: DeviceKind::Tap,
        mac_mutation: true,
    };

    fn patch(
        id: DeviceId,
        admin: Option<DesiredAdminState>,
        mtu: Option<u32>,
    ) -> DeviceConfigPatch {
        DeviceConfigPatch::new(id, admin, mtu).expect("a valid patch")
    }

    #[test]
    fn a_patch_for_another_device_is_rejected_without_a_native_call() {
        let mut fake = Fake::default();
        let patch = patch(DeviceId::new(4), Some(DesiredAdminState::Up), Some(1400));
        let result = apply_patch(&mut fake, TUN, &patch);
        assert!(matches!(result, Err(Error::InvalidState)), "{result:?}");
        assert!(fake.calls.is_empty());
    }

    #[test]
    fn an_mtu_above_u16_is_rejected_without_a_native_call() {
        let mut fake = Fake::default();
        let patch = patch(ID, Some(DesiredAdminState::Up), Some(70_000));
        let result = apply_patch(&mut fake, TUN, &patch);
        assert!(matches!(result, Err(Error::InvalidState)), "{result:?}");
        assert!(fake.calls.is_empty());
    }

    #[test]
    fn a_mac_for_a_tun_device_is_rejected_without_a_native_call() {
        let mut fake = Fake::default();
        let patch = patch(ID, None, Some(1400)).with_mac(NEW_MAC);
        let result = apply_patch(&mut fake, TUN, &patch);
        assert!(matches!(result, Err(Error::InvalidState)), "{result:?}");
        assert!(fake.calls.is_empty());
    }

    #[test]
    fn a_mac_without_mac_mutation_is_unsupported_without_a_native_call() {
        let mut fake = Fake::default();
        let target = ApplyTarget {
            mac_mutation: false,
            ..TAP
        };
        let patch = patch(ID, Some(DesiredAdminState::Up), Some(1400)).with_mac(NEW_MAC);
        let result = apply_patch(&mut fake, target, &patch);
        assert!(matches!(result, Err(Error::Unsupported)), "{result:?}");
        assert!(fake.calls.is_empty());
    }

    #[test]
    fn the_largest_u16_mtu_is_accepted() {
        let mut fake = Fake::default();
        apply_patch(&mut fake, TUN, &patch(ID, None, Some(u32::from(u16::MAX)))).expect("apply");
        assert_eq!(fake.calls, [Call::SetMtu(u16::MAX)]);
    }

    #[test]
    fn a_single_step_skips_the_pre_read() {
        let mut fake = Fake::default();
        apply_patch(&mut fake, TUN, &patch(ID, None, Some(1400))).expect("apply");
        assert_eq!(fake.calls, [Call::SetMtu(1400)]);

        let mut fake = Fake::default();
        apply_patch(
            &mut fake,
            TUN,
            &patch(ID, Some(DesiredAdminState::Down), None),
        )
        .expect("apply");
        assert_eq!(fake.calls, [Call::SetUp(false)]);

        let mut fake = Fake::default();
        apply_patch(&mut fake, TAP, &DeviceConfigPatch::new_mac(ID, NEW_MAC)).expect("apply");
        assert_eq!(fake.calls, [Call::SetMac(NEW_MAC)]);
    }

    #[test]
    fn every_step_runs_mtu_then_mac_then_admin_after_the_pre_reads() {
        let mut fake = Fake::default();
        let patch = patch(ID, Some(DesiredAdminState::Up), Some(1400)).with_mac(NEW_MAC);
        apply_patch(&mut fake, TAP, &patch).expect("apply");
        assert_eq!(
            fake.calls,
            [
                Call::ReadMtu,
                Call::ReadMac,
                Call::SetMtu(1400),
                Call::SetMac(NEW_MAC),
                Call::SetUp(true),
            ]
        );
    }

    #[test]
    fn mtu_and_mac_read_only_the_mtu_first() {
        let mut fake = Fake::default();
        let patch = patch(ID, None, Some(1400)).with_mac(NEW_MAC);
        apply_patch(&mut fake, TAP, &patch).expect("apply");
        assert_eq!(
            fake.calls,
            [Call::ReadMtu, Call::SetMtu(1400), Call::SetMac(NEW_MAC)]
        );
    }

    #[test]
    fn a_failed_mac_step_restores_the_mtu_and_skips_the_admin_step() {
        let mut fake = Fake {
            fail_set_mac: true,
            ..Fake::default()
        };
        let patch = patch(ID, Some(DesiredAdminState::Up), Some(1400)).with_mac(NEW_MAC);
        let result = apply_patch(&mut fake, TAP, &patch);
        assert!(matches!(result, Err(Error::PermissionDenied)), "{result:?}");
        assert_eq!(
            fake.calls,
            [
                Call::ReadMtu,
                Call::ReadMac,
                Call::SetMtu(1400),
                Call::SetMac(NEW_MAC),
                Call::SetMtu(1500),
            ]
        );
    }

    #[test]
    fn a_failed_admin_step_restores_the_mac_then_the_mtu() {
        let mut fake = Fake {
            fail_set_up: true,
            ..Fake::default()
        };
        let patch = patch(ID, Some(DesiredAdminState::Down), Some(1400)).with_mac(NEW_MAC);
        let result = apply_patch(&mut fake, TAP, &patch);
        assert!(matches!(result, Err(Error::PermissionDenied)), "{result:?}");
        assert_eq!(
            fake.calls,
            [
                Call::ReadMtu,
                Call::ReadMac,
                Call::SetMtu(1400),
                Call::SetMac(NEW_MAC),
                Call::SetUp(false),
                Call::SetMac(OLD_MAC),
                Call::SetMtu(1500),
            ]
        );
    }

    #[test]
    fn both_steps_run_mtu_first_after_a_pre_read() {
        let mut fake = Fake::default();
        apply_patch(
            &mut fake,
            TUN,
            &patch(ID, Some(DesiredAdminState::Up), Some(1400)),
        )
        .expect("apply");
        assert_eq!(
            fake.calls,
            [Call::ReadMtu, Call::SetMtu(1400), Call::SetUp(true)]
        );
    }

    #[test]
    fn a_failed_pre_read_changes_nothing() {
        let mut fake = Fake {
            fail_read: true,
            ..Fake::default()
        };
        let result = apply_patch(
            &mut fake,
            TUN,
            &patch(ID, Some(DesiredAdminState::Up), Some(1400)),
        );
        assert!(
            matches!(result, Err(Error::Platform(PlatformErrorCode::Unknown))),
            "{result:?}"
        );
        assert_eq!(fake.calls, [Call::ReadMtu]);
    }

    #[test]
    fn a_failed_mtu_step_skips_the_admin_step() {
        let mut fake = Fake {
            fail_set_mtu: true,
            ..Fake::default()
        };
        let result = apply_patch(
            &mut fake,
            TUN,
            &patch(ID, Some(DesiredAdminState::Up), Some(1400)),
        );
        assert!(matches!(result, Err(Error::PermissionDenied)), "{result:?}");
        assert_eq!(fake.calls, [Call::ReadMtu, Call::SetMtu(1400)]);
    }

    #[test]
    fn a_failed_admin_step_restores_the_mtu_and_returns_its_own_error() {
        let mut fake = Fake {
            fail_set_up: true,
            ..Fake::default()
        };
        let result = apply_patch(
            &mut fake,
            TUN,
            &patch(ID, Some(DesiredAdminState::Down), Some(1400)),
        );
        assert!(matches!(result, Err(Error::PermissionDenied)), "{result:?}");
        assert_eq!(
            fake.calls,
            [
                Call::ReadMtu,
                Call::SetMtu(1400),
                Call::SetUp(false),
                Call::SetMtu(1500),
            ]
        );
    }

    #[test]
    fn a_failed_revert_still_returns_the_original_error() {
        // `set_mtu` succeeds the first time and fails on the revert.
        struct FlakyRevert(u8);

        impl ApplySteps for FlakyRevert {
            fn mtu(&mut self) -> Result<u16> {
                Ok(1500)
            }

            fn set_mtu(&mut self, _mtu: u16) -> Result<()> {
                self.0 += 1;
                if self.0 > 1 {
                    return Err(Error::Platform(PlatformErrorCode::Unknown));
                }
                Ok(())
            }

            fn mac(&mut self) -> Result<MacAddress> {
                Ok(OLD_MAC)
            }

            fn set_mac(&mut self, _mac: MacAddress) -> Result<()> {
                Ok(())
            }

            fn set_up(&mut self, _up: bool) -> Result<()> {
                Err(Error::NotFound)
            }
        }

        let mut steps = FlakyRevert(0);
        let result = apply_patch(
            &mut steps,
            TUN,
            &patch(ID, Some(DesiredAdminState::Up), Some(1400)),
        );
        assert!(matches!(result, Err(Error::NotFound)), "{result:?}");
        assert_eq!(steps.0, 2, "the revert was attempted");
    }
}
