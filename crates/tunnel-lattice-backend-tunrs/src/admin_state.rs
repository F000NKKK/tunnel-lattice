//! The administrative-state reads behind `snapshot` on macOS and Windows.
//!
//! `tun-rs` exposes a getter only on Linux (`is_running`: `IFF_UP` and
//! `IFF_RUNNING`), so this backend reads the state itself elsewhere, with
//! the same meaning: the device is administratively up *and* able to pass
//! packets.
//!
//! | OS | Native read | `Up` iff |
//! |---|---|---|
//! | macOS (utun, the `dev` side of a feth pair) | `SIOCGIFFLAGS` by interface name | `IFF_UP` and `IFF_RUNNING` |
//! | Windows (Wintun, tap-windows6) | `GetIfEntry2` by interface LUID | `OperStatus == IfOperStatusUp` |
//!
//! On Windows `apply(Down)` disconnects the TAP media or ends the Wintun
//! session; neither changes the NDIS administrative status, so the
//! operational status (which NDIS derives from the administrative status
//! and the media state) is the signal that follows `apply`. NDIS applies
//! such a change asynchronously, so a read right after `apply` can still
//! report the previous state.
//!
//! Each read is a pure predicate over the native value plus a thin FFI
//! wrapper; the predicates and the private constants are unit-tested on
//! every host.

use std::ffi::c_int;

use tunnel_lattice_model::AdminState;

/// `IFF_UP`, identical on Linux and macOS (asserted against `libc` by the
/// unit tests on macOS).
const IFF_UP: c_int = 0x1;

/// `IFF_RUNNING`, identical on Linux and macOS (asserted against `libc` by
/// the unit tests on macOS).
const IFF_RUNNING: c_int = 0x40;

/// `IfOperStatusUp` from `ifdef.h` (asserted against `windows-sys` by the
/// unit tests on Windows).
const IF_OPER_STATUS_UP: i32 = 1;

/// The macOS predicate: `Up` iff the interface flags carry both `IFF_UP`
/// and `IFF_RUNNING`.
#[cfg_attr(
    all(not(target_os = "macos"), not(test)),
    expect(dead_code, reason = "the interface-flags read is macOS-only")
)]
pub(crate) fn admin_from_flags(flags: c_int) -> AdminState {
    if flags & IFF_UP != 0 && flags & IFF_RUNNING != 0 {
        AdminState::Up
    } else {
        AdminState::Down
    }
}

/// The Windows predicate: `Up` iff the operational status is
/// `IfOperStatusUp`; every other status (down, dormant, not present,
/// lower layer down, testing, unknown) is `Down`.
#[cfg_attr(
    all(not(target_os = "windows"), not(test)),
    expect(dead_code, reason = "the operational-status read is Windows-only")
)]
pub(crate) fn admin_from_oper(status: i32) -> AdminState {
    if status == IF_OPER_STATUS_UP {
        AdminState::Up
    } else {
        AdminState::Down
    }
}

/// `SIOCGIFFLAGS`, `_IOWR('i', 17, struct ifreq)`. `libc` has no Apple
/// constant for it; the value is pinned by a unit test that rebuilds it
/// from the `ioctl` encoding and `size_of::<libc::ifreq>()`.
#[cfg(target_os = "macos")]
const SIOCGIFFLAGS: libc::c_ulong = 0xc020_6911;

