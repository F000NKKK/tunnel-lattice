//! Synchronous rtnetlink control plane: link lookups and link changes.
//!
//! One blocking `NETLINK_ROUTE` socket, owned by the device and used only for
//! control calls (open, `snapshot`, `apply`), never on the packet path. No
//! async runtime is involved, so every call here is legal from any thread,
//! inside or outside a runtime.
//!
//! Protocol rules:
//!
//! - every request carries `NLM_F_REQUEST | NLM_F_ACK` and a fresh sequence
//!   number, and a call completes only on the kernel's `NLMSG_ERROR` answer
//!   with that sequence number (an ACK, or a NACK decoded to its errno);
//! - messages with another sequence number (left over from an interrupted
//!   earlier call) are skipped, and datagrams not sent by the kernel are
//!   dropped;
//! - each change is its own `RTM_NEWLINK` message, so a failure names
//!   exactly one attribute (Linux may apply one attribute of a merged
//!   message and reject the next);
//! - the administrative state is changed through `ifi_change = IFF_UP`, an
//!   atomic set rather than a read-modify-write of the flags.
//!
//! The socket lives in the network namespace it was opened in. Once the
//! device moves to another namespace or is deleted, the kernel answers
//! `ENODEV`, which maps to `Error::NotFound`.

use std::io;

use netlink_packet_core::{
    NLM_F_ACK, NLM_F_REQUEST, NetlinkBuffer, NetlinkHeader, NetlinkMessage, NetlinkPayload,
};
use netlink_packet_route::RouteNetlinkMessage;
use netlink_packet_route::link::{LinkAttribute, LinkFlags, LinkMessage};
use netlink_sys::protocols::NETLINK_ROUTE;
use netlink_sys::{Socket, SocketAddr};
use tunnel_lattice_core::{Error, Result};

use crate::errno::{self, Class, Op};

/// Which link a lookup names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LinkQuery<'a> {
    /// By interface name (`IFLA_IFNAME`). The caller has already checked the
    /// name against the kernel's rules.
    Name(&'a str),
    /// By interface index.
    Index(u32),
}

/// One attribute change, sent as its own `RTM_NEWLINK` request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LinkChange {
    /// Set `IFLA_MTU`.
    Mtu(u32),
    /// Set the hardware address (`IFLA_ADDRESS`).
    Mac([u8; 6]),
    /// Set (`true`) or clear (`false`) `IFF_UP`, touching no other flag.
    AdminUp(bool),
}

/// The state of one link, decoded from an `RTM_NEWLINK` reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Link {
    /// The interface index.
    pub(crate) index: u32,
    /// The interface name (`IFLA_IFNAME`).
    pub(crate) name: String,
    /// `IFLA_MTU`.
    pub(crate) mtu: u32,
    /// The raw `ifi_flags` (`IFF_*` bits).
    pub(crate) flags: u32,
    /// The hardware address, or `None` when the link has none (a TUN
    /// interface's address length is zero, so the kernel sends no
    /// `IFLA_ADDRESS`).
    pub(crate) mac: Option<[u8; 6]>,
}

/// A blocking rtnetlink socket connected to the kernel.
#[derive(Debug)]
pub(crate) struct RouteSocket {
    socket: Socket,
    sequence: u32,
    datagram: Vec<u8>,
}

impl RouteSocket {
    /// Opens a socket in the caller's current network namespace.
    ///
    /// Extended ACKs and capped ACKs are requested best effort: they only
    /// change the diagnostic payload of `NLMSG_ERROR`, never its errno, so a
    /// kernel that refuses either option is still usable.
    pub(crate) fn open() -> Result<Self> {
        let socket = syscall(|| Socket::new(NETLINK_ROUTE))?;
        // Connecting to the kernel's address (port 0) autobinds the socket
        // and makes the kernel refuse unicasts to it from any other port.
        syscall(|| socket.connect(&SocketAddr::new(0, 0)))?;
        let _ = socket.set_ext_ack(true);
        let _ = socket.set_cap_ack(true);
        Ok(Self {
            socket,
            sequence: 0,
            datagram: Vec::new(),
        })
    }

