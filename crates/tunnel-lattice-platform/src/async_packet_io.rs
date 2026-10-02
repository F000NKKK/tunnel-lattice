use std::future::Future;

use tunnel_lattice_core::Result;

/// Native async packet transfer on an open TUN/TAP device.
///
/// Implement this on a `Device` handle when the backend has a genuine
/// non-blocking I/O path (e.g. an async-registered file descriptor on
/// Linux/macOS, or an overlapped-I/O handle on Windows) instead of a
/// blocking syscall. When a backend implements this, it should also report
/// `Capability::NATIVE_ASYNC` so `tunnel-lattice`'s facade prefers this path
/// over `tunnel-lattice-async`'s thread-based adapter, which spawns one
/// blocking worker thread per device to bridge a [`crate::PacketIo`]
/// implementation onto a `futures::Stream`/`Sink` — correct for any backend,
/// but strictly worse for one that already has a real async path.
///
/// Available with the `async` feature.
pub trait AsyncPacketIo {
    /// Reads one packet into `buf`, returning the number of bytes written.
    ///
    /// On `Ok(n)` the implementation must have written `buf[..n]`, and
    /// `n <= buf.len()`. Callers may reuse `buf` across calls without
    /// clearing it, so bytes past what was actually written must never be
    /// reported as part of the packet.
    ///
    /// A packet is never truncated silently. If the next packet does not
    /// fit in `buf`, the implementation discards it and returns
    /// [`tunnel_lattice_core::Error::BufferTooSmall`]; the device stays
    /// usable, and the next `recv` with a large enough buffer receives the
    /// following packet. `tunnel_lattice_model::Device::recv_buffer_len`
    /// gives a buffer size that fits at the device's current MTU.
    fn recv(&self, buf: &mut [u8]) -> impl Future<Output = Result<usize>> + Send;

    /// Writes one packet from `buf`.
    ///
    /// # Cancellation
    ///
    /// Dropping the returned future before it completes must be safe and
    /// must leave either the whole packet sent or nothing: never part of a
    /// packet, and never the packet twice. The implementation must not use
    /// `buf` after the future is dropped (copy it first if a write can
    /// outlive the future). Whether a dropped send reached the device may
    /// be unknown to the caller, so the portable contract is "whole packet
    /// at most once; unknown after drop".
    fn send(&self, buf: &[u8]) -> impl Future<Output = Result<usize>> + Send;

