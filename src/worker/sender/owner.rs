//! Applies lane owners' feedback to each group's controller and publishes the
//! limits workers claim from; sends never wait for this owner.
use super::*;
use crate::{
    bbr::Controller,
    loss::ProbeTimeout,
    pacing::{Budget, Published, Sampler, SendRecord},
};
use std::sync::Arc;

struct LossBarrier {
    token: u64,
    before: Duration,
    force: bool,
    unsent: u64,
    pending: u64,
    next_due: Option<Duration>,
    dirty: bool,
}

/// One ACK's delivery evidence, accumulated across bounded report chunks.
#[derive(Clone, Copy)]
struct AckSum {
    at: Duration,
    newly: u64,
    newest: Option<SendRecord>,
    pmtu_only: bool,
}

struct Group {
    controller: Controller,
    sampler: Sampler,
    published: Published,
    rtt: Rtt,
    pto: ProbeTimeout,
    epoch: u64,
    ack: Option<(usize, PacketId)>,
    sum: AckSum,
    cwnd_limited: bool,
    recovery: Option<Duration>,
    loss_serial: u64,
    check: Option<LossBarrier>,
    probe: Option<(Option<PacketId>, u8)>,
    probe_cursor: Option<usize>,
}

pub(in crate::worker) struct Owner {
    budgets: Arc<[Budget]>,
    pub(super) timers: timers::Timers,
    groups: Vec<Group>,
    workers: usize,
    reports: VecDeque<Box<Report>>,
    cursor: usize,
    round_due: Option<Duration>,
    rescan: bool,
    // Latest time given to any controller, which rejects time going backwards.
    now: Duration,
    pub(in crate::worker) due: Option<Duration>,
    #[cfg(feature = "trace")]
    trace: Option<(Duration, u64)>,
}

#[cfg(feature = "trace")]
impl Owner {
    fn trace_control(&mut self, group: usize, now: Duration) {
        let state = &self.groups[group];
        let model = state.controller.trace_model();
        let key = model[0] | model[1] << 4;
        if self
            .trace
            .is_some_and(|(at, last)| last == key && now < at + Duration::from_micros(100))
        {
            return;
        }
        self.trace = Some((now, key));
        let limits = state.controller.limits();
        let inflight = self.budgets[group].in_flight();
        trace_event!(
            0,
            CONTROL,
            key,
            [
                limits.rate,
                limits.window,
                inflight,
                model[2],
                model[3],
                model[6]
            ]
        );
        trace_event!(
            0,
            BOUNDS,
            key,
            [
                model[4],
                model[5],
                model[7],
                model[8],
                limits.quantum,
                state.rtt.smoothed().as_nanos()
            ]
        );
    }
}

