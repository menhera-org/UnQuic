//! One connection/lane's fixed sent-packet history; payload storage is separate.

use crate::wire::AckRanges;
use std::{collections::TryReserveError, time::Duration};

/// Maximum retained packet-number span per lane.
pub const SENT_CAPACITY: usize = 65_536;
const WORDS: usize = SENT_CAPACITY / 64;

#[derive(Clone, Copy, Default)]
struct Entry {
    sent_at: Duration,
    outer_bytes: u16,
    eliciting: bool,
    charged: bool,
}

/// A successfully submitted physical transmission, independent of payload lifetime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SentPacket {
    /// Packet number in this history's connection and lane.
    pub number: u64,
    /// Actual successful socket-submission time.
    pub sent_at: Duration,
    /// IPv6 packet bytes, including the IPv6 and UDP headers.
    pub outer_bytes: u16,
    /// Whether this datagram elicits an ACK.
    pub eliciting: bool,
    /// Whether it consumes ordinary congestion-window credit.
    pub charged: bool,
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
    /// Advancing history would forget a reservation or unresolved congestion charge.
    Full,
    /// An ACK names a number above the highest successful submission.
    Future,
    /// An ACK covers a retained number that was never successfully submitted.
    Unsent,
}

impl std::fmt::Display for HistoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Invalid => "invalid or reused packet reservation",
            Self::Full => "sent history still has unresolved reservations or charges",
            Self::Future => "ACK exceeds the highest submitted packet number",
            Self::Unsent => "ACK covers an unsent packet number",
        })
    }
}

impl std::error::Error for HistoryError {}

/// Bounded sender history with allocation-free reservation, ACK and loss operations.
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
        })
    }

    /// Reserved storage, including inline bitmaps and excluding allocator overhead.
    pub const fn storage_bytes() -> usize {
        size_of::<Self>() + SENT_CAPACITY * size_of::<Entry>()
    }

    /// Numbers below this bound are permanently outside validation history.
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
            sent_at: Duration::ZERO,
            outer_bytes,
            eliciting,
            charged,
        };
        self.reserved[index / 64] |= 1 << (index % 64);
        Ok(())
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
        entry.sent_at = now;
        self.submitted[word] |= bit;
        if entry.charged {
            self.inflight[word] |= bit;
            self.inflight_bytes += u64::from(entry.outer_bytes);
        }
        self.highest_sent = self.highest_sent.max(number);
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
            for (word, mask) in words(range.start.max(self.floor), range.end) {
                if self.submitted[word as usize & (WORDS - 1)] & mask != mask {
                    return Err(HistoryError::Unsent);
                }
            }
        }
        Ok(())
    }

    /// Validates all ranges before changing state, then visits each new ACK once.
    ///
    /// Old portions are ignored. Callback metadata never owns frame payloads.
    /// A late ACK after loss does not release congestion credit a second time.
    pub fn acknowledge(
        &mut self,
        ranges: AckRanges<'_>,
        mut accepted: impl FnMut(Acknowledged),
    ) -> Result<AckProgress, HistoryError> {
        self.validate(ranges)?;
        let largest = ranges.iter().next().map_or(0, |range| range.end);
        let mut progress = AckProgress::default();
        for range in ranges.iter() {
            for (word, mask) in words(range.start.max(self.floor), range.end) {
                let index = word as usize & (WORDS - 1);
                let mut new = mask & !self.acknowledged[index];
                while new != 0 {
                    let bit = 1_u64 << new.trailing_zeros();
                    let number = (word << 6) | u64::from(new.trailing_zeros());
                    new &= !bit;
                    let packet = self.metadata(number);
                    let was_in_flight = self.inflight[index] & bit != 0;
                    self.inflight[index] &= !bit;
                    self.acknowledged[index] |= bit;
                    progress.packets += 1;
                    progress.eliciting |= packet.eliciting;
                    if was_in_flight {
                        progress.released_bytes += u64::from(packet.outer_bytes);
                        self.inflight_bytes -= u64::from(packet.outer_bytes);
                    }
                    if number == largest {
                        progress.rtt_sent_at = Some(packet.sent_at);
                    }
                    accepted(Acknowledged {
                        packet,
                        was_in_flight,
                    });
                }
            }
        }
        if !progress.eliciting {
            progress.rtt_sent_at = None;
        }
        Ok(progress)
    }

    /// Declares one outstanding charged packet lost and releases its charge once.
    ///
    /// Retains submission identity for late ACKs. Payload/repair policy is separate.
    pub fn lose(&mut self, number: u64) -> Option<SentPacket> {
        if !self.contains(number) {
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

    /// Visits outstanding charged packets in number order without allocation.
    pub fn outstanding(&self) -> impl Iterator<Item = SentPacket> + '_ {
        let end = if self.inflight_bytes == 0 {
            0
        } else {
            self.allocated
        };
        words(self.floor, end).flat_map(move |(word, mask)| {
            let mut bits = self.inflight[word as usize & (WORDS - 1)] & mask;
            std::iter::from_fn(move || {
                if bits == 0 {
                    return None;
                }
                let bit = bits.trailing_zeros();
                bits &= bits - 1;
                Some(self.metadata((word << 6) | u64::from(bit)))
            })
        })
    }

    /// Reuses storage only after every charge and reservation has been settled.
    pub fn reset(&mut self) -> Result<(), HistoryError> {
        if self.inflight_bytes != 0 || self.reserved.iter().any(|word| *word != 0) {
            return Err(HistoryError::Full);
        }
        self.submitted.fill(0);
        self.acknowledged.fill(0);
        self.floor = 1;
        self.allocated = 0;
        self.highest_sent = 0;
        Ok(())
    }

    fn contains(&self, number: u64) -> bool {
        number >= self.floor && number <= self.allocated
    }

    fn metadata(&self, number: u64) -> SentPacket {
        let entry = self.entries[number as usize & (SENT_CAPACITY - 1)];
        SentPacket {
            number,
            sent_at: entry.sent_at,
            outer_bytes: entry.outer_bytes,
            eliciting: entry.eliciting,
            charged: entry.charged,
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
