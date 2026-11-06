//! Per-frame byte coverage; packet accounting and payload lifetime remain separate.

use crate::MAX_FRAME_LEN;
use std::ops::Range;

const WORDS: usize = MAX_FRAME_LEN.div_ceil(64);

#[derive(Clone, Copy, Default)]
struct Word {
    low: u64,
    high: u64,
    pending: u64,
}

impl Word {
    fn generation(&self, attempt: u8) -> u64 {
        match attempt {
            0 => !self.low & !self.high,
            1 => self.low & !self.high,
            2 => !self.low & self.high,
            _ => self.low & self.high,
        }
    }

    fn at_least(&self, attempt: u8) -> u64 {
        match attempt {
            0 => u64::MAX,
            1 => self.low | self.high,
            _ => self.high,
        }
    }
}

/// Allocation-free selective-repair state for one retained Ethernet frame.
///
/// Three bits per byte track the original, first repair, second repair or ACK,
/// and whether that generation is outstanding. Never-submitted bytes are excluded
/// by the contiguous submission cursor. Reserve this storage against actual frame
/// quotas, alongside payload and packet-history storage.
///
/// Feed only piece mappings from validated successful transmissions. ACKs settle
/// frame coverage; they do not settle other copies' packet congestion charges.
/// Selection does not reserve bytes: serialize plans and submission results per
/// frame, and never submit overlapping plans for the same repair generation.
/// A frame's original deadline still applies, even when no repair is eligible.
pub struct FrameRecovery {
    words: [Word; WORDS],
    length: u16,
    sent: u16,
    unacked: u16,
}

impl FrameRecovery {
    /// Starts an unsent frame; rejects lengths outside the Ethernet frame limits.
    pub fn new(length: usize) -> Option<Self> {
        (14..=MAX_FRAME_LEN).contains(&length).then(|| Self {
            words: [Word::default(); WORDS],
            length: length as u16,
            sent: 0,
            unacked: length as u16,
        })
    }

    /// Reuses a retired frame's storage, clearing only words needed by the new length.
    ///
    /// The caller must invalidate the old frame identity in packet mappings first.
    /// Packet congestion charges remain independent of this payload-slot reuse.
    pub fn reset(&mut self, length: usize) -> bool {
        if !(14..=MAX_FRAME_LEN).contains(&length) {
            return false;
        }
        for word in self.words.iter_mut().take(length.div_ceil(64)) {
            *word = Word::default();
        }
        self.length = length as u16;
        self.sent = 0;
        self.unacked = length as u16;
        true
    }

    /// Bytes successfully submitted as fresh data, in contiguous prefix order.
    pub const fn sent(&self) -> usize {
        self.sent as usize
    }

    /// Bytes not yet acknowledged, including unsent and repair-exhausted bytes.
    pub const fn remaining(&self) -> usize {
        self.unacked as usize
    }

    /// Checks a proposed submission without mutation: zero is fresh, one/two repair.
    ///
    /// Fresh pieces extend the submitted prefix. Repairs require the previous
    /// generation to be lost and unacknowledged across the entire range.
    /// Revalidate deferred plans before I/O if feedback could have intervened.
    pub fn can_submit(&self, range: Range<usize>, attempt: u8) -> bool {
        if range.start >= range.end || range.end > usize::from(self.length) || attempt > 2 {
            return false;
        }
        if attempt == 0 {
            return range.start == self.sent();
        }
        range.end <= self.sent()
            && self
                .selected(&range)
                .all(|(word, mask)| word.generation(attempt - 1) & !word.pending & mask == mask)
    }

    /// Records only a successfully submitted piece; rejection changes nothing.
    ///
    /// Planning and failed socket submissions consume no repair allowance.
    pub fn submitted(&mut self, range: Range<usize>, attempt: u8) -> bool {
        if !self.can_submit(range.clone(), attempt) {
            return false;
        }
        for (word, mask) in self.selected_mut(&range) {
            if attempt == 1 {
                word.low |= mask;
            } else if attempt == 2 {
                word.low &= !mask;
                word.high |= mask;
            }
            word.pending |= mask;
        }
        if attempt == 0 {
            self.sent = range.end as u16;
        }
        true
    }

