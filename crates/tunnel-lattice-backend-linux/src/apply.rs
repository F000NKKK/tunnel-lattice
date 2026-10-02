//! The `DeviceMutator::apply` contract.
//!
//! 1. Every precondition is checked before any native call: a patch for
//!    another device, an MTU above `u16::MAX`, or a MAC address for a TUN
//!    device is `Error::InvalidState`; an administrative state this backend
//!    does not know, or a MAC address on a handle without
//!    `Capability::MAC_MUTATION`, is `Error::Unsupported`.
//! 2. The MTU is applied first, then the MAC address, then the
//!    administrative state, each as its own native request.
//! 3. A step's previous value is read before any change, and only when a
//!    later step exists that could fail and need it restored; a failed read
//!    returns its error with nothing changed. When a step fails, the steps
//!    already applied are restored in reverse order, best effort, and the
//!    failed step's own error is returned.
//!
//! After any `Err`, `snapshot` is authoritative.
//!
//! The steps are driven through [`Steps`], so ordering and compensation are
//! tested without a device.

use tunnel_lattice_core::{Error, Result};
use tunnel_lattice_model::{
    DesiredAdminState, DeviceConfigPatch, DeviceId, DeviceKind, MacAddress,
};

use crate::control::{LinkChange, LinkQuery, RouteSocket};
use crate::errno;

/// The native steps [`apply`] drives.
pub(crate) trait Steps {
    /// Reads the current MTU.
    fn mtu(&mut self) -> Result<u32>;
    /// Sets the MTU.
    fn set_mtu(&mut self, mtu: u32) -> Result<()>;
    /// Reads the current MAC address.
    fn mac(&mut self) -> Result<MacAddress>;
    /// Sets the MAC address.
    fn set_mac(&mut self, mac: MacAddress) -> Result<()>;
    /// Sets (`true`) or clears (`false`) the administrative up flag.
    fn set_up(&mut self, up: bool) -> Result<()>;
}

/// What a patch is checked against.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Target {
    /// The handle's device identity.
    pub(crate) id: DeviceId,
    /// The device's kind.
    pub(crate) kind: DeviceKind,
    /// Whether the handle reports `Capability::MAC_MUTATION`.
    pub(crate) mac_mutation: bool,
}

/// Applies `patch` through `steps`; see the module docs.
pub(crate) fn apply(
    steps: &mut impl Steps,
    target: Target,
    patch: &DeviceConfigPatch,
) -> Result<()> {
    if patch.device_id() != target.id {
        return Err(Error::InvalidState);
    }
    let mtu = match patch.mtu() {
        Some(mtu) if mtu > u32::from(u16::MAX) => return Err(Error::InvalidState),
        mtu => mtu,
    };
    let mac = patch.mac();
    if mac.is_some() && target.kind != DeviceKind::Tap {
        return Err(Error::InvalidState);
    }
    let up = match patch.admin_state() {
        None => None,
        Some(DesiredAdminState::Up) => Some(true),
        Some(DesiredAdminState::Down) => Some(false),
        // `#[non_exhaustive]`: a state this backend does not know.
        Some(_) => return Err(Error::Unsupported),
    };
    if mac.is_some() && !target.mac_mutation {
        return Err(Error::Unsupported);
    }

    // Previous values, read before anything changes, only where a later
    // step exists.
    let old_mtu = match mtu {
        Some(_) if mac.is_some() || up.is_some() => Some(steps.mtu()?),
        _ => None,
    };
    let old_mac = match mac {
        Some(_) if up.is_some() => Some(steps.mac()?),
        _ => None,
    };

    if let Some(mtu) = mtu {
        steps.set_mtu(mtu)?;
    }
    if let Some(mac) = mac
        && let Err(error) = steps.set_mac(mac)
    {
        restore(steps, old_mtu, None);
        return Err(error);
    }
    if let Some(up) = up
        && let Err(error) = steps.set_up(up)
    {
        restore(steps, old_mtu, old_mac);
        return Err(error);
    }
    Ok(())
}

/// Best-effort compensation, in reverse order (MAC, then MTU). A failed
/// restore is not reported: the original error is what the caller needs.
fn restore(steps: &mut impl Steps, mtu: Option<u32>, mac: Option<MacAddress>) {
    if let Some(mac) = mac {
        let _ = steps.set_mac(mac);
    }
    if let Some(mtu) = mtu {
        let _ = steps.set_mtu(mtu);
    }
}

