//! TX-owner histories and reserved cross-worker sender messages.
use super::*;
use crate::{
    ack::{AckProgress, History, OutstandingScan},
    loss::Rtt,
    pacing::{Budget, Claim, Flight, PacketId, Records, SendRecord},
    wire::{AckRange, AckRanges, MAX_ACK_RANGES},
};
use std::{num::NonZeroU16, sync::Arc};

mod history;
mod owner;
#[cfg(test)]
mod tests;
mod timers;
use owner::Owner;

const ACK_BUFFERS: usize = 4;
/// Send records per worker; their handles also key the worker's recovery journal.
pub(super) const RECORDS: usize = 65_535;
/// Retry interval while the shared window, not pacing, refuses a claim.
const WINDOW_RETRY: Duration = Duration::from_micros(50);
// Trace reasons for a refused claim, equal to trace::BLOCK_PACING and BLOCK_WINDOW.
const BLOCK_PACING: u64 = 2;
const BLOCK_WINDOW: u64 = 4;

/// Outcome of claiming send credit for the prepared job.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Claimed {
    /// Credit is attached; send now.
    Ready,
    /// Pacing, the window or message room refused; the plan waits for `retry`.
    Waiting,
    /// The job is gone: stale, expired, or a probe that cannot fit.
    Abandoned,
}
/// History numbers only probes may use, so a PTO probe can always expose an
/// unacknowledged tail. Control stops this far from the wall; DATA also leaves
/// room for one refused plan's allocated numbers.
pub(super) const PROBE_HEADROOM: u64 = BATCH_SIZE as u64;

pub(crate) struct Ack {
    origin: usize,
    session: usize,
    tag: u128,
    lane: u16,
    received: Duration,
    delay_us: u32,
    count: usize,
    ranges: [AckRange; MAX_ACK_RANGES],
}

// History owns the copied ranges once an ACK begins; only its timing must
// survive until the coordinator returns the final report.
pub(super) struct AckTiming {
    received: Duration,
    delay_us: u32,
}

impl Ack {
    fn new(origin: usize) -> Self {
        Self {
            origin,
            session: 0,
            tag: 0,
            lane: 0,
            received: Duration::ZERO,
            delay_us: 0,
            count: 0,
            ranges: [AckRange { start: 0, end: 0 }; MAX_ACK_RANGES],
        }
    }
}

#[derive(Clone, Copy, Default)]
pub(super) struct Entry {
    // Read by tests and traces; the record and handle carry everything else.
    #[cfg_attr(not(any(test, feature = "trace")), allow(dead_code))]
    pub(super) number: u64,
    pub(super) sample: Option<NonZeroU16>,
    settled: bool,
    // Copied from the worker's records before the report leaves it.
    pub(super) record: SendRecord,
}

/// One syscall's successful transmissions, for PTO bases and controller inputs.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Sent {
    group: usize,
    // Lane identity; the packet number is zero.
    id: PacketId,
    count: u32,
    at: Duration,
    mtu: u16,
    cwnd_limited: bool,
    // Charged from zero inflight: an idle restart.
    restart: bool,
}

#[derive(Clone, Copy)]
pub(super) enum ReportKind {
    Ack {
        received: Duration,
        delay_us: u32,
        first: bool,
        end: Option<AckProgress>,
    },
    Loss,
    Retire,
}

/// Independent of send completions: frozen feedback cannot consume their storage.
pub(crate) struct Report {
    origin: usize,
    group: usize,
    id: PacketId,
    pub(super) kind: ReportKind,
    pub(super) count: usize,
    pub(super) entries: [Entry; BATCH_SIZE],
}

impl Report {
    fn new(origin: usize) -> Self {
        Self {
            origin,
            group: 0,
            id: PacketId::default(),
            kind: ReportKind::Retire,
            count: 0,
            entries: [Entry::default(); BATCH_SIZE],
        }
    }
}

pub(crate) enum Message {
    Ack(Box<Ack>),
    AckReturn(Box<Ack>),
    Sent(Sent),
    Report(Box<Report>),
    ReportReturn(Box<Report>),
    Probe {
        group: usize,
        epoch: u64,
        session: usize,
        generation: u64,
        lane: u16,
        count: u8,
    },
    ProbeReturn {
        group: usize,
        epoch: u64,
        count: u8,
    },
    CheckLoss {
        group: usize,
        token: u64,
        before: Duration,
        force: bool,
    },
    LossChecked {
        group: usize,
        token: u64,
        worker: usize,
        next_due: Option<Duration>,
    },
    ProbeReplied {
        group: usize,
    },
}

pub(super) struct Lane {
    pub(super) session: usize,
    pub(super) lane: u16,
    pub(super) group: usize,
    pub(super) history: History,
    pub(super) generation: u64,
    pub(super) unsent_bytes: usize,
    pub(super) rtt: Rtt,
    pub(super) largest_ack: u64,
    pub(super) scan: Option<OutstandingScan>,
    pub(super) loss_due: Option<Duration>,
    pub(super) scan_due: Option<Duration>,
    pub(super) forced_before: Option<Duration>,
    pub(super) scan_cutoff: Option<Duration>,
    pub(super) ack: Option<AckTiming>,
    pub(super) ack_first: bool,
    pub(super) report_pending: bool,
    // Complete an older scan, if any, then one pass using the admitted ACK.
    pub(super) loss_passes: u8,
    pub(super) probes: [Option<(u64, u16)>; 2],
    // Retained loss handles pin more than half the history; retire them before DATA is refused.
    pub(super) retire_due: bool,
}

impl Lane {
    fn new(session: usize, lane: u16, group: usize) -> io::Result<Self> {
        Ok(Self {
            session,
            lane,
            group,
            history: History::new().map_err(io::Error::other)?,
            generation: 0,
            unsent_bytes: 0,
            rtt: Rtt::new(),
            largest_ack: 0,
            scan: None,
            loss_due: None,
            scan_due: None,
            forced_before: None,
            scan_cutoff: None,
            ack: None,
            ack_first: true,
            report_pending: false,
            loss_passes: 0,
            probes: [None; 2],
            retire_due: false,
        })
    }
}

#[derive(Clone, Copy)]
pub(super) enum JobKind {
    Data,
    Probe { length: usize, pmtu: bool },
}

#[derive(Clone, Copy)]
pub(super) struct Job {
    pub(super) session: usize,
    pub(super) generation: u64,
    pub(super) lane: usize,
    pub(super) path: usize,
    pub(super) group: usize,
    pub(super) first: u64,
    pub(super) count: usize,
    pub(super) deadline: Duration,
    pub(super) kind: JobKind,
    // Outer bytes of the lane's confirmed maximum datagram.
    pub(super) mtu: u16,
    // A PTO probe's round; it bypasses the window and goes stale with its round.
    pub(super) probe: Option<u64>,
}

#[derive(Clone, Copy)]
struct LossCheck {
    token: u64,
    before: Duration,
    force: bool,
    cursor: usize,
    started: Option<u64>,
    next_due: Option<Duration>,
}

pub(super) struct State {
    pub(super) owner: Option<Owner>,
    pub(super) fresh: Vec<bool>,
    reclaim: Option<usize>,
    pub(super) permitted: bool,
    pub(super) probes: Vec<Option<(u64, PacketId, u8)>>,
    losses: Vec<Option<LossCheck>>,
    loss_cursor: usize,
    checks_pending: usize,
    acks_pending: usize,
    loss_fences: usize,
    pub(super) offsets: Vec<usize>,
    pub(super) lanes: Vec<Lane>,
    pub(super) cursor: usize,
    pub(super) flight: Flight,
    pub(super) records: Records,
    pub(super) budgets: Arc<[Budget]>,
    // When a refused claim may succeed: the pacing time or a window retry.
    pub(super) retry: Option<Duration>,
    pub(super) job: Option<Job>,
    pub(super) report: Option<Box<Report>>,
    #[allow(clippy::vec_box)]
    pub(super) ack_free: Vec<Box<Ack>>,
    pub(super) incoming: VecDeque<Box<Ack>>,
    pub(super) outgoing: VecDeque<(usize, Message)>,
    outgoing_cursor: usize,
    outgoing_left: usize,
    outgoing_blocked: u64,
    liveness_cursor: usize,
}

