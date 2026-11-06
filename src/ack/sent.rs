//! One connection/lane's fixed sent-packet history; payload storage is separate.

use crate::wire::{AckRange, AckRanges, MAX_ACK_RANGES};
use std::{collections::TryReserveError, num::NonZeroU16, time::Duration};

/// Maximum retained packet-number span per lane.
pub const SENT_CAPACITY: usize = 65_536;
const WORDS: usize = SENT_CAPACITY / 64;
const ELICITING: u16 = 1 << 14;
const CHARGED: u16 = 1 << 15;
const SIZE_MASK: u16 = ELICITING - 1;

#[derive(Clone, Copy, Default)]
struct Entry {
    // Split Duration to use its padding for transmission metadata.
    sent_seconds: u64,
    sent_nanos: u32,
    size_flags: u16,
    sample_slot: Option<NonZeroU16>,
}

/// A successfully submitted physical transmission, independent of payload lifetime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SentPacket {
    /// Packet number in this history's connection and lane.
    pub number: u64,
    /// Pre-syscall timestamp of a successfully submitted physical transmission.
    pub sent_at: Duration,
    /// IPv6 packet bytes, including the IPv6 and UDP headers.
    pub outer_bytes: u16,
    /// Whether this datagram elicits an ACK.
    pub eliciting: bool,
    /// Whether it consumes ordinary congestion-window credit.
    pub charged: bool,
    /// Reserved sampler handle. ACK or explicit retirement transfers its ownership.
    pub sample_slot: Option<NonZeroU16>,
}

/// One number removed from sender history, including a canceled or skipped hole.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetiredEntry {
    /// Permanently retired packet number.
    pub number: u64,
    /// Sampling storage to reclaim after earlier accounting reports have drained.
    pub sample_slot: Option<NonZeroU16>,
}

/// One newly acknowledged transmission; duplicate ACKs produce no event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Acknowledged {
    /// Retained submission metadata, also available for late ACKs after loss.
    pub packet: SentPacket,
    /// Whether this ACK releases an outstanding congestion charge.
    pub was_in_flight: bool,
}

/// Accounting changes from one completely validated ACK record.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AckProgress {
    /// Newly acknowledged physical transmissions, including feedback-only packets.
    pub packets: u32,
    /// Whether any newly acknowledged transmission elicits ACKs.
    pub eliciting: bool,
    /// Ordinary inflight bytes released, excluding previously declared losses.
    pub released_bytes: u64,
    /// Largest ACKed packet's send time, only when RFC 9002 permits an RTT sample.
    pub rtt_sent_at: Option<Duration>,
}

/// Invalid history operation or ACK; rejection does not mutate history.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryError {
    /// Zero/reused number, invalid outer size or an uncharged ACK-eliciting packet.
    Invalid,
    /// Advancing history would forget a reservation, charge, sample handle or pending ACK.
    Full,
    /// An ACK names a number above the highest successful submission.
    Future,
    /// An ACK covers a retained number that was never successfully submitted.
    Unsent,
    /// An earlier validated ACK still requires bounded application.
    Busy,
}

impl std::fmt::Display for HistoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Invalid => "invalid or reused packet reservation",
            Self::Full => {
                "sent history is pinned by reservations, charges, samples or a pending ACK"
            }
            Self::Future => "ACK exceeds the highest submitted packet number",
            Self::Unsent => "ACK covers an unsent packet number",
            Self::Busy => "a validated ACK is still being applied",
        })
    }
}

impl std::error::Error for HistoryError {}

struct PendingAck {
    ranges: [AckRange; MAX_ACK_RANGES],
    len: usize,
    range: usize,
    next_word: u64,
    word: u64,
    bits: u64,
    largest: u64,
    progress: AckProgress,
}

/// Resumable visit of one lane's outstanding charges, bounded by bitmap work.
///
/// The upper packet number is fixed at creation, so continuous transmission
/// cannot extend the pass. ACK/loss and history eviction may run between steps.
/// A reservation submitted after its number was passed needs a later scan.
/// Discard this cursor before resetting or replacing its history/connection.
pub struct OutstandingScan {
    next: Option<u64>,
    end: u64,
}

impl OutstandingScan {
    /// Whether this pass has finished, including an empty history.
    pub const fn is_complete(&self) -> bool {
        self.next.is_none()
    }

