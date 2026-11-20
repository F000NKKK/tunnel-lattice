//! Private, incomplete TUN-only provider boundary.
//!
//! Native observation and mutation must be validated on macOS before the
//! backend is exposed.

use std::io;

use crate::utun_frame::BufferTooSmall;
use tunnel_lattice_core::{Error, PlatformErrorCode, Result};
use tunnel_lattice_model::{
    DeviceConfig, DeviceConfigPatch, DeviceId, DeviceKind,
    backend::{ApplySteps, ApplyTarget, HostOs, apply_patch, precheck_open},
};
use tunnel_lattice_platform::backend::errno::darwin::ENXIO;

#[cfg(target_os = "macos")]
use tunnel_lattice_platform::{
    Capability, CapabilityProvider, DeviceMutator, DeviceObserver, DeviceProvider, PacketIo,
};

#[cfg(target_os = "macos")]
use crate::utun::Utun;

/// Validate without a native call. The initial implementation only accepts
/// automatically named TUN devices and does not apply an MTU at open.
fn check_config(config: &DeviceConfig) -> Result<()> {
    if config.kind != DeviceKind::Tun {
        return Err(Error::Unsupported);
    }
    precheck_open(HostOs::Macos, config)?;
    if config.name.is_some() || config.mtu.is_some() {
        return Err(Error::Unsupported);
    }
    // Both are optional capability requests and are ignored off Linux.
    Ok(())
}

/// Matches the existing tun-rs backend's generic error mapping. ENXIO is
/// a macOS receive/send lifecycle signal, but is not interpreted that way
/// when opening a device.
fn map_error(err: io::Error, packet_io: bool, recv: bool) -> Error {
    if packet_io && err.raw_os_error() == Some(ENXIO) {
        return Error::Disconnected;
    }
    if recv
        && err
            .get_ref()
            .is_some_and(|source| source.is::<BufferTooSmall>())
    {
        return Error::BufferTooSmall;
    }
    match err.kind() {
        io::ErrorKind::PermissionDenied => Error::PermissionDenied,
        io::ErrorKind::NotFound => Error::NotFound,
        io::ErrorKind::AlreadyExists => Error::AlreadyExists,
        io::ErrorKind::BrokenPipe | io::ErrorKind::UnexpectedEof | io::ErrorKind::NotConnected => {
            Error::Disconnected
        }
        io::ErrorKind::Unsupported => Error::Unsupported,
        _ => Error::Platform(
            err.raw_os_error()
                .map_or(PlatformErrorCode::Unknown, PlatformErrorCode::Darwin),
        ),
    }
}

/// A vanished interface is a missing object during observation. Keep open
/// and packet-I/O error semantics separate from this classification.
fn snapshot_error(err: io::Error) -> Error {
    if err.raw_os_error() == Some(ENXIO) || err.kind() == io::ErrorKind::NotFound {
        Error::NotFound
    } else {
        map_error(err, false, false)
    }
}

/// Check the identity around the name-based ioctls and reject a detected
/// replacement. Independent reads cannot provide an atomic snapshot against
/// removal and index reuse (ABA).
fn snapshot_by_name(
    id: tunnel_lattice_model::DeviceId,
    name: String,
    mut index: impl FnMut(&str) -> io::Result<u32>,
    state: impl FnOnce(&str) -> io::Result<(u32, tunnel_lattice_model::AdminState)>,
) -> Result<tunnel_lattice_model::Device> {
    if u64::from(index(&name).map_err(snapshot_error)?) != id.value() {
        return Err(Error::NotFound);
    }
    let (mtu, admin_state) = state(&name).map_err(snapshot_error)?;
    if u64::from(index(&name).map_err(snapshot_error)?) != id.value() {
        return Err(Error::NotFound);
    }
    Ok(tunnel_lattice_model::Device::new(
        id,
        name,
        DeviceKind::Tun,
        mtu,
        admin_state,
    ))
}

/// Share exactly the model's precondition and compensation contract.
fn apply_tun(steps: &mut impl ApplySteps, id: DeviceId, patch: &DeviceConfigPatch) -> Result<()> {
    apply_patch(
        steps,
        ApplyTarget {
            id,
            kind: DeviceKind::Tun,
            mac_mutation: false,
        },
        patch,
    )
}

#[cfg(target_os = "macos")]
struct DarwinBackend;