impl State {
    pub(super) fn new(
        config: &crate::config::Validated,
        workers: usize,
        id: usize,
    ) -> io::Result<Self> {
        let mut offsets = Vec::new();
        offsets
            .try_reserve_exact(config.config().sessions.len() + 1)
            .map_err(io::Error::other)?;
        offsets.push(0);
        for session in 0..config.config().sessions.len() {
            let peer = config
                .peer_for_session(session)
                .ok_or(io::ErrorKind::InvalidInput)?;
            let count = (id..config.config().peers[peer].tx_lanes)
                .step_by(workers)
                .count();
            let next = offsets
                .last()
                .copied()
                .unwrap_or(0usize)
                .checked_add(count)
                .ok_or(io::ErrorKind::OutOfMemory)?;
            offsets.push(next);
        }
        let count = offsets.last().copied().unwrap_or(0);
        let mut lanes = Vec::new();
        lanes.try_reserve_exact(count).map_err(io::Error::other)?;
        for session in 0..config.config().sessions.len() {
            let peer = config
                .peer_for_session(session)
                .ok_or(io::ErrorKind::InvalidInput)?;
            let group = config
                .path_group_for_peer(peer)
                .ok_or(io::ErrorKind::InvalidInput)?;
            for lane in (id..config.config().peers[peer].tx_lanes).step_by(workers) {
                lanes.push(Lane::new(session, lane as u16 + 1, group)?);
            }
        }
        let mut ack_free = Vec::new();
        ack_free
            .try_reserve_exact(ACK_BUFFERS)
            .map_err(io::Error::other)?;
        for _ in 0..ACK_BUFFERS {
            ack_free.push(boxed(Ack::new(id))?);
        }
        let mut incoming = VecDeque::new();
        incoming
            .try_reserve_exact(workers * ACK_BUFFERS)
            .map_err(io::Error::other)?;
        let mut outgoing = VecDeque::new();
        outgoing
            .try_reserve_exact(workers * (ACK_BUFFERS + 4))
            .map_err(io::Error::other)?;
        let mut probes = Vec::new();
        probes
            .try_reserve_exact(config.path_groups())
            .map_err(io::Error::other)?;
        probes.resize(config.path_groups(), None);
        let mut losses = Vec::new();
        losses
            .try_reserve_exact(config.path_groups())
            .map_err(io::Error::other)?;
        losses.resize(config.path_groups(), None);
        Ok(Self {
            fresh: {
                let mut fresh = Vec::new();
                fresh
                    .try_reserve_exact(config.path_groups())
                    .map_err(io::Error::other)?;
                fresh.resize(config.path_groups(), false);
                fresh
            },
            reclaim: None,
            owner: None,
            permitted: false,
            probes,
            losses,
            loss_cursor: 0,
            checks_pending: 0,
            acks_pending: 0,
            loss_fences: 0,
            offsets,
            lanes,
            cursor: 0,
            flight: Flight::default(),
            records: Records::new(RECORDS)?,
            budgets: Arc::from(Vec::new()),
            retry: None,
            job: None,
            report: Some(boxed(Report::new(id))?),
            ack_free,
            incoming,
            outgoing,
            outgoing_cursor: 0,
            outgoing_left: 0,
            outgoing_blocked: 0,
            liveness_cursor: 0,
        })
    }

    fn lane_index(&self, session: usize, lane: usize, workers: usize, id: usize) -> Option<usize> {
        if lane % workers != id {
            return None;
        }
        let index = self.offsets.get(session)?.checked_add(lane / workers)?;
        (index < *self.offsets.get(session + 1)?).then_some(index)
    }

    pub(super) fn feedback_deadline(&self) -> Option<Duration> {
        // Report returns wake us. Active checks instead schedule their own bounded continuation.
        if self.report.is_none() || self.checks_pending != 0 {
            return None;
        }
        self.lanes
            .iter()
            .filter(|lane| self.acks_pending == 0 || lane.ack.is_some())
            .filter_map(Lane::deadline)
            .min()
    }

    pub(super) fn queue(&mut self, worker: usize, message: Message) -> io::Result<()> {
        if self.outgoing.len() == self.outgoing.capacity() {
            return Err(io::Error::other("sender message storage exhausted"));
        }
        self.outgoing.push_back((worker, message));
        Ok(())
    }
}

fn boxed<T>(value: T) -> io::Result<Box<T>> {
    let mut values = Vec::new();
    values.try_reserve_exact(1).map_err(io::Error::other)?;
    values.push(value);
    let array: Box<[T; 1]> = values
        .into_boxed_slice()
        .try_into()
        .map_err(|_| io::Error::other("invalid sender allocation"))?;
    // SAFETY: a one-element array has the same layout and alignment as T.
    Ok(unsafe { Box::from_raw(Box::into_raw(array).cast::<T>()) })
}

impl Worker {
    pub(super) fn incoming_ack(
        &mut self,
        session: usize,
        tag: u128,
        lane: u16,
        delay_us: u32,
        ranges: AckRanges<'_>,
        received: Duration,
    ) -> io::Result<()> {
        if lane == 0 || usize::from(lane) > self.sessions[session].tx.len() {
            return Ok(());
        }
        let Some(mut ack) = self.sender.ack_free.pop() else {
            trace_event!(self.id, ACK_DROP, 0, [session << 16 | usize::from(lane)]);
            self.counters.queue_drops = self.counters.queue_drops.wrapping_add(1);
            self.counters.ack_buffer_drops = self.counters.ack_buffer_drops.wrapping_add(1);
            self.sessions[session].totals.queue_drops =
                self.sessions[session].totals.queue_drops.wrapping_add(1);
            self.sessions[session].totals.ack_buffer_drops = self.sessions[session]
                .totals
                .ack_buffer_drops
                .wrapping_add(1);
            return Ok(());
        };
        ack.session = session;
        ack.tag = tag;
        ack.lane = lane;
        ack.delay_us = delay_us;
        ack.received = received;
        ack.count = ranges.len();
        for (slot, range) in ack.ranges.iter_mut().zip(ranges.iter()) {
            *slot = range;
        }
        let owner = (usize::from(lane) - 1) % self.shared.workers();
        if owner == self.id {
            self.queue_sender_ack(ack)?;
        } else {
            self.sender.queue(owner, Message::Ack(ack))?;
        }
        Ok(())
    }

    pub(super) fn return_ack(&mut self, ack: Box<Ack>) -> io::Result<()> {
        if ack.origin == self.id {
            self.sender.ack_free.push(ack);
        } else {
            self.sender.queue(ack.origin, Message::AckReturn(ack))?;
        }
        Ok(())
    }

    fn queue_sender_ack(&mut self, ack: Box<Ack>) -> io::Result<()> {
        let index = ack.lane.checked_sub(1).and_then(|lane| {
            self.sender.lane_index(
                ack.session,
                usize::from(lane),
                self.shared.workers(),
                self.id,
            )
        });
        let valid = self.matches(ack.session, ack.tag)
            && index.is_some_and(|index| {
                ack.ranges
                    .get(..ack.count)
                    .and_then(|ranges| AckRanges::new(ranges).ok())
                    .is_some_and(|ranges| self.sender.lanes[index].history.validate(ranges).is_ok())
            });
        if !valid {
            #[cfg(feature = "trace")]
            {
                let history = index.map(|index| &self.sender.lanes[index].history);
                let reason = match (
                    self.matches(ack.session, ack.tag),
                    history.zip(
                        ack.ranges
                            .get(..ack.count)
                            .and_then(|ranges| AckRanges::new(ranges).ok()),
                    ),
                ) {
                    (true, Some((history, ranges))) => match history.validate(ranges) {
                        Err(crate::ack::HistoryError::Future) => 2,
                        Err(crate::ack::HistoryError::Unsent) => 3,
                        _ => 4,
                    },
                    _ => 1,
                };
                trace_event!(
                    self.id,
                    ACK_REJECT,
                    reason,
                    [
                        ack.session << 16 | usize::from(ack.lane),
                        ack.ranges.first().map_or(0, |range| range.end),
                        ack.ranges[..ack.count]
                            .last()
                            .map_or(0, |range| range.start),
                        ack.count,
                        history.map_or(0, |history| history.highest_sent()),
                        history.map_or(0, |history| history.floor())
                    ]
                );
            }
            return self.return_ack(ack);
        }
        if self.sender.incoming.len() == self.sender.incoming.capacity() {
            return Err(io::Error::other("duplicate incoming ACK buffer"));
        }
        self.sender.incoming.push_back(ack);
        Ok(())
    }