    /// Returns one still-outstanding packet, spending one unit per bitmap read.
    ///
    /// None means either exhaustion of the budget or completion; check
    /// is_complete. Zero budget leaves the cursor unchanged. Call History::lose
    /// before reporting a loss: a pending validated ACK may protect this packet.
    pub fn advance(&mut self, history: &History, budget: &mut usize) -> Option<SentPacket> {
        while *budget != 0 {
            let start = self.next?.max(history.floor);
            if start > self.end || history.inflight_bytes == 0 {
                self.next = None;
                return None;
            }
            *budget -= 1;
            let end = self.end.min(start | 63);
            let mask = (u64::MAX << (start % 64)) & (u64::MAX >> (63 - end % 64));
            let bits = history.inflight[(start >> 6) as usize & (WORDS - 1)] & mask;
            let number = if bits == 0 {
                end
            } else {
                (start & !63) | u64::from(bits.trailing_zeros())
            };
            self.next = if number == self.end {
                None
            } else {
                number.checked_add(1)
            };
            if bits != 0 {
                return Some(history.metadata(number));
            }
        }
        None
    }
}

/// Bounded sender history with allocation-free reservation, ACK and loss operations.
///
/// Each packet uses 16 bytes of metadata plus four bitmap bits, preserving the
/// full range and nanosecond precision of `Duration`. Sampling handles occupy
/// padding freed by packing outer size and transmission flags into one u16.
///
/// Allocate once per connection/lane packet-number space, not per shared UDP
/// socket. Reserve before I/O, submit only the successful prefix, and cancel
/// attempted-but-unsent reservations. Retries use fresh packet numbers.
/// Uncharged feedback remains ACKable but does not block history advancement.
pub struct History {
    entries: Box<[Entry]>,
    reserved: [u64; WORDS],
    submitted: [u64; WORDS],
    acknowledged: [u64; WORDS],
    inflight: [u64; WORDS],
    floor: u64,
    allocated: u64,
    highest_sent: u64,
    inflight_bytes: u64,
    samples: u32,
    final_retired: bool,
    pending: Option<PendingAck>,
}

impl History {
    /// Fallibly reserves all storage up front; later operations never allocate.
    pub fn new() -> Result<Self, TryReserveError> {
        let mut entries = Vec::new();
        entries.try_reserve_exact(SENT_CAPACITY)?;
        entries.resize(SENT_CAPACITY, Entry::default());
        Ok(Self {
            entries: entries.into_boxed_slice(),
            reserved: [0; WORDS],
            submitted: [0; WORDS],
            acknowledged: [0; WORDS],
            inflight: [0; WORDS],
            floor: 1,
            allocated: 0,
            highest_sent: 0,
            inflight_bytes: 0,
            samples: 0,
            final_retired: false,
            pending: None,
        })
    }

    /// Reserved storage, including inline bitmaps and excluding allocator overhead.
    pub const fn storage_bytes() -> usize {
        size_of::<Self>() + SENT_CAPACITY * size_of::<Entry>()
    }

    /// Numbers below this bound are permanently outside validation history.
    /// Retiring u64::MAX also makes that last number old without wrapping the bound.
    pub const fn floor(&self) -> u64 {
        self.floor
    }

    /// Highest actual successful submission, not merely an allocated number.
    pub const fn highest_sent(&self) -> u64 {
        self.highest_sent
    }

    /// Outstanding ordinary congestion charges, unaffected by payload expiry.
    pub const fn inflight_bytes(&self) -> u64 {
        self.inflight_bytes
    }

    /// Reserves a fresh increasing number before sending; gaps become known-unsent holes.
    ///
    /// Outer size includes 48 bytes of IPv6/UDP overhead. Every ACK-eliciting
    /// transmission is charged; feedback-only traffic may be uncharged.
    /// While any sampler handles remain, advance the floor explicitly with
    /// retire_oldest_resolved before reserving a number outside the current span.
    pub fn reserve(
        &mut self,
        number: u64,
        outer_bytes: u16,
        eliciting: bool,
        charged: bool,
    ) -> Result<(), HistoryError> {
        if number == 0
            || number <= self.allocated
            || !(100..=9000).contains(&outer_bytes)
            || (eliciting && !charged)
        {
            return Err(HistoryError::Invalid);
        }
        let floor = self
            .floor
            .max(number.saturating_sub(SENT_CAPACITY as u64 - 1));
        if floor != self.floor && (self.pending.is_some() || self.samples != 0) {
            return Err(HistoryError::Full);
        }
        let end = self.allocated.min(floor - 1);
        for (word, mask) in words(self.floor, end) {
            let index = word as usize & (WORDS - 1);
            if (self.reserved[index] | self.inflight[index]) & mask != 0 {
                return Err(HistoryError::Full);
            }
        }
        for (word, mask) in words(self.floor, end) {
            let index = word as usize & (WORDS - 1);
            self.submitted[index] &= !mask;
            self.acknowledged[index] &= !mask;
        }
        self.floor = floor;
        self.allocated = number;
        let index = number as usize & (SENT_CAPACITY - 1);
        self.entries[index] = Entry {
            sent_seconds: 0,
            sent_nanos: 0,
            size_flags: outer_bytes
                | if eliciting { ELICITING } else { 0 }
                | if charged { CHARGED } else { 0 },
            sample_slot: None,
        };
        self.reserved[index / 64] |= 1 << (index % 64);
        Ok(())
    }

