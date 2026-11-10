//! Bounded receive-lane reassembly and permanent duplicate suppression.
//!
//! The caller validates the connection tag and registers the lane's sole owner
//! before admission. Storage is allocated at construction; packet admission,
//! expiry and delivery never allocate, lock or perform I/O. A completed frame
//! stays reserved until a full TAP write succeeds or the caller discards it.

use crate::{
    MAX_FRAME_LEN,
    ack::Schedule,
    wire::{AckRange, AckRanges, Datagram, MAX_ACK_RANGES, Record},
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
    /// Insufficient storage; existing DATA may progress, but no packet ACK,
    /// window advancement, new reservation or control record is applied.
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

    fn ranges(&self, highest: u64, output: &mut [AckRange; MAX_ACK_RANGES]) -> usize {
        let mut cursor = highest;
        let mut count = 0;
        while count < output.len() {
            let Some(end) = self.last_bit(cursor, true) else {
                break;
            };
            let start = self.last_bit(end, false).map_or(self.lower, |gap| gap + 1);
            output[count] = AckRange { start, end };
            count += 1;
            if start == self.lower {
                break;
            }
            cursor = start - 1;
        }
        count
    }

    fn last_bit(&self, mut number: u64, set: bool) -> Option<u64> {
        while number >= self.lower {
            let base = number & !63;
            let mut bits = self.bits[(number as usize / 64) % WORDS];
            if !set {
                bits = !bits;
            }
            bits &= u64::MAX >> (63 - number % 64);
            if base <= self.lower {
                bits &= u64::MAX << (self.lower % 64);
            }
            if bits != 0 {
                return Some(base + 63 - u64::from(bits.leading_zeros()));
            }
            if base <= self.lower {
                break;
            }
            number = base - 1;
        }
        None
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
    prefix_len: u16,
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
            prefix_len: 0,
            deadline: Duration::ZERO,
            covered: [0; MAX_FRAME_LEN.div_ceil(64)],
            bytes: [0; MAX_FRAME_LEN],
        }
    }

    fn start(&mut self, sequence: u64, length: u16, now: Duration) {
        self.sequence = sequence;
        self.length = length;
        self.remaining = length;
        self.prefix_len = 0;
        self.deadline = now.saturating_add(FRAME_LIFETIME);
        let words = usize::from(length).div_ceil(64).min(self.covered.len());
        self.covered[..words].fill(0);
    }

    fn piece(&mut self, offset: usize, bytes: &[u8]) {
        let end = offset + bytes.len();
        // Admission checks the range; this keeps every index below in bounds.
        if end > usize::from(self.length).min(MAX_FRAME_LEN) {
            return;
        }
        // Bulk-copy an append only while no out-of-order bytes exist beyond the prefix.
        if !bytes.is_empty()
            && offset == usize::from(self.prefix_len)
            && self.remaining == self.length - self.prefix_len
        {
            self.bytes[offset..end].copy_from_slice(bytes);
            self.covered[offset / 64..end.div_ceil(64)].fill(u64::MAX);
            self.covered[(end - 1) / 64] &= u64::MAX >> (63 - (end - 1) % 64);
            self.remaining -= bytes.len() as u16;
            self.prefix_len = end as u16;
            return;
        }
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
    ack: Schedule,
    terminal: Window,
    frames: Box<[Frame]>,
    index: [u8; INDEX_LEN],
    free: Vec<u8>,
    ready: VecDeque<u8>,
    deadline: Option<Duration>,
    retained_bytes: usize,
    expired_frames: u64,
    expired_incomplete_frames: u64,
    duplicate_pieces: u64,
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
            ack: Schedule::new(),
            terminal: Window::new(),
            frames: (0..FRAME_CAPACITY).map(|_| Frame::empty()).collect(),
            index: [EMPTY; INDEX_LEN],
            free: (0..FRAME_CAPACITY as u8).collect(),
            ready: VecDeque::with_capacity(FRAME_CAPACITY),
            deadline: None,
            retained_bytes: 0,
            expired_frames: 0,
            expired_incomplete_frames: 0,
            duplicate_pieces: 0,
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

    pub(crate) fn ready_count(&self) -> usize {
        self.ready.len()
    }

    /// Sum of complete frame lengths reserved by retained slots.
    pub const fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }

    /// Frames released at their deadline since the last reset.
    pub const fn expired_frames(&self) -> u64 {
        self.expired_frames
    }

    /// Incomplete frames released at their deadline; a subset of expired_frames.
    pub const fn expired_incomplete_frames(&self) -> u64 {
        self.expired_incomplete_frames
    }

    /// Entire DATA pieces already terminal or fully covered since the last reset.
    pub const fn duplicate_pieces(&self) -> u64 {
        self.duplicate_pieces
    }

    /// Permanent lower bounds for packet numbers and frame sequences.
    pub const fn lower_bounds(&self) -> (u64, u64) {
        (self.packets.lower, self.terminal.lower)
    }

    /// Earliest expiry wakeup. Removal can leave a harmless earlier wakeup.
    pub const fn deadline(&self) -> Option<Duration> {
        self.deadline
    }

    /// Earliest pending ACK construction time, independent of frame expiry.
    pub const fn ack_deadline(&self) -> Option<Duration> {
        self.ack.deadline
    }

    /// Constructs due feedback using at most the newest 32 receive-window ranges.
    ///
    /// Reserve feedback-queue space before calling: this consumes the pending
    /// schedule. The caller supplies a fresh local transmit packet number and
    /// applies the path group's control-traffic limit. No allocation or I/O occurs.
    pub fn take_ack<'a>(
        &mut self,
        now: Duration,
        output: &'a mut [AckRange; MAX_ACK_RANGES],
    ) -> Option<Record<'a>> {
        let delay_us = self.ack.take(now)?;
        let count = self.packets.ranges(self.ack.highest, output);
        Some(Record::Ack {
            lane: self.id,
            delay_us,
            ranges: AckRanges::new(output.get(..count)?).ok()?,
        })
    }

    /// Records successful ACK submission after queueing, preserving duplicate spacing.
    pub fn ack_transmitted(&mut self, sent_at: Duration) {
        self.ack.transmitted(sent_at);
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
        self.expired_frames = 0;
        self.expired_incomplete_frames = 0;
        self.duplicate_pieces = 0;
        self.packets.reset();
        self.ack = Schedule::new();
        self.terminal.reset();
        self.clear_frames();
        Ok(())
    }

    /// Drops retained frames and rejects all later packets until reset.
    pub fn retire(&mut self) {
        self.retired = true;
        self.ack = Schedule::new();
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
        self.retained_bytes = 0;
    }

    /// Validates all records before retaining DATA and making a packet ACK-eligible.
    ///
    /// On pressure, only pieces for existing frames are retained. The caller can
    /// deliver completed frames and retry the same datagram. Until all its DATA
    /// fits, neither packet history, window bounds nor control records advance.
    /// Due expirations remain independent. Other owners apply controls only on Fresh.
    pub fn admit(&mut self, datagram: &Datagram<'_>, now: Duration) -> Admission {
        let header = datagram.header();
        if self.retired || header.lane != self.id || header.receive_tag == 0 {
            return Admission::Invalid;
        }
        self.expire(now);
        if self.packets.old(header.packet_number) {
            if datagram.records().any(Record::ack_eliciting) {
                self.ack.duplicate(now);
            }
            return Admission::Duplicate;
        }
        let mut reservations = [(0_u64, 0_u16); 64];
        let mut count = 0;
        let mut highest = 0;
        let mut eliciting = false;
        for record in datagram.records() {
            eliciting |= record.ack_eliciting();
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
                    } else if let Some((_, length)) = reservations
                        .iter()
                        .take(count)
                        .find(|(id, _)| *id == sequence)
                    {
                        if u32::from(*length) != total_len {
                            return Admission::Invalid;
                        }
                    } else {
                        // A datagram holds at most 64 records; any excess still counts as pressure.
                        if let Some(reservation) = reservations.get_mut(count) {
                            *reservation = (sequence, total_len as u16);
                        }
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
        let pressure = count > self.free.len();
        if !pressure {
            self.advance_frames(highest);
        }
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
                        self.duplicate_pieces = self.duplicate_pieces.wrapping_add(1);
                        continue;
                    }
                    let slot = match self.find(sequence) {
                        Some(slot) => slot,
                        None if pressure => continue,
                        None => {
                            // The preflight reserved a slot; drop the piece as under pressure if not.
                            let Some(slot) = self.free.pop() else {
                                continue;
                            };
                            let slot = usize::from(slot);
                            self.frames[slot].start(sequence, total_len as u16, now);
                            self.retained_bytes += total_len as usize;
                            self.insert(slot);
                            let deadline = self.frames[slot].deadline;
                            self.deadline =
                                Some(self.deadline.map_or(deadline, |old| old.min(deadline)));
                            slot
                        }
                    };
                    let frame = &mut self.frames[slot];
                    let remaining = frame.remaining;
                    if remaining != 0 {
                        frame.piece(offset as usize, bytes);
                        if frame.remaining == 0 {
                            self.ready.push_back(slot as u8);
                        }
                    }
                    if remaining == frame.remaining {
                        self.duplicate_pieces = self.duplicate_pieces.wrapping_add(1);
                    }
                }
                Record::FrameAbandon {
                    origin: 0,
                    frame_id,
                    ..
                } if !pressure => {
                    self.abandon(frame_id);
                }
                _ => {}
            }
        }
        if pressure {
            return Admission::Pressure;
        }
        self.packets.mark(header.packet_number);
        self.ack.accepted(header.packet_number, eliciting, now);
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
                self.expired_incomplete_frames = self
                    .expired_incomplete_frames
                    .wrapping_add(u64::from(frame.remaining != 0));
                self.terminal.mark(frame.sequence);
                self.release(slot);
                expired += 1;
            } else {
                let deadline = frame.deadline;
                self.deadline = Some(self.deadline.map_or(deadline, |old| old.min(deadline)));
            }
        }
        self.expired_frames = self.expired_frames.wrapping_add(expired as u64);
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
            bytes: frame.bytes.get(..usize::from(frame.length))?,
        })
    }

    pub(crate) fn ready_frames(&mut self, now: Duration) -> impl Iterator<Item = &[u8]> {
        self.expire(now);
        self.ready.iter().map(|&slot| {
            let frame = &self.frames[usize::from(slot)];
            frame
                .bytes
                .get(..usize::from(frame.length))
                .unwrap_or_default()
        })
    }

    pub(crate) fn finish_ready(&mut self, mask: u64) {
        if mask == 1 {
            self.finish_front();
            return;
        }
        let mut slots = [0u8; FRAME_CAPACITY];
        let mut count = 0;
        let mut index = 0;
        self.ready.retain(|&slot| {
            let remove = mask & (1 << index) != 0;
            index += 1;
            if remove {
                slots[count] = slot;
                count += 1;
            }
            !remove
        });
        for &slot in &slots[..count] {
            let slot = usize::from(slot);
            self.terminal.mark(self.frames[slot].sequence);
            self.release_storage(slot);
        }
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
        if self
            .ready
            .front()
            .is_some_and(|&queued| usize::from(queued) == slot)
        {
            self.ready.pop_front();
        } else {
            self.ready.retain(|&queued| usize::from(queued) != slot);
        }
        self.release_storage(slot);
    }

    fn release_storage(&mut self, slot: usize) {
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
        self.retained_bytes -= usize::from(self.frames[slot].length);
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
    fn batch_completion_releases_only_selected_slots_and_preserves_other_frames() {
        let mut lane = Lane::new(1, 9022).unwrap();
        let mut bytes = [0; 256];
        for sequence in 1..=64u64 {
            let payload = [sequence as u8; 60];
            let n = crate::wire::encode_datagram(
                crate::wire::Header {
                    cookie: crate::UnquicUuid::from_bytes([1; 16]),
                    receive_tag: 1,
                    lane: 1,
                    packet_number: sequence,
                    record_area_len: 0,
                },
                &[crate::wire::Record::Data {
                    frame_id: (1 << 48) | sequence,
                    total_len: 60,
                    offset: 0,
                    bytes: &payload,
                }],
                &mut bytes,
            )
            .unwrap();
            assert_eq!(
                lane.admit(
                    &crate::wire::Datagram::decode(&bytes[..n]).unwrap(),
                    Duration::ZERO
                ),
                Admission::Fresh
            );
        }
        lane.finish_ready(0x5555_5555_5555_5555);
        assert_eq!(lane.retained_frames(), 32);
        assert_eq!(lane.retained_bytes(), 32 * 60);
        assert_eq!(
            lane.ready_frames(Duration::ZERO)
                .map(|f| f[0])
                .collect::<Vec<_>>(),
            (2..=64).step_by(2).collect::<Vec<_>>()
        );
        for sequence in 1..=64 {
            assert_eq!(lane.terminal.old(sequence), sequence % 2 == 1);
            assert_eq!(lane.find(sequence).is_none(), sequence % 2 == 1);
        }
        lane.finish_ready(1);
        assert!(lane.terminal.old(2));
        lane.finish_ready(u64::MAX);
        assert_eq!(lane.retained_frames(), 0);
        assert_eq!(lane.retained_bytes(), 0);
        assert_eq!(lane.free.len(), FRAME_CAPACITY);
        assert_eq!(lane.ready_frames(Duration::ZERO).count(), 0);
    }
    #[test]
    fn random_and_mutated_datagrams_never_panic_admission() {
        use crate::{
            UnquicUuid,
            tap::test_common::fuzz::{INPUTS, Rng},
            wire::{Header, encode_datagram},
        };
        let mut rng = Rng::new(0x4ea5_0001);
        let mut lane = Lane::new(1, 4000).unwrap();
        let payload = [0x5a; MAX_FRAME_LEN];
        let mut output = [0; 8192];
        let mut bytes = Vec::new();
        let mut ranges = [AckRange { start: 0, end: 0 }; MAX_ACK_RANGES];
        let (mut now, mut packet, mut base) = (Duration::ZERO, 1_u64, 1_u64);
        for _ in 0..INPUTS {
            // Mostly nearby frames, so pieces overlap, conflict and complete.
            base = base.max(lane.lower_bounds().1)
                + rng.below(3) as u64
                + if rng.below(1000) == 0 { 1 << 40 } else { 0 };
            let mut records = [Record::Probe; 4];
            let count = 1 + rng.below(records.len());
            for record in &mut records[..count] {
                let frame_id = (1 << 48) | ((base + rng.below(300) as u64) & SEQUENCE_MASK);
                let range = [128, 1600, MAX_FRAME_LEN - 13][rng.below(3)];
                let total = 14 + rng.below(range);
                let offset = rng.below(total);
                let length = 1 + rng.below((total - offset).min(1500));
                *record = if rng.below(8) == 0 {
                    Record::FrameAbandon {
                        lane: 1,
                        origin: 0,
                        reason: 1,
                        frame_id,
                    }
                } else {
                    Record::Data {
                        frame_id,
                        total_len: total as u32,
                        offset: offset as u32,
                        bytes: &payload[..length],
                    }
                };
            }
            packet += 1 + rng.below(2) as u64;
            let packet_number = match rng.below(10) {
                0 => packet.saturating_sub(rng.below(100_000) as u64).max(1),
                1 => {
                    packet += rng.below(70_000) as u64;
                    packet
                }
                _ => packet,
            };
            let header = Header {
                cookie: UnquicUuid::from_bytes([1; 16]),
                receive_tag: 1,
                lane: 1,
                record_area_len: 0,
                packet_number,
            };
            let Ok(n) = encode_datagram(header, &records[..count], &mut output) else {
                continue;
            };
            bytes.clear();
            bytes.extend_from_slice(&output[..n]);
            if rng.below(4) == 0 {
                rng.mutate(&mut bytes);
                // One mutated packet number must not age out every later packet.
                if let Some(number) = bytes.get_mut(40..48) {
                    number.copy_from_slice(&packet_number.to_be_bytes());
                }
            }
            if let Ok(datagram) = Datagram::decode(&bytes) {
                lane.admit(&datagram, now);
            }
            match rng.below(8) {
                0 => _ = lane.finish_front(),
                1 => lane.finish_ready(rng.bits()),
                2 => _ = lane.take_ack(now, &mut ranges),
                3 => _ = lane.abandon((1 << 48) | (base + rng.below(400) as u64)),
                // Rare expiry lets incomplete frames fill every slot.
                4 if rng.below(8) == 0 => now += Duration::from_millis(rng.below(150) as u64),
                5 => _ = lane.front(now),
                _ => {}
            }
        }
    }

    #[test]
    fn contiguous_copy_preserves_coverage_overlap_and_reused_slots() {
        let mut frame = Frame::empty();
        let first = [0x3c; MAX_FRAME_LEN];
        let rest = [0x5a; MAX_FRAME_LEN];
        for length in [14, 60, 64, 65, 128, 1514, MAX_FRAME_LEN] {
            for offset in [0, 1, 63, 64, 65, length / 2, length - 1] {
                if offset >= length {
                    continue;
                }
                for count in [1, 7, 63, 64, 65, length - offset] {
                    if count > length - offset {
                        continue;
                    }
                    frame.covered.fill(u64::MAX);
                    frame.start(1, length as u16, Duration::ZERO);
                    frame.piece(offset, &first[..count]);
                    assert_eq!(frame.remaining as usize, length - count);
                    assert_eq!(
                        frame.covered[..length.div_ceil(64)]
                            .iter()
                            .map(|word| word.count_ones() as usize)
                            .sum::<usize>(),
                        count
                    );
                    frame.piece(offset, &rest[..count]);
                    assert_eq!(frame.remaining as usize, length - count);
                    frame.piece(0, &rest[..length]);
                    assert_eq!(frame.remaining, 0);
                    for (at, &byte) in frame.bytes[..length].iter().enumerate() {
                        assert_eq!(
                            byte,
                            if (offset..offset + count).contains(&at) {
                                0x3c
                            } else {
                                0x5a
                            }
                        );
                    }
                }
            }
        }
        frame.start(2, 128, Duration::ZERO);
        frame.piece(0, &first[..60]);
        frame.piece(80, &first[..20]);
        frame.piece(60, &rest[..68]);
        assert_eq!(frame.remaining, 0);
        assert_eq!(&frame.bytes[60..80], &rest[..20]);
        assert_eq!(&frame.bytes[80..100], &first[..20]);
        assert_eq!(&frame.bytes[100..128], &rest[..28]);

        frame.start(3, 128, Duration::ZERO);
        for offset in (0..128).step_by(7) {
            frame.piece(offset, &first[..(128 - offset).min(7)]);
            assert_eq!(usize::from(frame.prefix_len), (offset + 7).min(128));
        }
        assert_eq!(frame.remaining, 0);
        assert_eq!(&frame.bytes[..128], &first[..128]);
    }

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
            let mut actual = [AckRange { start: 0, end: 0 }; MAX_ACK_RANGES];
            let count = window.ranges(highest, &mut actual);
            let mut expected: Vec<AckRange> = Vec::new();
            for &number in seen.iter().rev() {
                if let Some(last) = expected.last_mut()
                    && last.start.checked_sub(1) == Some(number)
                {
                    last.start = number;
                } else if expected.len() == MAX_ACK_RANGES {
                    break;
                } else {
                    expected.push(AckRange {
                        start: number,
                        end: number,
                    });
                }
            }
            assert_eq!(&actual[..count], expected);
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
    fn ack_ranges_cross_words_and_wrap_without_including_old_aliases() {
        let mut window = Window::new();
        let mut output = [AckRange { start: 0, end: 0 }; MAX_ACK_RANGES];
        assert_eq!(window.ranges(0, &mut output), 0);
        for number in 1..=WINDOW_LEN as u64 + 1 {
            window.mark(number);
        }
        assert_eq!(window.ranges(WINDOW_LEN as u64 + 1, &mut output), 1);
        assert_eq!(
            output[0],
            AckRange {
                start: 2,
                end: 65_537
            }
        );
        window.mark(u64::MAX);
        assert_eq!(window.ranges(u64::MAX, &mut output), 1);
        assert_eq!(
            output[0],
            AckRange {
                start: u64::MAX,
                end: u64::MAX
            }
        );
        for n in 1..=130 {
            window.mark(u64::MAX - n);
        }
        assert_eq!(window.ranges(u64::MAX, &mut output), 1);
        assert_eq!(
            output[0],
            AckRange {
                start: u64::MAX - 130,
                end: u64::MAX
            }
        );
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
