//! Runtime-independent readiness for nonblocking Linux packet queues.
//!
//! One process-wide epoll thread wakes futures on any executor. Queues are
//! registered lazily and independently; no executor or packet is retained
//! across an await. Each poll tries I/O first, registers its waker only on
//! EAGAIN, then retries I/O to close the registration/readiness race.

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};
use std::task::{Context, Poll, Waker};

use tunnel_lattice_core::{Error, PlatformErrorCode, Result};

use crate::io::{self as packet_io, Attempt};

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn native_error(error: io::Error) -> Error {
    match error.raw_os_error() {
        Some(libc::EACCES | libc::EPERM) => Error::PermissionDenied,
        Some(code) => Error::Platform(PlatformErrorCode::Linux(code)),
        None => Error::Platform(PlatformErrorCode::Unknown),
    }
}

fn error_code(code: i32) -> Error {
    native_error(io::Error::from_raw_os_error(code))
}

#[derive(Clone, Copy)]
enum Direction {
    Read,
    Write,
}

#[derive(Default)]
struct WaitState {
    failed: Option<i32>,
    readers: HashMap<u64, Waker>,
    writers: HashMap<u64, Waker>,
}

#[derive(Default)]
struct WaitSet {
    next: AtomicU64,
    state: Mutex<WaitState>,
}

impl WaitSet {
    fn next_id(&self) -> Result<u64> {
        self.next
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map_err(|_| Error::Platform(PlatformErrorCode::Unknown))
    }

    fn arm(&self, direction: Direction, id: u64, waker: &Waker) -> Result<()> {
        let mut state = lock(&self.state);
        if let Some(code) = state.failed {
            return Err(error_code(code));
        }
        let waiters = match direction {
            Direction::Read => &mut state.readers,
            Direction::Write => &mut state.writers,
        };
        waiters.insert(id, waker.clone());
        Ok(())
    }

    fn remove(&self, direction: Direction, id: u64) {
        let mut state = lock(&self.state);
        match direction {
            Direction::Read => &mut state.readers,
            Direction::Write => &mut state.writers,
        }
        .remove(&id);
    }

    fn wake(&self, events: u32, failure: Option<i32>) {
        let mut state = lock(&self.state);
        if let Some(code) = failure {
            state.failed = Some(code);
        }
        let error = (libc::EPOLLERR | libc::EPOLLHUP) as u32;
        let mut wakers = Vec::new();
        if failure.is_some() || events & (libc::EPOLLIN as u32 | error) != 0 {
            wakers.extend(state.readers.drain().map(|(_, w)| w));
        }
        if failure.is_some() || events & (libc::EPOLLOUT as u32 | error) != 0 {
            wakers.extend(state.writers.drain().map(|(_, w)| w));
        }
        drop(state);
        for waker in wakers {
            // A caller's custom Waker must not kill the shared driver.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| waker.wake()));
        }
    }
}

#[derive(Default)]
struct DriverState {
    failed: Option<i32>,
    queues: HashMap<u64, Weak<WaitSet>>,
}

struct Driver {
    epoll: OwnedFd,
    next: AtomicU64,
    state: Mutex<DriverState>,
}

impl Driver {
    fn start() -> io::Result<Arc<Self>> {
        // SAFETY: epoll_create1 has no pointer arguments and returns a new fd.
        let raw = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: raw is a fresh descriptor owned only by this object.
        let epoll = unsafe { OwnedFd::from_raw_fd(raw) };
        let driver = Arc::new(Self {
            epoll,
            next: AtomicU64::new(1),
            state: Mutex::new(DriverState::default()),
        });
        let worker = Arc::clone(&driver);
        std::thread::Builder::new()
            .name("tunnel-linux-epoll".into())
            .spawn(move || worker.run())?;
        Ok(driver)
    }

    fn shared() -> io::Result<Arc<Self>> {
        static DRIVER: OnceLock<Mutex<Option<Arc<Driver>>>> = OnceLock::new();
        let mut slot = lock(DRIVER.get_or_init(|| Mutex::new(None)));
        if let Some(driver) = slot.as_ref() {
            return Ok(Arc::clone(driver));
        }
        let driver = Self::start()?;
        *slot = Some(Arc::clone(&driver));
        Ok(driver)
    }

