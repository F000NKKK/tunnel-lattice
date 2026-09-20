use tunnel_lattice_core::Result;

use crate::DeviceObserver;

/// Changes an open device's MTU or administrative state after creation.
///
/// Distinct from [`crate::DeviceProvider::open`] the same way
/// `net-lattice-platform::InterfaceMutator` is distinct from interface
/// creation: this changes an existing device in place rather than
/// producing a new one. Gated by `Capability::DEVICE_MUTATION` — a backend
/// that can only set these at open time (no later reconfiguration) does not
/// implement this trait at all, rather than implementing it and always
/// returning [`tunnel_lattice_core::Error::Unsupported`].
pub trait DeviceMutator: DeviceObserver {
    /// The backend's post-open configuration patch.
    type DeviceConfigPatch;

    /// Applies `patch` to this device.
    fn apply(&self, patch: Self::DeviceConfigPatch) -> Result<()>;
}