/// Reads the interface flags of `name` with `SIOCGIFFLAGS` and applies
/// [`admin_from_flags`]. Unprivileged; uses a throw-away `AF_INET` datagram
/// socket that is closed before returning.
#[cfg(target_os = "macos")]
pub(crate) fn macos_admin_state(name: &str) -> std::io::Result<AdminState> {
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    let bytes = name.as_bytes();
    if bytes.len() >= libc::IFNAMSIZ || bytes.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "interface name does not fit ifreq",
        ));
    }
    // SAFETY: `socket` takes no pointers; a negative return is an error and
    // is handled below.
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a descriptor this function just opened and nothing
    // else owns; `OwnedFd` closes it on every return path below.
    let socket = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: `ifreq` is plain old data (a byte array and a union of plain
    // fields), for which all-zero bytes are a valid value.
    let mut request: libc::ifreq = unsafe { std::mem::zeroed() };
    // The name is at most `IFNAMSIZ - 1` bytes (checked above) and the rest
    // of `ifr_name` stays zero, so it is NUL-terminated.
    for (dst, src) in request.ifr_name.iter_mut().zip(bytes) {
        *dst = *src as libc::c_char;
    }
    // SAFETY: `socket` is an open socket owned by this function, and
    // `request` is a valid, writable `ifreq` with a NUL-terminated name of
    // at most 15 bytes. `SIOCGIFFLAGS` reads the name and writes only the
    // flags field of `request`; it touches no other memory.
    let rc = unsafe { libc::ioctl(socket.as_raw_fd(), SIOCGIFFLAGS, &mut request) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a successful `SIOCGIFFLAGS` wrote `ifru_flags`, and every bit
    // pattern is a valid `c_short`.
    let flags = unsafe { request.ifr_ifru.ifru_flags };
    // The flags are an unsigned 16-bit field stored in a `c_short`.
    Ok(admin_from_flags(c_int::from(flags as u16)))
}

/// Reads the operational status of the interface with `luid` with
/// `GetIfEntry2` and applies [`admin_from_oper`]. Unprivileged. A failure
/// is the raw Win32 code `GetIfEntry2` returned.
#[cfg(target_os = "windows")]
pub(crate) fn windows_admin_state(
    luid: windows_sys::Win32::NetworkManagement::Ndis::NET_LUID_LH,
) -> std::io::Result<AdminState> {
    use windows_sys::Win32::Foundation::NO_ERROR;
    use windows_sys::Win32::NetworkManagement::IpHelper::{GetIfEntry2, MIB_IF_ROW2};

    // SAFETY: `MIB_IF_ROW2` is plain old data (integers, arrays, GUIDs and
    // unions of integers), for which all-zero bytes are a valid value.
    let mut row: MIB_IF_ROW2 = unsafe { std::mem::zeroed() };
    row.InterfaceLuid = luid;
    // SAFETY: `row` is a valid, writable, zeroed `MIB_IF_ROW2` with
    // `InterfaceLuid` set, which is the key `GetIfEntry2` reads; it writes
    // only into `row` and keeps no pointer to it after returning.
    let result = unsafe { GetIfEntry2(&mut row) };
    if result != NO_ERROR {
        // A `WIN32_ERROR` is a `DWORD`; `std` stores it in an `i32`.
        return Err(std::io::Error::from_raw_os_error(result as i32));
    }
    Ok(admin_from_oper(row.OperStatus))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interface_flags_are_up_only_with_up_and_running() {
        assert_eq!(admin_from_flags(IFF_UP | IFF_RUNNING), AdminState::Up);
        // Other flags (broadcast, multicast, point-to-point) do not matter.
        assert_eq!(
            admin_from_flags(IFF_UP | IFF_RUNNING | 0x2 | 0x8000),
            AdminState::Up
        );
        assert_eq!(admin_from_flags(IFF_UP), AdminState::Down);
        assert_eq!(admin_from_flags(IFF_RUNNING), AdminState::Down);
        assert_eq!(admin_from_flags(0), AdminState::Down);
    }

    #[test]
    fn operational_status_is_up_only_when_up() {
        // `ifdef.h`: Up 1, Down 2, Testing 3, Unknown 4, Dormant 5,
        // NotPresent 6, LowerLayerDown 7.
        assert_eq!(admin_from_oper(1), AdminState::Up);
        for status in [2, 3, 4, 5, 6, 7, 0, -1] {
            assert_eq!(admin_from_oper(status), AdminState::Down, "{status}");
        }
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn interface_flag_constants_match_libc() {
        assert_eq!(IFF_UP, libc::IFF_UP);
        assert_eq!(IFF_RUNNING, libc::IFF_RUNNING);
    }

    /// `SIOCGIFFLAGS` is `_IOWR('i', 17, struct ifreq)`: the in/out
    /// direction bits, the parameter length in bits 16..29, the group
    /// character and the number.
    #[test]
    #[cfg(target_os = "macos")]
    fn siocgifflags_matches_its_ioctl_encoding() {
        const IOC_INOUT: libc::c_ulong = 0x8000_0000 | 0x4000_0000;
        const IOCPARM_MASK: libc::c_ulong = 0x1fff;
        let len = std::mem::size_of::<libc::ifreq>() as libc::c_ulong;
        let encoded =
            IOC_INOUT | ((len & IOCPARM_MASK) << 16) | ((b'i' as libc::c_ulong) << 8) | 17;
        assert_eq!(SIOCGIFFLAGS, encoded);
        assert_eq!(SIOCGIFFLAGS, 0xc020_6911);
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn operational_status_constants_match_windows_sys() {
        use windows_sys::Win32::NetworkManagement::Ndis::{
            IfOperStatusDormant, IfOperStatusDown, IfOperStatusLowerLayerDown,
            IfOperStatusNotPresent, IfOperStatusUp,
        };
        assert_eq!(IF_OPER_STATUS_UP, IfOperStatusUp);
        assert_eq!(admin_from_oper(IfOperStatusUp), AdminState::Up);
        for status in [
            IfOperStatusDown,
            IfOperStatusDormant,
            IfOperStatusNotPresent,
            IfOperStatusLowerLayerDown,
        ] {
            assert_eq!(admin_from_oper(status), AdminState::Down, "{status}");
        }
    }
}
