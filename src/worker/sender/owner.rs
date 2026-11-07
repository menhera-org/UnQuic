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

struct Group {
    controller: Controller,
    rtt: Rtt,
    pto: ProbeTimeout,
    epoch: u64,
    ack: Option<(usize, PacketId)>,
    cwnd_limited: bool,
    recovery: Option<Duration>,
    lost: u64,
    lost_at: Duration,
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
    pub(super) fn new(
        groups: usize,
        workers: usize,
        descriptors: &[(usize, u16, usize)],
    ) -> io::Result<(Self, Box<[Box<Flight>]>)> {
        let (sender, flights) = Sender::new(groups, workers, SAMPLE_SLOTS)?;
        let mut states = Vec::new();
        states.try_reserve_exact(groups).map_err(io::Error::other)?;
        for group in 0..groups {
            states.push(Group {
                controller: Controller::new(1280, None, Duration::ZERO, group as u64 + 1)
                    .map_err(io::Error::other)?,
                rtt: Rtt::new(),
                pto: ProbeTimeout::new(),
                epoch: 0,
                ack: None,
                cwnd_limited: false,
                recovery: None,
                lost: 0,
                lost_at: Duration::ZERO,
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
        let group = flight.group().ok_or(io::ErrorKind::InvalidInput)?;
        self.sender
            .reported(flight)
            .map_err(|(error, _)| io::Error::other(format!("sender completion: {error}")))?;
        self.rescan |= group < self.cursor;
        Ok(())
    }

    pub(super) fn report(&mut self, report: Box<Report>) -> io::Result<()> {
        if self.reports.len() == self.reports.capacity() {
            return Err(io::Error::other("duplicate sender feedback"));
        }
        self.sender.close(report.group).map_err(io::Error::other)?;
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
            if let Some(flight) = self.sender.advance(group, work).map_err(io::Error::other)? {
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
            } else if self.sender.commit_pending(group) {
                work = 0;
            }
            more |= self.sender.commit_pending(group);
            if self.sender.feedback_ready(group) {
                let active = self.groups[group].ack;
                let candidate = self.reports.iter().position(|report| {
                    report.group == group
                        && active.is_none_or(|id| id == (report.origin, report.id))
                });
                if let Some(index) =
                    candidate.filter(|_| work != 0 && outgoing.len() < outgoing.capacity())
                {
                    let report = self
                        .reports
                        .remove(index)
                        .ok_or(io::ErrorKind::InvalidData)?;
                    work = work.saturating_sub(report.count.max(1));
                    self.apply_report(&report, now)?;
                    outgoing.push_back((report.origin, Message::ReportReturn(report)));
                    more = true;
                }
                if self.groups[group].ack.is_none()
                    && !self.reports.iter().any(|r| r.group == group)
                {
                    self.sender.reopen(group).map_err(io::Error::other)?;
                } else {
                    more |= self.groups[group].ack.is_none();
                }
            }
            // A feedback transaction takes precedence over PTO and cannot lose its ACKed entries.
            if self.groups[group].ack.is_none() && !self.reports.iter().any(|r| r.group == group) {
                let latest = self.timers.earliest(group);
                let state = &mut self.groups[group];
                if let Some(expiry) = state.pto.expire(now, &state.rtt, latest.map(|(_, at)| at)) {
                    state.epoch = state.epoch.checked_add(1).ok_or(io::ErrorKind::Other)?;
                    self.sender
                        .probe_allowance(group, expiry.probe_limit)
                        .map_err(io::Error::other)?;
                    state.probe = Some((
                        latest
                            .map(|(id, _)| id)
                            .or_else(|| self.timers.installed(group)),
                        expiry.probe_limit,
                    ));
                    if expiry.declare_lost {
                        state.lost = u64::MAX >> (64 - self.requests.len());
                        state.lost_at = now;
                    }
                }
                if let Some(at) = state.pto.deadline(&state.rtt, latest.map(|(_, at)| at)) {
                    self.round_due = Some(self.round_due.map_or(at, |old| old.min(at)));
                }
                if state.lost != 0
                    && outgoing.len() < outgoing.capacity().saturating_sub(6 * self.requests.len())
                {
                    let worker = state.lost.trailing_zeros() as usize;
                    outgoing.push_back((
                        worker,
                        Message::Lost {
                            group,
                            epoch: state.epoch,
                            before: state.lost_at,
                        },
                    ));
                    state.lost &= state.lost - 1;
                    more = true;
                }
                if state.lost == 0
                    && outgoing.len() < outgoing.capacity().saturating_sub(6 * self.requests.len())
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
                        more = true;
                        continue;
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
            state
                .controller
                .on_transmit(
                    now,
                    inflight,
                    self.sender.is_app_limited(request.group).unwrap_or(false),
                )
                .map_err(io::Error::other)?;
            let mut limits = state.controller.limits();
            if state.pto.stalled() {
                limits.window = 0;
            }
            let burst = limits.quantum.min(16 * u32::from(limits.max_datagram));
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
            state.cwnd_limited |= available < u64::from(limits.max_datagram);
            let mut budget = u64::from(limits.quantum.min(16 * u32::from(limits.max_datagram)))
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
            let result = request.flight.truncate(bytes).and_then(|_| {
                self.sender.reserve(
                    request.group,
                    &mut request.flight,
                    request.probe.is_some(),
                    limits,
                    now,
                )
            });
            match result {
                Ok(()) => {
                    outgoing.push_back((worker, Message::Permit(request.flight)));
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
        let (mut owner, flights) = Owner::new(1, 2, &[(0, 1, 0), (0, 2, 0)]).unwrap();
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
        flight.complete(10, Some(Duration::ZERO), false).unwrap();
        owner.complete(flight).unwrap();
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
        let (mut owner, flights) = Owner::new(1, 2, &[(0, 1, 0), (0, 2, 0)]).unwrap();
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
        let (mut owner, _) = Owner::new(65, 1, &[(0, 1, 0), (1, 1, 64)]).unwrap();
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
        let (mut owner, flights) = Owner::new(65, 1, &[(0, 1, 0)]).unwrap();
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
        let (mut owner, flights) = Owner::new(1, 1, &[(0, 1, 0)]).unwrap();
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
        let (mut owner, flights) = Owner::new(1, 1, &[(0, 1, 0)]).unwrap();
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
            let (mut owner, flights) = Owner::new(1, 1, &[(0, 1, 0)]).unwrap();
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
        let (mut owner, _) = Owner::new(1, 1, &[(0, 1, 0)]).unwrap();
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
        let (mut owner, _) = Owner::new(1, 1, &[(0, 1, 0)]).unwrap();
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
