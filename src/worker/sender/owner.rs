//! Serializes grants and controller feedback without moving packet histories.
use super::*;
use crate::{
    bbr::Controller,
    loss::ProbeTimeout,
    pacing::{GrantError, Sender},
};

pub(super) const SAMPLE_SLOTS: usize = 65_535;

pub(crate) struct Request {
    pub flight: Box<Flight>,
    pub group: usize,
    pub deadline: Duration,
    pub probe: Option<u64>,
    pub mtu: u16,
    pub pmtu: bool,
}

struct LossBarrier {
    token: u64,
    before: Duration,
    force: bool,
    unsent: u64,
    pending: u64,
    next_due: Option<Duration>,
    dirty: bool,
}

struct Group {
    controller: Controller,
    rtt: Rtt,
    pto: ProbeTimeout,
    epoch: u64,
    ack: Option<(usize, PacketId)>,
    cwnd_limited: bool,
    recovery: Option<Duration>,
    loss_serial: u64,
    check: Option<LossBarrier>,
    probe: Option<(Option<PacketId>, u8)>,
}

pub(in crate::worker) struct Owner {
    sender: Sender,
    pub(super) timers: timers::Timers,
    groups: Vec<Group>,
    requests: Vec<Option<Request>>,
    reports: VecDeque<Box<Report>>,
    reclaim: u64,
    reclaim_due: Duration,
    cursor: usize,
    round_due: Option<Duration>,
    rescan: bool,
    request_cursor: usize,
    pub(in crate::worker) due: Option<Duration>,
}

impl Owner {
    pub(super) fn reidentify(&mut self, flight: &mut Flight, first: u64) -> Result<(), GrantError> {
        self.sender.reidentify(flight, first)
    }

    pub(super) fn new(
        groups: usize,
        workers: usize,
        descriptors: &[(usize, u16, usize)],
        now: Duration,
    ) -> io::Result<(Self, Box<[Box<Flight>]>)> {
        let (sender, flights) = Sender::new(groups, workers, SAMPLE_SLOTS)?;
        let mut states = Vec::new();
        states.try_reserve_exact(groups).map_err(io::Error::other)?;
        for group in 0..groups {
            states.push(Group {
                controller: Controller::new(1280, None, now, group as u64 + 1)
                    .map_err(io::Error::other)?,
                rtt: Rtt::new(),
                pto: ProbeTimeout::new(),
                epoch: 0,
                ack: None,
                cwnd_limited: false,
                recovery: None,
                loss_serial: 0,
                check: None,
                probe: None,
            });
        }
        let mut requests = Vec::new();
        requests
            .try_reserve_exact(workers)
            .map_err(io::Error::other)?;
        requests.resize_with(workers, || None);
        let mut reports = VecDeque::new();
        reports
            .try_reserve_exact(workers)
            .map_err(io::Error::other)?;
        Ok((
            Self {
                sender,
                timers: timers::Timers::new(groups, descriptors)?,
                groups: states,
                requests,
                reports,
                reclaim: 0,
                reclaim_due: Duration::ZERO,
                cursor: 0,
                round_due: None,
                rescan: false,
                request_cursor: 0,
                due: None,
            },
            flights,
        ))
    }

    pub(super) fn storage_bytes(
        groups: usize,
        workers: usize,
        descriptors: &[(usize, u16, usize)],
    ) -> Option<usize> {
        size_of::<Self>()
            .checked_add(
                Sender::storage_bytes(groups, workers, SAMPLE_SLOTS)?
                    .checked_sub(size_of::<Sender>())?,
            )?
            .checked_add(
                timers::Timers::storage_bytes(groups, descriptors)?
                    .checked_sub(size_of::<timers::Timers>())?,
            )?
            .checked_add(groups.checked_mul(size_of::<Group>())?)?
            .checked_add(
                workers.checked_mul(size_of::<Option<Request>>() + size_of::<Box<Report>>())?,
            )
    }

    pub(super) fn request(&mut self, request: Request) -> io::Result<()> {
        let slot = self
            .requests
            .get_mut(request.flight.worker())
            .ok_or(io::ErrorKind::InvalidInput)?;
        if slot.is_some() || request.group >= self.groups.len() {
            return Err(io::Error::other("duplicate sender request"));
        }
        *slot = Some(request);
        Ok(())
    }

    pub(super) fn complete(&mut self, flight: Box<Flight>) -> io::Result<()> {
        let sent = flight.completion().is_some_and(|(count, _, _)| count != 0);
        let group = flight.group().ok_or(io::ErrorKind::InvalidInput)?;
        self.sender
            .reported(flight)
            .map_err(|(error, _)| io::Error::other(format!("sender completion: {error}")))?;
        let (_, inflight) = self
            .sender
            .flight_bytes(group)
            .ok_or(io::ErrorKind::InvalidData)?;
        let state = &mut self.groups[group];
        let limits = state.controller.limits();
        state.cwnd_limited |= sent
            && inflight != 0
            && limits.window.saturating_sub(inflight) < u64::from(limits.max_datagram);
        self.rescan |= group < self.cursor;
        Ok(())
    }

    pub(super) fn report(&mut self, report: Box<Report>) -> io::Result<()> {
        if self.reports.len() == self.reports.capacity() {
            return Err(io::Error::other("duplicate sender feedback"));
        }
        self.sender.close(report.group).map_err(io::Error::other)?;
        if matches!(report.kind, ReportKind::Ack { .. })
            && let Some(check) = &mut self.groups[report.group].check
            && check.pending & (1_u64 << report.origin) == 0
        {
            check.dirty = true;
        }
        self.rescan |= report.group < self.cursor;
        self.reports.push_back(report);
        Ok(())
    }

    pub(super) fn snapshot(&self, group: usize) -> Option<Snapshot> {
        let state = self.groups.get(group)?;
        Some(Snapshot {
            rtt_ns: state.rtt.minimum().map_or(0, |_| {
                state.rtt.smoothed().as_nanos().min(u64::MAX as u128) as u64
            }),
            pacing_bytes_per_second: state.controller.limits().rate,
            bytes_in_flight: self.sender.flight_bytes(group)?.1,
            ..Snapshot::default()
        })
    }

    pub(super) fn probe_replied(&mut self, group: usize) -> io::Result<()> {
        self.groups
            .get_mut(group)
            .ok_or(io::ErrorKind::InvalidInput)?
            .pto
            .probe_replied();
        Ok(())
    }

