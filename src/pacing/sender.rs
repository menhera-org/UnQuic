//! One worker's prepared transmissions, their sampling handles and their claim.
use super::{Published, Records};
use std::{num::NonZeroU16, time::Duration};

const BATCH: usize = 64;

/// A transmission's identity across session replacement and lane reuse.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PacketId {
    /// Configuration session index.
    pub session: usize,
    /// Installed connection generation, preserved until accounting drains.
    pub generation: u64,
    /// Transmit lane number.
    pub lane: u16,
    /// Nonzero packet number in that lane.
    pub number: u64,
}

/// One charged physical datagram prepared before claiming send credit.
#[derive(Clone, Copy, Debug, Default)]
pub struct Planned {
    /// Generation-qualified packet identity.
    pub id: PacketId,
    /// Outer IPv6 bytes, including IP and UDP headers.
    pub bytes: u16,
}

/// A rejected flight operation; no rejection changes the flight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlightError {
    /// Invalid packets, completion or renumbering.
    Invalid,
    /// A claim, live handles or exhausted records block the operation.
    Blocked,
}

impl std::fmt::Display for FlightError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Invalid => "invalid transmission flight operation",
            Self::Blocked => "transmission flight is blocked",
        })
    }
}

impl std::error::Error for FlightError {}

/// Inflight and pacing credit taken immediately before one syscall.
#[derive(Clone, Copy, Debug)]
pub struct Claim {
    /// Sending path group.
    pub group: usize,
    /// Group inflight before this claim's charge.
    pub prior: u64,
    /// Charged outer bytes of the whole prepared prefix.
    pub bytes: u64,
    /// Claim time; a successful send starts no earlier.
    pub at: Duration,
    /// Publication read for this claim's delivery snapshots.
    pub state: Published,
}

#[derive(Clone, Copy, Debug, Default)]
struct Record {
    number: u64,
    bytes: u16,
    slot: Option<NonZeroU16>,
}

/// A worker-owned plan of up to 64 datagrams from one lane.
///
/// Each prepared packet holds one handle from the worker's records. A completed
/// prefix's handles pass to sent history; `release` returns the rest.
#[derive(Debug)]
pub struct Flight {
    origin: PacketId,
    records: [Record; BATCH],
    count: usize,
    successful: usize,
    started: Option<Duration>,
    host_limited: bool,
    pub(crate) pmtu: bool,
    completed: bool,
    claim: Option<Claim>,
}

impl Default for Flight {
    fn default() -> Self {
        Self {
            origin: PacketId::default(),
            records: [Record::default(); BATCH],
            count: 0,
            successful: 0,
            started: None,
            host_limited: false,
            pmtu: false,
            completed: false,
            claim: None,
        }
    }
}

impl Flight {
    /// Credit claimed for this plan; absent until the worker may send it.
    pub const fn claim(&self) -> Option<Claim> {
        self.claim
    }

    /// Sampling handle to attach to a successfully submitted history entry.
    pub fn handle(&self, ordinal: usize) -> Option<NonZeroU16> {
        (ordinal < self.count).then_some(())?;
        self.records.get(ordinal)?.slot
    }