    /// Retires one resolved number at the floor, including a canceled or skipped hole.
    ///
    /// One call does constant work. None means empty, pinned by a reservation or
    /// charge. The maximum packet number can retire once without wrapping the floor.
    /// A pending validated ACK returns Busy without advancing the floor.
    ///
    /// Apply earlier accounting reports before reclaiming the returned number's
    /// sampler/journal storage. This releases no charge and declares no loss;
    /// subsequent ACKs below the new floor cannot generate delivery callbacks.
    pub fn retire_oldest_resolved(&mut self) -> Result<Option<RetiredEntry>, HistoryError> {
        if self.pending.is_some() {
            return Err(HistoryError::Busy);
        }
        let number = self.floor;
        if number > self.allocated || self.final_retired {
            return Ok(None);
        }
        let (word, bit) = location(number);
        if (self.reserved[word] | self.inflight[word]) & bit != 0 {
            return Ok(None);
        }
        self.submitted[word] &= !bit;
        self.acknowledged[word] &= !bit;
        let sample_slot = self.take_sample(number);
        self.floor = number.saturating_add(1);
        self.final_retired = number == u64::MAX;
        Ok(Some(RetiredEntry {
            number,
            sample_slot,
        }))
    }

    /// Cancels an unsent reservation without permitting reuse of its number.
    pub fn cancel(&mut self, number: u64) -> bool {
        if !self.contains(number) {
            return false;
        }
        let (word, bit) = location(number);
        let existed = self.reserved[word] & bit != 0;
        self.reserved[word] &= !bit;
        existed
    }

    /// Records a reserved packet after successful submission, exactly once.
    ///
    /// False means no matching reservation. The caller must reserve every
    /// packet before its syscall and retain reservations until reporting results.
    pub fn submit(&mut self, number: u64, now: Duration) -> bool {
        if !self.cancel(number) {
            return false;
        }
        let (word, bit) = location(number);
        let entry = &mut self.entries[number as usize & (SENT_CAPACITY - 1)];
        entry.sent_seconds = now.as_secs();
        entry.sent_nanos = now.subsec_nanos();
        self.submitted[word] |= bit;
        if entry.size_flags & CHARGED != 0 {
            self.inflight[word] |= bit;
            self.inflight_bytes += u64::from(entry.size_flags & SIZE_MASK);
        }
        self.highest_sent = self.highest_sent.max(number);
        true
    }

    /// Submits one charged packet with a sampler slot reserved before its syscall.
    ///
    /// The grant owns unused slots; only successful submissions attach them here.
    /// An ACK takes the handle into its report, loss only copies it, and explicit
    /// retirement takes it for reclamation. Never reuse it before those reports drain.
    pub fn submit_sampled(&mut self, number: u64, now: Duration, sample_slot: NonZeroU16) -> bool {
        if !self.contains(number)
            || self.entries[number as usize & (SENT_CAPACITY - 1)].size_flags & CHARGED == 0
            || !self.submit(number, now)
        {
            return false;
        }
        self.entries[number as usize & (SENT_CAPACITY - 1)].sample_slot = Some(sample_slot);
        self.samples += 1;
        true
    }

    /// Retained successful submission, including acknowledged or lost packets.
    pub fn packet(&self, number: u64) -> Option<SentPacket> {
        let (word, bit) = location(number);
        (self.contains(number) && self.submitted[word] & bit != 0).then(|| self.metadata(number))
    }

