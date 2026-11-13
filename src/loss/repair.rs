//! Per-frame byte coverage; packet accounting and payload lifetime remain separate.

use crate::MAX_FRAME_LEN;
use std::ops::Range;

/// Largest frame WidePlanes cover, and so the largest offload frame a sender
/// builds: two reassembly slots, so a receiving lane holds 32 of them while
/// repairs are pending instead of 8 maximum-size ones.
pub const WIDE_FRAME_LEN: usize = 2 * MAX_FRAME_LEN;

const WORDS: usize = MAX_FRAME_LEN.div_ceil(64);
const WIDE_WORDS: usize = WIDE_FRAME_LEN.div_ceil(64);

/// Coverage planes for one offload frame, lent to a FrameRecovery while it
/// holds one. Kept zeroed while unattached.
pub struct WidePlanes([[u64; WIDE_WORDS]; 3]);

impl WidePlanes {
    /// Zeroed planes; allocate these only at startup.
    pub fn new() -> Box<Self> {
        Box::new(Self([[0; WIDE_WORDS]; 3]))
    }
}

/// Bytes of generation `attempt`: original, first repair, second repair or ACK.
const fn generation(low: u64, high: u64, attempt: u8) -> u64 {
    match attempt {
        0 => !low & !high,
        1 => low & !high,
        2 => !low & high,
        _ => low & high,
    }
}

/// Bytes whose generation is at least `attempt`.
const fn at_least(low: u64, high: u64, attempt: u8) -> u64 {
    match attempt {
        0 => u64::MAX,
        1 => low | high,
        _ => high,
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
///
/// The bit planes are stored apart so that a frame without losses touches only
/// one. Until its first loss a frame is clean: every sent byte is either an
/// outstanding original or acknowledged, so only `high` is kept, as the
/// acknowledged bytes. The first loss rebuilds the other planes from it.
///
/// The planes cover MAX_FRAME_LEN bytes, or WIDE_FRAME_LEN while WidePlanes
/// are attached.
pub struct FrameRecovery {
    // Low, high and pending planes.
    planes: [[u64; WORDS]; 3],
    wide: Option<Box<WidePlanes>>,
    length: u16,
    sent: u16,
    unacked: u16,
    // Words that may be nonzero in any plane; reset clears only these.
    dirty: u16,
    clean: bool,
}

/// The active low, high and pending planes; fields are passed separately so
/// callers keep the rest of the frame state.
fn planes<'a>(inline: &'a [[u64; WORDS]; 3], wide: &'a Option<Box<WidePlanes>>) -> [&'a [u64]; 3] {
    match wide {
        Some(wide) => {
            let [low, high, pending] = &wide.0;
            [low, high, pending]
        }
        None => {
            let [low, high, pending] = inline;
            [low, high, pending]
        }
    }
}

fn planes_mut<'a>(
    inline: &'a mut [[u64; WORDS]; 3],
    wide: &'a mut Option<Box<WidePlanes>>,
) -> [&'a mut [u64]; 3] {
    match wide {
        Some(wide) => {
            let [low, high, pending] = &mut wide.0;
            [low, high, pending]
        }
        None => {
            let [low, high, pending] = inline;
            [low, high, pending]
        }
    }
}

impl FrameRecovery {
    /// Starts an unsent frame; rejects lengths outside the Ethernet frame limits.
    pub fn new(length: usize) -> Option<Self> {
        (14..=MAX_FRAME_LEN).contains(&length).then_some(Self {
            planes: [[0; WORDS]; 3],
            wide: None,
            length: length as u16,
            sent: 0,
            unacked: length as u16,
            dirty: 0,
            clean: true,
        })
    }

    /// Reuses a retired frame's storage, clearing only words it wrote.
    ///
    /// The caller must invalidate the old frame identity in packet mappings first.
    /// Packet congestion charges remain independent of this payload-slot reuse.
    pub fn reset(&mut self, length: usize) -> bool {
        let limit = if self.wide.is_some() {
            WIDE_FRAME_LEN
        } else {
            MAX_FRAME_LEN
        };
        if !(14..=limit).contains(&length) {
            return false;
        }
        self.clear();
        self.length = length as u16;
        self.sent = 0;
        self.unacked = length as u16;
        true
    }

    /// Lends wide planes for an offload frame; call reset afterwards. Returns
    /// the planes if some are already attached.
    pub fn attach(&mut self, wide: Box<WidePlanes>) -> Result<(), Box<WidePlanes>> {
        if self.wide.is_some() {
            return Err(wide);
        }
        self.clear();
        self.wide = Some(wide);
        Ok(())
    }

    /// Takes back attached wide planes, zeroed, and returns to the inline
    /// planes; the frame must be reset before reuse.
    pub fn detach(&mut self) -> Option<Box<WidePlanes>> {
        self.wide.as_ref()?;
        self.clear();
        self.length = 14;
        self.sent = 0;
        self.unacked = 14;
        self.wide.take()
    }