    /// Writes a prefix of `packets`, in order, returning how many were sent.
    ///
    /// The async counterpart of [`crate::PacketIo::send_batch`], with the
    /// same prefix contract and no deferred errors: `Ok(n)` means
    /// `packets[..n]` were each sent whole and in order and `packets[n..]`
    /// were not touched (`n` may be less than `packets.len()`, so callers
    /// loop); an empty `packets` gives `Ok(0)`; `Err(e)` means nothing was
    /// sent and `e` belongs to `packets[0]`, classified exactly as
    /// [`Self::send`] would classify it. A failure after at least one packet
    /// was sent is reported as `Ok(k)`, and the next call, starting at
    /// `packets[k]`, sees the error.
    ///
    /// The provided implementation awaits [`Self::send`] for each packet in
    /// turn and stops at the first failure. A backend overrides it when it
    /// can send several packets more cheaply; an override keeps the same
    /// contract.
    ///
    /// # Cancellation
    ///
    /// Dropping the returned future before it completes must be safe and
    /// leaves an unknown prefix of `packets` sent: each packet whole and at
    /// most once, never part of a packet, never a packet twice, and never a
    /// packet after one that was not sent. As for [`Self::send`], the
    /// implementation must not use `packets` after the future is dropped. A
    /// caller that must know how many packets went out has to await the
    /// future to completion.
    fn send_batch(&self, packets: &[&[u8]]) -> impl Future<Output = Result<usize>> + Send
    where
        Self: Sync,
    {
        async move {
            for (sent, packet) in packets.iter().enumerate() {
                if let Err(error) = self.send(packet).await {
                    return if sent == 0 { Err(error) } else { Ok(sent) };
                }
            }
            Ok(packets.len())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::pin;
    use std::sync::Mutex;
    use std::task::{Context, Poll, Waker};

    use tunnel_lattice_core::{Error, Result};

    use super::AsyncPacketIo;

    /// What a send of the packet at a given index does.
    #[derive(Clone, Copy)]
    enum Outcome {
        Send,
        Fail,
        /// Stays `Pending` forever, sending nothing.
        Hang,
    }

    /// Each send's outcome is `outcome(packets sent so far)`.
    struct Mock {
        outcome: fn(usize) -> Outcome,
        sent: Mutex<Vec<Vec<u8>>>,
    }

    impl Mock {
        fn new(outcome: fn(usize) -> Outcome) -> Self {
            Self {
                outcome,
                sent: Mutex::new(Vec::new()),
            }
        }

        fn sent(&self) -> Vec<Vec<u8>> {
            self.sent.lock().unwrap().clone()
        }
    }

    impl AsyncPacketIo for Mock {
        async fn recv(&self, _buf: &mut [u8]) -> Result<usize> {
            Err(Error::Disconnected)
        }

        /// Decides and writes only when polled, so a future dropped while
        /// `Pending` sent nothing.
        fn send(&self, buf: &[u8]) -> impl Future<Output = Result<usize>> + Send {
            std::future::poll_fn(move |_| {
                let mut sent = self.sent.lock().unwrap();
                match (self.outcome)(sent.len()) {
                    Outcome::Send => {
                        sent.push(buf.to_vec());
                        Poll::Ready(Ok(buf.len()))
                    }
                    Outcome::Fail => Poll::Ready(Err(Error::Disconnected)),
                    Outcome::Hang => Poll::Pending,
                }
            })
        }
    }

    /// Polls `future` once with a no-op waker.
    fn poll_once<F: Future>(future: F) -> Poll<F::Output> {
        pin!(future).poll(&mut Context::from_waker(Waker::noop()))
    }

    /// Polls a future that never waits to completion.
    fn ready<F: Future>(future: F) -> F::Output {
        match poll_once(future) {
            Poll::Ready(output) => output,
            Poll::Pending => panic!("the mock future is not expected to wait"),
        }
    }

    const PACKETS: [&[u8]; 3] = [b"a", b"bb", b"ccc"];

    #[test]
    fn an_empty_batch_sends_nothing() {
        let mock = Mock::new(|_| Outcome::Fail);
        assert_eq!(ready(mock.send_batch(&[])).unwrap(), 0);
        assert!(mock.sent().is_empty());
    }

    #[test]
    fn a_full_batch_sends_every_packet_in_order() {
        let mock = Mock::new(|_| Outcome::Send);
        assert_eq!(ready(mock.send_batch(&PACKETS)).unwrap(), 3);
        assert_eq!(mock.sent(), [b"a".to_vec(), b"bb".into(), b"ccc".into()]);
    }

    #[test]
    fn a_failure_on_the_first_packet_is_an_error_and_sends_nothing() {
        let mock = Mock::new(|_| Outcome::Fail);
        assert!(matches!(
            ready(mock.send_batch(&PACKETS)),
            Err(Error::Disconnected)
        ));
        assert!(mock.sent().is_empty());
    }

    #[test]
    fn a_failure_after_k_packets_reports_the_sent_prefix() {
        let fail_from_two = |sent| {
            if sent < 2 {
                Outcome::Send
            } else {
                Outcome::Fail
            }
        };
        let mock = Mock::new(fail_from_two);
        assert_eq!(ready(mock.send_batch(&PACKETS)).unwrap(), 2);
        assert_eq!(mock.sent(), PACKETS[..2]);
        // The caller retries from `packets[2]` and now sees the error.
        assert!(matches!(
            ready(mock.send_batch(&PACKETS[2..])),
            Err(Error::Disconnected)
        ));
    }

    #[test]
    fn a_dropped_batch_future_leaves_a_whole_packet_prefix_sent() {
        let hang_from_one = |sent| {
            if sent < 1 {
                Outcome::Send
            } else {
                Outcome::Hang
            }
        };
        let mock = Mock::new(hang_from_one);
        // One poll sends `packets[0]`, then waits on `packets[1]`; returning
        // drops the future there.
        assert!(poll_once(mock.send_batch(&PACKETS)).is_pending());
        assert_eq!(mock.sent(), PACKETS[..1]);
    }

    #[test]
    fn the_batch_future_is_send() {
        fn assert_send<T: Send>(_: &T) {}
        let mock = Mock::new(|_| Outcome::Send);
        let future = mock.send_batch(&PACKETS);
        assert_send(&future);
        assert_eq!(ready(future).unwrap(), 3);
    }
}
