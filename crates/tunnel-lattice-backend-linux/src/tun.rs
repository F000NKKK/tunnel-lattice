//! `/dev/net/tun` queue descriptors and the ioctls issued on them.
//!
//! Every queue descriptor is opened `O_RDWR | O_CLOEXEC | O_NONBLOCK` and
//! keeps `O_NONBLOCK` for its whole life: the packet path waits with `poll`
//! instead of a blocking read, so the flag never has to be toggled on a
//! descriptor another thread may be using.
//!
//! The backend never creates the device node: a missing `/dev/net/tun` (or
//! a missing `tun` module) is reported as `Error::DriverUnavailable`, never
//! repaired with `mknod`.
//!
//! The functions here return the raw `errno` of a failed call; the caller
//! classifies it with the operation it belongs to (see [`crate::errno`]).
//!
//! `TUNSETPERSIST` and `TUNSETOFFLOAD` take their argument as an integer
//! **by value**: the kernel tests it directly (`tun.c`, `__tun_chr_ioctl`),
//! so passing a pointer, which is never zero, would set persistence or
//! request offload instead of clearing it.

use std::ffi::{CStr, c_int, c_short, c_ulong};
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

use tunnel_lattice_core::{Error, Result};
use tunnel_lattice_model::DeviceKind;
use tunnel_lattice_model::backend::UNIX_MAX_NAME_BYTES;

use crate::errno::{self, Class, Op};

/// The clone device every queue is opened from.
const TUN_NODE: &CStr = c"/dev/net/tun";

/// The `TUNSETIFF` flags that describe a queue's framing and queue model.
/// `IFF_PERSIST` and the `TUNGETIFF`-only bits (`IFF_NOFILTER`,
/// `IFF_DETACH_QUEUE`) are not among them.
const QUEUE_FLAGS: c_int = libc::IFF_TUN
    | libc::IFF_TAP
    | libc::IFF_NO_PI
    | libc::IFF_MULTI_QUEUE
    | libc::IFF_VNET_HDR
    | libc::IFF_NAPI
    | libc::IFF_NAPI_FRAGS;

/// The `TUNSETIFF` flags for a new queue of `kind`: always `IFF_NO_PI` (no
/// packet-information prefix), plus `IFF_MULTI_QUEUE` when requested. Never
/// `IFF_VNET_HDR`: this backend does not use offload framing yet.
pub(crate) fn request_flags(kind: DeviceKind, multi_queue: bool) -> Result<c_int> {
    let kind = match kind {
        DeviceKind::Tun => libc::IFF_TUN,
        DeviceKind::Tap => libc::IFF_TAP,
        // `DeviceKind` is `#[non_exhaustive]`: a kind this backend does not
        // know is unsupported, not a malformed request.
        _ => return Err(Error::Unsupported),
    };
    let queue = if multi_queue {
        libc::IFF_MULTI_QUEUE
    } else {
        0
    };
    Ok(kind | libc::IFF_NO_PI | queue)
}

/// The `TUNSETIFF` flags for another queue of a device whose flags read
/// back (`TUNGETIFF`) as `read_back`: the device's own framing and queue
/// model, not what the first queue requested. `IFF_NO_PI` is always set:
/// `TUNGETIFF` reports `IFF_NOFILTER`, which has the same bit, so the bit
/// read back does not tell the two apart, and this backend only ever
/// requests `IFF_NO_PI`.
pub(crate) const fn queue_flags(read_back: c_int) -> c_int {
    (read_back & QUEUE_FLAGS) | libc::IFF_NO_PI
}

/// Checks the flags a queue reads back after `TUNSETIFF`.
///
/// Joining a multi-queue device that already has queues leaves the
/// device's flags as its first queue set them, so they can differ from
/// what was requested. A device using virtio-net header framing
/// (`IFF_VNET_HDR`) would prefix every packet with that header, which this
/// backend does not parse yet: such a queue is refused with
/// `Error::Unsupported` rather than handing out misframed packets.
pub(crate) fn check_framing(read_back: c_int) -> Result<()> {
    if read_back & libc::IFF_VNET_HDR == 0 {
        Ok(())
    } else {
        Err(Error::Unsupported)
    }
}