#[cfg(target_os = "macos")]
struct DarwinDevice {
    utun: Utun,
    id: tunnel_lattice_model::DeviceId,
    control: std::sync::Mutex<()>,
}

#[cfg(target_os = "macos")]
impl DeviceProvider for DarwinBackend {
    type DeviceConfig = DeviceConfig;
    type Device = DarwinDevice;

    fn open(&self, config: DeviceConfig) -> Result<DarwinDevice> {
        check_config(&config)?;
        let utun = Utun::open().map_err(|err| map_error(err, false, false))?;
        let name = utun.name().map_err(|err| map_error(err, false, false))?;
        let index = crate::observe::index(&name).map_err(|err| map_error(err, false, false))?;
        Ok(DarwinDevice {
            utun,
            id: tunnel_lattice_model::DeviceId::new(u64::from(index)),
            control: std::sync::Mutex::new(()),
        })
    }
}

#[cfg(target_os = "macos")]
impl CapabilityProvider for DarwinBackend {
    fn capabilities(&self) -> Capability {
        Capability::DEVICE_MUTATION
    }
}

#[cfg(target_os = "macos")]
impl CapabilityProvider for DarwinDevice {
    fn capabilities(&self) -> Capability {
        Capability::DEVICE_MUTATION
    }
}

#[cfg(target_os = "macos")]
impl PacketIo for DarwinDevice {
    fn recv(&self, buf: &mut [u8]) -> Result<usize> {
        self.utun
            .recv(buf)
            .map_err(|err| map_error(err, true, true))
    }

    fn send(&self, buf: &[u8]) -> Result<usize> {
        self.utun
            .send(buf)
            .map_err(|err| map_error(err, true, false))
    }
}

#[cfg(target_os = "macos")]
impl DeviceObserver for DarwinDevice {
    type Device = tunnel_lattice_model::Device;

    fn id(&self) -> tunnel_lattice_model::DeviceId {
        self.id
    }

    fn snapshot(&self) -> Result<Self::Device> {
        let _guard = self
            .control
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let name = self.utun.name().map_err(snapshot_error)?;
        snapshot_by_name(self.id, name, crate::observe::index, crate::observe::state)
    }
}

#[cfg(target_os = "macos")]
struct DarwinSteps<'a>(&'a DarwinDevice);

#[cfg(target_os = "macos")]
impl DarwinSteps<'_> {
    /// Check identity immediately before a name-based operation. Darwin's
    /// ioctl API cannot make this check atomic with mutation.
    fn name(&self) -> Result<String> {
        let name = self.0.utun.name().map_err(snapshot_error)?;
        let index = crate::observe::index(&name).map_err(snapshot_error)?;
        if u64::from(index) != self.0.id.value() {
            return Err(Error::NotFound);
        }
        Ok(name)
    }
}

#[cfg(target_os = "macos")]
impl ApplySteps for DarwinSteps<'_> {
    fn mtu(&mut self) -> Result<u16> {
        let name = self.name()?;
        let (mtu, _) = crate::observe::state(&name).map_err(snapshot_error)?;
        u16::try_from(mtu).map_err(|_| Error::InvalidState)
    }

    fn set_mtu(&mut self, mtu: u16) -> Result<()> {
        let name = self.name()?;
        crate::observe::set_mtu(&name, mtu).map_err(snapshot_error)
    }

    fn mac(&mut self) -> Result<tunnel_lattice_model::MacAddress> {
        Err(Error::Unsupported)
    }

    fn set_mac(&mut self, _mac: tunnel_lattice_model::MacAddress) -> Result<()> {
        Err(Error::Unsupported)
    }

    fn set_up(&mut self, up: bool) -> Result<()> {
        let name = self.name()?;
        crate::observe::set_up(&name, up).map_err(snapshot_error)
    }
}

#[cfg(target_os = "macos")]
impl DeviceMutator for DarwinDevice {
    type DeviceConfigPatch = DeviceConfigPatch;

