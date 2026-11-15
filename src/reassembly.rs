//! Bounded receive-lane reassembly and permanent duplicate suppression.
//!
//! The caller validates the connection tag and registers the lane's sole owner
//! before admission. Storage is allocated at construction; packet admission,
//! expiry and delivery never allocate, lock or perform I/O. A completed frame
//! stays reserved until a full TAP write succeeds or the caller discards it.

use crate::{
    MAX_FRAME_LEN,
    ack::Schedule,
    wire::{AckRange, AckRanges, Datagram, MAX_ACK_RANGES, Record, STRIPE_TAG_LEN},
};
use std::{collections::VecDeque, fmt, mem::size_of, time::Duration};

/// Number of packet numbers and frame sequences retained in each lane window.
pub const WINDOW_LEN: usize = 65_536;
/// Maximum incomplete plus queued complete frames in this initial lane table.
pub const FRAME_CAPACITY: usize = 128;
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
    /// Whole Ethernet frame without FCS or TAP metadata; for an offload
    /// frame, its descriptor followed by the frame.
    pub bytes: &'a [u8],
    /// Whether this is an offload frame.
    pub offload: bool,
    /// Whether its transport checksum is left partial (PARTIAL_CHECKSUMS).
    pub partial: bool,
    /// Whether its bytes start with a stripe tag (STRIPED_FLOWS).
    pub striped: bool,
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

/// Bytes per reassembly slot: half the largest ordinary frame, rounded to
/// whole coverage words. A 1500-byte frame takes one slot and a jumbo frame
/// two, so frames of a 1500-byte MTU get twice the slots in the same storage.
pub(crate) const SLOT_LEN: usize = (MAX_FRAME_LEN / 2).next_multiple_of(64);
const SLOT_WORDS: usize = SLOT_LEN / 64;

// Per-slot state. Bytes and coverage live in Lane::bytes and Lane::covered,
// contiguous across slots, so a long frame spans a run of whole slots and
// needs no storage of its own. Its sequence lives in Lane::sequences.
#[derive(Clone, Copy)]
struct Frame {
    length: u16,
    remaining: u16,
    prefix_len: u16,
    // Slots this frame holds, starting at its own; 1 for ordinary frames.
    span: u8,
    offload: bool,
    // The transport checksum is left partial; set by admission after start.
    partial: bool,
    // The bytes start with a stripe tag; set by admission after start.
    striped: bool,
    deadline: Duration,
}
impl Frame {
    const EMPTY: Self = Self {
        length: 0,
        remaining: 0,
        prefix_len: 0,
        span: 1,
        offload: false,
        partial: false,
        striped: false,
        deadline: Duration::ZERO,
    };

    fn start(&mut self, length: u16, offload: bool, now: Duration, covered: &mut [u64]) {
        self.length = length;
        self.remaining = length;
        self.prefix_len = 0;
        // Only frames longer than one slot span more.
        self.span = span(length);
        self.offload = offload;
        self.partial = false;
        self.striped = false;
        self.deadline = now.saturating_add(FRAME_LIFETIME);
        let words = usize::from(length).div_ceil(64).min(covered.len());
        covered[..words].fill(0);
    }

