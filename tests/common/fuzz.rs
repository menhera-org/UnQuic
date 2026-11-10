//! Deterministic xorshift inputs for panic-freedom tests: random bytes and
//! mutations of valid fixtures.

/// Inputs fed to each untrusted-input entry point.
pub const INPUTS: usize = 200_000;

pub struct Rng(u64);

impl Rng {
    pub const fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    pub fn bits(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// A value below `n`, which must be nonzero.
    pub fn below(&mut self, n: usize) -> usize {
        (self.bits() % n as u64) as usize
    }

    pub fn fill(&mut self, bytes: &mut [u8]) {
        for chunk in bytes.chunks_mut(8) {
            let word = self.bits().to_ne_bytes();
            chunk.copy_from_slice(&word[..chunk.len()]);
        }
    }

    /// Replaces `out` with random bytes of length 0..=10000, or a seed after
    /// zero to three mutations.
    pub fn input(&mut self, seeds: &[Vec<u8>], out: &mut Vec<u8>) {
        out.clear();
        if seeds.is_empty() || self.below(4) == 0 {
            // Mostly short, so the per-input cost stays low in debug builds.
            let max = [64, 2048, 10_000][self.below(3)];
            out.resize(self.below(max + 1), 0);
            self.fill(out);
            return;
        }
        out.extend_from_slice(&seeds[self.below(seeds.len())]);
        for _ in 0..self.below(4) {
            self.mutate(out);
        }
    }

    /// Truncates, flips, overwrites, inserts, removes or appends bytes, or
    /// stores a boundary value in a 16-bit field of either byte order.
    pub fn mutate(&mut self, bytes: &mut Vec<u8>) {
        let len = bytes.len();
        let at = self.below(len + 1);
        match self.below(7) {
            0 => bytes.truncate(at),
            1 if at < len => bytes[at] ^= 1 << self.below(8),
            2 if at < len => bytes[at] = self.bits() as u8,
            3 => {
                let value = [0, 1, 0xff, 0x7fff, 0xffff, len, len.wrapping_sub(at)]
                    .get(self.below(8))
                    .map_or(self.bits() as u16, |&value| value as u16);
                let field = if self.below(2) == 0 {
                    value.to_be_bytes()
                } else {
                    value.to_ne_bytes()
                };
                for (slot, byte) in bytes.iter_mut().skip(at).zip(field) {
                    *slot = byte;
                }
            }
            4 => {
                let mut extra = vec![0; 1 + self.below(64)];
                self.fill(&mut extra);
                bytes.splice(at..at, extra);
            }
            5 => {
                bytes.drain(at..len.min(at + 1 + self.below(64)));
            }
            _ => {
                bytes.resize(len + self.below(2048), 0);
                if self.below(2) == 0 {
                    self.fill(&mut bytes[len..]);
                }
            }
        }
    }
}

/// Rewrites untagged IPv4/IPv6 and UDP length fields to match a mutated frame.
pub fn fix_lengths(bytes: &mut [u8]) {
    let len = bytes.len();
    let (at, protocol) = match bytes.get(12..14) {
        Some([0x08, 0x00]) if len >= 34 => {
            bytes[16..18].copy_from_slice(&((len - 14) as u16).to_be_bytes());
            (14 + usize::from(bytes[14] & 15) * 4, bytes[23])
        }
        Some([0x86, 0xdd]) if len >= 54 => {
            bytes[18..20].copy_from_slice(&((len - 54) as u16).to_be_bytes());
            (54, bytes[20])
        }
        _ => return,
    };
    if protocol == 17
        && let Some(field) = bytes.get_mut(at + 4..at + 6)
    {
        field.copy_from_slice(&((len - at) as u16).to_be_bytes());
    }
}