    pub(super) fn begin_sender_ack(&mut self) -> io::Result<()> {
        // A finite loss pass cannot wait behind an endless stream of duplicate ACKs.
        if self.sender.checks_pending != 0 || self.sender.loss_fences != 0 {
            return Ok(());
        }
        for _ in 0..self.sender.incoming.len().min(BATCH_SIZE) {
            let Some(ack) = self.sender.incoming.pop_front() else {
                break;
            };
            if !self.matches(ack.session, ack.tag) {
                self.return_ack(ack)?;
                continue;
            }
            let Some(index) = self.sender.lane_index(
                ack.session,
                usize::from(ack.lane) - 1,
                self.shared.workers(),
                self.id,
            ) else {
                self.return_ack(ack)?;
                continue;
            };
            let Some(lane) = self.sender.lanes.get_mut(index) else {
                return Err(io::Error::other("missing sender lane"));
            };
            if lane.ack.is_some() || lane.report_pending {
                self.sender.incoming.push_back(ack);
                continue;
            }
            if lane.begin_ack(&ack) {
                trace_event!(
                    self.id,
                    ACK_BEGIN,
                    0,
                    [
                        ack.session << 16 | usize::from(ack.lane),
                        ack.ranges.first().map_or(0, |range| range.end),
                        ack.ranges[..ack.count]
                            .last()
                            .map_or(0, |range| range.start),
                        ack.count,
                        lane.largest_ack,
                        lane.history.floor()
                    ]
                );
                self.sender.acks_pending += 1;
                lane.loss_passes = 1 + u8::from(lane.scan.is_some());
                self.sender.loss_fences = self
                    .sender
                    .loss_fences
                    .checked_add(1)
                    .ok_or(io::ErrorKind::InvalidData)?;
            }
            self.return_ack(ack)?;
        }
        Ok(())
    }
}

impl State {
    pub(super) fn storage_bytes(
        config: &crate::config::Validated,
        workers: usize,
    ) -> Option<usize> {
        let descriptors = descriptors(config).ok()?;
        let lanes = descriptors.len();
        let groups = config.path_groups();
        let per_worker = (config.config().sessions.len() + 1)
            .checked_mul(size_of::<usize>())?
            .checked_add(ACK_BUFFERS * (size_of::<Ack>() + size_of::<Box<Ack>>()))?
            .checked_add(size_of::<Report>())?
            .checked_add(Records::storage_bytes(RECORDS)?)?
            .checked_add(workers.checked_mul(ACK_BUFFERS * size_of::<Box<Ack>>())?)?
            .checked_add(
                (workers * (ACK_BUFFERS + 4))
                    .next_power_of_two()
                    .checked_mul(size_of::<(usize, Message)>())?,
            )?
            .checked_add(groups.checked_mul(
                size_of::<Option<(u64, PacketId, u8)>>()
                    + size_of::<Option<LossCheck>>()
                    + size_of::<bool>(),
            )?)?;
        workers
            .checked_mul(per_worker)?
            .checked_add(lanes.checked_mul(
                size_of::<Lane>() + History::storage_bytes() - size_of::<History>(),
            )?)?
            .checked_add(if groups == 0 || descriptors.is_empty() {
                0
            } else {
                Owner::storage_bytes(groups, workers, &descriptors)?
            })
    }
}

fn descriptors(config: &crate::config::Validated) -> io::Result<Vec<(usize, u16, usize)>> {
    let count = (0..config.config().sessions.len())
        .try_fold(0usize, |total, session| {
            total.checked_add(
                config
                    .config()
                    .peers
                    .get(config.peer_for_session(session)?)?
                    .tx_lanes,
            )
        })
        .ok_or(io::ErrorKind::OutOfMemory)?;
    let mut descriptors = Vec::new();
    descriptors
        .try_reserve_exact(count)
        .map_err(io::Error::other)?;
    for session in 0..config.config().sessions.len() {
        let peer = config
            .peer_for_session(session)
            .ok_or(io::ErrorKind::InvalidInput)?;
        let group = config
            .path_group_for_peer(peer)
            .ok_or(io::ErrorKind::InvalidInput)?;
        for lane in 1..=config.config().peers[peer].tx_lanes {
            descriptors.push((session, lane as u16, group));
        }
    }
    Ok(descriptors)
}

impl Worker {
    pub(crate) fn init_senders(workers: &mut [Self]) -> io::Result<()> {
        let first = workers.first().ok_or(io::ErrorKind::InvalidInput)?;
        let groups = first.shared.config.path_groups();
        if groups == 0 {
            return Ok(());
        }
        let descriptors = descriptors(&first.shared.config)?;
        if descriptors.is_empty() {
            return Ok(());
        }
        let now = first.io_clock(Duration::ZERO)()?;
        let mut budgets = Vec::new();
        budgets
            .try_reserve_exact(groups)
            .map_err(io::Error::other)?;
        budgets.resize_with(groups, Budget::new);
        let budgets: Arc<[Budget]> = Arc::from(budgets);
        let owner = Owner::new(workers.len(), &descriptors, now, Arc::clone(&budgets))?;
        for worker in workers.iter_mut() {
            worker.sender.budgets = Arc::clone(&budgets);
        }
        workers
            .first_mut()
            .ok_or(io::ErrorKind::InvalidInput)?
            .sender
            .owner = Some(owner);
        Ok(())
    }

    pub(super) fn sender_enabled(&self) -> bool {
        #[cfg(test)]
        if self.unpaced_fixture {
            return false;
        }
        true
    }

    pub(super) fn sender_index(&self, session: usize, lane: usize) -> io::Result<usize> {
        self.sender
            .lane_index(session, lane, self.shared.workers(), self.id)
            .ok_or_else(|| io::Error::other("transmit history on the wrong worker"))
    }

    pub(super) fn sender_message(&mut self, message: Message, now: Duration) -> io::Result<()> {
        match message {
            Message::Ack(ack) => self.queue_sender_ack(ack)?,
            Message::AckReturn(ack) => {
                if ack.origin != self.id || self.sender.ack_free.len() == ACK_BUFFERS {
                    return Err(io::Error::other("invalid returned ACK buffer"));
                }
                self.sender.ack_free.push(ack);
            }
            Message::Sent(sent) => self
                .sender
                .owner
                .as_mut()
                .ok_or(io::ErrorKind::InvalidData)?
                .sent(sent)?,
            Message::Report(report) => self
                .sender
                .owner
                .as_mut()
                .ok_or(io::ErrorKind::InvalidData)?
                .report(report)?,
            Message::ReportReturn(report) => self.report_returned(report)?,
            Message::ProbeReturn {
                group,
                epoch,
                count,
            } => self
                .sender
                .owner
                .as_mut()
                .ok_or(io::ErrorKind::InvalidData)?
                .return_probe(group, epoch, count)?,
            Message::CheckLoss {
                group,
                token,
                before,
                force,
            } => {
                let loss = self
                    .sender
                    .losses
                    .get_mut(group)
                    .ok_or(io::ErrorKind::InvalidInput)?;
                if loss.is_some() {
                    return Err(io::Error::other("overlapping loss checks"));
                }
                *loss = Some(LossCheck {
                    token,
                    before,
                    force,
                    cursor: 0,
                    started: None,
                    next_due: None,
                });
                self.sender.checks_pending += 1;
            }
            Message::LossChecked {
                group,
                token,
                worker,
                next_due,
            } => {
                self.sender
                    .owner
                    .as_mut()
                    .ok_or(io::ErrorKind::InvalidData)?
                    .loss_checked(group, token, worker, next_due)?;
            }
            Message::Probe {
                group,
                epoch,
                session,
                generation,
                lane,
                count,
            } => {
                let probe = self
                    .sender
                    .probes
                    .get_mut(group)
                    .ok_or(io::ErrorKind::InvalidInput)?;
                *probe = Some((
                    epoch,
                    PacketId {
                        session,
                        generation,
                        lane,
                        number: 0,
                    },
                    count,
                ));
            }
            Message::ProbeReplied { group } => self
                .sender
                .owner
                .as_mut()
                .ok_or(io::ErrorKind::InvalidData)?
                .probe_replied(group)?,
        }
        let _ = now;
        Ok(())
    }

