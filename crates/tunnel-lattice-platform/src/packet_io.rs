use tunnel_lattice_core::Result;

/// Synchronous packet transfer on an open TUN/TAP device.
///
/// Implemented by a backend's `Device` handle (the type returned by
/// [`crate::DeviceProvider::open`]), not by the backend connection object
/// itself — each open device is its own independent I/O channel.
///
/// A TUN device's buffers carry a raw IP packet with no L2 framing; a TAP
/// device's buffers carry a full Ethernet frame. This trait does not
/// distinguish the two at the type level — check
/// `tunnel_lattice_model::DeviceKind` on the `Device` this handle was opened
/// with to interpret buffer contents correctly.
pub trait PacketIo {
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
    fn recv(&self, buf: &mut [u8]) -> Result<usize>;

    /// Reads up to `min(bufs.len(), lens.len())` packets, one per element,
    /// returning how many were received.
    ///
    /// The result follows a prefix contract:
    ///
    /// - `Ok(n)`: packets were written to `bufs[..n]` in device order, and
    ///   `lens[i]` is the length of packet `i`, with
    ///   `lens[i] <= bufs[i].len()`. Each `bufs[i][..lens[i]]` holds exactly
    ///   one packet, framed as [`Self::recv`] frames it, never several and
    ///   never part of one.
    /// - `Ok(0)` only when `bufs` or `lens` is empty, without any native
    ///   call. A mismatch between the two lengths is not an error: the
    ///   shorter one sets the capacity.
    /// - Otherwise the call waits only until the first packet is available.
    ///   Once it has taken that packet from the device it never waits
    ///   again, and returns a short batch instead. `n` less than the
    ///   capacity is normal and does not mean the device is empty.
    /// - `Err(e)`: no packet was received, and `e` is classified exactly as
    ///   [`Self::recv`] would classify it. A failure after at least one
    ///   packet was received is reported as `Ok(k)` instead, and the next
    ///   call on the same queue sees it: either the device state that
    ///   caused it reproduces it, or it is retried or dropped as `recv`
    ///   retries or drops it, or, for
    ///   [`tunnel_lattice_core::Error::BufferTooSmall`], it is deferred to
    ///   the next call (see below). No error is swallowed.
    ///
    /// Only `bufs[i][..lens[i]]` for `i < n` is meaningful. On any result,
    /// the implementation may have written anywhere in every element of
    /// `bufs`, including `bufs[n..]` and the bytes past `lens[i]`, so a
    /// caller must not read them as packet data.
    ///
    /// A packet is never truncated silently. A packet that does not fit
    /// `bufs[0]` is discarded and reported as `Err(BufferTooSmall)`, as
    /// [`Self::recv`] does. A packet that does not fit `bufs[k]`, `k >= 1`,
    /// ends the batch with `Ok(k)`: either it stays queued for the next
    /// call, or, if it was already taken from the device, it is discarded
    /// and the next `recv` or `recv_batch` on that queue returns
    /// `Err(BufferTooSmall)` first, before receiving anything else.
    ///
    /// The provided implementation receives exactly one packet with
    /// [`Self::recv`] into `bufs[0]`. A backend overrides it when it can
    /// take several packets that are already waiting without blocking
    /// again; an override keeps the same contract.
    fn recv_batch(&self, bufs: &mut [&mut [u8]], lens: &mut [usize]) -> Result<usize> {
        let (Some(buf), Some(len)) = (bufs.first_mut(), lens.first_mut()) else {
            return Ok(0);
        };
        *len = self.recv(buf)?;
        Ok(1)
    }

    /// Writes one packet from `buf`.
    fn send(&self, buf: &[u8]) -> Result<usize>;