    /// Checks the whole ACK without mutation, bounded by the retained bitmap span.
    pub fn validate(&self, ranges: AckRanges<'_>) -> Result<(), HistoryError> {
        for range in ranges.iter() {
            if range.end > self.highest_sent {
                return Err(HistoryError::Future);
            }
            if self.final_retired {
                continue;
            }
            for (word, mask) in words(range.start.max(self.floor), range.end) {
                if self.submitted[word as usize & (WORDS - 1)] & mask != mask {
                    return Err(HistoryError::Unsent);
                }
            }
        }
        Ok(())
    }

    /// Whether a validated ACK still has bounded work to apply.
    pub const fn ack_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Validates a whole ACK and retains its ranges without applying callbacks.
    ///
    /// Validation visits at most 1025 bitmap words plus 31 repeated boundary-word
    /// visits. Application is separately budgeted by advance_ack. Until it
    /// finishes, reset and competing ACKs are rejected and the history floor
    /// cannot advance. Loss cannot settle a packet covered by this ACK.
    ///
    /// Retain the original arrival time and delay outside this history; use them
    /// for every resulting delivery/RTT report, even across later worker turns.
    pub fn begin_ack(&mut self, ranges: AckRanges<'_>) -> Result<(), HistoryError> {
        if self.pending.is_some() {
            return Err(HistoryError::Busy);
        }
        self.validate(ranges)?;
        let mut pending = PendingAck {
            ranges: [AckRange { start: 0, end: 0 }; MAX_ACK_RANGES],
            len: ranges.len(),
            range: 0,
            next_word: 0,
            word: 0,
            bits: 0,
            largest: 0,
            progress: AckProgress::default(),
        };
        for (slot, range) in pending.ranges.iter_mut().zip(ranges.iter()) {
            *slot = range;
        }
        pending.largest = pending.ranges[0].end;
        pending.next_word = pending.ranges[0].start.max(self.floor) >> 6;
        self.pending = Some(pending);
        Ok(())
    }

    /// Applies a validated ACK with a fixed work budget and no allocation.
    ///
    /// Each bitmap-word visit, range transition and callback costs one unit.
    /// Returns cumulative progress only on completion, or None when work remains
    /// (also when no ACK is pending). A zero budget leaves all state unchanged.
    /// Reserve callback/report capacity first; callback events cannot be dropped.
    /// Each callback takes its sampling handle; the history no longer owns it.
    pub fn advance_ack(
        &mut self,
        budget: usize,
        mut accepted: impl FnMut(Acknowledged),
    ) -> Option<AckProgress> {
        let mut pending = self.pending.take()?;
        for _ in 0..budget {
            if pending.bits != 0 {
                let bit_index = pending.bits.trailing_zeros();
                let bit = 1_u64 << bit_index;
                let number = (pending.word << 6) | u64::from(bit_index);
                pending.bits &= !bit;
                let index = pending.word as usize & (WORDS - 1);
                let packet = self.metadata(number);
                self.take_sample(number);
                let was_in_flight = self.inflight[index] & bit != 0;
                self.inflight[index] &= !bit;
                self.acknowledged[index] |= bit;
                pending.progress.packets += 1;
                pending.progress.eliciting |= packet.eliciting;
                if was_in_flight {
                    pending.progress.released_bytes += u64::from(packet.outer_bytes);
                    self.inflight_bytes -= u64::from(packet.outer_bytes);
                }
                if number == pending.largest {
                    pending.progress.rtt_sent_at = Some(packet.sent_at);
                }
                accepted(Acknowledged {
                    packet,
                    was_in_flight,
                });
            } else if pending.range < pending.len {
                let range = pending.ranges[pending.range];
                if self.final_retired
                    || range.end < self.floor
                    || pending.next_word > range.end >> 6
                {
                    pending.range += 1;
                    if pending.range < pending.len {
                        pending.next_word =
                            pending.ranges[pending.range].start.max(self.floor) >> 6;
                    }
                    continue;
                }
                let word = pending.next_word;
                let start = range.start.max(self.floor).max(word << 6);
                let end = range.end.min((word << 6) | 63);
                let mask = (u64::MAX << (start % 64)) & (u64::MAX >> (63 - end % 64));
                pending.word = word;
                pending.next_word += 1;
                pending.bits = mask & !self.acknowledged[word as usize & (WORDS - 1)];
            } else {
                break;
            }
        }
        if pending.range == pending.len && pending.bits == 0 {
            if !pending.progress.eliciting {
                pending.progress.rtt_sent_at = None;
            }
            Some(pending.progress)
        } else {
            self.pending = Some(pending);
            None
        }
    }

