//! Private feth pair lifecycle for the future Darwin TAP backend.
//!
//! Pairs are created by the kernel with automatic names; we never adopt an
//! existing interface. Linking uses XNU's driver-specific feth request, not a
//! shell command. No TAP I/O or public provider is exposed by this module yet.

use std::ffi::CStr;
use std::io;
use std::mem;
use std::num::NonZeroU32;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::Arc;

// XNU sockio.h: _IOWR('i', 120, ifreq), _IOW('i', 121, ifreq),
// _IOW('i', 123, ifdrv). All supported macOS targets are 64-bit.
const SIOCIFCREATE: libc::c_ulong = 0xc020_6978;
const SIOCIFDESTROY: libc::c_ulong = 0x8020_6979;
const SIOCSDRVSPEC: libc::c_ulong = 0x8028_697b;
const SET_PEER: libc::c_ulong = 1;

// XNU net/if_fake_var.h: four reserved u64 values followed by a 128-byte
// union whose first member is the peer name. Reserved bytes must be zero.
#[repr(C)]
struct FakeRequest {
    reserved: [u64; 4],
    data: [u8; 128],
}

// XNU net/if.h packs ifdrv to a maximum alignment of four bytes.
#[repr(C, packed(4))]
struct IfDrv {
    name: [libc::c_char; libc::IFNAMSIZ],
    command: libc::c_ulong,
    length: usize,
    data: *mut libc::c_void,
}

fn request(name: &[u8]) -> io::Result<libc::ifreq> {
    if name.is_empty() || name.len() >= libc::IFNAMSIZ || name.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid feth name",
        ));
    }
    // SAFETY: ifreq is a C structure and all-zero is valid for its union.
    let mut req: libc::ifreq = unsafe { mem::zeroed() };
    for (dst, src) in req.ifr_name.iter_mut().zip(name) {
        *dst = *src as libc::c_char;
    }
    Ok(req)
}

fn control_socket() -> io::Result<OwnedFd> {
    // SAFETY: socket has no pointer arguments and returns a new descriptor.
    let raw = unsafe { libc::socket(libc::AF_NDRV, libc::SOCK_RAW, 0) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: raw is a newly created, uniquely owned descriptor.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    // SAFETY: fcntl acts on the open owned descriptor, with no pointer arg.
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

/// An interface created by this process. Cleanup never intentionally adopts
/// or destroys an interface that was present before the create call.
struct Feth {
    name: [u8; libc::IFNAMSIZ],
    index: NonZeroU32,
    control: Arc<OwnedFd>,
}

impl Feth {
    fn create(control: Arc<OwnedFd>) -> io::Result<Self> {
        let mut req = request(b"feth")?;
        // SAFETY: req is a writable ifreq and control is an open socket.
        if unsafe { libc::ioctl(control.as_raw_fd(), SIOCIFCREATE, &mut req) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut name = [0u8; libc::IFNAMSIZ];
        for (dst, src) in name.iter_mut().zip(req.ifr_name) {
            *dst = src as u8;
        }
        let c_name = CStr::from_bytes_until_nul(&name)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid kernel feth name"))?;
        if !c_name.to_bytes().starts_with(b"feth") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid kernel feth name",
            ));
        }
        // SAFETY: c_name is NUL-terminated and lives through the call.
        let index = unsafe { libc::if_nametoindex(c_name.as_ptr()) };
        let index = NonZeroU32::new(index).ok_or_else(|| {
            let source = io::Error::last_os_error();
            io::Error::new(
                source.kind(),
                format!(
                    "created {} but cannot verify its index; manual cleanup may be needed: {source}",
                    c_name.to_string_lossy()
                ),
            )
        })?;
        Ok(Self {
            name,
            index,
            control,
        })
    }

    fn name(&self) -> &CStr {
        // The constructor checked this exact immutable byte array.
        CStr::from_bytes_until_nul(&self.name).expect("validated feth name")
    }
}

