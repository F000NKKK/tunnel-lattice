//! Native Linux TUN/TAP backend for Tunnel Lattice.
//!
//! Built directly on `/dev/net/tun` ioctls and a synchronous rtnetlink
//! socket, with no `tun-rs` and no async runtime on the control path. It is
//! meant to implement the same `tunnel-lattice-platform` provider contracts
//! as `tunnel-lattice-backend-tunrs`, selectable alongside it.
//!
//! **Status: in development.** This release contains the synchronous
//! device core (opening TUN and TAP devices, packet I/O, observing and
//! changing a device, persistence, and multi-queue) but does not export it
//! yet: the crate has no public API, and no `tunnel-lattice` facade feature
//! selects it. Use `tunnel-lattice-backend-tunrs` (the facade's default)
//! until the backend is exported.
//!
//! The whole crate is Linux-only: on every other target it compiles to an
//! empty crate with no dependencies beyond the workspace crates, so a
//! cross-platform workspace can depend on it unconditionally.

#![cfg(target_os = "linux")]
#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]

mod apply;
mod control;
mod errno;
#[cfg(test)]
mod golden_tests;
mod io;
mod tun;

use std::ffi::c_int;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tunnel_lattice_core::{Error, Result};
use tunnel_lattice_model::{
    AdminState, Device, DeviceConfig, DeviceConfigPatch, DeviceId, DeviceKind, MacAddress,
};
use tunnel_lattice_platform::{
    Capability, CapabilityProvider, DeviceMutator, DeviceObserver, DeviceProvider,
    MultiQueueProvider, PacketIo, PersistentDevice,
};

use control::{LinkChange, LinkQuery, RouteSocket};
use errno::{Class, Op};

/// The native Linux backend (not exported yet).
///
/// Stateless: every device opened through it is independent.
///
/// # Opening a device
///
/// 1. **Prechecks, before any native call.** A name that is empty, longer
///    than 15 bytes, or contains NUL or `%`, an MTU above `u16::MAX`, or a
///    MAC address for a TUN device is `Error::InvalidState`; an unknown
///    `DeviceKind` is `Error::Unsupported`.
/// 2. `/dev/net/tun` is opened non-blocking and close-on-exec. A missing
///    node or `tun` module is `Error::DriverUnavailable`; the device node is
///    never created.
/// 3. `TUNSETIFF` creates the interface, or attaches to an existing one of
///    that name (a persistent device with no queue attached, or any
///    multi-queue device of the same kind when multi-queue is requested),
///    always with `IFF_NO_PI`. `EBUSY` (a single-queue device that already
///    has a queue) is `Error::AlreadyExists`; `EINVAL` is
///    `Error::AlreadyExists` when an interface of that name exists (the
///    other kind, or a multi-queue mismatch) and `Platform(Linux(EINVAL))`
///    otherwise.
/// 4. The queue's name and flags are read back with `TUNGETIFF`. A device
///    that uses virtio-net header framing (possible when joining a
///    multi-queue device another process set up) is `Error::Unsupported`.
///    Any offload mask is cleared, since a queue without the header cannot
///    describe offloaded packets.
/// 5. One `RTM_GETLINK` by the read-back name captures the interface index,
///    which becomes the device's identity.
/// 6. The requested MTU, then the requested MAC, are set with one
///    `RTM_NEWLINK` each; the MAC is read back, and a device that did not
///    take it is `Error::Unsupported`.
///
/// Any failure after step 2 closes the descriptor before `open` returns, so
/// a device created by this call disappears again; an attached persistent
/// device is only detached. Nothing else is compensated.
///
/// `DeviceConfig::offload` is accepted and ignored: every queue uses plain
/// framing, and no handle reports `Capability::SEGMENTATION_OFFLOAD`.
#[derive(Debug, Default, Clone, Copy)]
struct LinuxBackend;

impl LinuxBackend {
    /// Creates the backend. It holds no state and opens nothing.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "exported by the facade integration step")
    )]
    const fn new() -> Self {
        Self
    }
}

