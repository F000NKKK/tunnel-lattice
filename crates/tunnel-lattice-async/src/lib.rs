//! Runtime-agnostic async adapters for Tunnel Lattice.
//!
//! Two ways to get a `futures::Stream` of received packets, both producing
//! the same [`PacketStream`] type:
//!
//! - [`from_async_device`] wraps a backend's own
//!   [`tunnel_lattice_platform::AsyncPacketIo`] directly — no worker thread,
//!   no polling loop. Dropping the stream drops the in-flight `recv` future,
//!   which is genuinely, immediately cancelled the same way dropping any
//!   other future is; there is nothing left running to leak. Prefer this
//!   whenever the backend reports `Capability::NATIVE_ASYNC`.
//! - [`from_device`] bridges a synchronous
//!   [`tunnel_lattice_platform::PacketIo`] device instead, mirroring
//!   `net-lattice-async::from_receiver`'s worker-thread bridge: it spawns
//!   one blocking worker thread per device, since a device's blocking
//!   `recv` has no waker-registration mechanism a direct `Stream`
//!   implementation could poll. This is the fallback for a backend with no
//!   native async path at all — see [`PacketStream`]'s docs for the real
//!   shutdown limitation this variant has that `from_async_device` does not.
//!
//! No Tokio, async-std, or smol dependency is imposed by this crate itself.
//!
//! The crate also provides [`PacketPool`], a fixed-capacity pool of receive
//! slots carved from one slab allocated once, and [`PacketBuf`], a 16-byte
//! (on 64-bit targets) offset view of one packet inside a pool slot.

#![warn(missing_docs)]

mod pool;

pub use pool::{PacketBuf, PacketPool};

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use std::thread;

use futures::Stream;
use futures::channel::mpsc::{UnboundedReceiver, unbounded};
pub use tunnel_lattice_core::{Error, Result};
use tunnel_lattice_platform::{AsyncPacketIo, PacketIo};

/// A runtime-agnostic asynchronous stream of received packets.
///
/// Each item is one packet's bytes. Constructed by [`from_async_device`]
/// (native async, genuinely cancellable on drop) or [`from_device`]
/// (thread-bridge, best-effort shutdown only) — see each function's docs
/// for which to prefer.
///
/// Both variants behave the same way on errors: a packet larger than the
/// receive buffer yields `Err(Error::BufferTooSmall)` (the packet is
/// discarded, never truncated) and the stream keeps receiving;
/// `Err(Error::Disconnected)` is yielded once and then the stream ends.
pub struct PacketStream {
    inner: Inner,
}

enum Inner {
    /// Backed by a boxed native async stream built directly from
    /// `AsyncPacketIo::recv` — dropping this drops the in-flight future,
    /// which is complete, immediate cancellation. No worker thread exists
    /// for this variant.
    Native(Pin<Box<dyn Stream<Item = Result<Vec<u8>>> + Send>>),
    /// Backed by a worker thread bridging a blocking `PacketIo::recv` loop —
    /// see [`from_device`]'s docs for its shutdown limitation.
    ThreadBridge {
        receiver: UnboundedReceiver<Result<Vec<u8>>>,
        stop: Arc<AtomicBool>,
    },
}

