//! The synchronous packet path of a plain (no virtio-net header) queue.
//!
//! The queue descriptor is non-blocking from creation (see [`crate::tun`]),
//! so every call tries the native read or write first and waits with
//! `poll` only when it reports `EAGAIN`. No lock, no reference count and no
//! async runtime is involved: a call is legal from any thread, inside or
//! outside a runtime.
//!
//! - **Receive** reads with `readv` into `[buf, 1-byte sentinel]`. Linux
//!   TUN/TAP truncates a packet that does not fit the read silently, so a
//!   packet longer than `buf` spills into the sentinel and the reported
//!   length exceeds `buf.len()`: the packet has then been consumed and is
//!   reported as `Error::BufferTooSmall`, and the queue stays usable.
//! - **Send** writes first; on `EAGAIN` it waits for `POLLOUT` and writes
//!   again. A write sends the whole packet or nothing.
//! - `EINTR`, from the read, the write or `poll`, is retried. Every other
//!   failure is classified from its raw `errno` (see [`crate::errno`]).
//!
//! `poll` always reports `POLLERR`/`POLLHUP`, whatever was asked for: once
//! the device is deleted the kernel reports `POLLERR` on the queue, and the
//! next read fails with `EBADFD`, which ends a waiting receive with
//! `Error::Disconnected` instead of leaving it asleep. A device that is
//! only administratively down reports nothing, so a receive keeps waiting
//! until it is up and a packet arrives.

use std::ffi::c_int;
use std::io::IoSliceMut;
use std::os::fd::{AsRawFd, BorrowedFd};

use tunnel_lattice_core::{Error, Result};

use crate::errno::{self, Class, Op};
use crate::tun::last_errno;

/// Receives one packet into `buf`; see the module docs.
pub(crate) fn recv(fd: BorrowedFd<'_>, buf: &mut [u8]) -> Result<usize> {
    let mut sentinel = [0u8; 1];
    loop {
        let read = {
            let mut iov = [IoSliceMut::new(buf), IoSliceMut::new(&mut sentinel)];
            readv(fd, &mut iov)
        };
        let code = match read {
            Ok(len) if len > buf.len() => return Err(Error::BufferTooSmall),
            Ok(len) => return Ok(len),
            Err(code) => code,
        };
        match errno::classify(code, Op::Recv) {
            Class::Retry => {}
            Class::WouldBlock => wait(fd, libc::POLLIN)?,
            Class::Fail(error) => return Err(error),
            _ => return Err(errno::platform(code)),
        }
    }
}

/// Sends one packet from `buf`; see the module docs.
pub(crate) fn send(fd: BorrowedFd<'_>, buf: &[u8]) -> Result<usize> {
    loop {
        let code = match write(fd, buf) {
            Ok(len) => return Ok(len),
            Err(code) => code,
        };
        match errno::classify(code, Op::Send) {
            Class::Retry => {}
            Class::WouldBlock => wait(fd, libc::POLLOUT)?,
            Class::Fail(error) => return Err(error),
            _ => return Err(errno::platform(code)),
        }
    }
}

/// Waits, with no timeout, until `fd` reports `events` or an error or
/// hang-up condition. The caller retries its call afterwards and learns
/// the outcome from it.
pub(crate) fn wait(fd: BorrowedFd<'_>, events: libc::c_short) -> Result<()> {
    let mut poll_fd = libc::pollfd {
        fd: fd.as_raw_fd(),
        events,
        revents: 0,
    };
    loop {
        // SAFETY: `poll_fd` is one live, writable `pollfd`, and the count
        // passed is 1; the kernel writes only its `revents`. `fd` stays
        // open for the duration of the call.
        let rc = unsafe { libc::poll(&raw mut poll_fd, 1, -1) };
        if rc >= 0 {
            return Ok(());
        }
        let code = last_errno();
        if code != libc::EINTR {
            return Err(errno::platform(code));
        }
    }
}