    /// Applies one whole ACK synchronously; worker loops should use bounded application.
    ///
    /// Validation precedes callbacks. Old portions and duplicate ACKs are ignored;
    /// a late ACK after loss cannot release congestion credit a second time.
    pub fn acknowledge(
        &mut self,
        ranges: AckRanges<'_>,
        accepted: impl FnMut(Acknowledged),
    ) -> Result<AckProgress, HistoryError> {
        self.begin_ack(ranges)?;
        Ok(self.advance_ack(usize::MAX, accepted).unwrap_or_default())
    }

    /// Declares one outstanding charged packet lost and releases its charge once.
    ///
    /// Retains submission identity for late ACKs. A pending validated ACK protects
    /// its covered packets from loss while application yields. Payload policy is separate.
    pub fn lose(&mut self, number: u64) -> Option<SentPacket> {
        if !self.contains(number)
            || self.pending.as_ref().is_some_and(|pending| {
                pending.ranges[..pending.len]
                    .iter()
                    .any(|range| (range.start..=range.end).contains(&number))
            })
        {
            return None;
        }
        let (word, bit) = location(number);
        if self.inflight[word] & bit == 0 {
            return None;
        }
        let packet = self.metadata(number);
        self.inflight[word] &= !bit;
        self.inflight_bytes -= u64::from(packet.outer_bytes);
        Some(packet)
    }

    /// Starts a bounded outstanding-charge pass through the current allocated number.
    ///
    /// Newer packets need a later pass. Loss policy and per-turn piece/report budgets
    /// belong to the caller; this cursor bounds scanning even across empty words.
    pub const fn outstanding_scan(&self) -> OutstandingScan {
        OutstandingScan {
            next: if self.inflight_bytes == 0 {
                None
            } else {
                Some(self.floor)
            },
            end: self.allocated,
        }
    }

    /// Visits outstanding charged packets synchronously in number order.
    ///
    /// Worker turns should use outstanding_scan to bound visits of empty words too.
    pub fn outstanding(&self) -> impl Iterator<Item = SentPacket> + '_ {
        let mut scan = self.outstanding_scan();
        let mut budget = usize::MAX;
        std::iter::from_fn(move || scan.advance(self, &mut budget))
    }

    /// Reuses storage only after every charge, reservation and sampling handle is settled.
    /// Callers must also drain earlier accounting reports before connection replacement.
    pub fn reset(&mut self) -> Result<(), HistoryError> {
        if self.pending.is_some() {
            return Err(HistoryError::Busy);
        }
        if self.inflight_bytes != 0
            || self.samples != 0
            || self.reserved.iter().any(|word| *word != 0)
        {
            return Err(HistoryError::Full);
        }
        self.submitted.fill(0);
        self.acknowledged.fill(0);
        self.floor = 1;
        self.allocated = 0;
        self.highest_sent = 0;
        self.final_retired = false;
        Ok(())
    }

    fn contains(&self, number: u64) -> bool {
        !self.final_retired && number >= self.floor && number <= self.allocated
    }

    fn take_sample(&mut self, number: u64) -> Option<NonZeroU16> {
        let slot = self.entries[number as usize & (SENT_CAPACITY - 1)]
            .sample_slot
            .take();
        self.samples -= u32::from(slot.is_some());
        slot
    }

    fn metadata(&self, number: u64) -> SentPacket {
        let entry = self.entries[number as usize & (SENT_CAPACITY - 1)];
        SentPacket {
            number,
            sent_at: Duration::new(entry.sent_seconds, entry.sent_nanos),
            outer_bytes: entry.size_flags & SIZE_MASK,
            eliciting: entry.size_flags & ELICITING != 0,
            charged: entry.size_flags & CHARGED != 0,
            sample_slot: entry.sample_slot,
        }
    }
}

fn location(number: u64) -> (usize, u64) {
    ((number >> 6) as usize & (WORDS - 1), 1 << (number % 64))
}

fn words(start: u64, end: u64) -> impl Iterator<Item = (u64, u64)> {
    (start >> 6..=end >> 6).filter_map(move |word| {
        if start > end {
            return None;
        }
        let low = if word == start >> 6 {
            u64::MAX << (start % 64)
        } else {
            u64::MAX
        };
        let high = if word == end >> 6 {
            u64::MAX >> (63 - end % 64)
        } else {
            u64::MAX
        };
        Some((word, low & high))
    })
}
