//! In-tree UUID representation and explicit cookie provisioning.

use std::{fmt, io, str::FromStr};

/// A UUID in network byte order.
///
/// Debug output hides the value because session cookies are bearer secrets.
/// Display deliberately exposes it for provisioning; do not use it in logs.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
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
    /// never call this to replace a configured cookie.
    pub fn new_v4() -> io::Result<Self> {
        let mut bytes = [0; 16];
        fill_random(&mut bytes, |remaining| {
            // SAFETY: remaining is writable for exactly the supplied length.
            let n = unsafe { libc::getrandom(remaining.as_mut_ptr().cast(), remaining.len(), 0) };
            if n < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(n as usize)
            }
        })?;
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        Ok(Self(bytes))
    }
}

fn fill_random(
    mut bytes: &mut [u8],
    mut read: impl FnMut(&mut [u8]) -> io::Result<usize>,
) -> io::Result<()> {
    while !bytes.is_empty() {
        match read(bytes) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "getrandom returned no bytes",
                ));
            }
            Ok(n) => bytes = &mut bytes[n..],
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
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
/// Lookups allocate nothing and take O(log n) comparisons. Values remain in the
/// caller's configuration order; only this index is sorted.
pub struct CookieIndex(Vec<(UnquicUuid, usize)>);

impl CookieIndex {
    /// Builds an index from cookies in configuration order.
    pub fn new(cookies: impl IntoIterator<Item = UnquicUuid>) -> Result<Self, DuplicateCookie> {
        let mut entries: Vec<_> = cookies
            .into_iter()
            .enumerate()
            .map(|(i, cookie)| (cookie, i))
            .collect();
        entries.sort_unstable_by_key(|entry| entry.0);
        if entries.windows(2).any(|pair| pair[0].0 == pair[1].0) {
            return Err(DuplicateCookie);
        }
        Ok(Self(entries))
    }

    /// Finds a configuration index without allocating state for unknown cookies.
    pub fn find(&self, cookie: UnquicUuid) -> Option<usize> {
        self.0
            .binary_search_by_key(&cookie, |entry| entry.0)
            .ok()
            .map(|i| self.0[i].1)
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn randomness_handles_short_reads_interrupts_and_failure() {
        let mut bytes = [0; 5];
        let mut calls = 0;
        fill_random(&mut bytes, |out| {
            calls += 1;
            if calls == 1 {
                return Err(io::ErrorKind::Interrupted.into());
            }
            let n = out.len().min(2);
            out[..n].fill(calls);
            Ok(n)
        })
        .unwrap();
        assert_eq!(bytes, [2, 2, 3, 3, 4]);
        assert_eq!(
            fill_random(&mut bytes, |_| Ok(0)).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        assert_eq!(
            fill_random(&mut bytes, |_| Err(io::ErrorKind::PermissionDenied.into()))
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
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