    fn register(self: &Arc<Self>, fd: BorrowedFd<'_>) -> Result<Registration> {
        let fd = fd.try_clone_to_owned().map_err(native_error)?;
        let token = self
            .next
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map_err(|_| Error::Platform(PlatformErrorCode::Unknown))?;
        let waiters = Arc::new(WaitSet::default());
        let mut state = lock(&self.state);
        if let Some(code) = state.failed {
            return Err(error_code(code));
        }
        state.queues.insert(token, Arc::downgrade(&waiters));
        let mut event = libc::epoll_event {
            events: (libc::EPOLLIN
                | libc::EPOLLOUT
                | libc::EPOLLERR
                | libc::EPOLLHUP
                | libc::EPOLLET) as u32,
            u64: token,
        };
        // SAFETY: both owned descriptors remain valid during the call and
        // event points to an initialized epoll_event. The queue duplicate is
        // removed from epoll before it is closed.
        if unsafe {
            libc::epoll_ctl(
                self.epoll.as_raw_fd(),
                libc::EPOLL_CTL_ADD,
                fd.as_raw_fd(),
                &mut event,
            )
        } < 0
        {
            state.queues.remove(&token);
            return Err(native_error(io::Error::last_os_error()));
        }
        drop(state);
        Ok(Registration {
            driver: Arc::clone(self),
            fd,
            token,
            waiters,
        })
    }

    fn run(&self) {
        let mut events = [libc::epoll_event { events: 0, u64: 0 }; 64];
        loop {
            // SAFETY: events is a live, writable array and epoll remains
            // owned by the shared driver for the duration of the thread.
            let count = unsafe {
                libc::epoll_wait(
                    self.epoll.as_raw_fd(),
                    events.as_mut_ptr(),
                    events.len() as i32,
                    -1,
                )
            };
            if count < 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                let code = error.raw_os_error().unwrap_or(libc::EIO);
                let mut state = lock(&self.state);
                state.failed = Some(code);
                let queues: Vec<_> = state.queues.values().filter_map(Weak::upgrade).collect();
                drop(state);
                for queue in queues {
                    queue.wake(0, Some(code));
                }
                return;
            }
            for event in &events[..count as usize] {
                // epoll_event is packed on Linux; copy the token before
                // borrowing it as a map key.
                let token = event.u64;
                let queue = lock(&self.state).queues.get(&token).and_then(Weak::upgrade);
                if let Some(queue) = queue {
                    queue.wake(event.events, None);
                }
            }
        }
    }
}

struct Registration {
    driver: Arc<Driver>,
    fd: OwnedFd,
    token: u64,
    waiters: Arc<WaitSet>,
}

impl Drop for Registration {
    fn drop(&mut self) {
        // SAFETY: both descriptors remain open; DEL ignores the event pointer.
        unsafe {
            libc::epoll_ctl(
                self.driver.epoll.as_raw_fd(),
                libc::EPOLL_CTL_DEL,
                self.fd.as_raw_fd(),
                std::ptr::null_mut(),
            );
        }
        lock(&self.driver.state).queues.remove(&self.token);
    }
}

/// One lazily registered queue. It never owns or depends on an async runtime.
#[derive(Default)]
pub(crate) struct Queue {
    registration: Mutex<Option<Arc<Registration>>>,
}

impl std::fmt::Debug for Queue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Queue").finish_non_exhaustive()
    }
}

impl Queue {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    fn registration(&self, fd: BorrowedFd<'_>) -> Result<Arc<Registration>> {
        let mut slot = lock(&self.registration);
        if let Some(registration) = slot.as_ref() {
            return Ok(Arc::clone(registration));
        }
        let driver = Driver::shared().map_err(native_error)?;
        let registration = Arc::new(driver.register(fd)?);
        *slot = Some(Arc::clone(&registration));
        Ok(registration)
    }

    pub(crate) async fn recv(&self, fd: BorrowedFd<'_>, buf: &mut [u8]) -> Result<usize> {
        IoFuture::new(self, fd, Operation::Read(buf)).await
    }

    pub(crate) async fn send(&self, fd: BorrowedFd<'_>, buf: &[u8]) -> Result<usize> {
        IoFuture::new(self, fd, Operation::Write(buf)).await
    }
}