    /// Reuses a report whose entries the owner applied, or that needed no owner.
    pub(super) fn report_returned(&mut self, report: Box<Report>) -> io::Result<()> {
        if self.sender.report.is_some() {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let index = self.sender_index(report.id.session, usize::from(report.id.lane) - 1)?;
        let ack = self.sender.lanes[index].report_completed(&report)?;
        if matches!(report.kind, ReportKind::Ack { end: None, .. }) {
            self.sender.cursor = index;
        }
        if ack.is_some() {
            self.sender.acks_pending -= 1;
        }
        self.sender.report = Some(report);
        Ok(())
    }

    /// Copies each entry's send record for the owner, then settles locally:
    /// ACKs and losses release their inflight charge, and ACKs and retirement
    /// return the record handle. A lost packet keeps its record for a late ACK.
    pub(super) fn settle_report(&mut self, report: &mut Report) -> io::Result<()> {
        let mut released = 0;
        for entry in &mut report.entries[..report.count] {
            let Some(handle) = entry.sample else {
                continue;
            };
            entry.record = self
                .sender
                .records
                .get(handle)
                .ok_or_else(|| io::Error::other("missing send record"))?;
            if entry.settled && !matches!(report.kind, ReportKind::Retire) {
                released += u64::from(entry.record.bytes);
            }
            if !matches!(report.kind, ReportKind::Loss) {
                self.sender.records.release(handle);
            }
        }
        if released != 0 {
            self.sender
                .budgets
                .get(report.group)
                .ok_or(io::ErrorKind::InvalidData)?
                .release(released);
        }
        Ok(())
    }

    pub(super) fn flush_sender_messages(&mut self, now: Duration) -> io::Result<bool> {
        if self.sender.outgoing_left == 0 {
            self.sender.outgoing_cursor = 0;
            self.sender.outgoing_blocked = 0;
            self.sender.outgoing_left = self.sender.outgoing.len();
        }
        let mut progress = false;
        for _ in 0..self.sender.outgoing_left.min(BATCH_SIZE) {
            let index = self.sender.outgoing_cursor;
            let worker = self
                .sender
                .outgoing
                .get(index)
                .ok_or(io::ErrorKind::InvalidData)?
                .0;
            self.sender.outgoing_left -= 1;
            let bit = 1_u64 << worker;
            if self.sender.outgoing_blocked & bit != 0 {
                self.sender.outgoing_cursor += 1;
                continue;
            }
            let (_, message) = self
                .sender
                .outgoing
                .remove(index)
                .ok_or(io::ErrorKind::InvalidData)?;
            if worker == self.id {
                self.sender_message(message, now)?;
                progress = true;
            } else {
                match self.push(worker, Event::Sender(message)) {
                    Ok(()) => progress = true,
                    Err(Event::Sender(message)) => {
                        // Keep destination FIFO across partial sweeps and mid-sweep queue relief.
                        self.sender.outgoing.insert(index, (worker, message));
                        self.sender.outgoing_cursor += 1;
                        self.sender.outgoing_blocked |= bit;
                    }
                    Err(_) => return Err(io::ErrorKind::InvalidData.into()),
                }
            }
        }
        Ok(progress || self.sender.outgoing_left != 0)
    }

    fn advance_loss_checks(&mut self, now: Duration, budget: &mut usize) -> io::Result<bool> {
        let mut more = false;
        for _ in 0..self.sender.losses.len().min(BATCH_SIZE) {
            if *budget == 0 {
                return Ok(self.sender.checks_pending != 0);
            }
            let group = self.sender.loss_cursor;
            self.sender.loss_cursor = (group + 1) % self.sender.losses.len();
            let Some(mut check) = self.sender.losses[group] else {
                continue;
            };
            *budget -= 1;
            while *budget != 0 && check.cursor < self.sender.lanes.len() {
                let lane = &mut self.sender.lanes[check.cursor];
                if lane.group != group {
                    check.cursor += 1;
                    *budget -= 1;
                    continue;
                }
                if lane.report_pending || self.sender.report.is_none() {
                    break;
                }
                if check
                    .started
                    .is_some_and(|generation| generation != lane.generation)
                {
                    // Installing another generation required all old reports and charges to drain.
                    check.started = None;
                    check.cursor += 1;
                    *budget -= 1;
                    continue;
                }
                if check.started.is_some() && lane.scan.is_none() {
                    check.next_due = match (check.next_due, lane.loss_due) {
                        (Some(a), Some(b)) => Some(a.min(b)),
                        (a, b) => a.or(b),
                    };
                    check.started = None;
                    check.cursor += 1;
                    *budget -= 1;
                    continue;
                }
                if lane.scan.is_none() {
                    lane.begin_loss_check(check.before, check.force)?;
                    check.started = Some(lane.generation);
                }
                let mut report = self
                    .sender
                    .report
                    .take()
                    .ok_or(io::ErrorKind::InvalidData)?;
                let prior = *budget;
                let fenced = lane.loss_passes != 0;
                let tag = self.sessions[lane.session]
                    .connection
                    .map(|c| c.local_tag().get());
                let produced = lane.advance_report_with_acks(
                    &mut report,
                    now,
                    budget,
                    &self.sender.incoming,
                    tag,
                )?;
                self.sender.loss_fences = self
                    .sender
                    .loss_fences
                    .checked_sub(usize::from(fenced && lane.loss_passes == 0))
                    .ok_or(io::ErrorKind::InvalidData)?;
                if *budget == prior {
                    *budget -= 1;
                }
                if produced {
                    self.recovery.staged = Some(report);
                    more = true;
                    break;
                }
                self.sender.report = Some(report);
            }
            if check.cursor == self.sender.lanes.len()
                && self.sender.outgoing.len()
                    < self
                        .sender
                        .outgoing
                        .capacity()
                        .saturating_sub(6 * self.shared.workers())
            {
                self.sender.queue(
                    0,
                    Message::LossChecked {
                        group,
                        token: check.token,
                        worker: self.id,
                        next_due: check.next_due,
                    },
                )?;
                self.sender.losses[group] = None;
                self.sender.checks_pending -= 1;
                more = true;
            } else {
                self.sender.losses[group] = Some(check);
                // Reports in transit wake us when returned; do not spin waiting for them.
                more |= self.sender.report.is_some()
                    && self.sender.outgoing.len()
                        < self
                            .sender
                            .outgoing
                            .capacity()
                            .saturating_sub(6 * self.shared.workers());
            }
        }
        Ok(more
            || (self.sender.checks_pending != 0
                && self.sender.report.is_some()
                && self.sender.outgoing.len()
                    < self
                        .sender
                        .outgoing
                        .capacity()
                        .saturating_sub(6 * self.shared.workers())))
    }

    pub(super) fn service_sender(&mut self, now: Duration) -> io::Result<bool> {
        if !self.sender_enabled() {
            return Ok(false);
        }
        let mut more = self.flush_sender_messages(now)?;
        let ack_blocked = self.sender.checks_pending != 0 || self.sender.loss_fences != 0;
        self.begin_sender_ack()?;
        for group in 0..self.sender.fresh.len() {
            if self.sender.fresh[group]
                && self.sender.outgoing.len()
                    < self
                        .sender
                        .outgoing
                        .capacity()
                        .saturating_sub(6 * self.shared.workers())
            {
                self.sender.queue(0, Message::ProbeReplied { group })?;
                self.sender.fresh[group] = false;
            }
        }
        let mut budget = BATCH_SIZE;
        if self.sender.acks_pending == 0 {
            more |= self.advance_loss_checks(now, &mut budget)?;
        }
        for _ in 0..self.sender.lanes.len().min(BATCH_SIZE) {
            if budget == 0 {
                more = true;
                break;
            }
            let Some(mut report) = self.sender.report.take() else {
                break;
            };
            let index = self.sender.cursor;
            self.sender.cursor = (index + 1) % self.sender.lanes.len();
            let lane = &mut self.sender.lanes[index];
            if lane.ack.is_none()
                && (self.sender.acks_pending != 0 || self.sender.checks_pending != 0)
            {
                self.sender.report = Some(report);
                budget -= 1;
                continue;
            }
            if self.sessions[lane.session].connection.is_none() {
                lane.force_loss(now);
            }
            let quiescing = self.sessions[lane.session].connection.is_none();
            let retire = quiescing || self.sender.reclaim.is_some() || lane.retire_due;
            let fenced = lane.loss_passes != 0;
            let tag = self.sessions[lane.session]
                .connection
                .map(|c| c.local_tag().get());
            let produced = lane.advance_report_with_acks(
                &mut report,
                now,
                &mut budget,
                &self.sender.incoming,
                tag,
            )? || (retire && lane.retire_report(&mut report, &mut budget)?);
            self.sender.loss_fences = self
                .sender
                .loss_fences
                .checked_sub(usize::from(fenced && lane.loss_passes == 0))
                .ok_or(io::ErrorKind::InvalidData)?;
            if lane.ack.is_some() {
                self.sender.cursor = index;
            }
            more |= lane.work_pending(now) || (quiescing && !lane.idle());
            if retire && !lane.report_pending {
                self.sender.reclaim = self
                    .sender
                    .reclaim
                    .and_then(|left| left.checked_sub(1).filter(|&n| n != 0));
            }
            if produced {
                self.recovery.staged = Some(report);
                more = true;
            } else {
                budget = budget.saturating_sub(1);
                self.sender.report = Some(report);
            }
        }
        more |= budget == 0 && (self.sender.acks_pending != 0 || self.sender.checks_pending != 0);
        // A fully protected final loss pass emits no report to wake queued ACK work.
        more |= ack_blocked
            && self.sender.checks_pending == 0
            && self.sender.loss_fences == 0
            && !self.sender.incoming.is_empty()
            && self.sender.report.is_some();
        more |= self.stage_recovery()?;
        if self.sender.owner.is_some() {
            // Apply local feedback before the shared owner considers fresh grants.
            more |= self.flush_sender_messages(now)?;
            // Reports can have arrived after the worker turn's original timestamp.
            let observed = self.io_clock(now)()?;
            if let Some(owner) = &mut self.sender.owner {
                more |= owner.advance(observed, &mut self.sender.outgoing)?;
            }
        }
        more |= self.flush_sender_messages(now)?;
        // Probes park an unclaimed DATA plan rather than wait behind it: under
        // pacing-limited load such a plan almost always occupies the job slot.
        if self.parkable() {
            for group in 0..self.sender.probes.len() {
                if let Some((epoch, id, count)) = self.sender.probes[group] {
                    // Fresh feedback ended this PTO round; its remaining probes are void.
                    if self
                        .sender
                        .budgets
                        .get(group)
                        .is_some_and(|budget| budget.probe_epoch() != epoch)
                    {
                        self.sender.probes[group] = None;
                        continue;
                    }
                    let installed = self.sessions.get(id.session).and_then(|s| s.installed);
                    if installed
                        .is_some_and(|c| c.generation == id.generation && c.connection.is_some())
                    {
                        self.request_probe(
                            id.session,
                            usize::from(id.lane) - 1,
                            BASE_UDP_PAYLOAD,
                            false,
                            Some(epoch),
                            now,
                        )?;
                        if self.sender.job.is_some() {
                            self.sender.probes[group] =
                                (count > 1).then_some((epoch, id, count - 1));
                        }
                    } else if self.sender.outgoing.len()
                        < self
                            .sender
                            .outgoing
                            .capacity()
                            .saturating_sub(6 * self.shared.workers())
                    {
                        self.sender.queue(
                            0,
                            Message::ProbeReturn {
                                group,
                                epoch,
                                count,
                            },
                        )?;
                        self.sender.probes[group] = None;
                        more = true;
                    } else {
                        more = true;
                    }
                    break;
                }
            }
        }
        if self.parkable() {
            for _ in 0..self.controls.len().min(BATCH_SIZE) {
                let session = self.sender.liveness_cursor;
                self.sender.liveness_cursor = (session + 1) % self.controls.len();
                let control = &self.controls[session];
                if now < control.probe_due
                    || control.barrier.is_active()
                    || control.state.connection().is_none()
                    || self.sessions[session].connection.is_none()
                {
                    continue;
                }
                let peer = self
                    .shared
                    .config
                    .peer_for_session(session)
                    .ok_or(io::ErrorKind::InvalidData)?;
                if !self.shared.peer_bound(peer) {
                    continue;
                }
                let path = self
                    .sessions
                    .get(session)
                    .ok_or(io::ErrorKind::InvalidData)?
                    .path;
                let socket = &self
                    .transmit
                    .get(peer)
                    .and_then(|peer| peer.paths.get(path))
                    .and_then(|path| path.lanes.first())
                    .ok_or(io::ErrorKind::InvalidData)?
                    .socket;
                // History pressure or an unavailable path must not spin on an overdue probe.
                self.controls[session].probe_due = now.saturating_add(RETRY);
                if socket.is_none() {
                    continue;
                }
                self.request_probe(session, 0, BASE_UDP_PAYLOAD, false, None, now)?;
                if self.sender.job.is_some() {
                    self.controls[session].probe_due = now.saturating_add(Duration::from_secs(1));

                    more = true;
                    break;
                }
            }
        }
        if self.sender.job.is_some() && self.claim_sender(now)? == Claimed::Ready {
            self.use_sender_permit(now)?;
            more = true;
        }
        Ok(more)
    }

    pub(super) fn sender_idle(&self, session: usize) -> bool {
        if !self.sender_enabled() {
            return true;
        }
        if self.recovery.quiesce.is_some()
            || self.sessions[session].tx_retained != 0
            || self.sender.job.is_some_and(|job| job.session == session)
        {
            return false;
        }
        let Some((&first, &end)) = self
            .sender
            .offsets
            .get(session)
            .zip(self.sender.offsets.get(session + 1))
        else {
            return false;
        };
        self.sender.lanes[first..end].iter().all(Lane::idle)
    }

    pub(super) fn install_sender(
        &mut self,
        session: usize,
        generation: u64,
        active: bool,
    ) -> io::Result<()> {
        if !self.sender_enabled() {
            return Ok(());
        }
        let (&first, &end) = self
            .sender
            .offsets
            .get(session)
            .zip(self.sender.offsets.get(session + 1))
            .ok_or(io::ErrorKind::InvalidInput)?;
        for lane in &mut self.sender.lanes[first..end] {
            lane.install(generation)?;
        }
        if active && let Some(owner) = &mut self.sender.owner {
            owner.timers.install(session, generation)?;
        }
        Ok(())
    }
}

impl Worker {
    /// True means history had no room for the plan's first packet: keep the plan,
    /// so its unreserved numbers are retried instead of becoming holes.
    pub(super) fn request_batch(
        &mut self,
        session: usize,
        lane: usize,
        batch: &TxBatch,
        deadline: Duration,
        mtu: usize,
    ) -> io::Result<bool> {
        if self.sender.job.is_some() {
            return Ok(false);
        }
        let index = self.sender_index(session, lane)?;
        let generation = self.sessions[session]
            .installed
            .ok_or(io::ErrorKind::InvalidData)?
            .generation;
        let mut packets = [pacing::Planned::default(); BATCH_SIZE];
        let mut count = 0;
        let mut full = false;
        for (ordinal, planned) in packets.iter_mut().take(batch.plan.len()).enumerate() {
            let packet = batch
                .plan
                .packet(ordinal)
                .ok_or(io::ErrorKind::InvalidData)?;
            let history = &mut self.sender.lanes[index].history;
            if packet.number <= history.highest_allocated() && !history.is_reserved(packet.number) {
                // A canceled higher reservation can overtake a never-reserved plan suffix.
                // Earlier entries in this increasing plan are only existing reservations.
                trace_hold!(
                    self.id,
                    HOLD_OVERTAKEN,
                    [
                        session << 16 | (lane + 1),
                        batch.len,
                        packet.number,
                        history.highest_allocated()
                    ]
                );
                return Ok(false);
            }
            match if history.is_reserved(packet.number) {
                Ok(())
            } else if history.can_reserve(packet.number.saturating_add(2 * PROBE_HEADROOM)) {
                history.reserve(packet.number, (packet.length + 48) as u16, true, true)
            } else {
                Err(crate::ack::HistoryError::Full)
            } {
                Ok(()) => {}
                Err(crate::ack::HistoryError::Full) => {
                    trace_hold!(
                        self.id,
                        HOLD_HISTORY,
                        [
                            session << 16 | (lane + 1),
                            batch.len,
                            packet.number,
                            count,
                            history.trace_floor()[0],
                            history.trace_floor()[1] << 32
                                | history.trace_floor()[2] << 8
                                | history.trace_floor()[3]
                        ]
                    );
                    self.sender.reclaim = Some(self.sender.lanes.len());
                    full = true;
                    break;
                }
                Err(error) => return Err(io::Error::other(error)),
            }
            *planned = pacing::Planned {
                id: PacketId {
                    session,
                    generation,
                    lane: lane as u16 + 1,
                    number: packet.number,
                },
                bytes: (packet.length + 48) as u16,
            };
            count += 1;
        }
        if count == 0 {
            return Ok(full);
        }
        if self.sender.records.available() < BATCH_SIZE {
            self.sender.reclaim = Some(self.sender.lanes.len());
        }
        match self
            .sender
            .flight
            .prepare(&packets[..count], &mut self.sender.records)
        {
            Ok(_) => {}
            // Retained loss records fill the slab: keep the reserved plan until retirement.
            Err(pacing::FlightError::Blocked) => return Ok(true),
            Err(error) => return Err(io::Error::other(error)),
        }
        self.sender.job = Some(Job {
            session,
            generation,
            lane,
            path: self.sessions[session].path,
            group: self.sender.lanes[index].group,
            first: packets[0].id.number,
            count,
            deadline,
            kind: JobKind::Data,
            mtu: (mtu + 48).max(1280) as u16,
            probe: None,
        });
        trace_event!(
            self.id,
            REQUEST,
            0,
            [
                session << 16 | (lane + 1),
                packets[..count]
                    .iter()
                    .map(|p| u64::from(p.bytes))
                    .sum::<u64>(),
                count,
                deadline.as_nanos(),
                self.frames.deadline().map_or(0, |at| at.as_nanos()),
                self.sender.lanes[index].unsent_bytes,
            ]
        );
        Ok(false)
    }