    /// Returns an unrequested part of one locally owned PTO command.
    pub(super) fn return_probe(&mut self, group: usize, epoch: u64, count: u8) -> io::Result<()> {
        let state = self
            .groups
            .get_mut(group)
            .ok_or(io::ErrorKind::InvalidInput)?;
        if epoch != state.epoch {
            return Ok(());
        }
        let pending = state.probe.map_or(0, |(_, count)| count);
        let total = pending.checked_add(count).filter(|&total| total <= 2);
        if !(1..=2).contains(&count) || total.is_none() {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        state.probe = Some((None, total.ok_or(io::ErrorKind::InvalidInput)?));
        self.rescan |= group < self.cursor;
        Ok(())
    }

    fn start_loss_check(&mut self, group: usize, before: Duration, force: bool) -> io::Result<()> {
        self.sender.close(group).map_err(io::Error::other)?;
        let state = &mut self.groups[group];
        state.loss_serial = state
            .loss_serial
            .checked_add(1)
            .ok_or(io::ErrorKind::Other)?;
        let mask = u64::MAX >> (64 - self.requests.len());
        state.check = Some(LossBarrier {
            token: state.loss_serial,
            before,
            force,
            unsent: mask,
            pending: mask,
            next_due: None,
            dirty: false,
        });
        Ok(())
    }

    pub(super) fn loss_checked(
        &mut self,
        group: usize,
        token: u64,
        worker: usize,
        next_due: Option<Duration>,
    ) -> io::Result<()> {
        if worker >= self.requests.len() {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let state = self
            .groups
            .get_mut(group)
            .ok_or(io::ErrorKind::InvalidInput)?;
        let Some(check) = &mut state.check else {
            return Ok(());
        };
        if token != check.token {
            return Ok(());
        }
        let bit = 1_u64 << worker;
        if check.unsent & bit != 0 || check.pending & bit == 0 {
            return Err(io::Error::other("unexpected loss-check completion"));
        }
        check.pending &= !bit;
        check.next_due = match (check.next_due, next_due) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        self.rescan |= group < self.cursor;
        Ok(())
    }

    fn advance_pto(
        &mut self,
        group: usize,
        now: Duration,
        outgoing: &mut VecDeque<(usize, Message)>,
    ) -> io::Result<bool> {
        let latest = self.timers.earliest(group);
        let due = self.groups[group]
            .pto
            .deadline(&self.groups[group].rtt, latest.map(|(_, at)| at));
        let mut more = false;
        if self.groups[group].check.is_none() && due.is_some_and(|at| at <= now) {
            self.start_loss_check(group, now, false)?;
            more = true;
        }
        if let Some(check) = &self.groups[group].check
            && check.pending == 0
        {
            let force = check.force;
            let repeat = check.dirty || check.next_due.is_some_and(|at| at <= now);
            self.groups[group].check = None;
            if !force && due.is_some_and(|at| at <= now) {
                if repeat {
                    self.start_loss_check(group, now, false)?;
                } else {
                    let state = &mut self.groups[group];
                    if let Some(expiry) =
                        state.pto.expire(now, &state.rtt, latest.map(|(_, at)| at))
                    {
                        state.epoch = state.epoch.checked_add(1).ok_or(io::ErrorKind::Other)?;
                        state.probe = Some((
                            latest
                                .map(|(id, _)| id)
                                .or_else(|| self.timers.installed(group)),
                            expiry.probe_limit,
                        ));
                        if expiry.declare_lost {
                            self.start_loss_check(group, now, true)?;
                        } else {
                            self.sender
                                .probe_allowance(group, expiry.probe_limit)
                                .map_err(io::Error::other)?;
                        }
                    }
                }
            } else if force && let Some((_, count)) = self.groups[group].probe {
                self.sender
                    .probe_allowance(group, count)
                    .map_err(io::Error::other)?;
            }
            more = true;
        }
        if let Some(check) = &mut self.groups[group].check {
            if self.sender.feedback_ready(group)
                && check.unsent != 0
                && outgoing.len() < outgoing.capacity().saturating_sub(6 * self.requests.len())
            {
                let worker = check.unsent.trailing_zeros() as usize;
                outgoing.push_back((
                    worker,
                    Message::CheckLoss {
                        group,
                        token: check.token,
                        before: check.before,
                        force: check.force,
                    },
                ));
                check.unsent &= !(1_u64 << worker);
                more = true;
            }
            // Completions and returned reports wake the owner. An expired PTO must not spin.
            return Ok(more);
        }
        if self.sender.feedback_ready(group) {
            self.sender.reopen(group).map_err(io::Error::other)?;
        }
        let state = &mut self.groups[group];
        if let Some(at) = state.pto.deadline(&state.rtt, latest.map(|(_, at)| at)) {
            self.round_due = Some(self.round_due.map_or(at, |old| old.min(at)));
        }
        if outgoing.len() < outgoing.capacity().saturating_sub(6 * self.requests.len())
            && let Some((target, count)) = state.probe
            && let Some(id) = target
                .filter(|id| self.timers.matches(*id))
                .or_else(|| self.timers.installed(group))
        {
            let worker = (usize::from(id.lane) - 1) % self.requests.len();
            let preempt = self.requests[worker]
                .as_ref()
                .is_some_and(|r| r.probe.is_none());
            if outgoing.capacity() - outgoing.len() < 1 + usize::from(preempt) {
                return Ok(more);
            }
            if preempt && let Some(request) = self.requests[worker].take() {
                outgoing.push_back((worker, Message::Recycle(request.flight)));
            }
            state.probe = None;
            outgoing.push_back((
                worker,
                Message::Probe {
                    group,
                    epoch: state.epoch,
                    session: id.session,
                    generation: id.generation,
                    lane: id.lane,
                    count,
                },
            ));
            more = true;
        }
        Ok(more)
    }

    // All producers retain their pooled message when the handoff queue is full.
    pub(super) fn advance(
        &mut self,
        now: Duration,
        outgoing: &mut VecDeque<(usize, Message)>,
    ) -> io::Result<bool> {
        let mut more = false;
        let mut work = BATCH_SIZE;
        let visits = (self.groups.len() - self.cursor).min(BATCH_SIZE);
        for _ in 0..visits {
            if work == 0 || outgoing.len() == outgoing.capacity() {
                more = true;
                break;
            }
            let group = self.cursor;
            self.cursor = (self.cursor + 1) % self.groups.len();
            while work != 0 && outgoing.len() < outgoing.capacity() {
                let Some(flight) = self.sender.advance(group, work).map_err(io::Error::other)?
                else {
                    if self.sender.commit_pending(group) {
                        work = 0;
                    }
                    break;
                };
                let count = flight.packets().len();
                work = work.saturating_sub(count.max(1));
                if let Some(packet) = flight.packets().next() {
                    let (_, start, _) = flight.completion().ok_or(io::ErrorKind::InvalidData)?;
                    self.timers.submitted(
                        packet.id.session,
                        packet.id.lane,
                        packet.id.generation,
                        count as u32,
                        start.ok_or(io::ErrorKind::InvalidData)?,
                    )?;
                }
                outgoing.push_back((flight.worker(), Message::Recycle(flight)));
                more = true;
            }
            more |= self.sender.commit_pending(group);
            if self.sender.feedback_ready(group) {
                while work != 0 && outgoing.len() < outgoing.capacity() {
                    let active = self.groups[group].ack;
                    let Some(index) = self.reports.iter().position(|report| {
                        report.group == group
                            && active.is_none_or(|id| id == (report.origin, report.id))
                    }) else {
                        break;
                    };
                    let cost = self.reports[index].count.max(1);
                    if cost > work {
                        more = true;
                        break;
                    }
                    let report = self
                        .reports
                        .remove(index)
                        .ok_or(io::ErrorKind::InvalidData)?;
                    work -= cost;
                    self.apply_report(&report, now)?;
                    outgoing.push_back((report.origin, Message::ReportReturn(report)));
                    more = true;
                }
                if self.groups[group].ack.is_none()
                    && self.groups[group].check.is_none()
                    && !self.reports.iter().any(|r| r.group == group)
                {
                    self.sender.reopen(group).map_err(io::Error::other)?;
                } else {
                    more |= self.groups[group].ack.is_none()
                        && self.reports.iter().any(|r| r.group == group);
                }
            }
            // A feedback transaction takes precedence over PTO and cannot lose its ACKed entries.
            if self.groups[group].ack.is_none() && !self.reports.iter().any(|r| r.group == group) {
                more |= self.advance_pto(group, now, outgoing)?;
            }
        }
        // Complete one bounded sweep before sleeping or replacing its minimum.
        if self.cursor == 0 {
            self.due = self.round_due.take();
            more |= std::mem::take(&mut self.rescan);
        } else {
            more = true;
        }
        if self.sender.available() < BATCH_SIZE && now >= self.reclaim_due {
            self.reclaim = u64::MAX >> (64 - self.requests.len());
            self.reclaim_due = now.saturating_add(RETRY);
        }
        if self.reclaim != 0
            && outgoing.len() < outgoing.capacity().saturating_sub(6 * self.requests.len())
        {
            let worker = self.reclaim.trailing_zeros() as usize;
            self.reclaim &= self.reclaim - 1;
            outgoing.push_back((worker, Message::Reclaim));
            more = true;
        }
        if self.sender.available() < BATCH_SIZE {
            self.due = Some(
                self.due
                    .map_or(self.reclaim_due, |due| due.min(self.reclaim_due)),
            );
        }
        let mut granted_next = None;
        for _ in 0..self.requests.len().min(BATCH_SIZE) {
            if work == 0 || outgoing.len() == outgoing.capacity() {
                break;
            }
            let worker = self.request_cursor;
            self.request_cursor = (self.request_cursor + 1) % self.requests.len();
            let Some(mut request) = self.requests[worker].take() else {
                continue;
            };
            work -= 1;
            let id = request
                .flight
                .packets()
                .next()
                .ok_or(io::ErrorKind::InvalidData)?
                .id;
            let state = &mut self.groups[request.group];
            if now >= request.deadline
                || !self.timers.matches(id)
                || request.probe.is_some_and(|epoch| epoch != state.epoch)
            {
                outgoing.push_back((worker, Message::Recycle(request.flight)));
                more = true;
                continue;
            }
            let (reserved, inflight) = self
                .sender
                .flight_bytes(request.group)
                .ok_or(io::ErrorKind::InvalidData)?;
            state
                .controller
                .set_max_datagram(request.mtu)
                .map_err(io::Error::other)?;
            let app_limited = self.sender.is_app_limited(request.group).unwrap_or(false);
            if inflight == 0 && app_limited {
                state.cwnd_limited = false;
            }
            state
                .controller
                .on_transmit(now, inflight, app_limited)
                .map_err(io::Error::other)?;
            let mut limits = state.controller.limits();
            if state.pto.stalled() {
                limits.window = 0;
            }
            let burst = limits.quantum.min(64 * u32::from(limits.max_datagram));
            // A probe must fit the whole window, not just the pacing burst.
            // A stalled zero window is temporary and cannot constrain path discovery.
            let probe_budget = if limits.window == 0 {
                burst
            } else {
                u64::from(burst).min(limits.window) as u32
            };
            if request.pmtu
                && request
                    .flight
                    .packets()
                    .next()
                    .is_some_and(|p| u32::from(p.bytes) > probe_budget)
            {
                outgoing.push_back((
                    worker,
                    Message::Resize {
                        flight: request.flight,
                        payload: probe_budget.saturating_sub(48) as usize,
                    },
                ));
                more = true;
                continue;
            }
            let available = limits
                .window
                .saturating_sub(inflight)
                .saturating_sub(reserved);
            let mut budget = u64::from(limits.quantum.min(64 * u32::from(limits.max_datagram)))
                .saturating_sub(reserved);
            if request.probe.is_none() {
                budget = budget.min(available);
            }
            let mut bytes = 0;
            for packet in request.flight.packets().take(self.sender.available()) {
                let next = bytes + u32::from(packet.bytes);
                if u64::from(next) > budget {
                    break;
                }
                bytes = next;
            }
            let result = self.sender.reserve_prefix(
                request.group,
                &mut request.flight,
                bytes,
                request.probe.is_some(),
                limits,
                now,
            );
            match result {
                Ok(()) => {
                    outgoing.push_back((worker, Message::Permit(request.flight)));
                    granted_next.get_or_insert(self.request_cursor);
                    more = true;
                }
                Err(GrantError::Blocked) if request.pmtu => {
                    outgoing.push_back((worker, Message::Recycle(request.flight)));
                    more = true;
                }
                Err(GrantError::Blocked) => {
                    if let Some(at) = self.sender.deadline(request.group, now) {
                        self.due = Some(self.due.map_or(at, |old| old.min(at)));
                    }
                    self.due = Some(
                        self.due
                            .map_or(request.deadline, |old| old.min(request.deadline)),
                    );
                    self.requests[worker] = Some(request);
                }
                Err(error) => {
                    return Err(io::Error::other(error));
                }
            }
        }
        // A full scan must not give the same ready worker first refusal again.
        if let Some(next) = granted_next {
            self.request_cursor = next;
        }
        Ok(more)
    }

    fn apply_report(&mut self, report: &Report, now: Duration) -> io::Result<()> {
        let group = report.group;
        let state = self
            .groups
            .get_mut(group)
            .ok_or(io::ErrorKind::InvalidInput)?;
        if let ReportKind::Ack { first: true, .. } = report.kind {
            if state.ack.is_some() {
                return Err(io::ErrorKind::InvalidData.into());
            }
            self.sender
                .begin_ack(group, now, false)
                .map_err(io::Error::other)?;
            state.ack = Some((report.origin, report.id));
        }
        let mut settled = 0;
        for entry in &report.entries[..report.count] {
            settled += u32::from(entry.settled);
            let Some(sample) = entry.sample else { continue };
            let id = PacketId {
                number: entry.number,
                ..report.id
            };
            match report.kind {
                ReportKind::Ack { .. } => self
                    .sender
                    .acknowledged(group, id, sample)
                    .map_err(io::Error::other)?,
                ReportKind::Loss => {
                    if let Some(loss) = self
                        .sender
                        .lost(group, id, sample)
                        .map_err(io::Error::other)?
                    {
                        if state.recovery.is_none() {
                            let (_, inflight) = self
                                .sender
                                .flight_bytes(group)
                                .ok_or(io::ErrorKind::InvalidData)?;
                            state
                                .controller
                                .enter_recovery(now, inflight, state.pto.stalled())
                                .map_err(io::Error::other)?;
                            state.recovery = Some(now);
                        }
                        state
                            .controller
                            .on_loss(loss, now)
                            .map_err(io::Error::other)?;
                    }
                }
                ReportKind::Retire => self
                    .sender
                    .retired(group, id, sample)
                    .map_err(io::Error::other)?,
            }
        }
        if settled != 0 {
            self.timers.settled(
                report.id.session,
                report.id.lane,
                report.id.generation,
                settled,
            )?;
        }
        if let ReportKind::Ack {
            received,
            delay_us,
            end: Some(progress),
            ..
        } = report.kind
        {
            if let Some(sent) = progress.rtt_sent_at {
                state.rtt.sample(sent, received, delay_us);
                if state.recovery.is_some_and(|start| sent > start) {
                    state
                        .controller
                        .recovery_finished(now, false)
                        .map_err(io::Error::other)?;
                    state.recovery = None;
                }
            }
            if progress.eliciting {
                state.pto.acknowledged(true);
                state.epoch = state.epoch.checked_add(1).ok_or(io::ErrorKind::Other)?;
                state.probe = None;
                self.sender
                    .probe_allowance(group, 0)
                    .map_err(io::Error::other)?;
            }
            if let Some(sample) = self
                .sender
                .finish_ack(group, state.rtt.minimum().unwrap_or(Duration::ZERO))
                .map_err(io::Error::other)?
            {
                let (_, inflight) = self
                    .sender
                    .flight_bytes(group)
                    .ok_or(io::ErrorKind::InvalidData)?;
                let rtt = progress
                    .rtt_sent_at
                    .and_then(|sent| received.checked_sub(sent));
                if state
                    .controller
                    .on_ack(sample, rtt, inflight, state.cwnd_limited, now)
                    .map_err(io::Error::other)?
                {
                    self.sender
                        .application_limited(group)
                        .map_err(io::Error::other)?;
                }
                state.cwnd_limited = false;
            }
            state.ack = None;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_ack_uses_controller_creation_time_at_any_uptime() {
        for start in [Duration::ZERO, Duration::from_secs(86_400)] {
            let (mut owner, _) = Owner::new(1, 1, &[(0, 1, 0)], start).unwrap();
            let mut delivery = crate::bbr::Delivery::new();
            let mut packet = delivery.sent(start, 1280, 0, false).unwrap();
            let rtt = Duration::from_millis(1);
            delivery.begin_ack(start + rtt, false).unwrap();
            delivery.acknowledged(&mut packet).unwrap();
            let sample = delivery.finish_ack(rtt).unwrap().unwrap();
            let controller = &mut owner.groups[0].controller;
            assert!(
                !controller
                    .on_ack(sample, Some(rtt), 0, false, start + rtt)
                    .unwrap()
            );
            assert_eq!(controller.state(), crate::bbr::State::Startup);
        }
    }

    fn request(flight: Box<Flight>, lane: u16, count: usize) -> Request {
        let mut flight = flight;
        let packets: Vec<_> = (1..=count)
            .map(|number| pacing::Planned {
                id: PacketId {
                    session: 0,
                    generation: 1,
                    lane,
                    number: number as u64,
                },
                bytes: 1280,
            })
            .collect();
        flight.prepare(&packets).unwrap();
        Request {
            flight,
            group: 0,
            deadline: Duration::from_millis(100),
            probe: None,
            mtu: 1280,
            pmtu: false,
        }
    }

    #[test]
    fn whole_prefix_window_pressure_keeps_deadline_and_recycles_expired_request() {
        let (mut owner, flights) =
            Owner::new(1, 2, &[(0, 1, 0), (0, 2, 0)], Duration::ZERO).unwrap();
        owner.timers.install(0, 1).unwrap();
        let mut flights = flights.into_vec().into_iter();
        let mut outgoing = VecDeque::with_capacity(16);
        owner
            .request(request(flights.next().unwrap(), 1, 64))
            .unwrap();
        owner.advance(Duration::ZERO, &mut outgoing).unwrap();
        let (_, Message::Permit(mut flight)) = outgoing.pop_front().unwrap() else {
            panic!()
        };
        assert_eq!(flight.packets().len(), 10);
        assert!(
            !owner.groups[0].cwnd_limited,
            "reservations are not successful window use"
        );
        flight.complete(10, Some(Duration::ZERO), false).unwrap();
        owner.complete(flight).unwrap();
        assert!(
            owner.groups[0].cwnd_limited,
            "the final successful grant fills the window"
        );
        owner
            .advance(Duration::from_millis(1), &mut outgoing)
            .unwrap();
        assert!(matches!(
            outgoing.pop_front().unwrap().1,
            Message::Recycle(_)
        ));
        assert_eq!(owner.sender.flight_bytes(0), Some((0, 12800)));
        owner
            .request(request(flights.next().unwrap(), 2, 1))
            .unwrap();
        owner
            .advance(Duration::from_millis(1), &mut outgoing)
            .unwrap();
        assert!(outgoing.is_empty());
        assert_eq!(owner.due, Some(Duration::from_millis(100)));
        owner
            .advance(Duration::from_millis(100), &mut outgoing)
            .unwrap();
        let (_, Message::Recycle(flight)) = outgoing.pop_front().unwrap() else {
            panic!()
        };
        assert!(flight.grant().is_none());
        assert_eq!(owner.sender.flight_bytes(0), Some((0, 12800)));
    }

    #[test]
    fn feedback_waits_for_commits_and_partial_ack_excludes_other_reports() {
        let (mut owner, flights) =
            Owner::new(1, 2, &[(0, 1, 0), (0, 2, 0)], Duration::ZERO).unwrap();
        owner.timers.install(0, 1).unwrap();
        let flight = flights.into_vec().remove(0);
        let mut outgoing = VecDeque::with_capacity(16);
        owner.request(request(flight, 1, 1)).unwrap();
        owner.advance(Duration::ZERO, &mut outgoing).unwrap();
        let (_, Message::Permit(mut flight)) = outgoing.pop_front().unwrap() else {
            panic!()
        };
        let packet = flight.packets().next().unwrap();
        let mut report = Box::new(Report::new(0));
        report.id = PacketId {
            number: 0,
            ..packet.id
        };
        report.kind = ReportKind::Ack {
            received: Duration::from_millis(1),
            delay_us: 0,
            first: true,
            end: None,
        };
        report.count = 1;
        report.entries[0] = Entry {
            number: 1,
            sample: flight.handle(0),
            settled: true,
        };
        owner.report(report).unwrap();
        owner
            .advance(Duration::from_millis(1), &mut outgoing)
            .unwrap();
        assert!(outgoing.is_empty());
        flight.complete(1, Some(Duration::ZERO), false).unwrap();
        owner.complete(flight).unwrap();
        owner
            .advance(Duration::from_millis(1), &mut outgoing)
            .unwrap();
        assert!(matches!(
            outgoing.pop_front().unwrap().1,
            Message::Recycle(_)
        ));
        let (_, Message::ReportReturn(mut report)) = outgoing.pop_front().unwrap() else {
            panic!()
        };
        assert_eq!(owner.sender.flight_bytes(0), Some((0, 0)));
        let mut other = Box::new(Report::new(1));
        other.id = PacketId {
            session: 0,
            generation: 1,
            lane: 2,
            number: 0,
        };
        other.kind = ReportKind::Loss;
        owner.report(other).unwrap();
        owner
            .advance(Duration::from_millis(2), &mut outgoing)
            .unwrap();
        assert!(outgoing.is_empty());
        report.count = 0;
        report.kind = ReportKind::Ack {
            received: Duration::from_millis(1),
            delay_us: 0,
            first: false,
            end: Some(AckProgress {
                packets: 1,
                eliciting: true,
                released_bytes: 1280,
                rtt_sent_at: Some(Duration::ZERO),
            }),
        };
        owner.report(report).unwrap();
        owner
            .advance(Duration::from_millis(2), &mut outgoing)
            .unwrap();
        assert_eq!(outgoing.pop_front().unwrap().0, 0);
        owner
            .advance(Duration::from_millis(2), &mut outgoing)
            .unwrap();
        assert_eq!(outgoing.pop_front().unwrap().0, 1);
        assert!(owner.groups[0].ack.is_none());
        assert_eq!(owner.sender.available(), SAMPLE_SLOTS);
        assert_eq!(owner.timers.earliest(0), None);
    }
}

#[cfg(test)]
mod scheduling_tests {
    use super::*;

    fn request(mut flight: Box<Flight>, deadline: Duration, pmtu: bool) -> Request {
        flight
            .prepare(&[pacing::Planned {
                id: PacketId {
                    session: 0,
                    generation: 1,
                    lane: 1,
                    number: 1,
                },
                bytes: 1280,
            }])
            .unwrap();
        Request {
            flight,
            group: 0,
            deadline,
            probe: None,
            mtu: 1280,
            pmtu,
        }
    }

    fn stalled(state: &mut Group) -> Duration {
        let mut now = Duration::ZERO;
        for _ in 0..3 {
            now = state
                .pto
                .deadline(&state.rtt, Some(Duration::ZERO))
                .unwrap();
            assert!(
                state
                    .pto
                    .expire(now, &state.rtt, Some(Duration::ZERO))
                    .is_some()
            );
        }
        assert!(state.pto.stalled());
        now
    }

    #[test]
    fn group_sweeps_preserve_earliest_deadline_then_remove_stale_minimum() {
        let (mut owner, _) = Owner::new(65, 1, &[(0, 1, 0), (1, 1, 64)], Duration::ZERO).unwrap();
        owner.timers.install(0, 1).unwrap();
        owner.timers.install(1, 1).unwrap();
        owner.timers.submitted(0, 1, 1, 1, Duration::ZERO).unwrap();
        owner
            .timers
            .submitted(1, 1, 1, 1, Duration::from_millis(100))
            .unwrap();
        let expected = owner.groups[0]
            .pto
            .deadline(&owner.groups[0].rtt, Some(Duration::ZERO));
        let mut outgoing = VecDeque::with_capacity(8);
        assert!(owner.advance(Duration::ZERO, &mut outgoing).unwrap());
        assert_eq!(owner.cursor, 64);
        assert!(!owner.advance(Duration::ZERO, &mut outgoing).unwrap());
        assert_eq!(owner.cursor, 0);
        assert_eq!(owner.due, expected);
        owner.timers.settled(0, 1, 1, 1).unwrap();
        owner.timers.settled(1, 1, 1, 1).unwrap();
        assert!(owner.advance(Duration::ZERO, &mut outgoing).unwrap());
        assert!(!owner.advance(Duration::ZERO, &mut outgoing).unwrap());
        assert!(owner.due.is_none());
    }

    #[test]
    fn completed_flight_behind_group_cursor_forces_another_sweep() {
        let (mut owner, flights) = Owner::new(65, 1, &[(0, 1, 0)], Duration::ZERO).unwrap();
        owner.timers.install(0, 1).unwrap();
        owner
            .request(request(
                flights.into_vec().pop().unwrap(),
                Duration::from_secs(1),
                false,
            ))
            .unwrap();
        let mut outgoing = VecDeque::with_capacity(8);
        assert!(owner.advance(Duration::ZERO, &mut outgoing).unwrap());
        assert_eq!(owner.cursor, 64);
        let (_, Message::Permit(mut flight)) = outgoing.pop_front().unwrap() else {
            panic!()
        };
        flight.complete(1, Some(Duration::ZERO), false).unwrap();
        owner.complete(flight).unwrap();
        assert!(owner.advance(Duration::ZERO, &mut outgoing).unwrap());
        assert_eq!(owner.cursor, 0);
        assert!(outgoing.is_empty());
        assert!(owner.advance(Duration::ZERO, &mut outgoing).unwrap());
        assert!(matches!(
            outgoing.pop_front().unwrap().1,
            Message::Recycle(_)
        ));
        assert_eq!(owner.sender.flight_bytes(0), Some((0, 1280)));
    }

    #[test]
    fn pending_pto_without_target_survives_install_and_preempts_ordinary_request() {
        let (mut owner, flights) = Owner::new(1, 1, &[(0, 1, 0)], Duration::ZERO).unwrap();
        let now = stalled(&mut owner.groups[0]);
        owner.groups[0].epoch = 7;
        owner.groups[0].probe = Some((None, 2));
        owner.sender.probe_allowance(0, 2).unwrap();
        let next = owner.groups[0].pto.deadline(&owner.groups[0].rtt, None);
        let mut outgoing = VecDeque::with_capacity(8);
        assert!(!owner.advance(now, &mut outgoing).unwrap());
        assert!(outgoing.is_empty());
        assert_eq!(owner.groups[0].probe, Some((None, 2)));
        owner.timers.install(0, 1).unwrap();
        owner
            .request(request(
                flights.into_vec().pop().unwrap(),
                now + Duration::from_secs(1),
                false,
            ))
            .unwrap();
        assert!(owner.advance(now, &mut outgoing).unwrap());
        assert!(matches!(
            outgoing.pop_front().unwrap().1,
            Message::Recycle(_)
        ));
        let (
            _,
            Message::Probe {
                generation,
                epoch,
                count,
                ..
            },
        ) = outgoing.pop_front().unwrap()
        else {
            panic!()
        };
        assert_eq!((generation, epoch, count), (1, 7, 2));
        assert!(owner.requests[0].is_none());
        assert_eq!(
            owner.groups[0].pto.deadline(&owner.groups[0].rtt, None),
            next
        );
        assert!(owner.groups[0].pto.stalled());
    }

    #[test]
    fn upward_probe_fits_total_window_without_raising_confirmed_mtu() {
        let (mut owner, flights) = Owner::new(1, 1, &[(0, 1, 0)], Duration::ZERO).unwrap();
        owner.timers.install(0, 1).unwrap();
        owner.groups[0]
            .controller
            .enter_recovery(Duration::ZERO, 3 * 1280, true)
            .unwrap();
        let limits = owner.groups[0].controller.limits();
        assert_eq!(limits.window, 5120);
        assert!(limits.quantum > 5140);
        let mut request = request(
            flights.into_vec().pop().unwrap(),
            Duration::from_secs(1),
            true,
        );
        let mut packet = request.flight.packets().next().unwrap();
        packet.bytes = 5140;
        request.flight.prepare(&[packet]).unwrap();
        owner.request(request).unwrap();
        let mut outgoing = VecDeque::with_capacity(8);
        owner.advance(Duration::ZERO, &mut outgoing).unwrap();
        let (
            _,
            Message::Resize {
                mut flight,
                payload,
            },
        ) = outgoing.pop_front().unwrap()
        else {
            panic!("probe larger than the whole window must be resized");
        };
        assert_eq!(payload, 5072);
        assert_eq!(owner.groups[0].controller.limits().max_datagram, 1280);
        assert_eq!(owner.sender.flight_bytes(0), Some((0, 0)));
        let mut path = pmtu::Path::new(crate::socket::MAX_UDP_PAYLOAD, Duration::ZERO).unwrap();
        assert!(path.submitted(
            pmtu::ProbeId {
                session: 0,
                tag: 1,
                lane: 1,
                packet: 1,
            },
            BASE_UDP_PAYLOAD,
            Duration::ZERO
        ));
        let now = Duration::from_millis(1);
        assert!(path.reply(
            pmtu::ProbeId {
                session: 0,
                tag: 1,
                lane: 1,
                packet: 1,
            },
            BASE_UDP_PAYLOAD,
            now
        ));
        assert_eq!(path.poll(now), Some(5092));
        assert!(path.limit_probe(payload, now));
        assert_eq!(path.payload_limit(), BASE_UDP_PAYLOAD);
        assert_eq!(path.poll(now), Some(payload));
        packet.id.number += 1;
        packet.bytes = (payload + 48) as u16;
        flight.prepare(&[packet]).unwrap();
        owner
            .request(Request {
                flight,
                group: 0,
                deadline: Duration::from_secs(1),
                probe: None,
                mtu: 1280,
                pmtu: true,
            })
            .unwrap();
        owner.advance(now, &mut outgoing).unwrap();
        assert!(matches!(
            outgoing.pop_front().unwrap().1,
            Message::Permit(_)
        ));
        assert_eq!(owner.sender.flight_bytes(0), Some((5120, 0)));
    }

    #[test]
    fn blocked_pmtu_recycles_but_stalled_data_waits_for_a_probe_event() {
        for pmtu in [false, true] {
            let (mut owner, flights) = Owner::new(1, 1, &[(0, 1, 0)], Duration::ZERO).unwrap();
            owner.timers.install(0, 1).unwrap();
            let now = stalled(&mut owner.groups[0]);
            owner
                .request(request(
                    flights.into_vec().pop().unwrap(),
                    now + Duration::from_secs(1),
                    pmtu,
                ))
                .unwrap();
            let mut outgoing = VecDeque::with_capacity(8);
            let more = owner.advance(now, &mut outgoing).unwrap();
            assert_eq!(more, pmtu);
            assert_eq!(owner.requests[0].is_none(), pmtu);
            if pmtu {
                assert!(matches!(
                    outgoing.pop_front().unwrap().1,
                    Message::Recycle(_)
                ));
            } else {
                assert!(outgoing.is_empty());
            }
        }
    }
}

#[cfg(test)]
mod returned_probe_tests {
    use super::*;

    #[test]
    fn returned_probe_retargets_replacement_without_resetting_epoch_or_backoff() {
        let (mut owner, _) = Owner::new(1, 1, &[(0, 1, 0)], Duration::ZERO).unwrap();
        owner.timers.install(0, 1).unwrap();
        let state = &mut owner.groups[0];
        let mut now = Duration::ZERO;
        for _ in 0..3 {
            now = state
                .pto
                .deadline(&state.rtt, Some(Duration::ZERO))
                .unwrap();
            assert!(
                state
                    .pto
                    .expire(now, &state.rtt, Some(Duration::ZERO))
                    .is_some()
            );
        }
        state.epoch = 9;
        state.probe = Some((None, 2));
        let deadline = state.pto.deadline(&state.rtt, None);
        owner.sender.probe_allowance(0, 2).unwrap();
        let mut outgoing = VecDeque::with_capacity(8);
        owner.advance(now, &mut outgoing).unwrap();
        assert!(matches!(
            outgoing.pop_front().unwrap().1,
            Message::Probe {
                generation: 1,
                count: 2,
                epoch: 9,
                ..
            }
        ));
        owner.timers.quiesce(0, 1).unwrap();
        owner.return_probe(0, 9, 2).unwrap();
        assert!(!owner.advance(now, &mut outgoing).unwrap());
        assert!(outgoing.is_empty());
        owner.timers.install(0, 2).unwrap();
        owner.advance(now, &mut outgoing).unwrap();
        assert!(matches!(
            outgoing.pop_front().unwrap().1,
            Message::Probe {
                generation: 2,
                count: 2,
                epoch: 9,
                ..
            }
        ));
        assert_eq!(
            owner.groups[0].pto.deadline(&owner.groups[0].rtt, None),
            deadline
        );
        assert!(owner.groups[0].pto.stalled());
        assert_eq!(owner.groups[0].epoch, 9);
        owner.return_probe(0, 8, 2).unwrap();
        assert!(owner.groups[0].probe.is_none());
    }

    #[test]
    fn returned_probe_counts_merge_within_one_epoch_and_reject_overflow_atomically() {
        let (mut owner, _) = Owner::new(1, 1, &[(0, 1, 0)], Duration::ZERO).unwrap();
        owner.groups[0].epoch = 3;
        for count in [0, 3, u8::MAX] {
            assert!(owner.return_probe(0, 3, count).is_err());
            assert!(owner.groups[0].probe.is_none());
        }
        owner.return_probe(0, 3, 1).unwrap();
        owner.return_probe(0, 3, 1).unwrap();
        assert_eq!(owner.groups[0].probe, Some((None, 2)));
        assert!(owner.return_probe(0, 3, 1).is_err());
        assert_eq!(owner.groups[0].probe, Some((None, 2)));
        owner.return_probe(0, 2, 2).unwrap();
        assert_eq!(owner.groups[0].probe, Some((None, 2)));
    }
}

#[cfg(test)]
mod loss_barrier_tests {
    use super::*;

    fn setup() -> (Owner, VecDeque<(usize, Message)>) {
        let (mut owner, _) = Owner::new(1, 1, &[(0, 1, 0)], Duration::ZERO).unwrap();
        owner.timers.install(0, 1).unwrap();
        owner.timers.submitted(0, 1, 1, 1, Duration::ZERO).unwrap();
        (owner, VecDeque::with_capacity(8))
    }

    fn check(
        owner: &mut Owner,
        outgoing: &mut VecDeque<(usize, Message)>,
        now: Duration,
    ) -> (u64, bool) {
        for _ in 0..3 {
            owner.advance(now, outgoing).unwrap();
            if let Some((0, Message::CheckLoss { token, force, .. })) = outgoing.pop_front() {
                return (token, force);
            }
        }
        panic!("missing loss check");
    }

    fn settle(owner: &mut Owner, outgoing: &mut VecDeque<(usize, Message)>, now: Duration) {
        let mut report = Box::new(Report::new(0));
        report.id = PacketId {
            session: 0,
            generation: 1,
            lane: 1,
            number: 0,
        };
        report.kind = ReportKind::Loss;
        report.count = 1;
        report.entries[0] = Entry {
            number: 1,
            sample: None,
            settled: true,
        };
        owner.report(report).unwrap();
        owner.advance(now, outgoing).unwrap();
        assert!(matches!(
            outgoing.pop_front().unwrap().1,
            Message::ReportReturn(_)
        ));
    }

    #[test]
    fn time_loss_settlement_removes_pto_and_waiting_does_not_spin() {
        let (mut owner, mut outgoing) = setup();
        let now = owner.groups[0]
            .pto
            .deadline(&owner.groups[0].rtt, Some(Duration::ZERO))
            .unwrap();
        let (token, force) = check(&mut owner, &mut outgoing, now);
        assert!(!force);
        assert!(!owner.advance(now, &mut outgoing).unwrap());
        assert!(outgoing.is_empty());
        assert_eq!(owner.due, None);
        settle(&mut owner, &mut outgoing, now);
        assert!(!owner.advance(now, &mut outgoing).unwrap());
        owner.loss_checked(0, token, 0, None).unwrap();
        owner.advance(now, &mut outgoing).unwrap();
        assert!(outgoing.is_empty());
        assert!(owner.groups[0].check.is_none());
        assert!(!owner.groups[0].pto.stalled());
        assert_eq!(owner.groups[0].epoch, 0);
    }

    #[test]
    fn third_expiry_waits_for_forced_reports_and_worker_completion_before_probes() {
        let (mut owner, mut outgoing) = setup();
        for _ in 0..2 {
            let state = &mut owner.groups[0];
            let now = state
                .pto
                .deadline(&state.rtt, Some(Duration::ZERO))
                .unwrap();
            state
                .pto
                .expire(now, &state.rtt, Some(Duration::ZERO))
                .unwrap();
        }
        let now = owner.groups[0]
            .pto
            .deadline(&owner.groups[0].rtt, Some(Duration::ZERO))
            .unwrap();
        let (normal, force) = check(&mut owner, &mut outgoing, now);
        assert!(!force);
        owner.loss_checked(0, normal, 0, None).unwrap();
        let (forced, force) = check(&mut owner, &mut outgoing, now);
        assert!(force && forced > normal);
        assert!(owner.groups[0].pto.stalled());
        settle(&mut owner, &mut outgoing, now);
        assert!(!owner.advance(now, &mut outgoing).unwrap());
        assert!(outgoing.is_empty());
        owner.loss_checked(0, normal, 0, None).unwrap(); // old token cannot release the gate
        assert!(owner.groups[0].check.is_some());
        owner.loss_checked(0, forced, 0, None).unwrap();
        owner.advance(now, &mut outgoing).unwrap();
        assert!(matches!(
            outgoing.pop_front().unwrap().1,
            Message::Probe { count: 2, .. }
        ));
        assert!(owner.groups[0].check.is_none());
    }

    #[test]
    fn loss_deadline_crossed_in_transit_requires_another_bounded_check() {
        let (mut owner, mut outgoing) = setup();
        let now = owner.groups[0]
            .pto
            .deadline(&owner.groups[0].rtt, Some(Duration::ZERO))
            .unwrap();
        let (token, _) = check(&mut owner, &mut outgoing, now);
        owner
            .loss_checked(0, token, 0, Some(now + Duration::from_millis(1)))
            .unwrap();
        let (next, force) = check(&mut owner, &mut outgoing, now + Duration::from_millis(2));
        assert!(next > token && !force);
        assert_eq!(owner.groups[0].epoch, 0);
        assert!(owner.loss_checked(0, next, 0, None).is_ok());
        assert!(owner.loss_checked(0, next, 0, None).is_err());
    }

    #[test]
    fn existing_grant_must_commit_before_worker_snapshot_is_requested() {
        let (mut owner, flights) = Owner::new(1, 1, &[(0, 1, 0)], Duration::ZERO).unwrap();
        owner.timers.install(0, 1).unwrap();
        owner.timers.submitted(0, 1, 1, 1, Duration::ZERO).unwrap();
        let mut flight = flights.into_vec().pop().unwrap();
        flight
            .prepare(&[pacing::Planned {
                id: PacketId {
                    session: 0,
                    generation: 1,
                    lane: 1,
                    number: 2,
                },
                bytes: 1280,
            }])
            .unwrap();
        owner
            .sender
            .reserve(
                0,
                &mut flight,
                false,
                owner.groups[0].controller.limits(),
                Duration::ZERO,
            )
            .unwrap();
        let now = owner.groups[0]
            .pto
            .deadline(&owner.groups[0].rtt, Some(Duration::ZERO))
            .unwrap();
        let mut outgoing = VecDeque::with_capacity(8);
        owner.advance(now, &mut outgoing).unwrap();
        assert!(outgoing.is_empty());
        assert!(!owner.advance(now, &mut outgoing).unwrap());
        flight.complete(1, Some(Duration::ZERO), false).unwrap();
        owner.complete(flight).unwrap();
        owner.advance(now, &mut outgoing).unwrap();
        assert!(matches!(
            outgoing.pop_front().unwrap().1,
            Message::Recycle(_)
        ));
        assert!(matches!(
            outgoing.pop_front().unwrap().1,
            Message::CheckLoss { .. }
        ));
        assert_eq!(owner.sender.flight_bytes(0), Some((0, 1280)));
        assert_eq!(owner.timers.earliest(0).unwrap().1, Duration::ZERO);
    }
}

#[cfg(test)]
mod blocked_prefix_regression {
    use super::*;
    #[test]
    fn blocked_grant_keeps_the_full_prepared_request() {
        let (mut owner, flights) = Owner::new(1, 1, &[(0, 1, 0)], Duration::ZERO).unwrap();
        owner.timers.install(0, 1).unwrap();
        let mut flight = flights.into_vec().pop().unwrap();
        let packets = std::array::from_fn::<_, 16, _>(|index| pacing::Planned {
            id: PacketId {
                session: 0,
                generation: 1,
                lane: 1,
                number: index as u64 + 1,
            },
            bytes: 1280,
        });
        flight.prepare(&packets).unwrap();
        owner
            .request(Request {
                flight,
                group: 0,
                deadline: Duration::from_millis(100),
                probe: None,
                mtu: 1280,
                pmtu: false,
            })
            .unwrap();
        owner.sender.close(0).unwrap();
        owner.groups[0].ack = Some((0, packets[0].id));
        let mut outgoing = VecDeque::with_capacity(8);
        owner.advance(Duration::ZERO, &mut outgoing).unwrap();
        assert!(outgoing.is_empty());
        assert_eq!(
            owner.requests[0].as_ref().unwrap().flight.packets().len(),
            16
        );
        owner.groups[0].ack = None;
        owner.groups[0]
            .controller
            .enter_recovery(Duration::ZERO, 100_000, true)
            .unwrap();
        owner
            .advance(Duration::from_millis(1), &mut outgoing)
            .unwrap();
        let (_, Message::Permit(flight)) = outgoing.pop_front().unwrap() else {
            panic!()
        };
        assert_eq!(flight.packets().len(), 16);
    }
}

#[cfg(test)]
mod drain_tests {
    use super::*;
    use crate::ack::AckProgress;

    fn ready(count: usize) -> Owner {
        let (mut owner, flights) =
            Owner::new(1, 2, &[(0, 1, 0), (0, 2, 0)], Duration::ZERO).unwrap();
        owner.timers.install(0, 1).unwrap();
        let limits = pacing::Limits {
            window: 1_000_000,
            rate: 1_000_000_000_000,
            quantum: 20_480,
            max_datagram: 1280,
        };
        let mut completed = Vec::new();
        for (worker, mut flight) in flights.into_vec().into_iter().enumerate() {
            let planned: Vec<_> = (1..=count)
                .map(|number| pacing::Planned {
                    id: PacketId {
                        session: 0,
                        generation: 1,
                        lane: worker as u16 + 1,
                        number: number as u64,
                    },
                    bytes: 100,
                })
                .collect();
            flight.prepare(&planned).unwrap();
            let at = Duration::from_micros(worker as u64);
            owner
                .sender
                .reserve(0, &mut flight, false, limits, at)
                .unwrap();
            let mut report = Box::new(Report::new(worker));
            report.id = PacketId {
                number: 0,
                ..planned[0].id
            };
            report.count = count;
            report.kind = ReportKind::Ack {
                received: Duration::from_millis(1),
                delay_us: 0,
                first: true,
                end: Some(AckProgress {
                    packets: count as u32,
                    eliciting: true,
                    released_bytes: count as u64 * 100,
                    rtt_sent_at: Some(at),
                }),
            };
            for (index, entry) in report.entries[..count].iter_mut().enumerate() {
                *entry = Entry {
                    number: index as u64 + 1,
                    sample: flight.handle(index),
                    settled: true,
                };
            }
            flight.complete(count, Some(at), false).unwrap();
            completed.push((flight, report));
        }
        for (flight, report) in completed.into_iter().rev() {
            owner.complete(flight).unwrap();
            owner.report(report).unwrap();
        }
        owner
    }

    #[test]
    fn one_turn_drains_ready_flights_and_terminal_acks_with_available_budget() {
        let mut owner = ready(2);
        let mut outgoing = VecDeque::with_capacity(16);
        owner
            .advance(Duration::from_millis(1), &mut outgoing)
            .unwrap();
        assert_eq!(outgoing.len(), 4);
        assert!(matches!(outgoing[0].1, Message::Recycle(_)));
        assert!(matches!(outgoing[1].1, Message::Recycle(_)));
        assert!(matches!(outgoing[2].1, Message::ReportReturn(_)));
        assert!(matches!(outgoing[3].1, Message::ReportReturn(_)));
        assert_eq!(owner.sender.flight_bytes(0), Some((0, 0)));
        assert!(!owner.sender.feedback_ready(0));
        assert_eq!(owner.timers.earliest(0), None);
    }

    #[test]
    fn ready_reports_do_not_overrun_remaining_turn_budget() {
        let mut owner = ready(31);
        let mut outgoing = VecDeque::with_capacity(16);
        owner
            .advance(Duration::from_millis(1), &mut outgoing)
            .unwrap();
        assert_eq!(outgoing.len(), 2);
        assert!(
            outgoing
                .iter()
                .all(|(_, message)| matches!(message, Message::Recycle(_)))
        );
        assert_eq!(owner.sender.flight_bytes(0), Some((0, 6200)));
        assert!(owner.sender.feedback_ready(0));
        outgoing.clear();
        owner
            .advance(Duration::from_millis(1), &mut outgoing)
            .unwrap();
        assert_eq!(outgoing.len(), 2);
        assert!(
            outgoing
                .iter()
                .all(|(_, message)| matches!(message, Message::ReportReturn(_)))
        );
        assert_eq!(owner.sender.flight_bytes(0), Some((0, 0)));
        assert!(!owner.sender.feedback_ready(0));
    }
}

#[cfg(test)]
mod fairness_regression {
    use super::*;

    fn request(mut flight: Box<Flight>, number: u64, group: usize) -> Request {
        flight
            .prepare(&[pacing::Planned {
                id: PacketId {
                    session: group,
                    generation: 1,
                    lane: if group == 0 {
                        flight.worker() as u16 + 1
                    } else {
                        1
                    },
                    number,
                },
                bytes: 1280,
            }])
            .unwrap();
        Request {
            flight,
            group,
            deadline: Duration::from_secs(1),
            probe: None,
            mtu: 1280,
            pmtu: false,
        }
    }

    #[test]
    fn continuously_ready_workers_alternate_across_pacer_deadlines() {
        let (mut owner, flights) =
            Owner::new(1, 2, &[(0, 1, 0), (0, 2, 0)], Duration::ZERO).unwrap();
        owner.timers.install(0, 1).unwrap();
        for flight in flights.into_vec() {
            owner.request(request(flight, 1, 0)).unwrap();
        }
        let mut outgoing = VecDeque::with_capacity(16);
        let mut next = [2, 2];
        let mut selected = Vec::new();
        let mut now = Duration::ZERO;
        for _ in 0..6 {
            owner.advance(now, &mut outgoing).unwrap();
            let (worker, Message::Permit(mut flight)) = outgoing.pop_front().unwrap() else {
                panic!()
            };
            assert!(outgoing.is_empty());
            selected.push(worker);
            flight.complete(0, None, false).unwrap();
            owner.complete(flight).unwrap();
            // Commit/recycle while pacing still blocks the other continuously queued worker.
            owner.advance(now, &mut outgoing).unwrap();
            let (origin, Message::Recycle(flight)) = outgoing.pop_front().unwrap() else {
                panic!()
            };
            assert_eq!(origin, worker);
            assert!(outgoing.is_empty());
            owner.request(request(flight, next[worker], 0)).unwrap();
            next[worker] += 1;
            now = owner.sender.deadline(0, now).unwrap();
        }
        assert_eq!(selected, [0, 1, 0, 1, 0, 1]);
    }

    #[test]
    fn later_group_grants_do_not_starve_another_workers_group() {
        let (mut owner, flights) =
            Owner::new(2, 3, &[(0, 1, 0), (0, 2, 0), (1, 1, 1)], Duration::ZERO).unwrap();
        for session in 0..2 {
            owner.timers.install(session, 1).unwrap();
        }
        for flight in flights.into_vec() {
            let group = usize::from(flight.worker() == 2);
            owner.request(request(flight, 1, group)).unwrap();
        }
        let mut outgoing = VecDeque::with_capacity(16);
        let mut next = [2; 3];
        let mut seen = 0;
        let mut now = Duration::ZERO;
        for round in 0..6 {
            owner.advance(now, &mut outgoing).unwrap();
            assert_eq!(outgoing.len(), 2);
            while let Some((worker, message)) = outgoing.pop_front() {
                let Message::Permit(mut flight) = message else {
                    panic!()
                };
                seen |= 1 << worker;
                flight.complete(0, None, false).unwrap();
                owner.complete(flight).unwrap();
            }
            owner.advance(now, &mut outgoing).unwrap();
            assert_eq!(outgoing.len(), 2);
            while let Some((worker, message)) = outgoing.pop_front() {
                let Message::Recycle(flight) = message else {
                    panic!()
                };
                owner
                    .request(request(flight, next[worker], usize::from(worker == 2)))
                    .unwrap();
                next[worker] += 1;
            }
            now = (0..2)
                .filter_map(|group| owner.sender.deadline(group, now))
                .max()
                .unwrap();
            if round % 3 == 2 {
                assert_eq!(seen, 0b111);
                seen = 0;
            }
        }
    }
}

#[cfg(test)]
mod window_evidence_tests {
    use super::*;

    #[test]
    fn unused_grant_adds_no_evidence_and_drained_retry_preserves_prior_evidence() {
        for app_limited in [false, true] {
            let (mut owner, flights) = Owner::new(1, 1, &[(0, 1, 0)], Duration::ZERO).unwrap();
            owner.timers.install(0, 1).unwrap();
            let mut flight = flights.into_vec().pop().unwrap();
            let packets: Vec<_> = (1..=10)
                .map(|number| pacing::Planned {
                    id: PacketId {
                        session: 0,
                        generation: 1,
                        lane: 1,
                        number,
                    },
                    bytes: 1280,
                })
                .collect();
            flight.prepare(&packets).unwrap();
            owner
                .request(Request {
                    flight,
                    group: 0,
                    deadline: Duration::from_secs(1),
                    probe: None,
                    mtu: 1280,
                    pmtu: false,
                })
                .unwrap();
            let mut outgoing = VecDeque::with_capacity(16);
            owner.advance(Duration::ZERO, &mut outgoing).unwrap();
            let (_, Message::Permit(mut flight)) = outgoing.pop_front().unwrap() else {
                panic!()
            };
            assert!(!owner.groups[0].cwnd_limited);
            flight.complete(0, None, false).unwrap();
            owner.complete(flight).unwrap();
            assert!(!owner.groups[0].cwnd_limited);
            owner
                .advance(Duration::from_millis(1), &mut outgoing)
                .unwrap();
            let (_, Message::Recycle(mut flight)) = outgoing.pop_front().unwrap() else {
                panic!()
            };
            flight.prepare(&packets).unwrap();
            owner.groups[0].cwnd_limited = true; // An earlier flight was entirely declared lost.
            if app_limited {
                owner.sender.close(0).unwrap();
                owner.sender.advance(0, BATCH_SIZE).unwrap();
                owner.sender.application_limited(0).unwrap();
            }
            owner
                .request(Request {
                    flight,
                    group: 0,
                    deadline: Duration::from_secs(1),
                    probe: None,
                    mtu: 1280,
                    pmtu: false,
                })
                .unwrap();
            owner
                .advance(Duration::from_millis(1), &mut outgoing)
                .unwrap();
            assert_eq!(owner.groups[0].cwnd_limited, !app_limited);
        }
    }
}