    fn apply(&self, patch: Self::DeviceConfigPatch) -> Result<()> {
        let _guard = self
            .control
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        apply_tun(&mut DarwinSteps(self), self.id, &patch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeSteps {
        calls: Vec<String>,
        fail_up: bool,
    }

    impl ApplySteps for FakeSteps {
        fn mtu(&mut self) -> Result<u16> {
            self.calls.push("read_mtu".into());
            Ok(1500)
        }
        fn set_mtu(&mut self, mtu: u16) -> Result<()> {
            self.calls.push(format!("mtu={mtu}"));
            Ok(())
        }
        fn mac(&mut self) -> Result<tunnel_lattice_model::MacAddress> {
            panic!("TUN must not read MAC")
        }
        fn set_mac(&mut self, _: tunnel_lattice_model::MacAddress) -> Result<()> {
            panic!("TUN must not write MAC")
        }
        fn set_up(&mut self, up: bool) -> Result<()> {
            self.calls.push(format!("up={up}"));
            if self.fail_up {
                Err(Error::PermissionDenied)
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn mutation_rejects_invalid_patches_before_any_native_step() {
        use tunnel_lattice_model::{DesiredAdminState, MacAddress};
        let id = DeviceId::new(7);
        let mut steps = FakeSteps::default();
        let wrong_id =
            DeviceConfigPatch::new(DeviceId::new(8), Some(DesiredAdminState::Up), None).unwrap();
        assert!(matches!(
            apply_tun(&mut steps, id, &wrong_id),
            Err(Error::InvalidState)
        ));
        let big_mtu = DeviceConfigPatch::new(id, None, Some(65536)).unwrap();
        assert!(matches!(
            apply_tun(&mut steps, id, &big_mtu),
            Err(Error::InvalidState)
        ));
        let mac = DeviceConfigPatch::new_mac(id, MacAddress::new([2, 0, 0, 0, 0, 1]));
        assert!(matches!(
            apply_tun(&mut steps, id, &mac),
            Err(Error::InvalidState)
        ));
        assert!(steps.calls.is_empty());
    }

    #[test]
    fn failed_admin_change_compensates_mtu_and_returns_original_error() {
        use tunnel_lattice_model::DesiredAdminState;
        let id = DeviceId::new(7);
        let mut steps = FakeSteps {
            fail_up: true,
            ..Default::default()
        };
        let patch = DeviceConfigPatch::new(id, Some(DesiredAdminState::Up), Some(1400)).unwrap();
        assert!(matches!(
            apply_tun(&mut steps, id, &patch),
            Err(Error::PermissionDenied)
        ));
        assert_eq!(steps.calls, ["read_mtu", "mtu=1400", "up=true", "mtu=1500"]);
    }

    #[test]
    fn snapshot_enxio_is_not_found_but_open_preserves_platform_error() {
        assert!(matches!(
            snapshot_error(io::Error::from_raw_os_error(ENXIO)),
            Error::NotFound
        ));
        assert!(matches!(
            map_error(io::Error::from_raw_os_error(ENXIO), false, false),
            Error::Platform(PlatformErrorCode::Darwin(ENXIO))
        ));
        assert!(matches!(
            snapshot_error(io::ErrorKind::PermissionDenied.into()),
            Error::PermissionDenied
        ));
    }

    #[test]
    fn snapshot_checks_identity_before_and_after_reading_state() {
        use tunnel_lattice_model::{AdminState, DeviceId};
        let id = DeviceId::new(7);
        let snapshot = snapshot_by_name(
            id,
            "utun7".into(),
            |_| Ok(7),
            |_| Ok((1500, AdminState::Up)),
        )
        .unwrap();
        assert_eq!((snapshot.id, snapshot.mtu), (id, 1500));
        assert!(matches!(
            snapshot_by_name(
                id,
                "utun7".into(),
                |_| Ok(8),
                |_| panic!("must not read another interface")
            ),
            Err(Error::NotFound)
        ));
        let mut indexes = [7, 8].into_iter();
        assert!(matches!(
            snapshot_by_name(
                id,
                "utun7".into(),
                |_| Ok(indexes.next().unwrap()),
                |_| Ok((1500, AdminState::Up))
            ),
            Err(Error::NotFound)
        ));
        assert!(matches!(
            snapshot_by_name(
                id,
                "utun7".into(),
                |_| Ok(7),
                |_| Err(io::Error::from_raw_os_error(ENXIO))
            ),
            Err(Error::NotFound)
        ));
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "opens a real utun interface and requires macOS privileges"]
    fn opened_tun_has_a_stable_identity_and_observable_snapshot() {
        let device = DarwinBackend
            .open(DeviceConfig::new(DeviceKind::Tun))
            .unwrap();
        let id = device.id();
        let first = device.snapshot().unwrap();
        let second = device.snapshot().unwrap();
        assert_eq!(first.id, id);
        assert_eq!(second.id, id);
        assert_eq!(first.kind, DeviceKind::Tun);
        assert_eq!(second.kind, DeviceKind::Tun);
        assert!(first.name.starts_with("utun"));
        assert!(first.mtu > 0);
        assert!(first.mac.is_none());
        assert!(device.capabilities().contains(Capability::DEVICE_MUTATION));
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "opens and mutates a real utun interface and requires macOS privileges"]
    fn real_tun_mtu_and_admin_patches_are_observable() {
        use tunnel_lattice_model::{AdminState, DesiredAdminState};
        let device = DarwinBackend
            .open(DeviceConfig::new(DeviceKind::Tun))
            .unwrap();
        let id = device.id();
        device
            .apply(DeviceConfigPatch::new(id, None, Some(1300)).unwrap())
            .unwrap();
        assert_eq!(device.snapshot().unwrap().mtu, 1300);
        device
            .apply(DeviceConfigPatch::new(id, Some(DesiredAdminState::Up), None).unwrap())
            .unwrap();
        assert_eq!(device.snapshot().unwrap().admin_state, AdminState::Up);
        device
            .apply(DeviceConfigPatch::new(id, Some(DesiredAdminState::Down), None).unwrap())
            .unwrap();
        assert_eq!(device.snapshot().unwrap().admin_state, AdminState::Down);
    }

    #[test]
    fn unsupported_and_invalid_requests_fail_before_native_calls() {
        assert!(check_config(&DeviceConfig::new(DeviceKind::Tun)).is_ok());
        assert!(matches!(
            check_config(&DeviceConfig::new(DeviceKind::Tap)),
            Err(Error::Unsupported)
        ));
        assert!(matches!(
            check_config(&DeviceConfig::new(DeviceKind::Tun).with_name("utun1")),
            Err(Error::Unsupported)
        ));
        assert!(matches!(
            check_config(&DeviceConfig::new(DeviceKind::Tun).with_name("wrong")),
            Err(Error::InvalidState)
        ));
        assert!(matches!(
            check_config(&DeviceConfig::new(DeviceKind::Tun).with_mtu(1500)),
            Err(Error::Unsupported)
        ));
        assert!(matches!(
            check_config(&DeviceConfig::new(DeviceKind::Tun).with_mtu(65536)),
            Err(Error::InvalidState)
        ));
        assert!(
            check_config(
                &DeviceConfig::new(DeviceKind::Tun)
                    .with_offload(true)
                    .with_multi_queue(true)
            )
            .is_ok()
        );
    }

    #[test]
    fn receive_overflow_is_distinct_from_native_invalid_input_and_send() {
        let overflow = || io::Error::new(io::ErrorKind::InvalidData, BufferTooSmall);
        assert!(matches!(
            map_error(overflow(), true, true),
            Error::BufferTooSmall
        ));
        assert!(matches!(
            map_error(overflow(), true, false),
            Error::Platform(PlatformErrorCode::Unknown)
        ));
        assert!(matches!(
            map_error(io::ErrorKind::InvalidInput.into(), true, true),
            Error::Platform(PlatformErrorCode::Unknown)
        ));
        assert!(matches!(
            map_error(io::ErrorKind::InvalidData.into(), true, true),
            Error::Platform(PlatformErrorCode::Unknown)
        ));
        assert!(matches!(
            map_error(io::Error::from_raw_os_error(22), true, true),
            Error::Platform(PlatformErrorCode::Darwin(22))
        ));
    }

    #[test]
    fn native_and_lifecycle_errors_are_classified_without_losing_codes() {
        assert!(matches!(
            map_error(io::ErrorKind::PermissionDenied.into(), false, false),
            Error::PermissionDenied
        ));
        assert!(matches!(
            map_error(io::ErrorKind::UnexpectedEof.into(), true, true),
            Error::Disconnected
        ));
        assert!(matches!(
            map_error(io::Error::from_raw_os_error(ENXIO), true, false),
            Error::Disconnected
        ));
        assert!(matches!(
            map_error(io::Error::from_raw_os_error(ENXIO), false, false),
            Error::Platform(PlatformErrorCode::Darwin(ENXIO))
        ));
    }
}