/// One `readv(2)`: the byte count, or the call's `errno`.
fn readv(fd: BorrowedFd<'_>, iov: &mut [IoSliceMut<'_>]) -> std::result::Result<usize, i32> {
    let count = c_int::try_from(iov.len()).unwrap_or(c_int::MAX);
    // SAFETY: `IoSliceMut` is guaranteed ABI-compatible with `struct iovec`
    // on unix, and every element describes a live, writable buffer
    // borrowed for the duration of the call; `count` does not exceed
    // `iov.len()`. The kernel writes at most each element's length into
    // it. `fd` stays open for the duration of the call.
    let rc = unsafe { libc::readv(fd.as_raw_fd(), iov.as_mut_ptr().cast(), count) };
    usize::try_from(rc).map_err(|_| last_errno())
}

/// One `write(2)`: the byte count, or the call's `errno`.
fn write(fd: BorrowedFd<'_>, buf: &[u8]) -> std::result::Result<usize, i32> {
    // SAFETY: `buf` is a live, readable buffer of `buf.len()` bytes for the
    // duration of the call; the kernel only reads from it. `fd` stays open
    // for the duration of the call.
    let rc = unsafe { libc::write(fd.as_raw_fd(), buf.as_ptr().cast(), buf.len()) };
    usize::try_from(rc).map_err(|_| last_errno())
}

/// The loop is exercised over Unix datagram socket pairs, which share the
/// properties the TUN queue path relies on: one read returns one whole
/// packet or its truncated head, a non-blocking descriptor reports
/// `EAGAIN`, and `poll` reports readiness.
#[cfg(test)]
mod tests {
    use std::os::fd::AsFd;
    use std::os::unix::net::UnixDatagram;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use tunnel_lattice_core::PlatformErrorCode;

    use super::*;

    /// A connected, non-blocking datagram pair: (device side, peer side).
    fn pair() -> (UnixDatagram, UnixDatagram) {
        let (device, peer) = UnixDatagram::pair().unwrap();
        device.set_nonblocking(true).unwrap();
        peer.set_nonblocking(true).unwrap();
        (device, peer)
    }

    #[test]
    fn a_packet_that_fits_exactly_is_received_whole() {
        let (device, peer) = pair();
        peer.send(b"abcd").unwrap();
        let mut buf = [0xEE; 4];
        assert_eq!(recv(device.as_fd(), &mut buf).unwrap(), 4);
        assert_eq!(&buf, b"abcd");

        // A shorter packet reports only its own bytes; the rest of `buf`
        // keeps what was there.
        peer.send(b"xy").unwrap();
        assert_eq!(recv(device.as_fd(), &mut buf).unwrap(), 2);
        assert_eq!(&buf, b"xycd");
    }

    #[test]
    fn an_oversize_packet_is_discarded_and_the_next_one_is_received() {
        let (device, peer) = pair();
        peer.send(b"too long").unwrap();
        peer.send(b"ok").unwrap();
        let mut buf = [0u8; 4];
        // One byte too long is already oversize: it reaches the sentinel.
        assert!(recv(device.as_fd(), &mut buf).is_err_and(|e| e.is_buffer_too_small()));
        assert_eq!(recv(device.as_fd(), &mut buf).unwrap(), 2);
        assert_eq!(&buf[..2], b"ok");

        peer.send(b"abcde").unwrap();
        assert!(recv(device.as_fd(), &mut buf).is_err_and(|e| e.is_buffer_too_small()));
        // An empty buffer fits no packet at all.
        peer.send(b"a").unwrap();
        assert!(recv(device.as_fd(), &mut []).is_err_and(|e| e.is_buffer_too_small()));
    }

