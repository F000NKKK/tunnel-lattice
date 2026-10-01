//! Packet parsing and reflection for the real-device benchmark. Pure: no
//! I/O, so `tests/device_bench.rs` covers it without privilege.
//!
//! The benchmark's UDP socket, bound to the device's own address `local`,
//! sends datagrams to `peer`, an address the host routes into the device.
//! The reflector reads each one from the device, swaps source and
//! destination (addresses, ports and, for TAP, MAC addresses) in place, and
//! writes it back into the device, so the host delivers it to the socket.
//! Swapping keeps the IPv4 header checksum and the UDP checksum valid: both
//! are one's-complement sums over the same 16-bit words in another order.

use std::net::Ipv4Addr;

/// Length of an Ethernet II header (TAP framing).
pub const ETHERNET_HEADER: usize = 14;

/// Shortest Ethernet frame without its FCS; shorter ARP replies are padded.
pub const MIN_ETHERNET_FRAME: usize = 60;

/// A datagram whose payload starts with this ends the reflector's loop.
pub const STOP_MAGIC: &[u8; 8] = b"TLSTOP!!";

/// The MAC the reflector answers ARP requests for `peer` with: locally
/// administered, unicast.
pub const PEER_MAC: [u8; 6] = [0x02, 0x54, 0x4c, 0x00, 0x00, 0x02];

/// IPv4 + UDP header bytes added to every datagram payload.
pub const IPV4_UDP_HEADERS: usize = 28;

/// How packets are framed on the device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Framing {
    /// TUN: one raw IP packet per read.
    Ip,
    /// TAP: one Ethernet II frame per read.
    Ethernet,
}

/// The two addresses of the benchmark flow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Flow {
    /// The device's own address; the socket is bound to it.
    pub local: Ipv4Addr,
    /// The far end the host routes into the device.
    pub peer: Ipv4Addr,
}

/// What to do with one received packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// The packet was reflected in place; write it back unchanged in length.
    Reflect,
    /// An ARP reply of this many bytes was written into the reply buffer.
    Reply(usize),
    /// A stop datagram of the flow: end the loop, do not write it back.
    Stop,
    /// Not part of the flow (for example IPv6 neighbor discovery): drop it.
    Ignore,
}

/// Classifies `packet` and, for a datagram of `flow`, reflects it in place.
/// An ARP request for `flow.peer` (TAP only) gets a reply written into
/// `reply`, which must hold at least [`MIN_ETHERNET_FRAME`] bytes.
pub fn reflect(framing: Framing, flow: Flow, packet: &mut [u8], reply: &mut [u8]) -> Action {
    match framing {
        Framing::Ip => reflect_ipv4(flow, packet),
        Framing::Ethernet => {
            if packet.len() < ETHERNET_HEADER {
                return Action::Ignore;
            }
            match u16::from_be_bytes([packet[12], packet[13]]) {
                0x0800 => {
                    let action = reflect_ipv4(flow, &mut packet[ETHERNET_HEADER..]);
                    if action == Action::Reflect {
                        let (dst, src) = packet.split_at_mut(6);
                        dst.swap_with_slice(&mut src[..6]);
                    }
                    action
                }
                0x0806 => arp_reply(flow, packet, reply),
                _ => Action::Ignore,
            }
        }
    }
}

/// Reflects an IPv4/UDP datagram from `flow.local` to `flow.peer`.
fn reflect_ipv4(flow: Flow, packet: &mut [u8]) -> Action {
    if packet.len() < 20 || packet[0] >> 4 != 4 {
        return Action::Ignore;
    }
    let header = usize::from(packet[0] & 0x0f) * 4;
    let total = usize::from(u16::from_be_bytes([packet[2], packet[3]]));
    // Fragments (MF set or a non-zero offset) are never reflected.
    let fragment = u16::from_be_bytes([packet[6], packet[7]]) & 0x3fff;
    if header < 20
        || total < header + 8
        || total > packet.len()
        || fragment != 0
        || packet[9] != 17
        || packet[12..16] != flow.local.octets()
        || packet[16..20] != flow.peer.octets()
    {
        return Action::Ignore;
    }
    let payload = &packet[header + 8..total];
    if payload.starts_with(STOP_MAGIC) {
        return Action::Stop;
    }
    let (source, rest) = packet[12..20].split_at_mut(4);
    source.swap_with_slice(rest);
    let (source_port, rest) = packet[header..header + 4].split_at_mut(2);
    source_port.swap_with_slice(rest);
    Action::Reflect
}

/// Answers an Ethernet/IPv4 ARP request for `flow.peer` with [`PEER_MAC`].
fn arp_reply(flow: Flow, frame: &[u8], reply: &mut [u8]) -> Action {
    const ARP_FRAME: usize = ETHERNET_HEADER + 28;
    if frame.len() < ARP_FRAME || reply.len() < MIN_ETHERNET_FRAME {
        return Action::Ignore;
    }
    let arp = &frame[ETHERNET_HEADER..ARP_FRAME];
    // Ethernet/IPv4, 6-byte/4-byte addresses, operation 1 (request).
    if arp[..8] != [0x00, 0x01, 0x08, 0x00, 6, 4, 0x00, 0x01] || arp[24..28] != flow.peer.octets() {
        return Action::Ignore;
    }
    let requester_mac = &arp[8..14];
    let requester_ip = &arp[14..18];
    let reply = &mut reply[..MIN_ETHERNET_FRAME];
    reply.fill(0);
    reply[..6].copy_from_slice(requester_mac);
    reply[6..12].copy_from_slice(&PEER_MAC);
    reply[12..14].copy_from_slice(&[0x08, 0x06]);
    reply[14..22].copy_from_slice(&[0x00, 0x01, 0x08, 0x00, 6, 4, 0x00, 0x02]);
    reply[22..28].copy_from_slice(&PEER_MAC);
    reply[28..32].copy_from_slice(&flow.peer.octets());
    reply[32..38].copy_from_slice(requester_mac);
    reply[38..42].copy_from_slice(requester_ip);
    Action::Reply(MIN_ETHERNET_FRAME)
}