    /// Marks a submitted generation lost without reopening newer or ACKed bytes.
    ///
    /// Old/duplicate losses are harmless. Packet history releases its own charge.
    pub fn lost(&mut self, range: Range<usize>, attempt: u8) -> bool {
        if !self.feedback_valid(&range, attempt) {
            return false;
        }
        for (word, mask) in self.selected_mut(&range) {
            word.pending &= !(word.generation(attempt) & mask);
        }
        true
    }

    /// Returns newly acknowledged bytes, or None for an invalid piece mapping.
    ///
    /// A late original ACK also settles bytes covered by a newer repair.
    pub fn acknowledge(&mut self, range: Range<usize>, attempt: u8) -> Option<usize> {
        if !self.feedback_valid(&range, attempt) {
            return None;
        }
        let new: u16 = self
            .selected(&range)
            .map(|(word, mask)| (mask & !(word.low & word.high)).count_ones() as u16)
            .sum();
        let remaining = self.unacked.checked_sub(new)?;
        for (word, mask) in self.selected_mut(&range) {
            word.low |= mask;
            word.high |= mask;
            word.pending &= !mask;
        }
        self.unacked = remaining;
        Some(usize::from(new))
    }

    /// First eligible contiguous range and its next attempt, bounded by max_bytes.
    ///
    /// Returns None for zero budget, outstanding-only data or exhausted repairs.
    /// None does not imply completion; use remaining() and the original deadline.
    pub fn next_repair(&self, max_bytes: usize) -> Option<(Range<usize>, u8)> {
        if max_bytes == 0 {
            return None;
        }
        let mut start = 0;
        let mut end = 0;
        let mut attempt = 0;
        for (index, word) in self.words.iter().enumerate().take(self.sent().div_ceil(64)) {
            let eligible = !word.high & !word.pending & mask(&(0..self.sent()), index);
            if attempt == 0 {
                if eligible == 0 {
                    continue;
                }
                let bit = eligible.trailing_zeros();
                start = index * 64 + bit as usize;
                end = start;
                attempt = 1 + ((word.low >> bit) & 1) as u8;
            }
            let limit = start.saturating_add(max_bytes).min(self.sent());
            let available = (index + 1) * 64;
            let target = available.min(limit);
            let bits = eligible & word.generation(attempt - 1);
            let count = ((bits >> (end % 64)).trailing_ones() as usize).min(target - end);
            end += count;
            if end < target || end == limit {
                break;
            }
        }
        (attempt != 0).then_some((start..end, attempt))
    }

    fn feedback_valid(&self, range: &Range<usize>, attempt: u8) -> bool {
        range.start < range.end
            && range.end <= self.sent()
            && attempt <= 2
            && self
                .selected(range)
                .all(|(word, mask)| word.at_least(attempt) & mask == mask)
    }

    fn selected(&self, range: &Range<usize>) -> impl Iterator<Item = (&Word, u64)> {
        self.words
            .iter()
            .enumerate()
            .take(range.end.div_ceil(64))
            .skip(range.start / 64)
            .map(|(index, word)| (word, mask(range, index)))
    }

    fn selected_mut(&mut self, range: &Range<usize>) -> impl Iterator<Item = (&mut Word, u64)> {
        self.words
            .iter_mut()
            .enumerate()
            .take(range.end.div_ceil(64))
            .skip(range.start / 64)
            .map(|(index, word)| (word, mask(range, index)))
    }
}

fn mask(range: &Range<usize>, index: usize) -> u64 {
    let first = if index == range.start / 64 {
        u64::MAX << (range.start % 64)
    } else {
        u64::MAX
    };
    let last = range.end.saturating_sub(1);
    let last = if index == last / 64 {
        u64::MAX >> (63 - last % 64)
    } else {
        u64::MAX
    };
    first & last
}
