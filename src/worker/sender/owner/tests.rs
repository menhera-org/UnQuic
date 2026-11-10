use super::*;
use crate::ack::AckProgress;

fn owner(groups: usize, workers: usize, descriptors: &[(usize, u16, usize)]) -> Owner {
    owner_at(groups, workers, descriptors, Duration::ZERO)
}

fn owner_at(
    groups: usize,
    workers: usize,
    descriptors: &[(usize, u16, usize)],
    now: Duration,
) -> Owner {
    let budgets: Arc<[Budget]> = (0..groups).map(|_| Budget::new()).collect();
    Owner::new(workers, descriptors, now, budgets, false).unwrap()
}

fn lane(lane: u16) -> PacketId {
    PacketId {
        session: 0,
        generation: 1,
        lane,
        number: 0,
    }
}

fn sent(id: PacketId, count: u32, at: Duration) -> Sent {
    Sent {
        group: 0,
        id,
        count,
        at,
        mtu: 1280,
        cwnd_limited: false,
        restart: false,
    }
}

/// Charges and records count 1280-byte sends as one worker syscall would;
/// the window is not under test here.
fn send(owner: &Owner, count: u64, at: Duration) -> Vec<SendRecord> {
    let budget = &owner.budgets[0];
    let state = budget.published();
    let prior = budget.charge(count * 1280, None).unwrap();
    let mut batch = budget.begin(&state, prior, count, at);
    (0..count).map(|_| batch.push(1280, false, false)).collect()
}

fn entry(number: u64, record: SendRecord) -> Entry {
    Entry {
        number,
        sample: NonZeroU16::new(number as u16),
        settled: true,
        record,
    }
}

fn report(origin: usize, id: PacketId, kind: ReportKind, entries: &[Entry]) -> Box<Report> {
    let mut report = Box::new(Report::new(origin));
    report.id = id;
    report.kind = kind;
    report.count = entries.len();
    report.entries[..entries.len()].copy_from_slice(entries);
    report
}

fn ack(first: bool, end: Option<AckProgress>) -> ReportKind {
    ReportKind::Ack {
        received: Duration::from_millis(1),
        delay_us: 0,
        first,
        end,
    }
}

