//! Raw `errno` to [`Error`] mapping.
//!
//! Every native failure in this crate is classified from its raw `errno` and
//! the operation that produced it, never from `std::io::ErrorKind`: the same
//! errno means different things in different places (`ENODEV` from opening
//! `/dev/net/tun` is a missing driver, from a netlink request by ifindex it
//! is a missing device), and `ErrorKind` folds distinctions this backend's
//! contract depends on.
//!
//! | errno | operation | outcome |
//! |---|---|---|
//! | `EINTR` | any | [`Class::Retry`] |
//! | `EAGAIN` | packet I/O | [`Class::WouldBlock`] |
//! | `EPERM`, `EACCES` | any | `PermissionDenied` |
//! | `ENOENT`, `ENODEV`, `ENXIO` | opening `/dev/net/tun` | `DriverUnavailable` |
//! | `EBUSY` | `TUNSETIFF` | `AlreadyExists` |
//! | `EINVAL` | `TUNSETIFF` | [`Class::CheckNameConflict`] |
//! | `ENODEV` | netlink | `NotFound` |
//! | `EEXIST` | netlink | `AlreadyExists` |
//! | `EOPNOTSUPP` | any | `Unsupported` |
//! | `EBADFD`, `ENXIO` | packet I/O | `Disconnected` |
//! | `EFAULT` | receive | `Disconnected` |
//! | `EIO` | send | `InvalidState` |
//! | `EINVAL` | offload-queue receive | [`Class::DropAndReread`] |
//! | `EINVAL` | offload coalesced send | [`Class::ResendPerPacket`] |
//! | anything else | any | `Platform(Linux(errno))` |

use std::io;

use tunnel_lattice_core::{Error, PlatformErrorCode};

/// The native operation an errno came from. The same errno maps differently
/// depending on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Op {
    /// `open("/dev/net/tun")`.
    OpenTun,
    /// The `TUNSETIFF` ioctl that creates or attaches to the interface.
    SetIff,
    /// Any other ioctl on a queue fd (`TUNGETIFF`, `TUNSETPERSIST`,
    /// `TUNSETOFFLOAD`, ...).
    QueueIoctl,
    /// A request on the rtnetlink control socket, or the kernel's
    /// `NLMSG_ERROR` answer to one.
    Netlink,
    /// A receive on a plain queue.
    Recv,
    /// A send on any queue.
    Send,
    /// A receive on a queue that uses offload (virtio-net header) framing.
    OffloadRecv,
    /// A coalesced (super-packet) write on an offload queue.
    OffloadSend,
}

/// What the caller does with a failed native call.
#[derive(Debug)]
pub(crate) enum Class {
    /// The call was interrupted by a signal; issue it again.
    Retry,
    /// The non-blocking queue fd has nothing to read (or no room to write):
    /// wait for readiness, or end a batch that already holds a packet.
    WouldBlock,
    /// `TUNSETIFF` refused the request with `EINVAL`. This is a conflict with
    /// an existing interface of that name (`AlreadyExists`) only if the name
    /// exists; otherwise it is `Platform(Linux(EINVAL))`. The caller looks
    /// the name up and decides with [`set_iff_einval`].
    CheckNameConflict,
    /// An offload queue delivered a frame the kernel itself rejected; drop
    /// it and read again.
    DropAndReread,
    /// The kernel refused a coalesced super-packet; resend its packets one
    /// by one.
    ResendPerPacket,
    /// The call failed for good with this error.
    Fail(Error),
}

/// Classifies `errno` returned by `op`.
pub(crate) fn classify(errno: i32, op: Op) -> Class {
    let io = matches!(op, Op::Recv | Op::Send | Op::OffloadRecv | Op::OffloadSend);
    match errno {
        libc::EINTR => Class::Retry,
        libc::EAGAIN if io => Class::WouldBlock,
        libc::EPERM | libc::EACCES => Class::Fail(Error::PermissionDenied),
        libc::EOPNOTSUPP => Class::Fail(Error::Unsupported),
        libc::ENOENT | libc::ENODEV | libc::ENXIO if op == Op::OpenTun => {
            Class::Fail(Error::DriverUnavailable)
        }
        libc::EBUSY if op == Op::SetIff => Class::Fail(Error::AlreadyExists),
        libc::EINVAL if op == Op::SetIff => Class::CheckNameConflict,
        libc::ENODEV if op == Op::Netlink => Class::Fail(Error::NotFound),
        libc::EEXIST if op == Op::Netlink => Class::Fail(Error::AlreadyExists),
        libc::EBADFD | libc::ENXIO if io => Class::Fail(Error::Disconnected),
        libc::EFAULT if matches!(op, Op::Recv | Op::OffloadRecv) => {
            Class::Fail(Error::Disconnected)
        }
        libc::EIO if matches!(op, Op::Send | Op::OffloadSend) => Class::Fail(Error::InvalidState),
        libc::EINVAL if op == Op::OffloadRecv => Class::DropAndReread,
        libc::EINVAL if op == Op::OffloadSend => Class::ResendPerPacket,
        other => Class::Fail(platform(other)),
    }
}