/// One open queue of a native TUN/TAP device (not exported yet).
///
/// # Packet I/O
///
/// The queue descriptor is non-blocking from creation and never changed:
/// `recv` reads with `readv` into the caller's buffer plus a one-byte
/// sentinel, and waits with `poll` only when nothing is queued; `send`
/// writes first and waits only when the queue is full. A packet that does
/// not fit `recv`'s buffer is discarded and reported as
/// `Error::BufferTooSmall`. Neither call takes a lock or needs a runtime,
/// so both are legal from any thread.
///
/// | Native signal | Result |
/// |---|---|
/// | `EINTR` | retried |
/// | `EAGAIN` | waits for readiness, then retries |
/// | `EBADFD`, `ENXIO` (the device was deleted) | `Error::Disconnected` |
/// | `EFAULT` on receive | `Error::Disconnected` |
/// | `EIO` on send (the device is administratively down) | `Error::InvalidState` |
/// | anything else | `Platform(Linux(errno))` |
///
/// A receive on a device that is administratively down keeps waiting until
/// the device is up and a packet arrives. Once the device is deleted, a
/// waiting receive wakes and returns `Error::Disconnected`.
///
/// # Control calls
///
/// `snapshot` and `apply` go through the device's rtnetlink socket, shared
/// by all its queues and opened in the network namespace the device was
/// opened in. Once the device has been deleted or moved to another
/// namespace, they return `Error::NotFound`; packet I/O keeps using the
/// queue descriptor and is unaffected by a move.
#[derive(Debug)]
struct LinuxDevice {
    /// The queue's `/dev/net/tun` descriptor, `O_NONBLOCK` from creation and
    /// never toggled.
    queue: OwnedFd,
    /// TUN or TAP.
    kind: DeviceKind,
    /// The interface index captured at open (the device id).
    index: u32,
    /// The queue's flags as read back with `TUNGETIFF` at open.
    flags: c_int,
    /// The control socket, shared by every queue of the device; never used
    /// on the packet path.
    control: Arc<Mutex<RouteSocket>>,
}

impl DeviceProvider for LinuxBackend {
    type DeviceConfig = DeviceConfig;
    type Device = LinuxDevice;

    /// See [`LinuxBackend`], "Opening a device".
    fn open(&self, config: DeviceConfig) -> Result<LinuxDevice> {
        let flags = tun::request_flags(config.kind, config.multi_queue)?;
        if let Some(name) = config.name.as_deref() {
            tun::precheck_name(name)?;
        }
        if config.mtu.is_some_and(|mtu| mtu > u32::from(u16::MAX)) {
            return Err(Error::InvalidState);
        }
        if config.mac.is_some() && config.kind != DeviceKind::Tap {
            return Err(Error::InvalidState);
        }

        let queue = tun::open_node()?;
        let mut control = RouteSocket::open()?;
        // From here on, dropping `queue` on an error closes it: a device
        // created by this call disappears, an attached one is detached.
        let read_back = attach(&queue, config.name.as_deref(), flags, &mut control)?;
        let name = read_back.name()?;
        let flags = read_back.flags();
        tun::check_framing(flags)?;
        tun::queue_ioctl(|| tun::clear_offload(queue.as_fd()))?;

        let link = control.get_link(LinkQuery::Name(&name))?;
        if let Some(mtu) = config.mtu {
            control.set_link(link.index, LinkChange::Mtu(mtu))?;
        }
        if let Some(mac) = config.mac {
            control.set_link(link.index, LinkChange::Mac(mac.octets()))?;
            let actual = control.get_link(LinkQuery::Index(link.index))?.mac;
            if actual != Some(mac.octets()) {
                return Err(Error::Unsupported);
            }
        }

        Ok(LinuxDevice {
            queue,
            kind: config.kind,
            index: link.index,
            flags,
            control: Arc::new(Mutex::new(control)),
        })
    }
}

