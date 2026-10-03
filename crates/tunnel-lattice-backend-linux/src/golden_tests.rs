//! Golden equivalence tests: they pin the backend's current, observable
//! behavior of the OS-independent rules (errno classification, name rules,
//! open prechecks, administrative-state flags, `apply` step traces and
//! compensation) so those rules can later move into shared support modules
//! with no change of behavior.
//!
//! Every test is table-driven, deterministic and non-privileged, and goes
//! only through the crate-internal entry points of the rules (never through
//! private helpers), so the tables can be reused unchanged against the moved
//! implementation. A table row is the recorded output of the current code;
//! if a row ever has to change, the behavior changed.

use tunnel_lattice_core::{Error, PlatformErrorCode, Result};
use tunnel_lattice_model::{
    AdminState, DesiredAdminState, DeviceConfig, DeviceConfigPatch, DeviceId, DeviceKind,
    MacAddress,
};

use tunnel_lattice_model::backend::{
    ApplySteps as Steps, ApplyTarget as Target, HostOs, UNIX_MAX_NAME_BYTES, admin_from_if_flags,
    apply_patch, precheck_name,
};

use crate::errno::{self, Op};
use crate::tun;

// ---- errno ---------------------------------------------------------------

/// The Linux errno values the rules depend on, pinned numerically so a
/// relocated plain-integer constant table cannot drift from the kernel ABI.
#[test]
fn errno_constants_have_their_linux_values() {
    for (name, actual, expected) in [
        ("EPERM", libc::EPERM, 1),
        ("ENOENT", libc::ENOENT, 2),
        ("EINTR", libc::EINTR, 4),
        ("EIO", libc::EIO, 5),
        ("ENXIO", libc::ENXIO, 6),
        ("EAGAIN", libc::EAGAIN, 11),
        ("EACCES", libc::EACCES, 13),
        ("EFAULT", libc::EFAULT, 14),
        ("EBUSY", libc::EBUSY, 16),
        ("EEXIST", libc::EEXIST, 17),
        ("ENODEV", libc::ENODEV, 19),
        ("EINVAL", libc::EINVAL, 22),
        ("EBADFD", libc::EBADFD, 77),
        ("EOPNOTSUPP", libc::EOPNOTSUPP, 95),
    ] {
        assert_eq!(actual, expected, "{name}");
    }
    assert_eq!(libc::EWOULDBLOCK, libc::EAGAIN);
    assert_eq!(libc::ENOTSUP, libc::EOPNOTSUPP);
}

/// Every operation, with the label of its row in [`EXPECTED_SPECIAL_ROWS`].
const ALL_OPS: [(Op, &str); 8] = [
    (Op::OpenTun, "OpenTun"),
    (Op::SetIff, "SetIff"),
    (Op::QueueIoctl, "QueueIoctl"),
    (Op::Netlink, "Netlink"),
    (Op::Recv, "Recv"),
    (Op::Send, "Send"),
    (Op::OffloadRecv, "OffloadRecv"),
    (Op::OffloadSend, "OffloadSend"),
];

const PERMISSION: &str = "Fail(PermissionDenied)";
const UNSUPPORTED: &str = "Fail(Unsupported)";
const DRIVER: &str = "Fail(DriverUnavailable)";
const EXISTS: &str = "Fail(AlreadyExists)";
const NOT_FOUND: &str = "Fail(NotFound)";
const DISCONNECTED: &str = "Fail(Disconnected)";
const INVALID_STATE: &str = "Fail(InvalidState)";

