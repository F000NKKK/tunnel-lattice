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
    fn recv(&self, buf: &mut [u8]) -> impl Future<Output = Result<usize>> + Send;

    /// Writes one packet from `buf`.
    fn send(&self, buf: &[u8]) -> impl Future<Output = Result<usize>> + Send;
}
