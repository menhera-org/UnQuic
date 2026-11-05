//! Bounded receive-lane reassembly and permanent duplicate suppression.
//!
//! The caller validates the connection tag and registers the lane's sole owner
//! before admission. Storage is allocated at construction; packet admission,
//! expiry and delivery never allocate, lock or perform I/O. A completed frame
//! stays reserved until a full TAP write succeeds or the caller discards it.

use crate::{
    MAX_FRAME_LEN,
    wire::{Datagram, Record},
};
use std::{collections::VecDeque, fmt, mem::size_of, time::Duration};

/// Number of packet numbers and frame sequences retained in each lane window.
pub const WINDOW_LEN: usize = 65_536;
/// Maximum incomplete plus queued complete frames in this initial lane table.
pub const FRAME_CAPACITY: usize = 64;
/// Lifetime from the first retained piece, unchanged by subsequent pieces.
pub const FRAME_LIFETIME: Duration = Duration::from_millis(100);
const WORDS: usize = WINDOW_LEN / 64;
const INDEX_LEN: usize = FRAME_CAPACITY * 2;
const EMPTY: u8 = u8::MAX;
const SEQUENCE_MASK: u64 = 0x0000_ffff_ffff_ffff;

/// Whether a fully validated datagram was retained by its receive owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admission {
    /// Applied once; DATA storage is retained or its frame is already terminal.
    Fresh,
    /// Packet number already seen or permanently below the acceptance bound.
    Duplicate,
    /// Insufficient storage; no records or packet/window advancement were applied.
    Pressure,
    /// Wrong/retired lane, oversized frame or inconsistent frame length.
    Invalid,
}

/// Invalid local lane identity or negotiated frame ceiling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LaneError;
impl fmt::Display for LaneError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("receive lane requires a nonzero ID and frame ceiling 1514..9022")
    }
}
impl std::error::Error for LaneError {}

/// Borrowed complete frame at the head of the TAP-write queue.
pub struct ReadyFrame<'a> {
    /// Connection-local frame identity, including its lane.
    pub id: u64,
    /// Whole Ethernet frame without FCS or TAP metadata.
    pub bytes: &'a [u8],
}

struct Window {
    lower: u64,
    bits: [u64; WORDS],
}
impl Window {
    const fn new() -> Self {
        Self {
            lower: 1,
            bits: [0; WORDS],
        }
    }

    fn contains(&self, number: u64) -> bool {
        number >= self.lower
            && number - self.lower < WINDOW_LEN as u64
            && self.bits[(number as usize / 64) % WORDS] & (1 << (number % 64)) != 0
    }

    fn old(&self, number: u64) -> bool {
        number < self.lower || self.contains(number)
    }

    fn advance(&mut self, number: u64) {
        let lower = number.saturating_sub(WINDOW_LEN as u64 - 1).max(1);
        if lower <= self.lower {
            return;
        }
        let distance = lower - self.lower;
        if distance >= WINDOW_LEN as u64 {
            self.bits.fill(0);
        } else {
            // Clear only departing positions; retained positions keep their bits.
            let mut start = self.lower;
            while start < lower {
                let offset = (start % 64) as u32;
                let count = (lower - start).min(u64::from(64 - offset)) as u32;
                let mask = (u64::MAX >> (64 - count)) << offset;
                self.bits[(start as usize / 64) % WORDS] &= !mask;
                start += u64::from(count);
            }
        }
        self.lower = lower;
    }

    fn mark(&mut self, number: u64) {
        if number < self.lower {
            return;
        }
        self.advance(number);
        self.bits[(number as usize / 64) % WORDS] |= 1 << (number % 64);
    }

    fn reset(&mut self) {
        self.lower = 1;
        self.bits.fill(0);
    }
}