    /// Writes a prefix of `packets`, in order, returning how many were sent.
    ///
    /// Each element is one packet, framed as for [`Self::send`]. The result
    /// follows a prefix contract, with no deferred errors:
    ///
    /// - `Ok(n)`: `packets[..n]` were each sent whole and in order, and
    ///   `packets[n..]` were not touched. `n` may be less than
    ///   `packets.len()` (a short batch), so a caller that must send every
    ///   packet loops, starting the next call at `packets[n]`.
    /// - `Ok(0)` for an empty `packets`, without any native call.
    /// - `Err(e)`: nothing was sent, and `e` is the error of `packets[0]`,
    ///   classified exactly as [`Self::send`] would classify it. A failure
    ///   after at least one packet was sent is reported as `Ok(k)` instead;
    ///   the caller sees the error on its next call, which starts at the
    ///   packet that failed.
    ///
    /// The provided implementation sends the packets one by one with
    /// [`Self::send`] and stops at the first failure. A backend overrides
    /// it when it can send several packets more cheaply, for example by
    /// coalescing them into fewer native writes; an override keeps the same
    /// contract.
    fn send_batch(&self, packets: &[&[u8]]) -> Result<usize> {
        for (sent, packet) in packets.iter().enumerate() {
            if let Err(error) = self.send(packet) {
                return if sent == 0 { Err(error) } else { Ok(sent) };
            }
        }
        Ok(packets.len())
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;

    use tunnel_lattice_core::{Error, Result};

    use super::PacketIo;

    /// Sends succeed until `fail_at` packets were sent; that send and every
    /// later one fail with `Error::Disconnected`. Receives take packets from
    /// `inbox` in order and fail with `Error::Disconnected` once it is empty.
    struct Mock {
        fail_at: usize,
        sent: RefCell<Vec<Vec<u8>>>,
        inbox: RefCell<VecDeque<Vec<u8>>>,
        /// `recv` calls so far.
        recvs: Cell<usize>,
    }

    impl Mock {
        fn new(fail_at: usize) -> Self {
            Self {
                fail_at,
                sent: RefCell::new(Vec::new()),
                inbox: RefCell::new(VecDeque::new()),
                recvs: Cell::new(0),
            }
        }

        fn with_inbox(packets: &[&[u8]]) -> Self {
            let mock = Self::new(usize::MAX);
            mock.inbox
                .borrow_mut()
                .extend(packets.iter().map(|packet| packet.to_vec()));
            mock
        }
    }

    impl PacketIo for Mock {
        /// Follows `recv`'s contract: an oversize packet is discarded and
        /// reported as `BufferTooSmall`.
        fn recv(&self, buf: &mut [u8]) -> Result<usize> {
            self.recvs.set(self.recvs.get() + 1);
            let packet = self
                .inbox
                .borrow_mut()
                .pop_front()
                .ok_or(Error::Disconnected)?;
            let slot = buf.get_mut(..packet.len()).ok_or(Error::BufferTooSmall)?;
            slot.copy_from_slice(&packet);
            Ok(packet.len())
        }

        fn send(&self, buf: &[u8]) -> Result<usize> {
            let mut sent = self.sent.borrow_mut();
            if sent.len() >= self.fail_at {
                return Err(Error::Disconnected);
            }
            sent.push(buf.to_vec());
            Ok(buf.len())
        }
    }

    const PACKETS: [&[u8]; 3] = [b"a", b"bb", b"ccc"];

    #[test]
    fn an_empty_batch_sends_nothing() {
        let mock = Mock::new(0);
        assert_eq!(mock.send_batch(&[]).unwrap(), 0);
        assert!(mock.sent.borrow().is_empty());
    }

    #[test]
    fn a_full_batch_sends_every_packet_in_order() {
        let mock = Mock::new(usize::MAX);
        assert_eq!(mock.send_batch(&PACKETS).unwrap(), 3);
        assert_eq!(
            *mock.sent.borrow(),
            [b"a".to_vec(), b"bb".into(), b"ccc".into()]
        );
    }

    #[test]
    fn a_failure_on_the_first_packet_is_an_error_and_sends_nothing() {
        let mock = Mock::new(0);
        assert!(matches!(
            mock.send_batch(&PACKETS),
            Err(Error::Disconnected)
        ));
        assert!(mock.sent.borrow().is_empty());
    }

    #[test]
    fn a_failure_after_k_packets_reports_the_sent_prefix() {
        for k in 1..PACKETS.len() {
            let mock = Mock::new(k);
            assert_eq!(mock.send_batch(&PACKETS).unwrap(), k);
            assert_eq!(*mock.sent.borrow(), PACKETS[..k]);
            // The caller retries from `packets[k]` and now sees the error.
            assert!(matches!(
                mock.send_batch(&PACKETS[k..]),
                Err(Error::Disconnected)
            ));
        }
    }

    #[test]
    fn the_trait_stays_dyn_compatible() {
        let mock = Mock::new(usize::MAX);
        let device: &dyn PacketIo = &mock;
        assert_eq!(device.send_batch(&PACKETS[..1]).unwrap(), 1);

        let mock = Mock::with_inbox(&PACKETS);
        let device: &dyn PacketIo = &mock;
        let mut buf = [0u8; 4];
        let mut lens = [0usize; 1];
        assert_eq!(device.recv_batch(&mut [&mut buf], &mut lens).unwrap(), 1);
        assert_eq!((&buf[..lens[0]], lens[0]), (&b"a"[..], 1));
    }

    #[test]
    fn a_zero_capacity_batch_receives_nothing_without_calling_recv() {
        let mock = Mock::with_inbox(&PACKETS);
        let mut buf = [0u8; 4];
        assert_eq!(mock.recv_batch(&mut [], &mut []).unwrap(), 0);
        assert_eq!(mock.recv_batch(&mut [], &mut [0; 2]).unwrap(), 0);
        // `lens` shorter than `bufs` sets the capacity.
        assert_eq!(mock.recv_batch(&mut [&mut buf], &mut []).unwrap(), 0);
        assert_eq!(mock.recvs.get(), 0);
        assert_eq!(mock.inbox.borrow().len(), PACKETS.len());
    }

    #[test]
    fn the_default_batch_receives_exactly_one_packet() {
        let mock = Mock::with_inbox(&PACKETS);
        let (mut first, mut second, mut third) = ([0u8; 4], [0xEEu8; 4], [0xEEu8; 4]);
        let mut lens = [usize::MAX; 3];
        let received = mock
            .recv_batch(&mut [&mut first, &mut second, &mut third], &mut lens)
            .unwrap();
        assert_eq!(received, 1);
        assert_eq!(lens[0], 1);
        assert_eq!(&first[..lens[0]], b"a");
        assert_eq!(mock.recvs.get(), 1, "one recv, no wait for more packets");
        assert_eq!((second, third), ([0xEE; 4], [0xEE; 4]));
        assert_eq!(lens[1..], [usize::MAX; 2]);

        // The rest stay queued, in order, for the next call.
        let mut buf = [0u8; 4];
        let mut lens = [0usize; 2];
        assert_eq!(mock.recv_batch(&mut [&mut buf], &mut lens).unwrap(), 1);
        assert_eq!(&buf[..lens[0]], b"bb");
        assert_eq!(mock.inbox.borrow().len(), 1);
    }

    #[test]
    fn a_failure_on_the_first_packet_is_the_recv_error() {
        let mock = Mock::with_inbox(&[]);
        let mut buf = [0u8; 4];
        let mut lens = [0usize; 2];
        assert!(matches!(
            mock.recv_batch(&mut [&mut buf], &mut lens),
            Err(Error::Disconnected)
        ));

        // An oversize first packet is discarded, as `recv` discards it.
        let mock = Mock::with_inbox(&[b"too long", b"ok"]);
        assert!(matches!(
            mock.recv_batch(&mut [&mut buf], &mut lens),
            Err(Error::BufferTooSmall)
        ));
        assert_eq!(mock.recv_batch(&mut [&mut buf], &mut lens).unwrap(), 1);
        assert_eq!(&buf[..lens[0]], b"ok");
    }
}
