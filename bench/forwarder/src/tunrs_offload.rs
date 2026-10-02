//! Buffers of the raw tun-rs `--offload` copy loops (all build sets).
//!
//! The loop shape follows tun-rs's own offload example: `recv_multiple`
//! reads one virtio-net frame into `original` and splits it into up to
//! `IDEAL_BATCH_SIZE` packets in `bufs` (each at `VIRTIO_NET_HDR_LEN`, with
//! its length in `sizes`), and `send_multiple` coalesces them through a
//! reused `GROTable` and writes them with their virtio-net headers. Every
//! buffer is allocated once, before the loop.

use tun_rs::{GROTable, IDEAL_BATCH_SIZE, VIRTIO_NET_HDR_LEN};

/// Room for the largest frame the kernel hands over: a virtio-net header and
/// a 64 KiB super-packet.
const ORIGINAL_LEN: usize = VIRTIO_NET_HDR_LEN + 65535;

/// Per-direction state of a tun-rs offload copy loop.
pub struct TunrsBatch {
    /// The raw frame `recv_multiple` reads.
    pub original: Vec<u8>,
    /// The split packets, each starting at [`TunrsBatch::OFFSET`].
    pub bufs: Vec<Vec<u8>>,
    /// The length of each packet in `bufs`.
    pub sizes: Vec<usize>,
    /// The coalescing table `send_multiple` reuses.
    pub gro: GROTable,
    /// Length of every `bufs` entry while receiving.
    recv_len: usize,
}

impl TunrsBatch {
    /// Where each packet starts in its buffer: `send_multiple` writes the
    /// virtio-net header in front of it.
    pub const OFFSET: usize = VIRTIO_NET_HDR_LEN;

    /// Buffers for packets of at most `mtu` bytes.
    ///
    /// Each packet buffer reserves enough capacity for `send_multiple` to
    /// coalesce a whole 64 KiB run into it without reallocating (tun-rs
    /// skips coalescing rather than grow a buffer), but is only `mtu` bytes
    /// long while receiving, so restoring it after a send zero-fills at
    /// most one packet's worth.
    pub fn new(mtu: u16) -> Self {
        let recv_len = Self::OFFSET + usize::from(mtu);
        let bufs = (0..IDEAL_BATCH_SIZE)
            .map(|_| {
                let mut buf = Vec::with_capacity(2 * Self::OFFSET + 65535);
                buf.resize(recv_len, 0);
                buf
            })
            .collect();
        Self {
            original: vec![0; ORIGINAL_LEN],
            bufs,
            sizes: vec![0; IDEAL_BATCH_SIZE],
            gro: GROTable::new(),
            recv_len,
        }
    }

    /// Trims the first `count` buffers to the packets `recv_multiple` put
    /// there, as `send_multiple` expects.
    pub fn trim(&mut self, count: usize) {
        for (buf, size) in self.bufs.iter_mut().zip(&self.sizes).take(count) {
            buf.truncate(Self::OFFSET + size);
        }
    }

    /// Restores the first `count` buffers to their receive length after a
    /// send (coalescing may have grown the head of a run).
    pub fn restore(&mut self, count: usize) {
        for buf in self.bufs.iter_mut().take(count) {
            buf.resize(self.recv_len, 0);
        }
    }
}
