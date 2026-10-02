//! Virtio-net header framing and segmentation offload for Linux TUN.
//!
//! A Linux TUN queue opened with `IFF_VNET_HDR` puts a 10-byte
//! `struct virtio_net_hdr` in front of every packet, in both directions.
//! With the kernel's TSO/USO offloads enabled, one read can return a
//! *super-packet* of up to 64 KiB whose header asks the reader to split it
//! into `gso_size`-byte segments, and one write can hand the kernel such a
//! super-packet for it to split.
//!
//! This module is the pure, I/O-free half of that framing:
//!
//! - [`VirtioNetHdr`] decodes and encodes the header. It is native-endian,
//!   the legacy framing a TUN device uses unless someone set
//!   `TUNSETVNETLE`/`TUNSETVNETBE` on it.
//! - [`SplitCursor`] validates one received frame, then writes it out one
//!   IP packet at a time into a caller buffer. It synthesizes each
//!   segment's IP and TCP/UDP headers with full checksums, or completes the
//!   partial checksum of a non-GSO packet.
//! - [`plan_run`] finds the run of adjacent same-flow packets at the head of
//!   a send batch that the kernel will split back into exactly those
//!   packets, and builds the header bytes to write in front of their
//!   payloads.
//! - [`checksum`], [`sum_words`] and [`fold`] compute the Internet checksum.
//!
//! Nothing here can panic on any input: offsets use checked arithmetic,
//! every slice access goes through `get`, and malformed input becomes a
//! [`DropReason`] (the caller drops the packet and reads again). Nothing
//! allocates, so the receive and send paths stay allocation-free.

use core::cmp::min;
use core::ops::Range;

/// Size of `struct virtio_net_hdr` without the `num_buffers` field, the
/// default (and the only supported) TUN vnet header size.
pub(crate) const VNET_HDR_LEN: usize = 10;

/// The largest IP packet (or GSO super-packet) a TUN device exchanges with
/// the kernel: tun sets no `tso_max_size`, so the legacy 64 KiB GSO limit
/// applies.
pub(crate) const MAX_PACKET_LEN: usize = 65_536;

/// Size of a receive staging buffer: the header, the largest super-packet
/// and one sentinel byte that only an oversized read can fill.
pub(crate) const STAGING_LEN: usize = VNET_HDR_LEN + MAX_PACKET_LEN + 1;

/// The most packets one coalesced write may carry. This is the kernel's
/// `UDP_MAX_SEGMENTS`, which it enforces on a UDP super-packet written to a
/// TUN device; TCP runs use the same bound.
///
/// It does not bound receiving: the split is lazy, so a received
/// super-packet of any segment count costs nothing extra, and dropping one
/// that splits into more segments (TCP with a small MSS) would lose data.
/// A received frame is bounded only by its length checks.
pub(crate) const MAX_SEGMENTS: usize = 128;

/// The header of a plain (non-GSO, no checksum offload) packet.
pub(crate) const VNET_HDR_NONE: [u8; VNET_HDR_LEN] = [0; VNET_HDR_LEN];

/// `VIRTIO_NET_HDR_GSO_NONE`: not a super-packet.
pub(crate) const GSO_NONE: u8 = 0;
/// `VIRTIO_NET_HDR_GSO_TCPV4`: an IPv4 TCP super-packet.
pub(crate) const GSO_TCPV4: u8 = 1;
/// `VIRTIO_NET_HDR_GSO_TCPV6`: an IPv6 TCP super-packet.
pub(crate) const GSO_TCPV6: u8 = 4;
/// `VIRTIO_NET_HDR_GSO_UDP_L4`: an IPv4 or IPv6 UDP super-packet (USO).
pub(crate) const GSO_UDP_L4: u8 = 5;
/// `VIRTIO_NET_HDR_GSO_ECN`: a modifier bit on a TCP `gso_type`.
pub(crate) const GSO_ECN: u8 = 0x80;
/// `VIRTIO_NET_HDR_F_NEEDS_CSUM`: the checksum from `csum_start` to the end
/// is partial and must be completed at `csum_start + csum_offset`.
pub(crate) const F_NEEDS_CSUM: u8 = 1;
/// `VIRTIO_NET_HDR_F_DATA_VALID`: the checksum was already verified.
/// Accepted by the split without a check of its own, so only the tests
/// name it.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const F_DATA_VALID: u8 = 2;

const PROTO_TCP: u8 = 6;
const PROTO_UDP: u8 = 17;
const IPV6_HOP_BY_HOP: u8 = 0;
const IPV6_DEST_OPTS: u8 = 60;
const IPV4_MIN_HDR_LEN: usize = 20;
const IPV6_HDR_LEN: usize = 40;
const TCP_MIN_HDR_LEN: usize = 20;
const UDP_HDR_LEN: usize = 8;
const TCP_CSUM_OFFSET: usize = 16;
const UDP_CSUM_OFFSET: usize = 6;
const TCP_FLAGS_OFFSET: usize = 13;
const TCP_FIN: u8 = 0x01;
const TCP_SYN: u8 = 0x02;
const TCP_RST: u8 = 0x04;
const TCP_PSH: u8 = 0x08;
const TCP_URG: u8 = 0x20;
const TCP_CWR: u8 = 0x80;
/// IPv4 "more fragments" flag plus the 13-bit fragment offset.
const IPV4_FRAGMENT_MASK: u16 = 0x3fff;
/// The longest IP plus L4 header a coalesced run can carry: an IPv4 header
/// with full options (60) plus a TCP header with full options (60).
const MAX_RUN_HDR_LEN: usize = 120;
const RUN_HEADER_CAP: usize = VNET_HDR_LEN + MAX_RUN_HDR_LEN;

/// Why a received frame was dropped. Every reason means the same thing to
/// the caller (drop the frame, read again); the variants exist so the tests
/// can tell the rejection paths apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DropReason {
    /// The frame is shorter than the vnet header.
    ShortHeader,
    /// The packet after the header is longer than [`MAX_PACKET_LEN`] (the
    /// read filled the staging sentinel).
    Oversized,
    /// Nothing follows the header.
    EmptyPacket,
    /// `gso_type` (ignoring [`GSO_ECN`]) is not NONE, TCPV4, TCPV6 or UDP_L4.
    UnknownGsoType,
    /// A super-packet without [`F_NEEDS_CSUM`], so `csum_start` does not
    /// locate its L4 header.
    MissingNeedsCsum,
    /// The IP version does not match `gso_type`, or is neither 4 nor 6.
    VersionMismatch,
    /// The IPv4 header length, or the IPv6 header and extension-header
    /// chain, does not fit in the packet.
    BadIpHeader,
    /// The (final) IP protocol is not the one `gso_type` names, or the IPv6
    /// chain holds an extension header other than hop-by-hop or destination
    /// options.
    ProtocolMismatch,
    /// An IPv4 fragment cannot be a super-packet.
    Fragmented,
    /// `csum_start` is not the L4 header offset.
    CsumStartMismatch,
    /// `csum_offset` is not the TCP (16) or UDP (6) checksum offset.
    CsumOffsetMismatch,
    /// The checksum field `csum_start + csum_offset` of a non-GSO packet
    /// does not fit in the packet.
    CsumOutOfRange,
    /// The TCP data offset is below the 20-byte minimum.
    BadL4Header,
    /// The L4 header runs past the end of the packet.
    L4HeaderOutOfRange,
    /// `gso_size` is zero.
    ZeroGsoSize,
    /// A super-packet with headers but no payload.
    EmptyPayload,
    /// A segment's length does not fit its IP or UDP length field.
    SegmentTooLarge,
    /// [`SplitCursor::write_next`] was given a frame other than the one the
    /// cursor validated.
    FrameMismatch,
}

/// The decoded `struct virtio_net_hdr` (native-endian legacy framing).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct VirtioNetHdr {
    /// `VIRTIO_NET_HDR_F_*` bits.
    pub(crate) flags: u8,
    /// `VIRTIO_NET_HDR_GSO_*` value, possibly with [`GSO_ECN`].
    pub(crate) gso_type: u8,
    /// Advisory header length. The kernel fills it with the skb's linear
    /// length on receive, which can exceed the headers, so the split derives
    /// the real header length itself and ignores this field.
    pub(crate) hdr_len: u16,
    /// Payload bytes per segment.
    pub(crate) gso_size: u16,
    /// Offset of the L4 header, where checksumming starts.
    pub(crate) csum_start: u16,
    /// Offset of the checksum field from `csum_start`.
    pub(crate) csum_offset: u16,
}

impl VirtioNetHdr {
    /// Decodes the header from the first [`VNET_HDR_LEN`] bytes of `frame`.
    pub(crate) fn decode(frame: &[u8]) -> Result<Self, DropReason> {
        let Some(&[flags, gso_type, h0, h1, s0, s1, c0, c1, o0, o1]) = frame.get(..VNET_HDR_LEN)
        else {
            return Err(DropReason::ShortHeader);
        };
        Ok(Self {
            flags,
            gso_type,
            hdr_len: u16::from_ne_bytes([h0, h1]),
            gso_size: u16::from_ne_bytes([s0, s1]),
            csum_start: u16::from_ne_bytes([c0, c1]),
            csum_offset: u16::from_ne_bytes([o0, o1]),
        })
    }

    /// Encodes the header into its native-endian wire form.
    pub(crate) fn encode(&self) -> [u8; VNET_HDR_LEN] {
        let [h0, h1] = self.hdr_len.to_ne_bytes();
        let [s0, s1] = self.gso_size.to_ne_bytes();
        let [c0, c1] = self.csum_start.to_ne_bytes();
        let [o0, o1] = self.csum_offset.to_ne_bytes();
        [self.flags, self.gso_type, h0, h1, s0, s1, c0, c1, o0, o1]
    }
}

/// Adds `data`, read as big-endian 16-bit words (an odd last byte padded
/// with zero), to the unfolded one's-complement accumulator `initial`.
///
/// Summing 32-bit words and folding later is equivalent to summing 16-bit
/// words, because 2^16 is congruent to 1 modulo 2^16 - 1. The accumulator
/// cannot wrap for any slice shorter than 2^34 bytes.
pub(crate) fn sum_words(data: &[u8], initial: u64) -> u64 {
    let mut acc = initial;
    let (quads, remainder) = data.as_chunks::<4>();
    for quad in quads {
        acc = acc.wrapping_add(u64::from(u32::from_be_bytes(*quad)));
    }
    let tail = match *remainder {
        [a, b, c] => u64::from(u16::from_be_bytes([a, b])) + u64::from(u16::from_be_bytes([c, 0])),
        [a, b] => u64::from(u16::from_be_bytes([a, b])),
        [a] => u64::from(u16::from_be_bytes([a, 0])),
        _ => 0,
    };
    acc.wrapping_add(tail)
}

/// Folds an accumulator from [`sum_words`] into a 16-bit one's-complement
/// sum (not complemented).
pub(crate) fn fold(mut acc: u64) -> u16 {
    while acc > 0xffff {
        acc = (acc & 0xffff) + (acc >> 16);
    }
    u16::try_from(acc).unwrap_or(u16::MAX)
}

/// The Internet checksum of `data` on top of the accumulator `initial`:
/// the complement of the folded sum. Over data that already carries a
/// correct checksum (and its pseudo-header in `initial`) it is zero.
pub(crate) fn checksum(data: &[u8], initial: u64) -> u16 {
    !fold(sum_words(data, initial))
}

/// The IP version of a packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ip {
    V4,
    V6,
}

