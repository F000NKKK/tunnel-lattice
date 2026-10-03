//! Raw operating-system error codes, as plain integers.
//!
//! Backends classify a native failure from its raw code, never from
//! `std::io::ErrorKind`: the same code means different things in different
//! places, and `std` decodes a raw code with the table of the host the code
//! is *read* on (code 6 is `ENXIO` on unix but `ERROR_INVALID_HANDLE` on
//! Windows), so a rule written against `ErrorKind` would behave differently
//! depending on where its tests run. The codes here are written out as
//! integers so the rules compile, and are tested, on every host, and so this
//! crate needs no `libc` or `windows-sys` dependency. The unit tests on
//! Linux check every Linux constant against `libc`.
//!
//! - [`linux`] holds the Linux constants and the table that maps an `errno`
//!   and the operation that produced it to an outcome ([`linux::classify`]),
//!   plus the subset of that table the `tun-rs`-backed backend applies
//!   ([`linux::packet_rule`]).
//! - [`darwin`] and [`windows`] hold only the codes a backend already uses.

/// Linux `errno` values and the native error table.
pub mod linux {
    use std::io;

    use tunnel_lattice_core::{Error, PlatformErrorCode};

    /// `EPERM`: operation not permitted.
    pub const EPERM: i32 = 1;
    /// `ENOENT`: no such file or directory.
    pub const ENOENT: i32 = 2;
    /// `EINTR`: interrupted by a signal.
    pub const EINTR: i32 = 4;
    /// `EIO`: input/output error. On a TUN/TAP write it means the device is
    /// administratively down.
    pub const EIO: i32 = 5;
    /// `ENXIO`: no such device or address.
    pub const ENXIO: i32 = 6;
    /// `EBADF`: bad file descriptor.
    pub const EBADF: i32 = 9;
    /// `EAGAIN`: the call would block. Identical to `EWOULDBLOCK`.
    pub const EAGAIN: i32 = 11;
    /// `ENOMEM`: out of memory.
    pub const ENOMEM: i32 = 12;
    /// `EACCES`: permission denied.
    pub const EACCES: i32 = 13;
    /// `EFAULT`: bad address. On a TUN/TAP read it means the read was already
    /// blocked when the device was deleted.
    pub const EFAULT: i32 = 14;
    /// `EBUSY`: device or resource busy.
    pub const EBUSY: i32 = 16;
    /// `EEXIST`: file exists.
    pub const EEXIST: i32 = 17;
    /// `ENODEV`: no such device.
    pub const ENODEV: i32 = 19;
    /// `EINVAL`: invalid argument.
    pub const EINVAL: i32 = 22;
    /// `ERANGE`: result out of range.
    pub const ERANGE: i32 = 34;
    /// `EBADFD`: file descriptor in bad state. A TUN/TAP file that was
    /// detached because its device was deleted fails with it.
    pub const EBADFD: i32 = 77;
    /// `EMSGSIZE`: message too long.
    pub const EMSGSIZE: i32 = 90;
    /// `EOPNOTSUPP`: operation not supported. Identical to `ENOTSUP`.
    pub const EOPNOTSUPP: i32 = 95;
    /// `ENOBUFS`: no buffer space available.
    pub const ENOBUFS: i32 = 105;