impl Drop for Feth {
    fn drop(&mut self) {
        // Do not delete an interface if its name now refers to another index.
        // The name/index check and ioctl cannot be atomic against other
        // processes; cleanup is best effort, as with the backend contract.
        // SAFETY: the stored C string remains valid for the duration of call.
        let current = unsafe { libc::if_nametoindex(self.name().as_ptr()) };
        if current != self.index.get() {
            return;
        }
        let Ok(req) = request(self.name().to_bytes()) else {
            return;
        };
        // SAFETY: req points to a valid ifreq and control is still owned.
        unsafe { libc::ioctl(self.control.as_raw_fd(), SIOCIFDESTROY, &req) };
    }
}

/// Owns both endpoints; dropping either guard destroys only its own endpoint.
pub(crate) struct FethPair {
    device: Feth,
    peer: Feth,
}

impl FethPair {
    pub(crate) fn create() -> io::Result<Self> {
        let control = Arc::new(control_socket()?);
        let device = Feth::create(Arc::clone(&control))?;
        let peer = Feth::create(Arc::clone(&control))?;
        let mut fake = FakeRequest {
            reserved: [0; 4],
            data: [0; 128],
        };
        let peer_name = peer.name().to_bytes_with_nul();
        fake.data[..peer_name.len()].copy_from_slice(peer_name);
        let mut name = [0 as libc::c_char; libc::IFNAMSIZ];
        for (dst, src) in name.iter_mut().zip(device.name) {
            *dst = src as libc::c_char;
        }
        let mut drv = IfDrv {
            name,
            command: SET_PEER,
            length: mem::size_of::<FakeRequest>(),
            data: (&mut fake as *mut FakeRequest).cast(),
        };
        // SAFETY: drv and the fake request it points to are initialized,
        // remain live during ioctl, and match the documented XNU structures.
        if unsafe { libc::ioctl(control.as_raw_fd(), SIOCSDRVSPEC, &mut drv) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { device, peer })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer_name(feth: &Feth) -> io::Result<Vec<u8>> {
        // XNU sockio.h: _IOWR('i', 123, struct ifdrv), get-peer cmd = 1.
        const SIOCGDRVSPEC: libc::c_ulong = 0xc028_697b;
        let mut fake = FakeRequest {
            reserved: [0; 4],
            data: [0; 128],
        };
        let mut name = [0 as libc::c_char; libc::IFNAMSIZ];
        for (dst, src) in name.iter_mut().zip(feth.name) {
            *dst = src as libc::c_char;
        }
        let mut drv = IfDrv {
            name,
            command: 1,
            length: mem::size_of::<FakeRequest>(),
            data: (&mut fake as *mut FakeRequest).cast(),
        };
        // SAFETY: drv and its writable request remain live for the ioctl.
        if unsafe { libc::ioctl(feth.control.as_raw_fd(), SIOCGDRVSPEC, &mut drv) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let peer = CStr::from_bytes_until_nul(&fake.data[..libc::IFNAMSIZ])
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid peer name"))?;
        Ok(peer.to_bytes().to_vec())
    }

    #[test]
    fn request_layouts_match_xnu_64_bit_abi() {
        assert_eq!(mem::size_of::<libc::ifreq>(), 32);
        assert_eq!(mem::size_of::<IfDrv>(), 40);
        assert_eq!(mem::size_of::<FakeRequest>(), 160);
        assert_eq!(mem::offset_of!(IfDrv, command), 16);
        assert_eq!(mem::offset_of!(IfDrv, length), 24);
        assert_eq!(mem::offset_of!(IfDrv, data), 32);
    }

    #[test]
    #[ignore = "creates a real feth pair and requires macOS privileges"]
    fn creates_links_and_destroys_two_feth_endpoints() {
        let pair = FethPair::create().expect("create and connect feth pair");
        let names = [pair.device.name().to_owned(), pair.peer.name().to_owned()];
        assert_ne!(names[0], names[1]);
        assert_ne!(pair.device.index, pair.peer.index);
        assert_eq!(peer_name(&pair.device).unwrap(), names[1].to_bytes());
        assert_eq!(peer_name(&pair.peer).unwrap(), names[0].to_bytes());
        drop(pair);
        for name in names {
            // SAFETY: name is an owned C string valid throughout the call.
            assert_eq!(unsafe { libc::if_nametoindex(name.as_ptr()) }, 0);
        }
    }
}