/// The L4 protocol of a super-packet or run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum L4 {
    Tcp,
    Udp,
}

impl L4 {
    fn proto(self) -> u8 {
        match self {
            Self::Tcp => PROTO_TCP,
            Self::Udp => PROTO_UDP,
        }
    }

    fn csum_offset(self) -> usize {
        match self {
            Self::Tcp => TCP_CSUM_OFFSET,
            Self::Udp => UDP_CSUM_OFFSET,
        }
    }
}

fn byte(buf: &[u8], at: usize) -> Option<u8> {
    buf.get(at).copied()
}

fn range(at: usize, len: usize) -> Option<Range<usize>> {
    Some(at..at.checked_add(len)?)
}

fn be16(buf: &[u8], at: usize) -> Option<u16> {
    let bytes: [u8; 2] = buf.get(range(at, 2)?)?.try_into().ok()?;
    Some(u16::from_be_bytes(bytes))
}

fn be32(buf: &[u8], at: usize) -> Option<u32> {
    let bytes: [u8; 4] = buf.get(range(at, 4)?)?.try_into().ok()?;
    Some(u32::from_be_bytes(bytes))
}

/// Copies `src` into `dst`; `None` (instead of a panic) on a length
/// mismatch.
fn copy(dst: &mut [u8], src: &[u8]) -> Option<()> {
    if dst.len() != src.len() {
        return None;
    }
    dst.copy_from_slice(src);
    Some(())
}

fn put_byte(buf: &mut [u8], at: usize, value: u8) -> Option<()> {
    *buf.get_mut(at)? = value;
    Some(())
}

fn put_be16(buf: &mut [u8], at: usize, value: u16) -> Option<()> {
    copy(buf.get_mut(range(at, 2)?)?, &value.to_be_bytes())
}

fn put_be32(buf: &mut [u8], at: usize, value: u32) -> Option<()> {
    copy(buf.get_mut(range(at, 4)?)?, &value.to_be_bytes())
}

fn ip_version(pkt: &[u8]) -> Option<Ip> {
    match byte(pkt, 0)? >> 4 {
        4 => Some(Ip::V4),
        6 => Some(Ip::V6),
        _ => None,
    }
}

/// The unfolded pseudo-header sum for an L4 segment of `l4_len` bytes,
/// using the addresses in the fixed IP header of `pkt`.
fn pseudo_sum(pkt: &[u8], ip: Ip, proto: u8, l4_len: usize) -> Option<u64> {
    let addrs = match ip {
        Ip::V4 => pkt.get(12..20)?,
        Ip::V6 => pkt.get(8..40)?,
    };
    let len = u64::try_from(l4_len).ok()?;
    Some(sum_words(addrs, u64::from(proto).checked_add(len)?))
}

/// Whether a packet of `len` bytes fits its IP length field and, for UDP,
/// its UDP length field.
fn lengths_fit(ip: Ip, l4: L4, l4_off: usize, len: usize) -> bool {
    let fits = |n: Option<usize>| n.is_some_and(|n| u16::try_from(n).is_ok());
    let ip_fits = match ip {
        Ip::V4 => fits(Some(len)),
        Ip::V6 => fits(len.checked_sub(IPV6_HDR_LEN)),
    };
    let l4_fits = match l4 {
        L4::Tcp => true,
        L4::Udp => fits(len.checked_sub(l4_off)),
    };
    len <= MAX_PACKET_LEN && ip_fits && l4_fits
}

/// Validates the IP header of a super-packet carrying `proto` and returns
/// its L4 offset. IPv4 options and IPv6 hop-by-hop / destination-options
/// headers are allowed and copied opaquely into every segment; any other
/// IPv6 extension header (notably routing, which changes the pseudo-header
/// destination) is rejected.
fn l4_offset(pkt: &[u8], ip: Ip, proto: u8) -> Result<usize, DropReason> {
    match ip {
        Ip::V4 => {
            let ihl = usize::from(byte(pkt, 0).ok_or(DropReason::BadIpHeader)? & 0x0f) * 4;
            if ihl < IPV4_MIN_HDR_LEN || ihl > pkt.len() {
                return Err(DropReason::BadIpHeader);
            }
            if byte(pkt, 9) != Some(proto) {
                return Err(DropReason::ProtocolMismatch);
            }
            if be16(pkt, 6).ok_or(DropReason::BadIpHeader)? & IPV4_FRAGMENT_MASK != 0 {
                return Err(DropReason::Fragmented);
            }
            Ok(ihl)
        }
        Ip::V6 => {
            if pkt.len() < IPV6_HDR_LEN {
                return Err(DropReason::BadIpHeader);
            }
            let mut next = byte(pkt, 6).ok_or(DropReason::BadIpHeader)?;
            let mut off = IPV6_HDR_LEN;
            // Each step advances by at least 8 bytes and stops at the end of
            // the packet, so the walk is bounded.
            while next == IPV6_HOP_BY_HOP || next == IPV6_DEST_OPTS {
                let ext_len = off
                    .checked_add(1)
                    .and_then(|at| byte(pkt, at))
                    .and_then(|units| usize::from(units).checked_add(1)?.checked_mul(8))
                    .ok_or(DropReason::BadIpHeader)?;
                next = byte(pkt, off).ok_or(DropReason::BadIpHeader)?;
                off = off
                    .checked_add(ext_len)
                    .filter(|&end| end <= pkt.len())
                    .ok_or(DropReason::BadIpHeader)?;
            }
            if next != proto {
                return Err(DropReason::ProtocolMismatch);
            }
            Ok(off)
        }
    }
}

/// How a validated frame turns into packets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Plan {
    /// Not a super-packet: copy it whole, completing the checksum field at
    /// `csum.1` over `csum.0..` when [`F_NEEDS_CSUM`] was set.
    Whole { csum: Option<(usize, usize)> },
    /// A super-packet: `hdr_len` bytes of IP+L4 headers (the L4 header at
    /// `l4_off`) followed by `payload_len` bytes cut into `gso_size` pieces.
    Gso {
        ip: Ip,
        l4: L4,
        l4_off: usize,
        hdr_len: usize,
        gso_size: usize,
        payload_len: usize,
    },
}

/// What [`SplitCursor::write_next`] produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Segment {
    /// The next IP packet, this many bytes long, is at the start of the
    /// output buffer.
    Packet(usize),
    /// The output buffer is shorter than the next packet (`needed` bytes).
    /// That packet is dropped and the cursor has moved past it.
    BufferTooSmall {
        /// The length of the dropped packet.
        needed: usize,
    },
    /// Every packet of the frame has been produced.
    Done,
}

/// Lazily splits one received frame (vnet header plus packet) into IP
/// packets, one per [`write_next`](Self::write_next) call.
///
/// The cursor is plain data with no borrow of the frame, so it can live
/// next to the staging buffer it describes; every call takes the frame
/// again and checks it is the one that was validated. It never allocates:
/// each packet is assembled straight into the caller's buffer (headers
/// copied and patched, payload copied once, checksums computed in place).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SplitCursor {
    plan: Plan,
    frame_len: usize,
    count: usize,
    next: usize,
}

impl SplitCursor {
    /// A cursor with nothing pending, for staging state before the first
    /// read.
    pub(crate) const IDLE: Self = Self {
        plan: Plan::Whole { csum: None },
        frame_len: 0,
        count: 0,
        next: 0,
    };

    /// Validates `frame` (the bytes one read returned: vnet header, then
    /// packet) and returns a cursor over the packets it carries.
    ///
    /// A non-GSO frame carries one packet, whose contents are not
    /// inspected beyond the checksum range when [`F_NEEDS_CSUM`] is set. A
    /// TCPv4, TCPv6 or UDP (v4 or v6) super-packet is fully validated
    /// first, so [`write_next`](Self::write_next) cannot fail on it.
    pub(crate) fn new(frame: &[u8]) -> Result<Self, DropReason> {
        let hdr = VirtioNetHdr::decode(frame)?;
        let pkt = frame.get(VNET_HDR_LEN..).ok_or(DropReason::ShortHeader)?;
        if pkt.len() > MAX_PACKET_LEN {
            return Err(DropReason::Oversized);
        }
        if pkt.is_empty() {
            return Err(DropReason::EmptyPacket);
        }
        let (plan, count) = match hdr.gso_type & !GSO_ECN {
            GSO_NONE => {
                let csum = if hdr.flags & F_NEEDS_CSUM != 0 {
                    let start = usize::from(hdr.csum_start);
                    let field = start
                        .checked_add(usize::from(hdr.csum_offset))
                        .filter(|&field| field.checked_add(2).is_some_and(|end| end <= pkt.len()))
                        .ok_or(DropReason::CsumOutOfRange)?;
                    Some((start, field))
                } else {
                    None
                };
                (Plan::Whole { csum }, 1)
            }
            GSO_TCPV4 => gso_plan(&hdr, pkt, Some(Ip::V4), L4::Tcp)?,
            GSO_TCPV6 => gso_plan(&hdr, pkt, Some(Ip::V6), L4::Tcp)?,
            GSO_UDP_L4 => gso_plan(&hdr, pkt, None, L4::Udp)?,
            _ => return Err(DropReason::UnknownGsoType),
        };
        Ok(Self {
            plan,
            frame_len: frame.len(),
            count,
            next: 0,
        })
    }

    /// How many packets the frame carries in total.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn segments(&self) -> usize {
        self.count
    }

    /// How many packets have not been produced (or dropped) yet.
    pub(crate) fn remaining(&self) -> usize {
        self.count.saturating_sub(self.next)
    }

    /// Whether every packet has been produced (or dropped).
    pub(crate) fn is_finished(&self) -> bool {
        self.remaining() == 0
    }

    /// The length of the next packet, without producing or dropping it, so
    /// a caller can check it fits before [`write_next`](Self::write_next)
    /// consumes it. `None` when every packet has been produced, or when the
    /// cursor's plan cannot size it (the next `write_next` then ends the
    /// cursor with [`DropReason::FrameMismatch`]).
    pub(crate) fn next_len(&self) -> Option<usize> {
        if self.is_finished() {
            return None;
        }
        self.segment_len(self.next)
    }

    /// Writes the next packet of `frame` to the start of `out` and advances.
    ///
    /// `frame` must be the frame passed to [`new`](Self::new); a frame of a
    /// different length ends the cursor with
    /// [`DropReason::FrameMismatch`]. A too-short `out` drops only the
    /// current packet ([`Segment::BufferTooSmall`]).
    pub(crate) fn write_next(
        &mut self,
        frame: &[u8],
        out: &mut [u8],
    ) -> Result<Segment, DropReason> {
        if self.is_finished() {
            return Ok(Segment::Done);
        }
        let k = self.next;
        self.next = k.saturating_add(1);
        if frame.len() != self.frame_len {
            self.next = self.count;
            return Err(DropReason::FrameMismatch);
        }
        let last = self.next == self.count;
        let Some(len) = self.segment_len(k) else {
            self.next = self.count;
            return Err(DropReason::FrameMismatch);
        };
        let Some(dst) = out.get_mut(..len) else {
            return Ok(Segment::BufferTooSmall { needed: len });
        };
        let written = frame
            .get(VNET_HDR_LEN..)
            .and_then(|pkt| write_segment(self.plan, pkt, dst, k, last));
        match written {
            Some(()) => Ok(Segment::Packet(len)),
            None => {
                self.next = self.count;
                Err(DropReason::FrameMismatch)
            }
        }
    }

    fn segment_len(&self, k: usize) -> Option<usize> {
        match self.plan {
            Plan::Whole { .. } => self.frame_len.checked_sub(VNET_HDR_LEN),
            Plan::Gso {
                hdr_len,
                gso_size,
                payload_len,
                ..
            } => {
                let left = payload_len.checked_sub(k.checked_mul(gso_size)?)?;
                hdr_len.checked_add(min(gso_size, left))
            }
        }
    }
}