/// For every operation, each errno in `0..=4095` whose class is not the
/// fallback `Fail(Platform(Linux(errno)))`, with that class. Sorted by
/// errno. Every other errno in the range is the fallback.
fn expected_special_rows(op: &str) -> Vec<(i32, &'static str)> {
    let (eperm, enoent, eintr, eio, enxio) = (
        libc::EPERM,
        libc::ENOENT,
        libc::EINTR,
        libc::EIO,
        libc::ENXIO,
    );
    let (eagain, eacces, efault, ebusy) = (libc::EAGAIN, libc::EACCES, libc::EFAULT, libc::EBUSY);
    let (eexist, enodev, einval, ebadfd, eopnotsupp) = (
        libc::EEXIST,
        libc::ENODEV,
        libc::EINVAL,
        libc::EBADFD,
        libc::EOPNOTSUPP,
    );
    let mut rows = vec![
        (eperm, PERMISSION),
        (eacces, PERMISSION),
        (eintr, "Retry"),
        (eopnotsupp, UNSUPPORTED),
    ];
    match op {
        "OpenTun" => rows.extend([(enoent, DRIVER), (enodev, DRIVER), (enxio, DRIVER)]),
        "SetIff" => rows.extend([(ebusy, EXISTS), (einval, "CheckNameConflict")]),
        "QueueIoctl" => {}
        "Netlink" => rows.extend([(enodev, NOT_FOUND), (eexist, EXISTS)]),
        "Recv" | "OffloadRecv" => {
            rows.extend([
                (eagain, "WouldBlock"),
                (ebadfd, DISCONNECTED),
                (enxio, DISCONNECTED),
                (efault, DISCONNECTED),
            ]);
            if op == "OffloadRecv" {
                rows.push((einval, "DropAndReread"));
            }
        }
        "Send" | "OffloadSend" => {
            rows.extend([
                (eagain, "WouldBlock"),
                (ebadfd, DISCONNECTED),
                (enxio, DISCONNECTED),
                (eio, INVALID_STATE),
            ]);
            if op == "OffloadSend" {
                rows.push((einval, "ResendPerPacket"));
            }
        }
        other => panic!("unknown op {other}"),
    }
    rows.sort_unstable_by_key(|&(errno, _)| errno);
    rows
}

#[test]
fn errno_classification_is_pinned_for_every_errno_and_op() {
    for (op, label) in ALL_OPS {
        let fallback = |errno: i32| format!("Fail(Platform(Linux({errno})))");
        let mut special = Vec::new();
        for errno in 0..=4095 {
            let class = format!("{:?}", errno::classify(errno, op));
            if class != fallback(errno) {
                special.push((errno, class));
            }
        }
        let expected = expected_special_rows(label);
        let expected: Vec<(i32, String)> = expected
            .into_iter()
            .map(|(errno, class)| (errno, class.to_owned()))
            .collect();
        assert_eq!(special, expected, "{label}");
    }
}

#[test]
fn errno_fallback_keeps_the_raw_code_at_the_range_edges() {
    for (op, label) in ALL_OPS {
        for errno in [0, 1_000, 4_095, 4_096, i32::MAX] {
            if expected_special_rows(label)
                .iter()
                .any(|&(special, _)| special == errno)
            {
                continue;
            }
            assert_eq!(
                format!("{:?}", errno::classify(errno, op)),
                format!("Fail(Platform(Linux({errno})))"),
                "{label} {errno}"
            );
        }
    }
}

#[test]
fn errno_helpers_are_pinned() {
    assert_eq!(
        format!("{:?}", errno::platform(libc::EMSGSIZE)),
        format!("Platform(Linux({}))", libc::EMSGSIZE)
    );
    assert_eq!(format!("{:?}", errno::malformed()), "Platform(Unknown)");
    assert_eq!(
        format!("{:?}", errno::set_iff_einval(true)),
        "AlreadyExists"
    );
    assert_eq!(
        format!("{:?}", errno::set_iff_einval(false)),
        format!("Platform(Linux({}))", libc::EINVAL)
    );
}

// ---- names ---------------------------------------------------------------

/// Which names the Linux rule accepts. The same corpus and Linux columns are
/// the recorded rows of the multi-OS name table in the tun-rs backend, so the
/// two backends can be seen to agree on every row.
fn linux_name_rows() -> Vec<(String, bool)> {
    vec![
        (String::new(), false),
        ("a\0b".to_owned(), false),
        ("tun0".to_owned(), true),
        ("tl-test".to_owned(), true),
        ("utun0".to_owned(), true),
        ("feth0".to_owned(), true),
        ("tl%d".to_owned(), false),
        ("%".to_owned(), false),
        ("feth%d".to_owned(), false),
        ("a".repeat(15), true),
        ("a".repeat(16), false),
        ("a".repeat(255), false),
        ("a".repeat(256), false),
        // The limit is in bytes: 7 two-byte characters are 14 bytes, 8 are 16.
        ("ä".repeat(7), true),
        ("ä".repeat(8), false),
        ("😀".repeat(3), true),
        ("😀".repeat(4), false),
    ]
}