struct Frame {
    sequence: u64,
    length: u16,
    remaining: u16,
    deadline: Duration,
    covered: [u64; MAX_FRAME_LEN.div_ceil(64)],
    bytes: [u8; MAX_FRAME_LEN],
}
impl Frame {
    const fn empty() -> Self {
        Self {
            sequence: 0,
            length: 0,
            remaining: 0,
            deadline: Duration::ZERO,
            covered: [0; MAX_FRAME_LEN.div_ceil(64)],
            bytes: [0; MAX_FRAME_LEN],
        }
    }

    fn start(&mut self, sequence: u64, length: u16, now: Duration) {
        self.sequence = sequence;
        self.length = length;
        self.remaining = length;
        self.deadline = now.saturating_add(FRAME_LIFETIME);
        self.covered.fill(0);
    }

    fn piece(&mut self, offset: usize, bytes: &[u8]) {
        let end = offset + bytes.len();
        let mut start = offset;
        while start < end {
            let bit = start % 64;
            let count = (end - start).min(64 - bit);
            let mask = (u64::MAX >> (64 - count)) << bit;
            let word = &mut self.covered[start / 64];
            let new = mask & !*word;
            if new == mask {
                self.bytes[start..start + count]
                    .copy_from_slice(&bytes[start - offset..start - offset + count]);
            } else {
                // First-retained bytes win even if a conflicting duplicate arrives.
                let mut pending = new;
                while pending != 0 {
                    let at = start / 64 * 64 + pending.trailing_zeros() as usize;
                    self.bytes[at] = bytes[at - offset];
                    pending &= pending - 1;
                }
            }
            self.remaining -= new.count_ones() as u16;
            *word |= mask;
            start += count;
        }
    }
}

/// One registered remote lane, used by exactly one worker.
///
/// Cookie/tag admission and the session-wide retired-lane bitmap belong to the
/// caller. Preserve this table while its lane remains active, including through
/// idle periods. Recycling it under the same connection/lane reopens old frames.
pub struct Lane {
    id: u16,
    frame_limit: u16,
    retired: bool,
    packets: Window,
    terminal: Window,
    frames: Box<[Frame]>,
    index: [u8; INDEX_LEN],
    free: Vec<u8>,
    ready: VecDeque<u8>,
    deadline: Option<Duration>,
}

impl Lane {
    /// Allocates all frame storage and histories for this lane.
    pub fn new(id: u16, frame_limit: u16) -> Result<Self, LaneError> {
        validate(id, frame_limit)?;
        Ok(Self {
            id,
            frame_limit,
            retired: false,
            packets: Window::new(),
            terminal: Window::new(),
            frames: (0..FRAME_CAPACITY).map(|_| Frame::empty()).collect(),
            index: [EMPTY; INDEX_LEN],
            free: (0..FRAME_CAPACITY as u8).collect(),
            ready: VecDeque::with_capacity(FRAME_CAPACITY),
            deadline: None,
        })
    }

    /// Fixed storage requested from the allocator, including the Lane value.
    ///
    /// The runtime must also budget its owner tables, queues and allocator
    /// overhead; this is not a measurement of process resident memory.
    pub const fn storage_bytes() -> usize {
        size_of::<Self>() + FRAME_CAPACITY * (size_of::<Frame>() + 2 * size_of::<u8>())
    }

    /// Registered remote lane ID.
    pub const fn id(&self) -> u16 {
        self.id
    }

    /// Number of retained incomplete and queued complete frames.
    pub fn retained_frames(&self) -> usize {
        FRAME_CAPACITY - self.free.len()
    }

    /// Permanent lower bounds for packet numbers and frame sequences.
    pub const fn lower_bounds(&self) -> (u64, u64) {
        (self.packets.lower, self.terminal.lower)
    }

    /// Earliest expiry wakeup. Removal can leave a harmless earlier wakeup.
    pub const fn deadline(&self) -> Option<Duration> {
        self.deadline
    }