    /// The native operation an `errno` came from. The same `errno` maps
    /// differently depending on it.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Op {
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
        /// A receive on a queue that uses offload (virtio-net header)
        /// framing.
        OffloadRecv,
        /// A coalesced (super-packet) write on an offload queue.
        OffloadSend,
    }

    /// A packet-path operation: the four [`Op`] values that move packets,
    /// the only ones [`packet_rule`] applies to.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum IoOp {
        /// A receive on a plain queue ([`Op::Recv`]).
        Recv,
        /// A send on any queue ([`Op::Send`]).
        Send,
        /// A receive on an offload-framed queue ([`Op::OffloadRecv`]).
        OffloadRecv,
        /// A coalesced write on an offload queue ([`Op::OffloadSend`]).
        OffloadSend,
    }

    impl From<IoOp> for Op {
        fn from(op: IoOp) -> Self {
            match op {
                IoOp::Recv => Self::Recv,
                IoOp::Send => Self::Send,
                IoOp::OffloadRecv => Self::OffloadRecv,
                IoOp::OffloadSend => Self::OffloadSend,
            }
        }
    }

    /// What the caller does with a failed native call.
    #[derive(Debug)]
    pub enum Class {
        /// The call was interrupted by a signal; issue it again.
        Retry,
        /// The non-blocking queue fd has nothing to read (or no room to
        /// write): wait for readiness, or end a batch that already holds a
        /// packet.
        WouldBlock,
        /// `TUNSETIFF` refused the request with `EINVAL`. This is a conflict
        /// with an existing interface of that name (`AlreadyExists`) only if
        /// the name exists; otherwise it is `Platform(Linux(EINVAL))`. The
        /// caller looks the name up and decides with [`set_iff_einval`].
        CheckNameConflict,
        /// An offload queue delivered a frame the kernel itself rejected;
        /// drop it and read again.
        DropAndReread,
        /// The kernel refused a coalesced super-packet; resend its packets
        /// one by one.
        ResendPerPacket,
        /// The call failed for good with this error.
        Fail(Error),
    }

    /// Classifies `errno` returned by `op`.
    ///
    /// | errno | operation | outcome |
    /// |---|---|---|
    /// | `EINTR` | any | [`Class::Retry`] |
    /// | `EAGAIN` | packet I/O | [`Class::WouldBlock`] |
    /// | `EPERM`, `EACCES` | any | `PermissionDenied` |
    /// | `ENOENT`, `ENODEV`, `ENXIO` | opening `/dev/net/tun` | `DriverUnavailable` |
    /// | `EBUSY` | `TUNSETIFF` | `AlreadyExists` |
    /// | `EINVAL` | `TUNSETIFF` | [`Class::CheckNameConflict`] |
    /// | `ENODEV` | netlink | `NotFound` |
    /// | `EEXIST` | netlink | `AlreadyExists` |
    /// | `EOPNOTSUPP` | any | `Unsupported` |
    /// | `EBADFD`, `ENXIO` | packet I/O | `Disconnected` |
    /// | `EFAULT` | receive | `Disconnected` |
    /// | `EIO` | send | `InvalidState` |
    /// | `EINVAL` | offload-queue receive | [`Class::DropAndReread`] |
    /// | `EINVAL` | offload coalesced send | [`Class::ResendPerPacket`] |
    /// | anything else | any | `Platform(Linux(errno))` |
    #[must_use]
    pub fn classify(errno: i32, op: Op) -> Class {
        let io = matches!(op, Op::Recv | Op::Send | Op::OffloadRecv | Op::OffloadSend);
        match errno {
            EINTR => Class::Retry,
            EAGAIN if io => Class::WouldBlock,
            EPERM | EACCES => Class::Fail(Error::PermissionDenied),
            EOPNOTSUPP => Class::Fail(Error::Unsupported),
            ENOENT | ENODEV | ENXIO if op == Op::OpenTun => Class::Fail(Error::DriverUnavailable),
            EBUSY if op == Op::SetIff => Class::Fail(Error::AlreadyExists),
            EINVAL if op == Op::SetIff => Class::CheckNameConflict,
            ENODEV if op == Op::Netlink => Class::Fail(Error::NotFound),
            EEXIST if op == Op::Netlink => Class::Fail(Error::AlreadyExists),
            EBADFD | ENXIO if io => Class::Fail(Error::Disconnected),
            EFAULT if matches!(op, Op::Recv | Op::OffloadRecv) => Class::Fail(Error::Disconnected),
            EIO if matches!(op, Op::Send | Op::OffloadSend) => Class::Fail(Error::InvalidState),
            EINVAL if op == Op::OffloadRecv => Class::DropAndReread,
            EINVAL if op == Op::OffloadSend => Class::ResendPerPacket,
            other => Class::Fail(platform(other)),
        }
    }

    /// The subset of [`classify`] that the `tun-rs`-backed backend applies
    /// to the raw code of an `io::Error` on the packet path, before its own
    /// generic mapping: `Some` for exactly these rows, `None` for every
    /// other code.
    ///
    /// | errno | operation | outcome |
    /// |---|---|---|
    /// | `EINTR` | any | [`Class::Retry`] |
    /// | `EBADFD`, `ENXIO` | any | `Disconnected` |
    /// | `EFAULT` | receive | `Disconnected` |
    /// | `EIO` | send | `InvalidState` |
    /// | `EINVAL` | offload-queue receive | [`Class::DropAndReread`] |
    /// | `EINVAL` | offload coalesced send | [`Class::ResendPerPacket`] |
    ///
    /// `EAGAIN`, `EPERM` and `EOPNOTSUPP` are deliberately not part of it,
    /// although [`classify`] maps them: the `tun-rs`-backed backend maps
    /// them (or ends a drain on them) by its own rules, and this subset
    /// leaves every code it did not already handle to those rules. Where it
    /// returns `Some`, the class is the one [`classify`] returns.
    #[must_use]
    pub fn packet_rule(errno: i32, op: IoOp) -> Option<Class> {
        match (errno, op) {
            (EINTR, _) => Some(Class::Retry),
            (EBADFD | ENXIO, _) => Some(Class::Fail(Error::Disconnected)),
            (EFAULT, IoOp::Recv | IoOp::OffloadRecv) => Some(Class::Fail(Error::Disconnected)),
            (EIO, IoOp::Send | IoOp::OffloadSend) => Some(Class::Fail(Error::InvalidState)),
            (EINVAL, IoOp::OffloadRecv) => Some(Class::DropAndReread),
            (EINVAL, IoOp::OffloadSend) => Some(Class::ResendPerPacket),
            _ => None,
        }
    }

    /// Classifies a failed native call reported as an [`io::Error`] (as a
    /// netlink socket does). An error that carries no `errno` becomes
    /// `Platform(Unknown)`, never a fabricated code.
    #[must_use]
    pub fn classify_io(error: &io::Error, op: Op) -> Class {
        match error.raw_os_error() {
            Some(errno) => classify(errno, op),
            None => Class::Fail(Error::Platform(PlatformErrorCode::Unknown)),
        }
    }

    /// Resolves [`Class::CheckNameConflict`]: `name_exists` is the result of
    /// looking the requested name up after `TUNSETIFF` failed with `EINVAL`.
    #[must_use]
    pub fn set_iff_einval(name_exists: bool) -> Error {
        if name_exists {
            Error::AlreadyExists
        } else {
            platform(EINVAL)
        }
    }

    /// `Platform(Linux(errno))`, the fallback for an `errno` with no typed
    /// meaning.
    #[must_use]
    pub fn platform(errno: i32) -> Error {
        Error::Platform(PlatformErrorCode::Linux(errno))
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
        const ALL_IO_OPS: [IoOp; 4] =
            [IoOp::Recv, IoOp::Send, IoOp::OffloadRecv, IoOp::OffloadSend];

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
                assert!(matches!(classify(EINTR, op), Class::Retry), "{op:?}");
            }
        }

        #[test]
        fn eagain_waits_on_packet_io_only() {
            for op in IO_OPS {
                assert!(matches!(classify(EAGAIN, op), Class::WouldBlock), "{op:?}");
            }
            for op in CONTROL_OPS {
                assert!(is_linux(classify(EAGAIN, op), EAGAIN), "{op:?}");
            }
        }

        #[test]
        fn permission_and_unsupported_errors_map_everywhere() {
            for op in ALL_OPS {
                for errno in [EPERM, EACCES] {
                    assert!(
                        fails_with(classify(errno, op), Error::is_permission_denied),
                        "{op:?} {errno}"
                    );
                }
                assert!(fails_with(classify(EOPNOTSUPP, op), Error::is_unsupported));
            }
        }

        #[test]
        fn a_missing_tun_node_or_module_is_driver_unavailable() {
            for errno in [ENOENT, ENODEV, ENXIO] {
                assert!(fails_with(
                    classify(errno, Op::OpenTun),
                    Error::is_driver_unavailable
                ));
            }
            // Outside `open`, ENOENT is not a missing driver.
            assert!(is_linux(classify(ENOENT, Op::Netlink), ENOENT));
            assert!(is_linux(classify(ENOENT, Op::Recv), ENOENT));
        }

        #[test]
        fn tunsetiff_conflicts() {
            assert!(fails_with(
                classify(EBUSY, Op::SetIff),
                Error::is_already_exists
            ));
            assert!(matches!(
                classify(EINVAL, Op::SetIff),
                Class::CheckNameConflict
            ));
            assert!(set_iff_einval(true).is_already_exists());
            assert!(matches!(
                set_iff_einval(false),
                Error::Platform(PlatformErrorCode::Linux(EINVAL))
            ));
            // EBUSY elsewhere keeps its raw code.
            assert!(is_linux(classify(EBUSY, Op::Netlink), EBUSY));
            assert!(is_linux(classify(EBUSY, Op::QueueIoctl), EBUSY));
        }

        #[test]
        fn netlink_errors() {
            assert!(fails_with(
                classify(ENODEV, Op::Netlink),
                Error::is_not_found
            ));
            assert!(fails_with(
                classify(EEXIST, Op::Netlink),
                Error::is_already_exists
            ));
            assert!(is_linux(classify(EINVAL, Op::Netlink), EINVAL));
            assert!(is_linux(classify(ERANGE, Op::Netlink), ERANGE));
            assert!(is_linux(classify(EEXIST, Op::SetIff), EEXIST));
        }

        #[test]
        fn packet_io_lifecycle_errors() {
            for op in IO_OPS {
                for errno in [EBADFD, ENXIO] {
                    assert!(
                        fails_with(classify(errno, op), Error::is_disconnected),
                        "{op:?} {errno}"
                    );
                }
            }
            for op in [Op::Recv, Op::OffloadRecv] {
                assert!(fails_with(classify(EFAULT, op), Error::is_disconnected));
                assert!(is_linux(classify(EIO, op), EIO));
            }
            for op in [Op::Send, Op::OffloadSend] {
                assert!(fails_with(classify(EIO, op), Error::is_invalid_state));
                assert!(is_linux(classify(EFAULT, op), EFAULT));
            }
            // A detached queue fd seen by an ioctl is not a packet-path event.
            assert!(is_linux(classify(EBADFD, Op::QueueIoctl), EBADFD));
        }

        #[test]
        fn offload_einval_rules() {
            assert!(matches!(
                classify(EINVAL, Op::OffloadRecv),
                Class::DropAndReread
            ));
            assert!(matches!(
                classify(EINVAL, Op::OffloadSend),
                Class::ResendPerPacket
            ));
            assert!(is_linux(classify(EINVAL, Op::Recv), EINVAL));
            assert!(is_linux(classify(EINVAL, Op::Send), EINVAL));
        }

        #[test]
        fn unknown_errnos_keep_their_raw_code() {
            for op in ALL_OPS {
                assert!(is_linux(classify(ENOMEM, op), ENOMEM));
                assert!(is_linux(classify(ENOBUFS, op), ENOBUFS));
                assert!(is_linux(classify(4095, op), 4095));
            }
            assert!(matches!(
                platform(EMSGSIZE),
                Error::Platform(PlatformErrorCode::Linux(EMSGSIZE))
            ));
        }

        #[test]
        fn io_errors_use_the_raw_errno_or_unknown() {
            let raw = io::Error::from_raw_os_error(ENODEV);
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
        }

        /// The packet rule is exactly the listed rows of the full table: the
        /// same class where it answers, and no answer for any other code,
        /// including `EAGAIN`, `EPERM` and `EOPNOTSUPP`, on every operation.
        #[test]
        fn packet_rule_is_the_documented_subset_of_the_table() {
            fn same(a: &Class, b: &Class) -> bool {
                match (a, b) {
                    (Class::Retry, Class::Retry)
                    | (Class::WouldBlock, Class::WouldBlock)
                    | (Class::CheckNameConflict, Class::CheckNameConflict)
                    | (Class::DropAndReread, Class::DropAndReread)
                    | (Class::ResendPerPacket, Class::ResendPerPacket) => true,
                    (Class::Fail(a), Class::Fail(b)) => format!("{a:?}") == format!("{b:?}"),
                    _ => false,
                }
            }

            let mut answered = 0;
            for op in ALL_IO_OPS {
                for errno in 0..=4095 {
                    let rule = packet_rule(errno, op);
                    let listed = match errno {
                        EINTR | EBADFD | ENXIO => true,
                        EFAULT => matches!(op, IoOp::Recv | IoOp::OffloadRecv),
                        EIO => matches!(op, IoOp::Send | IoOp::OffloadSend),
                        EINVAL => matches!(op, IoOp::OffloadRecv | IoOp::OffloadSend),
                        _ => false,
                    };
                    assert_eq!(rule.is_some(), listed, "{op:?} {errno}");
                    if let Some(class) = rule {
                        answered += 1;
                        assert!(same(&class, &classify(errno, op.into())), "{op:?} {errno}");
                    }
                }
            }
            // 3 codes x 4 ops, EFAULT x 2, EIO x 2, EINVAL x 2.
            assert_eq!(answered, 12 + 2 + 2 + 2);
            for op in ALL_IO_OPS {
                for errno in [EAGAIN, EPERM, EOPNOTSUPP, EACCES, ENOENT, ENODEV] {
                    assert!(packet_rule(errno, op).is_none(), "{op:?} {errno}");
                }
            }
        }

        /// Every constant equals its `libc` value. Linux only, and not under
        /// Miri, which has no use for a check against the host's C library.
        #[cfg(all(target_os = "linux", not(miri)))]
        #[test]
        fn constants_match_libc() {
            assert_eq!(EPERM, libc::EPERM);
            assert_eq!(ENOENT, libc::ENOENT);
            assert_eq!(EINTR, libc::EINTR);
            assert_eq!(EIO, libc::EIO);
            assert_eq!(ENXIO, libc::ENXIO);
            assert_eq!(EBADF, libc::EBADF);
            assert_eq!(EAGAIN, libc::EAGAIN);
            assert_eq!(EAGAIN, libc::EWOULDBLOCK);
            assert_eq!(ENOMEM, libc::ENOMEM);
            assert_eq!(EACCES, libc::EACCES);
            assert_eq!(EFAULT, libc::EFAULT);
            assert_eq!(EBUSY, libc::EBUSY);
            assert_eq!(EEXIST, libc::EEXIST);
            assert_eq!(ENODEV, libc::ENODEV);
            assert_eq!(EINVAL, libc::EINVAL);
            assert_eq!(ERANGE, libc::ERANGE);
            assert_eq!(EBADFD, libc::EBADFD);
            assert_eq!(EMSGSIZE, libc::EMSGSIZE);
            assert_eq!(EOPNOTSUPP, libc::EOPNOTSUPP);
            assert_eq!(EOPNOTSUPP, libc::ENOTSUP);
            assert_eq!(ENOBUFS, libc::ENOBUFS);
        }
    }
}

/// macOS (Darwin) `errno` values: only the ones a backend already uses.
pub mod darwin {
    /// `EINTR`: interrupted by a signal. The same value as on Linux.
    pub const EINTR: i32 = 4;
    /// `ENXIO`: no such device or address. A feth TAP's BPF descriptor fails
    /// with it once the interface is destroyed. The same value as on Linux.
    pub const ENXIO: i32 = 6;
}

/// Windows error codes: only the ones a backend already uses.
pub mod windows {
    /// `ERROR_OPERATION_ABORTED`: an overlapped operation was cancelled.
    pub const ERROR_OPERATION_ABORTED: i32 = 995;
}

#[cfg(test)]
mod tests {
    use super::{darwin, linux, windows};

    #[test]
    fn darwin_codes_equal_the_linux_codes_they_share() {
        assert_eq!(darwin::EINTR, linux::EINTR);
        assert_eq!(darwin::ENXIO, linux::ENXIO);
    }

    #[test]
    fn windows_codes_are_pinned() {
        assert_eq!(windows::ERROR_OPERATION_ABORTED, 995);
    }
}
