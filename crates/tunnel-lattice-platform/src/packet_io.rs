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
    use std::cell::RefCell;

    use tunnel_lattice_core::{Error, Result};

    use super::PacketIo;

    /// Sends succeed until `fail_at` packets were sent; that send and every
    /// later one fail with `Error::Disconnected`.
    struct Mock {
        fail_at: usize,
        sent: RefCell<Vec<Vec<u8>>>,
    }

    impl Mock {
        fn new(fail_at: usize) -> Self {
            Self {
                fail_at,
                sent: RefCell::new(Vec::new()),
            }
        }
    }

    impl PacketIo for Mock {
        fn recv(&self, _buf: &mut [u8]) -> Result<usize> {
            Err(Error::Disconnected)
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
    }
}
