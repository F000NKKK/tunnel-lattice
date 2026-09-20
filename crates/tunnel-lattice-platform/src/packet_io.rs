use tunnel_lattice_core::Result;

/// Synchronous packet transfer on an open TUN/TAP device.
///
/// Implemented by a backend's `Device` handle (the type returned by
/// [`crate::DeviceProvider::open`]), not by the backend connection object
/// itself — each open device is its own independent I/O channel.
///
/// A TUN device's buffers carry a raw IP packet with no L2 framing; a TAP
/// device's buffers carry a full Ethernet frame. This trait does not
/// distinguish the two at the type level — check
/// `tunnel_lattice_model::DeviceKind` on the `Device` this handle was opened
/// with to interpret buffer contents correctly.
pub trait PacketIo {
    /// Reads one packet into `buf`, returning the number of bytes written.
    ///
    /// Returns [`tunnel_lattice_core::Error::InvalidState`] if `buf` is too
    /// small for the next queued packet on a backend that cannot truncate
    /// it, rather than silently returning a truncated packet.
    fn recv(&self, buf: &mut [u8]) -> Result<usize>;

    /// Writes one packet from `buf`.
    fn send(&self, buf: &[u8]) -> Result<usize>;
}