#[test]
fn the_linux_name_rule_is_pinned() {
    assert_eq!(UNIX_MAX_NAME_BYTES, 15);
    for (name, accepted) in linux_name_rows() {
        let result = precheck_name(HostOs::Linux, DeviceKind::Tun, &name);
        if accepted {
            assert!(result.is_ok(), "{name:?}");
        } else {
            assert_eq!(
                format!("{:?}", result.unwrap_err()),
                "InvalidState",
                "{name:?}"
            );
        }
    }
}

// ---- open prechecks --------------------------------------------------------

/// A name no Linux precheck accepts.
const BAD_NAMES: [&str; 3] = ["", "a\0b", "tl%d"];

/// Whether the prechecks (name, MTU, MAC, in the order of `open`) must reject
/// the request. `kind -> name -> MTU -> MAC` is the documented order; every
/// rejection is `InvalidState`, so the order is only visible through which
/// check would otherwise have to run first; an unknown (`non_exhaustive`)
/// kind, the only `Unsupported` outcome, cannot be built outside the model.
fn precheck_rejects(kind: DeviceKind, name: Option<&str>, mtu: Option<u32>, mac: bool) -> bool {
    name.is_some_and(|name| precheck_name(HostOs::Linux, kind, name).is_err())
        || mtu.is_some_and(|mtu| mtu > u32::from(u16::MAX))
        || (mac && kind != DeviceKind::Tap)
}

#[test]
fn open_rejects_every_failing_precheck_combination_before_any_native_call() {
    use tunnel_lattice_platform::DeviceProvider;

    let backend = crate::LinuxBackend::new();
    let mac = MacAddress::new([0x02, 0, 0, 0, 0, 1]);
    let mut checked = 0;
    for kind in [DeviceKind::Tun, DeviceKind::Tap] {
        for name in [
            None,
            Some(BAD_NAMES[0]),
            Some(BAD_NAMES[1]),
            Some(BAD_NAMES[2]),
            Some("tl-gold"),
        ] {
            for mtu in [None, Some(1400), Some(65_535), Some(65_536), Some(u32::MAX)] {
                for with_mac in [false, true] {
                    if !precheck_rejects(kind, name, mtu, with_mac) {
                        // Would reach the native calls: not a precheck row.
                        continue;
                    }
                    let mut config = DeviceConfig::new(kind);
                    if let Some(name) = name {
                        config = config.with_name(name);
                    }
                    if let Some(mtu) = mtu {
                        config = config.with_mtu(mtu);
                    }
                    if with_mac {
                        config = config.with_mac(mac);
                    }
                    let result = backend.open(config);
                    assert!(
                        matches!(result, Err(Error::InvalidState)),
                        "{kind:?} {name:?} {mtu:?} {with_mac}: {:?}",
                        result.err()
                    );
                    checked += 1;
                }
            }
        }
    }
    // 2 kinds x 5 names x 5 MTUs x 2 MAC choices, minus the 18 accepted ones.
    assert_eq!(checked, 82);
}

// ---- flags -----------------------------------------------------------------

#[test]
fn administrative_state_follows_up_and_running_for_every_16_bit_flag_word() {
    assert_eq!(libc::IFF_UP, 0x1);
    assert_eq!(libc::IFF_RUNNING, 0x40);
    for flags in 0..=0xffff_u32 {
        let expected = if flags & 0x41 == 0x41 {
            AdminState::Up
        } else {
            AdminState::Down
        };
        assert_eq!(admin_from_if_flags(flags), expected, "{flags:#x}");
    }
    // Bits above the 16-bit interface flags change nothing.
    assert_eq!(admin_from_if_flags(0x8000_0041), AdminState::Up);
    assert_eq!(admin_from_if_flags(0xffff_ffbe), AdminState::Down);
}