    #[test]
    fn an_empty_queue_waits_for_the_next_packet() {
        let (device, peer) = pair();
        let sender = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            peer.send(b"late").unwrap();
            peer
        });
        let mut buf = [0u8; 8];
        assert_eq!(recv(device.as_fd(), &mut buf).unwrap(), 4);
        assert_eq!(&buf[..4], b"late");
        drop(sender.join().unwrap());
    }

    #[test]
    fn a_full_queue_waits_until_the_packet_can_be_written_whole() {
        let (device, peer) = pair();
        // Fill the peer's receive queue until the device side would block.
        let packet = [0x5A; 512];
        let mut queued = 0usize;
        loop {
            match device.send(&packet) {
                Ok(_) => queued += 1,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("{error}"),
            }
        }
        assert!(queued > 0);
        let drainer = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            let mut buf = [0u8; 1024];
            let mut received = 0usize;
            peer.set_nonblocking(false).unwrap();
            peer.set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            // Every queued packet plus the one `send` waited to write.
            while received < queued + 1 {
                let len = peer.recv(&mut buf).unwrap();
                assert_eq!(len, packet.len());
                received += 1;
            }
            received
        });
        assert_eq!(send(device.as_fd(), &packet).unwrap(), packet.len());
        assert_eq!(drainer.join().unwrap(), queued + 1);
    }

    extern "C" fn ignore_signal(_: c_int) {}

    #[test]
    fn interrupted_waits_are_retried() {
        // A handler installed without `SA_RESTART` makes a signal end a
        // blocked `poll` with `EINTR` (which `poll` never restarts anyway).
        // SAFETY: `action` is fully initialized (zeroed, then the handler
        // and an empty mask set) before `sigaction` reads it, and the
        // handler is an `extern "C"` function that does nothing, so it is
        // async-signal-safe. Only `SIGUSR2` is changed, which nothing else
        // in this test binary uses.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = ignore_signal as extern "C" fn(c_int) as libc::sighandler_t;
            libc::sigemptyset(&raw mut action.sa_mask);
            assert_eq!(
                libc::sigaction(libc::SIGUSR2, &raw const action, std::ptr::null_mut()),
                0
            );
        }
        let (device, peer) = pair();
        let (ready, thread_id) = mpsc::channel();
        let receiver = thread::spawn(move || {
            // SAFETY: `pthread_self` has no preconditions.
            ready.send(unsafe { libc::pthread_self() }).unwrap();
            let mut buf = [0u8; 8];
            let len = recv(device.as_fd(), &mut buf);
            len.map(|len| buf[..len].to_vec())
        });
        let target = thread_id.recv().unwrap();
        for _ in 0..20 {
            // SAFETY: `target` is the receiver thread, which cannot finish
            // before the packet below is sent, so it is still alive; the
            // signal's handler does nothing.
            assert_eq!(unsafe { libc::pthread_kill(target, libc::SIGUSR2) }, 0);
            thread::sleep(Duration::from_millis(5));
        }
        peer.send(b"after").unwrap();
        assert_eq!(receiver.join().unwrap().unwrap(), b"after");
    }

    #[test]
    fn native_failures_keep_their_raw_errno() {
        let (reader, writer) = std::io::pipe().unwrap();
        let mut buf = [0u8; 4];
        // Reading the write end and writing the read end fail with `EBADF`.
        assert!(matches!(
            recv(writer.as_fd(), &mut buf),
            Err(Error::Platform(PlatformErrorCode::Linux(libc::EBADF)))
        ));
        assert!(matches!(
            send(reader.as_fd(), b"x"),
            Err(Error::Platform(PlatformErrorCode::Linux(libc::EBADF)))
        ));
    }

    #[test]
    fn a_wait_ends_on_a_hang_up_whatever_was_asked_for() {
        let (reader, writer) = std::io::pipe().unwrap();
        let closer = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            drop(writer);
        });
        // `POLLHUP` is always reported, so this does not wait forever even
        // though no `POLLIN` data ever arrives.
        wait(reader.as_fd(), libc::POLLIN).unwrap();
        closer.join().unwrap();
    }
}