    /// Looks one link up (`RTM_GETLINK`). A missing link is
    /// `Error::NotFound`.
    pub(crate) fn get_link(&mut self, query: LinkQuery<'_>) -> Result<Link> {
        let sequence = self.next_sequence();
        self.exchange(&encode_query(query, sequence), sequence)?
            .ok_or_else(errno::malformed)
    }

    /// Applies one change to the link with `index` (`RTM_NEWLINK`).
    pub(crate) fn set_link(&mut self, index: u32, change: LinkChange) -> Result<()> {
        let sequence = self.next_sequence();
        self.exchange(&encode_change(index, change, sequence), sequence)
            .map(drop)
    }

    fn next_sequence(&mut self) -> u32 {
        // Zero is skipped so a reply can never match an unset field.
        self.sequence = self.sequence.wrapping_add(1).max(1);
        self.sequence
    }

    /// Sends `request` and reads datagrams until the kernel acknowledges
    /// `sequence`. Returns the link reply, if one arrived before the ACK.
    fn exchange(&mut self, request: &[u8], sequence: u32) -> Result<Option<Link>> {
        let sent = syscall(|| self.socket.send(request, 0))?;
        if sent != request.len() {
            return Err(errno::malformed());
        }
        let mut reply = Reply::default();
        loop {
            self.receive()?;
            if decode_datagram(&self.datagram, sequence, &mut reply)? {
                return Ok(reply.link);
            }
        }
    }

    /// Reads the next datagram sent by the kernel into `self.datagram`,
    /// whole: its size is peeked first, so a large reply is never cut.
    fn receive(&mut self) -> Result<()> {
        loop {
            self.datagram.clear();
            let (size, _) = syscall(|| {
                self.socket
                    .recv_from(&mut self.datagram, libc::MSG_PEEK | libc::MSG_TRUNC)
            })?;
            self.datagram.clear();
            self.datagram.reserve(size);
            let (read, from) =
                syscall(|| self.socket.recv_from(&mut self.datagram, libc::MSG_TRUNC))?;
            if read > self.datagram.len() {
                return Err(errno::malformed());
            }
            if from.port_number() == 0 {
                return Ok(());
            }
        }
    }
}

/// Runs a socket call, retrying it on `EINTR` and mapping any other failure
/// through the netlink row of the errno table.
fn syscall<T>(mut call: impl FnMut() -> io::Result<T>) -> Result<T> {
    loop {
        match call() {
            Ok(value) => return Ok(value),
            Err(error) => match errno::classify_io(&error, Op::Netlink) {
                Class::Retry => {}
                Class::Fail(error) => return Err(error),
                _ => {
                    return Err(error
                        .raw_os_error()
                        .map_or_else(errno::malformed, errno::platform));
                }
            },
        }
    }
}

/// Encodes an `RTM_GETLINK` request.
pub(crate) fn encode_query(query: LinkQuery<'_>, sequence: u32) -> Vec<u8> {
    let mut link = LinkMessage::default();
    match query {
        LinkQuery::Name(name) => link.attributes.push(LinkAttribute::IfName(name.to_owned())),
        LinkQuery::Index(index) => link.header.index = index,
    }
    encode(RouteNetlinkMessage::GetLink(link), sequence)
}

/// Encodes an `RTM_NEWLINK` request carrying exactly one change.
pub(crate) fn encode_change(index: u32, change: LinkChange, sequence: u32) -> Vec<u8> {
    let mut link = LinkMessage::default();
    link.header.index = index;
    match change {
        LinkChange::Mtu(mtu) => link.attributes.push(LinkAttribute::Mtu(mtu)),
        LinkChange::Mac(mac) => link.attributes.push(LinkAttribute::Address(mac.to_vec())),
        LinkChange::AdminUp(up) => {
            link.header.flags = if up {
                LinkFlags::Up
            } else {
                LinkFlags::empty()
            };
            link.header.change_mask = LinkFlags::Up;
        }
    }
    encode(RouteNetlinkMessage::NewLink(link), sequence)
}