    pub(super) fn request_probe(
        &mut self,
        session: usize,
        lane: usize,
        length: usize,
        pmtu: bool,
        probe: Option<u64>,
        now: Duration,
    ) -> io::Result<()> {
        if !self.parkable() {
            return Ok(());
        }
        // A probe goes before an unclaimed DATA plan, which keeps its reservations.
        self.park_job();
        if self.sender.records.available() == 0 {
            self.sender.reclaim = Some(self.sender.lanes.len());
            return Ok(());
        }
        let index = self.sender_index(session, lane)?;
        let local = self
            .sessions
            .get_mut(session)
            .ok_or(io::ErrorKind::InvalidInput)?;
        let Some(generation) = local
            .installed
            .filter(|_| local.connection.is_some())
            .map(|c| c.generation)
        else {
            return Ok(());
        };
        let mut prefix = [0; wire::PROBE_PREFIX_LEN];
        let Some(tx) = local.tx.get_mut(lane).and_then(Option::as_mut) else {
            return Ok(());
        };
        // A probe retried every turn under history pressure must not consume a number each time.
        if !self.sender.lanes[index]
            .history
            .can_reserve(tx.next_packet())
        {
            self.sender.reclaim = Some(self.sender.lanes.len());
            return Ok(());
        }
        let piece = tx.probe(length, &mut prefix).map_err(io::Error::other)?;
        match self.sender.lanes[index].history.reserve(
            piece.packet.number,
            (length + 48) as u16,
            true,
            true,
        ) {
            Ok(()) => {}
            Err(crate::ack::HistoryError::Full) => {
                self.sender.reclaim = Some(self.sender.lanes.len());
                return Ok(());
            }
            Err(error) => return Err(io::Error::other(error)),
        }
        self.sender
            .flight
            .prepare(
                &[pacing::Planned {
                    id: PacketId {
                        session,
                        generation,
                        lane: lane as u16 + 1,
                        number: piece.packet.number,
                    },
                    bytes: (length + 48) as u16,
                }],
                &mut self.sender.records,
            )
            .map_err(io::Error::other)?;
        self.sender.flight.pmtu = pmtu;
        let peer = self
            .shared
            .config
            .peer_for_session(session)
            .ok_or(io::ErrorKind::InvalidInput)?;
        let mtu = (self.transmit[peer].paths[local.path].lanes[lane]
            .mtu
            .payload_limit()
            + 48)
            .max(1280);
        self.sender.job = Some(Job {
            session,
            generation,
            lane,
            path: local.path,
            group: self.sender.lanes[index].group,
            first: piece.packet.number,
            count: 1,
            deadline: now.saturating_add(Duration::from_millis(100)),
            kind: JobKind::Probe { length, pmtu },
            mtu: mtu as u16,
            probe,
        });
        Ok(())
    }