#[test]
fn tun_flag_constants_and_queue_flag_masks_are_pinned() {
    for (name, actual, expected) in [
        ("IFF_TUN", libc::IFF_TUN, 0x0001),
        ("IFF_TAP", libc::IFF_TAP, 0x0002),
        ("IFF_NAPI", libc::IFF_NAPI, 0x0010),
        ("IFF_NAPI_FRAGS", libc::IFF_NAPI_FRAGS, 0x0020),
        ("IFF_MULTI_QUEUE", libc::IFF_MULTI_QUEUE, 0x0100),
        ("IFF_NO_PI", libc::IFF_NO_PI, 0x1000),
        ("IFF_VNET_HDR", libc::IFF_VNET_HDR, 0x4000),
    ] {
        assert_eq!(actual, expected, "{name}");
    }
    // Requested flags: kind, always IFF_NO_PI, optionally IFF_MULTI_QUEUE.
    for (kind, multi_queue, expected) in [
        (DeviceKind::Tun, false, 0x1001),
        (DeviceKind::Tun, true, 0x1101),
        (DeviceKind::Tap, false, 0x1002),
        (DeviceKind::Tap, true, 0x1102),
    ] {
        assert_eq!(
            tun::request_flags(kind, multi_queue).unwrap(),
            expected,
            "{kind:?} {multi_queue}"
        );
    }
    // Another queue keeps only the framing/queue-model bits of what the
    // device reports, plus IFF_NO_PI, for every 16-bit read-back.
    const KEPT: i32 = 0x0001 | 0x0002 | 0x1000 | 0x0100 | 0x4000 | 0x0010 | 0x0020;
    for read_back in 0..=0xffff {
        assert_eq!(
            tun::queue_flags(read_back),
            (read_back & KEPT) | 0x1000,
            "{read_back:#x}"
        );
        // Only header framing is refused.
        assert_eq!(
            tun::check_framing(read_back).is_ok(),
            read_back & 0x4000 == 0,
            "{read_back:#x}"
        );
    }
}

// ---- apply -------------------------------------------------------------------

/// One recorded native call.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Call {
    ReadMtu,
    SetMtu(u32),
    ReadMac,
    SetMac(MacAddress),
    SetUp(bool),
}

const OLD_MTU: u32 = 1500;
const NEW_MTU: u32 = 1400;
const OLD_MAC: MacAddress = MacAddress::new([0x02, 0, 0, 0, 0, 1]);
const NEW_MAC: MacAddress = MacAddress::new([0x02, 0, 0, 0, 0, 2]);
const ID: DeviceId = DeviceId::new(7);

/// The error a forward call at `index` fails with.
fn forward_error(index: usize) -> Error {
    Error::Platform(PlatformErrorCode::Linux(1000 + index as i32))
}

/// The error a failing restore fails with; it must never be returned.
fn restore_error() -> Error {
    Error::Platform(PlatformErrorCode::Linux(9999))
}

/// Records every call. The call at index `fail_at` fails with
/// [`forward_error`]; a restore (a write of an old value) additionally fails
/// with [`restore_error`] when `fail_restores` is set.
struct Recorder {
    calls: Vec<Call>,
    fail_at: Option<usize>,
    fail_restores: bool,
}

impl Recorder {
    fn record(&mut self, call: Call) -> Result<()> {
        let index = self.calls.len();
        let restoring = matches!(call, Call::SetMtu(OLD_MTU) | Call::SetMac(OLD_MAC));
        self.calls.push(call);
        if self.fail_at == Some(index) {
            Err(forward_error(index))
        } else if restoring && self.fail_restores {
            Err(restore_error())
        } else {
            Ok(())
        }
    }
}

impl Steps for Recorder {
    fn mtu(&mut self) -> Result<u16> {
        self.record(Call::ReadMtu)
            .map(|()| u16::try_from(OLD_MTU).unwrap())
    }
    fn set_mtu(&mut self, mtu: u16) -> Result<()> {
        self.record(Call::SetMtu(u32::from(mtu)))
    }
    fn mac(&mut self) -> Result<MacAddress> {
        self.record(Call::ReadMac).map(|()| OLD_MAC)
    }
    fn set_mac(&mut self, mac: MacAddress) -> Result<()> {
        self.record(Call::SetMac(mac))
    }
    fn set_up(&mut self, up: bool) -> Result<()> {
        self.record(Call::SetUp(up))
    }
}