/// Opens a new, not yet attached queue descriptor on `/dev/net/tun`.
///
/// A missing node or module (`ENOENT`, `ENODEV`, `ENXIO`) is
/// `Error::DriverUnavailable`, and no permission on the node is
/// `Error::PermissionDenied`.
pub(crate) fn open_node() -> Result<OwnedFd> {
    loop {
        // SAFETY: `TUN_NODE` is a valid NUL-terminated path that outlives
        // the call, and the flags are plain `open(2)` flags; the call reads
        // no other memory. On success the returned descriptor is new and
        // owned by nobody else.
        let fd = unsafe {
            libc::open(
                TUN_NODE.as_ptr(),
                libc::O_RDWR | libc::O_CLOEXEC | libc::O_NONBLOCK,
            )
        };
        if fd >= 0 {
            // SAFETY: `fd` was just returned by a successful `open`, so it
            // is a valid descriptor that nothing else owns or will close.
            return Ok(unsafe { OwnedFd::from_raw_fd(fd) });
        }
        let code = last_errno();
        match errno::classify(code, Op::OpenTun) {
            Class::Retry => {}
            Class::Fail(error) => return Err(error),
            _ => return Err(errno::platform(code)),
        }
    }
}

/// An interface request (`struct ifreq`) carrying a name and the
/// `ifr_flags` member.
pub(crate) struct IfReq(libc::ifreq);

impl IfReq {
    /// A request for `name` (empty lets the kernel pick `tun%d`/`tap%d`)
    /// with `flags`. The caller has prechecked the name, so it fits.
    pub(crate) fn new(name: Option<&str>, flags: c_int) -> Self {
        // SAFETY: `ifreq` is a plain C struct of integers, byte arrays and
        // a union of such members (and of a raw pointer, for which all-zero
        // is the null pointer); the all-zero bit pattern is a valid value
        // of every member.
        let mut req: libc::ifreq = unsafe { std::mem::zeroed() };
        let bytes = name.unwrap_or_default().as_bytes();
        for (slot, byte) in req
            .ifr_name
            .iter_mut()
            .zip(bytes.iter().take(UNIX_MAX_NAME_BYTES))
        {
            *slot = libc::c_char::from_ne_bytes([*byte]);
        }
        // `ifr_flags` is a C `short`; the TUN flags all fit in its 16 bits,
        // so the cast keeps every bit.
        req.ifr_ifru.ifru_flags = flags as c_short;
        Self(req)
    }

    /// The interface name, up to the first NUL. A name that is not UTF-8
    /// (never one this backend requests, nor a kernel-chosen `tunN`/`tapN`)
    /// is reported as a malformed reply.
    pub(crate) fn name(&self) -> Result<String> {
        let bytes: Vec<u8> = self
            .0
            .ifr_name
            .iter()
            .map(|c| c.to_ne_bytes()[0])
            .take_while(|&b| b != 0)
            .collect();
        String::from_utf8(bytes).map_err(|_| errno::malformed())
    }

    /// `ifr_flags`, widened without sign extension (the TUN flags use the
    /// top bit, `IFF_TUN_EXCL`, as a plain flag).
    pub(crate) fn flags(&self) -> c_int {
        // SAFETY: every `IfReq` is built by `new`, which writes
        // `ifru_flags`, or filled by the kernel's `TUNGETIFF`, which writes
        // `ifr_flags`; both initialize the `short` this reads, and every
        // bit pattern is a valid `c_short`.
        let flags = unsafe { self.0.ifr_ifru.ifru_flags };
        c_int::from(flags as u16)
    }
}

/// `TUNSETIFF`: creates the interface, or attaches the queue to an
/// existing one, as `req` describes. On success the kernel writes the
/// actual name back into `req`.
pub(crate) fn set_iff(fd: BorrowedFd<'_>, req: &mut IfReq) -> std::result::Result<(), i32> {
    // SAFETY: `fd` is an open descriptor for the duration of the call.
    // `TUNSETIFF` reads `sizeof(struct ifreq)` bytes through the pointer and
    // writes the name back into the same struct; `req.0` is exactly such a
    // live, writable `ifreq` that outlives the call. Nothing else is read
    // or written.
    let rc = unsafe { libc::ioctl(fd.as_raw_fd(), libc::TUNSETIFF, &raw mut req.0) };
    check(rc)
}

/// `TUNGETIFF`: the attached interface's current name and flags.
pub(crate) fn get_iff(fd: BorrowedFd<'_>) -> std::result::Result<IfReq, i32> {
    let mut req = IfReq::new(None, 0);
    // SAFETY: `fd` is an open descriptor for the duration of the call.
    // `TUNGETIFF` writes `sizeof(struct ifreq)` bytes through the pointer,
    // which points at `req.0`, a live, writable `ifreq`. Nothing else is
    // read or written.
    let rc = unsafe { libc::ioctl(fd.as_raw_fd(), libc::TUNGETIFF, &raw mut req.0) };
    check(rc).map(|()| req)
}

