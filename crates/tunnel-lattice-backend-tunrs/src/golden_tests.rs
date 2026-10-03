//! Golden equivalence tests: they pin the backend's current, observable
//! behavior of the OS-independent rules (name rules per host OS, open
//! prechecks, the raw-errno packet rules, administrative-state flags,
//! `apply` step traces and compensation) so those rules can later move into
//! shared support modules with no change of behavior.
//!
//! Every test is table-driven, deterministic and non-privileged, runs on
//! every host (each rule takes the OS as a parameter), and goes only through
//! the crate-internal entry points of the rules, so the tables can be reused
//! unchanged against the moved implementation. A table row is the recorded
//! output of the current code; if a row ever has to change, the behavior
//! changed.

use std::cell::RefCell;
use std::io;

use tunnel_lattice_core::{Error, PlatformErrorCode, Result};
use tunnel_lattice_model::{
    AdminState, DesiredAdminState, DeviceConfig, DeviceConfigPatch, DeviceId, DeviceKind,
    MacAddress,
};

use crate::admin_state::{admin_from_flags, admin_from_oper};
use crate::offload_queue::offload_request;
use crate::open_contract::{HOST_OS, HostOs, precheck_name};
use crate::recv_contract::{
    DrainStep, Step, drain_error_step, is_refused_offload_write, offload_recv_error_step,
    recv_step, send_step,
};
use crate::{ApplySteps, ApplyTarget, DeviceProvider, TunRsBackend, apply_patch, io_error};

const ALL_OSES: [HostOs; 4] = [HostOs::Linux, HostOs::Macos, HostOs::Windows, HostOs::Other];
const KINDS: [DeviceKind; 2] = [DeviceKind::Tun, DeviceKind::Tap];

// ---- HostOs and names ------------------------------------------------------

#[test]
fn the_host_os_constant_follows_the_compile_target() {
    let expected = if cfg!(target_os = "linux") {
        HostOs::Linux
    } else if cfg!(target_os = "macos") {
        HostOs::Macos
    } else if cfg!(target_os = "windows") {
        HostOs::Windows
    } else {
        HostOs::Other
    };
    assert_eq!(HOST_OS, expected);
}

/// The corpus, with whether each (OS, kind) accepts the name. Columns, in
/// order: Linux Tun, Linux Tap, macOS Tun, macOS Tap, Windows Tun, Windows
/// Tap, other Tun, other Tap.
fn name_rows() -> Vec<(String, [bool; 8])> {
    const T: bool = true;
    const F: bool = false;
    let row = |name: &str, accepted: [bool; 8]| (name.to_owned(), accepted);
    vec![
        row("", [F, F, F, F, F, F, F, F]),
        row("a\0b", [F, F, F, F, F, F, F, F]),
        row("utun0\0", [F, F, F, F, F, F, F, F]),
        row("tun0", [T, T, F, F, T, T, T, T]),
        row("tl-test", [T, T, F, F, T, T, T, T]),
        // macOS TUN: `utun` and a canonical unit below `u32::MAX`.
        row("utun0", [T, T, T, F, T, T, T, T]),
        row("utun7", [T, T, T, F, T, T, T, T]),
        row("utun007", [T, T, F, F, T, T, T, T]),
        row("utun+7", [T, T, F, F, T, T, T, T]),
        row("utun", [T, T, F, F, T, T, T, T]),
        row("utun4294967294", [T, T, T, F, T, T, T, T]),
        row("utun4294967295", [T, T, F, F, T, T, T, T]),
        row("utun42949672950", [T, T, F, F, T, T, T, T]),
        // macOS TAP: `feth` and a canonical unit up to 0x7fff.
        row("feth0", [T, T, F, T, T, T, T, T]),
        row("feth32767", [T, T, F, T, T, T, T, T]),
        row("feth32768", [T, T, F, F, T, T, T, T]),
        row("feth007", [T, T, F, F, T, T, T, T]),
        row("feth", [T, T, F, F, T, T, T, T]),
        row("fethX", [T, T, F, F, T, T, T, T]),
        row("feth4294967295", [T, T, F, F, T, T, T, T]),
        // `%` is a naming template only on Linux.
        row("tl%d", [F, F, F, F, T, T, T, T]),
        row("%", [F, F, F, F, T, T, T, T]),
        row("feth%d", [F, F, F, F, T, T, T, T]),
        // Lengths: 15 bytes on Linux, 255 UTF-16 units on Windows.
        ("a".repeat(15), [T, T, F, F, T, T, T, T]),
        ("a".repeat(16), [F, F, F, F, T, T, T, T]),
        ("a".repeat(255), [F, F, F, F, T, T, T, T]),
        ("a".repeat(256), [F, F, F, F, F, F, T, T]),
        // Bytes versus characters: 7 two-byte characters are 14 bytes.
        ("ä".repeat(7), [T, T, F, F, T, T, T, T]),
        ("ä".repeat(8), [F, F, F, F, T, T, T, T]),
        // Units versus characters: a 4-byte character is 2 UTF-16 units.
        ("😀".repeat(3), [T, T, F, F, T, T, T, T]),
        ("😀".repeat(4), [F, F, F, F, T, T, T, T]),
        ("😀".repeat(127), [F, F, F, F, T, T, T, T]),
        (format!("{}a", "😀".repeat(127)), [F, F, F, F, T, T, T, T]),
        (format!("{}ab", "😀".repeat(127)), [F, F, F, F, F, F, T, T]),
        ("😀".repeat(128), [F, F, F, F, F, F, T, T]),
    ]
}