/// Validates a super-packet `pkt` against its header. `want` is the IP
/// version `gso_type` implies (`None` for UDP_L4, which allows both).
fn gso_plan(
    hdr: &VirtioNetHdr,
    pkt: &[u8],
    want: Option<Ip>,
    l4: L4,
) -> Result<(Plan, usize), DropReason> {
    if hdr.flags & F_NEEDS_CSUM == 0 {
        return Err(DropReason::MissingNeedsCsum);
    }
    let ip = ip_version(pkt).ok_or(DropReason::VersionMismatch)?;
    if want.is_some_and(|want| want != ip) {
        return Err(DropReason::VersionMismatch);
    }
    let l4_off = l4_offset(pkt, ip, l4.proto())?;
    if usize::from(hdr.csum_start) != l4_off {
        return Err(DropReason::CsumStartMismatch);
    }
    if usize::from(hdr.csum_offset) != l4.csum_offset() {
        return Err(DropReason::CsumOffsetMismatch);
    }
    let l4_hdr_len = match l4 {
        L4::Tcp => {
            let data_offset = l4_off
                .checked_add(12)
                .and_then(|at| byte(pkt, at))
                .ok_or(DropReason::L4HeaderOutOfRange)?;
            let len = usize::from(data_offset >> 4) * 4;
            if len < TCP_MIN_HDR_LEN {
                return Err(DropReason::BadL4Header);
            }
            len
        }
        L4::Udp => UDP_HDR_LEN,
    };
    let hdr_len = l4_off
        .checked_add(l4_hdr_len)
        .filter(|&len| len <= pkt.len())
        .ok_or(DropReason::L4HeaderOutOfRange)?;
    let gso_size = usize::from(hdr.gso_size);
    if gso_size == 0 {
        return Err(DropReason::ZeroGsoSize);
    }
    let payload_len = pkt.len().saturating_sub(hdr_len);
    if payload_len == 0 {
        return Err(DropReason::EmptyPayload);
    }
    // No segment-count cap: the payload is at most 64 KiB, so even one-byte
    // segments stay countable, and every per-segment field (IPv4 id, TCP
    // sequence) is computed with wrapping or checked arithmetic.
    let count = payload_len.div_ceil(gso_size);
    let largest = hdr_len.saturating_add(min(gso_size, payload_len));
    if !lengths_fit(ip, l4, l4_off, largest) {
        return Err(DropReason::SegmentTooLarge);
    }
    let plan = Plan::Gso {
        ip,
        l4,
        l4_off,
        hdr_len,
        gso_size,
        payload_len,
    };
    Ok((plan, count))
}

/// Assembles packet `k` of `pkt` into `dst`, which is exactly that packet's
/// length. `None` only if `pkt` is not the validated packet.
fn write_segment(plan: Plan, pkt: &[u8], dst: &mut [u8], k: usize, last: bool) -> Option<()> {
    let (ip, l4, l4_off, hdr_len, gso_size) = match plan {
        Plan::Whole { csum } => {
            copy(dst, pkt)?;
            if let Some((start, field)) = csum {
                // Virtio semantics: the field already holds the partial
                // pseudo-header sum; checksum from `start` to the end. A
                // zero result is sent as 0xffff, as the kernel does, since
                // a zero UDP checksum means "none".
                let sum = checksum(dst.get(start..)?, 0);
                put_be16(dst, field, if sum == 0 { 0xffff } else { sum })?;
            }
            return Some(());
        }
        Plan::Gso {
            ip,
            l4,
            l4_off,
            hdr_len,
            gso_size,
            ..
        } => (ip, l4, l4_off, hdr_len, gso_size),
    };
    let seg_len = dst.len();
    let payload_off = k.checked_mul(gso_size)?;
    let src = hdr_len.checked_add(payload_off)?;
    let payload_len = seg_len.checked_sub(hdr_len)?;
    copy(dst.get_mut(..hdr_len)?, pkt.get(..hdr_len)?)?;
    copy(dst.get_mut(hdr_len..)?, pkt.get(range(src, payload_len)?)?)?;

    match ip {
        Ip::V4 => {
            put_be16(dst, 2, u16::try_from(seg_len).ok()?)?;
            // The id is modulo 2^16, like the kernel's per-segment increment.
            let id = be16(pkt, 4)?.wrapping_add(u16::try_from(k & 0xffff).ok()?);
            put_be16(dst, 4, id)?;
            put_be16(dst, 10, 0)?;
            let sum = checksum(dst.get(..l4_off)?, 0);
            put_be16(dst, 10, sum)?;
        }
        Ip::V6 => put_be16(
            dst,
            4,
            u16::try_from(seg_len.checked_sub(IPV6_HDR_LEN)?).ok()?,
        )?,
    }

    let l4_len = seg_len.checked_sub(l4_off)?;
    match l4 {
        L4::Tcp => {
            // Sequence numbers are modulo 2^32.
            let seq =
                be32(pkt, l4_off.checked_add(4)?)?.wrapping_add(u32::try_from(payload_off).ok()?);
            put_be32(dst, l4_off.checked_add(4)?, seq)?;
            // As the kernel's TCP segmentation: FIN and PSH only on the last
            // segment, CWR only on the first.
            let flags_at = l4_off.checked_add(TCP_FLAGS_OFFSET)?;
            let mut flags = byte(pkt, flags_at)?;
            if !last {
                flags &= !(TCP_FIN | TCP_PSH);
            }
            if k > 0 {
                flags &= !TCP_CWR;
            }
            put_byte(dst, flags_at, flags)?;
        }
        L4::Udp => put_be16(dst, l4_off.checked_add(4)?, u16::try_from(l4_len).ok()?)?,
    }
    let field = l4_off.checked_add(l4.csum_offset())?;
    put_be16(dst, field, 0)?;
    let pseudo = pseudo_sum(dst, ip, l4.proto(), l4_len)?;
    let mut sum = checksum(dst.get(l4_off..)?, pseudo);
    if l4 == L4::Udp && sum == 0 {
        sum = 0xffff;
    }
    put_be16(dst, field, sum)
}

/// Header facts of one packet that may join a coalesced run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Flow {
    ip: Ip,
    l4: L4,
    l4_off: usize,
    hdr_len: usize,
    payload_len: usize,
    tcp_flags: u8,
}

/// Classifies `pkt` as a coalescing candidate: a well-formed, unfragmented
/// IPv4 (any options) or IPv6 (no extension headers) TCP or UDP packet
/// with a non-empty payload, consistent length fields and valid IP and L4
/// checksums, and for TCP no SYN, FIN, RST, URG or CWR. Checking the
/// checksums keeps coalescing from turning a corrupt packet into segments
/// with freshly computed, valid checksums.
fn coalescible(pkt: &[u8]) -> Option<Flow> {
    if pkt.len() > MAX_PACKET_LEN {
        return None;
    }
    let ip = ip_version(pkt)?;
    let (l4_off, proto) = match ip {
        Ip::V4 => {
            let ihl = usize::from(byte(pkt, 0)? & 0x0f) * 4;
            let consistent = ihl >= IPV4_MIN_HDR_LEN
                && usize::from(be16(pkt, 2)?) == pkt.len()
                && be16(pkt, 6)? & IPV4_FRAGMENT_MASK == 0
                && checksum(pkt.get(..ihl)?, 0) == 0;
            if !consistent {
                return None;
            }
            (ihl, byte(pkt, 9)?)
        }
        Ip::V6 => {
            if usize::from(be16(pkt, 4)?) != pkt.len().checked_sub(IPV6_HDR_LEN)? {
                return None;
            }
            (IPV6_HDR_LEN, byte(pkt, 6)?)
        }
    };
    let (l4, l4_hdr_len, tcp_flags) = match proto {
        PROTO_TCP => {
            let len = usize::from(byte(pkt, l4_off.checked_add(12)?)? >> 4) * 4;
            let flags = byte(pkt, l4_off.checked_add(TCP_FLAGS_OFFSET)?)?;
            if len < TCP_MIN_HDR_LEN
                || flags & (TCP_FIN | TCP_SYN | TCP_RST | TCP_URG | TCP_CWR) != 0
            {
                return None;
            }
            (L4::Tcp, len, flags)
        }
        PROTO_UDP => {
            let udp_len = usize::from(be16(pkt, l4_off.checked_add(4)?)?);
            // A zero UDP checksum means "none"; it cannot be validated.
            if udp_len != pkt.len().checked_sub(l4_off)? || be16(pkt, l4_off.checked_add(6)?)? == 0
            {
                return None;
            }
            (L4::Udp, UDP_HDR_LEN, 0)
        }
        _ => return None,
    };
    let hdr_len = l4_off.checked_add(l4_hdr_len)?;
    let payload_len = pkt.len().checked_sub(hdr_len)?;
    let l4_len = pkt.len().checked_sub(l4_off)?;
    let pseudo = pseudo_sum(pkt, ip, proto, l4_len)?;
    if payload_len == 0 || checksum(pkt.get(l4_off..)?, pseudo) != 0 {
        return None;
    }
    Some(Flow {
        ip,
        l4,
        l4_off,
        hdr_len,
        payload_len,
        tcp_flags,
    })
}

/// Whether `a` and `b` are equal over `r` (and both long enough).
fn same(a: &[u8], b: &[u8], r: Range<usize>) -> bool {
    match (a.get(r.clone()), b.get(r)) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

/// Whether `pkt` continues the run that starts at `first` and currently
/// ends at `prev`: headers identical to `first` except the fields the
/// kernel's split rewrites, the IPv4 id one past `prev`'s, and for TCP the
/// sequence number right after `prev`'s payload.
fn continues(first: &[u8], prev: &[u8], prev_flow: &Flow, pkt: &[u8], flow: &Flow) -> bool {
    let l4 = flow.l4_off;
    let ip_ok = match flow.ip {
        // Version/IHL and TOS; flags/fragment, TTL and protocol; addresses
        // and options. Not: total length, id, header checksum.
        Ip::V4 => {
            same(first, pkt, 0..2)
                && same(first, pkt, 6..10)
                && same(first, pkt, 12..l4)
                && be16(pkt, 4)
                    .is_some_and(|id| Some(id) == be16(prev, 4).map(|p| p.wrapping_add(1)))
        }
        // Version/class/flow label; next header, hop limit and addresses.
        // Not: payload length.
        Ip::V6 => same(first, pkt, 0..4) && same(first, pkt, 6..IPV6_HDR_LEN),
    };
    if !ip_ok {
        return false;
    }
    match flow.l4 {
        L4::Tcp => {
            let at = |off: usize| l4.saturating_add(off);
            let flags_match = byte(first, at(TCP_FLAGS_OFFSET)).map(|f| f & !TCP_PSH)
                == byte(pkt, at(TCP_FLAGS_OFFSET)).map(|f| f & !TCP_PSH);
            let next_seq = u32::try_from(prev_flow.payload_len)
                .ok()
                .zip(be32(prev, at(4)))
                .map(|(len, seq)| seq.wrapping_add(len));
            // Ports; ack and data offset; window; urgent pointer and
            // options. Not: sequence number, PSH, checksum.
            same(first, pkt, l4..at(4))
                && same(first, pkt, at(8)..at(TCP_FLAGS_OFFSET))
                && flags_match
                && same(first, pkt, at(14)..at(TCP_CSUM_OFFSET))
                && same(first, pkt, at(18)..flow.hdr_len)
                && next_seq.is_some()
                && next_seq == be32(pkt, at(4))
        }
        // Ports. Not: length, checksum.
        L4::Udp => same(first, pkt, l4..l4.saturating_add(4)),
    }
}

/// A run of adjacent same-flow packets at the head of a send batch, ready
/// to be written as one GSO super-packet: [`header`](Self::header)
/// followed by [`payload`](Self::payload) of each of the first
/// [`count`](Self::count) packets, in order.
///
/// The kernel splits that super-packet back into exactly the original
/// packets: every packet but the last carries `gso_size` payload bytes,
/// TCP sequence numbers are contiguous, IPv4 ids consecutive, and every
/// other header field is identical.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Run {
    count: usize,
    hdr_len: usize,
    total_len: usize,
    header: [u8; RUN_HEADER_CAP],
    header_len: usize,
}