    /// Recycles storage for a new connection or a never-used remote lane ID.
    ///
    /// Call only after the old owner's commit/retirement barrier and preserve
    /// the old lane's tombstone in the session owner table.
    pub fn reset(&mut self, id: u16, frame_limit: u16) -> Result<(), LaneError> {
        validate(id, frame_limit)?;
        self.id = id;
        self.frame_limit = frame_limit;
        self.retired = false;
        self.packets.reset();
        self.terminal.reset();
        self.clear_frames();
        Ok(())
    }

    /// Drops retained frames and rejects all later packets until reset.
    pub fn retire(&mut self) {
        self.retired = true;
        self.clear_frames();
    }

    fn clear_frames(&mut self) {
        for frame in &mut self.frames {
            frame.sequence = 0;
        }
        self.index.fill(EMPTY);
        self.free.clear();
        self.free.extend(0..FRAME_CAPACITY as u8);
        self.ready.clear();
        self.deadline = None;
    }

    /// Reserves a whole validated datagram before applying any record.
    ///
    /// Due expirations happen independently before reservation. Pressure never
    /// acknowledges the packet or advances either bound. Non-DATA records for
    /// other owners are left to the caller after a Fresh result. Origin-zero
    /// abandonment on this lane is applied here.
    pub fn admit(&mut self, datagram: &Datagram<'_>, now: Duration) -> Admission {
        let header = datagram.header();
        if self.retired || header.lane != self.id || header.receive_tag == 0 {
            return Admission::Invalid;
        }
        self.expire(now);
        if self.packets.old(header.packet_number) {
            return Admission::Duplicate;
        }
        let mut reservations = [(0_u64, 0_u16); 64];
        let mut count = 0;
        let mut highest = 0;
        for record in datagram.records() {
            match record {
                Record::Data {
                    frame_id,
                    total_len,
                    ..
                } => {
                    if total_len > u32::from(self.frame_limit) {
                        return Admission::Invalid;
                    }
                    let sequence = frame_id & SEQUENCE_MASK;
                    highest = highest.max(sequence);
                    if self.terminal.old(sequence) {
                        continue;
                    }
                    if let Some(slot) = self.find(sequence) {
                        if u32::from(self.frames[slot].length) != total_len {
                            return Admission::Invalid;
                        }
                    } else if let Some((_, length)) =
                        reservations[..count].iter().find(|(id, _)| *id == sequence)
                    {
                        if u32::from(*length) != total_len {
                            return Admission::Invalid;
                        }
                    } else {
                        reservations[count] = (sequence, total_len as u16);
                        count += 1;
                    }
                }
                Record::FrameAbandon {
                    origin: 0,
                    frame_id,
                    ..
                } => {
                    highest = highest.max(frame_id & SEQUENCE_MASK);
                }
                _ => {}
            }
        }
        if count > self.free.len() {
            return Admission::Pressure;
        }
        self.advance_frames(highest);
        for record in datagram.records() {
            match record {
                Record::Data {
                    frame_id,
                    total_len,
                    offset,
                    bytes,
                } => {
                    let sequence = frame_id & SEQUENCE_MASK;
                    if self.terminal.old(sequence) {
                        continue;
                    }
                    let slot = match self.find(sequence) {
                        Some(slot) => slot,
                        None => {
                            let slot = usize::from(self.free.pop().expect("preflight reservation"));
                            self.frames[slot].start(sequence, total_len as u16, now);
                            self.insert(slot);
                            let deadline = self.frames[slot].deadline;
                            self.deadline =
                                Some(self.deadline.map_or(deadline, |old| old.min(deadline)));
                            slot
                        }
                    };
                    let frame = &mut self.frames[slot];
                    if frame.remaining != 0 {
                        frame.piece(offset as usize, bytes);
                        if frame.remaining == 0 {
                            self.ready.push_back(slot as u8);
                        }
                    }
                }
                Record::FrameAbandon {
                    origin: 0,
                    frame_id,
                    ..
                } => {
                    self.abandon(frame_id);
                }
                _ => {}
            }
        }
        self.packets.mark(header.packet_number);
        Admission::Fresh
    }