const TAP: Target = Target {
    id: ID,
    kind: DeviceKind::Tap,
    mac_mutation: true,
};
const TUN: Target = Target {
    id: ID,
    kind: DeviceKind::Tun,
    mac_mutation: false,
};

fn patch(mtu: Option<u32>, mac: bool, up: Option<bool>) -> DeviceConfigPatch {
    let admin = up.map(|up| {
        if up {
            DesiredAdminState::Up
        } else {
            DesiredAdminState::Down
        }
    });
    let patch = match (mtu, admin) {
        (None, None) => DeviceConfigPatch::new_mac(ID, NEW_MAC),
        _ => DeviceConfigPatch::new(ID, admin, mtu).unwrap(),
    };
    if mac && (mtu.is_some() || admin.is_some()) {
        patch.with_mac(NEW_MAC)
    } else {
        patch
    }
}

/// The documented trace of a patch: previous values are read first and only
/// when a later step exists, then MTU, MAC, administrative state; a failed
/// step restores the earlier writes in reverse, MAC then MTU, and the failed
/// step's own error is returned. Returns the calls and the result label for
/// the call at `fail_at` failing.
fn expected_trace(
    mtu: Option<u32>,
    mac: bool,
    up: Option<bool>,
    fail_at: Option<usize>,
) -> (Vec<Call>, String) {
    let mut forward = Vec::new();
    let read_mtu = mtu.is_some() && (mac || up.is_some());
    let read_mac = mac && up.is_some();
    if read_mtu {
        forward.push(Call::ReadMtu);
    }
    if read_mac {
        forward.push(Call::ReadMac);
    }
    if let Some(mtu) = mtu {
        forward.push(Call::SetMtu(mtu));
    }
    if mac {
        forward.push(Call::SetMac(NEW_MAC));
    }
    if let Some(up) = up {
        forward.push(Call::SetUp(up));
    }
    let Some(failed) = fail_at.filter(|&index| index < forward.len()) else {
        return (forward, "Ok".to_owned());
    };
    let mut calls = forward[..=failed].to_vec();
    match forward[failed] {
        Call::SetMac(_) => {
            if read_mtu {
                calls.push(Call::SetMtu(OLD_MTU));
            }
        }
        Call::SetUp(_) => {
            if read_mac {
                calls.push(Call::SetMac(OLD_MAC));
            }
            if read_mtu {
                calls.push(Call::SetMtu(OLD_MTU));
            }
        }
        _ => {}
    }
    (calls, format!("{:?}", forward_error(failed)))
}

#[test]
fn apply_traces_and_compensation_are_pinned_for_every_patch_and_failure() {
    let mut cases = 0;
    for target in [TAP, TUN] {
        for mtu in [None, Some(NEW_MTU), Some(u32::from(u16::MAX))] {
            for mac in [false, true] {
                for up in [None, Some(true), Some(false)] {
                    if (mtu.is_none() && !mac && up.is_none())
                        || (mac && target.kind != DeviceKind::Tap)
                    {
                        continue;
                    }
                    for fail_at in [None, Some(0), Some(1), Some(2), Some(3), Some(4), Some(5)] {
                        for fail_restores in [false, true] {
                            let mut recorder = Recorder {
                                calls: Vec::new(),
                                fail_at,
                                fail_restores,
                            };
                            let result = apply_patch(&mut recorder, target, &patch(mtu, mac, up));
                            let (calls, label) = expected_trace(mtu, mac, up, fail_at);
                            let context = format!(
                                "{:?} mtu {mtu:?} mac {mac} up {up:?} fail {fail_at:?} \
                                 restores {fail_restores}",
                                target.kind
                            );
                            assert_eq!(recorder.calls, calls, "{context}");
                            let got = match result {
                                Ok(()) => "Ok".to_owned(),
                                Err(error) => format!("{error:?}"),
                            };
                            assert_eq!(got, label, "{context}");
                            cases += 1;
                        }
                    }
                }
            }
        }
    }
    // 25 valid (target, patch) shapes x 7 failure points x 2 restore modes.
    assert_eq!(cases, 350);
}

