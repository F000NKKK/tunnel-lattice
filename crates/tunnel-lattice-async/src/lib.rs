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

#![warn(missing_docs)]

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
/// `mtu` bounds the per-packet receive buffer; a packet larger than `mtu`
/// bytes is truncated by the underlying device the same way `recv`
/// documents it, not by this adapter.
///
/// Dropping the returned stream drops whatever `AsyncPacketIo::recv` future
/// is currently in flight, the same way dropping any other future cancels
/// it — this genuinely stops the operation immediately, unlike
/// [`from_device`]'s worker thread, which can outlive the stream that
/// spawned it. Prefer this over `from_device` whenever the backend reports
/// `Capability::NATIVE_ASYNC`; `tunnel_lattice::Handle::packet_stream`
/// already does this automatically.
pub fn from_async_device<D>(device: Arc<D>, mtu: usize) -> PacketStream
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
        let mut buf = vec![0u8; mtu];
        match device.recv(&mut buf).await {
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
/// `mtu` bounds the per-packet receive buffer; a packet larger than `mtu`
/// bytes is truncated by the underlying device the same way `recv` documents
/// it, not by this adapter.
///
/// **Shutdown limitation**: dropping the returned stream signals the worker
/// thread to stop but does **not** join it: [`PacketIo::recv`] has no
/// timeout/cancellation contract, so a worker parked inside a blocking
/// `recv` with no further packets arriving cannot be woken by this adapter
/// alone — it exits only once the underlying device itself unblocks the
/// call (a packet arrives, or every other handle to the device is closed
/// and the OS returns an error). Use [`from_async_device`] instead whenever
/// the backend implements `AsyncPacketIo`, which has no such limitation.
pub fn from_device<D>(device: Arc<D>, mtu: usize) -> PacketStream
where
    D: PacketIo + Send + Sync + 'static,
{
    let (sender, receiver) = unbounded();
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = Arc::clone(&stop);
    thread::spawn(move || forward_device(device, mtu, sender, worker_stop));
    PacketStream {
        inner: Inner::ThreadBridge { receiver, stop },
    }
}

fn forward_device<D>(
    device: Arc<D>,
    mtu: usize,
    sender: futures::channel::mpsc::UnboundedSender<Result<Vec<u8>>>,
    stop: Arc<AtomicBool>,
) where
    D: PacketIo,
{
    let mut buf = vec![0u8; mtu];
    while !stop.load(Ordering::Acquire) {
        match device.recv(&mut buf) {
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
}
