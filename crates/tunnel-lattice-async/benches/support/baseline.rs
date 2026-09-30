//! A frozen copy of the 0.4 `PacketStream` receive paths, which allocate
//! one `Vec<u8>` per packet (and, on the thread bridge, copy it into an
//! unbounded channel). Kept here, not in the library, so later changes to
//! `PacketStream` can still be measured against this baseline. Do not
//! "improve" this code: its cost is the point of comparison.

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use std::thread;

use futures::Stream;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use tunnel_lattice_async::{Error, Result};
use tunnel_lattice_platform::{AsyncPacketIo, PacketIo};

/// The boxed item stream both baseline paths return.
pub type VecStream = Pin<Box<dyn Stream<Item = Result<Vec<u8>>> + Send>>;

/// 0.4 `from_async_device`: one zeroed `Vec` of `mtu` bytes per `recv`.
pub fn native<D>(device: Arc<D>, mtu: usize) -> VecStream
where
    D: AsyncPacketIo + Send + Sync + 'static,
{
    enum State<D> {
        Live(Arc<D>),
        Done,
    }

    Box::pin(futures::stream::unfold(
        State::Live(device),
        move |state| async move {
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
        },
    ))
}

/// 0.4 `from_device`: a worker thread copying each packet into a new `Vec`
/// sent over an unbounded channel.
pub fn bridge<D>(device: Arc<D>, mtu: usize) -> VecStream
where
    D: PacketIo + Send + Sync + 'static,
{
    let (sender, receiver) = unbounded();
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = Arc::clone(&stop);
    thread::spawn(move || forward(device, mtu, sender, worker_stop));
    Box::pin(Bridge { receiver, stop })
}

fn forward<D: PacketIo>(
    device: Arc<D>,
    mtu: usize,
    sender: UnboundedSender<Result<Vec<u8>>>,
    stop: Arc<AtomicBool>,
) {
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

struct Bridge {
    receiver: UnboundedReceiver<Result<Vec<u8>>>,
    stop: Arc<AtomicBool>,
}

impl Stream for Bridge {
    type Item = Result<Vec<u8>>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.get_mut().receiver).poll_next(cx)
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}
