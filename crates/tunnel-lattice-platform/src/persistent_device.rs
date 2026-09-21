use tunnel_lattice_core::Result;

/// Marks an open device to survive process exit, so a later process
/// requesting the same name can attach to it instead of creating a new one.
///
/// Distinct from [`crate::DeviceMutator`] the same way `PacketIo` is
/// distinct from device configuration: this changes the device's lifetime
/// relative to this process, not its MTU or administrative state. There is
/// no corresponding "un-persist" method — clearing persistence (where the
/// backend supports it at all) is an out-of-band operation on the
/// interface, not something this crate models, since Linux's own
/// `TUNSETPERSIST` ioctl (the only backend that implements this trait
/// today) only ever sets the flag, never clears it, through the handle that
/// set it. Gated by `Capability::PERSISTENT_DEVICES`.
pub trait PersistentDevice {
    /// Marks this device persistent.
    fn persist(&self) -> Result<()>;
}