/// `TUNSETPERSIST`, by value: `true` sets `IFF_PERSIST`, `false` clears
/// it. It acts on the device, so any of its queues may issue it.
pub(crate) fn set_persist(fd: BorrowedFd<'_>, persist: bool) -> std::result::Result<(), i32> {
    let value = c_ulong::from(persist);
    // SAFETY: `fd` is an open descriptor for the duration of the call.
    // `TUNSETPERSIST` takes its argument as an integer by value and reads
    // no memory through it. No Rust-owned memory is touched.
    let rc = unsafe { libc::ioctl(fd.as_raw_fd(), libc::TUNSETPERSIST, value) };
    check(rc)
}

/// `TUNSETOFFLOAD`, by value, with an empty mask: turns every offload off
/// on the device. Issued on a queue without `IFF_VNET_HDR`, where a mask
/// left behind by an earlier user (for example a persistent device last
/// opened with offload) would make the kernel hand over unsegmented
/// super-packets and unfinished checksums with no header to describe them.
pub(crate) fn clear_offload(fd: BorrowedFd<'_>) -> std::result::Result<(), i32> {
    let none: c_ulong = 0;
    // SAFETY: `fd` is an open descriptor for the duration of the call.
    // `TUNSETOFFLOAD` takes its argument as an integer by value and reads
    // no memory through it; `0` enables no offload. No Rust-owned memory
    // is touched.
    let rc = unsafe { libc::ioctl(fd.as_raw_fd(), libc::TUNSETOFFLOAD, none) };
    check(rc)
}

/// Turns an ioctl return code into the call's `errno`.
fn check(rc: c_int) -> std::result::Result<(), i32> {
    if rc < 0 { Err(last_errno()) } else { Ok(()) }
}

/// The calling thread's `errno` after a failed call.
pub(crate) fn last_errno() -> i32 {
    std::io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EIO)
}