/// The steps on a real link: one `RTM_GETLINK` per read and one
/// `RTM_NEWLINK` per change, by interface index.
pub(crate) struct LinkSteps<'a> {
    /// The device's control socket, locked for the whole `apply`.
    pub(crate) socket: &'a mut RouteSocket,
    /// The device's interface index.
    pub(crate) index: u32,
}

impl Steps for LinkSteps<'_> {
    fn mtu(&mut self) -> Result<u32> {
        Ok(self.socket.get_link(LinkQuery::Index(self.index))?.mtu)
    }

    fn set_mtu(&mut self, mtu: u32) -> Result<()> {
        self.socket.set_link(self.index, LinkChange::Mtu(mtu))
    }

    /// A link with no hardware address cannot have it restored; that is
    /// reported as a malformed reply, so `apply` changes nothing.
    fn mac(&mut self) -> Result<MacAddress> {
        self.socket
            .get_link(LinkQuery::Index(self.index))?
            .mac
            .map(MacAddress::new)
            .ok_or_else(errno::malformed)
    }

    fn set_mac(&mut self, mac: MacAddress) -> Result<()> {
        self.socket
            .set_link(self.index, LinkChange::Mac(mac.octets()))
    }

    fn set_up(&mut self, up: bool) -> Result<()> {
        self.socket.set_link(self.index, LinkChange::AdminUp(up))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One call the fake recorded.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Call {
        ReadMtu,
        SetMtu(u32),
        ReadMac,
        SetMac(MacAddress),
        SetUp(bool),
    }

    /// Records every call; the call at index `fail_at` (if any) fails with
    /// `Platform(Linux(EINVAL))`, and every restore fails too when
    /// `fail_restores` is set.
    #[derive(Default)]
    struct Fake {
        calls: Vec<Call>,
        fail_at: Option<usize>,
        fail_restores: bool,
    }

    const OLD_MTU: u32 = 1500;
    const OLD_MAC: MacAddress = MacAddress::new([0x02, 0, 0, 0, 0, 1]);
    const NEW_MAC: MacAddress = MacAddress::new([0x02, 0, 0, 0, 0, 2]);

    impl Fake {
        fn failing_at(index: usize) -> Self {
            Self {
                fail_at: Some(index),
                ..Self::default()
            }
        }

        fn record(&mut self, call: Call) -> Result<()> {
            let index = self.calls.len();
            let restoring = matches!(call, Call::SetMtu(OLD_MTU) | Call::SetMac(OLD_MAC));
            self.calls.push(call);
            if self.fail_at == Some(index) || (restoring && self.fail_restores) {
                Err(errno::platform(libc::EINVAL))
            } else {
                Ok(())
            }
        }
    }

    impl Steps for Fake {
        fn mtu(&mut self) -> Result<u32> {
            self.record(Call::ReadMtu).map(|()| OLD_MTU)
        }
        fn set_mtu(&mut self, mtu: u32) -> Result<()> {
            self.record(Call::SetMtu(mtu))
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

    const ID: DeviceId = DeviceId::new(7);
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

    fn patch(mtu: Option<u32>, mac: Option<MacAddress>, up: Option<bool>) -> DeviceConfigPatch {
        let admin = up.map(|up| {
            if up {
                DesiredAdminState::Up
            } else {
                DesiredAdminState::Down
            }
        });
        let patch = match (mtu, admin) {
            (None, None) => DeviceConfigPatch::new_mac(ID, mac.unwrap()),
            _ => DeviceConfigPatch::new(ID, admin, mtu).unwrap(),
        };
        match mac {
            Some(mac) => patch.with_mac(mac),
            None => patch,
        }
    }

    fn is_einval(error: &Error) -> bool {
        matches!(
            error,
            Error::Platform(tunnel_lattice_core::PlatformErrorCode::Linux(libc::EINVAL))
        )
    }

    #[test]
    fn preconditions_fail_before_any_native_call() {
        let mut fake = Fake::default();
        let other = DeviceConfigPatch::new(DeviceId::new(8), None, Some(1400)).unwrap();
        assert!(apply(&mut fake, TAP, &other).is_err_and(|e| e.is_invalid_state()));
        let huge = patch(Some(u32::from(u16::MAX) + 1), None, Some(true));
        assert!(apply(&mut fake, TAP, &huge).is_err_and(|e| e.is_invalid_state()));
        let tun_mac = patch(None, Some(NEW_MAC), None);
        assert!(apply(&mut fake, TUN, &tun_mac).is_err_and(|e| e.is_invalid_state()));
        let no_mutation = Target {
            mac_mutation: false,
            ..TAP
        };
        assert!(apply(&mut fake, no_mutation, &tun_mac).is_err_and(|e| e.is_unsupported()));
        assert!(fake.calls.is_empty());

        // The largest representable MTU is accepted.
        apply(
            &mut fake,
            TUN,
            &patch(Some(u32::from(u16::MAX)), None, None),
        )
        .unwrap();
        assert_eq!(fake.calls, [Call::SetMtu(65535)]);
    }

    #[test]
    fn a_single_step_reads_nothing_first() {
        for (patch, call) in [
            (patch(Some(1400), None, None), Call::SetMtu(1400)),
            (patch(None, Some(NEW_MAC), None), Call::SetMac(NEW_MAC)),
            (patch(None, None, Some(false)), Call::SetUp(false)),
        ] {
            let mut fake = Fake::default();
            apply(&mut fake, TAP, &patch).unwrap();
            assert_eq!(fake.calls, [call]);
        }
    }

    #[test]
    fn every_step_runs_mtu_then_mac_then_admin_after_the_reads() {
        let mut fake = Fake::default();
        apply(
            &mut fake,
            TAP,
            &patch(Some(1400), Some(NEW_MAC), Some(true)),
        )
        .unwrap();
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
        apply(&mut fake, TAP, &patch(Some(1400), Some(NEW_MAC), None)).unwrap();
        assert_eq!(
            fake.calls,
            [Call::ReadMtu, Call::SetMtu(1400), Call::SetMac(NEW_MAC)]
        );
    }

    #[test]
    fn a_failed_read_changes_nothing() {
        let mut fake = Fake::failing_at(0);
        let error = apply(&mut fake, TAP, &patch(Some(1400), None, Some(true))).unwrap_err();
        assert!(is_einval(&error));
        assert_eq!(fake.calls, [Call::ReadMtu]);

        let mut fake = Fake::failing_at(1);
        let error = apply(
            &mut fake,
            TAP,
            &patch(Some(1400), Some(NEW_MAC), Some(true)),
        )
        .unwrap_err();
        assert!(is_einval(&error));
        assert_eq!(fake.calls, [Call::ReadMtu, Call::ReadMac]);
    }

    #[test]
    fn a_failed_mtu_step_stops_there() {
        let mut fake = Fake::failing_at(1);
        let error = apply(&mut fake, TAP, &patch(Some(1400), None, Some(true))).unwrap_err();
        assert!(is_einval(&error));
        assert_eq!(fake.calls, [Call::ReadMtu, Call::SetMtu(1400)]);
    }

    #[test]
    fn a_failed_mac_step_restores_the_mtu_and_skips_the_admin_step() {
        let mut fake = Fake::failing_at(3);
        let error = apply(
            &mut fake,
            TAP,
            &patch(Some(1400), Some(NEW_MAC), Some(true)),
        )
        .unwrap_err();
        assert!(is_einval(&error));
        assert_eq!(
            fake.calls,
            [
                Call::ReadMtu,
                Call::ReadMac,
                Call::SetMtu(1400),
                Call::SetMac(NEW_MAC),
                Call::SetMtu(OLD_MTU),
            ]
        );
    }

    #[test]
    fn a_failed_admin_step_restores_the_mac_then_the_mtu() {
        let mut fake = Fake::failing_at(4);
        let error = apply(
            &mut fake,
            TAP,
            &patch(Some(1400), Some(NEW_MAC), Some(false)),
        )
        .unwrap_err();
        assert!(is_einval(&error));
        assert_eq!(
            fake.calls,
            [
                Call::ReadMtu,
                Call::ReadMac,
                Call::SetMtu(1400),
                Call::SetMac(NEW_MAC),
                Call::SetUp(false),
                Call::SetMac(OLD_MAC),
                Call::SetMtu(OLD_MTU),
            ]
        );
    }

    #[test]
    fn a_failed_restore_still_returns_the_original_error() {
        let mut fake = Fake {
            fail_at: Some(2),
            fail_restores: true,
            ..Fake::default()
        };
        let error = apply(&mut fake, TUN, &patch(Some(1400), None, Some(true))).unwrap_err();
        // The admin step's own error, not the restore's.
        assert!(is_einval(&error));
        assert_eq!(
            fake.calls,
            [
                Call::ReadMtu,
                Call::SetMtu(1400),
                Call::SetUp(true),
                Call::SetMtu(OLD_MTU),
            ]
        );
    }
}
