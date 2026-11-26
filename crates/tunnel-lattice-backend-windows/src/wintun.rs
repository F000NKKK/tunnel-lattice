//! Private Wintun loader and resource ownership; packet I/O is not yet wired.

use std::ffi::c_void;
use std::io;
use std::path::Path;
use std::ptr::{self, NonNull};
use std::sync::Arc;

use libloading::Library;

use crate::contract::{valid_ring_capacity, wide_name};
use crate::lifecycle::{Adapter, Lifecycle, Session};
use crate::packet::PacketApi;

type CreateAdapter =
    unsafe extern "system" fn(*const u16, *const u16, *const c_void) -> *mut c_void;
type OpenAdapter = unsafe extern "system" fn(*const u16) -> *mut c_void;
type CloseAdapter = unsafe extern "system" fn(*mut c_void);
type StartSession = unsafe extern "system" fn(*mut c_void, u32) -> *mut c_void;
type EndSession = unsafe extern "system" fn(*mut c_void);
type ReceivePacket = unsafe extern "system" fn(*mut c_void, *mut u32) -> *mut u8;
type ReleaseReceivePacket = unsafe extern "system" fn(*mut c_void, *const u8);
type AllocateSendPacket = unsafe extern "system" fn(*mut c_void, u32) -> *mut u8;
type SendPacket = unsafe extern "system" fn(*mut c_void, *const u8);

/// Keeps the DLL loaded while any adapter or session may call its functions.
struct Api {
    create_adapter: CreateAdapter,
    open_adapter: OpenAdapter,
    close_adapter: CloseAdapter,
    start_session: StartSession,
    end_session: EndSession,
    receive_packet: ReceivePacket,
    release_receive_packet: ReleaseReceivePacket,
    allocate_send_packet: AllocateSendPacket,
    send_packet: SendPacket,
    _library: Library,
}

impl Api {
    /// Load only an explicitly identified, trusted Wintun DLL.
    ///
    /// # Safety
    /// The caller must ensure the path identifies a trusted DLL implementing
    /// the Wintun 0.14.1 ABI, and that loading all of its transitive DLL
    /// dependencies is safe and trusted too. An absolute path does not by
    /// itself restrict the Windows search path for dependencies. Loading and
    /// unloading execute initialization and termination code, respectively;
    /// both must be safe in the caller's process.
    unsafe fn load(path: &Path) -> io::Result<Arc<Self>> {
        if !path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Wintun path must be absolute",
            ));
        }
        // SAFETY: The caller vouches for the DLL and its initialization code.
        let library = unsafe { Library::new(path) }.map_err(io::Error::other)?;
        // SAFETY: The caller guarantees these exported names have the official
        // Wintun signatures; Api retains the DLL until after all uses end.
        unsafe {
            Ok(Arc::new(Self {
                create_adapter: *library
                    .get(b"WintunCreateAdapter\0")
                    .map_err(io::Error::other)?,
                open_adapter: *library
                    .get(b"WintunOpenAdapter\0")
                    .map_err(io::Error::other)?,
                close_adapter: *library
                    .get(b"WintunCloseAdapter\0")
                    .map_err(io::Error::other)?,
                start_session: *library
                    .get(b"WintunStartSession\0")
                    .map_err(io::Error::other)?,
                end_session: *library
                    .get(b"WintunEndSession\0")
                    .map_err(io::Error::other)?,
                receive_packet: *library
                    .get(b"WintunReceivePacket\0")
                    .map_err(io::Error::other)?,
                release_receive_packet: *library
                    .get(b"WintunReleaseReceivePacket\0")
                    .map_err(io::Error::other)?,
                allocate_send_packet: *library
                    .get(b"WintunAllocateSendPacket\0")
                    .map_err(io::Error::other)?,
                send_packet: *library
                    .get(b"WintunSendPacket\0")
                    .map_err(io::Error::other)?,
                _library: library,
            }))
        }
    }
}