/// Issues `TUNSETIFF` on `queue` and returns what `TUNGETIFF` reads back.
/// `EINVAL` is resolved by looking the requested name up (see
/// [`errno::set_iff_einval`]).
fn attach(
    queue: &OwnedFd,
    name: Option<&str>,
    flags: c_int,
    control: &mut RouteSocket,
) -> Result<tun::IfReq> {
    let mut request = tun::IfReq::new(name, flags);
    while let Err(code) = tun::set_iff(queue.as_fd(), &mut request) {
        match errno::classify(code, Op::SetIff) {
            Class::Retry => {}
            Class::Fail(error) => return Err(error),
            Class::CheckNameConflict => {
                let exists = match name {
                    Some(name) => control.get_link(LinkQuery::Name(name)).is_ok(),
                    None => false,
                };
                return Err(errno::set_iff_einval(exists));
            }
            _ => return Err(errno::platform(code)),
        }
    }
    tun::queue_ioctl(|| tun::get_iff(queue.as_fd()))
}

impl LinuxDevice {
    /// The device's control socket. A panic while another queue held it
    /// leaves nothing half-done that matters here: a later request skips
    /// any stale reply by its sequence number.
    fn control(&self) -> MutexGuard<'_, RouteSocket> {
        self.control.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// See [`LinuxDevice`], "Packet I/O".
impl PacketIo for LinuxDevice {
    fn recv(&self, buf: &mut [u8]) -> Result<usize> {
        io::recv(self.queue.as_fd(), buf)
    }

    fn send(&self, buf: &[u8]) -> Result<usize> {
        io::send(self.queue.as_fd(), buf)
    }
}

impl DeviceObserver for LinuxDevice {
    type Device = Device;

    /// The interface index captured at open. It may no longer name this
    /// device after the device moved to another network namespace.
    fn id(&self) -> DeviceId {
        DeviceId::new(u64::from(self.index))
    }

    /// One `RTM_GETLINK` by the index captured at open. The administrative
    /// state is `AdminState::Up` iff the interface flags carry both
    /// `IFF_UP` and `IFF_RUNNING`; a TAP device's MAC address is included.
    /// A deleted or moved device is `Error::NotFound`.
    fn snapshot(&self) -> Result<Device> {
        let link = self.control().get_link(LinkQuery::Index(self.index))?;
        let device = Device::new(
            self.id(),
            link.name,
            self.kind,
            link.mtu,
            admin_state(link.flags),
        );
        Ok(match (self.kind, link.mac) {
            (DeviceKind::Tap, Some(mac)) => device.with_mac(MacAddress::new(mac)),
            _ => device,
        })
    }
}

/// `AdminState::Up` iff `flags` carry both `IFF_UP` and `IFF_RUNNING`.
fn admin_state(flags: u32) -> AdminState {
    let up = (libc::IFF_UP | libc::IFF_RUNNING) as u32;
    if flags & up == up {
        AdminState::Up
    } else {
        AdminState::Down
    }
}

impl DeviceMutator for LinuxDevice {
    type DeviceConfigPatch = DeviceConfigPatch;

    /// Follows the trait's contract, with one `RTM_NEWLINK` per step by the
    /// index captured at open. The administrative state is changed through
    /// the `IFF_UP` change mask, touching no other flag.
    fn apply(&self, patch: DeviceConfigPatch) -> Result<()> {
        let target = apply::Target {
            id: self.id(),
            kind: self.kind,
            mac_mutation: self.kind == DeviceKind::Tap,
        };
        let mut control = self.control();
        let mut steps = apply::LinkSteps {
            socket: &mut control,
            index: self.index,
        };
        apply::apply(&mut steps, target, &patch)
    }
}

/// `TUNSETPERSIST`, with its argument passed by value. Both calls act on
/// the device, so they work from any of its queues, and need no privilege
/// beyond the open queue.
impl PersistentDevice for LinuxDevice {
    fn persist(&self) -> Result<()> {
        tun::queue_ioctl(|| tun::set_persist(self.queue.as_fd(), true))
    }