/// Classifies a failed native call reported as an [`io::Error`] (as the
/// netlink socket does). An error that carries no errno becomes
/// `Platform(Unknown)`, never a fabricated code.
pub(crate) fn classify_io(error: &io::Error, op: Op) -> Class {
    match error.raw_os_error() {
        Some(errno) => classify(errno, op),
        None => Class::Fail(Error::Platform(PlatformErrorCode::Unknown)),
    }
}

/// Resolves [`Class::CheckNameConflict`]: `name_exists` is the result of
/// looking the requested name up after `TUNSETIFF` failed with `EINVAL`.
pub(crate) fn set_iff_einval(name_exists: bool) -> Error {
    if name_exists {
        Error::AlreadyExists
    } else {
        platform(libc::EINVAL)
    }
}

/// `Platform(Linux(errno))`, the fallback for an errno with no typed meaning.
pub(crate) fn platform(errno: i32) -> Error {
    Error::Platform(PlatformErrorCode::Linux(errno))
}

/// The error for a reply that does not follow the netlink protocol (a
/// truncated or undecodable message). It carries no errno, so it is
/// `Platform(Unknown)`.
pub(crate) fn malformed() -> Error {
    Error::Platform(PlatformErrorCode::Unknown)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_OPS: [Op; 8] = [
        Op::OpenTun,
        Op::SetIff,
        Op::QueueIoctl,
        Op::Netlink,
        Op::Recv,
        Op::Send,
        Op::OffloadRecv,
        Op::OffloadSend,
    ];
    const IO_OPS: [Op; 4] = [Op::Recv, Op::Send, Op::OffloadRecv, Op::OffloadSend];
    const CONTROL_OPS: [Op; 4] = [Op::OpenTun, Op::SetIff, Op::QueueIoctl, Op::Netlink];

    fn fails_with(class: Class, check: fn(&Error) -> bool) -> bool {
        matches!(class, Class::Fail(ref error) if check(error))
    }

    fn is_linux(class: Class, errno: i32) -> bool {
        matches!(
            class,
            Class::Fail(Error::Platform(PlatformErrorCode::Linux(code))) if code == errno
        )
    }

    #[test]
    fn eintr_is_retried_everywhere() {
        for op in ALL_OPS {
            assert!(matches!(classify(libc::EINTR, op), Class::Retry), "{op:?}");
        }
    }

    #[test]
    fn eagain_waits_on_packet_io_only() {
        assert_eq!(libc::EAGAIN, libc::EWOULDBLOCK);
        for op in IO_OPS {
            assert!(
                matches!(classify(libc::EAGAIN, op), Class::WouldBlock),
                "{op:?}"
            );
        }
        for op in CONTROL_OPS {
            assert!(is_linux(classify(libc::EAGAIN, op), libc::EAGAIN), "{op:?}");
        }
    }

    #[test]
    fn permission_and_unsupported_errors_map_everywhere() {
        for op in ALL_OPS {
            for errno in [libc::EPERM, libc::EACCES] {
                assert!(
                    fails_with(classify(errno, op), Error::is_permission_denied),
                    "{op:?} {errno}"
                );
            }
            assert!(fails_with(
                classify(libc::EOPNOTSUPP, op),
                Error::is_unsupported
            ));
            assert!(fails_with(
                classify(libc::ENOTSUP, op),
                Error::is_unsupported
            ));
        }
    }

    #[test]
    fn a_missing_tun_node_or_module_is_driver_unavailable() {
        for errno in [libc::ENOENT, libc::ENODEV, libc::ENXIO] {
            assert!(fails_with(
                classify(errno, Op::OpenTun),
                Error::is_driver_unavailable
            ));
        }
        // Outside `open`, ENOENT is not a missing driver.
        assert!(is_linux(classify(libc::ENOENT, Op::Netlink), libc::ENOENT));
        assert!(is_linux(classify(libc::ENOENT, Op::Recv), libc::ENOENT));
    }

    #[test]
    fn tunsetiff_conflicts() {
        assert!(fails_with(
            classify(libc::EBUSY, Op::SetIff),
            Error::is_already_exists
        ));
        assert!(matches!(
            classify(libc::EINVAL, Op::SetIff),
            Class::CheckNameConflict
        ));
        assert!(set_iff_einval(true).is_already_exists());
        assert!(matches!(
            set_iff_einval(false),
            Error::Platform(PlatformErrorCode::Linux(libc::EINVAL))
        ));
        // EBUSY elsewhere keeps its raw code.
        assert!(is_linux(classify(libc::EBUSY, Op::Netlink), libc::EBUSY));
        assert!(is_linux(classify(libc::EBUSY, Op::QueueIoctl), libc::EBUSY));
    }

    #[test]
    fn netlink_errors() {
        assert!(fails_with(
            classify(libc::ENODEV, Op::Netlink),
            Error::is_not_found
        ));
        assert!(fails_with(
            classify(libc::EEXIST, Op::Netlink),
            Error::is_already_exists
        ));
        assert!(is_linux(classify(libc::EINVAL, Op::Netlink), libc::EINVAL));
        assert!(is_linux(classify(libc::ERANGE, Op::Netlink), libc::ERANGE));
        assert!(is_linux(classify(libc::EEXIST, Op::SetIff), libc::EEXIST));
    }

    #[test]
    fn packet_io_lifecycle_errors() {
        for op in IO_OPS {
            for errno in [libc::EBADFD, libc::ENXIO] {
                assert!(
                    fails_with(classify(errno, op), Error::is_disconnected),
                    "{op:?} {errno}"
                );
            }
        }
        for op in [Op::Recv, Op::OffloadRecv] {
            assert!(fails_with(
                classify(libc::EFAULT, op),
                Error::is_disconnected
            ));
            assert!(is_linux(classify(libc::EIO, op), libc::EIO));
        }
        for op in [Op::Send, Op::OffloadSend] {
            assert!(fails_with(classify(libc::EIO, op), Error::is_invalid_state));
            assert!(is_linux(classify(libc::EFAULT, op), libc::EFAULT));
        }
        // A detached queue fd seen by an ioctl is not a packet-path event.
        assert!(is_linux(
            classify(libc::EBADFD, Op::QueueIoctl),
            libc::EBADFD
        ));
    }

    #[test]
    fn offload_einval_rules() {
        assert!(matches!(
            classify(libc::EINVAL, Op::OffloadRecv),
            Class::DropAndReread
        ));
        assert!(matches!(
            classify(libc::EINVAL, Op::OffloadSend),
            Class::ResendPerPacket
        ));
        assert!(is_linux(classify(libc::EINVAL, Op::Recv), libc::EINVAL));
        assert!(is_linux(classify(libc::EINVAL, Op::Send), libc::EINVAL));
    }

    #[test]
    fn unknown_errnos_keep_their_raw_code() {
        for op in ALL_OPS {
            assert!(is_linux(classify(libc::ENOMEM, op), libc::ENOMEM));
            assert!(is_linux(classify(libc::ENOBUFS, op), libc::ENOBUFS));
            assert!(is_linux(classify(4095, op), 4095));
        }
        assert!(matches!(
            platform(libc::EMSGSIZE),
            Error::Platform(PlatformErrorCode::Linux(libc::EMSGSIZE))
        ));
    }

    #[test]
    fn io_errors_use_the_raw_errno_or_unknown() {
        let raw = io::Error::from_raw_os_error(libc::ENODEV);
        assert!(fails_with(
            classify_io(&raw, Op::Netlink),
            Error::is_not_found
        ));
        assert!(fails_with(
            classify_io(&raw, Op::OpenTun),
            Error::is_driver_unavailable
        ));
        // `ErrorKind` alone is never trusted: no errno means Unknown.
        let kind_only = io::Error::from(io::ErrorKind::PermissionDenied);
        assert!(matches!(
            classify_io(&kind_only, Op::Netlink),
            Class::Fail(Error::Platform(PlatformErrorCode::Unknown))
        ));
        assert!(matches!(
            malformed(),
            Error::Platform(PlatformErrorCode::Unknown)
        ));
    }
}