    /// Claims inflight and pacing credit for the prepared job immediately before
    /// its syscall. A refusal keeps the plan and sets the retry time.
    pub(super) fn claim_sender(&mut self, now: Duration) -> io::Result<Claimed> {
        let Some(job) = self.sender.job else {
            return Ok(Claimed::Abandoned);
        };
        if self.sender.permitted {
            return Ok(Claimed::Ready);
        }
        let now = self.io_clock(now)()?;
        let local = self
            .sessions
            .get(job.session)
            .ok_or(io::ErrorKind::InvalidData)?;
        let budget = self
            .sender
            .budgets
            .get(job.group)
            .ok_or(io::ErrorKind::InvalidData)?;
        if now >= job.deadline
            || local.connection.is_none()
            || local.path != job.path
            || local
                .installed
                .is_none_or(|c| c.generation != job.generation)
            || job.probe.is_some_and(|epoch| epoch != budget.probe_epoch())
        {
            self.rejected_sender_grant(job.session, job.lane, now >= job.deadline);
            self.recycle_job(now)?;
            return Ok(Claimed::Abandoned);
        }
        // Every successful send owes the owner one summary; never strand it.
        if self.sender.outgoing.len()
            >= self
                .sender
                .outgoing
                .capacity()
                .saturating_sub(6 * self.shared.workers())
        {
            self.sender.retry = Some(now);
            return Ok(Claimed::Waiting);
        }
        let state = budget.published();
        let pmtu = matches!(job.kind, JobKind::Probe { pmtu: true, .. });
        let first = self
            .sender
            .flight
            .packets()
            .next()
            .ok_or(io::ErrorKind::InvalidData)?;
        // A probe must fit the whole window, not just the pacing burst.
        // A stalled zero window is temporary and cannot constrain path discovery.
        let probe_limit = if state.window == 0 {
            state.quantum
        } else {
            state.quantum.min(state.window)
        };
        if pmtu && u64::from(first.bytes) > probe_limit {
            let peer = self
                .shared
                .config
                .peer_for_session(job.session)
                .ok_or(io::ErrorKind::InvalidInput)?;
            self.transmit[peer].paths[job.path].lanes[job.lane]
                .mtu
                .limit_probe(probe_limit.saturating_sub(48) as usize, now);
            self.recycle_job(now)?;
            return Ok(Claimed::Abandoned);
        }
        let window = job.probe.is_none().then_some(state.window);
        let room = window.map_or(state.quantum, |window| {
            state.quantum.min(window.saturating_sub(budget.in_flight()))
        });
        let refused = match self.sender.flight.truncate(room, &mut self.sender.records) {
            Ok(bytes) => match budget.charge(bytes, window) {
                Some(prior) => match budget.pace(bytes, state.rate, now) {
                    Ok(()) => {
                        let claim = Claim {
                            group: job.group,
                            prior,
                            bytes,
                            at: now,
                            state,
                        };
                        self.sender
                            .flight
                            .set_claim(claim)
                            .map_err(io::Error::other)?;
                        None
                    }
                    Err(at) => {
                        budget.release(bytes);
                        Some((at, BLOCK_PACING))
                    }
                },
                None => Some((now, BLOCK_WINDOW)),
            },
            Err(pacing::FlightError::Blocked) => Some((now, BLOCK_WINDOW)),
            Err(error) => return Err(io::Error::other(error)),
        };
        if let Some((at, _reason)) = refused {
            trace_event!(
                self.id,
                BLOCK,
                self.id | job.group << 8,
                [
                    _reason,
                    state.rate,
                    at.as_nanos(),
                    state.window,
                    budget.in_flight()
                ]
            );
            if pmtu {
                // Path discovery retries later instead of holding DATA behind it.
                self.recycle_job(now)?;
                return Ok(Claimed::Abandoned);
            }
            let retry = if at > now {
                at
            } else if state.window == 0 {
                now.saturating_add(RETRY)
            } else {
                now.saturating_add(WINDOW_RETRY)
            };
            self.sender.retry = Some(retry.min(job.deadline));
            return Ok(Claimed::Waiting);
        }
        trace_event!(self.id, PERMIT, 0, [state.quantum, now.as_nanos()]);
        self.counters.send_grants = self.counters.send_grants.wrapping_add(1);
        let local = &mut self.sessions[job.session];
        local.totals.send_grants = local.totals.send_grants.wrapping_add(1);
        local.tx_totals[job.lane].send_grants =
            local.tx_totals[job.lane].send_grants.wrapping_add(1);
        local.tx_dirty |= 1_u64 << job.lane;
        self.sender.permitted = true;
        self.sender.retry = None;
        Ok(Claimed::Ready)
    }

