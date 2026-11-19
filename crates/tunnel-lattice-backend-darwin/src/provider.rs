//! Private, incomplete TUN-only provider boundary.
//!
//! Mutation is intentionally not implemented. The native observation path
//! must be validated on macOS before the backend is exposed.

use std::io;

use crate::utun_frame::BufferTooSmall;
use tunnel_lattice_core::{Error, PlatformErrorCode, Result};
use tunnel_lattice_model::{
    DeviceConfig, DeviceKind,
    backend::{HostOs, precheck_open},
};
use tunnel_lattice_platform::backend::errno::darwin::ENXIO;

#[cfg(target_os = "macos")]
use tunnel_lattice_platform::{
    Capability, CapabilityProvider, DeviceObserver, DeviceProvider, PacketIo,
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

#[cfg(target_os = "macos")]
struct DarwinBackend;

#[cfg(target_os = "macos")]
struct DarwinDevice {
    utun: Utun,
    id: tunnel_lattice_model::DeviceId,
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
        })
    }
}

#[cfg(target_os = "macos")]
impl CapabilityProvider for DarwinBackend {
    fn capabilities(&self) -> Capability {
        Capability::empty()
    }
}

#[cfg(target_os = "macos")]
impl CapabilityProvider for DarwinDevice {
    fn capabilities(&self) -> Capability {
        Capability::empty()
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
        let name = self.utun.name().map_err(snapshot_error)?;
        snapshot_by_name(self.id, name, crate::observe::index, crate::observe::state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(!device.capabilities().contains(Capability::DEVICE_MUTATION));
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
