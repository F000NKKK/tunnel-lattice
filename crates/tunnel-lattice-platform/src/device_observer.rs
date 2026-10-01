use tunnel_lattice_core::{Id, Result};

/// Reads back an open device's current observed state.
///
/// Separate from [`crate::PacketIo`] (packet transfer) and
/// [`crate::DeviceMutator`] (changing the device) the same way
/// `net-lattice-platform` keeps read and write concerns as distinct traits
/// per domain — a backend that can transfer packets but cannot re-query
/// name/MTU/administrative state after open implements `PacketIo` alone.
pub trait DeviceObserver {
    /// The backend's observed device record.
    type Device;

    /// Returns this device's current observed state.
    ///
    /// The record's identifier must equal [`DeviceObserver::id`].
    fn snapshot(&self) -> Result<Self::Device>;

    /// Returns this handle's device identity, captured when the device was
    /// opened and fixed for the handle's lifetime.
    ///
    /// Must not make a native call. The identity is not guaranteed to equal
    /// the interface's current OS index, which some platforms change after
    /// open (for example Windows after an adapter is disabled and
    /// re-enabled); resolve an interface by its name for interop with other
    /// crates.
    fn id(&self) -> Id<Self::Device>;
}