    fn unpersist(&self) -> Result<()> {
        tun::queue_ioctl(|| tun::set_persist(self.queue.as_fd(), false))
    }
}

impl MultiQueueProvider for LinuxDevice {
    /// Opens a new queue descriptor the way `open` does and attaches it to
    /// this device by its current name, with the flags read back at open.
    /// A device opened without multi-queue is `Error::Unsupported`. The
    /// new queue shares this device's control socket.
    fn additional_queue(&self) -> Result<Self> {
        if self.flags & libc::IFF_MULTI_QUEUE == 0 {
            return Err(Error::Unsupported);
        }
        let current = tun::queue_ioctl(|| tun::get_iff(self.queue.as_fd()))?;
        let name = current.name()?;
        let queue = tun::open_node()?;
        let read_back = attach(
            &queue,
            Some(&name),
            tun::queue_flags(self.flags),
            &mut self.control(),
        )?;
        let flags = read_back.flags();
        tun::check_framing(flags)?;
        Ok(Self {
            queue,
            kind: self.kind,
            index: self.index,
            flags,
            control: Arc::clone(&self.control),
        })
    }
}

/// What every host supports: `DEVICE_MUTATION`, `PERSISTENT_DEVICES`,
/// `TAP_DEVICES` and `MULTI_QUEUE`. Advisory: `open` stays the
/// authoritative check, and no flag proves the process is privileged.
const HOST_CAPABILITIES: Capability = Capability::DEVICE_MUTATION
    .union(Capability::PERSISTENT_DEVICES)
    .union(Capability::TAP_DEVICES)
    .union(Capability::MULTI_QUEUE);

impl CapabilityProvider for LinuxBackend {
    fn capabilities(&self) -> Capability {
        HOST_CAPABILITIES
    }
}

/// The host capabilities, plus `MAC_MUTATION` on a TAP handle.
impl CapabilityProvider for LinuxDevice {
    fn capabilities(&self) -> Capability {
        match self.kind {
            DeviceKind::Tap => HOST_CAPABILITIES | Capability::MAC_MUTATION,
            _ => HOST_CAPABILITIES,
        }
    }
}

#[cfg(test)]
mod tests {
    use tunnel_lattice_model::DesiredAdminState;

    use super::*;

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn the_backend_and_its_devices_are_send_and_sync() {
        assert_send_sync::<LinuxBackend>();
        assert_send_sync::<LinuxDevice>();
    }

    #[test]
    fn the_host_reports_the_linux_flags_of_the_tun_rs_backend() {
        let host = LinuxBackend::new().capabilities();
        assert_eq!(
            host,
            Capability::DEVICE_MUTATION
                | Capability::PERSISTENT_DEVICES
                | Capability::TAP_DEVICES
                | Capability::MULTI_QUEUE
        );
        assert!(!host.intersects(
            Capability::MAC_MUTATION | Capability::SEGMENTATION_OFFLOAD | Capability::NATIVE_ASYNC
        ));
    }

    /// A device over `/dev/null`, for checks that make no native call on
    /// the queue.
    fn fake_device(kind: DeviceKind, flags: c_int) -> LinuxDevice {
        LinuxDevice {
            queue: std::fs::File::open("/dev/null").unwrap().into(),
            kind,
            index: 7,
            flags,
            control: Arc::new(Mutex::new(RouteSocket::open().unwrap())),
        }
    }

    #[test]
    fn a_handle_adds_mac_mutation_on_tap_only() {
        let tun = fake_device(DeviceKind::Tun, libc::IFF_TUN);
        let tap = fake_device(DeviceKind::Tap, libc::IFF_TAP);
        assert_eq!(tun.capabilities(), HOST_CAPABILITIES);
        assert_eq!(
            tap.capabilities(),
            HOST_CAPABILITIES | Capability::MAC_MUTATION
        );
        assert!(
            !tap.capabilities()
                .contains(Capability::SEGMENTATION_OFFLOAD)
        );
        assert_eq!(tun.id(), DeviceId::new(7));
    }

    #[test]
    fn the_admin_state_needs_both_up_and_running() {
        let up = libc::IFF_UP as u32;
        let running = libc::IFF_RUNNING as u32;
        assert_eq!(admin_state(up | running), AdminState::Up);
        assert_eq!(admin_state(up), AdminState::Down);
        assert_eq!(admin_state(running), AdminState::Down);
        assert_eq!(admin_state(0), AdminState::Down);
    }