fn encode(message: RouteNetlinkMessage, sequence: u32) -> Vec<u8> {
    let mut header = NetlinkHeader::default();
    header.flags = NLM_F_REQUEST | NLM_F_ACK;
    header.sequence_number = sequence;
    let mut packet = NetlinkMessage::new(header, NetlinkPayload::InnerMessage(message));
    packet.finalize();
    let mut bytes = vec![0; packet.buffer_len()];
    packet.serialize(&mut bytes);
    bytes
}

/// What one exchange has collected so far.
#[derive(Debug, Default)]
pub(crate) struct Reply {
    /// The link from an `RTM_NEWLINK` reply, if one arrived.
    pub(crate) link: Option<Link>,
}

/// Decodes one datagram of the reply to the request with `sequence`.
///
/// Returns `Ok(true)` once the kernel's ACK for `sequence` is seen, and
/// `Ok(false)` when more datagrams are needed. A NACK is returned as its
/// mapped error; a message that breaks the protocol is
/// `Platform(Unknown)`.
pub(crate) fn decode_datagram(datagram: &[u8], sequence: u32, reply: &mut Reply) -> Result<bool> {
    let mut rest = datagram;
    while !rest.is_empty() {
        let buffer = NetlinkBuffer::new_checked(rest).map_err(|_| errno::malformed())?;
        let length = buffer.length() as usize;
        if buffer.sequence_number() == sequence {
            let message = NetlinkMessage::<RouteNetlinkMessage>::deserialize(&rest[..length])
                .map_err(|_| errno::malformed())?;
            match message.payload {
                NetlinkPayload::Error(error) => {
                    return match error.code {
                        None => Ok(true),
                        Some(code) => Err(nack(code.get())),
                    };
                }
                NetlinkPayload::InnerMessage(RouteNetlinkMessage::NewLink(link)) => {
                    reply.link = Some(decode_link(link)?);
                }
                NetlinkPayload::Done(_) | NetlinkPayload::Overrun(_) => {
                    return Err(errno::malformed());
                }
                // NLMSG_NOOP and unrelated rtnetlink messages carry nothing
                // this exchange needs.
                _ => {}
            }
        }
        // Messages are 4-byte aligned inside a datagram; the last one may
        // omit its padding.
        let next = length.next_multiple_of(4).min(rest.len());
        rest = &rest[next..];
    }
    Ok(false)
}

/// The error for a NACK carrying `code` (a negated errno).
fn nack(code: i32) -> Error {
    let errno = code.saturating_neg();
    match errno::classify(errno, Op::Netlink) {
        Class::Fail(error) => error,
        // A NACK is final: an interrupted or retryable kernel-side failure
        // is reported, never replayed here.
        _ => errno::platform(errno),
    }
}

fn decode_link(message: LinkMessage) -> Result<Link> {
    let mut name = None;
    let mut mtu = None;
    let mut mac = None;
    for attribute in message.attributes {
        match attribute {
            LinkAttribute::IfName(value) => name = Some(value),
            LinkAttribute::Mtu(value) => mtu = Some(value),
            LinkAttribute::Address(bytes) => {
                mac = Some(<[u8; 6]>::try_from(bytes).map_err(|_| errno::malformed())?);
            }
            _ => {}
        }
    }
    match (name, mtu) {
        (Some(name), Some(mtu)) => Ok(Link {
            index: message.header.index,
            name,
            mtu,
            flags: message.header.flags.bits(),
            mac,
        }),
        _ => Err(errno::malformed()),
    }
}

#[cfg(test)]
mod tests {
    use tunnel_lattice_core::PlatformErrorCode;

    use super::*;

    const RTM_NEWLINK: u16 = 16;
    const RTM_GETLINK: u16 = 18;
    const NLMSG_ERROR: u16 = 2;
    const NLMSG_DONE: u16 = 3;
    const NLMSG_NOOP: u16 = 1;
    const IFLA_ADDRESS: u16 = 1;
    const IFLA_IFNAME: u16 = 3;
    const IFLA_MTU: u16 = 4;
    const REQUEST_ACK: u16 = 0x01 | 0x04;
    const IFF_UP: u32 = 0x1;
    const IFF_RUNNING: u32 = 0x40;
    const ARPHRD_NONE: u16 = 0xfffe;
    const ARPHRD_ETHER: u16 = 1;