impl Owner {
    /// One budget per path group; workers claim from the same shared slice.
    pub(super) fn new(
        workers: usize,
        descriptors: &[(usize, u16, usize)],
        now: Duration,
        budgets: Arc<[Budget]>,
    ) -> io::Result<Self> {
        let groups = budgets.len();
        if !(1..=64).contains(&workers) {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let mut states = Vec::new();
        states.try_reserve_exact(groups).map_err(io::Error::other)?;
        for group in 0..groups {
            states.push(Group {
                controller: Controller::new(1280, None, now, group as u64 + 1)
                    .map_err(io::Error::other)?,
                sampler: Sampler::default(),
                published: Published::default(),
                rtt: Rtt::new(),
                pto: ProbeTimeout::new(),
                epoch: 0,
                ack: None,
                sum: AckSum {
                    at: now,
                    newly: 0,
                    newest: None,
                    pmtu_only: true,
                },
                cwnd_limited: false,
                recovery: None,
                loss_serial: 0,
                check: None,
                probe: None,
                probe_cursor: None,
            });
        }
        let mut reports = VecDeque::new();
        reports
            .try_reserve_exact(workers)
            .map_err(io::Error::other)?;
        let mut owner = Self {
            budgets,
            timers: timers::Timers::new(groups, descriptors)?,
            groups: states,
            workers,
            reports,
            cursor: 0,
            round_due: None,
            rescan: false,
            now,
            due: None,
            #[cfg(feature = "trace")]
            trace: None,
        };
        for group in 0..groups {
            owner.publish(group);
        }
        Ok(owner)
    }

    pub(super) fn storage_bytes(
        groups: usize,
        workers: usize,
        descriptors: &[(usize, u16, usize)],
    ) -> Option<usize> {
        size_of::<Self>()
            .checked_add(
                timers::Timers::storage_bytes(groups, descriptors)?
                    .checked_sub(size_of::<timers::Timers>())?,
            )?
            .checked_add(groups.checked_mul(size_of::<Group>() + size_of::<Budget>())?)?
            .checked_add(workers.checked_mul(size_of::<Box<Report>>())?)?
            .checked_add(2 * size_of::<usize>())
    }

    /// Records a worker's successful transmissions for PTO and controller inputs.
    pub(super) fn sent(&mut self, sent: Sent) -> io::Result<()> {
        let state = self
            .groups
            .get_mut(sent.group)
            .ok_or(io::ErrorKind::InvalidInput)?;
        // A lane owner may install a generation before this owner does; that
        // lane's PTO base then starts with its next send.
        if !self.timers.matches(sent.id) {
            return Ok(());
        }
        self.timers.submitted(
            sent.id.session,
            sent.id.lane,
            sent.id.generation,
            sent.count,
            sent.at,
        )?;
        state
            .controller
            .set_max_datagram(sent.mtu)
            .map_err(io::Error::other)?;
        if sent.restart {
            let app_limited = state.sampler.is_app_limited();
            if app_limited {
                state.cwnd_limited = false;
            }
            self.now = self.now.max(sent.at);
            state
                .controller
                .on_transmit(self.now, 0, app_limited)
                .map_err(io::Error::other)?;
        }
        state.cwnd_limited |= sent.cwnd_limited;
        self.rescan |= sent.group < self.cursor;
        Ok(())
    }

    pub(super) fn report(&mut self, report: Box<Report>) -> io::Result<()> {
        if self.reports.len() == self.reports.capacity() || report.group >= self.groups.len() {
            return Err(io::Error::other("duplicate sender feedback"));
        }
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
            bytes_in_flight: self.budgets.get(group)?.in_flight(),
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
        let state = &mut self.groups[group];
        state.loss_serial = state
            .loss_serial
            .checked_add(1)
            .ok_or(io::ErrorKind::Other)?;
        let mask = u64::MAX >> (64 - self.workers);
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
        if worker >= self.workers {
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
                        let target = if state.pto.stalled() {
                            // An unanswered lane must not hide working sessions in this group.
                            let target = self.timers.next_probe(group, state.probe_cursor);
                            state.probe_cursor = target.map(|id| id.session);
                            target
                        } else {
                            latest
                                .map(|(id, _)| id)
                                .or_else(|| self.timers.installed(group))
                        };
                        // A forced settlement holds the probes until its reports drain.
                        state.probe = Some((target, expiry.probe_limit));
                        if expiry.declare_lost {
                            self.start_loss_check(group, now, true)?;
                        }
                    }
                }
            }
            more = true;
        }
        if let Some(check) = &mut self.groups[group].check {
            if check.unsent != 0
                && outgoing.len() < outgoing.capacity().saturating_sub(6 * self.workers)
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
            // Returned reports and loss-check completions wake the owner. An expired PTO must not spin.
            return Ok(more);
        }
        let state = &mut self.groups[group];
        if let Some(at) = state.pto.deadline(&state.rtt, latest.map(|(_, at)| at)) {
            self.round_due = Some(self.round_due.map_or(at, |old| old.min(at)));
        }
        if outgoing.len() < outgoing.capacity().saturating_sub(6 * self.workers)
            && let Some((target, count)) = state.probe
            && let Some(id) = target
                .filter(|id| self.timers.matches(*id))
                .or_else(|| self.timers.installed(group))
        {
            let worker = (usize::from(id.lane) - 1) % self.workers;
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
        self.now = self.now.max(now);
        let now = self.now;
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
            // A feedback transaction takes precedence over PTO and cannot lose its ACKed entries.
            if self.groups[group].ack.is_none() {
                if self.reports.iter().any(|r| r.group == group) {
                    more = true;
                } else {
                    more |= self.advance_pto(group, now, outgoing)?;
                }
            }
            self.publish(group);
        }
        // Complete one bounded sweep before sleeping or replacing its minimum.
        if self.cursor == 0 {
            self.due = self.round_due.take();
            more |= std::mem::take(&mut self.rescan);
        } else {
            more = true;
        }
        Ok(more)
    }

    /// Publishes changed limits and delivery state, and the current PTO round.
    fn publish(&mut self, group: usize) {
        let state = &mut self.groups[group];
        let limits = state.controller.limits();
        let window = if state.pto.stalled() {
            0
        } else {
            limits.window
        };
        let burst = u64::from(limits.quantum.min(64 * u32::from(limits.max_datagram)));
        let published = state.sampler.published(limits.rate, window, burst);
        let budget = &self.budgets[group];
        if published != state.published {
            state.published = published;
            budget.publish(&published);
        }
        if budget.probe_epoch() != state.epoch {
            budget.set_probe_epoch(state.epoch);
        }
    }

    fn apply_report(&mut self, report: &Report, now: Duration) -> io::Result<()> {
        let group = report.group;
        let in_flight = self.budgets[group].in_flight();
        let state = self
            .groups
            .get_mut(group)
            .ok_or(io::ErrorKind::InvalidInput)?;
        if let ReportKind::Ack { first: true, .. } = report.kind {
            if state.ack.is_some() {
                return Err(io::ErrorKind::InvalidData.into());
            }
            state.ack = Some((report.origin, report.id));
            state.sum = AckSum {
                at: now,
                newly: 0,
                newest: None,
                pmtu_only: true,
            };
        }
        let mut settled = 0;
        for entry in &report.entries[..report.count] {
            settled += u32::from(entry.settled);
            if entry.sample.is_none() {
                continue;
            }
            let record = entry.record;
            match report.kind {
                ReportKind::Ack { .. } => {
                    let sum = &mut state.sum;
                    sum.newly = sum.newly.saturating_add(u64::from(record.bytes));
                    sum.pmtu_only &= record.pmtu;
                    let key = (record.sent_ns, record.order);
                    if sum.newest.is_none_or(|old| key > (old.sent_ns, old.order)) {
                        sum.newest = Some(record);
                    }
                }
                ReportKind::Loss => {
                    let loss = state.sampler.lost(&record);
                    trace_event!(
                        0,
                        LOSS,
                        0,
                        [
                            report.id.session << 16 | usize::from(report.id.lane),
                            entry.number,
                            loss.bytes
                        ]
                    );
                    if state.recovery.is_none() {
                        state
                            .controller
                            .enter_recovery(now, in_flight, state.pto.stalled())
                            .map_err(io::Error::other)?;
                        state.recovery = Some(now);
                    }
                    state
                        .controller
                        .on_loss(loss, now)
                        .map_err(io::Error::other)?;
                }
                ReportKind::Retire => {}
            }
        }
        // Sends reported before this owner installed the lane were never counted.
        let id = report.id;
        let settled = settled.min(
            self.timers
                .outstanding(id.session, id.lane, id.generation)
                .unwrap_or(0),
        );
        if settled != 0 {
            self.timers
                .settled(id.session, id.lane, id.generation, settled)?;
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
            }
            if let Some(newest) = state.sum.newest {
                let sum = state.sum;
                let sample = state.sampler.ack(
                    sum.at,
                    sum.newly,
                    &newest,
                    sum.pmtu_only,
                    false,
                    state.rtt.minimum().unwrap_or(Duration::ZERO),
                );
                let rtt = progress
                    .rtt_sent_at
                    .and_then(|sent| received.checked_sub(sent));
                if state
                    .controller
                    .on_ack(sample, rtt, in_flight, state.cwnd_limited, now)
                    .map_err(io::Error::other)?
                {
                    state.sampler.application_limited(in_flight);
                }
                state.cwnd_limited = false;
            }
            state.ack = None;
        }
        #[cfg(feature = "trace")]
        self.trace_control(group, now);
        Ok(())
    }
}

#[cfg(test)]
mod tests;