    /// `covered` and `bytes` start at this frame's first slot.
    fn piece(&mut self, covered: &mut [u64], storage: &mut [u8], offset: usize, bytes: &[u8]) {
        let end = offset + bytes.len();
        // Admission checks the range; this keeps every index below in bounds.
        if end > usize::from(self.length) || end > storage.len() {
            return;
        }
        // Bulk-copy an append only while no out-of-order bytes exist beyond the prefix.
        if !bytes.is_empty()
            && offset == usize::from(self.prefix_len)
            && self.remaining == self.length - self.prefix_len
        {
            storage[offset..end].copy_from_slice(bytes);
            covered[offset / 64..end.div_ceil(64)].fill(u64::MAX);
            covered[(end - 1) / 64] &= u64::MAX >> (63 - (end - 1) % 64);
            self.remaining -= bytes.len() as u16;
            self.prefix_len = end as u16;
            return;
        }
        let mut start = offset;
        while start < end {
            let bit = start % 64;
            let count = (end - start).min(64 - bit);
            let mask = (u64::MAX >> (64 - count)) << bit;
            let word = &mut covered[start / 64];
            let new = mask & !*word;
            if new == mask {
                storage[start..start + count]
                    .copy_from_slice(&bytes[start - offset..start - offset + count]);
            } else {
                // First-retained bytes win even if a conflicting duplicate arrives.
                let mut pending = new;
                while pending != 0 {
                    let at = start / 64 * 64 + pending.trailing_zeros() as usize;
                    storage[at] = bytes[at - offset];
                    pending &= pending - 1;
                }
            }
            self.remaining -= new.count_ones() as u16;
            *word |= mask;
            start += count;
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Reservation {
    sequence: u64,
    length: u16,
    offload: bool,
    partial: bool,
    striped: bool,
    slot: u8,
}

/// Slots a frame of this length holds: at most 15, for MAX_OFFLOAD_FRAME_LEN.
const fn span(length: u16) -> u8 {
    // Nearly every frame fits one slot; skip the division for them.
    if length as usize <= SLOT_LEN {
        return 1;
    }
    let slots = (length as usize).div_ceil(SLOT_LEN);
    if slots == 0 { 1 } else { slots as u8 }
}

const fn run(span: u8) -> u128 {
    u128::MAX >> (128 - span as u32)
}

/// Reserves `span` free slots: the lowest free slot for a one-slot frame, the
/// highest free run for a longer one, so the two rarely fragment each other.
fn take(reserved: &mut u128, span: u8) -> Option<usize> {
    let run = run(span);
    let slot = if span == 1 {
        Some((!*reserved).trailing_zeros() as usize).filter(|&slot| slot < FRAME_CAPACITY)
    } else {
        (0..=FRAME_CAPACITY - usize::from(span))
            .rev()
            .find(|&slot| *reserved & (run << slot) == 0)
    }?;
    *reserved |= run << slot;
    Some(slot)
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
    // Whether the connection negotiated offload frames.
    offload: bool,
    // Whether the connection negotiated partial checksums.
    partial: bool,
    // Whether this lane's owner hands striped frames to their reorder writer.
    striped: bool,
    frames: [Frame; FRAME_CAPACITY],
    covered: Box<[u64]>,
    bytes: Box<[u8]>,
    // Sequence held by each slot; zero marks a free slot or an offload frame's tail.
    sequences: [u64; FRAME_CAPACITY],
    // One bit per slot starting a frame, so scans visit only occupied slots.
    occupied: u128,
    // One bit per slot in use, including the later slots of long frames.
    reserved: u128,
    // New frames one datagram reserves, with their first slot; kept so no
    // datagram refills 1 KiB.
    reservations: [Reservation; 64],
    index: [u8; INDEX_LEN],
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
            offload: false,
            partial: false,
            striped: false,
            frames: [Frame::EMPTY; FRAME_CAPACITY],
            covered: vec![0; FRAME_CAPACITY * SLOT_WORDS].into_boxed_slice(),
            bytes: vec![0; FRAME_CAPACITY * SLOT_LEN].into_boxed_slice(),
            sequences: [0; FRAME_CAPACITY],
            occupied: 0,
            reserved: 0,
            reservations: [Reservation::default(); 64],
            index: [EMPTY; INDEX_LEN],
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
        size_of::<Self>()
            + FRAME_CAPACITY * (SLOT_LEN + SLOT_WORDS * size_of::<u64>() + size_of::<u8>())
    }

    /// Accepts offload frames until the next reset; call after reset when the
    /// connection negotiated them.
    pub fn accept_offload_frames(&mut self, accept: bool) {
        self.offload = accept;
    }

    pub(crate) const fn accepts_offload_frames(&self) -> bool {
        self.offload
    }

    /// Accepts frames with partial checksums until the next reset; call after
    /// reset when the connection negotiated them.
    pub fn accept_partial_checksums(&mut self, accept: bool) {
        self.partial = accept;
    }

    /// Accepts striped frames until the next reset; call after reset only
    /// when the connection negotiated them and the caller takes striped
    /// frames from [`Lane::front`] to their reorder writer.
    pub fn accept_striped_flows(&mut self, accept: bool) {
        self.striped = accept;
    }

    pub(crate) const fn accepts_striped_flows(&self) -> bool {
        self.striped
    }

    /// Registered remote lane ID.
    pub const fn id(&self) -> u16 {
        self.id
    }

    /// Number of retained incomplete and queued complete frames.
    pub fn retained_frames(&self) -> usize {
        self.occupied.count_ones() as usize
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
        self.offload = false;
        self.partial = false;
        self.striped = false;
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
        self.sequences.fill(0);
        self.occupied = 0;
        self.reserved = 0;
        self.index.fill(EMPTY);
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
        let mut count = 0;
        let mut highest = 0;
        let mut eliciting = false;
        let mut pressure = false;
        // Slots are chosen here, against the pre-admission mask, and taken in
        // the second pass; releases in between only free more slots.
        let mut trial = self.reserved;
        for record in datagram.records() {
            eliciting |= record.ack_eliciting();
            match record {
                Record::Data {
                    frame_id,
                    total_len,
                    offload,
                    partial,
                    striped,
                    ..
                } => {
                    // The codec bounds offload frames by MAX_OFFLOAD_FRAME_LEN;
                    // a stripe tag adds to the ordinary ceiling.
                    let tag = if striped { STRIPE_TAG_LEN as u32 } else { 0 };
                    if if offload {
                        !self.offload
                    } else {
                        total_len > u32::from(self.frame_limit) + tag
                    } || (partial && !self.partial)
                        || (striped && !self.striped)
                    {
                        return Admission::Invalid;
                    }
                    let sequence = frame_id & SEQUENCE_MASK;
                    highest = highest.max(sequence);
                    if self.terminal.old(sequence) {
                        continue;
                    }
                    let (length, kind) = if let Some(slot) = self.find(sequence) {
                        let frame = &self.frames[slot];
                        (frame.length, (frame.offload, frame.partial, frame.striped))
                    } else if let Some(reservation) = self
                        .reservations
                        .iter()
                        .take(count)
                        .find(|reservation| reservation.sequence == sequence)
                    {
                        (
                            reservation.length,
                            (
                                reservation.offload,
                                reservation.partial,
                                reservation.striped,
                            ),
                        )
                    } else {
                        // Ordinary frames hold one slot; a striped one past it holds two.
                        let span = span(total_len as u16);
                        let slot = take(&mut trial, span);
                        pressure |= slot.is_none();
                        // A datagram holds at most 64 records; any excess still counts as pressure.
                        match self.reservations.get_mut(count) {
                            Some(reservation) => {
                                *reservation = Reservation {
                                    sequence,
                                    length: total_len as u16,
                                    offload,
                                    partial,
                                    striped,
                                    slot: slot.unwrap_or_default() as u8,
                                };
                            }
                            None => pressure = true,
                        }
                        count += 1;
                        continue;
                    };
                    // Every piece of a frame carries the same flags.
                    if u32::from(length) != total_len || kind != (offload, partial, striped) {
                        return Admission::Invalid;
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
                    ..
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
                            // The first pass reserved a slot for every new frame.
                            let Some(reservation) = self
                                .reservations
                                .iter()
                                .take(count)
                                .find(|reservation| reservation.sequence == sequence)
                            else {
                                continue;
                            };
                            let slot = usize::from(reservation.slot);
                            let frame = &mut self.frames[slot];
                            frame.start(
                                total_len as u16,
                                reservation.offload,
                                now,
                                &mut self.covered[slot * SLOT_WORDS..],
                            );
                            frame.partial = reservation.partial;
                            frame.striped = reservation.striped;
                            self.reserved |= run(frame.span) << slot;
                            let deadline = frame.deadline;
                            self.sequences[slot] = sequence;
                            self.occupied |= 1 << slot;
                            self.retained_bytes += total_len as usize;
                            self.insert(slot);
                            self.deadline =
                                Some(self.deadline.map_or(deadline, |old| old.min(deadline)));
                            slot
                        }
                    };
                    let frame = &mut self.frames[slot];
                    let remaining = frame.remaining;
                    if remaining != 0 {
                        frame.piece(
                            &mut self.covered[slot * SLOT_WORDS..],
                            &mut self.bytes[slot * SLOT_LEN..],
                            offset as usize,
                            bytes,
                        );
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
        let mut occupied = self.occupied;
        while occupied != 0 {
            let slot = occupied.trailing_zeros() as usize;
            occupied &= occupied - 1;
            let sequence = self.sequences[slot];
            let frame = &self.frames[slot];
            if frame.deadline <= now {
                self.expired_incomplete_frames = self
                    .expired_incomplete_frames
                    .wrapping_add(u64::from(frame.remaining != 0));
                self.terminal.mark(sequence);
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
        let slot = usize::from(*self.ready.front()?);
        let frame = &self.frames[slot];
        Some(ReadyFrame {
            id: (u64::from(self.id) << 48) | self.sequences[slot],
            bytes: self.slot_bytes(slot),
            offload: frame.offload,
            partial: frame.partial,
            striped: frame.striped,
        })
    }

    /// Ordinary ready frames in queue order, stopping before any offload or
    /// striped frame, which go out through [`Lane::front`].
    pub(crate) fn ready_frames(&mut self, now: Duration) -> impl Iterator<Item = &[u8]> {
        self.expire(now);
        self.ready.iter().map_while(|&slot| {
            let slot = usize::from(slot);
            let frame = &self.frames[slot];
            (!frame.offload && !frame.striped).then(|| self.slot_bytes(slot))
        })
    }

    /// One bit per frame ready_frames(now) yields, at most 64, set for frames
    /// with partial checksums.
    pub(crate) fn ready_partial(&mut self, now: Duration) -> u64 {
        self.expire(now);
        self.ready
            .iter()
            .take(64)
            .map_while(|&slot| {
                let frame = &self.frames[usize::from(slot)];
                (!frame.offload && !frame.striped).then_some(frame.partial)
            })
            .enumerate()
            .fold(0, |mask, (index, partial)| {
                mask | u64::from(partial) << index
            })
    }

    fn slot_bytes(&self, slot: usize) -> &[u8] {
        let start = slot * SLOT_LEN;
        self.bytes
            .get(start..start + usize::from(self.frames[slot].length))
            .unwrap_or_default()
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
            self.terminal.mark(self.sequences[slot]);
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
        self.terminal.mark(self.sequences[slot]);
        self.release(slot);
        true
    }

    fn advance_frames(&mut self, highest: u64) {
        let old = self.terminal.lower;
        self.terminal.advance(highest);
        if old != self.terminal.lower {
            let mut occupied = self.occupied;
            while occupied != 0 {
                let slot = occupied.trailing_zeros() as usize;
                occupied &= occupied - 1;
                if self.sequences[slot] < self.terminal.lower {
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
            if self.sequences[usize::from(slot)] == sequence {
                return Some(usize::from(slot));
            }
            at = (at + 1) % INDEX_LEN;
        }
    }

    fn insert(&mut self, slot: usize) {
        let mut at = hash(self.sequences[slot]);
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
        let mut hole = hash(self.sequences[slot]);
        while self.index[hole] != slot as u8 {
            hole = (hole + 1) % INDEX_LEN;
        }
        self.index[hole] = EMPTY;
        let mut at = (hole + 1) % INDEX_LEN;
        while self.index[at] != EMPTY {
            let home = hash(self.sequences[usize::from(self.index[at])]);
            if (hole.wrapping_sub(home) % INDEX_LEN) < (at.wrapping_sub(home) % INDEX_LEN) {
                self.index[hole] = self.index[at];
                self.index[at] = EMPTY;
                hole = at;
            }
            at = (at + 1) % INDEX_LEN;
        }
        self.retained_bytes -= usize::from(self.frames[slot].length);
        self.sequences[slot] = 0;
        self.occupied &= !(1 << slot);
        self.reserved &= !(run(self.frames[slot].span) << slot);
    }
}

const fn hash(sequence: u64) -> usize {
    // Multiplication folds high sequence bits into the small, half-full table.
    (sequence.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> (64 - INDEX_LEN.trailing_zeros())) as usize
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
                    offload: false,
                    partial: false,
                    striped: false,
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
        assert_eq!(lane.reserved, 0);
        assert_eq!(lane.ready_frames(Duration::ZERO).count(), 0);
    }
    #[test]
    fn ordinary_batches_stop_before_an_offload_frame() {
        let mut lane = Lane::new(1, 9022).unwrap();
        lane.accept_offload_frames(true);
        let mut bytes = [0; 256];
        for (sequence, offload) in [(1, false), (2, true), (3, false)] {
            let n = crate::wire::encode_datagram(
                crate::wire::Header {
                    cookie: crate::UnquicUuid::from_bytes([1; 16]),
                    receive_tag: 1,
                    lane: 1,
                    packet_number: sequence,
                    record_area_len: 0,
                },
                &[Record::Data {
                    frame_id: (1 << 48) | sequence,
                    total_len: 60,
                    offset: 0,
                    bytes: &[sequence as u8; 60],
                    offload,
                    partial: false,
                    striped: false,
                }],
                &mut bytes,
            )
            .unwrap();
            let datagram = Datagram::decode(&bytes[..n]).unwrap();
            assert_eq!(lane.admit(&datagram, Duration::ZERO), Admission::Fresh);
        }
        let firsts = |lane: &mut Lane| {
            lane.ready_frames(Duration::ZERO)
                .map(|frame| frame[0])
                .collect::<Vec<_>>()
        };
        assert_eq!(firsts(&mut lane), [1]);
        lane.finish_ready(1);
        assert_eq!(firsts(&mut lane), []);
        assert!(lane.front(Duration::ZERO).unwrap().offload);
        assert!(lane.finish_front());
        assert_eq!(firsts(&mut lane), [3]);
    }

    #[test]
    fn striped_frames_need_acceptance_leave_through_front_and_may_span_two_slots() {
        let mut lane = Lane::new(1, 9022).unwrap();
        lane.accept_offload_frames(true);
        lane.accept_partial_checksums(true);
        let mut bytes = vec![0; 9200];
        let mut admit = |lane: &mut Lane, number: u64, piece: Record<'_>| {
            let n = crate::wire::encode_datagram(
                crate::wire::Header {
                    cookie: crate::UnquicUuid::from_bytes([1; 16]),
                    receive_tag: 1,
                    lane: 1,
                    packet_number: number,
                    record_area_len: 0,
                },
                &[piece],
                &mut bytes,
            )
            .unwrap_or_else(|error| panic!("packet {number}: {error:?}"));
            lane.admit(&Datagram::decode(&bytes[..n]).unwrap(), Duration::ZERO)
        };
        let tag = |sequence: u64| ((7_u64 << 48) | sequence).to_be_bytes();
        let short = [&tag(1)[..], &[1; 72]].concat();
        let piece = |sequence: u64, frame: &'static [u8], offset: u32, striped| Record::Data {
            frame_id: (1 << 48) | sequence,
            total_len: frame.len() as u32,
            offset,
            bytes: frame,
            offload: false,
            partial: false,
            striped,
        };
        let short: &'static [u8] = short.leak();
        assert_eq!(
            admit(&mut lane, 1, piece(1, short, 0, true)),
            Admission::Invalid
        );
        assert_eq!(lane.retained_bytes(), 0);
        lane.accept_striped_flows(true);
        // An ordinary frame, then a striped one: the batch stops before it.
        let plain: &'static [u8] = vec![2; 60].leak();
        assert_eq!(
            admit(&mut lane, 1, piece(1, plain, 0, false)),
            Admission::Fresh
        );
        assert_eq!(
            admit(&mut lane, 2, piece(2, short, 0, true)),
            Admission::Fresh
        );
        assert_eq!(lane.ready_frames(Duration::ZERO).count(), 1);
        lane.finish_ready(1);
        let front = lane.front(Duration::ZERO).unwrap();
        assert!(front.striped && !front.offload);
        assert_eq!(crate::wire::stripe_tag(front.bytes), Some((7, 1)));
        assert_eq!(&front.bytes[STRIPE_TAG_LEN..], &[1; 72]);
        assert!(lane.finish_front());
        // A striped frame past one slot holds a run of two, and its later
        // pieces must repeat the flag.
        let long = [&tag(2)[..], &vec![3; MAX_FRAME_LEN]].concat();
        let long: &'static [u8] = long.leak();
        let long_piece = |offset: usize, end: usize, striped| Record::Data {
            frame_id: (1 << 48) | 3,
            total_len: long.len() as u32,
            offset: offset as u32,
            bytes: &long[offset..end],
            offload: false,
            partial: false,
            striped,
        };
        assert_eq!(
            admit(&mut lane, 3, long_piece(0, 4000, true)),
            Admission::Fresh
        );
        assert_eq!(
            admit(&mut lane, 4, long_piece(4000, long.len(), true)),
            Admission::Fresh
        );
        let front = lane.front(Duration::ZERO).unwrap();
        assert!(front.striped);
        assert_eq!(front.bytes, long);
        assert_eq!(lane.reserved.count_ones(), 2);
        assert!(lane.finish_front());
        assert_eq!(lane.reserved, 0);
        // A later piece of a striped frame must repeat the flag.
        let split = |offset: usize, striped| Record::Data {
            frame_id: (1 << 48) | 4,
            total_len: short.len() as u32,
            offset: offset as u32,
            bytes: &short[offset..offset + 40],
            offload: false,
            partial: false,
            striped,
        };
        assert_eq!(admit(&mut lane, 5, split(0, true)), Admission::Fresh);
        assert_eq!(admit(&mut lane, 6, split(40, false)), Admission::Invalid);
        assert_eq!(admit(&mut lane, 6, split(40, true)), Admission::Fresh);
        // A reset forgets acceptance.
        lane.reset(1, 9022).unwrap();
        assert_eq!(
            admit(&mut lane, 1, piece(1, short, 0, true)),
            Admission::Invalid
        );
    }

    #[test]
    fn partial_checksums_need_acceptance_one_flag_per_frame_and_mark_ready_frames() {
        let mut lane = Lane::new(1, 9022).unwrap();
        let mut bytes = [0; 256];
        let mut admit =
            |lane: &mut Lane, number: u64, sequence: u64, offset: u32, flags: (bool, bool)| {
                let (offload, partial) = flags;
                let n = crate::wire::encode_datagram(
                    crate::wire::Header {
                        cookie: crate::UnquicUuid::from_bytes([1; 16]),
                        receive_tag: 1,
                        lane: 1,
                        packet_number: number,
                        record_area_len: 0,
                    },
                    &[Record::Data {
                        frame_id: (1 << 48) | sequence,
                        total_len: 60,
                        offset,
                        bytes: &[sequence as u8; 30],
                        offload,
                        partial,
                        striped: false,
                    }],
                    &mut bytes,
                )
                .unwrap();
                lane.admit(&Datagram::decode(&bytes[..n]).unwrap(), Duration::ZERO)
            };
        assert_eq!(admit(&mut lane, 1, 1, 0, (false, true)), Admission::Invalid);
        lane.accept_partial_checksums(true);
        lane.accept_offload_frames(true);
        // Frames 1 and 3 are partial, 2 is not, 4 is an offload frame.
        for (number, sequence, flags) in [
            (1, 1, (false, true)),
            (2, 2, (false, false)),
            (3, 3, (false, true)),
            (4, 4, (true, false)),
            (5, 5, (false, true)),
        ] {
            assert_eq!(
                admit(&mut lane, number, sequence, 0, flags),
                Admission::Fresh
            );
        }
        // A later piece must repeat its frame's flags.
        assert_eq!(
            admit(&mut lane, 6, 1, 30, (false, false)),
            Admission::Invalid
        );
        assert_eq!(
            admit(&mut lane, 6, 2, 30, (false, true)),
            Admission::Invalid
        );
        for (number, sequence, flags) in [
            (6, 1, (false, true)),
            (7, 2, (false, false)),
            (8, 3, (false, true)),
            (9, 4, (true, false)),
            (10, 5, (false, true)),
        ] {
            assert_eq!(
                admit(&mut lane, number, sequence, 30, flags),
                Admission::Fresh
            );
        }
        // The mask stops where ready_frames stops, before the offload frame.
        assert_eq!(lane.ready_frames(Duration::ZERO).count(), 3);
        assert_eq!(lane.ready_partial(Duration::ZERO), 0b101);
        lane.finish_ready(0b111);
        assert!(lane.front(Duration::ZERO).unwrap().offload);
        assert!(!lane.front(Duration::ZERO).unwrap().partial);
        assert!(lane.finish_front());
        assert!(lane.front(Duration::ZERO).unwrap().partial);
        assert_eq!(lane.ready_partial(Duration::ZERO), 1);
        // A reset forgets acceptance.
        lane.reset(1, 9022).unwrap();
        assert_eq!(admit(&mut lane, 1, 1, 0, (false, true)), Admission::Invalid);
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
        lane.accept_offload_frames(true);
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
                // Offload frames follow their sequence, so most of their pieces agree.
                let offload = (frame_id + rng.below(40) as u64).is_multiple_of(8);
                let range = [128, 1600, MAX_FRAME_LEN - 13, 65_536 - 24]
                    [rng.below(3 + usize::from(offload))];
                let total = if offload { 24 } else { 14 } + rng.below(range);
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
                        offload,
                        partial: false,
                        striped: false,
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
            // Every frame owns exactly its run of slots, and runs never overlap.
            let mut runs = 0;
            let mut occupied = lane.occupied;
            while occupied != 0 {
                let slot = occupied.trailing_zeros() as usize;
                occupied &= occupied - 1;
                let frame = &lane.frames[slot];
                assert_eq!(frame.span, span(frame.length));
                assert_eq!(runs & (run(frame.span) << slot), 0);
                runs |= run(frame.span) << slot;
            }
            assert_eq!(runs, lane.reserved);
        }
    }

    #[test]
    fn frames_of_a_1500_byte_mtu_take_one_slot_and_jumbo_frames_two() {
        assert_eq!(FRAME_CAPACITY * SLOT_LEN, 128 * 4544);
        assert_eq!(span(1514), 1);
        assert_eq!(span(SLOT_LEN as u16), 1);
        assert_eq!(span(SLOT_LEN as u16 + 1), 2);
        assert_eq!(span(MAX_FRAME_LEN as u16), 2);
        // A lane holds 128 one-slot frames, or 64 jumbo frames, at once.
        let mut reserved = 0;
        assert!((0..FRAME_CAPACITY).all(|_| take(&mut reserved, 1).is_some()));
        assert_eq!(take(&mut reserved, 1), None);
        let mut reserved = 0;
        assert!((0..FRAME_CAPACITY / 2).all(|_| take(&mut reserved, 2).is_some()));
        assert_eq!(take(&mut reserved, 2), None);
    }

    #[test]
    fn contiguous_copy_preserves_coverage_overlap_and_reused_slots() {
        // Two slots' storage, enough for the largest ordinary frame, viewed
        // as the fields a frame used to own.
        struct Slot {
            frame: Frame,
            covered: [u64; 2 * SLOT_WORDS],
            bytes: [u8; MAX_FRAME_LEN],
        }
        impl Slot {
            fn start(&mut self, length: u16, now: Duration) {
                self.frame.start(length, false, now, &mut self.covered);
            }
            fn piece(&mut self, offset: usize, bytes: &[u8]) {
                self.frame
                    .piece(&mut self.covered, &mut self.bytes, offset, bytes);
            }
        }
        impl std::ops::Deref for Slot {
            type Target = Frame;
            fn deref(&self) -> &Frame {
                &self.frame
            }
        }
        let mut frame = Box::new(Slot {
            frame: Frame::EMPTY,
            covered: [0; 2 * SLOT_WORDS],
            bytes: [0; MAX_FRAME_LEN],
        });
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
                    frame.start(length as u16, Duration::ZERO);
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
        frame.start(128, Duration::ZERO);
        frame.piece(0, &first[..60]);
        frame.piece(80, &first[..20]);
        frame.piece(60, &rest[..68]);
        assert_eq!(frame.remaining, 0);
        assert_eq!(&frame.bytes[60..80], &rest[..20]);
        assert_eq!(&frame.bytes[80..100], &first[..20]);
        assert_eq!(&frame.bytes[100..128], &rest[..28]);

        frame.start(128, Duration::ZERO);
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
        for home in [0, 120, INDEX_LEN - 1] {
            let colliding: Vec<_> = (1..200_000)
                .filter(|&seq| hash(seq) == home)
                .take(FRAME_CAPACITY)
                .collect();
            assert_eq!(colliding.len(), FRAME_CAPACITY);
            lane.reserved = u128::MAX;
            for (slot, &seq) in colliding.iter().enumerate() {
                lane.sequences[slot] = seq;
                lane.insert(slot);
            }
            for step in 0..FRAME_CAPACITY {
                let slot = (step * 17) % FRAME_CAPACITY;
                lane.release(slot);
                for (i, &seq) in colliding.iter().enumerate() {
                    assert_eq!(
                        lane.find(seq),
                        if lane.sequences[i] == 0 {
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
