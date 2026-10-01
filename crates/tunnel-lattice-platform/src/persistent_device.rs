use tunnel_lattice_core::Result;

/// Controls whether an open device survives the last handle to it closing,
/// so a later process requesting the same name can attach to it instead of
/// creating a new one.
///
/// Distinct from [`crate::DeviceMutator`] the same way `PacketIo` is
/// distinct from device configuration: this changes the device's lifetime
/// relative to the processes holding it, not its MTU or administrative
/// state. Gated by `Capability::PERSISTENT_DEVICES`.
///
/// # Lifetime
///
/// A non-persistent device is destroyed when its last handle (in any
/// process) closes. [`Self::persist`] keeps it after that, with no handle
/// open; [`Self::unpersist`] restores the default, so the device is
/// destroyed once its last handle closes again. Both change one flag on the
/// device itself, not on this handle, so either call works from any handle
/// (any queue) attached to the device, including one obtained by
/// re-attaching in another process.
///
/// # Re-attaching
///
/// There is no separate attach call: open the device again with the same
/// name, kind, and multi-queue setting. Whether that attaches or creates,
/// and which mismatches are refused, is backend-specific; with the `tun-rs`
/// backend on Linux, see `DeviceConfig::name` and `DeviceConfig::multi_queue`
/// in `tunnel-lattice-model`. An `open` result does not say whether it
/// attached to an existing device or created a new one.
///
/// # Privilege
///
/// On Linux the kernel performs no capability check on the persistence
/// flag itself: holding a handle attached to the device is enough. Getting
/// that handle normally required `CAP_NET_ADMIN` or ownership of the
/// device.
pub trait PersistentDevice {
    /// Marks this device persistent: it survives its last handle closing,
    /// including this process exiting. Idempotent.
    fn persist(&self) -> Result<()>;

    /// Clears persistence: the device is destroyed when its last handle (in
    /// any process) closes. Idempotent: clearing an already non-persistent
    /// device succeeds and changes nothing.
    ///
    /// Does not close any handle itself, so the device stays usable through
    /// this and every other attached handle until they close.
    fn unpersist(&self) -> Result<()>;
}
