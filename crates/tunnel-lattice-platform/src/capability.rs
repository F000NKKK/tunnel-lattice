bitflags::bitflags! {
    /// Runtime-dependent backend features that cannot be expressed through
    /// Rust trait implementation alone.
    ///
    /// Mirrors `net-lattice-platform::Capability`'s rationale: a backend
    /// either implements [`crate::DeviceMutator`] or it doesn't (fixed at
    /// compile time), but whether the *running* kernel/driver actually
    /// supports, say, persistent devices is a fact about the current
    /// machine, not the crate.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Capability: u64 {
        /// The backend can change an open device's MTU or administrative
        /// state after creation, through [`crate::DeviceMutator`].
        const DEVICE_MUTATION = 1 << 0;
        /// The backend implements [`crate::PersistentDevice`]: it can mark a
        /// device to survive its last handle closing and clear that again,
        /// and opening the same name, kind, and multi-queue setting
        /// attaches to such a device rather than creating a new one (on
        /// Linux this is the kernel's own `TUNSETIFF`-by-name behavior; the
        /// persistence flag is the kernel's `TUNSETPERSIST`, which sets or
        /// clears it).
        const PERSISTENT_DEVICES = 1 << 1;
        /// The backend can open a TAP (Ethernet-framed) device, not only TUN.
        /// Some platforms/drivers support TUN only.
        ///
        /// Where TAP depends on a separately installed driver, a backend
        /// reports this only if it detected that driver. The flag is
        /// advisory: opening a TAP device remains the authoritative check.
        const TAP_DEVICES = 1 << 2;
        /// The backend implements [`crate::MultiQueueProvider`]: it can
        /// duplicate a hardware-scheduled queue on the same device for
        /// multi-threaded packet I/O (Linux `IFF_MULTI_QUEUE`).
        const MULTI_QUEUE = 1 << 3;
        /// The backend implements [`crate::AsyncPacketIo`] natively, so
        /// `tunnel-lattice`'s async surface does not need to fall back to
        /// `tunnel-lattice-async`'s thread-based adapter over
        /// [`crate::PacketIo`]. Only meaningful with the `async` feature.
        const NATIVE_ASYNC = 1 << 4;
        /// The device's MAC address can be changed after creation, through
        /// a [`crate::DeviceMutator`] patch. Reported on a TAP device's
        /// handle only, and only where the platform allows it; where it is
        /// absent, a MAC can still be requested when the device is opened.
        const MAC_MUTATION = 1 << 5;
        /// The handle uses the kernel's segmentation offload internally:
        /// [`crate::PacketIo::recv`] (and the async `recv`) transparently
        /// splits each offloaded super-packet the kernel delivers into
        /// single IP packets, and [`crate::PacketIo::send_batch`] (and the
        /// async `send_batch`) may coalesce adjacent
        /// packets of the same flow into one native write.
        ///
        /// Packet framing is unchanged: every receive still returns exactly
        /// one packet and every send still takes one packet per buffer, so
        /// a caller never sees an offload header or an aggregate. Reported
        /// on an open device's handle only, never as a host capability, and
        /// only once the backend has verified that the device actually uses
        /// offload framing. Requested with
        /// `tunnel_lattice_model::DeviceConfig::with_offload`; the request
        /// is a hint, and this flag is the answer.
        const SEGMENTATION_OFFLOAD = 1 << 6;
    }
}

/// Reports which runtime-dependent [`Capability`] flags the connected
/// device currently has available.
///
/// Every backend implements this — an empty flag set is a valid answer —
/// which is why `capabilities` returns a bare `Capability` rather than
/// `Result<Capability>`, mirroring
/// `net-lattice-platform::CapabilityProvider`'s rationale.
pub trait CapabilityProvider {
    /// Returns the runtime-dependent capabilities this device currently has
    /// available.
    fn capabilities(&self) -> Capability;
}

#[cfg(test)]
mod tests {
    use super::Capability;

    #[test]
    fn capability_flags_are_distinct_bits() {
        assert_ne!(
            Capability::DEVICE_MUTATION.bits(),
            Capability::PERSISTENT_DEVICES.bits()
        );
        assert!(!Capability::TAP_DEVICES.contains(Capability::MULTI_QUEUE));
    }

    #[test]
    fn segmentation_offload_is_bit_six_and_overlaps_no_other_flag() {
        assert_eq!(Capability::SEGMENTATION_OFFLOAD.bits(), 1 << 6);
        let others = Capability::all().difference(Capability::SEGMENTATION_OFFLOAD);
        assert!(!others.intersects(Capability::SEGMENTATION_OFFLOAD));
        assert_eq!(others.bits(), (1 << 6) - 1, "bits 0-5 are unchanged");
    }
}