/// Wraps a backend's native [`AsyncPacketIo`] as a [`PacketStream`] — no
/// worker thread, no polling loop.
///
/// `buf_len` is the per-packet receive buffer size; for a device opened
/// through the `tunnel-lattice` facade, use
/// `handle.snapshot()?.recv_buffer_len()`, which fits one packet at the
/// device's current MTU (Ethernet framing included for TAP). A packet
/// larger than `buf_len` is never truncated: it is discarded and the stream
/// yields `Err(Error::BufferTooSmall)`, then keeps receiving. A
/// non-conforming device that reports more bytes than `buf_len` is treated
/// the same way. The stream ends after yielding `Err(Error::Disconnected)`;
/// every other error is yielded and the stream continues.
///
/// Dropping the returned stream drops whatever `AsyncPacketIo::recv` future
/// is currently in flight, the same way dropping any other future cancels
/// it — this genuinely stops the operation immediately, unlike
/// [`from_device`]'s worker thread, which can outlive the stream that
/// spawned it. Prefer this over `from_device` whenever the backend reports
/// `Capability::NATIVE_ASYNC`; `tunnel_lattice::Handle::packet_stream`
/// already does this automatically.
pub fn from_async_device<D>(device: Arc<D>, buf_len: usize) -> PacketStream
where
    D: AsyncPacketIo + Send + Sync + 'static,
{
    enum State<D> {
        Live(Arc<D>),
        Done,
    }

    let stream = futures::stream::unfold(State::Live(device), move |state| async move {
        let device = match state {
            State::Live(device) => device,
            State::Done => return None,
        };
        let mut buf = vec![0u8; buf_len];
        match checked_len(device.recv(&mut buf).await, buf_len) {
            Ok(len) => {
                buf.truncate(len);
                Some((Ok(buf), State::Live(device)))
            }
            Err(Error::Disconnected) => Some((Err(Error::Disconnected), State::Done)),
            Err(err) => Some((Err(err), State::Live(device))),
        }
    });
    PacketStream {
        inner: Inner::Native(Box::pin(stream)),
    }
}

/// Bridges a synchronous [`PacketIo`] device to a waker-aware stream of
/// received packets.
///
/// `buf_len` is the per-packet receive buffer size, with the same meaning
/// and the same too-small-buffer and end-of-stream behavior as
/// [`from_async_device`]'s: use `handle.snapshot()?.recv_buffer_len()`; an
/// oversize packet yields `Err(Error::BufferTooSmall)` and the stream keeps
/// receiving; only `Err(Error::Disconnected)` ends it.
///
/// **Shutdown limitation**: dropping the returned stream signals the worker
/// thread to stop but does **not** join it: [`PacketIo::recv`] has no
/// timeout/cancellation contract, so a worker parked inside a blocking
/// `recv` with no further packets arriving cannot be woken by this adapter
/// alone — it exits only once the underlying device itself unblocks the
/// call (a packet arrives, or every other handle to the device is closed
/// and the OS returns an error). Use [`from_async_device`] instead whenever
/// the backend implements `AsyncPacketIo`, which has no such limitation.
pub fn from_device<D>(device: Arc<D>, buf_len: usize) -> PacketStream
where
    D: PacketIo + Send + Sync + 'static,
{
    let (sender, receiver) = unbounded();
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = Arc::clone(&stop);
    thread::spawn(move || forward_device(device, buf_len, sender, worker_stop));
    PacketStream {
        inner: Inner::ThreadBridge { receiver, stop },
    }
}

/// Enforces the `recv` contract's `n <= buf.len()` bound on a device's
/// result: a larger `n` means the packet did not fit, reported as
/// [`Error::BufferTooSmall`] instead of an out-of-bounds slice.
fn checked_len(result: Result<usize>, buf_len: usize) -> Result<usize> {
    match result {
        Ok(len) if len > buf_len => Err(Error::BufferTooSmall),
        other => other,
    }
}

fn forward_device<D>(
    device: Arc<D>,
    buf_len: usize,
    sender: futures::channel::mpsc::UnboundedSender<Result<Vec<u8>>>,
    stop: Arc<AtomicBool>,
) where
    D: PacketIo,
{
    let mut buf = vec![0u8; buf_len];
    while !stop.load(Ordering::Acquire) {
        match checked_len(device.recv(&mut buf), buf_len) {
            Ok(len) => {
                if sender.unbounded_send(Ok(buf[..len].to_vec())).is_err() {
                    break;
                }
            }
            Err(Error::Disconnected) => {
                let _ = sender.unbounded_send(Err(Error::Disconnected));
                break;
            }
            Err(err) => {
                if sender.unbounded_send(Err(err)).is_err() {
                    break;
                }
            }
        }
    }
}

impl Stream for PacketStream {
    type Item = Result<Vec<u8>>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match &mut self.get_mut().inner {
            Inner::Native(stream) => stream.as_mut().poll_next(cx),
            Inner::ThreadBridge { receiver, .. } => Pin::new(receiver).poll_next(cx),
        }
    }
}

