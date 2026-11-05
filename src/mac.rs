//! Checked Ethernet unicast addresses.
use crate::{sha256::sha256, sys::cvt};
use std::{ffi::CStr, fmt, io, str::FromStr};

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

    /// Derives a stable address in Menhera's 60:a4:34:4x:xx:xx MAC-M range.
    ///
    /// Hashes the exact hostname bytes, one NUL, and the interface name bytes
    /// with SHA-256, without a trailing NUL or case normalization. Digest bytes
    /// `h` map to `[0x60, 0xa4, 0x34, 0x40 | (h[0] & 0x0f), h[1], h[2]]`.
    /// The 20-bit suffix can collide; use an explicit address to resolve one.
    pub fn from_names(hostname: &CStr, interface: &CStr) -> Self {
        let hash = sha256(&[hostname.to_bytes_with_nul(), interface.to_bytes()]);
        Self([0x60, 0xa4, 0x34, 0x40 | (hash[0] & 0x0f), hash[1], hash[2]])
    }

    pub(crate) fn for_interface(interface: &CStr) -> io::Result<Self> {
        // Linux limits the hostname to 64 bytes, excluding the terminating NUL.
        let mut hostname = [0u8; 65];
        // SAFETY: hostname is writable for the supplied length.
        cvt(unsafe { libc::gethostname(hostname.as_mut_ptr().cast(), hostname.len()) })?;
        let hostname = CStr::from_bytes_until_nul(&hostname)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "unterminated hostname"))?;
        Ok(Self::from_names(hostname, interface))
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
    fn derived_addresses_match_independent_hash_vectors() {
        for (host, interface, expected) in [
            (c"unquic-test-host", c"uq-test", "60:a4:34:49:63:0a"),
            (c"host", c"tap0", "60:a4:34:42:d7:f0"),
            (c"host", c"tap1", "60:a4:34:44:84:2a"),
            (c"Host", c"tap0", "60:a4:34:42:f2:ab"),
        ] {
            let mac = MacAddress::from_names(host, interface);
            assert_eq!(mac.to_string(), expected);
            assert_eq!(MacAddress::new(*mac.as_bytes()), Ok(mac));
        }
        let mut host = [b'a'; 65];
        host[64] = 0;
        let host = CStr::from_bytes_with_nul(&host).unwrap();
        assert_eq!(
            MacAddress::from_names(host, c"bbbbbbbbbbbbbbb").to_string(),
            "60:a4:34:4d:c9:8a"
        );
        assert_ne!(
            MacAddress::from_names(c"ab", c"c"),
            MacAddress::from_names(c"a", c"bc")
        );
    }

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