    #[test]
    fn open_rejects_unusable_requests_before_any_native_call() {
        let backend = LinuxBackend::new();
        let invalid = [
            DeviceConfig::new(DeviceKind::Tun).with_name(""),
            DeviceConfig::new(DeviceKind::Tun).with_name("tun%d"),
            DeviceConfig::new(DeviceKind::Tun).with_name("a234567890abcdef"),
            DeviceConfig::new(DeviceKind::Tun).with_name("a\0b"),
            DeviceConfig::new(DeviceKind::Tun).with_mtu(65536),
            DeviceConfig::new(DeviceKind::Tun).with_mac(MacAddress::new([2, 0, 0, 0, 0, 1])),
        ];
        for config in invalid {
            let error = backend.open(config.clone()).unwrap_err();
            assert!(error.is_invalid_state(), "{config:?}: {error:?}");
        }
    }

    #[test]
    fn additional_queue_needs_a_multi_queue_device() {
        let device = fake_device(DeviceKind::Tun, libc::IFF_TUN | libc::IFF_NO_PI);
        assert!(device.additional_queue().is_err_and(|e| e.is_unsupported()));
    }

    #[test]
    fn apply_checks_the_patch_before_any_native_call() {
        let device = fake_device(DeviceKind::Tun, libc::IFF_TUN);
        let other = DeviceConfigPatch::new(DeviceId::new(8), Some(DesiredAdminState::Up), None);
        assert!(
            device
                .apply(other.unwrap())
                .is_err_and(|e| e.is_invalid_state())
        );
        let mac = DeviceConfigPatch::new_mac(device.id(), MacAddress::new([2, 0, 0, 0, 0, 1]));
        assert!(device.apply(mac).is_err_and(|e| e.is_invalid_state()));
    }

    /// Whether this process may create TUN/TAP devices: it holds
    /// `CAP_NET_ADMIN` (bit 12 of the effective set).
    fn has_net_admin() -> bool {
        let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
        status
            .lines()
            .find_map(|line| line.strip_prefix("CapEff:"))
            .and_then(|caps| u64::from_str_radix(caps.trim(), 16).ok())
            .is_some_and(|caps| caps & (1 << 12) != 0)
    }

    #[test]
    fn an_unprivileged_open_is_permission_denied() {
        if has_net_admin() {
            return;
        }
        let result = LinuxBackend::new().open(DeviceConfig::new(DeviceKind::Tun));
        let Err(error) = result else {
            panic!("created a device without CAP_NET_ADMIN");
        };
        // Without the clone device (a minimal container) the driver is
        // unavailable instead; both are documented mappings.
        assert!(
            error.is_permission_denied() || error.is_driver_unavailable(),
            "{error:?}"
        );
    }
}

/// Tests that create real devices. They need `CAP_NET_ADMIN` (run with
/// `sudo -E cargo test -p tunnel-lattice-backend-linux -- --ignored`), use
/// names no other test uses, and delete every device they create on every
/// exit path, including a failed assertion. Packet tests also need the `ip`
/// tool.
#[cfg(test)]
mod privileged_tests {
    use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
    use std::process::Command;
    use std::time::Duration;

    use tunnel_lattice_model::DesiredAdminState;

    use super::*;

    /// A per-process, per-test interface name (at most 15 bytes).
    fn unique_name(tag: &str) -> String {
        format!("tln{tag}{}", std::process::id() % 100_000)
    }