enum Operation<'a> {
    Read(&'a mut [u8]),
    Write(&'a [u8]),
}

struct Waiter {
    set: Arc<WaitSet>,
    direction: Direction,
    id: u64,
}

impl Drop for Waiter {
    fn drop(&mut self) {
        self.set.remove(self.direction, self.id);
    }
}

struct IoFuture<'a> {
    queue: &'a Queue,
    fd: BorrowedFd<'a>,
    operation: Operation<'a>,
    waiter: Option<Waiter>,
}

impl<'a> IoFuture<'a> {
    fn new(queue: &'a Queue, fd: BorrowedFd<'a>, operation: Operation<'a>) -> Self {
        Self {
            queue,
            fd,
            operation,
            waiter: None,
        }
    }

    fn attempt(&mut self) -> Attempt {
        match &mut self.operation {
            Operation::Read(buf) => packet_io::try_recv(self.fd, buf),
            Operation::Write(buf) => packet_io::try_send(self.fd, buf),
        }
    }
}

impl Future for IoFuture<'_> {
    type Output = Result<usize>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Attempt::Ready(result) = self.attempt() {
            self.waiter.take();
            return Poll::Ready(result);
        }
        if self.waiter.is_none() {
            let registration = match self.queue.registration(self.fd) {
                Ok(registration) => registration,
                Err(error) => return Poll::Ready(Err(error)),
            };
            let id = match registration.waiters.next_id() {
                Ok(id) => id,
                Err(error) => return Poll::Ready(Err(error)),
            };
            let direction = match self.operation {
                Operation::Read(_) => Direction::Read,
                Operation::Write(_) => Direction::Write,
            };
            self.waiter = Some(Waiter {
                set: Arc::clone(&registration.waiters),
                direction,
                id,
            });
        }
        let waiter = self.waiter.as_ref().expect("waiter initialized");
        if let Err(error) = waiter.set.arm(waiter.direction, waiter.id, cx.waker()) {
            self.waiter.take();
            return Poll::Ready(Err(error));
        }
        // Close the race in which readiness arrived between the initial
        // attempt and waker registration. No await follows a successful I/O.
        match self.attempt() {
            Attempt::Ready(result) => {
                self.waiter.take();
                Poll::Ready(result)
            }
            Attempt::WouldBlock => Poll::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsFd;
    use std::os::unix::net::UnixDatagram;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Wake;
    use std::time::Duration;

    use super::*;

    fn pair() -> (UnixDatagram, UnixDatagram) {
        let (device, peer) = UnixDatagram::pair().unwrap();
        device.set_nonblocking(true).unwrap();
        peer.set_nonblocking(true).unwrap();
        (device, peer)
    }

    #[test]
    fn async_io_executor_wakes_and_cancelled_receive_keeps_packet() {
        let (device, peer) = pair();
        let queue = Queue::new();
        let mut first = [0u8; 8];
        let mut pending = Box::pin(queue.recv(device.as_fd(), &mut first));
        let waker = std::task::Waker::noop();
        let mut context = Context::from_waker(waker);
        assert!(matches!(pending.as_mut().poll(&mut context), Poll::Pending));
        drop(pending);
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            peer.send(b"packet").unwrap();
        });
        let mut second = [0u8; 8];
        let result = async_io::block_on(futures_lite::future::or(
            async { Some(queue.recv(device.as_fd(), &mut second).await) },
            async {
                async_io::Timer::after(Duration::from_secs(2)).await;
                None
            },
        ));
        assert_eq!(result.expect("async-io receive timed out").unwrap(), 6);
        writer.join().unwrap();
        assert_eq!(&second[..6], b"packet");
    }

    #[tokio::test]
    async fn cancelled_wait_removes_its_waker_and_keeps_the_next_packet() {
        let (device, peer) = pair();
        let queue = Queue::new();
        assert!(lock(&queue.registration).is_none());
        let mut buf = [0; 8];
        assert!(
            tokio::time::timeout(
                Duration::from_millis(30),
                queue.recv(device.as_fd(), &mut buf)
            )
            .await
            .is_err()
        );
        let registration = queue.registration(device.as_fd()).unwrap();
        assert!(lock(&registration.waiters.state).readers.is_empty());
        peer.send(b"packet").unwrap();
        assert_eq!(queue.recv(device.as_fd(), &mut buf).await.unwrap(), 6);
        assert_eq!(&buf[..6], b"packet");
    }

    #[tokio::test]
    async fn overflow_is_reported_once_and_the_next_packet_is_intact() {
        let (device, peer) = pair();
        let queue = Queue::new();
        peer.send(b"long").unwrap();
        peer.send(b"ok").unwrap();
        let mut buf = [0; 2];
        assert!(matches!(
            queue.recv(device.as_fd(), &mut buf).await,
            Err(Error::BufferTooSmall)
        ));
        assert_eq!(queue.recv(device.as_fd(), &mut buf).await.unwrap(), 2);
        assert_eq!(&buf, b"ok");
    }

    #[tokio::test]
    async fn cancelled_full_queue_send_does_not_enqueue_a_packet() {
        let (device, peer) = pair();
        let queue = Queue::new();
        let mut count = 0;
        loop {
            match device.send(b"old") {
                Ok(3) => count += 1,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                other => panic!("unexpected queue result: {other:?}"),
            }
            assert!(count < 10_000, "queue did not fill");
        }
        assert!(
            tokio::time::timeout(
                Duration::from_millis(30),
                queue.send(device.as_fd(), b"new")
            )
            .await
            .is_err()
        );
        let registration = queue.registration(device.as_fd()).unwrap();
        assert!(lock(&registration.waiters.state).writers.is_empty());
        let mut buf = [0; 3];
        for _ in 0..count {
            assert_eq!(peer.recv(&mut buf).unwrap(), 3);
            assert_eq!(&buf, b"old");
        }
        assert!(matches!(peer.recv(&mut buf), Err(e) if e.kind() == io::ErrorKind::WouldBlock));
        assert_eq!(queue.send(device.as_fd(), b"new").await.unwrap(), 3);
        assert_eq!(peer.recv(&mut buf).unwrap(), 3);
        assert_eq!(&buf, b"new");
    }

    #[tokio::test]
    async fn concurrent_waiters_both_make_progress() {
        let (device, peer) = pair();
        let device = Arc::new(device);
        let queue = Arc::new(Queue::new());
        let mut tasks = Vec::new();
        for _ in 0..2 {
            let device = Arc::clone(&device);
            let queue = Arc::clone(&queue);
            tasks.push(tokio::spawn(async move {
                let mut buf = [0; 1];
                queue.recv(device.as_fd(), &mut buf).await.unwrap();
                buf[0]
            }));
        }
        tokio::task::yield_now().await;
        peer.send(b"a").unwrap();
        peer.send(b"b").unwrap();
        let mut got = Vec::new();
        for task in tasks {
            got.push(
                tokio::time::timeout(Duration::from_secs(2), task)
                    .await
                    .unwrap()
                    .unwrap(),
            );
        }
        got.sort_unstable();
        assert_eq!(got, b"ab");
    }

    struct Counter(AtomicUsize);
    impl Wake for Counter {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn error_only_readiness_wakes_a_reader() {
        let (reader, writer) = std::io::pipe().unwrap();
        let queue = Queue::new();
        let registration = queue.registration(writer.as_fd()).unwrap();
        let counter = Arc::new(Counter(AtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&counter));
        let id = registration.waiters.next_id().unwrap();
        registration
            .waiters
            .arm(Direction::Read, id, &waker)
            .unwrap();
        drop(reader);
        tokio::time::timeout(Duration::from_secs(2), async {
            while counter.0.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("EPOLLERR must wake an outstanding read waiter");
    }

    #[test]
    fn driver_failure_is_delivered_to_current_and_future_waiters() {
        let waiters = WaitSet::default();
        let counter = Arc::new(Counter(AtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&counter));
        waiters.arm(Direction::Read, 0, &waker).unwrap();
        waiters.wake(0, Some(libc::EIO));
        assert_eq!(counter.0.load(Ordering::SeqCst), 1);
        assert!(matches!(
            waiters.arm(Direction::Read, 1, &waker),
            Err(Error::Platform(PlatformErrorCode::Linux(libc::EIO)))
        ));
    }
}