impl Drop for PacketStream {
    fn drop(&mut self) {
        // `Inner::Native` needs no explicit action: dropping `self.inner`
        // right after this drops the boxed stream (and whatever
        // `AsyncPacketIo::recv` future it was polling), which is itself the
        // cancellation — see `from_async_device`'s docs.
        if let Inner::ThreadBridge { stop, .. } = &self.inner {
            // Best-effort only — see `from_device`'s docs on why this
            // cannot join the worker thread.
            stop.store(true, Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;

    use futures::{FutureExt, StreamExt};

    use super::*;

    /// A mock `AsyncPacketIo` whose `recv` never resolves — models a real
    /// device with no traffic arriving, the exact case
    /// `tunnel-lattice`'s own `PacketStream` docs describe as unrecoverable
    /// for the thread-bridge variant. No real device or privilege needed:
    /// this tests `from_async_device`'s cancellation contract in isolation.
    struct BlocksForever;

    impl AsyncPacketIo for BlocksForever {
        fn recv(&self, _buf: &mut [u8]) -> impl Future<Output = Result<usize>> + Send {
            std::future::pending()
        }

        fn send(&self, _buf: &[u8]) -> impl Future<Output = Result<usize>> + Send {
            std::future::pending()
        }
    }

    #[test]
    fn from_async_device_drop_completes_immediately_even_mid_recv() {
        let mut stream = from_async_device(Arc::new(BlocksForever), 1500);

        // Poll once to put the boxed `recv` future in flight (mirrors a
        // real caller awaiting the stream), without a runtime: `now_or_never`
        // polls exactly once and asserts it doesn't complete yet.
        assert!(
            stream.next().now_or_never().is_none(),
            "a stream over a device with no traffic must not resolve immediately"
        );

        // The real assertion is that this line is ever reached at all: with
        // the old thread-bridge design there would be no way to prove a
        // parked worker thread was gone without waiting on it, which is
        // exactly the limitation 0.4 fixes for a backend with native async.
        // Dropping a boxed future is synchronous and unconditional, so a
        // hang here would mean this test itself never finishes.
        drop(stream);
    }

    /// A mock `AsyncPacketIo` that resolves once with data, then reports
    /// `Error::Disconnected` — exercises the stream's termination path.
    struct RecvOnceThenDisconnect {
        yielded: std::sync::atomic::AtomicBool,
    }

    impl AsyncPacketIo for RecvOnceThenDisconnect {
        fn recv(&self, buf: &mut [u8]) -> impl Future<Output = Result<usize>> + Send {
            let first = !self.yielded.swap(true, Ordering::AcqRel);
            async move {
                if first {
                    buf[..3].copy_from_slice(b"abc");
                    Ok(3)
                } else {
                    Err(Error::Disconnected)
                }
            }
        }

        async fn send(&self, _buf: &[u8]) -> Result<usize> {
            Ok(0)
        }
    }

    #[test]
    fn from_async_device_terminates_after_disconnected() {
        let device = Arc::new(RecvOnceThenDisconnect {
            yielded: std::sync::atomic::AtomicBool::new(false),
        });
        let mut stream = from_async_device(device, 1500);

        let first = stream
            .next()
            .now_or_never()
            .expect("first recv resolves immediately in this mock")
            .expect("stream yields an item");
        assert_eq!(first.unwrap(), b"abc");

        let second = stream
            .next()
            .now_or_never()
            .expect("second recv resolves immediately in this mock")
            .expect("stream yields the disconnect error before ending");
        assert!(matches!(second, Err(Error::Disconnected)));

        let third = stream.next().now_or_never().expect("stream has ended");
        assert!(third.is_none(), "stream must end after Disconnected");
    }

    /// One scripted `recv` outcome.
    #[derive(Clone, Copy)]
    enum Step {
        /// A packet that fits.
        Packet(&'static [u8]),
        /// A conforming device's report of an oversize packet.
        TooSmall,
        /// A non-conforming device reporting one byte more than the buffer.
        Overlong,
        /// End of life.
        Disconnected,
    }

    /// A mock device implementing both `PacketIo` and `AsyncPacketIo`,
    /// replaying `steps` in order; each resolves immediately.
    struct Scripted {
        steps: std::sync::Mutex<std::collections::VecDeque<Step>>,
    }

    impl Scripted {
        fn new(steps: &[Step]) -> Arc<Self> {
            Arc::new(Self {
                steps: std::sync::Mutex::new(steps.iter().copied().collect()),
            })
        }

        fn next(&self, buf: &mut [u8]) -> Result<usize> {
            let step = self
                .steps
                .lock()
                .unwrap()
                .pop_front()
                .expect("the stream received past Disconnected");
            match step {
                Step::Packet(bytes) => {
                    buf[..bytes.len()].copy_from_slice(bytes);
                    Ok(bytes.len())
                }
                Step::TooSmall => Err(Error::BufferTooSmall),
                Step::Overlong => Ok(buf.len() + 1),
                Step::Disconnected => Err(Error::Disconnected),
            }
        }
    }

    impl PacketIo for Scripted {
        fn recv(&self, buf: &mut [u8]) -> Result<usize> {
            self.next(buf)
        }

        fn send(&self, buf: &[u8]) -> Result<usize> {
            Ok(buf.len())
        }
    }

    impl AsyncPacketIo for Scripted {
        fn recv(&self, buf: &mut [u8]) -> impl Future<Output = Result<usize>> + Send {
            let result = self.next(buf);
            async move { result }
        }

        async fn send(&self, buf: &[u8]) -> Result<usize> {
            Ok(buf.len())
        }
    }

    const SCRIPT: [Step; 5] = [
        Step::TooSmall,
        Step::Packet(b"abc"),
        Step::Overlong,
        Step::Packet(b"defg"),
        Step::Disconnected,
    ];

    /// Collects every item until the stream ends, blocking on each (the
    /// thread bridge delivers from another thread).
    fn collect(mut stream: PacketStream) -> Vec<Result<Vec<u8>>> {
        let mut items = Vec::new();
        while let Some(item) = futures::executor::block_on(stream.next()) {
            items.push(item);
        }
        items
    }

    /// Oversize packets (reported by the device, or implied by an overlong
    /// length) yield `BufferTooSmall` without ending the stream; the
    /// stream ends right after `Disconnected`.
    fn assert_script_items(items: &[Result<Vec<u8>>]) {
        assert_eq!(items.len(), 5, "one item per step, then the end");
        assert!(matches!(items[0], Err(Error::BufferTooSmall)));
        assert_eq!(items[1].as_deref().unwrap(), b"abc");
        assert!(matches!(items[2], Err(Error::BufferTooSmall)));
        assert_eq!(items[3].as_deref().unwrap(), b"defg");
        assert!(matches!(items[4], Err(Error::Disconnected)));
    }

    #[test]
    fn from_async_device_buffer_too_small_is_not_terminal() {
        let items = collect(from_async_device(Scripted::new(&SCRIPT), 8));
        assert_script_items(&items);
    }

    #[test]
    fn from_device_buffer_too_small_is_not_terminal() {
        let items = collect(from_device(Scripted::new(&SCRIPT), 8));
        assert_script_items(&items);
    }

    /// A zero-length buffer cannot hold any packet: every non-empty packet
    /// is `BufferTooSmall` on both paths, and neither slices out of bounds.
    #[test]
    fn a_zero_length_buffer_reports_every_packet_as_too_small() {
        let script = [Step::Overlong, Step::Disconnected];
        for stream in [
            from_async_device(Scripted::new(&script), 0),
            from_device(Scripted::new(&script), 0),
        ] {
            let items = collect(stream);
            assert_eq!(items.len(), 2);
            assert!(matches!(items[0], Err(Error::BufferTooSmall)));
            assert!(matches!(items[1], Err(Error::Disconnected)));
        }
    }

    #[test]
    fn checked_len_rejects_only_lengths_past_the_buffer() {
        assert!(matches!(checked_len(Ok(8), 8), Ok(8)));
        assert!(matches!(checked_len(Ok(0), 8), Ok(0)));
        assert!(matches!(checked_len(Ok(9), 8), Err(Error::BufferTooSmall)));
        assert!(matches!(
            checked_len(Err(Error::Disconnected), 8),
            Err(Error::Disconnected)
        ));
    }
}