    /// Deletes the interface when dropped, so a persistent device or a
    /// failed test never leaves one behind.
    struct Cleanup(String);

    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = Command::new("ip")
                .args(["link", "delete", &self.0])
                .output();
        }
    }

    fn ip(args: &[&str]) {
        let status = Command::new("ip").args(args).status().unwrap();
        assert!(status.success(), "ip {args:?}");
    }

    fn exists(name: &str) -> bool {
        std::path::Path::new("/sys/class/net").join(name).exists()
    }

    fn tun_flags(name: &str) -> c_int {
        let path = format!("/sys/class/net/{name}/tun_flags");
        let text = std::fs::read_to_string(path).unwrap();
        c_int::from_str_radix(text.trim().trim_start_matches("0x"), 16).unwrap()
    }

    fn open(config: DeviceConfig) -> LinuxDevice {
        LinuxBackend::new().open(config).unwrap()
    }

    #[test]
    #[ignore = "needs CAP_NET_ADMIN"]
    fn open_reports_the_requested_name_kind_and_mtu() {
        for (kind, tag) in [(DeviceKind::Tun, "o"), (DeviceKind::Tap, "p")] {
            let name = unique_name(tag);
            let _cleanup = Cleanup(name.clone());
            let device = open(DeviceConfig::new(kind).with_name(&name).with_mtu(1400));
            let snapshot = device.snapshot().unwrap();
            assert_eq!(snapshot.id, device.id());
            assert_eq!(
                (snapshot.name.as_str(), snapshot.kind),
                (name.as_str(), kind)
            );
            assert_eq!(snapshot.mtu, 1400);
            assert_eq!(snapshot.admin_state, AdminState::Down);
            assert_eq!(snapshot.mac.is_some(), kind == DeviceKind::Tap);
            let flags = tun_flags(&name);
            assert_ne!(flags & libc::IFF_NO_PI, 0);
            assert_eq!(flags & libc::IFF_VNET_HDR, 0);
            drop(device);
            assert!(
                !exists(&name),
                "a non-persistent device outlived its handle"
            );
        }
    }

    #[test]
    #[ignore = "needs CAP_NET_ADMIN"]
    fn an_unnamed_device_gets_a_kernel_chosen_name() {
        let device = open(DeviceConfig::new(DeviceKind::Tun));
        let name = device.snapshot().unwrap().name;
        let _cleanup = Cleanup(name.clone());
        assert!(name.starts_with("tun"), "{name}");
    }

    #[test]
    #[ignore = "needs CAP_NET_ADMIN"]
    fn a_tap_mac_is_set_at_open_and_changed_by_apply() {
        let name = unique_name("m");
        let _cleanup = Cleanup(name.clone());
        let first = MacAddress::new([0x02, 0x11, 0x22, 0x33, 0x44, 0x55]);
        let second = MacAddress::new([0x02, 0x11, 0x22, 0x33, 0x44, 0x66]);
        let device = open(
            DeviceConfig::new(DeviceKind::Tap)
                .with_name(&name)
                .with_mac(first),
        );
        assert_eq!(device.snapshot().unwrap().mac, Some(first));
        device
            .apply(DeviceConfigPatch::new_mac(device.id(), second))
            .unwrap();
        assert_eq!(device.snapshot().unwrap().mac, Some(second));
    }

    #[test]
    #[ignore = "needs CAP_NET_ADMIN"]
    fn apply_changes_mtu_and_admin_state() {
        let name = unique_name("a");
        let _cleanup = Cleanup(name.clone());
        let device = open(DeviceConfig::new(DeviceKind::Tun).with_name(&name));
        let patch =
            DeviceConfigPatch::new(device.id(), Some(DesiredAdminState::Up), Some(1280)).unwrap();
        device.apply(patch).unwrap();
        let snapshot = device.snapshot().unwrap();
        assert_eq!((snapshot.mtu, snapshot.admin_state), (1280, AdminState::Up));
        let down = DeviceConfigPatch::new(device.id(), Some(DesiredAdminState::Down), None);
        device.apply(down.unwrap()).unwrap();
        assert_eq!(device.snapshot().unwrap().admin_state, AdminState::Down);

        // An MTU the kernel refuses fails the MTU step, and nothing is left
        // changed.
        let refused =
            DeviceConfigPatch::new(device.id(), Some(DesiredAdminState::Up), Some(1)).unwrap();
        assert!(device.apply(refused).is_err());
        let snapshot = device.snapshot().unwrap();
        assert_eq!(
            (snapshot.mtu, snapshot.admin_state),
            (1280, AdminState::Down)
        );
    }

    #[test]
    #[ignore = "needs CAP_NET_ADMIN"]
    fn opening_an_existing_name_as_the_other_kind_or_a_second_queue_is_already_exists() {
        let name = unique_name("x");
        let _cleanup = Cleanup(name.clone());
        let _device = open(DeviceConfig::new(DeviceKind::Tun).with_name(&name));
        let backend = LinuxBackend::new();
        let tap = backend.open(DeviceConfig::new(DeviceKind::Tap).with_name(&name));
        assert!(tap.is_err_and(|e| e.is_already_exists()));
        let again = backend.open(DeviceConfig::new(DeviceKind::Tun).with_name(&name));
        assert!(again.is_err_and(|e| e.is_already_exists()));
        let multi = backend.open(
            DeviceConfig::new(DeviceKind::Tun)
                .with_name(&name)
                .with_multi_queue(true),
        );
        assert!(multi.is_err_and(|e| e.is_already_exists()));
    }

    #[test]
    #[ignore = "needs CAP_NET_ADMIN"]
    fn persist_keeps_the_device_and_reopening_attaches_to_it() {
        let name = unique_name("s");
        let cleanup = Cleanup(name.clone());
        let device = open(DeviceConfig::new(DeviceKind::Tun).with_name(&name));
        let index = device.id();
        device.persist().unwrap();
        device.persist().unwrap();
        assert_ne!(tun_flags(&name) & libc::IFF_PERSIST, 0);
        drop(device);
        assert!(
            exists(&name),
            "a persistent device went away with its handle"
        );

        let device = open(DeviceConfig::new(DeviceKind::Tun).with_name(&name));
        assert_eq!(device.id(), index);
        device.unpersist().unwrap();
        device.unpersist().unwrap();
        assert_eq!(tun_flags(&name) & libc::IFF_PERSIST, 0);
        drop(device);
        assert!(!exists(&name));
        drop(cleanup);
    }

    #[test]
    #[ignore = "needs CAP_NET_ADMIN"]
    fn additional_queues_join_a_multi_queue_device() {
        let name = unique_name("q");
        let _cleanup = Cleanup(name.clone());
        let device = open(
            DeviceConfig::new(DeviceKind::Tun)
                .with_name(&name)
                .with_multi_queue(true),
        );
        let second = device.additional_queue().unwrap();
        let third = second.additional_queue().unwrap();
        assert_eq!((second.id(), third.id()), (device.id(), device.id()));
        assert_ne!(tun_flags(&name) & libc::IFF_MULTI_QUEUE, 0);
        // Persistence set through one queue is the device's.
        third.persist().unwrap();
        assert_ne!(tun_flags(&name) & libc::IFF_PERSIST, 0);
        second.unpersist().unwrap();
        drop((device, second));
        // The device lives while any queue is attached.
        assert!(exists(&name));
        assert_eq!(third.snapshot().unwrap().name, name);
        drop(third);
        assert!(!exists(&name));

        let single = unique_name("r");
        let _cleanup = Cleanup(single.clone());
        let device = open(DeviceConfig::new(DeviceKind::Tun).with_name(&single));
        assert!(device.additional_queue().is_err_and(|e| e.is_unsupported()));
    }

    /// Brings `name` up with `10.<octet>.0.1/24`, and returns a socket bound
    /// to that address plus the peer address routed into the device.
    fn route_into(name: &str, octet: u8) -> (UdpSocket, SocketAddrV4) {
        let address = format!("10.{octet}.0.1/24");
        ip(&["address", "add", &address, "dev", name]);
        ip(&["link", "set", name, "up"]);
        let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::new(10, octet, 0, 1), 0)).unwrap();
        (socket, SocketAddrV4::new(Ipv4Addr::new(10, octet, 0, 2), 9))
    }

    #[test]
    #[ignore = "needs CAP_NET_ADMIN and the ip tool"]
    fn packets_routed_into_the_device_are_received_and_oversize_ones_rejected() {
        let name = unique_name("i");
        let _cleanup = Cleanup(name.clone());
        let device = open(DeviceConfig::new(DeviceKind::Tun).with_name(&name));
        let octet = 200 + u8::try_from(std::process::id() % 50).unwrap();
        let (socket, peer) = route_into(&name, octet);

        // One UDP datagram of 100 bytes is a 128-byte IPv4 packet.
        let mut buf = [0u8; 1500];
        loop {
            socket.send_to(&[0xAB; 100], peer).unwrap();
            let len = device.recv(&mut buf).unwrap();
            // Skip IPv6 router solicitations and the like.
            if buf[0] >> 4 == 4 && len == 128 {
                break;
            }
        }
        socket.send_to(&[0xAB; 100], peer).unwrap();
        socket.send_to(&[0xCD; 10], peer).unwrap();
        let mut small = [0u8; 64];
        loop {
            match device.recv(&mut small) {
                Err(error) if error.is_buffer_too_small() => break,
                Ok(_) => {}
                Err(error) => panic!("{error:?}"),
            }
        }
        loop {
            let len = device.recv(&mut buf).unwrap();
            if buf[0] >> 4 == 4 && len == 38 {
                break;
            }
        }

        // A packet the device sends is delivered to the host stack: an IPv4
        // UDP datagram from the peer to the bound socket.
        let reply = ipv4_udp(peer, local(&socket), b"pong");
        assert_eq!(device.send(&reply).unwrap(), reply.len());
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut got = [0u8; 16];
        let (len, from) = socket.recv_from(&mut got).unwrap();
        assert_eq!((&got[..len], from), (&b"pong"[..], peer.into()));
    }

    fn local(socket: &UdpSocket) -> SocketAddrV4 {
        match socket.local_addr().unwrap() {
            std::net::SocketAddr::V4(address) => address,
            std::net::SocketAddr::V6(_) => unreachable!(),
        }
    }

    /// An IPv4 UDP packet with a valid header checksum and no UDP checksum.
    fn ipv4_udp(from: SocketAddrV4, to: SocketAddrV4, payload: &[u8]) -> Vec<u8> {
        let total = u16::try_from(28 + payload.len()).unwrap();
        let mut packet = vec![0x45, 0, 0, 0, 0, 0, 0x40, 0, 64, 17, 0, 0];
        packet[2..4].copy_from_slice(&total.to_be_bytes());
        packet.extend_from_slice(&from.ip().octets());
        packet.extend_from_slice(&to.ip().octets());
        let mut sum: u32 = packet
            .chunks(2)
            .map(|pair| u32::from(u16::from_be_bytes([pair[0], pair[1]])))
            .sum();
        while sum > 0xffff {
            sum = (sum & 0xffff) + (sum >> 16);
        }
        packet[10..12].copy_from_slice(&(!(sum as u16)).to_be_bytes());
        packet.extend_from_slice(&from.port().to_be_bytes());
        packet.extend_from_slice(&to.port().to_be_bytes());
        packet.extend_from_slice(&(total - 20).to_be_bytes());
        packet.extend_from_slice(&[0, 0]);
        packet.extend_from_slice(payload);
        packet
    }

    #[test]
    #[ignore = "needs CAP_NET_ADMIN and the ip tool"]
    fn sending_to_a_down_device_is_invalid_state() {
        let name = unique_name("d");
        let _cleanup = Cleanup(name.clone());
        let device = open(DeviceConfig::new(DeviceKind::Tun).with_name(&name));
        let packet = ipv4_udp(
            SocketAddrV4::new(Ipv4Addr::new(10, 9, 0, 2), 9),
            SocketAddrV4::new(Ipv4Addr::new(10, 9, 0, 1), 9),
            b"x",
        );
        assert!(device.send(&packet).is_err_and(|e| e.is_invalid_state()));
        let up = DeviceConfigPatch::new(device.id(), Some(DesiredAdminState::Up), None);
        device.apply(up.unwrap()).unwrap();
        assert_eq!(device.send(&packet).unwrap(), packet.len());
    }

    #[test]
    #[ignore = "needs CAP_NET_ADMIN and the ip tool"]
    fn deleting_the_device_wakes_a_waiting_recv_and_control_calls_are_not_found() {
        let name = unique_name("g");
        let _cleanup = Cleanup(name.clone());
        let device = Arc::new(open(DeviceConfig::new(DeviceKind::Tun).with_name(&name)));
        let receiver = {
            let device = Arc::clone(&device);
            std::thread::spawn(move || device.recv(&mut [0u8; 1500]))
        };
        std::thread::sleep(Duration::from_millis(100));
        ip(&["link", "delete", &name]);
        let result = receiver.join().unwrap();
        assert!(result.is_err_and(|e| e.is_disconnected()));
        assert!(
            device
                .recv(&mut [0u8; 1500])
                .is_err_and(|e| e.is_disconnected())
        );
        assert!(device.snapshot().is_err_and(|e| e.is_not_found()));
        let patch = DeviceConfigPatch::new(device.id(), None, Some(1400)).unwrap();
        assert!(device.apply(patch).is_err_and(|e| e.is_not_found()));
    }
}