    /// Prepared packets, or the successful prefix once completed.
    pub fn packets(&self) -> impl ExactSizeIterator<Item = Planned> + '_ {
        let count = if self.completed {
            self.successful
        } else {
            self.count
        };
        self.records[..count].iter().map(|record| Planned {
            id: PacketId {
                number: record.number,
                ..self.origin
            },
            bytes: record.bytes,
        })
    }

    /// Successful count, pre-syscall time and host-limited flag, once completed.
    pub const fn completion(&self) -> Option<(usize, Option<Duration>, bool)> {
        if self.completed {
            Some((self.successful, self.started, self.host_limited))
        } else {
            None
        }
    }

    /// Replaces an empty flight with 1..=64 packets of one lane and generation
    /// in strictly increasing number order, taking one record handle each.
    /// Fewer free handles prepare a shorter prefix, returned; none is Blocked.
    pub fn prepare(
        &mut self,
        packets: &[Planned],
        records: &mut Records,
    ) -> Result<usize, FlightError> {
        if self.claim.is_some() || self.records[..self.count].iter().any(|r| r.slot.is_some()) {
            return Err(FlightError::Blocked);
        }
        let first = packets.first().ok_or(FlightError::Invalid)?;
        if packets.len() > BATCH {
            return Err(FlightError::Invalid);
        }
        let mut previous = 0;
        for packet in packets {
            if packet.id.number <= previous
                || !(100..=9000).contains(&packet.bytes)
                || packet.id.session != first.id.session
                || packet.id.generation != first.id.generation
                || packet.id.lane != first.id.lane
            {
                return Err(FlightError::Invalid);
            }
            previous = packet.id.number;
        }
        let count = packets.len().min(records.available());
        if count == 0 {
            return Err(FlightError::Blocked);
        }
        for (record, packet) in self.records.iter_mut().zip(&packets[..count]) {
            *record = Record {
                number: packet.id.number,
                bytes: packet.bytes,
                slot: records.take(),
            };
        }
        self.origin = first.id;
        self.origin.number = 0;
        self.count = count;
        self.successful = 0;
        self.started = None;
        self.host_limited = false;
        self.pmtu = false;
        self.completed = false;
        Ok(count)
    }

    /// Keeps the largest whole-packet prefix within budget outer bytes and
    /// returns the suffix's handles. Only an unclaimed plan can shrink.
    pub fn truncate(&mut self, budget: u64, records: &mut Records) -> Result<u64, FlightError> {
        if self.claim.is_some() || self.completed {
            return Err(FlightError::Blocked);
        }
        if self.count == 0 {
            return Err(FlightError::Invalid);
        }
        let mut bytes = 0;
        let mut count = 0;
        for record in &self.records[..self.count] {
            if bytes + u64::from(record.bytes) > budget {
                break;
            }
            bytes += u64::from(record.bytes);
            count += 1;
        }
        if count == 0 {
            return Err(FlightError::Blocked);
        }
        for record in &mut self.records[count..self.count] {
            if let Some(slot) = record.slot.take() {
                records.release(slot);
            }
        }
        self.count = count;
        Ok(bytes)
    }

    /// Attaches the credit claimed for exactly this plan's bytes.
    pub fn set_claim(&mut self, claim: Claim) -> Result<(), FlightError> {
        let bytes: u64 = self.packets().map(|p| u64::from(p.bytes)).sum();
        if self.claim.is_some() || self.completed || self.count == 0 || claim.bytes != bytes {
            return Err(FlightError::Invalid);
        }
        self.claim = Some(claim);
        Ok(())
    }

    /// Renumbers a claimed, unsent plan after local control used its numbers.
    /// Sizes, origin, handles and claim stay unchanged.
    pub fn reidentify(&mut self, first: u64) -> Result<(), FlightError> {
        if self.claim.is_none()
            || self.completed
            || self.count == 0
            || first <= self.records[self.count - 1].number
            || first.checked_add(self.count as u64 - 1).is_none()
        {
            return Err(FlightError::Invalid);
        }
        for (offset, record) in self.records[..self.count].iter_mut().enumerate() {
            record.number = first + offset as u64;
        }
        Ok(())
    }

    /// Records one syscall's successful prefix and its pre-syscall timestamp.
    /// A zero-success completion has no timestamp.
    pub fn complete(
        &mut self,
        count: usize,
        started: Option<Duration>,
        host_limited: bool,
    ) -> Result<(), FlightError> {
        let claim = self.claim.ok_or(FlightError::Invalid)?;
        if self.completed
            || count > self.count
            || match started {
                Some(at) => count == 0 || at < claim.at,
                None => count != 0,
            }
        {
            return Err(FlightError::Invalid);
        }
        self.successful = count;
        self.started = started;
        self.host_limited = host_limited;
        self.completed = true;
        Ok(())
    }

    /// Ends the plan: handles of a completed prefix now belong to history; every
    /// other handle returns to records. The flight is then empty.
    pub fn release(&mut self, records: &mut Records) {
        let kept = if self.completed { self.successful } else { 0 };
        for (ordinal, record) in self.records[..self.count].iter_mut().enumerate() {
            if let Some(slot) = record.slot.take()
                && ordinal >= kept
            {
                records.release(slot);
            }
        }
        self.count = 0;
        self.successful = 0;
        self.started = None;
        self.completed = false;
        self.claim = None;
    }
}

#[cfg(test)]
mod tests;