/// Runs a queue ioctl, retrying `EINTR` and classifying any other failure
/// as a queue-ioctl error.
pub(crate) fn queue_ioctl<T>(mut call: impl FnMut() -> std::result::Result<T, i32>) -> Result<T> {
    loop {
        match call() {
            Ok(value) => return Ok(value),
            Err(errno) => match errno::classify(errno, Op::QueueIoctl) {
                Class::Retry => {}
                Class::Fail(error) => return Err(error),
                _ => return Err(errno::platform(errno)),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsFd;

    use tunnel_lattice_core::PlatformErrorCode;

    use super::*;

    /// The shared name limit is the kernel's `IFNAMSIZ` minus the NUL.
    #[test]
    fn the_shared_name_limit_is_the_kernel_limit() {
        assert_eq!(UNIX_MAX_NAME_BYTES, libc::IFNAMSIZ - 1);
    }

    #[test]
    fn requested_flags_always_strip_packet_information() {
        assert_eq!(
            request_flags(DeviceKind::Tun, false).unwrap(),
            libc::IFF_TUN | libc::IFF_NO_PI
        );
        assert_eq!(
            request_flags(DeviceKind::Tap, true).unwrap(),
            libc::IFF_TAP | libc::IFF_NO_PI | libc::IFF_MULTI_QUEUE
        );
        for kind in [DeviceKind::Tun, DeviceKind::Tap] {
            let flags = request_flags(kind, true).unwrap();
            assert_eq!(flags & libc::IFF_VNET_HDR, 0);
            assert_eq!(flags & libc::IFF_PERSIST, 0);
        }
    }

    #[test]
    fn another_queue_uses_the_flags_read_back() {
        // What `TUNGETIFF` reports for a persistent multi-queue TUN device
        // with no socket filter: `IFF_NOFILTER` shares the `IFF_NO_PI` bit.
        let read_back = libc::IFF_TUN
            | libc::IFF_MULTI_QUEUE
            | libc::IFF_PERSIST
            | libc::IFF_NOFILTER
            | libc::IFF_DETACH_QUEUE;
        assert_eq!(
            queue_flags(read_back),
            libc::IFF_TUN | libc::IFF_MULTI_QUEUE | libc::IFF_NO_PI
        );
        assert_eq!(queue_flags(libc::IFF_TAP), libc::IFF_TAP | libc::IFF_NO_PI);
        assert_eq!(
            queue_flags(libc::IFF_TAP | libc::IFF_VNET_HDR) & libc::IFF_VNET_HDR,
            libc::IFF_VNET_HDR
        );
    }

    #[test]
    fn a_queue_with_header_framing_is_unsupported() {
        assert!(check_framing(libc::IFF_TUN | libc::IFF_NO_PI).is_ok());
        assert!(check_framing(libc::IFF_TAP | libc::IFF_MULTI_QUEUE).is_ok());
        assert!(
            check_framing(libc::IFF_TUN | libc::IFF_VNET_HDR).is_err_and(|e| e.is_unsupported())
        );
    }

    #[test]
    fn an_interface_request_round_trips_its_name_and_flags() {
        let flags = libc::IFF_TAP | libc::IFF_NO_PI | libc::IFF_MULTI_QUEUE;
        let req = IfReq::new(Some("tl-test0"), flags);
        assert_eq!(req.name().unwrap(), "tl-test0");
        assert_eq!(req.flags(), flags);
        // The longest name keeps its terminating NUL.
        let req = IfReq::new(Some("a234567890abcde"), 0);
        assert_eq!(req.name().unwrap(), "a234567890abcde");
        assert_eq!(req.0.ifr_name[UNIX_MAX_NAME_BYTES], 0);
        // No name asks the kernel to choose one.
        let req = IfReq::new(None, libc::IFF_TUN);
        assert_eq!(req.name().unwrap(), "");
        // The top flag bit is not sign-extended.
        let req = IfReq::new(None, 0x8000 | libc::IFF_TUN);
        assert_eq!(req.flags(), 0x8001);
    }

    #[test]
    fn a_name_that_is_not_utf8_is_malformed() {
        let mut req = IfReq::new(None, 0);
        req.0.ifr_name[0] = libc::c_char::from_ne_bytes([0xff]);
        assert!(matches!(
            req.name(),
            Err(Error::Platform(PlatformErrorCode::Unknown))
        ));
    }

    #[test]
    fn ioctls_report_the_raw_errno_of_a_descriptor_that_is_not_a_queue() {
        // A file that is not `/dev/net/tun` refuses every TUN ioctl with
        // `ENOTTY`; nothing is changed.
        let file = std::fs::File::open("/dev/null").unwrap();
        let fd = file.as_fd();
        assert_eq!(get_iff(fd).err(), Some(libc::ENOTTY));
        assert_eq!(set_persist(fd, false), Err(libc::ENOTTY));
        assert_eq!(clear_offload(fd), Err(libc::ENOTTY));
        let mut req = IfReq::new(Some("tl-never"), libc::IFF_TUN);
        assert_eq!(set_iff(fd, &mut req), Err(libc::ENOTTY));
        assert!(matches!(
            queue_ioctl(|| set_persist(fd, true)),
            Err(Error::Platform(PlatformErrorCode::Linux(libc::ENOTTY)))
        ));
    }

    #[test]
    fn queue_ioctls_retry_eintr_and_map_through_the_table() {
        let mut calls = 0;
        let result = queue_ioctl(|| {
            calls += 1;
            if calls < 3 { Err(libc::EINTR) } else { Ok(7) }
        });
        assert_eq!((result.unwrap(), calls), (7, 3));
        assert!(queue_ioctl(|| Err::<(), _>(libc::EPERM)).is_err_and(|e| e.is_permission_denied()));
        assert!(matches!(
            queue_ioctl(|| Err::<(), _>(libc::EBADFD)),
            Err(Error::Platform(PlatformErrorCode::Linux(libc::EBADFD)))
        ));
    }

    /// Opening the clone device needs no privilege where the node exists
    /// (it is normally mode 0666); only `TUNSETIFF` does. The descriptor is
    /// non-blocking and close-on-exec from creation.
    #[test]
    fn the_clone_device_opens_non_blocking_and_close_on_exec() {
        let fd = match open_node() {
            Ok(fd) => fd,
            Err(error) => {
                // No node or module here (a minimal container), or no
                // access to it: both are the documented mappings.
                assert!(
                    error.is_driver_unavailable() || error.is_permission_denied(),
                    "{error:?}"
                );
                return;
            }
        };
        // SAFETY: `fd` is an open descriptor owned by this test; `F_GETFL`
        // and `F_GETFD` only read its flags.
        let (status, descriptor) = unsafe {
            (
                libc::fcntl(fd.as_raw_fd(), libc::F_GETFL),
                libc::fcntl(fd.as_raw_fd(), libc::F_GETFD),
            )
        };
        assert_ne!(status & libc::O_NONBLOCK, 0);
        assert_eq!(status & libc::O_ACCMODE, libc::O_RDWR);
        assert_ne!(descriptor & libc::FD_CLOEXEC, 0);
        // A queue that was never attached refuses queue ioctls with
        // `EBADFD`.
        assert_eq!(get_iff(fd.as_fd()).err(), Some(libc::EBADFD));
    }
}
