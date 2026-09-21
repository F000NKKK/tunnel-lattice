use tunnel_lattice_core::Result;

use crate::PacketIo;

/// Duplicates an open device's queue for use from another thread, when the
/// backend and the running kernel support hardware-scheduled multiple
/// queues on one device (Linux `IFF_MULTI_QUEUE`).
///
/// Distinct from ordinary multi-threaded use of [`PacketIo`], which is
/// already safe on every backend — `recv`/`send` take `&self`, so sharing
/// one [`crate::DeviceProvider::open`]ed device across threads via `Arc`
/// always works. A queue clone instead gets its own kernel-scheduled queue,
/// letting the OS distribute packets across CPUs (typically via RSS-style
/// hashing) instead of every thread contending on one shared queue —
/// `additional_queue` is a throughput optimization on top of that baseline,
/// not a prerequisite for correctness. Gated by `Capability::MULTI_QUEUE`,
/// and further requires the device to have been opened with multi-queue
/// requested (e.g. `tunnel_lattice_model::DeviceConfig::with_multi_queue`);
/// calling this on a device that wasn't returns
/// [`tunnel_lattice_core::Error::Unsupported`].
pub trait MultiQueueProvider: PacketIo {
    /// Duplicates this device's queue, returning an independent handle
    /// usable from another thread.
    fn additional_queue(&self) -> Result<Self>
    where
        Self: Sized;
}