// SAFETY: a caller of Api::load guarantees that the DLL implements Wintun
// 0.14.1, including buffer lifetime and send/receive ownership semantics.
unsafe impl PacketApi for Api {
    unsafe fn receive(&self, session: NonNull<c_void>) -> io::Result<(NonNull<u8>, usize)> {
        let mut size = 0u32;
        // SAFETY: caller provides a live session and size is writable.
        let packet = unsafe { (self.receive_packet)(session.as_ptr(), &mut size) };
        let packet = NonNull::new(packet).ok_or_else(io::Error::last_os_error)?;
        Ok((packet, size as usize))
    }

    unsafe fn release(&self, session: NonNull<c_void>, packet: NonNull<u8>) {
        // SAFETY: caller owns this received packet for this live session.
        unsafe { (self.release_receive_packet)(session.as_ptr(), packet.as_ptr()) };
    }

    unsafe fn allocate(&self, session: NonNull<c_void>, len: u32) -> io::Result<NonNull<u8>> {
        // SAFETY: caller provides a live session and a validated length.
        let packet = unsafe { (self.allocate_send_packet)(session.as_ptr(), len) };
        NonNull::new(packet).ok_or_else(io::Error::last_os_error)
    }

    unsafe fn commit(&self, session: NonNull<c_void>, packet: NonNull<u8>) {
        // SAFETY: caller owns this allocated send packet for this session.
        unsafe { (self.send_packet)(session.as_ptr(), packet.as_ptr()) };
    }
}

impl Lifecycle for Api {
    unsafe fn close_adapter(&self, handle: NonNull<c_void>) {
        // SAFETY: forwarded from the Lifecycle contract.
        unsafe { (self.close_adapter)(handle.as_ptr()) };
    }
    unsafe fn end_session(&self, handle: NonNull<c_void>) {
        // SAFETY: forwarded from the Lifecycle contract.
        unsafe { (self.end_session)(handle.as_ptr()) };
    }
}

// WintunCloseAdapter removes an adapter created with WintunCreateAdapter;
// opening an existing adapter does not itself create one. Callers must not
// assume the two acquisition paths have identical lifecycle effects.
impl Adapter<Api> {
    fn create(api: Arc<Api>, name: &str, tunnel_type: &str) -> io::Result<Arc<Self>> {
        let name = wide_name(name)?;
        let tunnel_type = wide_name(tunnel_type)?;
        // SAFETY: Api guarantees the ABI and retains the DLL; strings are
        // valid NUL-terminated UTF-16 for the duration of this call.
        let handle =
            unsafe { (api.create_adapter)(name.as_ptr(), tunnel_type.as_ptr(), ptr::null()) };
        let handle = NonNull::new(handle).ok_or_else(io::Error::last_os_error)?;
        // SAFETY: Wintun returned a fresh owned adapter handle.
        Ok(unsafe { Self::from_handle(api, handle) })
    }

    fn open(api: Arc<Api>, name: &str) -> io::Result<Arc<Self>> {
        let name = wide_name(name)?;
        // SAFETY: Api retains the DLL and name is valid NUL-terminated UTF-16.
        let handle = unsafe { (api.open_adapter)(name.as_ptr()) };
        let handle = NonNull::new(handle).ok_or_else(io::Error::last_os_error)?;
        // SAFETY: Wintun returned a fresh owned adapter handle.
        Ok(unsafe { Self::from_handle(api, handle) })
    }
}

impl Session<Api> {
    fn start(adapter: Arc<Adapter<Api>>, capacity: u32) -> io::Result<Self> {
        if !valid_ring_capacity(capacity) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid Wintun ring capacity",
            ));
        }
        // SAFETY: The Arc keeps the adapter and DLL live during and after
        // this call, and capacity meets Wintun's documented constraints.
        let handle = unsafe { (adapter.api.start_session)(adapter.handle.as_ptr(), capacity) };
        let handle = NonNull::new(handle).ok_or_else(io::Error::last_os_error)?;
        // SAFETY: Wintun returned a fresh session for this adapter.
        Ok(unsafe { Self::from_handle(adapter, handle) })
    }
}
