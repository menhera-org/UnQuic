//! In-tree UUID representation and explicit cookie provisioning.

use std::{
    collections::HashMap,
    fmt,
    hash::{Hash, Hasher},
    io,
    str::FromStr,
};

/// A UUID in network byte order.
///
/// Debug output hides the value because session cookies are bearer secrets.
/// Display deliberately exposes it for provisioning; do not use it in logs.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct UnquicUuid([u8; 16]);

impl UnquicUuid {
    /// Preserves all 128 bits, without imposing a UUID version.
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// Returns the bytes used on the wire.
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// Generates a UUIDv4 for explicit provisioning.
    ///
    /// Blocks until kernel randomness is available. Session establishment must
    /// never call this to replace a configured cookie. Requests of at most
    /// 256 bytes complete in one call once the kernel pool is initialized;
    /// an interrupted call during early boot returns an error and the caller
    /// may retry.
    pub fn new_v4() -> io::Result<Self> {
        let mut bytes = [0; 16];
        // SAFETY: bytes is writable for exactly the supplied length.
        let n = unsafe { libc::getrandom(bytes.as_mut_ptr().cast(), bytes.len(), 0) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        if n as usize != bytes.len() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "getrandom returned fewer bytes than requested",
            ));
        }
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        Ok(Self(bytes))
    }
}

impl Hash for UnquicUuid {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        // Cookies are fixed-width; a slice-length prefix would add no information.
        state.write(&self.0);
    }
}

impl fmt::Debug for UnquicUuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("UnquicUuid([redacted])")
    }
}

impl fmt::Display for UnquicUuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, byte) in self.0.iter().enumerate() {
            if matches!(i, 4 | 6 | 8 | 10) {
                f.write_str("-")?;
            }
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// Invalid canonical UUID text. The input is never retained in the error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParseUuidError;

impl fmt::Display for ParseUuidError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("expected a UUID in 8-4-4-4-12 hexadecimal format")
    }
}
impl std::error::Error for ParseUuidError {}

impl FromStr for UnquicUuid {
    type Err = ParseUuidError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let input = text.as_bytes();
        if input.len() != 36 {
            return Err(ParseUuidError);
        }
        let mut bytes = [0; 16];
        let mut at = 0;
        for (i, byte) in bytes.iter_mut().enumerate() {
            if matches!(i, 4 | 6 | 8 | 10) {
                if input[at] != b'-' {
                    return Err(ParseUuidError);
                }
                at += 1;
            }
            *byte = (hex(input[at])? << 4) | hex(input[at + 1])?;
            at += 2;
        }
        Ok(Self(bytes))
    }
}

const fn hex(byte: u8) -> Result<u8, ParseUuidError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(ParseUuidError),
    }
}

/// Immutable cookie index. Construction rejects duplicate local cookies.
///
/// Small configurations use a short linear scan; larger ones use the standard
/// library's randomized hash table. Lookups allocate nothing and take no locks.
/// Returned indices always refer to the caller's original configuration order.
pub struct CookieIndex(CookieLookup);

enum CookieLookup {
    Small(Vec<(UnquicUuid, usize)>),
    Large(HashMap<UnquicUuid, usize>),
}

impl CookieIndex {
    /// Builds the lookup table once at startup.
    pub fn new(cookies: impl IntoIterator<Item = UnquicUuid>) -> Result<Self, DuplicateCookie> {
        let entries: Vec<_> = cookies
            .into_iter()
            .enumerate()
            .map(|(index, cookie)| (cookie, index))
            .collect();
        // Short contiguous scans beat hashing for small session sets.
        if entries.len() <= 16 {
            for (at, entry) in entries.iter().enumerate() {
                if entries[..at].iter().any(|previous| previous.0 == entry.0) {
                    return Err(DuplicateCookie);
                }
            }
            return Ok(Self(CookieLookup::Small(entries)));
        }
        let mut table = HashMap::with_capacity(entries.len());
        for (cookie, index) in entries {
            if table.insert(cookie, index).is_some() {
                return Err(DuplicateCookie);
            }
        }
        Ok(Self(CookieLookup::Large(table)))
    }

