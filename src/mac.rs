//! Checked Ethernet unicast addresses.
use std::{fmt, str::FromStr};

/// A nonzero, unicast Ethernet address.
///
/// Both globally administered and locally administered addresses are accepted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MacAddress([u8; 6]);

impl MacAddress {
    /// Rejects the all-zero address and addresses with the multicast bit set.
    pub const fn new(bytes: [u8; 6]) -> Result<Self, InvalidMacAddress> {
        if bytes[0] & 1 != 0 || matches!(bytes, [0, 0, 0, 0, 0, 0]) {
            Err(InvalidMacAddress)
        } else {
            Ok(Self(bytes))
        }
    }

    /// Returns the six bytes in Ethernet wire order.
    pub const fn as_bytes(&self) -> &[u8; 6] {
        &self.0
    }
}

impl fmt::Display for MacAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [a, b, c, d, e, last] = self.0;
        write!(f, "{a:02x}:{b:02x}:{c:02x}:{d:02x}:{e:02x}:{last:02x}")
    }
}

impl FromStr for MacAddress {
    type Err = InvalidMacAddress;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        if text.len() != 17 {
            return Err(InvalidMacAddress);
        }
        let mut bytes = [0; 6];
        let mut parts = text.split(':');
        for byte in &mut bytes {
            let part = parts.next().ok_or(InvalidMacAddress)?;
            if part.len() != 2 || !part.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(InvalidMacAddress);
            }
            *byte = u8::from_str_radix(part, 16).map_err(|_| InvalidMacAddress)?;
        }
        if parts.next().is_some() {
            return Err(InvalidMacAddress);
        }
        Self::new(bytes)
    }
}

/// Invalid unicast MAC bytes or colon-separated hexadecimal text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidMacAddress;

impl fmt::Display for InvalidMacAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("expected a nonzero unicast MAC address in xx:xx:xx:xx:xx:xx format")
    }
}
impl std::error::Error for InvalidMacAddress {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_all_first_octets_and_rejects_zero() {
        for first in 0..=255 {
            assert_eq!(
                MacAddress::new([first, 0, 0, 0, 0, 1]).is_ok(),
                first & 1 == 0
            );
        }
        assert!(MacAddress::new([0; 6]).is_err());
        assert!(MacAddress::new([255; 6]).is_err());
    }

    #[test]
    fn parses_canonical_text_and_formats_lowercase() {
        for text in ["00:11:22:33:44:55", "02:AA:BB:CC:DD:EE"] {
            let mac: MacAddress = text.parse().unwrap();
            assert_eq!(mac.to_string(), text.to_lowercase());
            assert_eq!(MacAddress::new(*mac.as_bytes()), Ok(mac));
        }
        for text in [
            "",
            "00:00:00:00:00:00",
            "01:00:00:00:00:01",
            "ff:ff:ff:ff:ff:ff",
            "+2:00:00:00:00:01",
            "02-00-00-00-00-01",
            "02:0:00:00:00:001",
            "02:00:00:00:00:gg",
            "02:00:00:00:00:01:02",
            "02:00:00:00:00:é",
        ] {
            assert!(text.parse::<MacAddress>().is_err(), "{text}");
        }
    }
}