fn progress(packets: u32, sent_at: Duration) -> AckProgress {
    AckProgress {
        packets,
        eliciting: true,
        released_bytes: u64::from(packets) * 1280,
        rtt_sent_at: Some(sent_at),
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
fn first_ack_uses_controller_creation_time_at_any_uptime() {
    for start in [Duration::ZERO, Duration::from_secs(86_400)] {
        let mut owner = owner_at(1, 1, &[(0, 1, 0)], start);
        owner.timers.install(0, 1).unwrap();
        let records = send(&owner, 1, start);
        owner.sent(sent(lane(1), 1, start)).unwrap();
        owner.budgets[0].release(1280);
        let rtt = Duration::from_millis(1);
        owner
            .report(report(
                0,
                lane(1),
                ack(true, Some(progress(1, start))),
                &[entry(1, records[0])],
            ))
            .unwrap();
        let mut outgoing = VecDeque::with_capacity(8);
        owner.advance(start + rtt, &mut outgoing).unwrap();
        assert!(matches!(outgoing[0].1, Message::ReportReturn(_)));
        assert_eq!(
            owner.groups[0].controller.state(),
            crate::bbr::State::Startup
        );
    }
}

#[test]
fn the_group_datagram_fits_every_lane_not_only_the_last_sender() {
    let mut owner = owner(1, 2, &[(0, 1, 0), (0, 2, 0)]);
    owner.timers.install(0, 1).unwrap();
    let mut send = |number, mtu, micros| {
        owner
            .sent(Sent {
                mtu,
                ..sent(lane(number), 1, Duration::from_micros(micros))
            })
            .unwrap();
        owner.groups[0].controller.limits().max_datagram
    };
    assert_eq!(send(1, 9000, 1), 9000);
    assert_eq!(send(2, 9000, 2), 9000);
    // Lane 1 learns a 1500-byte path; lane 2 still sends 9000-byte datagrams,
    // so the quantum must keep fitting them or its probes are refused forever.
    assert_eq!(send(1, 1548, 3), 9000);
    assert_eq!(send(2, 1548, 4), 1548);
}

#[test]
fn acks_report_the_newest_record_across_chunks_and_exclude_other_reports() {
    let mut owner = owner(1, 2, &[(0, 1, 0), (0, 2, 0)]);
    owner.timers.install(0, 1).unwrap();
    let initial = owner.budgets[0].published();
    assert_eq!(initial.window, 10 * 1280);
    assert!(initial.rate != 0 && initial.quantum != 0);
    let records = send(&owner, 2, Duration::ZERO);
    owner
        .sent(Sent {
            restart: true,
            ..sent(lane(1), 2, Duration::ZERO)
        })
        .unwrap();
    assert_eq!(owner.timers.earliest(0).unwrap().1, Duration::ZERO);
    owner
        .report(report(0, lane(1), ack(true, None), &[entry(1, records[0])]))
        .unwrap();
    let mut outgoing = VecDeque::with_capacity(16);
    owner
        .advance(Duration::from_millis(1), &mut outgoing)
        .unwrap();
    assert_eq!(outgoing.pop_front().unwrap().0, 0);
    assert!(owner.groups[0].ack.is_some());
    // Another lane's report waits until the open ACK finishes.
    owner
        .report(report(1, lane(2), ReportKind::Loss, &[]))
        .unwrap();
    owner
        .advance(Duration::from_millis(2), &mut outgoing)
        .unwrap();
    assert!(outgoing.is_empty());
    owner.budgets[0].release(2560);
    owner
        .report(report(
            0,
            lane(1),
            ack(false, Some(progress(2, Duration::ZERO))),
            &[entry(2, records[1])],
        ))
        .unwrap();
    owner
        .advance(Duration::from_millis(2), &mut outgoing)
        .unwrap();
    assert_eq!(outgoing.pop_front().unwrap().0, 0);
    owner
        .advance(Duration::from_millis(2), &mut outgoing)
        .unwrap();
    assert_eq!(outgoing.pop_front().unwrap().0, 1);
    assert!(owner.groups[0].ack.is_none());
    assert_eq!(owner.timers.earliest(0), None);
    let published = owner.budgets[0].published();
    assert_eq!((published.delivered, published.acks), (2560, 1));
    // The sample's delivery time is when the owner began this ACK.
    assert_eq!(published.delivered_at, Duration::from_millis(1));
    assert_eq!(published.first_sent_at, Duration::ZERO);
}

#[test]
fn losses_enter_recovery_once_and_late_acks_still_deliver() {
    let mut owner = owner(1, 1, &[(0, 1, 0)]);
    owner.timers.install(0, 1).unwrap();
    let records = send(&owner, 3, Duration::ZERO);
    owner.sent(sent(lane(1), 3, Duration::ZERO)).unwrap();
    owner.budgets[0].release(1280);
    owner
        .report(report(
            0,
            lane(1),
            ReportKind::Loss,
            &[entry(1, records[0])],
        ))
        .unwrap();
    let mut outgoing = VecDeque::with_capacity(8);
    owner
        .advance(Duration::from_millis(1), &mut outgoing)
        .unwrap();
    assert_eq!(owner.groups[0].recovery, Some(Duration::from_millis(1)));
    assert_eq!(owner.budgets[0].published().lost, 1280);
    assert_eq!(owner.timers.outstanding(0, 1, 1), Some(2));
    let late = Entry {
        settled: false,
        ..entry(1, records[0])
    };
    owner.budgets[0].release(2560);
    owner
        .report(report(
            0,
            lane(1),
            ack(true, Some(progress(3, Duration::ZERO))),
            &[late, entry(2, records[1]), entry(3, records[2])],
        ))
        .unwrap();
    owner
        .advance(Duration::from_millis(2), &mut outgoing)
        .unwrap();
    let published = owner.budgets[0].published();
    assert_eq!((published.delivered, published.lost), (3840, 1280));
    assert_eq!(owner.timers.earliest(0), None);
    // The ACK's newest packet predates recovery, so recovery continues.
    assert!(owner.groups[0].recovery.is_some());
}

#[test]
fn sent_feeds_timers_mtu_and_window_evidence() {
    for app_limited in [false, true] {
        let mut owner = owner(1, 1, &[(0, 1, 0)]);
        // Before this owner installs the lane, its sends are not PTO bases.
        owner.sent(sent(lane(1), 1, Duration::ZERO)).unwrap();
        assert_eq!(owner.timers.earliest(0), None);
        owner.timers.install(0, 1).unwrap();
        owner
            .sent(Sent {
                mtu: 1500,
                ..sent(lane(1), 1, Duration::ZERO)
            })
            .unwrap();
        assert_eq!(owner.groups[0].controller.limits().max_datagram, 1500);
        assert!(!owner.groups[0].cwnd_limited);
        owner
            .sent(Sent {
                cwnd_limited: true,
                ..sent(lane(1), 1, Duration::from_micros(1))
            })
            .unwrap();
        assert!(owner.groups[0].cwnd_limited);
        if app_limited {
            owner.groups[0].sampler.application_limited(0);
        }
        // An idle restart inside an application bubble discards the evidence.
        owner
            .sent(Sent {
                restart: true,
                ..sent(lane(1), 1, Duration::from_micros(2))
            })
            .unwrap();
        assert_eq!(owner.groups[0].cwnd_limited, !app_limited);
        assert_eq!(owner.timers.outstanding(0, 1, 1), Some(3));
    }
}

#[test]
fn stalled_pto_publishes_a_zero_window_and_its_probe_round() {
    let mut owner = owner(1, 1, &[(0, 1, 0)]);
    owner.timers.install(0, 1).unwrap();
    assert_eq!(owner.budgets[0].published().window, 12_800);
    let now = stalled(&mut owner.groups[0]);
    owner.groups[0].epoch = 5;
    let mut outgoing = VecDeque::with_capacity(8);
    owner.advance(now, &mut outgoing).unwrap();
    let published = owner.budgets[0].published();
    assert_eq!(published.window, 0);
    assert_ne!(published.quantum, 0, "probes still get a burst");
    assert_eq!(owner.budgets[0].probe_epoch(), 5);
    owner.probe_replied(0).unwrap();
    owner.advance(now, &mut outgoing).unwrap();
    assert_eq!(owner.budgets[0].published().window, 12_800);
}

#[test]
fn group_sweeps_preserve_earliest_deadline_then_remove_stale_minimum() {
    let mut owner = owner(65, 1, &[(0, 1, 0), (1, 1, 64)]);
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
fn pending_pto_without_target_survives_install() {
    let mut owner = owner(1, 1, &[(0, 1, 0)]);
    let now = stalled(&mut owner.groups[0]);
    owner.groups[0].epoch = 7;
    owner.groups[0].probe = Some((None, 2));
    let next = owner.groups[0].pto.deadline(&owner.groups[0].rtt, None);
    let mut outgoing = VecDeque::with_capacity(8);
    assert!(!owner.advance(now, &mut outgoing).unwrap());
    assert!(outgoing.is_empty());
    assert_eq!(owner.groups[0].probe, Some((None, 2)));
    owner.timers.install(0, 1).unwrap();
    assert!(owner.advance(now, &mut outgoing).unwrap());
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
    assert_eq!(
        owner.groups[0].pto.deadline(&owner.groups[0].rtt, None),
        next
    );
    assert!(owner.groups[0].pto.stalled());
}

#[test]
fn returned_probe_retargets_replacement_without_resetting_its_epoch() {
    let mut owner = owner(1, 1, &[(0, 1, 0)]);
    owner.timers.install(0, 1).unwrap();
    // A 100 ms period, so backoff is visible below the one-second cap.
    assert!(
        owner.groups[0]
            .rtt
            .sample(Duration::ZERO, Duration::from_millis(33), 0)
    );
    let now = stalled(&mut owner.groups[0]);
    let state = &mut owner.groups[0];
    state.epoch = 9;
    state.probe = Some((None, 2));
    let unbacked = state.rtt.pto_period(0);
    assert!(state.pto.deadline(&state.rtt, Some(now)) > Some(now + unbacked));
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
    owner.install(0, 2).unwrap();
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
    // The committed replacement resets silence and backoff, as a new ACK would.
    assert!(!owner.groups[0].pto.stalled());
    assert_eq!(
        owner.groups[0]
            .pto
            .deadline(&owner.groups[0].rtt, Some(now)),
        Some(now + unbacked)
    );
    assert_eq!(owner.groups[0].epoch, 9);
    owner.return_probe(0, 8, 2).unwrap();
    assert!(owner.groups[0].probe.is_none());
}

#[test]
fn only_a_new_generation_resets_the_groups_pto() {
    let mut owner = owner(1, 1, &[(0, 1, 0)]);
    owner.install(0, 1).unwrap();
    assert!(
        owner.groups[0]
            .rtt
            .sample(Duration::ZERO, Duration::from_millis(33), 0)
    );
    let now = stalled(&mut owner.groups[0]);
    owner.install(0, 1).unwrap();
    assert!(owner.groups[0].pto.stalled());
    assert!(
        owner.groups[0]
            .pto
            .deadline(&owner.groups[0].rtt, Some(now))
            > Some(now + owner.groups[0].rtt.pto_period(0))
    );
    owner.timers.quiesce(0, 1).unwrap();
    owner.install(0, 2).unwrap();
    let state = &owner.groups[0];
    assert!(!state.pto.stalled());
    assert_eq!(
        state.pto.deadline(&state.rtt, Some(now)),
        Some(now + state.rtt.pto_period(0))
    );
}

#[test]
fn returned_probe_counts_merge_within_one_epoch_and_reject_overflow_atomically() {
    let mut owner = owner(1, 1, &[(0, 1, 0)]);
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

#[test]
fn ready_reports_do_not_overrun_the_turn_budget() {
    let mut owner = owner(1, 2, &[(0, 1, 0), (0, 2, 0)]);
    owner.timers.install(0, 1).unwrap();
    for worker in 0..2 {
        let id = lane(worker as u16 + 1);
        let records = send(&owner, 40, Duration::ZERO);
        owner.sent(sent(id, 40, Duration::ZERO)).unwrap();
        let entries: Vec<_> = (0..40)
            .map(|index| entry(index as u64 + 1, records[index]))
            .collect();
        owner
            .report(report(
                worker,
                id,
                ack(true, Some(progress(40, Duration::ZERO))),
                &entries,
            ))
            .unwrap();
    }
    let mut outgoing = VecDeque::with_capacity(16);
    assert!(
        owner
            .advance(Duration::from_millis(1), &mut outgoing)
            .unwrap()
    );
    assert_eq!(outgoing.len(), 1, "the second report exceeds the budget");
    owner
        .advance(Duration::from_millis(1), &mut outgoing)
        .unwrap();
    assert_eq!(outgoing.len(), 2);
    assert_eq!(owner.timers.earliest(0), None);
}

mod loss_barrier {
    use super::*;

    fn setup() -> (Owner, VecDeque<(usize, Message)>) {
        let mut owner = owner(1, 1, &[(0, 1, 0)]);
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
        let entry = Entry {
            sample: None,
            ..entry(1, SendRecord::default())
        };
        owner
            .report(report(0, lane(1), ReportKind::Loss, &[entry]))
            .unwrap();
        owner.advance(now, outgoing).unwrap();
        assert!(matches!(
            outgoing.pop_front().unwrap().1,
            Message::ReportReturn(_)
        ));
    }

    #[test]
    fn stalled_pto_reaches_an_idle_session_beside_an_unanswered_lane() {
        let mut owner = owner(1, 1, &[(0, 1, 0), (1, 1, 0)]);
        owner.timers.install(0, 1).unwrap();
        owner.timers.install(1, 1).unwrap();
        let mut now = Duration::ZERO;
        for _ in 0..3 {
            let state = &mut owner.groups[0];
            now = state.pto.deadline(&state.rtt, Some(now)).unwrap();
            state
                .pto
                .expire(now, &state.rtt, Some(Duration::ZERO))
                .unwrap();
        }
        // A probe sent after the forced settlement remains unanswered.
        owner.timers.submitted(0, 1, 1, 2, now).unwrap();
        let mut outgoing = VecDeque::with_capacity(8);
        let mut reached = 0;
        for _ in 0..2 {
            let (_, sent) = owner.timers.earliest(0).unwrap();
            let state = &owner.groups[0];
            now = state.pto.deadline(&state.rtt, Some(sent)).unwrap();
            let (token, force) = check(&mut owner, &mut outgoing, now);
            assert!(!force);
            owner.loss_checked(0, token, 0, None).unwrap();
            owner.advance(now, &mut outgoing).unwrap();
            let (_, Message::Probe { session, count, .. }) = outgoing.pop_front().unwrap() else {
                panic!("missing bounded PTO allowance")
            };
            assert_eq!(count, 2);
            assert!(outgoing.is_empty());
            reached |= 1 << session;
            owner
                .timers
                .submitted(session, 1, 1, u32::from(count), now)
                .unwrap();
        }
        assert_ne!(
            reached & 2,
            0,
            "an unanswered lane monopolized all PTO probes"
        );
        assert!(owner.groups[0].pto.stalled());
        owner.probe_replied(0).unwrap();
        assert!(!owner.groups[0].pto.stalled());
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
}
