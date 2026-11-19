//! Internal synchronous utun socket; not yet a platform provider.

use std::ffi::CStr;
use std::io::{self, IoSlice, IoSliceMut};
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use crate::utun_frame::{BufferTooSmall, address_family, valid_family};

const CONTROL_NAME: &[u8] = b"com.apple.net.utun_control\0";

/// Owns a kernel control socket; dropping it closes the utun interface.
pub(crate) struct Utun {
    fd: OwnedFd,
}

impl Utun {
    /// Opens a fresh automatically named utun interface.
    pub(crate) fn open() -> io::Result<Self> {
        // SAFETY: socket has no borrowed arguments and returns a new fd.
        let raw =
            unsafe { libc::socket(libc::PF_SYSTEM, libc::SOCK_DGRAM, libc::SYSPROTO_CONTROL) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: socket returned a fresh valid descriptor, owned only here.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        // SAFETY: fcntl operates on the owned descriptor, without pointer arguments.
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: ctl_info is a plain C struct for which all-zero is valid.
        let mut info: libc::ctl_info = unsafe { mem::zeroed() };
        for (dst, src) in info.ctl_name.iter_mut().zip(CONTROL_NAME) {
            *dst = *src as libc::c_char;
        }
        // SAFETY: info points to a writable ctl_info of the expected size.
        if unsafe { libc::ioctl(fd.as_raw_fd(), libc::CTLIOCGINFO, &mut info) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let addr = libc::sockaddr_ctl {
            sc_len: mem::size_of::<libc::sockaddr_ctl>() as u8,
            sc_family: libc::AF_SYSTEM as u8,
            ss_sysaddr: libc::AF_SYS_CONTROL as u16,
            sc_id: info.ctl_id,
            sc_unit: 0,
            sc_reserved: [0; 5],
        };
        // SAFETY: addr is a correctly initialized sockaddr_ctl of the given length.
        if unsafe {
            libc::connect(
                fd.as_raw_fd(),
                (&addr as *const libc::sockaddr_ctl).cast(),
                mem::size_of_val(&addr) as libc::socklen_t,
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { fd })
    }

    pub(crate) fn name(&self) -> io::Result<String> {
        let mut name = [0u8; 64];
        let mut len = name.len() as libc::socklen_t;
        // SAFETY: name and len are writable buffers of the advertised size.
        if unsafe {
            libc::getsockopt(
                self.fd.as_raw_fd(),
                libc::SYSPROTO_CONTROL,
                libc::UTUN_OPT_IFNAME,
                name.as_mut_ptr().cast(),
                &mut len,
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        let used = usize::try_from(len).unwrap_or(usize::MAX).min(name.len());
        let value = CStr::from_bytes_until_nul(&name[..used])
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid utun name"))?;
        value
            .to_str()
            .map(str::to_owned)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "non-UTF-8 utun name"))
    }

    /// Reads one IP packet without silently truncating it.
    pub(crate) fn recv(&self, out: &mut [u8]) -> io::Result<usize> {
        let mut family = [0u8; 4];
        let mut sentinel = [0u8; 1];
        let mut slices = [
            IoSliceMut::new(&mut family),
            IoSliceMut::new(&mut *out),
            IoSliceMut::new(&mut sentinel),
        ];
        let n = loop {
            // SAFETY: slices are live writable buffers and IoSliceMut has iovec layout.
            let n = unsafe {
                libc::readv(
                    self.fd.as_raw_fd(),
                    slices.as_mut_ptr().cast(),
                    slices.len() as i32,
                )
            };
            if n >= 0 {
                break n as usize;
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        };
        if n > out.len() + 4 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, BufferTooSmall));
        }
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "utun channel closed",
            ));
        }
        if n <= 4 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "empty or truncated utun frame",
            ));
        }
        if !valid_family(family, &out[..n - 4]) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "utun address family mismatch",
            ));
        }
        Ok(n - 4)
    }

    /// Writes one IP packet, prepending the address-family header.
    pub(crate) fn send(&self, packet: &[u8]) -> io::Result<usize> {
        let family = address_family(packet)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid IP packet"))?
            .to_be_bytes();
        let slices = [IoSlice::new(&family), IoSlice::new(packet)];
        let n = loop {
            // SAFETY: slices are valid readable buffers and IoSlice has iovec layout.
            let n = unsafe {
                libc::writev(
                    self.fd.as_raw_fd(),
                    slices.as_ptr().cast(),
                    slices.len() as i32,
                )
            };
            if n >= 0 {
                break n as usize;
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        };
        if n != packet.len() + 4 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "partial utun packet",
            ));
        }
        Ok(packet.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "opens a real utun interface and requires macOS privileges"]
    fn opens_and_closes_a_real_utun() {
        let dev = Utun::open().unwrap();
        assert!(dev.name().unwrap().starts_with("utun"));
    }
}
