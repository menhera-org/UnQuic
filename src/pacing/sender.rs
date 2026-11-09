//! Ordered send accounting and shared delivery metadata for the control owner.
use super::{Grant, GrantError, Grants, Limits};
use crate::bbr::{Delivery, DeliveryError, DeliveryPacket, LossSample, RateSample};
use std::{cmp::Reverse, io, mem::size_of, num::NonZeroU16, time::Duration};

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

/// One charged physical datagram prepared before requesting permission to send.
#[derive(Clone, Copy, Debug, Default)]
pub struct Planned {
    /// Generation-qualified packet identity.
    pub id: PacketId,
    /// Outer IPv6 bytes, including IP and UDP headers.
    pub bytes: u16,
}

#[derive(Clone, Copy, Debug, Default)]
struct Record {
    number: u64,
    bytes: u16,
    slot: Option<NonZeroU16>,
}

/// A reusable worker-owned request, permission and completion buffer.
///
/// Startup creates exactly one per worker. Move its box through the handoff
/// queues; retain it on queue backpressure. Return every granted flight, even
/// when no syscall succeeds. A completion retains its handles until ordered
/// sampling finishes; outgoing ACK buffers cannot replace this storage.
#[derive(Debug)]
pub struct Flight {
    worker: usize,
    group: Option<usize>,
    grant: Option<Grant>,
    origin: PacketId,
    records: [Record; BATCH],
    count: usize,
    successful: usize,
    started: Option<Duration>,
    host_limited: bool,
    pub(crate) pmtu: bool,
    completed: bool,
    cursor: usize,
}

impl Flight {
    /// Worker to which the buffer must return.
    pub const fn worker(&self) -> usize {
        self.worker
    }

    /// Issued permission; absent before reservation and after ordered commit.
    pub const fn grant(&self) -> Option<Grant> {
        self.grant
    }

    /// Issuing path group; absent while the worker prepares a new request.
    pub const fn group(&self) -> Option<usize> {
        self.group
    }

    /// Sampling handle to attach to a successfully submitted history entry.
    pub fn handle(&self, ordinal: usize) -> Option<NonZeroU16> {
        (ordinal < self.count).then_some(())?;
        self.records.get(ordinal)?.slot
    }