#[test]
fn apply_literal_traces_of_the_full_patch_are_pinned() {
    let full = patch(Some(NEW_MTU), true, Some(true));
    let forward = [
        Call::ReadMtu,
        Call::ReadMac,
        Call::SetMtu(NEW_MTU),
        Call::SetMac(NEW_MAC),
        Call::SetUp(true),
    ];
    let mut ok = Recorder {
        calls: Vec::new(),
        fail_at: None,
        fail_restores: false,
    };
    assert!(apply_patch(&mut ok, TAP, &full).is_ok());
    assert_eq!(ok.calls, forward);

    let mut mac_fails = Recorder {
        calls: Vec::new(),
        fail_at: Some(3),
        fail_restores: false,
    };
    assert_eq!(
        format!("{:?}", apply_patch(&mut mac_fails, TAP, &full).unwrap_err()),
        "Platform(Linux(1003))"
    );
    assert_eq!(
        mac_fails.calls,
        [
            Call::ReadMtu,
            Call::ReadMac,
            Call::SetMtu(NEW_MTU),
            Call::SetMac(NEW_MAC),
            Call::SetMtu(OLD_MTU),
        ]
    );

    let mut admin_fails = Recorder {
        calls: Vec::new(),
        fail_at: Some(4),
        fail_restores: true,
    };
    assert_eq!(
        format!(
            "{:?}",
            apply_patch(&mut admin_fails, TAP, &full).unwrap_err()
        ),
        "Platform(Linux(1004))"
    );
    assert_eq!(
        admin_fails.calls,
        [
            Call::ReadMtu,
            Call::ReadMac,
            Call::SetMtu(NEW_MTU),
            Call::SetMac(NEW_MAC),
            Call::SetUp(true),
            Call::SetMac(OLD_MAC),
            Call::SetMtu(OLD_MTU),
        ]
    );
}

/// Preconditions, in checking order: device id, MTU range, MAC on a non-TAP
/// device (both `InvalidState`), MAC without `MAC_MUTATION` (`Unsupported`).
/// None of them makes a native call.
#[test]
fn apply_preconditions_are_pinned() {
    let other_id = DeviceConfigPatch::new(DeviceId::new(8), None, Some(NEW_MTU)).unwrap();
    let huge = |mtu: u32| patch(Some(mtu), false, Some(true));
    let tap_no_mutation = Target {
        mac_mutation: false,
        ..TAP
    };
    let mac_only = patch(None, true, None);
    let huge_with_mac = patch(Some(65_536), true, None);
    let rows: [(&str, Target, DeviceConfigPatch, &str); 9] = [
        ("other device", TAP, other_id, "InvalidState"),
        ("mtu 65536", TAP, huge(65_536), "InvalidState"),
        ("mtu u32::MAX", TUN, huge(u32::MAX), "InvalidState"),
        ("mac on tun", TUN, mac_only.clone(), "InvalidState"),
        (
            "mac without mutation",
            tap_no_mutation,
            mac_only.clone(),
            "Unsupported",
        ),
        (
            "mtu before mutation",
            tap_no_mutation,
            huge_with_mac.clone(),
            "InvalidState",
        ),
        ("mtu with mac on tun", TUN, huge_with_mac, "InvalidState"),
        (
            "kind before mutation",
            Target {
                mac_mutation: false,
                ..TUN
            },
            mac_only,
            "InvalidState",
        ),
        (
            "mtu 65535 is valid",
            TUN,
            patch(Some(65_535), false, None),
            "Ok",
        ),
    ];
    for (name, target, patch, expected) in rows {
        let mut recorder = Recorder {
            calls: Vec::new(),
            fail_at: None,
            fail_restores: false,
        };
        let got = match apply_patch(&mut recorder, target, &patch) {
            Ok(()) => "Ok".to_owned(),
            Err(error) => format!("{error:?}"),
        };
        assert_eq!(got, expected, "{name}");
        let made_calls = !recorder.calls.is_empty();
        assert_eq!(made_calls, expected == "Ok", "{name}: {:?}", recorder.calls);
    }
}