    /// One netlink message: header plus `payload`, padded to 4 bytes.
    fn nlmsg(kind: u16, flags: u16, sequence: u32, payload: &[u8]) -> Vec<u8> {
        let length = u32::try_from(16 + payload.len()).unwrap();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&length.to_ne_bytes());
        bytes.extend_from_slice(&kind.to_ne_bytes());
        bytes.extend_from_slice(&flags.to_ne_bytes());
        bytes.extend_from_slice(&sequence.to_ne_bytes());
        bytes.extend_from_slice(&0u32.to_ne_bytes());
        bytes.extend_from_slice(payload);
        bytes.resize(bytes.len().next_multiple_of(4), 0);
        bytes
    }

    /// A `struct ifinfomsg`.
    fn ifinfomsg(link_type: u16, index: u32, flags: u32, change: u32) -> Vec<u8> {
        let mut bytes = vec![0, 0];
        bytes.extend_from_slice(&link_type.to_ne_bytes());
        bytes.extend_from_slice(&index.to_ne_bytes());
        bytes.extend_from_slice(&flags.to_ne_bytes());
        bytes.extend_from_slice(&change.to_ne_bytes());
        bytes
    }

    /// One attribute, padded to 4 bytes.
    fn nla(kind: u16, data: &[u8]) -> Vec<u8> {
        let length = u16::try_from(4 + data.len()).unwrap();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&length.to_ne_bytes());
        bytes.extend_from_slice(&kind.to_ne_bytes());
        bytes.extend_from_slice(data);
        bytes.resize(bytes.len().next_multiple_of(4), 0);
        bytes
    }

    /// An `NLMSG_ERROR` answer: `code` (0 = ACK) plus the echoed request
    /// header and any extended-ACK attributes.
    fn ack(sequence: u32, code: i32, trailer: &[u8]) -> Vec<u8> {
        let mut payload = code.to_ne_bytes().to_vec();
        payload.extend_from_slice(&nlmsg(RTM_NEWLINK, REQUEST_ACK, sequence, &[]));
        payload.extend_from_slice(trailer);
        nlmsg(NLMSG_ERROR, 0, sequence, &payload)
    }

    fn link_reply(
        sequence: u32,
        link_type: u16,
        index: u32,
        flags: u32,
        attrs: &[Vec<u8>],
    ) -> Vec<u8> {
        let mut payload = ifinfomsg(link_type, index, flags, 0);
        for attr in attrs {
            payload.extend_from_slice(attr);
        }
        nlmsg(RTM_NEWLINK, 0, sequence, &payload)
    }

    fn is_unknown(error: &Error) -> bool {
        matches!(error, Error::Platform(PlatformErrorCode::Unknown))
    }

    #[cfg(target_endian = "little")]
    #[test]
    fn get_by_name_matches_the_literal_wire_bytes() {
        #[rustfmt::skip]
        let expected: [u8; 44] = [
            44, 0, 0, 0,   18, 0,   5, 0,   7, 0, 0, 0,   0, 0, 0, 0, // nlmsghdr
            0, 0,   0, 0,   0, 0, 0, 0,   0, 0, 0, 0,   0, 0, 0, 0,   // ifinfomsg
            9, 0,   3, 0,   b't', b'u', b'n', b'0', 0,   0, 0, 0,    // IFLA_IFNAME
        ];
        assert_eq!(encode_query(LinkQuery::Name("tun0"), 7), expected);
    }

    #[test]
    fn get_requests_encode_as_rtm_getlink_with_ack() {
        let mut by_name = ifinfomsg(0, 0, 0, 0);
        by_name.extend_from_slice(&nla(IFLA_IFNAME, b"tl-test\0"));
        assert_eq!(
            encode_query(LinkQuery::Name("tl-test"), 1),
            nlmsg(RTM_GETLINK, REQUEST_ACK, 1, &by_name)
        );
        assert_eq!(
            encode_query(LinkQuery::Index(42), u32::MAX),
            nlmsg(RTM_GETLINK, REQUEST_ACK, u32::MAX, &ifinfomsg(0, 42, 0, 0))
        );
    }

    #[test]
    fn each_change_is_one_rtm_newlink_with_one_attribute() {
        let mut mtu = ifinfomsg(0, 9, 0, 0);
        mtu.extend_from_slice(&nla(IFLA_MTU, &1400u32.to_ne_bytes()));
        assert_eq!(
            encode_change(9, LinkChange::Mtu(1400), 3),
            nlmsg(RTM_NEWLINK, REQUEST_ACK, 3, &mtu)
        );

        let address = [0x02, 0x00, 0x5e, 0x10, 0x20, 0x30];
        let mut mac = ifinfomsg(0, 9, 0, 0);
        mac.extend_from_slice(&nla(IFLA_ADDRESS, &address));
        assert_eq!(
            encode_change(9, LinkChange::Mac(address), 4),
            nlmsg(RTM_NEWLINK, REQUEST_ACK, 4, &mac)
        );
    }

    #[test]
    fn admin_state_sets_only_iff_up_through_the_change_mask() {
        assert_eq!(
            encode_change(5, LinkChange::AdminUp(true), 1),
            nlmsg(
                RTM_NEWLINK,
                REQUEST_ACK,
                1,
                &ifinfomsg(0, 5, IFF_UP, IFF_UP)
            )
        );
        assert_eq!(
            encode_change(5, LinkChange::AdminUp(false), 2),
            nlmsg(RTM_NEWLINK, REQUEST_ACK, 2, &ifinfomsg(0, 5, 0, IFF_UP))
        );
    }

    #[test]
    fn a_link_reply_then_its_ack_decode_in_one_datagram() {
        let mut datagram = link_reply(
            11,
            ARPHRD_NONE,
            7,
            IFF_UP | IFF_RUNNING,
            &[
                nla(IFLA_IFNAME, b"tun0\0"),
                nla(IFLA_MTU, &1500u32.to_ne_bytes()),
            ],
        );
        datagram.extend_from_slice(&ack(11, 0, &[]));
        let mut reply = Reply::default();
        assert!(decode_datagram(&datagram, 11, &mut reply).unwrap());
        assert_eq!(
            reply.link,
            Some(Link {
                index: 7,
                name: "tun0".to_owned(),
                mtu: 1500,
                flags: IFF_UP | IFF_RUNNING,
                mac: None,
            })
        );
    }

    #[test]
    fn a_tap_reply_carries_its_mac_and_may_span_two_datagrams() {
        let address = [0x02, 0, 0, 0, 0, 1];
        let first = link_reply(
            2,
            ARPHRD_ETHER,
            8,
            0,
            &[
                nla(IFLA_IFNAME, b"tap0\0"),
                nla(IFLA_MTU, &9000u32.to_ne_bytes()),
                nla(IFLA_ADDRESS, &address),
            ],
        );
        let mut reply = Reply::default();
        assert!(!decode_datagram(&first, 2, &mut reply).unwrap());
        assert!(decode_datagram(&ack(2, 0, &[]), 2, &mut reply).unwrap());
        let link = reply.link.unwrap();
        assert_eq!((link.index, link.mtu, link.mac), (8, 9000, Some(address)));
        assert_eq!(link.flags & IFF_UP, 0);
    }

    #[test]
    fn a_plain_ack_completes_a_change() {
        let mut reply = Reply::default();
        assert!(decode_datagram(&ack(3, 0, &[]), 3, &mut reply).unwrap());
        assert!(reply.link.is_none());
    }

    type ErrorCheck = fn(&Error) -> bool;

    #[test]
    fn nacks_map_through_the_errno_table() {
        let cases: [(i32, ErrorCheck); 5] = [
            (libc::ENODEV, Error::is_not_found),
            (libc::EPERM, Error::is_permission_denied),
            (libc::EEXIST, Error::is_already_exists),
            (libc::EOPNOTSUPP, Error::is_unsupported),
            (libc::EINTR, |e| {
                matches!(e, Error::Platform(PlatformErrorCode::Linux(libc::EINTR)))
            }),
        ];
        for (errno, check) in cases {
            let error =
                decode_datagram(&ack(4, -errno, &[]), 4, &mut Reply::default()).unwrap_err();
            assert!(check(&error), "errno {errno}: {error:?}");
        }
        let error =
            decode_datagram(&ack(4, -libc::ERANGE, &[]), 4, &mut Reply::default()).unwrap_err();
        assert!(matches!(
            error,
            Error::Platform(PlatformErrorCode::Linux(libc::ERANGE))
        ));
    }

    #[test]
    fn extended_ack_attributes_do_not_change_the_errno() {
        // NLMSGERR_ATTR_MSG (1) with a text, as the kernel appends under
        // NETLINK_EXT_ACK; the header carries NLM_F_ACK_TLVS (0x200).
        let trailer = nla(1, b"Invalid MTU\0");
        let mut payload = (-libc::EINVAL).to_ne_bytes().to_vec();
        payload.extend_from_slice(&nlmsg(RTM_NEWLINK, REQUEST_ACK, 5, &[]));
        payload.extend_from_slice(&trailer);
        let datagram = nlmsg(NLMSG_ERROR, 0x200, 5, &payload);
        let error = decode_datagram(&datagram, 5, &mut Reply::default()).unwrap_err();
        assert!(matches!(
            error,
            Error::Platform(PlatformErrorCode::Linux(libc::EINVAL))
        ));
        // An ACK with attributes attached is still an ACK.
        let mut reply = Reply::default();
        assert!(decode_datagram(&ack(6, 0, &trailer), 6, &mut reply).unwrap());
    }

    #[test]
    fn messages_for_other_sequence_numbers_are_skipped() {
        let mut datagram = ack(1, -libc::EPERM, &[]);
        datagram.extend_from_slice(&nlmsg(NLMSG_NOOP, 0, 9, &[]));
        datagram.extend_from_slice(&link_reply(
            1,
            ARPHRD_NONE,
            3,
            0,
            &[
                nla(IFLA_IFNAME, b"old\0"),
                nla(IFLA_MTU, &1u32.to_ne_bytes()),
            ],
        ));
        let mut reply = Reply::default();
        assert!(!decode_datagram(&datagram, 9, &mut reply).unwrap());
        assert!(reply.link.is_none());
        assert!(decode_datagram(&ack(9, 0, &[]), 9, &mut reply).unwrap());
    }

    #[test]
    fn protocol_violations_are_platform_unknown() {
        let mut reply = Reply::default();
        // Shorter than a netlink header.
        assert!(is_unknown(
            &decode_datagram(&[1, 2, 3], 1, &mut reply).unwrap_err()
        ));
        // A length field past the end of the datagram.
        let mut cut = ack(1, 0, &[]);
        cut.truncate(cut.len() - 4);
        assert!(is_unknown(
            &decode_datagram(&cut, 1, &mut reply).unwrap_err()
        ));
        // An NLMSG_ERROR too short for its code.
        assert!(is_unknown(
            &decode_datagram(&nlmsg(NLMSG_ERROR, 0, 1, &[0, 0]), 1, &mut reply).unwrap_err()
        ));
        // NLMSG_DONE never ends a non-dump request.
        assert!(is_unknown(
            &decode_datagram(&nlmsg(NLMSG_DONE, 0, 1, &0i32.to_ne_bytes()), 1, &mut reply)
                .unwrap_err()
        ));
        // A length field shorter than the header itself (0 would otherwise
        // never advance through the datagram).
        for length in [0u32, 4, 15] {
            let mut short = ack(1, 0, &[]);
            short[..4].copy_from_slice(&length.to_ne_bytes());
            assert!(is_unknown(
                &decode_datagram(&short, 1, &mut reply).unwrap_err()
            ));
        }
    }

    #[test]
    fn a_tun_reply_with_link_info_decodes() {
        // What the kernel reports for a TUN device: IFLA_LINKINFO (18)
        // carrying IFLA_INFO_KIND (1) "tun" and IFLA_INFO_DATA (2) with the
        // tun attributes (IFLA_TUN_TYPE = 1, IFLA_TUN_PI = 0, ...).
        let mut tun_data = nla(1, &[1]);
        tun_data.extend_from_slice(&nla(2, &[0]));
        tun_data.extend_from_slice(&nla(8, &[0]));
        let mut info = nla(1, b"tun\0");
        info.extend_from_slice(&nla(2, &tun_data));
        let mut datagram = link_reply(
            12,
            ARPHRD_NONE,
            21,
            0,
            &[
                nla(IFLA_IFNAME, b"tln0\0"),
                nla(IFLA_MTU, &1500u32.to_ne_bytes()),
                nla(18, &info),
            ],
        );
        datagram.extend_from_slice(&ack(12, 0, &[]));
        let mut reply = Reply::default();
        assert!(decode_datagram(&datagram, 12, &mut reply).unwrap());
        let link = reply.link.unwrap();
        assert_eq!(
            (link.index, link.name.as_str(), link.mac),
            (21, "tln0", None)
        );
    }

    #[test]
    fn link_replies_missing_fields_or_with_odd_addresses_are_rejected() {
        let no_mtu = link_reply(1, ARPHRD_NONE, 3, 0, &[nla(IFLA_IFNAME, b"tun0\0")]);
        assert!(is_unknown(
            &decode_datagram(&no_mtu, 1, &mut Reply::default()).unwrap_err()
        ));
        let no_name = link_reply(
            1,
            ARPHRD_NONE,
            3,
            0,
            &[nla(IFLA_MTU, &1500u32.to_ne_bytes())],
        );
        assert!(is_unknown(
            &decode_datagram(&no_name, 1, &mut Reply::default()).unwrap_err()
        ));
        let short_mac = link_reply(
            1,
            ARPHRD_ETHER,
            3,
            0,
            &[
                nla(IFLA_IFNAME, b"tap0\0"),
                nla(IFLA_MTU, &1500u32.to_ne_bytes()),
                nla(IFLA_ADDRESS, &[1, 2, 3, 4]),
            ],
        );
        assert!(is_unknown(
            &decode_datagram(&short_mac, 1, &mut Reply::default()).unwrap_err()
        ));
    }

    #[test]
    fn sequence_numbers_never_use_zero() {
        let mut socket = RouteSocket::open().unwrap();
        socket.sequence = u32::MAX;
        assert_eq!(socket.next_sequence(), 1);
        assert_eq!(socket.next_sequence(), 2);
    }

    // The tests below talk to the running kernel. They only read (no
    // privilege, no change to the host) and rely on the loopback interface
    // every network namespace has.

    #[test]
    fn looks_up_loopback_by_name_and_by_index() {
        let mut socket = RouteSocket::open().unwrap();
        let by_name = socket.get_link(LinkQuery::Name("lo")).unwrap();
        assert_eq!(by_name.name, "lo");
        assert!(by_name.mtu > 0);
        assert!(by_name.index > 0);
        let by_index = socket.get_link(LinkQuery::Index(by_name.index)).unwrap();
        assert_eq!(by_index.name, "lo");
        assert_eq!(by_index.index, by_name.index);
    }

    #[test]
    fn missing_links_are_not_found() {
        let mut socket = RouteSocket::open().unwrap();
        let error = socket
            .get_link(LinkQuery::Name("tl-absent-9z"))
            .unwrap_err();
        assert!(error.is_not_found(), "{error:?}");
        let error = socket
            .get_link(LinkQuery::Index(i32::MAX as u32))
            .unwrap_err();
        assert!(error.is_not_found(), "{error:?}");
        // The socket stays usable after a NACK.
        assert_eq!(socket.get_link(LinkQuery::Name("lo")).unwrap().name, "lo");
    }

    #[test]
    fn a_change_to_a_missing_link_changes_nothing_and_is_refused() {
        // `RTM_NEWLINK` is checked for CAP_NET_ADMIN before the link is
        // looked up: without it the answer is `PermissionDenied`, with it
        // `NotFound`. Either way no interface exists to be changed.
        let mut socket = RouteSocket::open().unwrap();
        let error = socket
            .set_link(i32::MAX as u32, LinkChange::AdminUp(false))
            .unwrap_err();
        assert!(
            error.is_not_found() || error.is_permission_denied(),
            "{error:?}"
        );
        assert_eq!(socket.get_link(LinkQuery::Name("lo")).unwrap().name, "lo");
    }
}