    /// Makes a frame terminal, including an abandonment received before DATA.
    ///
    /// Returns false for another lane or a zero sequence. The containing
    /// datagram must already have passed wire, tag and storage admission.
    pub fn abandon(&mut self, frame_id: u64) -> bool {
        let sequence = frame_id & SEQUENCE_MASK;
        if self.retired || frame_id >> 48 != u64::from(self.id) || sequence == 0 {
            return false;
        }
        self.advance_frames(sequence);
        if let Some(slot) = self.find(sequence) {
            self.release(slot);
        }
        self.terminal.mark(sequence);
        true
    }

    /// Drops overdue frames permanently and returns the number released.
    pub fn expire(&mut self, now: Duration) -> usize {
        if self.deadline.is_none_or(|deadline| deadline > now) {
            return 0;
        }
        self.deadline = None;
        let mut expired = 0;
        for slot in 0..self.frames.len() {
            let frame = &self.frames[slot];
            if frame.sequence == 0 {
                continue;
            }
            if frame.deadline <= now {
                self.terminal.mark(frame.sequence);
                self.release(slot);
                expired += 1;
            } else {
                let deadline = frame.deadline;
                self.deadline = Some(self.deadline.map_or(deadline, |old| old.min(deadline)));
            }
        }
        expired
    }

    /// Borrows the next complete frame after expiring overdue reservations.
    ///
    /// On TAP WouldBlock leave it queued. After a successful full write or any
    /// other error call finish_front exactly once. It is never enqueued twice.
    pub fn front(&mut self, now: Duration) -> Option<ReadyFrame<'_>> {
        self.expire(now);
        let frame = &self.frames[usize::from(*self.ready.front()?)];
        Some(ReadyFrame {
            id: (u64::from(self.id) << 48) | frame.sequence,
            bytes: &frame.bytes[..usize::from(frame.length)],
        })
    }

    /// Marks the head frame terminal and releases its storage.
    ///
    /// Used for either full TAP delivery or permanent write failure.
    pub fn finish_front(&mut self) -> bool {
        let Some(&slot) = self.ready.front() else {
            return false;
        };
        let slot = usize::from(slot);
        self.terminal.mark(self.frames[slot].sequence);
        self.release(slot);
        true
    }

    fn advance_frames(&mut self, highest: u64) {
        let old = self.terminal.lower;
        self.terminal.advance(highest);
        if old != self.terminal.lower {
            for slot in 0..self.frames.len() {
                let sequence = self.frames[slot].sequence;
                if sequence != 0 && sequence < self.terminal.lower {
                    self.release(slot);
                }
            }
        }
    }

    fn find(&self, sequence: u64) -> Option<usize> {
        let mut at = hash(sequence);
        loop {
            let slot = self.index[at];
            if slot == EMPTY {
                return None;
            }
            if self.frames[usize::from(slot)].sequence == sequence {
                return Some(usize::from(slot));
            }
            at = (at + 1) % INDEX_LEN;
        }
    }

    fn insert(&mut self, slot: usize) {
        let mut at = hash(self.frames[slot].sequence);
        while self.index[at] != EMPTY {
            at = (at + 1) % INDEX_LEN;
        }
        self.index[at] = slot as u8;
    }

    fn release(&mut self, slot: usize) {
        let mut hole = hash(self.frames[slot].sequence);
        while self.index[hole] != slot as u8 {
            hole = (hole + 1) % INDEX_LEN;
        }
        self.index[hole] = EMPTY;
        let mut at = (hole + 1) % INDEX_LEN;
        while self.index[at] != EMPTY {
            let home = hash(self.frames[usize::from(self.index[at])].sequence);
            if (hole.wrapping_sub(home) % INDEX_LEN) < (at.wrapping_sub(home) % INDEX_LEN) {
                self.index[hole] = self.index[at];
                self.index[at] = EMPTY;
                hole = at;
            }
            at = (at + 1) % INDEX_LEN;
        }
        if self
            .ready
            .front()
            .is_some_and(|&queued| usize::from(queued) == slot)
        {
            self.ready.pop_front();
        } else {
            self.ready.retain(|&queued| usize::from(queued) != slot);
        }
        self.frames[slot].sequence = 0;
        self.free.push(slot as u8);
    }
}