    /// Finds a configuration index without allocating for known or unknown cookies.
    #[inline]
    pub fn find(&self, cookie: UnquicUuid) -> Option<usize> {
        match &self.0 {
            CookieLookup::Small(entries) => entries
                .iter()
                .find(|entry| entry.0 == cookie)
                .map(|entry| entry.1),
            CookieLookup::Large(table) => table.get(&cookie).copied(),
        }
    }
}

/// Two configured sessions have the same cookie.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DuplicateCookie;

impl fmt::Display for DuplicateCookie {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("duplicate session cookie")
    }
}
impl std::error::Error for DuplicateCookie {}

#[cfg(feature = "bin")]
impl<'de> serde::Deserialize<'de> for UnquicUuid {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(feature = "uuid-compat")]
impl From<::uuid::Uuid> for UnquicUuid {
    fn from(value: ::uuid::Uuid) -> Self {
        Self::from_bytes(*value.as_bytes())
    }
}

#[cfg(feature = "uuid-compat")]
impl From<UnquicUuid> for ::uuid::Uuid {
    fn from(value: UnquicUuid) -> Self {
        Self::from_bytes(*value.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "uuid-compat")]
    #[test]
    fn uuid_conversions_preserve_every_bit() {
        for bytes in [[0; 16], [255; 16], [0x42; 16]] {
            let external = ::uuid::Uuid::from_bytes(bytes);
            let own = UnquicUuid::from(external);
            assert_eq!(own.as_bytes(), &bytes);
            assert_eq!(::uuid::Uuid::from(own), external);
        }
    }

    #[test]
    fn canonical_text_and_redacted_diagnostics() {
        let text = "00112233-4455-4677-8899-aabbccddeeff";
        let id: UnquicUuid = text.parse().unwrap();
        assert_eq!(
            id.as_bytes(),
            &[
                0, 0x11, 0x22, 0x33, 0x44, 0x55, 0x46, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
                0xee, 0xff
            ]
        );
        assert_eq!(id.to_string(), text);
        assert_eq!(text.to_uppercase().parse::<UnquicUuid>(), Ok(id));
        assert_eq!(format!("{id:?}"), "UnquicUuid([redacted])");
        for bad in [
            "",
            "00112233445546778899aabbccddeeff",
            "00112233_4455-4677-8899-aabbccddeeff",
            "g0112233-4455-4677-8899-aabbccddeeff",
            "00112233-4455-4677-8899-aabbccddeeé",
        ] {
            assert!(bad.parse::<UnquicUuid>().is_err());
        }
        for i in 0..text.len() {
            assert!(text[..i].parse::<UnquicUuid>().is_err());
        }
    }

    #[test]
    fn provisioning_sets_version_and_variant() {
        for _ in 0..16 {
            let id = UnquicUuid::new_v4().unwrap();
            assert_eq!(id.0[6] >> 4, 4);
            assert_eq!(id.0[8] >> 6, 2);
            assert_eq!(id.to_string().parse::<UnquicUuid>(), Ok(id));
        }
    }

    #[test]
    fn index_preserves_configuration_order_and_rejects_duplicates() {
        let cookies = [
            UnquicUuid([3; 16]),
            UnquicUuid([1; 16]),
            UnquicUuid([2; 16]),
        ];
        let index = CookieIndex::new(cookies).unwrap();
        for (i, cookie) in cookies.into_iter().enumerate() {
            assert_eq!(index.find(cookie), Some(i));
        }
        assert_eq!(index.find(UnquicUuid([4; 16])), None);
        assert!(CookieIndex::new([cookies[0], cookies[0]]).is_err());
        assert_eq!(CookieIndex::new([]).unwrap().find(cookies[0]), None);
    }
}