    /// Whether a probe may take the job slot: it is free or holds unclaimed DATA.
    pub(super) fn parkable(&self) -> bool {
        self.sender
            .job
            .is_none_or(|job| !self.sender.permitted && matches!(job.kind, JobKind::Data))
    }

    /// Steps an unclaimed DATA job aside. Its plan and history reservations stay
    /// in the batch, so the next flush requests the same numbers again.
    pub(super) fn park_job(&mut self) {
        if self.sender.job.take().is_some() {
            self.sender.flight.release(&mut self.sender.records);
            self.sender.retry = None;
        }
    }

    /// Ends the current job: unsent reservations and handles return, and a plan
    /// still starting at the job's first number is discarded.
    pub(super) fn recycle_job(&mut self, now: Duration) -> io::Result<()> {
        let sent = self
            .sender
            .flight
            .completion()
            .is_some_and(|(count, _, _)| count != 0);
        trace_event!(
            self.id,
            RECYCLE,
            0,
            [self
                .sender
                .flight
                .completion()
                .map_or(u64::MAX, |(count, _, _)| count as u64)]
        );
        if let Some(job) = self.sender.job.take() {
            if matches!(job.kind, JobKind::Probe { pmtu: true, .. }) && !sent {
                let peer = self
                    .shared
                    .config
                    .peer_for_session(job.session)
                    .ok_or(io::ErrorKind::InvalidInput)?;
                self.transmit[peer].paths[job.path].lanes[job.lane]
                    .mtu
                    .defer(now);
            }
            // Completed prefixes no longer own a retained plan's unattempted suffix.
            let index = self.sender_index(job.session, job.lane)?;
            for offset in 0..job.count {
                self.sender.lanes[index]
                    .history
                    .cancel(job.first + offset as u64);
            }
            if let Some(batch) = self.sessions[job.session].batches[job.lane].as_mut()
                && batch.plan.packet(0).is_some_and(|p| p.number == job.first)
            {
                batch.discard(Some(&mut self.sender.lanes[index].history));
            }
        }
        self.sender.flight.release(&mut self.sender.records);
        self.sender.permitted = false;
        self.sender.retry = None;
        Ok(())
    }

    pub(super) fn finish_sender_send(
        &mut self,
        count: usize,
        at: Option<Duration>,
        host_limited: bool,
    ) -> io::Result<()> {
        let job = self.sender.job.ok_or(io::ErrorKind::InvalidData)?;
        let index = self.sender_index(job.session, job.lane)?;
        let claim = self
            .sender
            .flight
            .claim()
            .ok_or(io::ErrorKind::InvalidData)?;
        let planned = self.sender.flight.packets().len();
        self.settle_sent_pieces(count)?;
        trace_event!(
            self.id,
            SENT,
            0,
            [count, planned, at.map_or(0, |at| at.as_nanos())]
        );
        let host_limited = host_limited || count < planned;
        self.sender
            .flight
            .complete(count, at, host_limited)
            .map_err(io::Error::other)?;
        let budget = self
            .sender
            .budgets
            .get(claim.group)
            .ok_or(io::ErrorKind::InvalidData)?;
        let mut sent_bytes = 0;
        if let Some(at) = at {
            let mut batch = budget.begin(&claim.state, claim.prior, count as u64, at);
            for (ordinal, packet) in self.sender.flight.packets().enumerate() {
                let handle = self
                    .sender
                    .flight
                    .handle(ordinal)
                    .ok_or(io::ErrorKind::InvalidData)?;
                sent_bytes += u64::from(packet.bytes);
                let record = batch.push(packet.bytes, self.sender.flight.pmtu, host_limited);
                if !self.sender.records.set(handle, record)
                    || !self.sender.lanes[index].submitted(packet.id.number, at, Some(handle))
                {
                    return Err(io::Error::other("submitted packet has no reservation"));
                }
            }
        }
        budget.release(claim.bytes.saturating_sub(sent_bytes));
        if let Some(at) = at {
            let in_flight = budget.in_flight();
            let sent = Sent {
                group: claim.group,
                id: PacketId {
                    session: job.session,
                    generation: job.generation,
                    lane: job.lane as u16 + 1,
                    number: 0,
                },
                count: count as u32,
                at,
                mtu: job.mtu,
                cwnd_limited: in_flight != 0
                    && claim.state.window.saturating_sub(in_flight) < u64::from(job.mtu),
                restart: claim.prior == 0,
            };
            self.sender.queue(0, Message::Sent(sent))?;
        }
        // A fully sent trimmed plan keeps its never-attempted suffix reserved.
        if matches!(job.kind, JobKind::Data) && count != 0 && count == planned {
            self.sender.job = Some(Job { count, ..job });
        }
        self.recycle_job(at.unwrap_or(claim.at))
    }

    pub(super) fn rejected_sender_grant(&mut self, session: usize, lane: usize, expired: bool) {
        let local = &mut self.sessions[session];
        local.tx_dirty |= 1_u64 << lane;
        for counters in [
            &mut self.counters,
            &mut local.totals,
            &mut local.tx_totals[lane],
        ] {
            let count = if expired {
                &mut counters.expired_grants
            } else {
                &mut counters.cancelled_grants
            };
            *count = count.wrapping_add(1);
        }
    }

    // Uncharged feedback sent on a DATA lane can take a claimed plan's numbers.
    // Refresh unsent identities locally; the claim keeps its bytes and time.
    fn refresh_sender_numbers(&mut self, job: Job, index: usize) -> io::Result<bool> {
        let flight = &mut self.sender.flight;
        let local = &mut self.sessions[job.session];
        let tx = local.tx[job.lane]
            .as_mut()
            .ok_or(io::ErrorKind::InvalidData)?;
        let mut old_count = job.count;
        let first = match job.kind {
            JobKind::Data => {
                let batch = local.batches[job.lane]
                    .as_mut()
                    .ok_or(io::ErrorKind::InvalidData)?;
                if batch
                    .plan
                    .packet(0)
                    .is_none_or(|packet| packet.number != job.first)
                {
                    return Ok(false);
                }
                old_count = old_count.max(batch.plan.len());
                batch.plan.renumber(tx)
            }
            JobKind::Probe { .. } => tx.allocate_packets(1),
        };
        let Ok(first) = first else { return Ok(false) };
        let history = &mut self.sender.lanes[index].history;
        // The old plan has been replaced, including any retained ungranted reservations.
        for offset in 0..old_count {
            history.cancel(job.first + offset as u64);
        }
        // Refreshed DATA keeps the same headroom as a fresh request; a probe may use it.
        let headroom = match job.kind {
            JobKind::Data => 2 * PROBE_HEADROOM,
            JobKind::Probe { .. } => 0,
        };
        let mut reserved = 0;
        for packet in flight.packets() {
            let number = first + reserved as u64;
            match if history.can_reserve(number.saturating_add(headroom)) {
                history.reserve(number, packet.bytes, true, true)
            } else {
                Err(crate::ack::HistoryError::Full)
            } {
                Ok(()) => reserved += 1,
                Err(error) => {
                    for offset in 0..reserved {
                        history.cancel(first + offset as u64);
                    }
                    return if error == crate::ack::HistoryError::Full {
                        Ok(false)
                    } else {
                        Err(io::Error::other(error))
                    };
                }
            }
        }
        if let Err(error) = flight.reidentify(first) {
            for offset in 0..reserved {
                history.cancel(first + offset as u64);
            }
            return Err(io::Error::other(error));
        }
        self.sender.job = Some(Job {
            first,
            count: reserved,
            ..job
        });
        Ok(true)
    }

