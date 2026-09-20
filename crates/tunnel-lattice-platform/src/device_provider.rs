use tunnel_lattice_core::Result;

/// Opens a TUN/TAP device.
///
/// Generic over associated `DeviceConfig`/`Device` types rather than naming
/// `tunnel_lattice_model` types directly — `tunnel-lattice-platform` does
/// not depend on `tunnel-lattice-model` (see the crate's top-level docs).
/// The facade crate (`tunnel_lattice`) is what constrains these to the
/// concrete model types.
///
/// Unlike `net-lattice-platform`'s read/write provider split (routes and
/// interfaces exist independently of the calling process, so listing them
/// is a separate, normally unprivileged operation from mutating them), a
/// TUN/TAP device exists only because this process created it: opening one
/// *is* the privileged operation, and the returned `Device` handle is both
/// the packet-I/O surface ([`crate::PacketIo`]) and, where the backend
/// implements it, the target of [`crate::DeviceMutator`]/
/// [`crate::DeviceObserver`]. There is no separate always-unprivileged
/// "list every device" call in this contract; a backend that can enumerate
/// pre-existing persistent devices exposes that as its own inherent API,
/// gated by `Capability::PERSISTENT_DEVICES`.
pub trait DeviceProvider {
    /// The backend's device-creation descriptor.
    type DeviceConfig;
    /// The backend's open-device handle.
    type Device;

    /// Opens a new device matching `config`, or attaches to a persistent one
    /// requested by name where the backend and `Capability::PERSISTENT_DEVICES`
    /// support it.
    fn open(&self, config: Self::DeviceConfig) -> Result<Self::Device>;
}
