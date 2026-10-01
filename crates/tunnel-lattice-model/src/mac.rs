use std::fmt;

/// A MAC (Ethernet hardware) address.
///
/// Stored as six octets in transmission order. Pure data with no OS
/// dependency, shaped like `net-lattice-model`'s own `MacAddress` so a
/// value converts between the two through `[u8; 6]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MacAddress([u8; 6]);

impl MacAddress {
    /// Creates a MAC address from six octets in transmission order.
    pub const fn new(octets: [u8; 6]) -> Self {
        Self(octets)
    }

    /// Returns the six octets in transmission order.
    pub const fn octets(&self) -> [u8; 6] {
        self.0
    }
}

impl From<[u8; 6]> for MacAddress {
    fn from(octets: [u8; 6]) -> Self {
        Self::new(octets)
    }
}

impl From<MacAddress> for [u8; 6] {
    fn from(mac: MacAddress) -> Self {
        mac.octets()
    }
}

impl fmt::Display for MacAddress {
    /// Lowercase hex octets separated by colons, e.g. `02:42:ab:cd:ef:01`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [a, b, c, d, e, g] = self.0;
        write!(f, "{a:02x}:{b:02x}:{c:02x}:{d:02x}:{e:02x}:{g:02x}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn displays_in_lowercase_colon_hex() {
        let mac = MacAddress::new([0x02, 0x42, 0xAB, 0xCD, 0xEF, 0x01]);
        assert_eq!(mac.to_string(), "02:42:ab:cd:ef:01");
    }

    #[test]
    fn octets_round_trip_through_every_conversion() {
        let octets = [0x02, 0, 0, 0, 0, 0xff];
        let mac = MacAddress::from(octets);
        assert_eq!(mac.octets(), octets);
        assert_eq!(<[u8; 6]>::from(mac), octets);
    }
}
