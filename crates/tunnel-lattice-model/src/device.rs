use tunnel_lattice_core::{Error, Id, Result};

/// Identifies a [`Device`].
///
/// Construction convention mirrors `net-lattice-model::InterfaceId`: a
/// backend widens whatever native handle/index it has (a Linux `ifindex`
/// after the device is created, a `tun-rs` internal device index, ...) to
/// `u64` — `DeviceId::new(u64::from(native_index))`. No backend derives a
/// `DeviceId` from a hash.
pub type DeviceId = Id<Device>;

/// Whether a device presents Ethernet-framed (TAP) or raw IP (TUN) packets.
///
/// TUN devices carry raw IP packets with no L2 framing; TAP devices carry
/// full Ethernet frames. This distinction is fixed at creation time and
/// never changes for a given device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DeviceKind {
    /// A TUN device: raw IP packets, no L2 framing.
    Tun,
    /// A TAP device: full Ethernet frames.
    Tap,
}

/// The administrative state of a device, as observed from the backend.
///
/// Distinct from a desired [`DesiredAdminState`] the same way
/// `net-lattice-model::interface::AdminState` is distinct from its own
/// `DesiredAdminState` — observed `Unknown` must never be requested back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AdminState {
    /// The device is administratively enabled (able to pass packets).
    Up,
    /// The device is administratively disabled.
    Down,
    /// The backend does not expose a separate administrative state for this
    /// device (observed only; never requested).
    Unknown,
}

/// The administrative state requested for a [`DeviceConfigPatch`].
///
/// Has no `Unknown` variant: a caller can request `Up` or `Down` only,
/// never the observed-only `AdminState::Unknown`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DesiredAdminState {
    /// Request the device be administratively enabled.
    Up,
    /// Request the device be administratively disabled.
    Down,
}

/// Desired intent for creating a new TUN/TAP device.
///
/// Distinct from the observed [`Device`]: a caller has no device identity
/// yet, only the shape of the device it wants opened. `name` is advisory —
/// Linux honors an exact requested name (or a kernel-assigned one if
/// `None`); Windows and some BSD/macOS configurations may only support a
/// kernel-assigned name and ignore the request. Backends report the actual
/// assigned name on the returned [`Device`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct DeviceConfig {
    /// Whether to open a TUN or TAP device.
    pub kind: DeviceKind,
    /// A requested device name, honored where the platform supports it.
    pub name: Option<String>,
    /// A requested MTU, applied at creation where the platform allows it.
    pub mtu: Option<u32>,
}

impl DeviceConfig {
    /// Creates a device-creation descriptor for `kind` with no name or MTU
    /// preference — the backend chooses both.
    pub const fn new(kind: DeviceKind) -> Self {
        Self {
            kind,
            name: None,
            mtu: None,
        }
    }

    /// Requests `name` for the new device (advisory; see the type's docs).
    #[must_use]
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Requests `mtu` for the new device.
    #[must_use]
    pub const fn with_mtu(mut self, mtu: u32) -> Self {
        self.mtu = Some(mtu);
        self
    }
}

/// A patch requesting a change to an already-open [`Device`]'s MTU or
/// administrative state.
///
/// Mirrors `net-lattice-model::interface::InterfaceConfig`'s "don't touch"
/// semantics: a field left `None` is left exactly as it is, and the
/// constructor rejects a patch that requests nothing.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct DeviceConfigPatch {
    device_id: DeviceId,
    admin_state: Option<DesiredAdminState>,
    mtu: Option<u32>,
}

impl DeviceConfigPatch {
    /// Creates a patch for `device_id`.
    ///
    /// Returns [`Error::InvalidState`] when no setting is requested or when
    /// `mtu` is zero, mirroring
    /// `net-lattice-model::interface::InterfaceConfig::new`'s precondition.
    pub fn new(
        device_id: DeviceId,
        admin_state: Option<DesiredAdminState>,
        mtu: Option<u32>,
    ) -> Result<Self> {
        if (admin_state.is_none() && mtu.is_none()) || mtu == Some(0) {
            return Err(Error::InvalidState);
        }

        Ok(Self {
            device_id,
            admin_state,
            mtu,
        })
    }

    /// Returns the device targeted by this patch.
    pub const fn device_id(&self) -> DeviceId {
        self.device_id
    }

    /// Returns the requested administrative state, if any.
    pub const fn admin_state(&self) -> Option<DesiredAdminState> {
        self.admin_state
    }

    /// Returns the requested MTU, if any.
    pub const fn mtu(&self) -> Option<u32> {
        self.mtu
    }
}

/// An observed, already-open TUN/TAP device.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct Device {
    /// This device's identity.
    pub id: DeviceId,
    /// The device's actual name, as assigned by the backend.
    pub name: String,
    /// Whether this is a TUN or TAP device.
    pub kind: DeviceKind,
    /// The device's current MTU.
    pub mtu: u32,
    /// The device's current administrative state.
    pub admin_state: AdminState,
}

impl Device {
    /// Constructs an observed device record.
    ///
    /// The only constructor available outside this crate: `Device` is
    /// `#[non_exhaustive]`, so a backend crate cannot use a struct literal
    /// directly (see ARCHITECTURE.md's convention on frozen public
    /// structs).
    pub const fn new(
        id: DeviceId,
        name: String,
        kind: DeviceKind,
        mtu: u32,
        admin_state: AdminState,
    ) -> Self {
        Self {
            id,
            name,
            kind,
            mtu,
            admin_state,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_config_builders_set_only_the_requested_fields() {
        let config = DeviceConfig::new(DeviceKind::Tun)
            .with_name("tun0")
            .with_mtu(1500);
        assert_eq!(config.kind, DeviceKind::Tun);
        assert_eq!(config.name.as_deref(), Some("tun0"));
        assert_eq!(config.mtu, Some(1500));
    }

    #[test]
    fn device_config_defaults_leave_name_and_mtu_unset() {
        let config = DeviceConfig::new(DeviceKind::Tap);
        assert_eq!(config.name, None);
        assert_eq!(config.mtu, None);
    }

    #[test]
    fn patch_requires_at_least_one_setting() {
        let device_id = DeviceId::new(1);
        assert!(DeviceConfigPatch::new(device_id, None, None).is_err());
    }

    #[test]
    fn patch_rejects_zero_mtu() {
        let device_id = DeviceId::new(1);
        assert!(DeviceConfigPatch::new(device_id, None, Some(0)).is_err());
    }

    #[test]
    fn patch_accepts_admin_state_only() {
        let device_id = DeviceId::new(1);
        let patch = DeviceConfigPatch::new(device_id, Some(DesiredAdminState::Up), None).unwrap();
        assert_eq!(patch.device_id(), device_id);
        assert_eq!(patch.admin_state(), Some(DesiredAdminState::Up));
        assert_eq!(patch.mtu(), None);
    }

    #[test]
    fn patch_accepts_mtu_only() {
        let device_id = DeviceId::new(1);
        let patch = DeviceConfigPatch::new(device_id, None, Some(1400)).unwrap();
        assert_eq!(patch.admin_state(), None);
        assert_eq!(patch.mtu(), Some(1400));
    }
}
