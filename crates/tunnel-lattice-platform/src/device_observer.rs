use tunnel_lattice_core::Result;

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
    fn snapshot(&self) -> Result<Self::Device>;
}
