//! Nonblocking Wintun packet operations and receive-buffer ownership.
//!
//! This module does not wait on the read event and has no async cancellation
//! contract. Send allocation, copy and commit are one synchronous operation:
//! Wintun has no API to abandon a successfully allocated send packet.

use std::ffi::c_void;
use std::io;
use std::ptr::NonNull;

use crate::lifecycle::{Lifecycle, Session};

const MAX_PACKET: usize = 0xffff;
// Win32 error values documented by the Wintun 0.14.1 API.
const ERROR_INVALID_DATA: i32 = 13;
const ERROR_HANDLE_EOF: i32 = 38;
const ERROR_BUFFER_OVERFLOW: i32 = 111;
const ERROR_NO_MORE_ITEMS: i32 = 259;

/// Native implementation of Wintun's packet-buffer contract.
///
/// # Safety
/// Successful receives/allocations must return distinct live buffers of the
/// reported/requested lengths, valid until release/commit with that session.
/// A received buffer must not be mutated by the API until it is released.
/// No buffer may alias a Rust slice passed to these safe methods. Release and
/// commit must not unwind. The session stays live while its borrowed guard or
/// synchronous send exists.
pub(crate) unsafe trait PacketApi: Lifecycle {
    unsafe fn receive(&self, session: NonNull<c_void>) -> io::Result<(NonNull<u8>, usize)>;
    unsafe fn release(&self, session: NonNull<c_void>, packet: NonNull<u8>);
    unsafe fn allocate(&self, session: NonNull<c_void>, len: u32) -> io::Result<NonNull<u8>>;
    unsafe fn commit(&self, session: NonNull<c_void>, packet: NonNull<u8>);
}

#[derive(Debug)]
pub(crate) enum ReceiveError {
    BufferTooSmall { needed: usize },
    InvalidPacketLength,
    NoData,
    Disconnected,
    CorruptRing,
    Io(io::Error),
}

#[derive(Debug)]
pub(crate) enum SendError {
    InvalidPacketLength,
    RingFull,
    Disconnected,
    Io(io::Error),
}

fn receive_error(error: io::Error) -> ReceiveError {
    match error.raw_os_error() {
        Some(ERROR_NO_MORE_ITEMS) => ReceiveError::NoData,
        Some(ERROR_HANDLE_EOF) => ReceiveError::Disconnected,
        Some(ERROR_INVALID_DATA) => ReceiveError::CorruptRing,
        _ => ReceiveError::Io(error),
    }
}

fn send_error(error: io::Error) -> SendError {
    match error.raw_os_error() {
        Some(ERROR_BUFFER_OVERFLOW) => SendError::RingFull,
        Some(ERROR_HANDLE_EOF) => SendError::Disconnected,
        _ => SendError::Io(error),
    }
}

/// Releases a packet on every exit, including errors and unwinding.
pub(crate) struct Received<'a, A: PacketApi> {
    session: &'a Session<A>,
    ptr: NonNull<u8>,
    len: usize,
}

impl<A: PacketApi> Received<'_, A> {
    /// Borrows the packet in place while this guard keeps the ring slot alive.
    pub(crate) fn as_slice(&self) -> &[u8] {
        // SAFETY: receive and the guard establish that this validated buffer
        // remains readable and unmodified until the guard is dropped.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl<A: PacketApi> Drop for Received<'_, A> {
    fn drop(&mut self) {
        // SAFETY: the pointer came from this live session's receive call and
        // this guard is the only owner that releases it.
        unsafe { self.session.api().release(self.session.handle(), self.ptr) };
    }
}

