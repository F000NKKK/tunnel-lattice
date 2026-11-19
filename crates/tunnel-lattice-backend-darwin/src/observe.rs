//! Read-only Darwin interface observation for an open utun.

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
    }
}