#[test]
fn name_rules_are_pinned_for_every_os_kind_and_corpus_name() {
    for (name, accepted) in name_rows() {
        let mut column = 0;
        for os in ALL_OSES {
            for kind in KINDS {
                let result = precheck_name(os, kind, &name);
                if accepted[column] {
                    assert!(result.is_ok(), "{os:?}/{kind:?} {name:?}");
                } else {
                    assert_eq!(
                        format!("{:?}", result.unwrap_err()),
                        "InvalidState",
                        "{os:?}/{kind:?} {name:?}"
                    );
                }
                column += 1;
            }
        }
    }
}

/// The length boundaries and the macOS `feth` unit limit, for every kind.
#[test]
fn name_boundaries_move_only_the_matching_os() {
    for kind in KINDS {
        for (os, name, accepted) in [
            (HostOs::Linux, "a".repeat(15), true),
            (HostOs::Linux, "a".repeat(16), false),
            (HostOs::Windows, "a".repeat(255), true),
            (HostOs::Windows, "a".repeat(256), false),
            (HostOs::Other, "a".repeat(4096), true),
        ] {
            assert_eq!(
                precheck_name(os, kind, &name).is_ok(),
                accepted,
                "{os:?}/{kind:?} {}",
                name.len()
            );
        }
    }
    assert!(precheck_name(HostOs::Macos, DeviceKind::Tap, "feth32767").is_ok());
    assert!(precheck_name(HostOs::Macos, DeviceKind::Tap, "feth32768").is_err());
}

#[test]
fn the_offload_request_rule_is_pinned() {
    for os in ALL_OSES {
        for kind in KINDS {
            for requested in [false, true] {
                let expected = requested && os == HostOs::Linux && kind == DeviceKind::Tun;
                assert_eq!(
                    offload_request(os, kind, requested),
                    expected,
                    "{os:?}/{kind:?} {requested}"
                );
            }
        }
    }
}

// ---- open prechecks ----------------------------------------------------------

/// Whether the prechecks must reject the request on every host: a name no OS
/// accepts, an MTU above `u16::MAX`, or a MAC address on a TUN device. The
/// documented order is kind, name, MTU, then MAC; every rejection is
/// `InvalidState`, so the order is not observable through the error. An
/// unknown (`non_exhaustive`) kind, the only `Unsupported` outcome, cannot be
/// built outside the model.
fn precheck_rejects(kind: DeviceKind, name_rejected: bool, mtu: Option<u32>, mac: bool) -> bool {
    name_rejected
        || mtu.is_some_and(|mtu| mtu > u32::from(u16::MAX))
        || (mac && kind != DeviceKind::Tap)
}

