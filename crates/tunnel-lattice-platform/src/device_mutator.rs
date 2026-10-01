use tunnel_lattice_core::Result;

use crate::DeviceObserver;

/// Changes an open device's MTU, MAC address, or administrative state after
/// creation.
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
    ///
    /// Every implementation follows the same contract:
    ///
    /// 1. **Preconditions, before any native call.** A patch for another
    ///    device (its identifier differs from [`DeviceObserver::id`]) or a
    ///    value outside what the platform can represent (for example an MTU
    ///    above `u16::MAX`, or a MAC address for a TUN device, which has
    ///    none) returns
    ///    [`Error::InvalidState`](tunnel_lattice_core::Error::InvalidState).
    ///    After those checks, a requested setting this backend cannot apply
    ///    (for example a MAC address on a handle without
    ///    [`Capability::MAC_MUTATION`](crate::Capability::MAC_MUTATION))
    ///    returns [`Error::Unsupported`](tunnel_lattice_core::Error::Unsupported).
    ///    Either way nothing has changed.
    /// 2. **Order.** The MTU is applied first, then the MAC address, then
    ///    the administrative state. The administrative state goes last
    ///    because some platforms cannot read it back, so it cannot be
    ///    reverted.
    /// 3. **Compensation.** If a step fails, the steps already applied are
    ///    reverted in reverse order, on a best-effort basis, and the failed
    ///    step's own error is returned. A failed revert is not reported. To
    ///    revert, a step's previous value is read before any change, and
    ///    only when a later step exists; if that read fails, `apply`
    ///    returns its error and changes nothing.
    ///
    /// After any `Err`, [`DeviceObserver::snapshot`] is authoritative for
    /// what the device's settings now are.
    fn apply(&self, patch: Self::DeviceConfigPatch) -> Result<()>;
}