impl<A: PacketApi> Session<A> {
    /// Attempts one receive without waiting; the returned guard allows a
    /// zero-copy view and retains the slot until the consumer drops it.
    pub(crate) fn try_receive(&self) -> Result<Received<'_, A>, ReceiveError> {
        // SAFETY: self owns a live session; the native API's safety contract
        // guarantees a valid buffer on success.
        let (ptr, len) = unsafe { self.api().receive(self.handle()) }.map_err(receive_error)?;
        let packet = Received {
            session: self,
            ptr,
            len,
        };
        if packet.len > MAX_PACKET {
            return Err(ReceiveError::InvalidPacketLength);
        }
        Ok(packet)
    }

    /// Attempts one receive without waiting. An oversized packet is consumed
    /// and released, and never partially copied to `out`.
    pub(crate) fn try_receive_into(&self, out: &mut [u8]) -> Result<usize, ReceiveError> {
        let packet = self.try_receive()?;
        if packet.len > out.len() {
            return Err(ReceiveError::BufferTooSmall { needed: packet.len });
        }
        out[..packet.len].copy_from_slice(packet.as_slice());
        Ok(packet.len)
    }

    /// Copies and commits a packet without exposing a cancellable allocation.
    pub(crate) fn try_send(&self, packet: &[u8]) -> Result<usize, SendError> {
        if packet.len() > MAX_PACKET {
            return Err(SendError::InvalidPacketLength);
        }
        // SAFETY: length fits u32, self owns a live session and PacketApi
        // guarantees a writable, non-aliased buffer on success.
        let ptr = unsafe { self.api().allocate(self.handle(), packet.len() as u32) }
            .map_err(send_error)?;
        // SAFETY: both buffers are valid for packet.len and do not overlap.
        // No fallible operation or await occurs between allocation and commit.
        unsafe {
            std::ptr::copy_nonoverlapping(packet.as_ptr(), ptr.as_ptr(), packet.len());
            self.api().commit(self.handle(), ptr);
        }
        Ok(packet.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::Adapter;
    use std::sync::{Arc, Mutex};

    struct State {
        incoming: Box<[u8]>,
        outstanding: bool,
        outgoing: Box<[u8]>,
        released: usize,
        committed: Vec<Vec<u8>>,
        receive_error: Option<i32>,
        allocate_error: Option<i32>,
    }

    struct Fake(Mutex<State>);

    impl Lifecycle for Fake {
        unsafe fn close_adapter(&self, _: NonNull<c_void>) {}
        unsafe fn end_session(&self, _: NonNull<c_void>) {}
    }

    // SAFETY: boxes remain allocated and unmoved for each operation; tests
    // neither replace nor alias them while the native buffer is borrowed.
    unsafe impl PacketApi for Fake {
        unsafe fn receive(&self, _: NonNull<c_void>) -> io::Result<(NonNull<u8>, usize)> {
            let mut state = self.0.lock().unwrap();
            if let Some(code) = state.receive_error {
                return Err(io::Error::from_raw_os_error(code));
            }
            if state.outstanding {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            state.outstanding = true;
            let len = state.incoming.len();
            Ok((NonNull::new(state.incoming.as_mut_ptr()).unwrap(), len))
        }
        unsafe fn release(&self, _: NonNull<c_void>, _: NonNull<u8>) {
            let mut state = self.0.lock().unwrap();
            state.released += 1;
            state.outstanding = false;
        }
        unsafe fn allocate(&self, _: NonNull<c_void>, len: u32) -> io::Result<NonNull<u8>> {
            let mut state = self.0.lock().unwrap();
            if let Some(code) = state.allocate_error {
                return Err(io::Error::from_raw_os_error(code));
            }
            state.outgoing = vec![0; len as usize].into_boxed_slice();
            Ok(NonNull::new(state.outgoing.as_mut_ptr()).unwrap())
        }
        unsafe fn commit(&self, _: NonNull<c_void>, _: NonNull<u8>) {
            let mut state = self.0.lock().unwrap();
            let packet = state.outgoing.to_vec();
            state.committed.push(packet);
        }
    }

    fn fixture(incoming: Vec<u8>) -> (Arc<Fake>, Session<Fake>) {
        let api = Arc::new(Fake(Mutex::new(State {
            incoming: incoming.into_boxed_slice(),
            outstanding: false,
            outgoing: Box::default(),
            released: 0,
            committed: Vec::new(),
            receive_error: None,
            allocate_error: None,
        })));
        let handle = NonNull::dangling();
        // SAFETY: fake methods do not dereference these sentinel handles.
        let adapter = unsafe { Adapter::from_handle(api.clone(), handle) };
        // SAFETY: fake methods do not dereference these sentinel handles.
        let session = unsafe { Session::from_handle(adapter, handle) };
        (api, session)
    }

    #[test]
    fn receive_copies_full_packet_and_releases_once() {
        let (api, session) = fixture(vec![1, 2, 3]);
        let mut out = [0; 4];
        assert_eq!(session.try_receive_into(&mut out).unwrap(), 3);
        assert_eq!(out, [1, 2, 3, 0]);
        assert_eq!(api.0.lock().unwrap().released, 1);
    }

    #[test]
    fn zero_copy_view_retains_slot_until_guard_drop() {
        let (api, session) = fixture(vec![4, 5, 6]);
        let guard = session.try_receive().unwrap();
        assert_eq!(guard.as_slice(), &[4, 5, 6]);
        assert_eq!(api.0.lock().unwrap().released, 0);
        drop(guard);
        assert_eq!(api.0.lock().unwrap().released, 1);
    }

    #[test]
    fn receive_too_small_drops_packet_without_partial_copy() {
        let (api, session) = fixture(vec![1, 2, 3]);
        let mut out = [9; 2];
        assert!(matches!(
            session.try_receive_into(&mut out),
            Err(ReceiveError::BufferTooSmall { needed: 3 })
        ));
        assert_eq!(out, [9; 2]);
        assert_eq!(api.0.lock().unwrap().released, 1);
    }

    #[test]
    fn invalid_native_length_is_released_and_never_copied() {
        let (api, session) = fixture(vec![1; MAX_PACKET + 1]);
        let mut out = vec![9; MAX_PACKET + 1];
        assert!(matches!(
            session.try_receive_into(&mut out),
            Err(ReceiveError::InvalidPacketLength)
        ));
        assert!(out.iter().all(|&b| b == 9));
        assert_eq!(api.0.lock().unwrap().released, 1);
    }

    #[test]
    fn receive_guard_releases_during_unwind() {
        let (api, session) = fixture(vec![1]);
        let (ptr, len) = unsafe { api.receive(session.handle()) }.unwrap();
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _packet = Received {
                session: &session,
                ptr,
                len,
            };
            panic!("simulated consumer panic");
        }));
        assert!(caught.is_err());
        assert_eq!(api.0.lock().unwrap().released, 1);
    }

    #[test]
    fn documented_native_errors_are_classified_without_losing_unknown_codes() {
        let (api, session) = fixture(vec![1]);
        api.0.lock().unwrap().receive_error = Some(ERROR_NO_MORE_ITEMS);
        assert!(matches!(
            session.try_receive_into(&mut [0]),
            Err(ReceiveError::NoData)
        ));
        api.0.lock().unwrap().receive_error = Some(ERROR_HANDLE_EOF);
        assert!(matches!(
            session.try_receive_into(&mut [0]),
            Err(ReceiveError::Disconnected)
        ));
        api.0.lock().unwrap().receive_error = Some(ERROR_INVALID_DATA);
        assert!(matches!(
            session.try_receive_into(&mut [0]),
            Err(ReceiveError::CorruptRing)
        ));
        api.0.lock().unwrap().receive_error = Some(9999);
        assert!(
            matches!(session.try_receive_into(&mut [0]), Err(ReceiveError::Io(e)) if e.raw_os_error() == Some(9999))
        );
        api.0.lock().unwrap().allocate_error = Some(ERROR_BUFFER_OVERFLOW);
        assert!(matches!(session.try_send(&[1]), Err(SendError::RingFull)));
        api.0.lock().unwrap().allocate_error = Some(ERROR_HANDLE_EOF);
        assert!(matches!(
            session.try_send(&[1]),
            Err(SendError::Disconnected)
        ));
        api.0.lock().unwrap().allocate_error = Some(9999);
        assert!(
            matches!(session.try_send(&[1]), Err(SendError::Io(e)) if e.raw_os_error() == Some(9999))
        );
        let state = api.0.lock().unwrap();
        assert_eq!(state.released, 0);
        assert!(state.committed.is_empty());
    }

    #[test]
    fn send_commits_exact_bytes_once_and_rejects_oversize_before_allocation() {
        let (api, session) = fixture(vec![1]);
        assert_eq!(session.try_send(&[3, 2, 1]).unwrap(), 3);
        assert!(matches!(
            session.try_send(&vec![0; MAX_PACKET + 1]),
            Err(SendError::InvalidPacketLength)
        ));
        assert_eq!(api.0.lock().unwrap().committed, [vec![3, 2, 1]]);
    }
}
