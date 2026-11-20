//! Input constraints specified by Wintun's public interface.

use std::io;

const MAX_NAME_UNITS: usize = 127;
const MIN_RING: u32 = 0x2_0000;
const MAX_RING: u32 = 0x400_0000;

pub(crate) fn wide_name(name: &str) -> io::Result<Vec<u16>> {
    let mut value: Vec<_> = name.encode_utf16().collect();
    if value.is_empty() || value.len() > MAX_NAME_UNITS || value.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid Wintun name",
        ));
    }
    value.push(0);
    Ok(value)
}

pub(crate) fn valid_ring_capacity(capacity: u32) -> bool {
    (MIN_RING..=MAX_RING).contains(&capacity) && capacity.is_power_of_two()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_limits_count_utf16_units_and_reject_embedded_nul() {
        assert_eq!(wide_name("tun").unwrap(), [116, 117, 110, 0]);
        assert!(wide_name("").is_err());
        assert!(wide_name("a\0b").is_err());
        assert!(wide_name(&"a".repeat(127)).is_ok());
        assert!(wide_name(&"a".repeat(128)).is_err());
        assert!(wide_name(&"😀".repeat(64)).is_err());
    }

    #[test]
    fn ring_capacity_must_be_a_power_of_two_in_the_documented_range() {
        assert!(!valid_ring_capacity(MIN_RING - 1));
        assert!(valid_ring_capacity(MIN_RING));
        assert!(!valid_ring_capacity(MIN_RING + 1));
        assert!(valid_ring_capacity(MAX_RING));
        assert!(!valid_ring_capacity(MAX_RING * 2));
    }
}