const fn hash(sequence: u64) -> usize {
    // Multiplication folds high sequence bits into the small, half-full table.
    (sequence.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> (64 - 7)) as usize
}

fn validate(id: u16, frame_limit: u16) -> Result<(), LaneError> {
    if id == 0 || !(1514..=MAX_FRAME_LEN as u16).contains(&frame_limit) {
        Err(LaneError)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_clears_departing_bits_without_reopening_old_numbers() {
        let mut window = Window::new();
        for n in [1, 63, 64, 65, 1000, 65535, 65536] {
            window.mark(n);
        }
        assert!(!window.old(2));
        window.mark(65538);
        assert_eq!(window.lower, 3);
        for n in [1, 2, 63, 64, 65, 1000, 65535, 65536, 65538] {
            assert!(window.old(n));
        }
        assert!(!window.old(65537));
        window.mark(65536 + 64);
        assert_eq!(window.lower, 65);
        assert!(window.contains(65));
        assert!(!window.contains(65536 + 63));
        window.mark(u64::MAX);
        assert_eq!(window.lower, u64::MAX - 65535);
        assert!(window.old(1));
        assert!(window.contains(u64::MAX));
        assert!(!window.contains(u64::MAX - 1));
        window.mark(u64::MAX - 1);
        assert!(window.contains(u64::MAX - 1));
    }

    #[test]
    fn window_matches_a_set_across_wraps_gaps_and_reordering() {
        let mut window = Window::new();
        let mut seen = std::collections::BTreeSet::new();
        let mut highest = 1_u64;
        let mut random = 0x1234_5678_9abc_def0_u64;
        for step in 0..10_000 {
            random ^= random << 13;
            random ^= random >> 7;
            random ^= random << 17;
            let number = if step % 13 == 0 {
                highest.saturating_sub(random % 100_000).max(1)
            } else {
                highest + random % 512
            };
            highest = highest.max(number);
            let lower = highest.saturating_sub(65535).max(1);
            let was_old = number < window.lower || seen.contains(&number);
            assert_eq!(window.old(number), was_old);
            window.mark(number);
            seen.insert(number);
            seen = seen.split_off(&lower);
            assert_eq!(window.lower, lower);
            for candidate in [
                number,
                lower,
                lower - 1,
                highest,
                highest.saturating_sub(random % 65536),
                highest + 1,
            ] {
                assert_eq!(
                    window.old(candidate),
                    candidate < lower || seen.contains(&candidate)
                );
            }
        }
    }

    #[test]
    fn index_deletion_preserves_colliding_and_wrapping_probe_chains() {
        let mut lane = Lane::new(1, 9022).unwrap();
        for home in [0, 120, 127] {
            let colliding: Vec<_> = (1..100_000)
                .filter(|&seq| hash(seq) == home)
                .take(64)
                .collect();
            assert_eq!(colliding.len(), 64);
            lane.free.clear();
            for (slot, &seq) in colliding.iter().enumerate() {
                lane.frames[slot].sequence = seq;
                lane.insert(slot);
            }
            for step in 0..64 {
                let slot = (step * 17) % 64;
                lane.release(slot);
                for (i, &seq) in colliding.iter().enumerate() {
                    assert_eq!(
                        lane.find(seq),
                        if lane.frames[i].sequence == 0 {
                            None
                        } else {
                            Some(i)
                        }
                    );
                }
            }
            // This test inserts slots directly, so restore the free-list invariant.
            lane.reset(1, 9022).unwrap();
        }
    }
}
