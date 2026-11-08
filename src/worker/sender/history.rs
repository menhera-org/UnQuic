//! Bounded TX-local ACK/loss/retirement work using the worker's reserved report.
use super::*;

impl Lane {
    pub(in crate::worker) fn idle(&self) -> bool {
        self.unsent_bytes == 0
            && self.ack.is_none()
            && !self.report_pending
            && self.loss_passes == 0
            && self.history.is_retired()
    }

    pub(in crate::worker) fn install(&mut self, generation: u64) -> io::Result<()> {
        if generation < self.generation {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        if generation == self.generation {
            return Ok(());
        }
        if self.unsent_bytes != 0
            || self.ack.is_some()
            || self.report_pending
            || self.loss_passes != 0
        {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        self.history.reset().map_err(io::Error::other)?;
        self.generation = generation;
        self.rtt = Rtt::new();
        self.largest_ack = 0;
        self.scan = None;
        self.loss_due = None;
        self.scan_due = None;
        self.forced_before = None;
        self.scan_cutoff = None;
        self.probes = [None; 2];
        Ok(())
    }

    /// Rejecting an ACK retains its buffer for return to its origin.
    pub(in crate::worker) fn begin_ack(&mut self, ack: Box<Ack>) -> Result<(), Box<Ack>> {
        if self.ack.is_some()
            || self.report_pending
            || ack.session != self.session
            || ack.lane != self.lane
        {
            return Err(ack);
        }
        let Some(ranges) = ack
            .ranges
            .get(..ack.count)
            .and_then(|r| AckRanges::new(r).ok())
        else {
            return Err(ack);
        };
        if self.history.begin_ack(ranges).is_err() {
            return Err(ack);
        }
        self.largest_ack = self
            .largest_ack
            .max(ranges.iter().map(|r| r.end).max().unwrap_or(0));
        self.ack_first = true;
        self.ack = Some(ack);
        Ok(())
    }

    /// Reserve/cancel directly in history; attach a sampled handle only after success.
    pub(in crate::worker) fn submitted(
        &mut self,
        number: u64,
        at: Duration,
        sample: Option<NonZeroU16>,
    ) -> bool {
        let submitted = match sample {
            Some(slot) => self.history.submit_sampled(number, at, slot),
            None => self.history.submit(number, at),
        };
        if submitted && let Some(due) = self.rtt.loss_deadline(number, self.largest_ack, at) {
            self.loss_due = Some(self.loss_due.map_or(due, |old| old.min(due)));
        }
        submitted
    }

    /// Starts a fresh barrier pass only after the preceding pass/report has finished.
    pub(super) fn begin_loss_check(&mut self, before: Duration, force: bool) -> io::Result<()> {
        if self.ack.is_some() || self.report_pending || self.scan.is_some() {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        self.scan = Some(self.history.outstanding_scan());
        self.scan_cutoff = self.forced_before.take();
        if force {
            self.scan_cutoff = Some(self.scan_cutoff.map_or(before, |old| old.max(before)));
        }
        self.scan_due = None;
        self.loss_due = None;
        Ok(())
    }

    /// Queue a fixed-cutoff sweep; newer sends cannot extend an in-progress pass.
    pub(in crate::worker) fn force_loss(&mut self, cutoff: Duration) {
        if self.scan_cutoff.is_none_or(|old| cutoff > old) {
            self.forced_before = Some(self.forced_before.map_or(cutoff, |old| old.max(cutoff)));
        }
    }

    pub(in crate::worker) fn deadline(&self) -> Option<Duration> {
        if self.report_pending {
            None
        } else if self.ack.is_some() || self.scan.is_some() || self.forced_before.is_some() {
            Some(Duration::ZERO)
        } else {
            self.loss_due
        }
    }

    pub(in crate::worker) fn work_pending(&self, now: Duration) -> bool {
        self.deadline().is_some_and(|due| due <= now)
    }

    fn prepare_report(&self, report: &mut Report, kind: ReportKind) {
        report.group = self.group;
        report.id = PacketId {
            session: self.session,
            generation: self.generation,
            lane: self.lane,
            number: 0,
        };
        report.count = 0;
        report.kind = kind;
    }

    /// True transfers the populated report to the owner; retain it on queue pressure.
    /// Budget bounds bitmap reads as well as callbacks, with at most 64 entries.
    pub(in crate::worker) fn advance_report(
        &mut self,
        report: &mut Report,
        now: Duration,
        budget: &mut usize,
    ) -> io::Result<bool> {
        if self.report_pending || *budget == 0 {
            return Ok(false);
        }
        let allowance = (*budget).min(BATCH_SIZE);
        if let Some(ack) = &self.ack {
            self.prepare_report(
                report,
                ReportKind::Ack {
                    received: ack.received,
                    delay_us: ack.delay_us,
                    first: self.ack_first,
                    end: None,
                },
            );
            *budget -= allowance;
            let end = self.history.advance_ack(allowance, |ack| {
                report.entries[report.count] = Entry {
                    number: ack.packet.number,
                    sample: ack.packet.sample_slot,
                    settled: ack.was_in_flight && ack.packet.eliciting,
                };
                report.count += 1;
            });
            if let Some(progress) = end {
                if let Some(sent) = progress.rtt_sent_at {
                    self.rtt.sample(sent, ack.received, ack.delay_us);
                }
                // Finish this bounded pass before revisiting earlier packets with new RTT/ACK state.
                self.loss_due = Some(now);
                report.kind = ReportKind::Ack {
                    received: ack.received,
                    delay_us: ack.delay_us,
                    first: self.ack_first,
                    end: Some(progress),
                };
            }
            if report.count != 0 || end.is_some() {
                self.ack_first = false;
                self.report_pending = true;
                return Ok(true);
            }
            return Ok(false);
        }
        if self.scan.is_none() {
            if self.forced_before.is_none() && self.loss_due.is_none_or(|due| due > now) {
                return Ok(false);
            }
            self.scan = Some(self.history.outstanding_scan());
            self.scan_cutoff = self.forced_before.take();
            self.scan_due = None;
            self.loss_due = None;
        }
        self.prepare_report(report, ReportKind::Loss);
        let mut remaining = allowance;
        let scan = self.scan.as_mut().ok_or(io::ErrorKind::InvalidData)?;
        while let Some(packet) = scan.advance(&self.history, &mut remaining) {
            let due = self
                .rtt
                .loss_deadline(packet.number, self.largest_ack, packet.sent_at);
            if self
                .scan_cutoff
                .is_some_and(|cutoff| packet.sent_at <= cutoff)
                || due.is_some_and(|due| due <= now)
            {
                if let Some(packet) = self.history.lose(packet.number) {
                    report.entries[report.count] = Entry {
                        number: packet.number,
                        sample: packet.sample_slot,
                        settled: packet.eliciting,
                    };
                    report.count += 1;
                }
            } else if let Some(due) = due {
                self.scan_due = Some(self.scan_due.map_or(due, |old| old.min(due)));
            }
        }
        *budget -= allowance - remaining;
        if scan.is_complete() {
            self.loss_passes = self.loss_passes.saturating_sub(1);
            self.scan = None;
            self.scan_cutoff = None;
            self.loss_due = match (self.loss_due, self.scan_due.take()) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
        }
        self.report_pending = report.count != 0;
        Ok(self.report_pending)
    }

    /// Explicit pressure/quiescence retirement; ordinary ticks must not trim late-ACK history.
    pub(in crate::worker) fn retire_report(
        &mut self,
        report: &mut Report,
        budget: &mut usize,
    ) -> io::Result<bool> {
        if self.report_pending || self.ack.is_some() || *budget == 0 {
            return Ok(false);
        }
        self.prepare_report(report, ReportKind::Retire);
        for _ in 0..(*budget).min(BATCH_SIZE) {
            *budget -= 1;
            let Some(entry) = self
                .history
                .retire_oldest_resolved()
                .map_err(io::Error::other)?
            else {
                break;
            };
            report.entries[report.count] = Entry {
                number: entry.number,
                sample: entry.sample_slot,
                settled: false,
            };
            report.count += 1;
        }
        self.report_pending = report.count != 0;
        Ok(self.report_pending)
    }

    /// Call only after the control owner consumed all entries and returns this report.
    pub(in crate::worker) fn report_completed(
        &mut self,
        report: &Report,
    ) -> io::Result<Option<Box<Ack>>> {
        if !self.report_pending
            || report.group != self.group
            || report.id.session != self.session
            || report.id.lane != self.lane
            || report.id.generation != self.generation
        {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        self.report_pending = false;
        if matches!(report.kind, ReportKind::Ack { end: Some(_), .. }) {
            Ok(self.ack.take())
        } else {
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lane() -> Lane {
        let mut lane = Lane::new(3, 2, 1).unwrap();
        lane.install(7).unwrap();
        lane
    }

    fn send(lane: &mut Lane, number: u64, at: u64) {
        lane.history.reserve(number, 1500, true, true).unwrap();
        assert!(lane.submitted(
            number,
            Duration::from_millis(at),
            NonZeroU16::new(number as u16)
        ));
    }

    fn ack(start: u64, end: u64) -> Box<Ack> {
        let mut ack = Box::new(Ack::new(4));
        ack.session = 3;
        ack.lane = 2;
        ack.received = Duration::from_millis(100);
        ack.ranges[0] = AckRange { start, end };
        ack.count = 1;
        ack
    }

    #[test]
    fn ack_chunks_preserve_handles_and_final_buffer_until_report_return() {
        let mut lane = lane();
        for number in 1..=130 {
            send(&mut lane, number, 1);
        }
        assert!(lane.begin_ack(ack(1, 130)).is_ok());
        let mut report = Report::new(0);
        let mut entries = Vec::new();
        let mut reports = 0;
        let mut completed = false;
        for _ in 0..300 {
            if !lane
                .advance_report(&mut report, Duration::from_millis(100), &mut 7)
                .unwrap()
            {
                continue;
            }
            assert!(report.count <= 7);
            let ReportKind::Ack { first, end, .. } = report.kind else {
                panic!()
            };
            assert_eq!(first, reports == 0);
            reports += 1;
            assert!(lane.install(8).is_err());
            assert!(
                !lane
                    .advance_report(&mut Report::new(0), Duration::from_secs(1), &mut 64)
                    .unwrap()
            );
            assert!(lane.ack.is_some());
            for entry in &report.entries[..report.count] {
                assert_eq!(entry.sample.unwrap().get() as u64, entry.number);
                assert!(entry.settled);
                entries.push(entry.number);
            }
            let returned = lane.report_completed(&report).unwrap();
            assert_eq!(returned.is_some(), end.is_some());
            if let Some(progress) = end {
                assert_eq!(progress.packets, 130);
                assert_eq!(progress.released_bytes, 130 * 1500);
                completed = true;
                break;
            }
        }
        assert!(completed);
        entries.sort_unstable();
        assert_eq!(entries, (1..=130).collect::<Vec<_>>());
        assert_eq!(lane.history.inflight_bytes(), 0);
        assert!(lane.install(8).is_ok());
    }

    #[test]
    fn forced_loss_copies_handles_and_late_ack_takes_them_once() {
        let mut lane = lane();
        send(&mut lane, 1, 1);
        send(&mut lane, 2, 2);
        lane.force_loss(Duration::from_millis(1));
        let mut report = Report::new(0);
        assert!(
            lane.advance_report(&mut report, Duration::from_millis(3), &mut 64)
                .unwrap()
        );
        assert!(matches!(report.kind, ReportKind::Loss));
        assert_eq!(report.count, 1);
        assert_eq!(report.entries[0].number, 1);
        assert!(report.entries[0].settled);
        assert_eq!(
            lane.history.packet(1).unwrap().sample_slot,
            report.entries[0].sample
        );
        assert!(lane.begin_ack(ack(1, 2)).is_err());
        assert!(lane.report_completed(&report).unwrap().is_none());
        assert!(lane.begin_ack(ack(1, 2)).is_ok());
        assert!(
            lane.advance_report(&mut report, Duration::from_millis(100), &mut 64)
                .unwrap()
        );
        assert_eq!(report.count, 2);
        for entry in &report.entries[..2] {
            assert!(entry.sample.is_some());
            assert_eq!(entry.settled, entry.number == 2);
        }
        assert!(lane.report_completed(&report).unwrap().is_some());
        assert!(lane.history.packet(1).unwrap().sample_slot.is_none());
        assert!(lane.begin_ack(ack(1, 2)).is_ok());
        assert!(
            lane.advance_report(&mut report, Duration::from_millis(101), &mut 64)
                .unwrap()
        );
        assert_eq!(report.count, 0);
        assert!(lane.report_completed(&report).unwrap().is_some());
    }

    #[test]
    fn forced_sweep_is_bounded_and_does_not_consume_newer_sends() {
        let mut lane = lane();
        for number in 1..=130 {
            send(&mut lane, number, 1);
        }
        lane.force_loss(Duration::from_millis(1));
        let mut report = Report::new(0);
        assert!(
            lane.advance_report(&mut report, Duration::from_millis(2), &mut 4)
                .unwrap()
        );
        assert!(report.count <= 4);
        let mut lost = report.count;
        lane.report_completed(&report).unwrap();
        send(&mut lane, 131, 2);
        for _ in 0..100 {
            if lane
                .advance_report(&mut report, Duration::from_millis(3), &mut 4)
                .unwrap()
            {
                assert!(report.count <= 4);
                assert!(
                    report.entries[..report.count]
                        .iter()
                        .all(|entry| entry.number <= 130)
                );
                lost += report.count;
                lane.report_completed(&report).unwrap();
            }
            if lane.deadline().is_none() {
                break;
            }
        }
        assert_eq!(lost, 130);
        assert_eq!(lane.history.inflight_bytes(), 1500);
        lane.force_loss(Duration::from_millis(2));
        let mut finished = false;
        for _ in 0..100 {
            if lane
                .advance_report(&mut report, Duration::from_millis(3), &mut 4)
                .unwrap()
            {
                assert_eq!(report.count, 1);
                assert_eq!(report.entries[0].number, 131);
                lane.report_completed(&report).unwrap();
                finished = true;
                break;
            }
        }
        assert!(finished);
    }

    #[test]
    fn retirement_counts_holes_and_pins_taken_handles_until_return() {
        let mut lane = lane();
        lane.history.reserve(1, 100, false, false).unwrap();
        assert!(lane.history.cancel(1));
        send(&mut lane, 3, 1);
        assert!(!lane.idle());
        lane.force_loss(Duration::from_millis(1));
        let mut report = Report::new(0);
        assert!(
            lane.advance_report(&mut report, Duration::from_millis(2), &mut 64)
                .unwrap()
        );
        lane.report_completed(&report).unwrap();
        assert!(lane.retire_report(&mut report, &mut 2).unwrap());
        assert_eq!(report.count, 2);
        assert!(
            report.entries[..2]
                .iter()
                .all(|entry| entry.sample.is_none())
        );
        lane.report_completed(&report).unwrap();
        assert!(!lane.idle());
        assert!(lane.retire_report(&mut report, &mut 2).unwrap());
        assert_eq!(report.count, 1);
        assert_eq!(report.entries[0].sample.unwrap().get(), 3);
        assert!(!lane.idle());
        assert!(lane.install(8).is_err());
        lane.report_completed(&report).unwrap();
        assert!(lane.idle());
        lane.install(8).unwrap();
        assert!(lane.idle());
    }

    #[test]
    fn invalid_ack_returns_original_buffer_and_changes_no_lane_state() {
        let mut lane = lane();
        send(&mut lane, 1, 1);
        send(&mut lane, 3, 1);
        let invalid = ack(1, 3);
        let ptr = std::ptr::from_ref(invalid.as_ref());
        let returned = lane.begin_ack(invalid).err().unwrap();
        assert_eq!(ptr, std::ptr::from_ref(returned.as_ref()));
        assert_eq!(lane.largest_ack, 0);
        assert!(!lane.history.ack_pending());
        assert_eq!(lane.history.inflight_bytes(), 3000);
        assert!(lane.begin_ack(ack(3, 3)).is_ok());
        let mut report = Report::new(0);
        assert!(
            lane.advance_report(&mut report, Duration::from_millis(100), &mut 64)
                .unwrap()
        );
        lane.report_completed(&report).unwrap();
        assert!(lane.deadline().is_some());
    }
}

#[cfg(test)]
mod progress_tests {
    use super::*;

    #[test]
    fn sparse_loss_scan_progress_survives_continuous_duplicate_acks() {
        let mut lane = Lane::new(0, 1, 0).unwrap();
        lane.install(1).unwrap();
        for number in [257, 513] {
            lane.history.reserve(number, 1500, true, true).unwrap();
            assert!(lane.submitted(
                number,
                Duration::from_millis(1),
                NonZeroU16::new(number as u16)
            ));
        }
        lane.force_loss(Duration::from_millis(1));
        let mut report = Report::new(0);
        let now = Duration::from_millis(10);
        let mut budget = 1;
        assert!(!lane.advance_report(&mut report, now, &mut budget).unwrap());
        assert_eq!(budget, 0);
        assert!(lane.work_pending(now));
        let mut lost = false;
        for _ in 0..16 {
            let mut ack = Box::new(Ack::new(0));
            ack.lane = 1;
            ack.received = now;
            ack.count = 1;
            ack.ranges[0] = AckRange {
                start: 513,
                end: 513,
            };
            assert!(lane.begin_ack(ack).is_ok());
            assert!(lane.advance_report(&mut report, now, &mut 64).unwrap());
            lane.report_completed(&report).unwrap();
            let mut budget = 1;
            if lane.advance_report(&mut report, now, &mut budget).unwrap() {
                assert_eq!(budget, 0);
                assert_eq!(report.count, 1);
                assert_eq!(report.entries[0].number, 257);
                lane.report_completed(&report).unwrap();
                lost = true;
                break;
            }
            assert_eq!(budget, 0);
        }
        assert!(lost);
    }
}
