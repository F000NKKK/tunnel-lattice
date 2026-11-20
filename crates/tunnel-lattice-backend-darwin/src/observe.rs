//! Darwin interface observation and control for an open utun.

use std::io;

/// `ifreq.ifr_name` is IFNAMSIZ (16) on Darwin, including the NUL.
fn checked_name(name: &str) -> io::Result<[u8; 16]> {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes.len() >= 16 || bytes.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid interface name",
        ));
    }
    let mut result = [0; 16];
    result[..bytes.len()].copy_from_slice(bytes);
    Ok(result)
}

#[cfg(target_os = "macos")]
use std::{
    ffi::CString,
    mem,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
};

#[cfg(target_os = "macos")]
fn request(name: &[u8; 16]) -> libc::ifreq {
    // SAFETY: ifreq is a C struct and a zeroed union is valid.
    let mut value: libc::ifreq = unsafe { mem::zeroed() };
    for (dst, src) in value.ifr_name.iter_mut().zip(name) {
        *dst = *src as libc::c_char;
    }
    value
}

#[cfg(target_os = "macos")]
fn control_socket() -> io::Result<OwnedFd> {
    // SAFETY: socket has no pointer arguments and returns a new descriptor.
    let raw = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: raw is a fresh valid descriptor with no other owner.
    let socket = unsafe { OwnedFd::from_raw_fd(raw) };
    // SAFETY: fcntl acts on the owned descriptor and has no pointer argument.
    if unsafe { libc::fcntl(socket.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(socket)
}

/// Preserve unrelated flags: enabling follows tun-rs and sets both UP and
/// RUNNING; disabling clears UP only.
fn desired_flags(flags: u16, up: bool) -> u16 {
    let up_flag = tunnel_lattice_model::backend::IFF_UP as u16;
    let running_flag = tunnel_lattice_model::backend::IFF_RUNNING as u16;
    if up {
        flags | up_flag | running_flag
    } else {
        flags & !up_flag
    }
}

/// Returns the nonzero interface index captured at open time.
#[cfg(target_os = "macos")]
pub(crate) fn index(name: &str) -> io::Result<u32> {
    checked_name(name)?;
    let c_name = CString::new(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid interface name"))?;
    // SAFETY: c_name remains a valid NUL-terminated string during the call.
    let index = unsafe { libc::if_nametoindex(c_name.as_ptr()) };
    if index == 0 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(0) {
            return Err(io::ErrorKind::NotFound.into());
        }
        return Err(err);
    }
    Ok(index)
}

/// Reads MTU and interface flags using the same ioctls as the tun-rs backend.
#[cfg(target_os = "macos")]
pub(crate) fn state(name: &str) -> io::Result<(u32, tunnel_lattice_model::AdminState)> {
    let name = checked_name(name)?;
    let socket = control_socket()?;
    // Darwin _IOWR('i', 51/17, struct ifreq), as used by tun-rs.
    const SIOCGIFMTU: libc::c_ulong = 0xc020_6933;
    const SIOCGIFFLAGS: libc::c_ulong = 0xc020_6911;
    let mut mtu_req = request(&name);
    // SAFETY: the socket is open and mtu_req is a writable named ifreq.
    if unsafe { libc::ioctl(socket.as_raw_fd(), SIOCGIFMTU, &mut mtu_req) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the successful ioctl initialized ifru_mtu.
    let mtu = unsafe { mtu_req.ifr_ifru.ifru_mtu };
    let mtu = u16::try_from(mtu)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid interface MTU"))?;
    let mut flags_req = request(&name);
    // SAFETY: the socket is open and flags_req is a writable named ifreq.
    if unsafe { libc::ioctl(socket.as_raw_fd(), SIOCGIFFLAGS, &mut flags_req) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the successful ioctl initialized ifru_flags.
    let flags = unsafe { flags_req.ifr_ifru.ifru_flags } as u16 as u32;
    Ok((
        u32::from(mtu),
        tunnel_lattice_model::backend::admin_from_if_flags(flags),
    ))
}

/// Sets MTU on the named interface, using Darwin SIOCSIFMTU.
#[cfg(target_os = "macos")]
pub(crate) fn set_mtu(name: &str, mtu: u16) -> io::Result<()> {
    const SIOCSIFMTU: libc::c_ulong = 0x8020_6934;
    let name = checked_name(name)?;
    let socket = control_socket()?;
    let mut req = request(&name);
    req.ifr_ifru.ifru_mtu = i32::from(mtu);
    // SAFETY: socket and the named ifreq with ifru_mtu remain valid.
    if unsafe { libc::ioctl(socket.as_raw_fd(), SIOCSIFMTU, &req) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Sets the administrative flag after reading the current flags.
#[cfg(target_os = "macos")]
pub(crate) fn set_up(name: &str, up: bool) -> io::Result<()> {
    const SIOCGIFFLAGS: libc::c_ulong = 0xc020_6911;
    const SIOCSIFFLAGS: libc::c_ulong = 0x8020_6910;
    let name = checked_name(name)?;
    let socket = control_socket()?;
    let mut req = request(&name);
    // SAFETY: socket is open and req is a writable named ifreq.
    if unsafe { libc::ioctl(socket.as_raw_fd(), SIOCGIFFLAGS, &mut req) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful SIOCGIFFLAGS initialized ifru_flags.
    let flags = unsafe { req.ifr_ifru.ifru_flags } as u16;
    req.ifr_ifru.ifru_flags = desired_flags(flags, up) as libc::c_short;
    // SAFETY: socket and named ifreq with initialized flags remain valid.
    if unsafe { libc::ioctl(socket.as_raw_fd(), SIOCSIFFLAGS, &req) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interface_names_fit_and_are_nul_terminated() {
        assert_eq!(&checked_name("utun42").unwrap()[..7], b"utun42\0");
        assert!(checked_name("").is_err());
        assert!(checked_name("utun\0x").is_err());
        assert!(checked_name("1234567890123456").is_err());
        assert_eq!(checked_name("123456789012345").unwrap()[15], 0);
    }

    #[test]
    fn changing_admin_flags_preserves_other_flags() {
        use tunnel_lattice_model::backend::{IFF_RUNNING, IFF_UP};
        let other = 0x2000;
        assert_eq!(
            desired_flags(other, true),
            other | IFF_UP as u16 | IFF_RUNNING as u16
        );
        assert_eq!(
            desired_flags(other | IFF_UP as u16 | IFF_RUNNING as u16, false),
            other | IFF_RUNNING as u16
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn ioctl_constants_match_darwin_ifreq_layout() {
        let iowr = |n: u64| {
            0xc000_0000_u64
                | (std::mem::size_of::<libc::ifreq>() as u64) << 16
                | (b'i' as u64) << 8
                | n
        };
        assert_eq!(iowr(51), 0xc020_6933);
        assert_eq!(iowr(17), 0xc020_6911);
        let iow = |n: u64| {
            0x8000_0000_u64
                | (std::mem::size_of::<libc::ifreq>() as u64) << 16
                | (b'i' as u64) << 8
                | n
        };
        assert_eq!(iow(52), 0x8020_6934);
        assert_eq!(iow(16), 0x8020_6910);
    }
}
