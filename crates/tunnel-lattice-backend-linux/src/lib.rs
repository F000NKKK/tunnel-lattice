//! Native Linux TUN/TAP backend for Tunnel Lattice.
//!
//! Built directly on `/dev/net/tun` ioctls and a synchronous rtnetlink
//! socket, with no `tun-rs` and no async runtime on the control path. It is
//! meant to implement the same `tunnel-lattice-platform` provider contracts
//! as `tunnel-lattice-backend-tunrs`, selectable alongside it.
//!
//! **Status: in development.** This release contains the internal
//! foundations only (the errno mapping and the rtnetlink control plane). It
//! exports no public API yet and cannot open a device; use
//! `tunnel-lattice-backend-tunrs` (the `tunnel-lattice` facade's default)
//! until the device API lands.
//!
//! The whole crate is Linux-only: on every other target it compiles to an
//! empty crate with no dependencies beyond the workspace crates, so a
//! cross-platform workspace can depend on it unconditionally.

#![cfg(target_os = "linux")]
#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]
// The foundations below are wired into the device core by the next
// development step; until then only their unit tests reach them.
#![cfg_attr(
    not(test),
    expect(dead_code, reason = "used once the device core lands")
)]

mod control;
mod errno;

use std::os::fd::OwnedFd;
use std::sync::Mutex;

use control::RouteSocket;

/// The native Linux backend (skeleton; not exported yet).
///
/// Will implement `DeviceProvider` and the host-level `CapabilityProvider`.
#[derive(Debug, Default, Clone, Copy)]
struct LinuxBackend;

impl LinuxBackend {
    /// Creates the backend. It holds no state and opens nothing.
    const fn new() -> Self {
        Self
    }
}

/// One open queue of a native TUN/TAP device (skeleton; not exported yet).
#[derive(Debug)]
struct LinuxDevice {
    /// The queue's `/dev/net/tun` file descriptor, `O_NONBLOCK` from
    /// creation and never toggled.
    queue: OwnedFd,
    /// The interface index captured at open (the device id).
    index: u32,
    /// The control socket, shared by the device's control calls; never used
    /// on the packet path.
    control: Mutex<RouteSocket>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn skeleton_types_are_send_and_sync() {
        assert_send_sync::<LinuxBackend>();
        assert_send_sync::<LinuxDevice>();
        let _ = LinuxBackend::new();
    }

    #[test]
    fn a_device_owns_its_queue_fd_and_control_socket() {
        use std::os::fd::AsRawFd;

        let device = LinuxDevice {
            queue: std::fs::File::open("/dev/null").unwrap().into(),
            index: 1,
            control: Mutex::new(RouteSocket::open().unwrap()),
        };
        assert!(device.queue.as_raw_fd() >= 0);
        assert_eq!(device.index, 1);
        assert!(device.control.lock().is_ok());
    }
}