impl Run {
    /// How many packets from the head of the batch the run covers (2 to
    /// [`MAX_SEGMENTS`]).
    pub(crate) fn count(&self) -> usize {
        self.count
    }

    /// The bytes to write before the payloads: the vnet header followed by
    /// the first packet's IP and L4 headers, patched for the whole run
    /// (lengths, IPv4 header checksum, the partial L4 checksum, and PSH
    /// from the last packet).
    pub(crate) fn header(&self) -> &[u8] {
        self.header.get(..self.header_len).unwrap_or(&[])
    }

    /// The payload of one of the run's packets: everything after its IP
    /// and L4 headers. Every packet of the run has the same header length.
    pub(crate) fn payload<'a>(&self, packet: &'a [u8]) -> &'a [u8] {
        packet.get(self.hdr_len..).unwrap_or(&[])
    }

    /// The length of the IP super-packet the run is written as (without
    /// the vnet header).
    pub(crate) fn total_len(&self) -> usize {
        self.total_len
    }
}

/// Finds the run of packets at the head of `packets` that can be written
/// as one GSO super-packet. UDP runs need the kernel's USO support
/// (`udp_gso`).
///
/// Returns `None` when fewer than two packets qualify; the caller then
/// writes `packets[0]` alone behind [`VNET_HDR_NONE`]. A run stops before
/// any packet that is not a coalescing candidate, belongs to another flow,
/// is not contiguous with the previous one, has more payload than the
/// first, or would exceed [`MAX_SEGMENTS`] packets or 64 KiB (and any IP or
/// UDP length field); it stops after a TCP packet with PSH and after any
/// packet shorter than the first.
pub(crate) fn plan_run(packets: &[&[u8]], udp_gso: bool) -> Option<Run> {
    let first: &[u8] = packets.first()?;
    let flow = coalescible(first)?;
    if flow.l4 == L4::Udp && !udp_gso {
        return None;
    }
    let gso_size = flow.payload_len;
    let mut count = 1;
    let mut total = first.len();
    let mut prev = first;
    let mut prev_flow = flow;
    for &pkt in packets.get(1..).unwrap_or(&[]) {
        if count >= MAX_SEGMENTS
            || prev_flow.payload_len < gso_size
            || prev_flow.tcp_flags & TCP_PSH != 0
        {
            break;
        }
        let Some(next) = coalescible(pkt) else { break };
        let Some(new_total) = total.checked_add(next.payload_len) else {
            break;
        };
        let fits = next.ip == flow.ip
            && next.l4 == flow.l4
            && next.l4_off == flow.l4_off
            && next.hdr_len == flow.hdr_len
            && next.payload_len <= gso_size
            && lengths_fit(flow.ip, flow.l4, flow.l4_off, new_total);
        if !fits || !continues(first, prev, &prev_flow, pkt, &flow) {
            break;
        }
        count += 1;
        total = new_total;
        prev = pkt;
        prev_flow = next;
    }
    if count < 2 {
        return None;
    }

    let gso_type = match (flow.ip, flow.l4) {
        (Ip::V4, L4::Tcp) => GSO_TCPV4,
        (Ip::V6, L4::Tcp) => GSO_TCPV6,
        (_, L4::Udp) => GSO_UDP_L4,
    };
    let vnet = VirtioNetHdr {
        flags: F_NEEDS_CSUM,
        gso_type,
        hdr_len: u16::try_from(flow.hdr_len).ok()?,
        gso_size: u16::try_from(gso_size).ok()?,
        csum_start: u16::try_from(flow.l4_off).ok()?,
        csum_offset: u16::try_from(flow.l4.csum_offset()).ok()?,
    };
    let header_len = VNET_HDR_LEN.checked_add(flow.hdr_len)?;
    let mut header = [0; RUN_HEADER_CAP];
    copy(header.get_mut(..VNET_HDR_LEN)?, &vnet.encode())?;
    let hdrs = header.get_mut(VNET_HDR_LEN..header_len)?;
    copy(hdrs, first.get(..flow.hdr_len)?)?;

    match flow.ip {
        Ip::V4 => {
            put_be16(hdrs, 2, u16::try_from(total).ok()?)?;
            put_be16(hdrs, 10, 0)?;
            let sum = checksum(hdrs.get(..flow.l4_off)?, 0);
            put_be16(hdrs, 10, sum)?;
        }
        Ip::V6 => put_be16(
            hdrs,
            4,
            u16::try_from(total.checked_sub(IPV6_HDR_LEN)?).ok()?,
        )?,
    }
    let l4_len = total.checked_sub(flow.l4_off)?;
    match flow.l4 {
        L4::Tcp => {
            let flags = flow.tcp_flags | (prev_flow.tcp_flags & TCP_PSH);
            put_byte(hdrs, flow.l4_off.checked_add(TCP_FLAGS_OFFSET)?, flags)?;
        }
        L4::Udp => put_be16(
            hdrs,
            flow.l4_off.checked_add(4)?,
            u16::try_from(l4_len).ok()?,
        )?,
    }
    // NEEDS_CSUM: the field holds the folded (not complemented)
    // pseudo-header sum over the whole run's L4 length.
    let partial = fold(pseudo_sum(hdrs, flow.ip, flow.l4.proto(), l4_len)?);
    put_be16(
        hdrs,
        flow.l4_off.checked_add(flow.l4.csum_offset())?,
        partial,
    )?;

    Some(Run {
        count,
        hdr_len: flow.hdr_len,
        total_len: total,
        header,
        header_len,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACK: u8 = 0x10;
    const ECE: u8 = 0x40;
    const SRC4: [u8; 4] = [10, 0, 0, 1];
    const DST4: [u8; 4] = [10, 0, 0, 2];
    const SRC6: [u8; 16] = [0xfd, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
    const DST6: [u8; 16] = [0xfd, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2];

    /// An independent, deliberately naive RFC 1071 checksum (16-bit words,
    /// fold after every add) to check the module's optimized one against.
    fn ref_checksum(data: &[u8]) -> u16 {
        let mut sum: u32 = 0;
        for pair in data.chunks(2) {
            let word = (u32::from(pair[0]) << 8) | u32::from(*pair.get(1).unwrap_or(&0));
            sum += word;
            while sum > 0xffff {
                sum = (sum & 0xffff) + (sum >> 16);
            }
        }
        !(sum as u16)
    }

    fn ref_pseudo(pkt: &[u8], ip: Ip, proto: u8, l4_len: usize) -> Vec<u8> {
        let mut p = Vec::new();
        match ip {
            Ip::V4 => {
                p.extend_from_slice(&pkt[12..20]);
                p.extend_from_slice(&[0, proto]);
                p.extend_from_slice(&(l4_len as u16).to_be_bytes());
            }
            Ip::V6 => {
                p.extend_from_slice(&pkt[8..40]);
                p.extend_from_slice(&(l4_len as u32).to_be_bytes());
                p.extend_from_slice(&[0, 0, 0, proto]);
            }
        }
        p
    }

    /// Whether `pkt`'s IPv4 header and L4 checksums verify with the
    /// reference implementation.
    fn ref_valid(pkt: &[u8], shape: Shape) -> bool {
        let (l4_off, _) = shape.offsets();
        if shape.ip == Ip::V4 && ref_checksum(&pkt[..l4_off]) != 0 {
            return false;
        }
        let mut data = ref_pseudo(pkt, shape.ip, shape.l4.proto(), pkt.len() - l4_off);
        data.extend_from_slice(&pkt[l4_off..]);
        ref_checksum(&data) == 0
    }

    /// One packet layout: IP version, L4 protocol and the number of IPv4
    /// option words / IPv6 hop-by-hop 8-byte units.
    #[derive(Clone, Copy, Debug)]
    struct Shape {
        ip: Ip,
        l4: L4,
        opts: usize,
    }

    const TCP4: Shape = Shape {
        ip: Ip::V4,
        l4: L4::Tcp,
        opts: 0,
    };
    const TCP6: Shape = Shape {
        ip: Ip::V6,
        l4: L4::Tcp,
        opts: 0,
    };
    const UDP4: Shape = Shape {
        ip: Ip::V4,
        l4: L4::Udp,
        opts: 0,
    };
    const UDP6: Shape = Shape {
        ip: Ip::V6,
        l4: L4::Udp,
        opts: 0,
    };

    impl Shape {
        /// (L4 offset, IP+L4 header length). TCP carries 12 option bytes.
        fn offsets(self) -> (usize, usize) {
            let l3 = match self.ip {
                Ip::V4 => 20 + 4 * self.opts,
                Ip::V6 => 40 + 8 * self.opts,
            };
            let l4 = match self.l4 {
                L4::Tcp => 32,
                L4::Udp => 8,
            };
            (l3, l3 + l4)
        }

        fn gso_type(self) -> u8 {
            match (self.ip, self.l4) {
                (Ip::V4, L4::Tcp) => GSO_TCPV4,
                (Ip::V6, L4::Tcp) => GSO_TCPV6,
                (_, L4::Udp) => GSO_UDP_L4,
            }
        }
    }

    fn payload(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 7 + 3) as u8).collect()
    }

    /// Builds one packet from scratch with correct lengths and checksums
    /// (computed with the reference checksum). `id` is ignored for IPv6,
    /// `seq` and `flags` for UDP. An IPv4 total length above 65535 is
    /// written as 0.
    fn build(shape: Shape, id: u16, seq: u32, flags: u8, data: &[u8]) -> Vec<u8> {
        let proto = shape.l4.proto();
        let mut p = Vec::new();
        match shape.ip {
            Ip::V4 => {
                let ihl = 5 + shape.opts as u8;
                p.extend_from_slice(&[0x40 | ihl, 0x02, 0, 0]);
                p.extend_from_slice(&id.to_be_bytes());
                p.extend_from_slice(&[0x40, 0, 64, proto, 0, 0]);
                p.extend_from_slice(&SRC4);
                p.extend_from_slice(&DST4);
                p.extend(std::iter::repeat_n(1u8, 4 * shape.opts));
            }
            Ip::V6 => {
                let next = if shape.opts > 0 {
                    IPV6_HOP_BY_HOP
                } else {
                    proto
                };
                p.extend_from_slice(&[0x60, 0x01, 0x23, 0x45, 0, 0, next, 64]);
                p.extend_from_slice(&SRC6);
                p.extend_from_slice(&DST6);
                if shape.opts > 0 {
                    let n = 8 * shape.opts;
                    // Next header, length in 8-byte units minus one, then
                    // one PadN option filling the rest.
                    p.extend_from_slice(&[proto, (shape.opts - 1) as u8, 1, (n - 4) as u8]);
                    p.extend(std::iter::repeat_n(0u8, n - 4));
                }
            }
        }
        let (l4_off, _) = shape.offsets();
        assert_eq!(p.len(), l4_off);
        match shape.l4 {
            L4::Tcp => {
                p.extend_from_slice(&40000u16.to_be_bytes());
                p.extend_from_slice(&5201u16.to_be_bytes());
                p.extend_from_slice(&seq.to_be_bytes());
                p.extend_from_slice(&0x0102_0304u32.to_be_bytes());
                p.extend_from_slice(&[0x80, flags, 0x20, 0x00, 0, 0, 0, 0]);
                // NOP, NOP, timestamps.
                p.extend_from_slice(&[1, 1, 8, 10, 0, 0, 0, 7, 0, 0, 0, 9]);
            }
            L4::Udp => {
                p.extend_from_slice(&40000u16.to_be_bytes());
                p.extend_from_slice(&4789u16.to_be_bytes());
                p.extend_from_slice(&[0, 0, 0, 0]);
            }
        }
        p.extend_from_slice(data);
        let len = p.len();
        match shape.ip {
            Ip::V4 => {
                let total = u16::try_from(len).unwrap_or(0);
                p[2..4].copy_from_slice(&total.to_be_bytes());
                let sum = ref_checksum(&p[..l4_off]);
                p[10..12].copy_from_slice(&sum.to_be_bytes());
            }
            Ip::V6 => {
                p[4..6].copy_from_slice(&((len - 40) as u16).to_be_bytes());
            }
        }
        if shape.l4 == L4::Udp {
            p[l4_off + 4..l4_off + 6].copy_from_slice(&((len - l4_off) as u16).to_be_bytes());
        }
        let mut data = ref_pseudo(&p, shape.ip, proto, len - l4_off);
        data.extend_from_slice(&p[l4_off..]);
        let mut sum = ref_checksum(&data);
        if shape.l4 == L4::Udp && sum == 0 {
            sum = 0xffff;
        }
        let field = l4_off + shape.l4.csum_offset();
        p[field..field + 2].copy_from_slice(&sum.to_be_bytes());
        p
    }

    /// Builds the frame the kernel would hand a reader: the vnet header
    /// and the super-packet, whose L4 checksum field holds the partial
    /// pseudo-header sum.
    fn super_frame(
        shape: Shape,
        id: u16,
        seq: u32,
        flags: u8,
        data: &[u8],
        gso_size: u16,
    ) -> Vec<u8> {
        let mut pkt = build(shape, id, seq, flags, data);
        let (l4_off, hdr_len) = shape.offsets();
        let partial = !ref_checksum(&ref_pseudo(
            &pkt,
            shape.ip,
            shape.l4.proto(),
            pkt.len() - l4_off,
        ));
        let field = l4_off + shape.l4.csum_offset();
        pkt[field..field + 2].copy_from_slice(&partial.to_be_bytes());
        let hdr = VirtioNetHdr {
            flags: F_NEEDS_CSUM,
            gso_type: shape.gso_type(),
            hdr_len: hdr_len as u16,
            gso_size,
            csum_start: l4_off as u16,
            csum_offset: shape.l4.csum_offset() as u16,
        };
        [hdr.encode().as_slice(), &pkt].concat()
    }

    /// The packets the kernel's segmentation would produce, built from
    /// scratch.
    fn expected(
        shape: Shape,
        id: u16,
        seq: u32,
        flags: u8,
        data: &[u8],
        gso: usize,
    ) -> Vec<Vec<u8>> {
        let n = data.len().div_ceil(gso);
        data.chunks(gso)
            .enumerate()
            .map(|(k, chunk)| {
                let mut f = flags;
                if k + 1 < n {
                    f &= !(TCP_FIN | TCP_PSH);
                }
                if k > 0 {
                    f &= !TCP_CWR;
                }
                build(
                    shape,
                    id.wrapping_add(k as u16),
                    seq.wrapping_add((k * gso) as u32),
                    f,
                    chunk,
                )
            })
            .collect()
    }

    fn split_all(frame: &[u8]) -> Result<Vec<Vec<u8>>, DropReason> {
        let mut cursor = SplitCursor::new(frame)?;
        let mut out = vec![0u8; MAX_PACKET_LEN];
        let mut packets = Vec::new();
        loop {
            match cursor.write_next(frame, &mut out)? {
                Segment::Packet(n) => packets.push(out[..n].to_vec()),
                Segment::Done => break,
                Segment::BufferTooSmall { needed } => panic!("full buffer too small for {needed}"),
            }
        }
        assert!(cursor.is_finished());
        assert_eq!(packets.len(), cursor.segments());
        Ok(packets)
    }

    fn assert_golden(shape: Shape, id: u16, seq: u32, flags: u8, data_len: usize, gso: usize) {
        let data = payload(data_len);
        let frame = super_frame(shape, id, seq, flags, &data, gso as u16);
        let got = split_all(&frame).unwrap();
        let want = expected(shape, id, seq, flags, &data, gso);
        assert_eq!(got.len(), want.len(), "{shape:?}");
        for (k, (g, w)) in got.iter().zip(&want).enumerate() {
            assert_eq!(g, w, "{shape:?} segment {k}");
            assert!(ref_valid(g, shape), "{shape:?} segment {k} checksums");
        }
    }

    fn frame_with(hdr: VirtioNetHdr, pkt: &[u8]) -> Vec<u8> {
        [hdr.encode().as_slice(), pkt].concat()
    }

    fn gso_hdr(shape: Shape, gso_size: u16) -> VirtioNetHdr {
        let (l4_off, hdr_len) = shape.offsets();
        VirtioNetHdr {
            flags: F_NEEDS_CSUM,
            gso_type: shape.gso_type(),
            hdr_len: hdr_len as u16,
            gso_size,
            csum_start: l4_off as u16,
            csum_offset: shape.l4.csum_offset() as u16,
        }
    }

    // ---- header and checksum ------------------------------------------

    #[test]
    fn header_decode_table() {
        let hdr = VirtioNetHdr {
            flags: F_NEEDS_CSUM | F_DATA_VALID,
            gso_type: GSO_TCPV4 | GSO_ECN,
            hdr_len: 0x0102,
            gso_size: 1448,
            csum_start: 20,
            csum_offset: 16,
        };
        let bytes = hdr.encode();
        assert_eq!(VirtioNetHdr::decode(&bytes), Ok(hdr));
        let mut longer = bytes.to_vec();
        longer.extend_from_slice(&[0xaa; 5]);
        assert_eq!(VirtioNetHdr::decode(&longer), Ok(hdr));
        assert_eq!(
            VirtioNetHdr::decode(&bytes[..9]),
            Err(DropReason::ShortHeader)
        );
        assert_eq!(VirtioNetHdr::decode(&[]), Err(DropReason::ShortHeader));
        assert_eq!(
            VirtioNetHdr::decode(&VNET_HDR_NONE),
            Ok(VirtioNetHdr::default())
        );
        #[cfg(target_endian = "little")]
        assert_eq!(bytes, [0x03, 0x81, 0x02, 0x01, 0xa8, 0x05, 20, 0, 16, 0]);
        #[cfg(target_endian = "big")]
        assert_eq!(bytes, [0x03, 0x81, 0x01, 0x02, 0x05, 0xa8, 0, 20, 0, 16]);
    }

    #[test]
    fn checksum_known_vector_and_reference_agreement() {
        // The textbook IPv4 header whose checksum is 0xb861.
        let mut header = [
            0x45, 0x00, 0x00, 0x73, 0x00, 0x00, 0x40, 0x00, 0x40, 0x11, 0x00, 0x00, 0xc0, 0xa8,
            0x00, 0x01, 0xc0, 0xa8, 0x00, 0xc7,
        ];
        assert_eq!(checksum(&header, 0), 0xb861);
        header[10..12].copy_from_slice(&0xb861u16.to_be_bytes());
        assert_eq!(checksum(&header, 0), 0);

        assert_eq!(checksum(&[], 0), 0xffff);
        assert_eq!(fold(0x1_fffe), 0xffff);
        assert_eq!(fold(u64::MAX), 0xffff);
        for len in 0..64 {
            let data = payload(len * 37 % 1500 + len);
            assert_eq!(
                checksum(&data, 0),
                ref_checksum(&data),
                "len {}",
                data.len()
            );
            // An initial accumulator equals summing it as leading words.
            let initial = sum_words(&[0x12, 0x34, 0x56, 0x78], 0);
            let mut joined = vec![0x12, 0x34, 0x56, 0x78];
            joined.extend_from_slice(&data);
            assert_eq!(checksum(&data, initial), ref_checksum(&joined));
        }
    }

    // ---- golden split vectors ------------------------------------------

    #[test]
    fn split_tcpv4_golden_with_fin_psh_cwr() {
        // 3 segments, the last short; FIN/PSH only last, CWR only first.
        let flags = ACK | ECE | TCP_CWR | TCP_PSH | TCP_FIN;
        assert_golden(TCP4, 0xfffe, 0xffff_ff00, flags, 1448 * 2 + 100, 1448);
    }

    #[test]
    fn split_tcpv6_golden() {
        assert_golden(TCP6, 0, 0x1000_0000, ACK | TCP_PSH, 1440 * 5, 1440);
    }

    #[test]
    fn split_udpv4_and_udpv6_golden() {
        assert_golden(UDP4, 0x00ff, 0, 0, 1200 * 3 + 17, 1200);
        assert_golden(UDP6, 0, 0, 0, 1232 * 4, 1232);
    }

    #[test]
    fn split_copies_ipv4_options_and_ipv6_extension_headers() {
        let tcp4_opts = Shape { opts: 10, ..TCP4 };
        let udp4_opts = Shape { opts: 1, ..UDP4 };
        let tcp6_ext = Shape { opts: 1, ..TCP6 };
        let udp6_ext = Shape { opts: 3, ..UDP6 };
        assert_golden(tcp4_opts, 7, 1, ACK, 1000 * 3 + 1, 1000);
        assert_golden(udp4_opts, 7, 0, 0, 500 * 2, 500);
        assert_golden(tcp6_ext, 0, 99, ACK | TCP_PSH, 1300 * 4 + 5, 1300);
        assert_golden(udp6_ext, 0, 0, 0, 700 * 3, 700);

        // Destination options after hop-by-hop: walk the whole chain.
        let mut pkt = build(Shape { opts: 2, ..UDP6 }, 0, 0, 0, &payload(64));
        // Shrink hop-by-hop to 8 bytes pointing at an 8-byte dest-opts
        // header (each holding one 4-byte PadN), followed by UDP.
        pkt[40..44].copy_from_slice(&[IPV6_DEST_OPTS, 0, 1, 4]);
        pkt[48..52].copy_from_slice(&[PROTO_UDP, 0, 1, 4]);
        let hdr = gso_hdr(Shape { opts: 2, ..UDP6 }, 32);
        assert_eq!(
            SplitCursor::new(&frame_with(hdr, &pkt)).map(|c| c.segments()),
            Ok(2)
        );
    }

    #[test]
    fn split_ecn_gso_bit_is_accepted() {
        let data = payload(300);
        let mut frame = super_frame(TCP4, 1, 1, ACK | TCP_CWR, &data, 100);
        frame[1] |= GSO_ECN;
        assert_eq!(
            split_all(&frame).unwrap(),
            expected(TCP4, 1, 1, ACK | TCP_CWR, &data, 100)
        );
    }

    // ---- non-GSO frames --------------------------------------------------

    #[test]
    fn needs_csum_completion_on_non_gso_frames() {
        for shape in [TCP4, UDP6, Shape { opts: 2, ..TCP6 }] {
            let pkt = build(shape, 3, 77, ACK, &payload(333));
            let mut partial = super_frame(shape, 3, 77, ACK, &payload(333), 1000);
            partial[1] = GSO_NONE; // a non-GSO packet with a partial checksum
            assert_ne!(&partial[VNET_HDR_LEN..], pkt.as_slice());
            assert_eq!(split_all(&partial).unwrap(), vec![pkt.clone()], "{shape:?}");
        }
    }

    #[test]
    fn non_gso_frames_without_needs_csum_are_copied_verbatim() {
        let junk: Vec<u8> = (0..77u8).collect();
        for flags in [0, F_DATA_VALID] {
            let hdr = VirtioNetHdr {
                flags,
                ..VirtioNetHdr::default()
            };
            assert_eq!(
                split_all(&frame_with(hdr, &junk)).unwrap(),
                vec![junk.clone()]
            );
        }
        let pkt = build(TCP4, 1, 1, ACK, &payload(10));
        assert_eq!(
            split_all(&[VNET_HDR_NONE.as_slice(), &pkt].concat()).unwrap(),
            vec![pkt]
        );
    }

    // ---- rejections ------------------------------------------------------

    #[test]
    fn rejects_every_malformed_frame() {
        use DropReason::*;
        let data = payload(3000);
        let tcp4 = super_frame(TCP4, 1, 1, ACK, &data, 1000);
        let tcp6 = super_frame(TCP6, 1, 1, ACK, &data, 1000);
        let udp4 = super_frame(UDP4, 1, 1, 0, &data, 1000);
        let new = |f: &[u8]| SplitCursor::new(f).map(|c| c.segments());
        let patched = |base: &[u8], at: usize, bytes: &[u8]| {
            let mut f = base.to_vec();
            f[at..at + bytes.len()].copy_from_slice(bytes);
            f
        };
        let pkt4 = &tcp4[VNET_HDR_LEN..];
        let hdr4 = gso_hdr(TCP4, 1000);

        assert_eq!(new(&tcp4), Ok(3));
        assert_eq!(new(&tcp4[..9]), Err(ShortHeader));
        assert_eq!(new(&[]), Err(ShortHeader));
        assert_eq!(new(&VNET_HDR_NONE), Err(EmptyPacket));
        assert_eq!(
            new(&vec![0u8; VNET_HDR_LEN + MAX_PACKET_LEN + 1]),
            Err(Oversized)
        );
        for gso_type in [2, 3, 6, 0x7f, 0xff] {
            assert_eq!(
                new(&patched(&tcp4, 1, &[gso_type])),
                Err(UnknownGsoType),
                "{gso_type}"
            );
        }
        assert_eq!(
            new(&patched(&tcp4, 0, &[F_DATA_VALID])),
            Err(MissingNeedsCsum)
        );
        assert_eq!(new(&patched(&tcp4, 1, &[GSO_TCPV6])), Err(VersionMismatch));
        assert_eq!(new(&patched(&tcp6, 1, &[GSO_TCPV4])), Err(VersionMismatch));
        assert_eq!(
            new(&patched(&udp4, VNET_HDR_LEN, &[0x55])),
            Err(VersionMismatch)
        );
        assert_eq!(
            new(&patched(&tcp4, VNET_HDR_LEN, &[0x44])),
            Err(BadIpHeader)
        );
        assert_eq!(new(&frame_with(hdr4, &[0x4f; 40])), Err(BadIpHeader));
        assert_eq!(new(&frame_with(hdr4, &pkt4[..1])), Err(BadIpHeader));
        assert_eq!(
            new(&frame_with(
                gso_hdr(TCP6, 1000),
                &tcp6[VNET_HDR_LEN..][..39]
            )),
            Err(BadIpHeader)
        );
        // A hop-by-hop header whose length runs past the packet.
        let mut hbh = build(Shape { opts: 1, ..UDP6 }, 0, 0, 0, &payload(16));
        hbh[41] = 200;
        assert_eq!(
            new(&frame_with(gso_hdr(Shape { opts: 1, ..UDP6 }, 8), &hbh)),
            Err(BadIpHeader)
        );
        assert_eq!(
            new(&patched(&tcp4, VNET_HDR_LEN + 9, &[PROTO_UDP])),
            Err(ProtocolMismatch)
        );
        assert_eq!(
            new(&patched(&tcp6, VNET_HDR_LEN + 6, &[43])),
            Err(ProtocolMismatch)
        );
        assert_eq!(
            new(&patched(&tcp4, VNET_HDR_LEN + 6, &[0x20, 0])),
            Err(Fragmented)
        );
        assert_eq!(
            new(&patched(&tcp4, VNET_HDR_LEN + 6, &[0x00, 0x08])),
            Err(Fragmented)
        );
        let at = 6; // csum_start
        assert_eq!(
            new(&patched(&tcp4, at, &0u16.to_ne_bytes())),
            Err(CsumStartMismatch)
        );
        assert_eq!(
            new(&patched(&tcp4, at, &24u16.to_ne_bytes())),
            Err(CsumStartMismatch)
        );
        assert_eq!(
            new(&patched(&tcp4, at, &u16::MAX.to_ne_bytes())),
            Err(CsumStartMismatch)
        );
        let at = 8; // csum_offset
        assert_eq!(
            new(&patched(&tcp4, at, &6u16.to_ne_bytes())),
            Err(CsumOffsetMismatch)
        );
        assert_eq!(
            new(&patched(&udp4, at, &16u16.to_ne_bytes())),
            Err(CsumOffsetMismatch)
        );
        let tcp_doff = VNET_HDR_LEN + 20 + 12;
        assert_eq!(new(&patched(&tcp4, tcp_doff, &[0x40])), Err(BadL4Header));
        assert_eq!(new(&patched(&tcp4, tcp_doff, &[0x00])), Err(BadL4Header));
        assert_eq!(
            new(&frame_with(hdr4, &pkt4[..20 + 12])),
            Err(L4HeaderOutOfRange)
        );
        assert_eq!(
            new(&frame_with(hdr4, &pkt4[..20 + 31])),
            Err(L4HeaderOutOfRange)
        );
        assert_eq!(
            new(&frame_with(gso_hdr(UDP4, 8), &pkt4[..20 + 7])),
            Err(ProtocolMismatch)
        );
        let udp_pkt4 = &udp4[VNET_HDR_LEN..];
        assert_eq!(
            new(&frame_with(gso_hdr(UDP4, 8), &udp_pkt4[..20 + 7])),
            Err(L4HeaderOutOfRange)
        );
        assert_eq!(
            new(&patched(&tcp4, 4, &0u16.to_ne_bytes())),
            Err(ZeroGsoSize)
        );
        assert_eq!(new(&frame_with(hdr4, &pkt4[..52])), Err(EmptyPayload));
        // Non-GSO checksum offload range.
        let none = |start: u16, offset: u16| VirtioNetHdr {
            flags: F_NEEDS_CSUM,
            csum_start: start,
            csum_offset: offset,
            ..VirtioNetHdr::default()
        };
        assert_eq!(new(&frame_with(none(0, 18), &[0; 20])), Ok(1));
        assert_eq!(new(&frame_with(none(0, 19), &[0; 20])), Err(CsumOutOfRange));
        assert_eq!(new(&frame_with(none(20, 0), &[0; 20])), Err(CsumOutOfRange));
        assert_eq!(
            new(&frame_with(none(u16::MAX, u16::MAX), &[0; 20])),
            Err(CsumOutOfRange)
        );
    }

    #[test]
    fn frame_mismatch_ends_the_cursor() {
        let frame = super_frame(TCP4, 1, 1, ACK, &payload(3000), 1000);
        let mut cursor = SplitCursor::new(&frame).unwrap();
        let mut out = vec![0u8; 2000];
        assert_eq!(
            cursor.write_next(&frame[..frame.len() - 1], &mut out),
            Err(DropReason::FrameMismatch)
        );
        assert!(cursor.is_finished());
        assert_eq!(cursor.write_next(&frame, &mut out), Ok(Segment::Done));
        // A same-length but different frame cannot cause a panic either.
        let mut cursor = SplitCursor::new(&frame).unwrap();
        let junk = vec![0xffu8; frame.len()];
        for _ in 0..4 {
            let _ = cursor.write_next(&junk, &mut out);
        }
    }

    // ---- boundaries ------------------------------------------------------

    #[test]
    fn gso_size_edges() {
        // gso_size equal to, one below and above the payload.
        assert_golden(TCP4, 1, 1, ACK | TCP_FIN | TCP_PSH, 1000, 1000);
        assert_golden(TCP4, 1, 1, ACK | TCP_FIN | TCP_PSH, 1000, 999);
        assert_golden(UDP6, 0, 0, 0, 1000, 1001);
        assert_golden(UDP4, 0, 0, 0, 1000, usize::from(u16::MAX));
        // The send-side coalescing cap does not bound receiving: more than
        // 128 one-byte (or small) segments split like any other frame.
        assert_golden(TCP6, 0, 5, ACK, MAX_SEGMENTS, 1);
        assert_golden(TCP6, 0, 5, ACK, MAX_SEGMENTS + 1, 1);
        assert_golden(UDP4, 0, 0, 0, MAX_SEGMENTS * 3 + 1, 3);
    }

    /// A TCP super-packet with a small MSS splits into far more than
    /// [`MAX_SEGMENTS`] segments, all delivered: the receive side has no
    /// segment cap. Covers a near-64 KiB IPv4 frame at an MSS of 88 (the
    /// kernel's `TCP_MIN_MSS`), with sequence-number and IPv4-id wraparound,
    /// and the extreme of one-byte segments.
    #[test]
    fn receive_split_is_not_capped_at_the_coalescing_limit() {
        let (_, hdr4) = TCP4.offsets();
        let data_len = 65_535 - hdr4;
        let count = data_len.div_ceil(88);
        assert!(count > MAX_SEGMENTS * 5, "{count}");
        assert_golden(
            TCP4,
            u16::MAX - 3,
            u32::MAX - 1000,
            ACK | TCP_PSH,
            data_len,
            88,
        );
        assert_golden(UDP6, 0, 0, 0, 4000, 1);
        let frame = super_frame(TCP4, 0, 1, ACK, &payload(data_len), 88);
        assert_eq!(SplitCursor::new(&frame).map(|c| c.segments()), Ok(count));
    }

    #[test]
    fn sixty_four_kib_boundaries() {
        // Exactly 65536 packet bytes (IPv6 payload length 65496) splits.
        let (_, hdr6) = TCP6.offsets();
        let data = payload(MAX_PACKET_LEN - hdr6);
        let frame = super_frame(TCP6, 0, 9, ACK, &data, 1440);
        assert_eq!(frame.len(), VNET_HDR_LEN + MAX_PACKET_LEN);
        assert_golden(TCP6, 0, 9, ACK, data.len(), 1440);
        // One byte more fills the staging sentinel.
        let mut over = frame.clone();
        over.push(0);
        assert_eq!(over.len(), STAGING_LEN);
        assert_eq!(SplitCursor::new(&over), Err(DropReason::Oversized));
        // The largest IPv4 packet (65535) splits; a single 65536-byte IPv4
        // segment cannot be expressed in the total-length field.
        let (_, hdr4) = TCP4.offsets();
        assert_golden(TCP4, 9, 9, ACK, 65_535 - hdr4, 1448);
        let frame = super_frame(TCP4, 9, 9, ACK, &payload(MAX_PACKET_LEN - hdr4), u16::MAX);
        assert_eq!(SplitCursor::new(&frame), Err(DropReason::SegmentTooLarge));
        let (_, hdr_u4) = UDP4.offsets();
        let frame = super_frame(UDP4, 9, 0, 0, &payload(MAX_PACKET_LEN - hdr_u4), u16::MAX);
        assert_eq!(SplitCursor::new(&frame), Err(DropReason::SegmentTooLarge));
    }

    #[test]
    fn short_buffer_drops_only_the_current_segment() {
        let data = payload(2500);
        let frame = super_frame(TCP4, 1, 1, ACK, &data, 1000);
        let want = expected(TCP4, 1, 1, ACK, &data, 1000);
        let mut cursor = SplitCursor::new(&frame).unwrap();
        assert_eq!(cursor.remaining(), 3);
        let mut out = vec![0u8; 2000];
        let needed = want[0].len();
        assert_eq!(
            cursor.write_next(&frame, &mut out[..needed - 1]),
            Ok(Segment::BufferTooSmall { needed })
        );
        assert_eq!(cursor.remaining(), 2);
        assert_eq!(
            cursor.write_next(&frame, &mut out),
            Ok(Segment::Packet(want[1].len()))
        );
        assert_eq!(&out[..want[1].len()], want[1].as_slice());
        // The last (short) segment fits a buffer of exactly its length.
        let last = want[2].len();
        assert_eq!(
            cursor.write_next(&frame, &mut out[..last]),
            Ok(Segment::Packet(last))
        );
        assert_eq!(&out[..last], want[2].as_slice());
        assert_eq!(cursor.write_next(&frame, &mut out), Ok(Segment::Done));
        assert!(cursor.is_finished());

        let mut idle = SplitCursor::IDLE;
        assert!(idle.is_finished());
        assert_eq!(idle.write_next(&frame, &mut out), Ok(Segment::Done));
    }

    /// `next_len` sizes the next segment without consuming it: asking any
    /// number of times leaves the cursor where it was, and the length is
    /// exactly what `write_next` then produces. `None` once finished.
    #[test]
    fn next_len_sizes_the_next_segment_without_consuming_it() {
        let data = payload(2500);
        let frame = super_frame(TCP4, 1, 1, ACK, &data, 1000);
        let want = expected(TCP4, 1, 1, ACK, &data, 1000);
        let mut cursor = SplitCursor::new(&frame).unwrap();
        let mut out = vec![0u8; 2000];
        for packet in &want {
            assert_eq!(cursor.next_len(), Some(packet.len()));
            assert_eq!(cursor.next_len(), Some(packet.len()), "asking again");
            let remaining = cursor.remaining();
            assert_eq!(
                cursor.write_next(&frame, &mut out),
                Ok(Segment::Packet(packet.len()))
            );
            assert_eq!(cursor.remaining(), remaining - 1);
        }
        assert_eq!(cursor.next_len(), None);
        assert_eq!(SplitCursor::IDLE.next_len(), None);

        // A non-GSO frame: one packet, the frame without its header.
        let plain = [VNET_HDR_NONE.as_slice(), &[0x45u8; 20]].concat();
        let cursor = SplitCursor::new(&plain).unwrap();
        assert_eq!(cursor.next_len(), Some(20));
    }

    // ---- coalescing --------------------------------------------------------

    /// Packets of one flow with `sizes[i]` payload bytes each, contiguous
    /// sequence numbers (wrapping) and consecutive IPv4 ids (wrapping). The
    /// last packet carries `last_flags`, the others `ACK`.
    fn flow(shape: Shape, sizes: &[usize], last_flags: u8) -> Vec<Vec<u8>> {
        flow_with(shape, sizes, ACK, last_flags)
    }

    fn flow_with(shape: Shape, sizes: &[usize], flags: u8, last_flags: u8) -> Vec<Vec<u8>> {
        let mut seq = 0xffff_f000u32;
        sizes
            .iter()
            .enumerate()
            .map(|(i, &n)| {
                let flags = if i + 1 == sizes.len() {
                    last_flags
                } else {
                    flags
                };
                let p = build(
                    shape,
                    0xfff0u16.wrapping_add(i as u16),
                    seq,
                    flags,
                    &payload(n),
                );
                seq = seq.wrapping_add(n as u32);
                p
            })
            .collect()
    }

    fn refs(packets: &[Vec<u8>]) -> Vec<&[u8]> {
        packets.iter().map(Vec::as_slice).collect()
    }

    fn run_frame(run: &Run, packets: &[&[u8]]) -> Vec<u8> {
        let mut frame = run.header().to_vec();
        for p in &packets[..run.count()] {
            frame.extend_from_slice(run.payload(p));
        }
        assert_eq!(frame.len(), VNET_HDR_LEN + run.total_len());
        frame
    }

    fn assert_round_trip(shape: Shape, packets: &[Vec<u8>], count: usize) {
        let r = refs(packets);
        let run = plan_run(&r, true).unwrap_or_else(|| panic!("{shape:?}: no run"));
        assert_eq!(run.count(), count, "{shape:?}");
        let frame = run_frame(&run, &r);
        let hdr = VirtioNetHdr::decode(&frame).unwrap();
        let (l4_off, hdr_len) = shape.offsets();
        assert_eq!(hdr.flags, F_NEEDS_CSUM);
        assert_eq!(hdr.gso_type, shape.gso_type());
        assert_eq!(usize::from(hdr.hdr_len), hdr_len);
        assert_eq!(usize::from(hdr.csum_start), l4_off);
        assert_eq!(usize::from(hdr.csum_offset), shape.l4.csum_offset());
        assert_eq!(usize::from(hdr.gso_size), packets[0].len() - hdr_len);
        assert_eq!(
            split_all(&frame).unwrap(),
            packets[..count].to_vec(),
            "{shape:?}"
        );
    }

    #[test]
    fn split_coalesce_round_trip() {
        let v4_opts = Shape { opts: 2, ..TCP4 };
        for shape in [TCP4, TCP6, UDP4, UDP6, v4_opts] {
            // Equal sizes, then a shorter last packet carrying PSH.
            let mut sizes = vec![1400; 6];
            sizes.push(321);
            let packets = flow(shape, &sizes, ACK | TCP_PSH);
            assert_round_trip(shape, &packets, 7);
            // All equal, no PSH.
            let packets = flow(shape, &[1000; 4], ACK);
            assert_round_trip(shape, &packets, 4);
        }
        // ECE is kept on every segment.
        let packets = flow_with(TCP4, &[500; 3], ACK | ECE, ACK | ECE | TCP_PSH);
        assert_round_trip(TCP4, &packets, 3);
    }

    /// The NEEDS_CSUM partial sum a run's L4 checksum field must hold,
    /// computed naively and only from the original packets: the folded,
    /// not complemented, ones' complement sum of the pseudo-header over
    /// the whole run's L4 length (the first packet's L4 header plus every
    /// packet's payload).
    fn naive_partial(shape: Shape, packets: &[Vec<u8>]) -> u16 {
        let (l4_off, hdr_len) = shape.offsets();
        let l4_len: usize =
            (hdr_len - l4_off) + packets.iter().map(|p| p.len() - hdr_len).sum::<usize>();
        let first = &packets[0];
        let (addrs, len_words) = match shape.ip {
            Ip::V4 => (&first[12..20], vec![l4_len as u32]),
            Ip::V6 => (
                &first[8..40],
                vec![(l4_len >> 16) as u32, (l4_len & 0xffff) as u32],
            ),
        };
        let mut sum: u32 = u32::from(shape.l4.proto());
        for word in len_words {
            sum += word;
        }
        for pair in addrs.chunks(2) {
            sum += (u32::from(pair[0]) << 8) | u32::from(pair[1]);
        }
        while sum > 0xffff {
            sum = (sum & 0xffff) + (sum >> 16);
        }
        sum as u16
    }

    /// A coalesced run's header carries, in its L4 checksum field, the
    /// pseudo-header partial sum NEEDS_CSUM asks for (checked against an
    /// independent naive computation), and completing that checksum over
    /// the super-packet, as the kernel or a device would, gives a valid
    /// L4 checksum.
    #[test]
    fn a_run_header_holds_the_pseudo_header_partial_sum() {
        let v4_opts = Shape { opts: 2, ..TCP4 };
        for shape in [TCP4, TCP6, UDP4, UDP6, v4_opts] {
            let mut short_last = vec![1400; 6];
            short_last.push(321);
            let cases = [
                flow(shape, &short_last, ACK | TCP_PSH),
                flow(shape, &[1000; 4], ACK),
                flow(shape, &[7; 3], ACK),
            ];
            for packets in &cases {
                let r = refs(packets);
                let run = plan_run(&r, true).unwrap_or_else(|| panic!("{shape:?}: no run"));
                assert_eq!(run.count(), packets.len(), "{shape:?}");
                let frame = run_frame(&run, &r);
                let (l4_off, _) = shape.offsets();
                let field = VNET_HDR_LEN + l4_off + shape.l4.csum_offset();
                let partial = u16::from_be_bytes([frame[field], frame[field + 1]]);
                assert_eq!(
                    partial,
                    naive_partial(shape, packets),
                    "{shape:?}, {} packets: the partial sum",
                    packets.len()
                );
                // NEEDS_CSUM completion: the complemented sum over the L4
                // bytes from `csum_start`, with the partial in the field.
                let mut pkt = frame[VNET_HDR_LEN..].to_vec();
                let l4_csum = ref_checksum(&pkt[l4_off..]);
                pkt[field - VNET_HDR_LEN..field - VNET_HDR_LEN + 2]
                    .copy_from_slice(&l4_csum.to_be_bytes());
                assert!(
                    ref_valid(&pkt, shape),
                    "{shape:?}, {} packets: the completed checksum",
                    packets.len()
                );
            }
        }
        // The largest UDP run: 128 segments.
        let packets = flow(UDP6, &[10; MAX_SEGMENTS], 0);
        let r = refs(&packets);
        let run = plan_run(&r, true).expect("a run");
        assert_eq!(run.count(), MAX_SEGMENTS);
        let frame = run_frame(&run, &r);
        let field = VNET_HDR_LEN + 40 + L4::Udp.csum_offset();
        assert_eq!(
            u16::from_be_bytes([frame[field], frame[field + 1]]),
            naive_partial(UDP6, &packets)
        );
    }

    #[test]
    fn run_caps_at_128_segments_and_64_kib() {
        let packets = flow(UDP4, &[10; MAX_SEGMENTS + 5], ACK);
        assert_round_trip(UDP4, &packets, MAX_SEGMENTS);

        // IPv4: total length must stay within 65535.
        let (_, hdr4) = TCP4.offsets();
        let packets = flow(TCP4, &[1448; 50], ACK);
        let run = plan_run(&refs(&packets), true).unwrap();
        let fit = (65_535 - hdr4) / 1448;
        assert_eq!(run.count(), fit);
        assert!(run.total_len() <= 65_535 && run.total_len() + 1448 > 65_535);
        assert_round_trip(TCP4, &packets, fit);

        // IPv6: up to 65536 bytes in total.
        let (_, hdr6) = TCP6.offsets();
        let size = (MAX_PACKET_LEN - hdr6) / 4;
        let packets = flow(TCP6, &[size; 5], ACK);
        let run = plan_run(&refs(&packets), true).unwrap();
        assert_eq!(run.count(), 4);
        assert_eq!(run.total_len(), hdr6 + 4 * size);
        assert_round_trip(TCP6, &packets, 4);
    }

    #[test]
    fn run_stops_after_psh_or_short_packet() {
        // PSH on the 2nd of 4: the run is the first two.
        let mut packets = flow(TCP4, &[800; 4], ACK);
        packets[1] = build(
            TCP4,
            0xfff1,
            0xffff_f000 + 800,
            ACK | TCP_PSH,
            &payload(800),
        );
        assert_round_trip(TCP4, &packets, 2);
        // A short 2nd packet ends the run after it.
        let packets = flow(UDP6, &[800, 500, 800], 0);
        assert_round_trip(UDP6, &packets, 2);
    }

    #[test]
    fn coalescing_refusals() {
        let run_len = |packets: &[Vec<u8>]| plan_run(&refs(packets), true).map(|r| r.count());
        let base = flow(TCP4, &[600; 3], ACK);
        assert_eq!(run_len(&base), Some(3));
        assert_eq!(plan_run(&[], true), None);
        assert_eq!(run_len(&base[..1]), None);

        let udp = flow(UDP4, &[600; 3], 0);
        assert_eq!(plan_run(&refs(&udp), false), None);
        assert_eq!(plan_run(&refs(&udp), true).map(|r| r.count()), Some(3));

        // Rewrites packet `i` at byte `at` and re-checksums it.
        let edit = |i: usize, at: usize, bytes: &[u8], shape: Shape, set: &[Vec<u8>]| {
            let mut set = set.to_vec();
            let p = &mut set[i];
            p[at..at + bytes.len()].copy_from_slice(bytes);
            refresh(p, shape);
            set
        };
        let l4 = 20;
        // Another destination port, a sequence gap, an id gap, another TTL,
        // ack, window, timestamp option.
        assert_eq!(run_len(&edit(1, l4 + 2, &[0, 1], TCP4, &base)), None);
        assert_eq!(run_len(&edit(1, l4 + 4, &[0, 0, 0, 1], TCP4, &base)), None);
        assert_eq!(run_len(&edit(1, 4, &[0, 0], TCP4, &base)), None);
        assert_eq!(run_len(&edit(1, 8, &[63], TCP4, &base)), None);
        assert_eq!(run_len(&edit(1, l4 + 8, &[9], TCP4, &base)), None);
        assert_eq!(run_len(&edit(1, l4 + 14, &[9], TCP4, &base)), None);
        assert_eq!(run_len(&edit(1, l4 + 31, &[8], TCP4, &base)), None);
        assert_eq!(run_len(&edit(2, 1, &[0x03], TCP4, &base)), Some(2)); // TOS/ECN
        // Forbidden flags on the first or a later packet; ECE mismatch.
        for flag in [TCP_SYN, TCP_FIN, TCP_RST, TCP_URG, TCP_CWR] {
            assert_eq!(
                run_len(&edit(0, l4 + 13, &[ACK | flag], TCP4, &base)),
                None,
                "{flag:#x}"
            );
            assert_eq!(
                run_len(&edit(1, l4 + 13, &[ACK | flag], TCP4, &base)),
                None,
                "{flag:#x}"
            );
        }
        assert_eq!(run_len(&edit(1, l4 + 13, &[ACK | ECE], TCP4, &base)), None);
        assert_eq!(
            run_len(&edit(0, l4 + 13, &[ACK | TCP_PSH], TCP4, &base)),
            None
        );
        // Bad checksums (not refreshed).
        let mut bad = base.clone();
        bad[1][l4 + 16] ^= 1;
        assert_eq!(run_len(&bad), None);
        let mut bad = base.clone();
        bad[1][10] ^= 1;
        assert_eq!(run_len(&bad), None);
        // Fragments; a total length that disagrees with the packet length.
        assert_eq!(run_len(&edit(1, 6, &[0x60, 0], TCP4, &base)), None);
        assert_eq!(run_len(&edit(0, 6, &[0x00, 0x01], TCP4, &base)), None);
        let mut padded = base.clone();
        padded[1].push(0);
        assert_eq!(run_len(&padded), None);
        // A bigger second payload; a pure ACK; mixed versions or protocols.
        assert_eq!(run_len(&flow(TCP4, &[600, 601], ACK)), None);
        assert_eq!(run_len(&flow(TCP4, &[0, 0], ACK)), None);
        let mixed = vec![base[0].clone(), flow(TCP6, &[600; 2], ACK)[1].clone()];
        assert_eq!(run_len(&mixed), None);
        let mixed = vec![base[0].clone(), udp[1].clone()];
        assert_eq!(run_len(&mixed), None);
        // Differing IPv4 option lengths.
        let mixed = vec![
            base[0].clone(),
            flow(Shape { opts: 1, ..TCP4 }, &[600; 2], ACK)[1].clone(),
        ];
        assert_eq!(run_len(&mixed), None);
        // IPv6 extension headers are never coalesced.
        assert_eq!(
            run_len(&flow(Shape { opts: 1, ..TCP6 }, &[600; 3], ACK)),
            None
        );
        // UDP: a zero (absent) checksum, another source port, a length
        // field that disagrees.
        let mut zero = udp.clone();
        zero[1][26..28].copy_from_slice(&[0, 0]);
        assert_eq!(run_len(&zero), None);
        assert_eq!(run_len(&edit(1, 20, &[0, 1], UDP4, &udp)), None);
        let mut short_len = udp.clone();
        short_len[1][24..26].copy_from_slice(&100u16.to_be_bytes());
        refresh(&mut short_len[1], UDP4);
        assert_eq!(run_len(&short_len), None);
        // Not IP at all.
        assert_eq!(run_len(&[vec![0x00; 60], vec![0x00; 60]]), None);
        assert_eq!(run_len(&[base[0].clone(), vec![]]), None);
    }

    /// Recomputes `p`'s IPv4 header and L4 checksums after an edit.
    fn refresh(p: &mut [u8], shape: Shape) {
        let (l4_off, _) = shape.offsets();
        if shape.ip == Ip::V4 {
            p[10..12].copy_from_slice(&[0, 0]);
            let sum = ref_checksum(&p[..l4_off]);
            p[10..12].copy_from_slice(&sum.to_be_bytes());
        }
        let field = l4_off + shape.l4.csum_offset();
        p[field..field + 2].copy_from_slice(&[0, 0]);
        let mut data = ref_pseudo(p, shape.ip, shape.l4.proto(), p.len() - l4_off);
        data.extend_from_slice(&p[l4_off..]);
        let mut sum = ref_checksum(&data);
        if shape.l4 == L4::Udp && sum == 0 {
            sum = 0xffff;
        }
        p[field..field + 2].copy_from_slice(&sum.to_be_bytes());
    }

    // ---- no-panic fuzzing --------------------------------------------------

    /// xorshift64: a deterministic, dependency-free generator.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n.max(1) as u64) as usize
        }

        fn byte(&mut self) -> u8 {
            self.next() as u8
        }
    }

    fn mutate(rng: &mut Rng, data: &mut Vec<u8>, focus: usize) {
        for _ in 0..=rng.below(4) {
            if data.is_empty() {
                break;
            }
            let at = if rng.below(4) == 0 {
                rng.below(data.len())
            } else {
                rng.below(data.len().min(focus))
            };
            data[at] = rng.byte();
        }
        if rng.below(8) == 0 {
            let len = rng.below(data.len() + 1);
            data.truncate(len);
        }
    }

    #[test]
    fn fuzz_split_never_panics() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        let mut none = super_frame(TCP6, 1, 1, ACK, &payload(900), 1000);
        none[1] = GSO_NONE;
        let bases = [
            super_frame(TCP4, 1, 1, ACK | TCP_PSH, &payload(3000), 1000),
            super_frame(Shape { opts: 3, ..TCP4 }, 1, 1, ACK, &payload(2000), 512),
            super_frame(Shape { opts: 2, ..TCP6 }, 1, 1, ACK, &payload(2500), 700),
            super_frame(UDP4, 1, 0, 0, &payload(1300), 64),
            super_frame(UDP6, 1, 0, 0, &payload(4000), 1232),
            super_frame(TCP6, 0, 0, ACK, &payload(MAX_SEGMENTS), 1),
            none,
        ];
        let mut out = vec![0u8; MAX_PACKET_LEN];
        let mut accepted = 0;
        for _ in 0..20_000 {
            let mut frame = match rng.below(4) {
                0 => (0..rng.below(200)).map(|_| rng.byte()).collect(),
                1 | 2 => {
                    let mut f = bases[rng.below(bases.len())].clone();
                    mutate(&mut rng, &mut f, VNET_HDR_LEN + 140);
                    f
                }
                _ => {
                    let mut f = bases[rng.below(bases.len())].clone();
                    for b in &mut f[..VNET_HDR_LEN] {
                        *b = rng.byte();
                    }
                    f
                }
            };
            if rng.below(64) == 0 {
                frame.resize(VNET_HDR_LEN + MAX_PACKET_LEN + rng.below(2), 0);
            }
            let Ok(mut cursor) = SplitCursor::new(&frame) else {
                continue;
            };
            accepted += 1;
            assert!((1..=MAX_PACKET_LEN).contains(&cursor.segments()));
            for step in 0..=cursor.segments() {
                let len = if rng.below(4) == 0 {
                    rng.below(1600)
                } else {
                    out.len()
                };
                let wrong = rng.below(200) == 0;
                let f = if wrong {
                    &frame[..frame.len() / 2]
                } else {
                    &frame[..]
                };
                match cursor.write_next(f, &mut out[..len]) {
                    Ok(Segment::Packet(n)) => assert!(n <= len && n <= frame.len()),
                    Ok(Segment::BufferTooSmall { needed }) => assert!(needed > len),
                    Ok(Segment::Done) => break,
                    Err(e) => {
                        assert!(wrong, "{e:?}");
                        break;
                    }
                }
                assert!(step < cursor.segments());
            }
        }
        assert!(accepted > 100, "fuzz accepted only {accepted} frames");
    }

    #[test]
    fn fuzz_plan_run_never_panics_and_round_trips() {
        let mut rng = Rng(0x2545_f491_4f6c_dd1d);
        let bases = [
            flow(TCP4, &[700, 700, 700, 300], ACK | TCP_PSH),
            flow(TCP6, &[1000; 4], ACK),
            flow(UDP4, &[400, 400, 400], 0),
            flow(UDP6, &[64; 6], 0),
            flow(Shape { opts: 1, ..TCP4 }, &[100; 3], ACK),
        ];
        let mut runs = 0;
        for _ in 0..20_000 {
            let mut set = bases[rng.below(bases.len())].clone();
            match rng.below(4) {
                0 => {}
                1 => {
                    let i = rng.below(set.len());
                    mutate(&mut rng, &mut set[i], 80);
                }
                2 => {
                    let i = rng.below(set.len());
                    set[i] = (0..rng.below(120)).map(|_| rng.byte()).collect();
                }
                _ => {
                    let len = rng.below(set.len() + 1);
                    set.truncate(len);
                }
            }
            let r = refs(&set);
            let Some(run) = plan_run(&r, rng.below(2) == 0) else {
                continue;
            };
            runs += 1;
            assert!((2..=MAX_SEGMENTS.min(r.len())).contains(&run.count()));
            let frame = run_frame(&run, &r);
            assert_eq!(split_all(&frame).unwrap(), set[..run.count()].to_vec());
        }
        assert!(runs > 100, "fuzz planned only {runs} runs");
    }
}
