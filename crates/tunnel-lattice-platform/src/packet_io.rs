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
    /// On `Ok(n)` the implementation must have written `buf[..n]`, and
    /// `n <= buf.len()`. Callers may reuse `buf` across calls without
    /// clearing it, so bytes past what was actually written must never be
    /// reported as part of the packet.
    ///
    /// A packet is never truncated silently. If the next packet does not
    /// fit in `buf`, the implementation discards it and returns
    /// [`tunnel_lattice_core::Error::BufferTooSmall`]; the device stays
    /// usable, and the next `recv` with a large enough buffer receives the
    /// following packet. `tunnel_lattice_model::Device::recv_buffer_len`
    /// gives a buffer size that fits at the device's current MTU.
    fn recv(&self, buf: &mut [u8]) -> Result<usize>;

    /// Writes one packet from `buf`.
    fn send(&self, buf: &[u8]) -> Result<usize>;
}
