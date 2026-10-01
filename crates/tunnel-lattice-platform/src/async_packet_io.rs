use std::future::Future;

use tunnel_lattice_core::Result;

/// Native async packet transfer on an open TUN/TAP device.
///
/// Implement this on a `Device` handle when the backend has a genuine
/// non-blocking I/O path (e.g. an async-registered file descriptor on
/// Linux/macOS, or an overlapped-I/O handle on Windows) instead of a
/// blocking syscall. When a backend implements this, it should also report
/// `Capability::NATIVE_ASYNC` so `tunnel-lattice`'s facade prefers this path
/// over `tunnel-lattice-async`'s thread-based adapter, which spawns one
/// blocking worker thread per device to bridge a [`crate::PacketIo`]
/// implementation onto a `futures::Stream`/`Sink` — correct for any backend,
/// but strictly worse for one that already has a real async path.
///
/// Available with the `async` feature.
pub trait AsyncPacketIo {
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
    fn recv(&self, buf: &mut [u8]) -> impl Future<Output = Result<usize>> + Send;

    /// Writes one packet from `buf`.
    ///
    /// # Cancellation
    ///
    /// Dropping the returned future before it completes must be safe and
    /// must leave either the whole packet sent or nothing: never part of a
    /// packet, and never the packet twice. The implementation must not use
    /// `buf` after the future is dropped (copy it first if a write can
    /// outlive the future). Whether a dropped send reached the device may
    /// be unknown to the caller, so the portable contract is "whole packet
    /// at most once; unknown after drop".
    fn send(&self, buf: &[u8]) -> impl Future<Output = Result<usize>> + Send;
}
