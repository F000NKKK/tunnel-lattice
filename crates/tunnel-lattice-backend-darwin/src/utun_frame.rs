//! `utun` frames carry a four-byte network-order address family before IP.

const AF_INET: u32 = 2;
const AF_INET6: u32 = 30;

/// Distinguishes a consumed oversized packet from unrelated I/O errors.
#[derive(Debug)]
pub(crate) struct BufferTooSmall;

impl std::fmt::Display for BufferTooSmall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("packet buffer too small")
    }
}

impl std::error::Error for BufferTooSmall {}

pub(crate) fn address_family(packet: &[u8]) -> Option<u32> {
    match packet.first()? >> 4 {
        4 => Some(AF_INET),
        6 => Some(AF_INET6),
        _ => None,
    }
}

pub(crate) fn valid_family(family: [u8; 4], packet: &[u8]) -> bool {
    address_family(packet) == Some(u32::from_be_bytes(family))
}

#[cfg(test)]
fn encode(packet: &[u8]) -> Option<Vec<u8>> {
    let family = address_family(packet)?;
    let mut frame = Vec::with_capacity(4 + packet.len());
    frame.extend_from_slice(&family.to_be_bytes());
    frame.extend_from_slice(packet);
    Some(frame)
}

#[cfg(test)]
fn decode(frame: &[u8]) -> Option<&[u8]> {
    let (header, packet) = frame.split_at_checked(4)?;
    valid_family(header.try_into().ok()?, packet).then_some(packet)
}

#[test]
fn round_trip_both_ip_families() {
    for (packet, family) in [(&[0x45, 1, 2][..], AF_INET), (&[0x60, 3, 4][..], AF_INET6)] {
        let frame = encode(packet).unwrap();
        assert_eq!(&frame[..4], &family.to_be_bytes());
        assert_eq!(decode(&frame), Some(packet));
    }
}

#[test]
fn reject_missing_unknown_or_mismatched_headers() {
    assert_eq!(encode(&[]), None);
    assert_eq!(encode(&[0x70]), None);
    assert_eq!(decode(&[0, 0, 0]), None);
    assert_eq!(decode(&[0, 0, 0, 2]), None);
    assert_eq!(decode(&[0, 0, 0, 2, 0x60]), None);
    assert_eq!(decode(&[0, 0, 0, 99, 0x45]), None);
}