    pub(super) fn use_sender_permit(&mut self, now: Duration) -> io::Result<()> {
        let now = self.io_clock(now)()?;
        let job = self.sender.job.ok_or(io::ErrorKind::InvalidData)?;
        let local = self
            .sessions
            .get(job.session)
            .ok_or(io::ErrorKind::InvalidData)?;
        let index = self.sender_index(job.session, job.lane)?;
        if now >= job.deadline
            || local.connection.is_none()
            || local.path != job.path
            || local
                .installed
                .is_none_or(|c| c.generation != job.generation)
        {
            self.rejected_sender_grant(job.session, job.lane, now >= job.deadline);
            self.finish_sender_send(0, None, false)?;
            if let Some(batch) = self.sessions[job.session].batches[job.lane].as_mut() {
                batch.discard(Some(&mut self.sender.lanes[index].history));
            }
            return Ok(());
        }
        if job.first <= self.sender.lanes[index].history.highest_sent()
            && !self.refresh_sender_numbers(job, index)?
        {
            self.rejected_sender_grant(job.session, job.lane, false);
            self.finish_sender_send(0, None, false)?;
            if let Some(batch) = self.sessions[job.session].batches[job.lane].as_mut() {
                batch.discard(Some(&mut self.sender.lanes[index].history));
            }
            return Ok(());
        }
        let job = self.sender.job.ok_or(io::ErrorKind::InvalidData)?;
        match job.kind {
            JobKind::Data => {
                let mut batch = self.sessions[job.session].batches[job.lane]
                    .take()
                    .ok_or(io::ErrorKind::InvalidData)?;
                if batch
                    .plan
                    .packet(0)
                    .is_none_or(|packet| packet.number != job.first)
                {
                    self.rejected_sender_grant(job.session, job.lane, false);
                    self.finish_sender_send(0, None, false)?;
                } else {
                    self.flush_batch(job.session, job.lane, &mut batch, now)?;
                    if self.sender.permitted {
                        self.rejected_sender_grant(job.session, job.lane, false);
                        self.finish_sender_send(0, None, false)?;
                        batch.discard(Some(&mut self.sender.lanes[index].history));
                    }
                }
                self.sessions[job.session].batches[job.lane] = Some(batch);
            }
            JobKind::Probe { length, pmtu } => {
                let mut clock = self.io_clock(now);
                let local = &self.sessions[job.session];
                let connection = local.connection.ok_or(io::ErrorKind::InvalidData)?;
                let header = Header {
                    cookie: self.shared.config.config().sessions[job.session].cookie,
                    receive_tag: connection.remote_tag().get(),
                    lane: job.lane as u16 + 1,
                    packet_number: job.first,
                    record_area_len: 0,
                };
                let mut prefix = [0; wire::PROBE_PREFIX_LEN];
                wire::encode_probe_prefix(header, length, &mut prefix).map_err(io::Error::other)?;
                let padding = pack::PROBE_PADDING
                    .get(..length - wire::PROBE_PREFIX_LEN)
                    .ok_or(io::ErrorKind::InvalidInput)?;
                let parts = [&prefix[..], padding];
                let peer = self
                    .shared
                    .config
                    .peer_for_session(job.session)
                    .ok_or(io::ErrorKind::InvalidInput)?;
                let socket = self.transmit[peer].paths[job.path].lanes[job.lane]
                    .socket
                    .as_ref();
                let mut rejected = None;
                let result = if let Some(socket) = socket {
                    socket.send_batch_checked(&[SendDatagram { parts: &parts }], length, || {
                        let at = clock()?;
                        if at >= job.deadline {
                            rejected = Some(true);
                            return Err(io::ErrorKind::WouldBlock.into());
                        }
                        Ok(at)
                    })
                } else {
                    Err(io::ErrorKind::NotConnected.into())
                };
                if let Some(expired) = rejected {
                    self.rejected_sender_grant(job.session, job.lane, expired);
                }
                let sent = result.as_ref().ok().copied();
                self.finish_sender_send(
                    sent.map_or(0, |(n, _)| n),
                    sent.and_then(|(n, at)| (n != 0).then_some(at)),
                    result
                        .as_ref()
                        .is_err_and(|e| e.kind() == io::ErrorKind::WouldBlock),
                )?;
                if let Some((1, at)) = sent {
                    let index = self.sender_index(job.session, job.lane)?;
                    self.sender.lanes[index].probes.rotate_left(1);
                    self.sender.lanes[index].probes[1] = Some((job.first, length as u16));
                    let local = &mut self.sessions[job.session];
                    if pmtu {
                        let mtu = &mut self.transmit[peer].paths[job.path].lanes[job.lane].mtu;
                        if !mtu.submitted(
                            pmtu::ProbeId {
                                session: job.session,
                                tag: connection.local_tag().get(),
                                lane: job.lane as u16 + 1,
                                packet: job.first,
                            },
                            length,
                            at,
                        ) {
                            mtu.defer(at);
                        }
                    } else if self.id == 0 && job.lane == 0 {
                        self.controls[job.session].probe = Some((job.first, length as u16));
                        self.controls[job.session].probe_due =
                            at.saturating_add(Duration::from_secs(1));
                    }
                    for totals in [
                        &mut self.counters,
                        &mut local.totals,
                        &mut local.tx_totals[job.lane],
                    ] {
                        totals.tx_outer_bytes =
                            totals.tx_outer_bytes.wrapping_add((length + 48) as u64);
                    }
                    local.tx_dirty |= 1_u64 << job.lane;
                } else {
                    if pmtu {
                        self.transmit[peer].paths[job.path].lanes[job.lane]
                            .mtu
                            .defer(now);
                    }
                    if let Err(error) = result {
                        if error.raw_os_error() == Some(libc::EMSGSIZE) {
                            self.mtu_send_error(peer, job.path, job.lane, length, now)?;
                        } else if !transient(&error) {
                            self.counters.socket_errors =
                                self.counters.socket_errors.wrapping_add(1);
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

impl Worker {
    pub(super) fn quiesced_sender(&mut self, session: usize) -> io::Result<()> {
        if !self.sender_enabled() {
            return Ok(());
        }
        let index = self.sender_index(session, 0)?;
        let generation = self.sender.lanes[index].generation;
        if let Some(owner) = &mut self.sender.owner
            && owner.timers.matches(PacketId {
                session,
                generation,
                lane: 1,
                number: 0,
            })
        {
            owner.timers.quiesce(session, generation)?;
        }
        Ok(())
    }
}

impl Worker {
    pub(super) fn publish_sender(&mut self) {
        self.counters.rtt_ns = 0;
        self.counters.bytes_in_flight = 0;
        self.counters.pacing_bytes_per_second = 0;
        for local in &mut self.sessions {
            local.totals.bytes_in_flight = 0;
            local.totals.rtt_ns = 0;
        }
        for lane in &self.sender.lanes {
            let local = &mut self.sessions[lane.session];
            let totals = &mut local.tx_totals[usize::from(lane.lane) - 1];
            totals.bytes_in_flight = lane.history.inflight_bytes();
            totals.rtt_ns = lane.rtt.minimum().map_or(0, |_| {
                lane.rtt.smoothed().as_nanos().min(u64::MAX as u128) as u64
            });
            local.tx_dirty |= 1_u64 << (lane.lane - 1);
            local.totals.bytes_in_flight += totals.bytes_in_flight;
            local.totals.rtt_ns = local.totals.rtt_ns.max(totals.rtt_ns);
            self.counters.bytes_in_flight += totals.bytes_in_flight;
            self.counters.rtt_ns = self.counters.rtt_ns.max(totals.rtt_ns);
        }
        if let Some(owner) = &self.sender.owner {
            for group in 0..self.shared.config.path_groups() {
                if let Some(snapshot) = owner.snapshot(group) {
                    self.counters.pacing_bytes_per_second = self
                        .counters
                        .pacing_bytes_per_second
                        .saturating_add(snapshot.pacing_bytes_per_second);
                }
            }
            for (session, local) in self.sessions.iter_mut().enumerate() {
                if let Some(snapshot) = self
                    .shared
                    .config
                    .peer_for_session(session)
                    .and_then(|peer| self.shared.config.path_group_for_peer(peer))
                    .and_then(|group| owner.snapshot(group))
                {
                    local.totals.pacing_bytes_per_second = snapshot.pacing_bytes_per_second;
                }
            }
        }
    }
}
