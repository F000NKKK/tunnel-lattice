//! Runtime-agnostic async adapters for Tunnel Lattice.
//!
//! [`from_device`] bridges a synchronous
//! [`tunnel_lattice_platform::PacketIo`] device onto a `futures::Stream`,
//! mirroring `net-lattice-async::from_receiver`'s worker-thread bridge: it
//! spawns one blocking worker thread per device, since a device's blocking
//! `recv` has no waker-registration mechanism a direct `Stream`
//! implementation could poll. No Tokio, async-std, or smol dependency is
//! imposed.
//!
//! Prefer a backend's own `tunnel_lattice_platform::AsyncPacketIo`
//! implementation over this adapter when
//! `Capability::NATIVE_ASYNC` is set — see that trait's docs. This adapter
//! exists for backends (including a future non-`tun-rs` one) that only ever
//! offer blocking I/O.

#![warn(missing_docs)]

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use std::thread;

use futures::Stream;
use futures::channel::mpsc::{UnboundedReceiver, unbounded};
pub use tunnel_lattice_core::{Error, Result};
use tunnel_lattice_platform::PacketIo;

/// A runtime-agnostic asynchronous stream of received packets.
///
/// Each item is one packet's bytes, as delivered by the underlying device's
/// blocking `recv`. Dropping the stream signals the worker thread to stop
/// but does **not** join it: [`PacketIo::recv`] has no timeout/cancellation
/// contract, so a worker parked inside a blocking `recv` with no further
/// packets arriving cannot be woken by this adapter alone — it exits only
/// once the underlying device itself unblocks the call (a packet arrives,
/// or every other handle to the device is closed and the OS returns an
/// error). This is a known bootstrap-stage limitation; a future revision
/// should either require a cancellable `recv` variant or document the
/// specific per-backend unblocking behavior instead of a generic bound.
pub struct PacketStream {
    receiver: UnboundedReceiver<Result<Vec<u8>>>,
    stop: Arc<AtomicBool>,
}

/// Bridges a synchronous [`PacketIo`] device to a waker-aware stream of
/// received packets.
///
/// `mtu` bounds the per-packet receive buffer; a packet larger than `mtu`
/// bytes is truncated by the underlying device the same way `recv` documents
/// it, not by this adapter.
pub fn from_device<D>(device: Arc<D>, mtu: usize) -> PacketStream
where
    D: PacketIo + Send + Sync + 'static,
{
    let (sender, receiver) = unbounded();
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = Arc::clone(&stop);
    thread::spawn(move || forward_device(device, mtu, sender, worker_stop));
    PacketStream { receiver, stop }
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

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.receiver).poll_next(cx)
    }
}

impl Drop for PacketStream {
    fn drop(&mut self) {
        // Best-effort only — see the type's docs on why this cannot join
        // the worker thread.
        self.stop.store(true, Ordering::Release);
    }
}
