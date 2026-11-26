//! Bounds-checked parsing of Darwin BPF records, independent of BPF I/O.
//!
//! The 64-bit Darwin BPF header uses a 32-bit timeval, followed by captured
//! and original lengths and the header length. Records start on four-byte
//! boundaries; `bh_hdrlen` includes any padding before the packet. See
//! Apple's `bsd/net/bpf.h` for the ABI. No unaligned struct reads are needed.

const HEADER_FIELDS_LEN: usize = 18;
const ALIGNMENT: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrameError {
    /// The record header, packet or padding is malformed or incomplete.
    Malformed,
    /// The kernel captured only part of the original packet.
    TruncatedCapture,
}

/// Sequentially walks the records returned by a single BPF read.
/// A malformed record ends iteration. A capture truncated by BPF is reported
/// and skipped when its record boundary is known, preserving later packets.
pub(crate) struct Frames<'a> {
    remaining: &'a [u8],
}

impl<'a> Frames<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Self { remaining: bytes }
    }

    fn next_record(&mut self) -> Result<&'a [u8], FrameError> {
        let data = self.remaining;
        if data.len() < HEADER_FIELDS_LEN {
            return Err(FrameError::Malformed);
        }
        let caplen = u32::from_ne_bytes(data[8..12].try_into().unwrap()) as usize;
        let datalen = u32::from_ne_bytes(data[12..16].try_into().unwrap()) as usize;
        let hdrlen = u16::from_ne_bytes(data[16..18].try_into().unwrap()) as usize;
        if hdrlen < HEADER_FIELDS_LEN || caplen > datalen {
            return Err(FrameError::Malformed);
        }
        let end = hdrlen.checked_add(caplen).ok_or(FrameError::Malformed)?;
        let packet = data.get(hdrlen..end).ok_or(FrameError::Malformed)?;
        // BPF_WORDALIGN is sizeof(int32_t) on both supported 64-bit macOS
        // architectures. The final record need not include trailing padding.
        let aligned = end
            .checked_add(ALIGNMENT - 1)
            .map(|n| n & !(ALIGNMENT - 1))
            .ok_or(FrameError::Malformed)?;
        let next = if aligned <= data.len() {
            aligned
        } else if end == data.len() {
            end
        } else {
            return Err(FrameError::Malformed);
        };
        self.remaining = &data[next..];
        if caplen != datalen {
            return Err(FrameError::TruncatedCapture);
        }
        Ok(packet)
    }
}

impl<'a> Iterator for Frames<'a> {
    type Item = Result<&'a [u8], FrameError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining.is_empty() {
            return None;
        }
        let record = self.next_record();
        if matches!(record, Err(FrameError::Malformed)) {
            self.remaining = &[];
        }
        Some(record)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(frame: &[u8], header_len: usize, original_len: u32) -> Vec<u8> {
        let mut bytes = vec![0; header_len];
        bytes[8..12].copy_from_slice(&(frame.len() as u32).to_ne_bytes());
        bytes[12..16].copy_from_slice(&original_len.to_ne_bytes());
        bytes[16..18].copy_from_slice(&(header_len as u16).to_ne_bytes());
        bytes.extend_from_slice(frame);
        bytes
    }

    #[test]
    fn reads_multiple_padded_records_and_final_unpadded_record() {
        let mut bytes = record(b"abc", 18, 3);
        bytes.resize((bytes.len() + 3) & !3, 0);
        bytes.extend_from_slice(&record(b"ethernet", 20, 8));
        let packets: Vec<_> = Frames::new(&bytes).collect();
        assert_eq!(packets, [Ok(&b"abc"[..]), Ok(&b"ethernet"[..])]);
    }

    #[test]
    fn rejects_incomplete_headers_packets_and_padding() {
        assert_eq!(
            Frames::new(&[0; 17]).next(),
            Some(Err(FrameError::Malformed))
        );
        let mut bytes = record(b"abc", 18, 3);
        bytes.truncate(bytes.len() - 1);
        assert_eq!(Frames::new(&bytes).next(), Some(Err(FrameError::Malformed)));
        let mut bytes = record(b"abc", 18, 3);
        bytes.extend_from_slice(&[0]); // some, but not all, alignment padding
        assert_eq!(Frames::new(&bytes).next(), Some(Err(FrameError::Malformed)));
    }

    #[test]
    fn rejects_short_header_and_capture_truncation() {
        let mut bytes = record(b"abc", 18, 3);
        bytes[16..18].copy_from_slice(&17u16.to_ne_bytes());
        assert_eq!(Frames::new(&bytes).next(), Some(Err(FrameError::Malformed)));
        let bytes = record(b"abc", 18, 4);
        assert_eq!(
            Frames::new(&bytes).next(),
            Some(Err(FrameError::TruncatedCapture))
        );
        let bytes = record(b"abc", 18, 2);
        assert_eq!(Frames::new(&bytes).next(), Some(Err(FrameError::Malformed)));
    }

    #[test]
    fn preserves_later_records_after_a_truncated_capture() {
        let mut bytes = record(b"bad", 18, 4);
        bytes.resize((bytes.len() + 3) & !3, 0);
        bytes.extend_from_slice(&record(b"good", 18, 4));
        let mut frames = Frames::new(&bytes);
        assert_eq!(frames.next(), Some(Err(FrameError::TruncatedCapture)));
        assert_eq!(frames.next(), Some(Ok(&b"good"[..])));
        assert_eq!(frames.next(), None);
    }

    #[test]
    fn stops_after_a_record_with_an_untrustworthy_boundary() {
        let mut bytes = record(b"bad", 18, 3);
        bytes[16..18].copy_from_slice(&17u16.to_ne_bytes());
        bytes.resize((bytes.len() + 3) & !3, 0);
        bytes.extend_from_slice(&record(b"good", 18, 4));
        let mut frames = Frames::new(&bytes);
        assert_eq!(frames.next(), Some(Err(FrameError::Malformed)));
        assert_eq!(frames.next(), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn offsets_match_the_platform_header() {
        assert_eq!(std::mem::offset_of!(libc::bpf_hdr, bh_caplen), 8);
        assert_eq!(std::mem::offset_of!(libc::bpf_hdr, bh_datalen), 12);
        assert_eq!(std::mem::offset_of!(libc::bpf_hdr, bh_hdrlen), 16);
    }
}