#[test]
fn open_rejects_every_failing_precheck_combination_before_any_native_call() {
    let backend = TunRsBackend::new();
    let mac = MacAddress::new([0x02, 0, 0, 0, 0, 1]);
    // Rejected by every OS: empty, a NUL, and longer than every limit.
    let rejected_names = [String::new(), "a\0b".to_owned(), "a".repeat(300)];
    let mut checked = 0;
    for kind in KINDS {
        let mut names: Vec<(Option<&str>, bool)> = vec![(None, false)];
        names.extend(
            rejected_names
                .iter()
                .map(|name| (Some(name.as_str()), true)),
        );
        for (name, name_rejected) in names {
            for mtu in [None, Some(1400), Some(65_535), Some(65_536), Some(u32::MAX)] {
                for with_mac in [false, true] {
                    if !precheck_rejects(kind, name_rejected, mtu, with_mac) {
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
    // 2 kinds x 4 names x 5 MTUs x 2 MAC choices, minus the 9 accepted ones.
    assert_eq!(checked, 71);
}

// ---- raw errno packet rules ----------------------------------------------------

/// Recorded outcome of every `recv`/`send` step for the raw errno `errno` on a
/// handle of `os`, as `(errno, outcome)` for each errno in `0..=4095` whose
/// outcome is not the generic `io_error` mapping of that same error.
fn special_errnos(step: impl Fn(io::Error) -> Step) -> Vec<(i32, String)> {
    let mut rows = Vec::new();
    for errno in 0..=4095 {
        let got = format!("{:?}", step(io::Error::from_raw_os_error(errno)));
        let fallback = format!(
            "{:?}",
            Step::Done(Err(io_error(io::Error::from_raw_os_error(errno))))
        );
        if got != fallback {
            rows.push((errno, got));
        }
    }
    rows
}

const RETRY: &str = "Retry";
const DISCONNECTED: &str = "Done(Err(Disconnected))";
const INVALID_STATE: &str = "Done(Err(InvalidState))";

fn rows(expected: &[(i32, &str)]) -> Vec<(i32, String)> {
    expected
        .iter()
        .map(|&(errno, outcome)| (errno, outcome.to_owned()))
        .collect()
}

/// The Linux values the table below is written with: `EINTR` 4, `EIO` 5,
/// `ENXIO` 6, `EFAULT` 14, `EINVAL` 22, `EBADFD` 77. macOS shares 4 and 6;
/// Windows `ERROR_OPERATION_ABORTED` is 995.
#[test]
fn recv_errno_rules_are_pinned_for_every_errno_os_and_kind() {
    for kind in KINDS {
        for (os, expected) in [
            (
                HostOs::Linux,
                rows(&[
                    (4, RETRY),
                    (6, DISCONNECTED),
                    (14, DISCONNECTED),
                    (77, DISCONNECTED),
                ]),
            ),
            (HostOs::Macos, rows(&[(4, RETRY), (6, DISCONNECTED)])),
            (
                HostOs::Windows,
                if kind == DeviceKind::Tap {
                    rows(&[(995, INVALID_STATE)])
                } else {
                    Vec::new()
                },
            ),
            (HostOs::Other, Vec::new()),
        ] {
            let got = special_errnos(|err| {
                // A TAP adapter that does not read `Up` never retries 995.
                recv_step(os, kind, 1500, Err(err), &mut false, || {
                    Ok(AdminState::Down)
                })
            });
            assert_eq!(got, expected, "recv {os:?}/{kind:?}");
        }
    }
}

#[test]
fn send_errno_rules_are_pinned_for_every_errno_os_and_kind() {
    for kind in KINDS {
        for (os, expected) in [
            (
                HostOs::Linux,
                rows(&[
                    (4, RETRY),
                    (5, INVALID_STATE),
                    (6, DISCONNECTED),
                    (77, DISCONNECTED),
                ]),
            ),
            (HostOs::Macos, rows(&[(4, RETRY), (6, DISCONNECTED)])),
            (
                HostOs::Windows,
                if kind == DeviceKind::Tap {
                    rows(&[(995, INVALID_STATE)])
                } else {
                    Vec::new()
                },
            ),
            (HostOs::Other, Vec::new()),
        ] {
            let got = special_errnos(|err| send_step(os, kind, Err(err)));
            assert_eq!(got, expected, "send {os:?}/{kind:?}");
        }
    }
}

#[test]
fn offload_recv_errno_rules_are_pinned_for_every_errno_and_os() {
    for kind in KINDS {
        for (os, expected) in [
            (
                HostOs::Linux,
                rows(&[
                    (4, RETRY),
                    (6, DISCONNECTED),
                    (14, DISCONNECTED),
                    (22, RETRY),
                    (77, DISCONNECTED),
                ]),
            ),
            (HostOs::Macos, rows(&[(4, RETRY), (6, DISCONNECTED)])),
            (
                HostOs::Windows,
                if kind == DeviceKind::Tap {
                    rows(&[(995, INVALID_STATE)])
                } else {
                    Vec::new()
                },
            ),
            (HostOs::Other, Vec::new()),
        ] {
            let got = special_errnos(|err| offload_recv_error_step(os, kind, err));
            assert_eq!(got, expected, "offload recv {os:?}/{kind:?}");
        }
    }
}

#[test]
fn drain_and_refused_write_errno_sets_are_pinned() {
    for (os, offload, expected) in [
        (HostOs::Linux, false, vec![4]),
        (HostOs::Linux, true, vec![4, 22]),
        (HostOs::Macos, false, vec![4]),
        (HostOs::Macos, true, vec![4]),
        (HostOs::Windows, false, vec![]),
        (HostOs::Windows, true, vec![]),
        (HostOs::Other, false, vec![]),
        (HostOs::Other, true, vec![]),
    ] {
        let repeats: Vec<i32> = (0..=4095)
            .filter(|&errno| {
                drain_error_step(os, offload, &io::Error::from_raw_os_error(errno))
                    == DrainStep::Repeat
            })
            .collect();
        assert_eq!(repeats, expected, "drain {os:?} offload {offload}");
    }
    for os in ALL_OSES {
        let refused: Vec<i32> = (0..=4095)
            .filter(|&errno| is_refused_offload_write(os, &io::Error::from_raw_os_error(errno)))
            .collect();
        let expected = if os == HostOs::Linux {
            vec![22]
        } else {
            vec![]
        };
        assert_eq!(refused, expected, "refused write {os:?}");
    }
}

/// A Windows TAP `recv` retries one raw 995 only while the adapter reads
/// `Up`, and a second 995 in the same call is reported.
#[test]
fn the_windows_tap_995_retry_is_pinned() {
    let abort = || io::Error::from_raw_os_error(995);
    let step = |retried: &mut bool, oper: io::Result<AdminState>| {
        format!(
            "{:?}",
            recv_step(
                HostOs::Windows,
                DeviceKind::Tap,
                1500,
                Err(abort()),
                retried,
                || oper
            )
        )
    };
    let mut retried = false;
    assert_eq!(step(&mut retried, Ok(AdminState::Up)), RETRY);
    assert!(retried);
    assert_eq!(step(&mut retried, Ok(AdminState::Up)), INVALID_STATE);
    for oper in [
        Ok(AdminState::Down),
        Ok(AdminState::Unknown),
        Err(io::Error::from_raw_os_error(5)),
    ] {
        let mut retried = false;
        assert_eq!(step(&mut retried, oper), INVALID_STATE);
        assert!(!retried);
    }
}

// ---- flags ----------------------------------------------------------------------

#[test]
fn administrative_state_flags_are_pinned_for_every_16_bit_word() {
    for flags in 0..=0xffff {
        let expected = if flags & 0x41 == 0x41 {
            AdminState::Up
        } else {
            AdminState::Down
        };
        assert_eq!(admin_from_flags(flags), expected, "{flags:#x}");
    }
}

#[test]
fn operational_status_is_up_only_for_one() {
    for status in -64..=4096 {
        let expected = if status == 1 {
            AdminState::Up
        } else {
            AdminState::Down
        };
        assert_eq!(admin_from_oper(status), expected, "{status}");
    }
}

// ---- apply ------------------------------------------------------------------------

/// One recorded native call.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Call {
    ReadMtu,
    SetMtu(u16),
    ReadMac,
    SetMac(MacAddress),
    SetEnabled(bool),
}

const OLD_MTU: u16 = 1500;
const NEW_MTU: u16 = 1400;
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
    calls: RefCell<Vec<Call>>,
    fail_at: Option<usize>,
    fail_restores: bool,
}

impl Recorder {
    fn new(fail_at: Option<usize>, fail_restores: bool) -> Self {
        Self {
            calls: RefCell::new(Vec::new()),
            fail_at,
            fail_restores,
        }
    }

    fn record(&self, call: Call) -> Result<()> {
        let restoring = matches!(call, Call::SetMtu(OLD_MTU) | Call::SetMac(OLD_MAC));
        let mut calls = self.calls.borrow_mut();
        let index = calls.len();
        calls.push(call);
        if self.fail_at == Some(index) {
            Err(forward_error(index))
        } else if restoring && self.fail_restores {
            Err(restore_error())
        } else {
            Ok(())
        }
    }
}

impl ApplySteps for Recorder {
    fn mtu(&self) -> Result<u16> {
        self.record(Call::ReadMtu).map(|()| OLD_MTU)
    }
    fn set_mtu(&self, mtu: u16) -> Result<()> {
        self.record(Call::SetMtu(mtu))
    }
    fn mac(&self) -> Result<MacAddress> {
        self.record(Call::ReadMac).map(|()| OLD_MAC)
    }
    fn set_mac(&self, mac: MacAddress) -> Result<()> {
        self.record(Call::SetMac(mac))
    }
    fn set_enabled(&self, enabled: bool) -> Result<()> {
        self.record(Call::SetEnabled(enabled))
    }
}

const TAP: ApplyTarget = ApplyTarget {
    id: ID,
    kind: DeviceKind::Tap,
    mac_mutation: true,
};
const TUN: ApplyTarget = ApplyTarget {
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

fn label(result: Result<()>) -> String {
    match result {
        Ok(()) => "Ok".to_owned(),
        Err(error) => format!("{error:?}"),
    }
}

/// The documented trace of a patch: previous values are read first and only
/// when a later step exists, then MTU, MAC, administrative state; a failed
/// step restores the earlier writes in reverse, MAC then MTU, and the failed
/// step's own error is returned. Returns the calls and the result label for
/// the call at `fail_at` failing.
fn expected_trace(
    mtu: Option<u16>,
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
        forward.push(Call::SetEnabled(up));
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
        Call::SetEnabled(_) => {
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
        for mtu in [None, Some(NEW_MTU), Some(u16::MAX)] {
            for mac in [false, true] {
                for up in [None, Some(true), Some(false)] {
                    if (mtu.is_none() && !mac && up.is_none())
                        || (mac && target.kind != DeviceKind::Tap)
                    {
                        continue;
                    }
                    for fail_at in [None, Some(0), Some(1), Some(2), Some(3), Some(4), Some(5)] {
                        for fail_restores in [false, true] {
                            let recorder = Recorder::new(fail_at, fail_restores);
                            let result =
                                apply_patch(&recorder, target, &patch(mtu.map(u32::from), mac, up));
                            let (calls, expected) = expected_trace(mtu, mac, up, fail_at);
                            let context = format!(
                                "{:?} mtu {mtu:?} mac {mac} up {up:?} fail {fail_at:?} \
                                 restores {fail_restores}",
                                target.kind
                            );
                            assert_eq!(*recorder.calls.borrow(), calls, "{context}");
                            assert_eq!(label(result), expected, "{context}");
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
    let full = patch(Some(u32::from(NEW_MTU)), true, Some(true));
    let forward = [
        Call::ReadMtu,
        Call::ReadMac,
        Call::SetMtu(NEW_MTU),
        Call::SetMac(NEW_MAC),
        Call::SetEnabled(true),
    ];
    let ok = Recorder::new(None, false);
    assert!(apply_patch(&ok, TAP, &full).is_ok());
    assert_eq!(*ok.calls.borrow(), forward);

    let mac_fails = Recorder::new(Some(3), false);
    assert_eq!(
        label(apply_patch(&mac_fails, TAP, &full)),
        "Platform(Linux(1003))"
    );
    assert_eq!(
        *mac_fails.calls.borrow(),
        [
            Call::ReadMtu,
            Call::ReadMac,
            Call::SetMtu(NEW_MTU),
            Call::SetMac(NEW_MAC),
            Call::SetMtu(OLD_MTU),
        ]
    );

    let admin_fails = Recorder::new(Some(4), true);
    assert_eq!(
        label(apply_patch(&admin_fails, TAP, &full)),
        "Platform(Linux(1004))"
    );
    assert_eq!(
        *admin_fails.calls.borrow(),
        [
            Call::ReadMtu,
            Call::ReadMac,
            Call::SetMtu(NEW_MTU),
            Call::SetMac(NEW_MAC),
            Call::SetEnabled(true),
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
    let other_id =
        DeviceConfigPatch::new(DeviceId::new(8), None, Some(u32::from(NEW_MTU))).unwrap();
    let huge = |mtu: u32| patch(Some(mtu), false, Some(true));
    let tap_no_mutation = ApplyTarget {
        mac_mutation: false,
        ..TAP
    };
    let mac_only = patch(None, true, None);
    let huge_with_mac = patch(Some(65_536), true, None);
    let rows: [(&str, ApplyTarget, DeviceConfigPatch, &str); 9] = [
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
            ApplyTarget {
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
        let recorder = Recorder::new(None, false);
        assert_eq!(
            label(apply_patch(&recorder, target, &patch)),
            expected,
            "{name}"
        );
        let made_calls = !recorder.calls.borrow().is_empty();
        assert_eq!(made_calls, expected == "Ok", "{name}");
    }
}