    /// Prepared packets, or the successful prefix after the owner returns this flight.
    /// Completion metadata remains readable until prepare replaces the request.
    pub fn packets(&self) -> impl ExactSizeIterator<Item = Planned> + '_ {
        let count = if self.completed && self.grant.is_none() {
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
    /// Zero-success completions have no timestamp.
    pub const fn completion(&self) -> Option<(usize, Option<Duration>, bool)> {
        if self.completed {
            Some((self.successful, self.started, self.host_limited))
        } else {
            None
        }
    }

    /// Keeps the largest whole-packet prefix fitting an outer-byte budget.
    /// Only ungranted prepared requests may be trimmed. Zero-fitting budgets
    /// return Blocked without changing the request; empty requests are Invalid.
    pub fn truncate(&mut self, budget: u32) -> Result<usize, GrantError> {
        if self.grant.is_some() {
            return Err(GrantError::Blocked);
        }
        if self.count == 0 {
            return Err(GrantError::Invalid);
        }
        let mut bytes = 0_u32;
        let count = self.records[..self.count]
            .iter()
            .take_while(|record| {
                bytes += u32::from(record.bytes);
                bytes <= budget
            })
            .count();
        if count == 0 {
            return Err(GrantError::Blocked);
        }
        self.count = count;
        Ok(count)
    }

    /// Replaces an ungranted request without allocating.
    ///
    /// One request contains 1..=64 charged packets from one lane/generation in
    /// strictly increasing packet-number order. Failed validation changes nothing.
    pub fn prepare(&mut self, packets: &[Planned]) -> Result<(), GrantError> {
        if self.grant.is_some() {
            return Err(GrantError::Blocked);
        }
        let first = packets.first().ok_or(GrantError::Invalid)?;
        if packets.len() > BATCH {
            return Err(GrantError::Invalid);
        }
        let mut previous = 0;
        for packet in packets {
            if packet.id.number <= previous
                || !(100..=9000).contains(&packet.bytes)
                || packet.id.session != first.id.session
                || packet.id.generation != first.id.generation
                || packet.id.lane != first.id.lane
            {
                return Err(GrantError::Invalid);
            }
            previous = packet.id.number;
        }
        for (record, packet) in self.records.iter_mut().zip(packets) {
            *record = Record {
                number: packet.id.number,
                bytes: packet.bytes,
                slot: None,
            };
        }
        self.origin = first.id;
        self.origin.number = 0;
        self.count = packets.len();
        self.successful = 0;
        self.started = None;
        self.pmtu = false;
        self.completed = false;
        self.cursor = 0;
        Ok(())
    }

    /// Records one syscall's successful prefix, using its pre-syscall timestamp.
    ///
    /// A zero-success completion requires no timestamp. A grant authorizes one
    /// batch attempt (including a failed GSO attempt and its checked fallback).
    /// After this call, hand the whole box back to the accounting owner.
    pub fn complete(
        &mut self,
        count: usize,
        started: Option<Duration>,
        host_limited: bool,
    ) -> Result<(), GrantError> {
        let grant = self.grant.ok_or(GrantError::Stale)?;
        if self.completed {
            return Err(GrantError::Stale);
        }
        if count > self.count
            || (grant.is_probe() && count != 0 && count != 1)
            || match started {
                Some(at) => count == 0 || !grant.valid_at(at),
                None => count != 0,
            }
        {
            return Err(GrantError::Invalid);
        }
        self.successful = count;
        self.started = started;
        self.host_limited = host_limited;
        self.completed = true;
        Ok(())
    }
}

enum SlotState {
    Free(Option<NonZeroU16>),
    Reserved,
    Sent {
        packet: DeliveryPacket,
        charged: bool,
    },
}

struct Slot {
    group: usize,
    id: PacketId,
    bytes: u16,
    state: SlotState,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Open,
    Closing,
    Commit,
    Feedback,
}

struct Group {
    grants: Grants,
    delivery: Delivery,
    // Boxes move through worker queues and return without copying packet records.
    #[allow(clippy::vec_box)]
    reports: Vec<Box<Flight>>,
    phase: Phase,
    sample_flight: u64,
    planned: usize,
    sent_bytes: u64,
    time: Duration,
    ack: bool,
}

/// Control-owner accounting for every sending path group in one daemon.
///
/// A shared slab retains at most 65,535 complete delivery snapshots. Each group
/// closes issuance on its first completion or explicit close, awaits all grant
/// reports, then commits runs in timestamp/grant order before applying feedback.
/// Before closure, outstanding reserved bytes also bound cumulative epoch bytes.
///
/// The caller supplies controller limits and consumes rate/loss samples in group
/// order. Runtime report routing, timers and controller activation are separate.
/// All steady-state methods are bounded and allocate nothing.
pub struct Sender {
    groups: Box<[Group]>,
    slots: Box<[Slot]>,
    free: Option<NonZeroU16>,
    available: usize,
    busy: u64,
    workers: usize,
}

impl Sender {
    /// Allocates the shared slab, group ledgers and one movable flight per worker.
    /// Charge storage_bytes against the daemon ceiling before calling this.
    #[allow(clippy::type_complexity)]
    pub fn new(
        groups: usize,
        workers: usize,
        slots: usize,
    ) -> io::Result<(Self, Box<[Box<Flight>]>)> {
        if Self::storage_bytes(groups, workers, slots).is_none() {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let mut state = Vec::new();
        state.try_reserve_exact(groups).map_err(io::Error::other)?;
        for _ in 0..groups {
            let mut reports = Vec::new();
            reports
                .try_reserve_exact(workers)
                .map_err(io::Error::other)?;
            state.push(Group {
                grants: Grants::new(workers)?,
                delivery: Delivery::new(),
                reports,
                phase: Phase::Open,
                sample_flight: 0,
                planned: 0,
                sent_bytes: 0,
                time: Duration::ZERO,
                ack: false,
            });
        }
        let mut slab = Vec::new();
        slab.try_reserve_exact(slots).map_err(io::Error::other)?;
        for index in 0..slots {
            slab.push(Slot {
                group: 0,
                id: PacketId::default(),
                bytes: 0,
                state: SlotState::Free(
                    (index + 1 < slots)
                        .then(|| NonZeroU16::new((index + 2) as u16))
                        .flatten(),
                ),
            });
        }
        let mut flights = Vec::new();
        flights
            .try_reserve_exact(workers)
            .map_err(io::Error::other)?;
        for worker in 0..workers {
            // Vec's fallible allocation avoids an infallible Box::new at startup.
            let mut one = Vec::new();
            one.try_reserve_exact(1).map_err(io::Error::other)?;
            one.push(Flight {
                worker,
                group: None,
                grant: None,
                origin: PacketId::default(),
                records: [Record::default(); BATCH],
                count: 0,
                successful: 0,
                started: None,
                host_limited: false,
                pmtu: false,
                completed: false,
                cursor: 0,
            });
            let array: Box<[Flight; 1]> = one
                .into_boxed_slice()
                .try_into()
                .map_err(|_| io::ErrorKind::InvalidData)?;
            // SAFETY: a one-element array has the same layout and alignment as Flight.
            flights.push(unsafe { Box::from_raw(Box::into_raw(array).cast::<Flight>()) });
        }
        Ok((
            Self {
                groups: state.into_boxed_slice(),
                slots: slab.into_boxed_slice(),
                free: NonZeroU16::new(1),
                available: slots,
                busy: 0,
                workers,
            },
            flights.into_boxed_slice(),
        ))
    }

    /// Fixed reserved bytes including the returned flights, excluding allocator overhead.
    pub fn storage_bytes(groups: usize, workers: usize, slots: usize) -> Option<usize> {
        if groups == 0 || !(1..=64).contains(&workers) || !(1..=65535).contains(&slots) {
            return None;
        }
        let group = size_of::<Group>()
            .checked_add(Grants::storage_bytes(workers)?.checked_sub(size_of::<Grants>())?)?
            .checked_add(workers.checked_mul(size_of::<Box<Flight>>())?)?;
        size_of::<Self>()
            .checked_add(groups.checked_mul(group)?)?
            .checked_add(slots.checked_mul(size_of::<Slot>())?)?
            .checked_add(workers.checked_mul(size_of::<Flight>() + size_of::<Box<Flight>>())?)
    }

    /// Unreserved sampling slots; expired grants still hold their slots.
    pub const fn available(&self) -> usize {
        self.available
    }

    /// Reserved and submitted-but-unsettled outer bytes for a group.
    pub fn flight_bytes(&self, group: usize) -> Option<(u64, u64)> {
        let group = self.groups.get(group)?;
        Some((group.grants.reserved(), group.grants.in_flight()))
    }

    /// Current sampler application-limited marker for the shared group.
    pub fn is_app_limited(&self, group: usize) -> Option<bool> {
        Some(self.groups.get(group)?.delivery.is_app_limited())
    }

    /// Issuance phase (0 open, 1 closing, 2 commit, 3 feedback) and next pacing time.
    #[cfg(feature = "trace")]
    pub fn trace_state(&self, group: usize) -> Option<(u64, Duration)> {
        let state = self.groups.get(group)?;
        Some((state.phase as u64, state.grants.next_send()))
    }

    /// Whether a committed transmission was already declared lost, for diagnostics only.
    #[cfg(feature = "trace")]
    pub fn trace_uncharged(&self, id: PacketId, handle: NonZeroU16) -> bool {
        self.slots
            .get(usize::from(handle.get()) - 1)
            .is_some_and(|slot| {
                slot.id == id && matches!(slot.state, SlotState::Sent { charged: false, .. })
            })
    }

    /// Pacing deadline, independent of report and sampler backpressure.
    pub fn deadline(&self, group: usize, now: Duration) -> Option<Duration> {
        self.groups.get(group)?.grants.deadline(now)
    }

    /// Sets aggregate PTO allowance without allocating a sampler slot.
    pub fn probe_allowance(&mut self, group: usize, count: u8) -> Result<(), GrantError> {
        self.groups
            .get_mut(group)
            .ok_or(GrantError::Invalid)?
            .grants
            .probe_allowance(count)
    }

    /// Reserves every packet's metadata and byte credit before permission leaves the owner.
    /// The worker must first reserve these unique packet numbers in its sent history.
    pub fn reserve(
        &mut self,
        group: usize,
        flight: &mut Flight,
        probe: bool,
        limits: Limits,
        now: Duration,
    ) -> Result<(), GrantError> {
        let state = self.groups.get_mut(group).ok_or(GrantError::Invalid)?;
        if now < state.time {
            return Err(GrantError::Invalid);
        }
        if flight.worker >= self.workers || flight.count == 0 || (probe && flight.count != 1) {
            return Err(GrantError::Invalid);
        }
        if state.phase != Phase::Open
            || flight.grant.is_some()
            || self.busy & (1_u64 << flight.worker) != 0
            || self.available < flight.count
        {
            return Err(GrantError::Blocked);
        }
        let records = flight
            .records
            .get_mut(..flight.count)
            .ok_or(GrantError::Invalid)?;
        // Check the complete free chain before changing grant ownership.
        let mut free = self.free;
        let mut bytes = 0_u32;
        for record in records.iter() {
            let handle = free.ok_or(GrantError::Blocked)?;
            let slot = self
                .slots
                .get(usize::from(handle.get()) - 1)
                .ok_or(GrantError::Invalid)?;
            let SlotState::Free(next) = slot.state else {
                return Err(GrantError::Invalid);
            };
            free = next;
            bytes += u32::from(record.bytes);
        }
        if (state.planned + flight.count) as u64 > state.delivery.remaining_packets()
            || state
                .sent_bytes
                .checked_add(state.grants.reserved())
                .and_then(|total| total.checked_add(u64::from(bytes)))
                .is_none()
        {
            return Err(GrantError::Exhausted);
        }
        let grant = state
            .grants
            .reserve(flight.worker, bytes, probe, limits, now)?;
        state.planned += flight.count;
        if state.grants.reserved() == u64::from(bytes) {
            state.sample_flight = state.grants.in_flight();
        }
        for record in records {
            let handle = self.free.ok_or(GrantError::Invalid)?;
            let slot = self
                .slots
                .get_mut(usize::from(handle.get()) - 1)
                .ok_or(GrantError::Invalid)?;
            let SlotState::Free(next) = slot.state else {
                return Err(GrantError::Invalid);
            };
            self.free = next;
            slot.group = group;
            slot.id = PacketId {
                number: record.number,
                ..flight.origin
            };
            slot.bytes = record.bytes;
            slot.state = SlotState::Reserved;
            record.slot = Some(handle);
        }
        state.time = now;
        self.available -= flight.count;
        self.busy |= 1_u64 << flight.worker;
        flight.group = Some(group);
        flight.grant = Some(grant);
        Ok(())
    }

    /// Replaces an unsent grant's packet numbers after local control overtakes it.
    /// Sizes, origin, handles and grant timing remain unchanged. Validate every
    /// reservation before changing either copy of the identities.
    pub(crate) fn reidentify(&mut self, flight: &mut Flight, first: u64) -> Result<(), GrantError> {
        let group = flight.group.ok_or(GrantError::Stale)?;
        if flight.grant.is_none()
            || flight.completed
            || flight.count == 0
            || first <= flight.records[flight.count - 1].number
            || first.checked_add(flight.count as u64 - 1).is_none()
        {
            return Err(GrantError::Invalid);
        }
        for record in &flight.records[..flight.count] {
            let handle = record.slot.ok_or(GrantError::Stale)?;
            let slot = self
                .slots
                .get(usize::from(handle.get()) - 1)
                .ok_or(GrantError::Stale)?;
            if slot.group != group
                || slot.id
                    != (PacketId {
                        number: record.number,
                        ..flight.origin
                    })
                || slot.bytes != record.bytes
                || !matches!(slot.state, SlotState::Reserved)
            {
                return Err(GrantError::Stale);
            }
        }
        for (offset, record) in flight.records[..flight.count].iter_mut().enumerate() {
            let handle = record.slot.ok_or(GrantError::Stale)?;
            record.number = first + offset as u64;
            self.slots[usize::from(handle.get()) - 1].id.number = record.number;
        }
        Ok(())
    }

    /// Reserves the largest fitting prefix; a rejected grant leaves the plan intact.
    pub(crate) fn reserve_prefix(
        &mut self,
        group: usize,
        flight: &mut Flight,
        budget: u32,
        probe: bool,
        limits: Limits,
        now: Duration,
    ) -> Result<(), GrantError> {
        let count = flight.count;
        flight.truncate(budget)?;
        let result = self.reserve(group, flight, probe, limits, now);
        if result.is_err() {
            flight.count = count;
        }
        result
    }

    /// Stops new grants, including for an empty group needing pressure retirement.
    pub fn close(&mut self, group: usize) -> Result<(), GrantError> {
        let state = self.groups.get_mut(group).ok_or(GrantError::Invalid)?;
        if state.phase == Phase::Open {
            state.phase = Phase::Closing;
        }
        Ok(())
    }

    /// Retains a whole completion; rejected buffers remain owned by the caller.
    pub fn reported(&mut self, flight: Box<Flight>) -> Result<(), (GrantError, Box<Flight>)> {
        match self.accept_report(&flight) {
            Ok(group) => {
                if let Some(state) = self.groups.get_mut(group) {
                    state.reports.push(flight);
                    return Ok(());
                }
                Err((GrantError::Invalid, flight))
            }
            Err(error) => Err((error, flight)),
        }
    }

    fn accept_report(&mut self, flight: &Flight) -> Result<usize, GrantError> {
        let group = flight.group.ok_or(GrantError::Stale)?;
        let grant = flight.grant.ok_or(GrantError::Stale)?;
        let state = self.groups.get_mut(group).ok_or(GrantError::Invalid)?;
        if !flight.completed || !matches!(state.phase, Phase::Open | Phase::Closing) {
            return Err(GrantError::Invalid);
        }
        if state.reports.len() >= self.workers {
            return Err(GrantError::Blocked);
        }
        let records = flight
            .records
            .get(..flight.count)
            .ok_or(GrantError::Invalid)?;
        let mut bytes = 0;
        for (ordinal, record) in records.iter().enumerate() {
            let handle = record.slot.ok_or(GrantError::Invalid)?;
            let slot = self
                .slots
                .get(usize::from(handle.get()) - 1)
                .ok_or(GrantError::Invalid)?;
            if slot.group != group
                || slot.id
                    != (PacketId {
                        number: record.number,
                        ..flight.origin
                    })
                || slot.bytes != record.bytes
                || !matches!(slot.state, SlotState::Reserved)
            {
                return Err(GrantError::Stale);
            }
            if ordinal < flight.successful {
                bytes += u32::from(slot.bytes);
            }
        }
        state.grants.reported(grant, bytes, flight.started)?;
        state.sent_bytes += u64::from(bytes);
        state.phase = Phase::Closing;
        for record in records.iter().skip(flight.successful) {
            Self::release(
                &mut self.slots,
                &mut self.free,
                &mut self.available,
                record.slot.ok_or(GrantError::Invalid)?,
            )?;
        }
        Ok(group)
    }

    /// Commits at most budget physical packets and returns at most one reusable flight.
    ///
    /// No send can commit until every issued grant reports. Ordering sorts at
    /// most 64 completion descriptors once; each returned flight's samples are
    /// installed before its worker can reuse the buffer. Poll feedback_ready
    /// after servicing returns. A zero budget makes no progress.
    pub fn advance(
        &mut self,
        group: usize,
        budget: usize,
    ) -> Result<Option<Box<Flight>>, GrantError> {
        let state = self.groups.get_mut(group).ok_or(GrantError::Invalid)?;
        if budget == 0 || state.grants.reserved() != 0 {
            return Ok(None);
        }
        if state.phase == Phase::Closing {
            state.reports.sort_unstable_by_key(|report| {
                Reverse((
                    report.started.unwrap_or(Duration::ZERO),
                    report.grant.map_or(0, |grant| grant.id),
                ))
            });
            state.phase = Phase::Commit;
        }
        if state.phase != Phase::Commit {
            return Ok(None);
        }
        let Some(report) = state.reports.last_mut() else {
            state.phase = Phase::Feedback;
            return Ok(None);
        };
        let stop = report.successful.min(report.cursor.saturating_add(budget));
        while report.cursor < stop {
            let record = report
                .records
                .get(report.cursor)
                .ok_or(GrantError::Invalid)?;
            let handle = record.slot.ok_or(GrantError::Invalid)?;
            let slot = self
                .slots
                .get_mut(usize::from(handle.get()) - 1)
                .ok_or(GrantError::Invalid)?;
            if !matches!(slot.state, SlotState::Reserved) {
                return Err(GrantError::Stale);
            }
            let mut packet = state
                .delivery
                .sent(
                    report.started.ok_or(GrantError::Invalid)?,
                    slot.bytes,
                    state.sample_flight,
                    report.host_limited,
                )
                .map_err(delivery_error)?;
            packet.pmtu = report.pmtu;
            state.time = state.time.max(report.started.ok_or(GrantError::Invalid)?);
            slot.state = SlotState::Sent {
                packet,
                charged: true,
            };
            state.sample_flight += u64::from(slot.bytes);
            report.cursor += 1;
        }
        if report.cursor != report.successful {
            return Ok(None);
        }
        let mut report = state.reports.pop().ok_or(GrantError::Invalid)?;
        self.busy &= !(1_u64 << report.worker);
        report.group = None;
        report.grant = None;
        report.count = 0;
        if state.reports.is_empty() {
            state.phase = Phase::Feedback;
        }
        Ok(Some(report))
    }

    /// Whether a bounded commit pass can continue without another worker report.
    pub(crate) fn commit_pending(&self, group: usize) -> bool {
        self.groups.get(group).is_some_and(|state| {
            state.grants.reserved() == 0 && matches!(state.phase, Phase::Closing | Phase::Commit)
        })
    }

    /// Whether all epoch sends have committed and feedback may be consumed.
    pub fn feedback_ready(&self, group: usize) -> bool {
        self.groups
            .get(group)
            .is_some_and(|state| state.phase == Phase::Feedback)
    }

    /// Starts a new issuance epoch after the caller's bounded feedback pass.
    pub fn reopen(&mut self, group: usize) -> Result<(), GrantError> {
        let state = self.feedback(group, false)?;
        state.phase = Phase::Open;
        state.planned = 0;
        Ok(())
    }

    /// Starts one ACK transaction with fresh owner observation time.
    pub fn begin_ack(
        &mut self,
        group: usize,
        now: Duration,
        host_limited: bool,
    ) -> Result<(), GrantError> {
        let state = self.feedback(group, false)?;
        if now < state.time {
            return Err(GrantError::Invalid);
        }
        state
            .delivery
            .begin_ack(now, host_limited)
            .map_err(delivery_error)?;
        state.time = now;
        state.ack = true;
        Ok(())
    }

    /// Consumes a history-validated ACK handle and releases its charge at most once.
    pub fn acknowledged(
        &mut self,
        group: usize,
        id: PacketId,
        handle: NonZeroU16,
    ) -> Result<(), GrantError> {
        let state = self.feedback(group, true)?;
        if !state.ack {
            return Err(GrantError::Invalid);
        }
        let state = self.groups.get_mut(group).ok_or(GrantError::Invalid)?;
        let slot = Self::sample(&mut self.slots, group, id, handle)?;
        let SlotState::Sent { packet, charged } = &mut slot.state else {
            return Err(GrantError::Stale);
        };
        state
            .delivery
            .acknowledged(packet)
            .map_err(delivery_error)?;
        if *charged {
            state.grants.settled(u64::from(slot.bytes))?;
        }
        Self::release(&mut self.slots, &mut self.free, &mut self.available, handle)
    }

    /// Finishes one ACK sample after all its bounded chunks, preserving late-loss delivery.
    pub fn finish_ack(
        &mut self,
        group: usize,
        minimum_rtt: Duration,
    ) -> Result<Option<RateSample>, GrantError> {
        let state = self.feedback(group, true)?;
        if !state.ack {
            return Err(GrantError::Invalid);
        }
        let sample = state
            .delivery
            .finish_ack(minimum_rtt)
            .map_err(delivery_error)?;
        state.ack = false;
        Ok(sample)
    }

    /// Settles one physical loss while retaining the snapshot for a late ACK.
    pub fn lost(
        &mut self,
        group: usize,
        id: PacketId,
        handle: NonZeroU16,
    ) -> Result<Option<LossSample>, GrantError> {
        self.feedback(group, false)?;
        let state = self.groups.get_mut(group).ok_or(GrantError::Invalid)?;
        let slot = Self::sample(&mut self.slots, group, id, handle)?;
        let SlotState::Sent { packet, charged } = &mut slot.state else {
            return Err(GrantError::Stale);
        };
        let loss = state
            .delivery
            .declared_lost(packet)
            .map_err(delivery_error)?;
        if loss.is_some() && *charged {
            state.grants.settled(u64::from(slot.bytes))?;
            *charged = false;
        }
        Ok(loss)
    }

    /// Reclaims an explicitly retired resolved history entry, without declaring loss.
    pub fn retired(
        &mut self,
        group: usize,
        id: PacketId,
        handle: NonZeroU16,
    ) -> Result<(), GrantError> {
        self.feedback(group, false)?;
        let slot = Self::sample(&mut self.slots, group, id, handle)?;
        if !matches!(slot.state, SlotState::Sent { charged: false, .. }) {
            return Err(GrantError::Blocked);
        }
        Self::release(&mut self.slots, &mut self.free, &mut self.available, handle)
    }

    /// Marks a verified application-limited bubble; host backpressure is not sufficient.
    pub fn application_limited(&mut self, group: usize) -> Result<(), GrantError> {
        let state = self.feedback(group, false)?;
        state
            .delivery
            .application_limited(state.grants.in_flight())
            .map_err(delivery_error)
    }

    fn feedback(&mut self, group: usize, allow_ack: bool) -> Result<&mut Group, GrantError> {
        let state = self.groups.get_mut(group).ok_or(GrantError::Invalid)?;
        if state.phase != Phase::Feedback || (state.ack && !allow_ack) {
            return Err(GrantError::Blocked);
        }
        Ok(state)
    }

    fn sample(
        slots: &mut [Slot],
        group: usize,
        id: PacketId,
        handle: NonZeroU16,
    ) -> Result<&mut Slot, GrantError> {
        let slot = slots
            .get_mut(usize::from(handle.get()) - 1)
            .ok_or(GrantError::Stale)?;
        if slot.group != group || slot.id != id {
            return Err(GrantError::Stale);
        }
        Ok(slot)
    }

    fn release(
        slots: &mut [Slot],
        free: &mut Option<NonZeroU16>,
        available: &mut usize,
        handle: NonZeroU16,
    ) -> Result<(), GrantError> {
        let slot = slots
            .get_mut(usize::from(handle.get()) - 1)
            .ok_or(GrantError::Invalid)?;
        if matches!(slot.state, SlotState::Free(_)) {
            return Err(GrantError::Stale);
        }
        slot.state = SlotState::Free(*free);
        *free = Some(handle);
        *available += 1;
        Ok(())
    }
}

fn delivery_error(error: DeliveryError) -> GrantError {
    match error {
        DeliveryError::Invalid => GrantError::Invalid,
        DeliveryError::Busy => GrantError::Blocked,
        DeliveryError::Exhausted => GrantError::Exhausted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_commit_requires_another_turn_but_unreported_grants_do_not() {
        let (mut sender, flights) = Sender::new(1, 1, 2).unwrap();
        let mut flight = flights.into_vec().pop().unwrap();
        let packets = [1, 2].map(|number| Planned {
            id: PacketId {
                number,
                ..PacketId::default()
            },
            bytes: 100,
        });
        flight.prepare(&packets).unwrap();
        sender
            .reserve(
                0,
                &mut flight,
                false,
                Limits {
                    window: 1280,
                    rate: 1_000_000,
                    quantum: 1280,
                    max_datagram: 1280,
                },
                Duration::ZERO,
            )
            .unwrap();
        sender.close(0).unwrap();
        assert!(!sender.commit_pending(0));
        flight.complete(2, Some(Duration::ZERO), false).unwrap();
        sender.reported(flight).unwrap();
        assert!(sender.commit_pending(0));
        assert!(sender.advance(0, 1).unwrap().is_none());
        assert!(sender.commit_pending(0));
        assert!(!sender.feedback_ready(0));
        assert!(sender.advance(0, 1).unwrap().is_some());
        assert!(!sender.commit_pending(0));
        assert!(sender.feedback_ready(0));
        assert!(!sender.commit_pending(1));
    }

    #[test]
    fn identity_and_lifetime_byte_exhaustion_precede_permission_to_send() {
        for packet_limit in [true, false] {
            let (mut sender, flights) = Sender::new(1, 2, 3).unwrap();
            let mut flights = flights.into_vec();
            let mut b = flights.pop().unwrap();
            let mut a = flights.pop().unwrap();
            let planned = Planned {
                id: PacketId {
                    number: 1,
                    ..PacketId::default()
                },
                bytes: 100,
            };
            a.prepare(&[planned]).unwrap();
            b.prepare(&[Planned {
                id: PacketId {
                    lane: 1,
                    ..planned.id
                },
                ..planned
            }])
            .unwrap();
            if packet_limit {
                sender.groups[0].delivery.limit_packets_for_test(1);
            } else {
                sender.groups[0].sent_bytes = u64::MAX - 100;
            }
            let limits = Limits {
                window: u64::MAX,
                rate: 1_000_000_000,
                quantum: 1280,
                max_datagram: 1280,
            };
            sender
                .reserve(0, &mut a, false, limits, Duration::ZERO)
                .unwrap();
            assert_eq!(
                sender.reserve(0, &mut b, false, limits, Duration::from_micros(1)),
                Err(GrantError::Exhausted)
            );
            assert!(b.grant().is_none());
            assert_eq!(sender.available(), 2);
            let handle = a.handle(0).unwrap();
            a.complete(1, Some(Duration::ZERO), false).unwrap();
            sender.reported(a).unwrap();
            let _a = sender.advance(0, 1).unwrap().unwrap();
            sender
                .begin_ack(0, Duration::from_micros(2), false)
                .unwrap();
            sender.acknowledged(0, planned.id, handle).unwrap();
            assert_eq!(
                sender
                    .finish_ack(0, Duration::ZERO)
                    .unwrap()
                    .unwrap()
                    .newly_acked,
                100
            );
            sender.reopen(0).unwrap();
            assert_eq!(
                sender.reserve(0, &mut b, false, limits, Duration::from_micros(3)),
                Err(GrantError::Exhausted)
            );
            assert_eq!(sender.flight_bytes(0), Some((0, 0)));
            assert_eq!(sender.available(), 3);
        }
    }
}

#[cfg(test)]
mod reidentify_tests {
    use super::*;

    #[test]
    fn probe_provenance_survives_partial_completion_but_not_flight_reuse() {
        let (mut sender, flights) = Sender::new(1, 1, 4).unwrap();
        let mut flight = flights.into_vec().pop().unwrap();
        let limits = Limits {
            window: 100_000,
            rate: 1_000_000_000,
            quantum: 100_000,
            max_datagram: 1280,
        };
        for (round, (pmtu, count)) in [(true, 0), (true, 1), (false, 2), (true, 2), (false, 1)]
            .into_iter()
            .enumerate()
        {
            let at = Duration::from_millis(round as u64 * 50);
            let packets = [1, 2].map(|number| Planned {
                id: PacketId {
                    session: 0,
                    generation: 1,
                    lane: 1,
                    number: round as u64 * 2 + number,
                },
                bytes: 1280,
            });
            flight.prepare(&packets).unwrap();
            assert!(!flight.pmtu);
            flight.pmtu = pmtu;
            sender.reserve(0, &mut flight, false, limits, at).unwrap();
            let handles = [flight.handle(0).unwrap(), flight.handle(1).unwrap()];
            flight
                .complete(count, (count != 0).then_some(at), false)
                .unwrap();
            sender.reported(flight).unwrap();
            flight = sender.advance(0, 64).unwrap().unwrap();
            sender
                .begin_ack(0, at + Duration::from_millis(1), false)
                .unwrap();
            for index in 0..count {
                sender
                    .acknowledged(0, packets[index].id, handles[index])
                    .unwrap();
            }
            let sample = sender.finish_ack(0, Duration::ZERO).unwrap();
            if count == 0 {
                assert!(sample.is_none());
            } else {
                let sample = sample.unwrap();
                assert_eq!(sample.pmtu_only, pmtu);
                assert_eq!(sample.newly_acked, count as u64 * 1280);
                assert!(!sample.app_limited);
            }
            assert_eq!(sender.flight_bytes(0), Some((0, 0)));
            assert_eq!(sender.available(), 4);
            sender.reopen(0).unwrap();
        }
    }

    #[test]
    fn refresh_preserves_grant_handles_and_exact_delivery_identity() {
        let (mut sender, flights) = Sender::new(1, 1, 4).unwrap();
        let mut flight = flights.into_vec().pop().unwrap();
        flight
            .prepare(&[1, 2, 3].map(|number| Planned {
                id: PacketId {
                    session: 4,
                    generation: 5,
                    lane: 6,
                    number,
                },
                bytes: 1280,
            }))
            .unwrap();
        sender
            .reserve_prefix(
                0,
                &mut flight,
                2560,
                false,
                Limits {
                    window: 4096,
                    rate: 1_000_000,
                    quantum: 4096,
                    max_datagram: 1280,
                },
                Duration::ZERO,
            )
            .unwrap();
        let grant = flight.grant().unwrap();
        let handles = [flight.handle(0).unwrap(), flight.handle(1).unwrap()];
        let reserved = sender.flight_bytes(0);
        let due = sender.deadline(0, Duration::ZERO);
        assert_eq!(
            sender.reidentify(&mut flight, u64::MAX),
            Err(GrantError::Invalid)
        );
        assert_eq!(sender.reidentify(&mut flight, 2), Err(GrantError::Invalid));
        // An invalid later slot must not mutate the earlier valid slot.
        let second = usize::from(handles[1].get()) - 1;
        sender.slots[second].bytes = 1281;
        assert_eq!(sender.reidentify(&mut flight, 20), Err(GrantError::Stale));
        assert_eq!(flight.packets().next().unwrap().id.number, 1);
        assert_eq!(sender.slots[usize::from(handles[0].get()) - 1].id.number, 1);
        sender.slots[second].bytes = 1280;
        sender.close(0).unwrap(); // Another completion may already have closed the epoch.
        sender.reidentify(&mut flight, 20).unwrap();
        assert_eq!(flight.grant().unwrap().id, grant.id);
        assert_eq!(sender.flight_bytes(0), reserved);
        assert_eq!(sender.deadline(0, Duration::ZERO), due);
        assert_eq!(
            [flight.handle(0).unwrap(), flight.handle(1).unwrap()],
            handles
        );
        let id = flight.packets().next().unwrap().id;
        flight.complete(1, Some(Duration::ZERO), false).unwrap();
        assert_eq!(sender.reidentify(&mut flight, 30), Err(GrantError::Invalid));
        sender.reported(flight).unwrap();
        let _flight = sender.advance(0, 64).unwrap().unwrap();
        sender
            .begin_ack(0, Duration::from_millis(1), false)
            .unwrap();
        assert!(
            sender
                .acknowledged(0, PacketId { number: 1, ..id }, handles[0])
                .is_err()
        );
        sender.acknowledged(0, id, handles[0]).unwrap();
        assert_eq!(
            sender
                .finish_ack(0, Duration::ZERO)
                .unwrap()
                .unwrap()
                .newly_acked,
            1280
        );
        assert_eq!(sender.available(), 4);
    }
}