    fn clear(&mut self) {
        let dirty = usize::from(self.dirty);
        let [low, high, pending] = planes_mut(&mut self.planes, &mut self.wide);
        high[..dirty].fill(0);
        if !self.clean {
            low[..dirty].fill(0);
            pending[..dirty].fill(0);
        }
        self.dirty = 0;
        self.clean = true;
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
        let [low, high, pending] = planes(&self.planes, &self.wide);
        // Nothing is repairable before the first loss.
        !self.clean
            && range.end <= self.sent()
            && words(&range).all(|(i, mask)| {
                generation(low[i], high[i], attempt - 1) & !pending[i] & mask == mask
            })
    }

    /// Records only a successfully submitted piece; rejection changes nothing.
    ///
    /// Planning and failed socket submissions consume no repair allowance.
    pub fn submitted(&mut self, range: Range<usize>, attempt: u8) -> bool {
        if !self.can_submit(range.clone(), attempt) {
            return false;
        }
        if !self.clean {
            let [low, high, pending] = planes_mut(&mut self.planes, &mut self.wide);
            for (i, mask) in words(&range) {
                if attempt == 1 {
                    low[i] |= mask;
                } else if attempt == 2 {
                    low[i] &= !mask;
                    high[i] |= mask;
                }
                pending[i] |= mask;
            }
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
        self.materialize();
        let [low, high, pending] = planes_mut(&mut self.planes, &mut self.wide);
        for (i, mask) in words(&range) {
            pending[i] &= !(generation(low[i], high[i], attempt) & mask);
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
        let clean = self.clean;
        let [low, high, pending] = planes_mut(&mut self.planes, &mut self.wide);
        let acknowledged = |low: u64, high: u64| if clean { high } else { low & high };
        let new: u16 = words(&range)
            .map(|(i, mask)| (mask & !acknowledged(low[i], high[i])).count_ones() as u16)
            .sum();
        let remaining = self.unacked.checked_sub(new)?;
        if clean {
            for (i, mask) in words(&range) {
                high[i] |= mask;
            }
            self.dirty = self.dirty.max(range.end.div_ceil(64) as u16);
        } else {
            for (i, mask) in words(&range) {
                low[i] |= mask;
                high[i] |= mask;
                pending[i] &= !mask;
            }
        }
        self.unacked = remaining;
        Some(usize::from(new))
    }

    /// First eligible contiguous range and its next attempt, bounded by max_bytes.
    ///
    /// Returns None for zero budget, outstanding-only data or exhausted repairs.
    /// None does not imply completion; use remaining() and the original deadline.
    pub fn next_repair(&self, max_bytes: usize) -> Option<(Range<usize>, u8)> {
        // A clean frame has only outstanding originals and acknowledged bytes.
        if max_bytes == 0 || self.clean {
            return None;
        }
        let [lows, highs, pending] = planes(&self.planes, &self.wide);
        let mut start = 0;
        let mut end = 0;
        let mut attempt = 0;
        for index in 0..self.sent().div_ceil(64) {
            let (low, high) = (lows[index], highs[index]);
            let eligible = !high & !pending[index] & mask(&(0..self.sent()), index);
            if attempt == 0 {
                if eligible == 0 {
                    continue;
                }
                let bit = eligible.trailing_zeros();
                start = index * 64 + bit as usize;
                end = start;
                attempt = 1 + ((low >> bit) & 1) as u8;
            }
            let limit = start.saturating_add(max_bytes).min(self.sent());
            let available = (index + 1) * 64;
            let target = available.min(limit);
            let bits = eligible & generation(low, high, attempt - 1);
            let count = ((bits >> (end % 64)).trailing_ones() as usize).min(target - end);
            end += count;
            if end < target || end == limit {
                break;
            }
        }
        (attempt != 0).then_some((start..end, attempt))
    }

    /// Leaves the clean state: acknowledged bytes take generation 3 and every
    /// other sent byte becomes an outstanding original.
    fn materialize(&mut self) {
        if !self.clean {
            return;
        }
        let sent = 0..self.sent();
        if !sent.is_empty() {
            let [low, high, pending] = planes_mut(&mut self.planes, &mut self.wide);
            for (i, mask) in words(&sent) {
                low[i] = high[i];
                pending[i] = mask & !high[i];
            }
        }
        self.dirty = self.length.div_ceil(64);
        self.clean = false;
    }

    fn feedback_valid(&self, range: &Range<usize>, attempt: u8) -> bool {
        let [low, high, _] = planes(&self.planes, &self.wide);
        range.start < range.end
            && range.end <= self.sent()
            && attempt <= 2
            && words(range).all(|(i, mask)| {
                // While clean, acknowledged bytes are the only ones past generation 0.
                let reached = if self.clean {
                    if attempt == 0 { u64::MAX } else { high[i] }
                } else {
                    at_least(low[i], high[i], attempt)
                };
                reached & mask == mask
            })
    }
}

/// Word indexes a nonempty range covers, each with its bit mask.
fn words(range: &Range<usize>) -> impl Iterator<Item = (usize, u64)> + use<> {
    let range = range.clone();
    (range.start / 64..range.end.div_ceil(64)).map(move |index| (index, mask(&range, index)))
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
