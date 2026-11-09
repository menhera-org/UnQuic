use super::*;

fn workers() -> ([Worker; 2], ReceiveSocket) {
    workers_with_lanes(2)
}

fn workers_with_lanes(lanes: usize) -> ([Worker; 2], ReceiveSocket) {
    let (_, mut workers) = super::super::tests::test_workers_with_lanes(lanes);
    let params = wire::Params {
        port: 40000,
        max_frame_len: 9022,
        mandatory_features: 0,
        optional_features: 0,
    };
    let connection = Connection {
        role: handshake::Role::Initiator,
        initiator: Tag::new(1).unwrap(),
        responder: Tag::new(2).unwrap(),
        local: params,
        peer: wire::Params {
            port: 40001,
            ..params
        },
        max_frame_len: 9022,
    };
    for worker in &mut workers {
        worker
            .command(
                0,
                Command {
                    generation: 1,
                    phase: Phase::Install,
                    connection: Some(connection),
                },
                0,
            )
            .unwrap();
        worker.unpaced_fixture = false;
        worker.io_time = Some(Ok);
    }
    for from in 0..2 {
        let to = 1 - from;
        let (tx, rx) = crate::spsc::channel();
        workers[from].outgoing[to] = Some(tx);
        workers[to].incoming[from] = Some(rx);
    }
    Worker::init_senders(&mut workers).unwrap();
    let rx = ReceiveSocket::bind("[::1]:40005".parse().unwrap(), None).unwrap();
    for (index, worker) in workers.iter_mut().enumerate() {
        worker.io_time = Some(|_| Ok(Duration::from_millis(1)));
        worker.install_sender(0, 1, true).unwrap();
        worker.transmit[0].paths[0].lanes[index].socket =
            Some(TransmitSocket::connect(&rx, rx.local_addr()).unwrap());
    }
    (workers, rx)
}

fn queue_frame(worker: &mut Worker, length: usize) {
    let mut frame = worker.free.pop().unwrap();
    frame.session = 0;
    frame.lane = worker.id;
    frame.tag = 1;
    frame.length = length;
    frame.bytes[..length].fill(0xa5);
    frame.deadline = Duration::from_millis(100);
    frame.tx_reserved = true;
    worker.sessions[0].tx_retained += 1;
    worker.sessions[0].tx_bytes += frame.length;
    worker.input_frame(frame, Duration::ZERO).unwrap();
}

fn submit_original(worker: &mut Worker, slot: usize, range: std::ops::Range<usize>) {
    assert!(
        worker
            .frames
            .recovery_mut(slot)
            .unwrap()
            .submitted(range.clone(), 0)
    );
    let (session, lane) = worker.frame_owner(slot).unwrap();
    worker.release_unsent(session, lane, range.len()).unwrap();
}

fn frame(worker: &mut Worker) {
    queue_frame(worker, 60);
    worker.flush_frames(Duration::ZERO).unwrap();
}

fn service(workers: &mut [Worker; 2]) {
    for worker in workers {
        worker.drain_queues(Duration::ZERO).unwrap();
        worker.service_sender(Duration::ZERO).unwrap();
    }
}

#[test]
#[ignore = "kernel descriptor test; run in a disposable container"]
fn fresh_claim_clock_survives_older_worker_turn() {
    let (mut workers, rx) = workers();
    frame(&mut workers[1]);
    for _ in 0..8 {
        service(&mut workers);
    }
    let mut bytes = [0; BASE_UDP_PAYLOAD];
    let length = rx.recv(&mut bytes).unwrap().0;
    let datagram = Datagram::decode(&bytes[..length]).unwrap();
    assert_eq!(datagram.header().lane, 2);
    let number = datagram.header().packet_number;
    assert!(
        datagram
            .records()
            .any(|record| matches!(record, Record::Data { bytes: data, .. } if data == [0xa5; 60]))
    );
    let index = workers[1].sender_index(0, 1).unwrap();
    let packet = workers[1].sender.lanes[index]
        .history
        .packet(number)
        .unwrap();
    assert_eq!(packet.sent_at, Duration::from_millis(1));
    assert!(packet.sample_slot.is_some());
    assert!(workers[1].sender.job.is_none());
    assert!(workers[1].sessions[0].tx_retained != 0);
    assert_eq!(
        workers[1]
            .recovery
            .journal
            .packet(usize::from(packet.sample_slot.unwrap().get()) - 1)
            .count(),
        1
    );
}

#[test]
#[ignore = "kernel descriptor test; run in a disposable container"]
fn local_ack_releases_the_shared_charge_in_the_same_turn() {
    let (mut workers, rx) = workers();
    frame(&mut workers[0]);
    for _ in 0..8 {
        service(&mut workers);
    }
    let mut bytes = [0; BASE_UDP_PAYLOAD];
    let length = rx.recv(&mut bytes).unwrap().0;
    let number = Datagram::decode(&bytes[..length])
        .unwrap()
        .header()
        .packet_number;
    assert!(
        workers[0]
            .sender
            .owner
            .as_ref()
            .unwrap()
            .snapshot(0)
            .unwrap()
            .bytes_in_flight
            != 0
    );
    let now = Duration::from_millis(2);
    for worker in &mut workers {
        worker.io_time = Some(Ok);
    }
    let ranges = [AckRange {
        start: number,
        end: number,
    }];
    workers[0]
        .incoming_ack(0, 1, 1, 0, AckRanges::new(&ranges).unwrap(), now)
        .unwrap();
    workers[0].drain_queues(now).unwrap();
    workers[0].service_sender(now).unwrap();
    let index = workers[0].sender_index(0, 0).unwrap();
    assert_eq!(workers[0].sender.lanes[index].history.inflight_bytes(), 0);
    assert_eq!(
        workers[0]
            .sender
            .owner
            .as_ref()
            .unwrap()
            .snapshot(0)
            .unwrap()
            .bytes_in_flight,
        0,
        "the lane owner releases its ACKed charge before the next claim"
    );
    assert!(!workers[0].sender.lanes[index].report_pending);
    assert!(workers[0].sender.report.is_some());
}

#[test]
#[ignore = "kernel descriptor test; run in a disposable container"]
fn steady_control_does_not_starve_local_data_or_consume_another_claim() {
    let (mut workers, rx) = workers();
    for worker in &mut workers {
        worker.io_time = Some(Ok);
    }
    let index = workers[0].sender_index(0, 0).unwrap();
    let mut previous = 0;
    for turn in 1..=8 {
        let now = Duration::from_millis(turn);
        // Pacing holds the plan while control takes the lane's next number.
        workers[0].sender.budgets[0].hold_pacing(Duration::MAX);
        frame(&mut workers[0]);
        let old = workers[0].sender.job.unwrap().first;
        let ranges = [wire::AckRange { start: 1, end: 1 }];
        let control = workers[0]
            .send_record(
                0,
                Record::Ack {
                    lane: 1,
                    delay_us: 0,
                    ranges: wire::AckRanges::new(&ranges).unwrap(),
                },
                now,
            )
            .unwrap()
            .unwrap()
            .0;
        assert!(control > old && old > previous);
        workers[0].sender.budgets[0].hold_pacing(Duration::ZERO);
        for _ in 0..8 {
            for worker in &mut workers {
                worker.drain_queues(now).unwrap();
                worker.service_sender(now).unwrap();
            }
        }
        let mut bytes = [0; BASE_UDP_PAYLOAD];
        let length = rx.recv(&mut bytes).unwrap().0;
        assert_eq!(
            Datagram::decode(&bytes[..length])
                .unwrap()
                .header()
                .packet_number,
            control
        );
        let length = rx.recv(&mut bytes).unwrap().0;
        let data = Datagram::decode(&bytes[..length]).unwrap();
        previous = data.header().packet_number;
        assert!(previous > control);
        assert!(
            data.records()
                .any(|r| matches!(r, Record::Data { bytes, .. } if bytes == [0xa5; 60]))
        );
        let history = &workers[0].sender.lanes[index].history;
        assert!(history.packet(previous).unwrap().sample_slot.is_some());
        assert_eq!(
            history.validate(
                wire::AckRanges::new(&[wire::AckRange {
                    start: old,
                    end: old
                }])
                .unwrap()
            ),
            Err(crate::ack::HistoryError::Unsent)
        );
        assert!(workers[0].sender.job.is_none());
        assert!(workers[0].sessions[0].tx_retained != 0);
    }
    assert_eq!(workers[0].counters.send_grants, 8);
    assert_eq!(workers[0].counters.cancelled_grants, 0);
}

#[test]
#[ignore = "kernel descriptor test; run in a disposable container"]
fn control_overtaking_a_waiting_plan_refreshes_it_when_claimed() {
    let (mut workers, rx) = workers();
    let worker = &mut workers[0];
    let held = Duration::from_millis(5);
    worker.sender.budgets[0].hold_pacing(held);
    frame(worker);
    let job = worker.sender.job.unwrap();
    assert!(!worker.sender.permitted);
    assert_eq!(worker.sender.retry, Some(held));
    let now = Duration::from_millis(1);
    let control = worker
        .send_record(
            0,
            Record::ProbeReply {
                lane: 1,
                packet_number: 99,
                payload_len: BASE_UDP_PAYLOAD as u16,
            },
            now,
        )
        .unwrap()
        .unwrap()
        .0;
    assert!(control > job.first);
    workers[0].io_time = Some(|_| Ok(Duration::from_millis(5)));
    for _ in 0..8 {
        service(&mut workers);
    }
    let mut bytes = [0; BASE_UDP_PAYLOAD];
    let length = rx.recv(&mut bytes).unwrap().0;
    assert_eq!(
        Datagram::decode(&bytes[..length])
            .unwrap()
            .header()
            .packet_number,
        control
    );
    let length = rx.recv(&mut bytes).unwrap().0;
    let data = Datagram::decode(&bytes[..length]).unwrap();
    assert!(data.header().packet_number > control);
    assert!(data.records().any(|r| matches!(r, Record::Data { .. })));
    assert_eq!(workers[0].counters.send_grants, 1);
    assert_eq!(workers[0].counters.cancelled_grants, 0);
}

#[test]
#[ignore = "kernel descriptor test; run in a disposable container"]
fn inactive_install_does_not_quiesce_a_newer_uninstalled_timer_generation() {
    let (mut workers, _rx) = workers();
    let connection = workers[0].sessions[0].connection;
    let id = PacketId {
        session: 0,
        generation: 1,
        lane: 1,
        number: 0,
    };
    assert!(workers[0].sender.owner.as_ref().unwrap().timers.matches(id));
    for worker in &mut workers {
        worker
            .command(
                0,
                Command {
                    generation: 2,
                    phase: Phase::Quiesce,
                    connection: None,
                },
                0,
            )
            .unwrap();
    }
    workers[0].quiesced_sender(0).unwrap();
    for worker in &mut workers {
        worker
            .command(
                0,
                Command {
                    generation: 2,
                    phase: Phase::Install,
                    connection: None,
                },
                0,
            )
            .unwrap();
        assert!(worker.sender_idle(0));
    }
    assert!(!workers[0].sender.owner.as_ref().unwrap().timers.matches(id));
    for worker in &mut workers {
        worker
            .command(
                0,
                Command {
                    generation: 3,
                    phase: Phase::Quiesce,
                    connection,
                },
                0,
            )
            .unwrap();
    }
    // Timers retain generation 1 while each lane has inactive generation 2.
    workers[0].quiesced_sender(0).unwrap();
    for worker in &mut workers {
        worker
            .command(
                0,
                Command {
                    generation: 3,
                    phase: Phase::Install,
                    connection,
                },
                0,
            )
            .unwrap();
    }
    let owner = workers[0].sender.owner.as_ref().unwrap();
    assert!(owner.timers.matches(PacketId {
        generation: 3,
        ..id
    }));
    assert!(!owner.timers.matches(id));
    assert!(owner.timers.earliest(0).is_none());
}

#[test]
#[ignore = "kernel descriptor test; run in a disposable container"]
fn acked_partial_batches_resume_after_recycle_without_waiting_for_expiry() {
    partial_batches_with_feedback(false);
}

#[test]
#[ignore = "kernel descriptor test; run in a disposable container"]
fn trimmed_grants_refresh_only_the_submitted_prefix_under_feedback() {
    partial_batches_with_feedback(true);
}

fn partial_batches_with_feedback(feedback: bool) {
    let (mut workers, rx) = workers();
    for worker in &mut workers {
        worker.io_time = Some(Ok);
        for _ in 0..96 {
            queue_frame(worker, 1514);
        }
    }
    let mut received = [[0usize; 96]; 2];
    let mut next_packet = [1_u64; 2];
    let mut delivered = [0_u64; 2];
    let mut receiver = [
        reassembly::Lane::new(1, 9022).unwrap(),
        reassembly::Lane::new(2, 9022).unwrap(),
    ];
    let mut complete = 0;
    let mut storage = [0; BASE_UDP_PAYLOAD];
    for tick in 0..100 {
        let now = Duration::from_millis(tick);
        for worker in &mut workers {
            worker.drain_queues(now).unwrap();
            worker.service_sender(now).unwrap();
            worker.drain_tap_inputs(now).unwrap();
            worker.flush_frames(now).unwrap();
            if feedback && worker.id == 0 && worker.sender.job.is_some() {
                worker
                    .send_record(
                        0,
                        Record::ProbeReply {
                            lane: 1,
                            packet_number: 99,
                            payload_len: BASE_UDP_PAYLOAD as u16,
                        },
                        now,
                    )
                    .unwrap();
            }
        }
        for _ in 0..64 {
            let length = match rx.recv(&mut storage) {
                Ok((length, _)) => length,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("{error}"),
            };
            let datagram = Datagram::decode(&storage[..length]).unwrap();
            let header = datagram.header();
            let owner = usize::from(header.lane - 1);
            assert!(
                header.packet_number >= next_packet[owner],
                "packet numbers must increase across control and DATA sends"
            );
            if !feedback {
                assert_eq!(
                    header.packet_number, next_packet[owner],
                    "never-attempted suffix must retain its original identity"
                );
                assert_eq!(receiver[owner].admit(&datagram, now), Admission::Fresh);
                while let Some(frame) = receiver[owner].front(now) {
                    delivered[owner] += 1;
                    assert_eq!(frame.id, ((owner as u64 + 1) << 48) | delivered[owner]);
                    assert_eq!(frame.bytes.len(), 1514);
                    assert!(frame.bytes.iter().all(|&byte| byte == 0xa5));
                    assert!(receiver[owner].finish_front());
                }
            }
            next_packet[owner] = header.packet_number + 1;
            for record in datagram.records() {
                if let Record::Data {
                    frame_id,
                    total_len,
                    offset,
                    bytes,
                } = record
                {
                    let sequence = (frame_id & ((1 << 48) - 1)) as usize;
                    let position = &mut received[owner][sequence - 1];
                    assert_eq!(*position, offset as usize);
                    assert!(bytes.iter().all(|&byte| byte == 0xa5));
                    *position += bytes.len();
                    if *position == total_len as usize {
                        complete += 1;
                    }
                    if sequence == 96 {
                        assert!(
                            received[1 - owner][0] != 0,
                            "one lane drained before the other progressed"
                        );
                    }
                }
            }
            let ranges = [AckRange {
                start: header.packet_number,
                end: header.packet_number,
            }];
            let observed = now + Duration::from_micros(100);
            workers[owner]
                .incoming_ack(
                    0,
                    1,
                    header.lane,
                    0,
                    AckRanges::new(&ranges).unwrap(),
                    observed,
                )
                .unwrap();
            // Return the reserved ACK/report buffers before accepting another ACK.
            for _ in 0..3 {
                for worker in &mut workers {
                    worker.drain_queues(observed).unwrap();
                    worker.service_sender(observed).unwrap();
                }
            }
        }
        if complete == 192 {
            break;
        }
    }
    assert_eq!(complete, 192, "received {received:?}");
    if !feedback {
        assert_eq!(delivered, [96; 2]);
        for (index, lane) in receiver.iter_mut().enumerate() {
            let mut output = [AckRange { start: 0, end: 0 }; wire::MAX_ACK_RANGES];
            let Some(Record::Ack { ranges, .. }) =
                lane.take_ack(Duration::from_millis(100), &mut output)
            else {
                panic!()
            };
            assert_eq!(ranges.len(), 1);
            let range = ranges.iter().next().unwrap();
            assert_eq!((range.start, range.end), (1, next_packet[index] - 1));
        }
    }
    assert_eq!(workers[0].counters.cancelled_grants, 0);
    for worker in &mut workers {
        while worker.cleanup_frames().unwrap() {}
        assert_eq!(worker.sessions[0].tx_retained, 0);
        assert_eq!(worker.counters.expired_frames, 0);
        assert_eq!(worker.counters.queue_drops, 0);
    }
}

#[test]
#[ignore = "kernel descriptor test; run in a disposable container"]
fn crossed_group_acks_finish_before_loss_checks_use_the_only_report() {
    let (mut workers, _rx) = workers();
    let descriptors = [(0, 1, 0), (0, 2, 0), (1, 1, 1), (1, 2, 1)];
    let budgets: Arc<[Budget]> = (0..2).map(|_| Budget::new()).collect();
    let mut owner = Owner::new(2, &descriptors, Duration::ZERO, Arc::clone(&budgets)).unwrap();
    for session in 0..2 {
        owner.timers.install(session, 1).unwrap();
    }
    for worker in workers.iter_mut() {
        let command = worker.sessions[0].installed.unwrap();
        worker.command(1, command, 0).unwrap();
        worker.install_sender(1, 1, true).unwrap();
        worker.sender.owner = None;
        worker.sender.budgets = Arc::clone(&budgets);
        worker.sender.fresh.resize(2, false);
        worker.sender.probes.resize(2, None);
        worker.sender.losses.resize(2, None);
        for lane in &mut worker.sender.lanes {
            lane.group = lane.session;
            for number in 1..=130 {
                lane.history.reserve(number, 1280, true, true).unwrap();
                assert!(lane.submitted(number, Duration::ZERO, None));
            }
            owner
                .timers
                .submitted(lane.session, lane.lane, 1, 130, Duration::ZERO)
                .unwrap();
        }
        let session = 1 - worker.id;
        let mut ack = worker.sender.ack_free.pop().unwrap();
        ack.session = session;
        ack.tag = 1;
        ack.lane = worker.id as u16 + 1;
        ack.received = Duration::from_millis(1);
        ack.count = 1;
        ack.ranges[0] = AckRange { start: 1, end: 130 };
        worker.sender.incoming.push_back(ack);
        worker.begin_sender_ack().unwrap();
        let index = worker.sender_index(session, worker.id).unwrap();
        let mut report = worker.sender.report.take().unwrap();
        assert!(
            worker.sender.lanes[index]
                .advance_report(&mut report, Duration::from_millis(1), &mut 64,)
                .unwrap()
        );
        assert!(matches!(report.kind, ReportKind::Ack { end: None, .. }));
        owner.report(report).unwrap();
    }
    let mut outgoing = VecDeque::with_capacity(16);
    for _ in 0..2 {
        owner
            .advance(Duration::from_millis(1), &mut outgoing)
            .unwrap();
    }
    while let Some((worker, message)) = outgoing.pop_front() {
        workers[worker]
            .sender_message(message, Duration::from_millis(1))
            .unwrap();
    }
    workers[0].sender.owner = Some(owner);
    for worker in &mut workers {
        assert_eq!(worker.sender.acks_pending, 1);
        worker
            .sender_message(
                Message::CheckLoss {
                    group: worker.id,
                    token: 1,
                    before: Duration::ZERO,
                    force: true,
                },
                Duration::from_millis(1),
            )
            .unwrap();
        // New duplicate feedback must remain queued while the accepted partial ACK finishes.
        let mut duplicate = worker.sender.ack_free.pop().unwrap();
        duplicate.session = 1 - worker.id;
        duplicate.tag = 1;
        duplicate.lane = worker.id as u16 + 1;
        duplicate.received = Duration::from_millis(1);
        duplicate.count = 1;
        duplicate.ranges[0] = AckRange { start: 1, end: 130 };
        worker.sender.incoming.push_back(duplicate);
        worker.begin_sender_ack().unwrap();
        assert_eq!(worker.sender.incoming.len(), 1);
    }
    for _ in 0..100 {
        service(&mut workers);
    }
    for worker in &workers {
        assert_eq!(worker.sender.checks_pending, 0);
        assert_eq!(worker.sender.acks_pending, 0);
        assert!(worker.sender.incoming.is_empty());
        assert!(worker.sender.report.is_some());
        assert!(worker.sender.lanes.iter().all(|lane| {
            lane.ack.is_none()
                && !lane.report_pending
                && lane.history.outstanding().next().is_none()
        }));
    }
    let owner = workers[0].sender.owner.as_ref().unwrap();
    assert!(owner.timers.earliest(0).is_none());
    assert!(owner.timers.earliest(1).is_none());
}

#[test]
#[ignore = "kernel descriptor test; run in a disposable container"]
fn loss_check_waits_for_final_report_return_and_survives_replacement_and_queue_pressure() {
    let (mut workers, _rx) = workers();
    let worker = &mut workers[1];
    let index = worker.sender_index(0, 1).unwrap();
    for number in 1..=65 {
        worker.sender.lanes[index]
            .history
            .reserve(number, 1280, true, true)
            .unwrap();
        assert!(worker.sender.lanes[index].submitted(number, Duration::ZERO, None));
    }
    worker
        .sender_message(
            Message::CheckLoss {
                group: 0,
                token: 7,
                before: Duration::ZERO,
                force: true,
            },
            Duration::ZERO,
        )
        .unwrap();
    assert!(worker.advance_loss_checks(Duration::ZERO, &mut 64).unwrap());
    assert!(!worker.stage_recovery().unwrap());
    let (_, Message::Report(report)) = worker.sender.outgoing.pop_front().unwrap() else {
        panic!()
    };
    assert!(report.count < 65);
    worker.setup_due = Duration::MAX;
    worker.mtu_due = Duration::MAX;
    let other = worker.sender_index(1, 1).unwrap();
    worker.sender.lanes[other]
        .begin_loss_check(Duration::ZERO, false)
        .unwrap();
    assert_eq!(worker.sender.lanes[other].deadline(), Some(Duration::ZERO));
    assert_eq!(worker.deadline(Duration::ZERO), Some(Duration::MAX));
    assert!(!worker.advance_loss_checks(Duration::ZERO, &mut 64).unwrap());
    assert!(worker.sender.outgoing.is_empty());
    worker
        .sender_message(Message::ReportReturn(report), Duration::ZERO)
        .unwrap();
    for _ in 0..4 {
        worker.advance_loss_checks(Duration::ZERO, &mut 64).unwrap();
        worker.stage_recovery().unwrap();
        if let Some((_, Message::Report(report))) = worker.sender.outgoing.pop_front() {
            worker
                .sender_message(Message::ReportReturn(report), Duration::ZERO)
                .unwrap();
        }
        if worker.sender.lanes[index].scan.is_none() {
            break;
        }
    }
    // A replacement is allowed only after all reports and old history have drained.
    while worker.sender.lanes[index]
        .history
        .retire_oldest_resolved()
        .unwrap()
        .is_some()
    {}
    worker.sender.lanes[index].install(2).unwrap();
    let capacity = worker.sender.outgoing.capacity();
    for _ in 0..capacity {
        worker
            .sender
            .outgoing
            .push_back((0, Message::ProbeReplied { group: 0 }));
    }
    assert!(!worker.advance_loss_checks(Duration::ZERO, &mut 64).unwrap());
    assert_eq!(worker.sender.checks_pending, 1);
    worker.sender.outgoing.clear();
    assert!(worker.advance_loss_checks(Duration::ZERO, &mut 64).unwrap());
    assert_eq!(worker.sender.checks_pending, 0);
    assert!(matches!(
        worker.sender.outgoing.pop_front().unwrap().1,
        Message::LossChecked {
            token: 7,
            worker: 1,
            ..
        }
    ));
}

#[test]
#[ignore = "kernel descriptor test; run in a disposable container"]
fn sender_queue_pressure_preserves_destination_order_across_partial_sweeps() {
    let (mut workers, _rx) = workers();
    let [worker, peer] = &mut workers;
    let mut filled = 0;
    while worker
        .push(1, Event::Sender(Message::ProbeReplied { group: 0 }))
        .is_ok()
    {
        filled += 1;
    }
    assert!(filled > 64);
    worker.sender.outgoing = VecDeque::with_capacity(128);
    for token in 1..=70 {
        worker
            .sender
            .queue(
                1,
                Message::CheckLoss {
                    group: 0,
                    token,
                    before: Duration::ZERO,
                    force: false,
                },
            )
            .unwrap();
        if token == 1 {
            worker
                .sender
                .queue(
                    0,
                    Message::Probe {
                        group: 0,
                        epoch: 0,
                        session: 0,
                        generation: 1,
                        lane: 1,
                        count: 1,
                    },
                )
                .unwrap();
        }
    }
    assert!(worker.flush_sender_messages(Duration::ZERO).unwrap());
    assert!(
        worker.sender.probes[0].is_some(),
        "another destination must still progress"
    );
    let incoming = peer.incoming[0].as_mut().unwrap();
    assert!(incoming.try_pop().is_some());
    // Space appears between budget chunks; the failed head still precedes its suffix.
    assert!(!worker.flush_sender_messages(Duration::ZERO).unwrap());
    let mut retained = 0;
    while let Some(event) = incoming.try_pop() {
        assert!(matches!(event, Event::Sender(Message::ProbeReplied { .. })));
        retained += 1;
    }
    assert_eq!(retained, filled - 1);
    for _ in 0..2 {
        worker.flush_sender_messages(Duration::ZERO).unwrap();
    }
    let mut received = Vec::new();
    while let Some(Event::Sender(Message::CheckLoss { token, .. })) = incoming.try_pop() {
        received.push(token);
    }
    assert_eq!(received, (1..=70).collect::<Vec<_>>());
    assert!(worker.sender.outgoing.is_empty());
}

#[test]
#[ignore = "kernel descriptor test; run in a disposable container"]
fn feedback_overtakes_a_request_held_by_pacing_without_starving_it() {
    let (mut workers, rx) = workers();
    for worker in &mut workers {
        worker.io_time = Some(Ok);
    }
    let now = Duration::from_millis(1);
    frame(&mut workers[0]);
    for _ in 0..8 {
        for w in &mut workers {
            w.drain_queues(now).unwrap();
            w.service_sender(now).unwrap();
        }
    }
    let mut bytes = [0; BASE_UDP_PAYLOAD];
    rx.recv(&mut bytes).unwrap();
    let later = Duration::from_millis(2);
    workers[0].sender.budgets[0].hold_pacing(later);
    frame(&mut workers[0]);
    let old = workers[0].sender.job.unwrap().first;
    for _ in 0..4 {
        for w in &mut workers {
            w.drain_queues(now).unwrap();
            w.service_sender(now).unwrap();
        }
    }
    assert_eq!(workers[0].counters.send_grants, 1);
    assert_eq!(workers[0].sender.job.unwrap().first, old);
    let control = workers[0]
        .send_record(
            0,
            Record::ProbeReply {
                lane: 1,
                packet_number: 99,
                payload_len: BASE_UDP_PAYLOAD as u16,
            },
            now,
        )
        .unwrap()
        .unwrap()
        .0;
    for _ in 0..8 {
        for w in &mut workers {
            w.drain_queues(later).unwrap();
            w.service_sender(later).unwrap();
        }
    }
    rx.recv(&mut bytes).unwrap();
    let length = rx.recv(&mut bytes).unwrap().0;
    let data = Datagram::decode(&bytes[..length]).unwrap();
    assert!(data.header().packet_number > control);
    assert!(data.records().any(|r| matches!(r, Record::Data { .. })));
    assert_eq!(workers[0].counters.send_grants, 2);
    assert_eq!(workers[0].counters.cancelled_grants, 0);
}

#[test]
#[ignore = "kernel descriptor test; run in a disposable container"]
fn overtaken_probe_refreshes_its_history_and_reply_identity() {
    let (mut workers, rx) = workers();
    workers[0]
        .request_probe(0, 0, BASE_UDP_PAYLOAD, false, None, Duration::ZERO)
        .unwrap();
    let old = workers[0].sender.job.unwrap().first;
    let control = workers[0]
        .send_record(
            0,
            Record::ProbeReply {
                lane: 1,
                packet_number: 99,
                payload_len: BASE_UDP_PAYLOAD as u16,
            },
            Duration::ZERO,
        )
        .unwrap()
        .unwrap()
        .0;
    for _ in 0..8 {
        service(&mut workers);
    }
    let mut bytes = [0; BASE_UDP_PAYLOAD];
    rx.recv(&mut bytes).unwrap();
    let length = rx.recv(&mut bytes).unwrap().0;
    let packet = Datagram::decode(&bytes[..length]).unwrap();
    let number = packet.header().packet_number;
    assert!(number > control && control > old);
    assert!(packet.records().any(|r| matches!(r, Record::Probe)));
    let index = workers[0].sender_index(0, 0).unwrap();
    let lane = &workers[0].sender.lanes[index];
    assert_eq!(lane.probes[1], Some((number, BASE_UDP_PAYLOAD as u16)));
    assert!(lane.history.packet(number).unwrap().sample_slot.is_some());
    assert_eq!(workers[0].counters.cancelled_grants, 0);
}

#[test]
#[ignore = "kernel descriptor test; run in a disposable container"]
fn refresh_history_pressure_reports_unused_and_preserves_frame() {
    let (mut workers, rx) = workers();
    let index = workers[0].sender_index(0, 0).unwrap();
    let pinned = workers[0].sessions[0].tx[0]
        .as_mut()
        .unwrap()
        .allocate_packets(1)
        .unwrap();
    let history = &mut workers[0].sender.lanes[index].history;
    history.reserve(pinned, 100, true, true).unwrap();
    assert!(history.submit(pinned, Duration::ZERO));
    workers[0].sender.budgets[0].hold_pacing(Duration::MAX);
    frame(&mut workers[0]);
    let retained = workers[0].sessions[0].tx_retained;
    let old = workers[0].sender.job.unwrap().first;
    let tx = workers[0].sessions[0].tx[0].as_mut().unwrap();
    // Control stops PROBE_HEADROOM short of the wall; the refreshed DATA then cannot fit.
    let mut skip = 65536 - 3 - PROBE_HEADROOM as usize;
    while skip != 0 {
        let count = skip.min(BATCH_SIZE);
        tx.allocate_packets(count).unwrap();
        skip -= count;
    }
    let control = workers[0]
        .send_record(
            0,
            Record::ProbeReply {
                lane: 1,
                packet_number: 99,
                payload_len: BASE_UDP_PAYLOAD as u16,
            },
            Duration::ZERO,
        )
        .unwrap()
        .unwrap()
        .0;
    assert_eq!(control, 65536 - PROBE_HEADROOM);
    workers[0].sender.budgets[0].hold_pacing(Duration::ZERO);
    for _ in 0..8 {
        service(&mut workers);
    }
    let index = workers[0].sender_index(0, 0).unwrap();
    assert!(workers[0].sender.lanes[index].history.packet(old).is_none());
    assert_eq!(workers[0].sender.lanes[index].history.inflight_bytes(), 100);
    assert!(workers[0].sender.job.is_none());
    assert_eq!(workers[0].sessions[0].tx_retained, retained);
    assert_eq!(workers[0].counters.cancelled_grants, 1);
    let mut bytes = [0; BASE_UDP_PAYLOAD];
    rx.recv(&mut bytes).unwrap();
    assert_eq!(
        rx.recv(&mut bytes).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    assert!(
        workers[0].sender.lanes[index]
            .history
            .lose(pinned)
            .is_some()
    );
    for worker in &mut workers {
        worker.io_time = Some(Ok);
    }
    let later = Duration::from_millis(2);
    workers[0].flush_frames(later).unwrap();
    for _ in 0..8 {
        for w in &mut workers {
            w.drain_queues(later).unwrap();
            w.service_sender(later).unwrap();
        }
    }
    let length = rx.recv(&mut bytes).unwrap().0;
    assert!(
        Datagram::decode(&bytes[..length])
            .unwrap()
            .records()
            .any(|r| matches!(r, Record::Data { .. }))
    );
    assert!(workers[0].sessions[0].tx_retained != 0);
}

#[test]
#[ignore = "kernel descriptor test; run in a disposable container"]
fn retained_suffix_reservations_survive_recycle_and_leave_on_discard() {
    for cause in 0..3 {
        let (mut workers, _rx) = refilled_request();
        let worker = &mut workers[1];
        let job = worker.sender.job.unwrap();
        let index = worker.sender_index(0, 1).unwrap();
        let batch = worker.sessions[0].batches[1].as_ref().unwrap();
        let first = batch.plan.packet(0).expect("ungranted suffix").number;
        assert_eq!(first, job.first);
        for number in first..job.first + job.count as u64 {
            assert!(worker.sender.lanes[index].history.is_reserved(number));
        }
        match cause {
            0 => {
                worker.expire_frames(Duration::from_millis(100)).unwrap();
            }
            1 => {
                let state = worker.transmit[0].paths[0].lanes[1].mtu.state();
                worker.mtu_changed(
                    0,
                    0,
                    1,
                    (BASE_UDP_PAYLOAD + 1, state),
                    Duration::from_millis(1),
                );
            }
            _ => {
                worker
                    .command(
                        0,
                        Command {
                            generation: 2,
                            phase: Phase::Quiesce,
                            connection: worker.sessions[0].connection,
                        },
                        0,
                    )
                    .unwrap();
                worker.progress_barriers(Duration::from_millis(1)).unwrap();
                assert!(worker.sessions[0].command.is_some());
                assert!(!worker.sender_idle(0));
                worker.flush_frames(Duration::from_millis(1)).unwrap();
            }
        }
        for number in job.first..job.first + job.count as u64 {
            assert!(
                !worker.sender.lanes[index].history.is_reserved(number),
                "discard cause {cause} stranded packet {number}"
            );
        }
        assert!(
            worker.sessions[0].batches[1]
                .as_ref()
                .unwrap()
                .plan
                .is_empty()
        );
        if cause == 2 {
            for _ in 0..(frames::CAPACITY / BATCH_SIZE + 16) {
                for worker in &mut workers {
                    worker.io_time = Some(Ok);
                    worker.drain_queues(Duration::from_millis(2)).unwrap();
                    worker.service_sender(Duration::from_millis(2)).unwrap();
                    worker.flush_frames(Duration::from_millis(2)).unwrap();
                }
            }
            assert!(workers[1].sender_idle(0));

            workers[1]
                .progress_barriers(Duration::from_millis(2))
                .unwrap();
            assert!(workers[1].sessions[0].command.is_none());
        }
    }
}

#[test]
#[ignore = "kernel descriptor test; run in a disposable container"]
fn partial_attempt_discards_all_unsent_reservations_and_retries_fresh_numbers() {
    let (mut workers, rx) = refilled_request();
    let worker = &mut workers[1];
    let job = worker.sender.job.unwrap();
    let now = Duration::from_millis(2);
    worker.sender.budgets[0].hold_pacing(Duration::ZERO);
    assert_eq!(worker.claim_sender(now).unwrap(), Claimed::Ready);
    assert!(worker.sender.flight.packets().len() > 1);
    let mut batch = worker.sessions[0].batches[1].take().unwrap();
    let inputs: Vec<_> = batch.frames[..batch.len]
        .iter()
        .map(|id| worker.frames.input(id.unwrap()).unwrap())
        .collect();
    let submitted: Vec<_> = batch
        .plan
        .pieces(0)
        .unwrap()
        .map(|(slot, range)| (batch.frames[slot].unwrap(), range.end))
        .collect();
    let mut parts = [&[][..]; pack::BATCH_SLICES];
    let bytes: Vec<_> = batch
        .plan
        .slices(0, &inputs, &mut parts)
        .unwrap()
        .iter()
        .flat_map(|part| part.iter().copied())
        .collect();
    let granted = worker.sender.flight.packets().len();
    assert!(worker.reserve_pieces(&batch, granted).unwrap());
    worker.transmit[0].paths[0].lanes[1]
        .socket
        .as_ref()
        .unwrap()
        .send(&bytes, BASE_UDP_PAYLOAD)
        .unwrap();
    worker.finish_sender_send(1, Some(now), false).unwrap();
    worker.submitted_batch(0, 1, &mut batch, 1, false).unwrap();
    assert!(batch.plan.is_empty());
    let index = worker.sender_index(0, 1).unwrap();
    assert!(
        worker.sender.lanes[index]
            .history
            .packet(job.first)
            .unwrap()
            .sample_slot
            .is_some()
    );
    for number in job.first + 1..job.first + job.count as u64 {
        assert!(!worker.sender.lanes[index].history.is_reserved(number));
    }
    for (id, end) in submitted {
        if let Some(frame) = worker.frames.frame(id) {
            assert_eq!(
                (frame.offset, frame.deadline),
                (end, Duration::from_millis(100))
            );
        }
    }
    batch
        .prepare(
            worker.sessions[0].tx[1].as_mut().unwrap(),
            &worker.frames,
            BASE_UDP_PAYLOAD,
        )
        .unwrap();
    assert!(batch.plan.packet(0).unwrap().number >= job.first + job.count as u64);
    worker.sessions[0].batches[1] = Some(batch);
    let mut received = [0; BASE_UDP_PAYLOAD];
    let length = rx.recv(&mut received).unwrap().0;
    assert_eq!(&received[..length], bytes);
}

#[test]
#[ignore = "kernel descriptor test; run in a disposable container"]
fn canceled_higher_reservation_discards_an_unreserved_old_plan_suffix() {
    let (mut workers, _rx) = workers();
    let worker = &mut workers[1];
    queue_frame(worker, 9022);
    let mut batch = worker.sessions[0].batches[1].take().unwrap();
    batch.frames[0] = batch.pending.pop_front(&mut worker.frames).unwrap();
    batch.len = 1;
    batch
        .prepare(
            worker.sessions[0].tx[1].as_mut().unwrap(),
            &worker.frames,
            BASE_UDP_PAYLOAD,
        )
        .unwrap();
    assert!(batch.plan.len() > 1);
    let first = batch.plan.packet(0).unwrap();
    let overtaker = worker.sessions[0].tx[1]
        .as_mut()
        .unwrap()
        .allocate_packets(1)
        .unwrap();
    let index = worker.sender_index(0, 1).unwrap();
    let history = &mut worker.sender.lanes[index].history;
    history
        .reserve(first.number, (first.length + 48) as u16, true, true)
        .unwrap();
    history.reserve(overtaker, 100, false, false).unwrap();
    assert!(history.cancel(overtaker));
    assert_eq!(history.highest_sent(), 0);
    // Pacing keeps the renumbered plan waiting so its identity is observable.
    worker.sender.budgets[0].hold_pacing(Duration::MAX);
    worker
        .flush_batch(0, 1, &mut batch, Duration::from_millis(1))
        .unwrap();
    assert!(!worker.sender.lanes[index].history.is_reserved(first.number));
    assert!(worker.sender.job.unwrap().first > overtaker);
    worker.sessions[0].batches[1] = Some(batch);
}

#[test]
#[ignore = "kernel descriptor test; run in a disposable container"]
fn ack_buffer_pressure_counts_only_excess_and_reuses_returned_storage() {
    let (mut workers, _rx) = workers();
    frame(&mut workers[1]);
    for _ in 0..8 {
        service(&mut workers);
    }
    let index = workers[1].sender_index(0, 1).unwrap();
    let number = workers[1].sender.lanes[index].history.highest_sent();
    assert!(workers[1].sender.lanes[index].history.inflight_bytes() != 0);
    let ranges = [AckRange {
        start: number,
        end: number,
    }];
    let now = Duration::from_millis(2);
    for worker in &mut workers {
        worker.io_time = Some(Ok);
    }
    assert_eq!(workers[0].sender.ack_free.len(), ACK_BUFFERS);
    for _ in 0..ACK_BUFFERS {
        workers[0]
            .incoming_ack(0, 1, 2, 0, AckRanges::new(&ranges).unwrap(), now)
            .unwrap();
    }
    assert!(workers[0].sender.ack_free.is_empty());
    assert_eq!(workers[0].sender.outgoing.len(), ACK_BUFFERS);
    assert_eq!(workers[0].counters.ack_buffer_drops, 0);
    for _ in 0..2 {
        workers[0]
            .incoming_ack(0, 1, 2, 0, AckRanges::new(&ranges).unwrap(), now)
            .unwrap();
    }
    assert_eq!(workers[0].sender.outgoing.len(), ACK_BUFFERS);
    assert_eq!(workers[0].counters.ack_buffer_drops, 2);
    assert_eq!(workers[0].counters.queue_drops, 2);
    assert_eq!(workers[0].sessions[0].totals.ack_buffer_drops, 2);
    assert_eq!(workers[0].sessions[0].totals.queue_drops, 2);
    for _ in 0..8 * ACK_BUFFERS {
        for worker in &mut workers {
            worker.drain_queues(now).unwrap();
            worker.service_sender(now).unwrap();
        }
    }
    assert_eq!(workers[0].sender.ack_free.len(), ACK_BUFFERS);
    assert_eq!(workers[1].sender.lanes[index].history.inflight_bytes(), 0);
    assert!(workers[1].sender.lanes[index].ack.is_none());
    workers[0]
        .incoming_ack(0, 1, 2, 0, AckRanges::new(&ranges).unwrap(), now)
        .unwrap();
    assert_eq!(workers[0].sender.ack_free.len(), ACK_BUFFERS - 1);
    for _ in 0..8 {
        for worker in &mut workers {
            worker.drain_queues(now).unwrap();
            worker.service_sender(now).unwrap();
        }
    }
    assert_eq!(workers[0].sender.ack_free.len(), ACK_BUFFERS);
    assert_eq!(workers[0].counters.ack_buffer_drops, 2);
    assert_eq!(workers[0].counters.queue_drops, 2);
}

#[test]
#[ignore = "kernel descriptor test; run in a disposable container"]
fn retained_plan_refills_before_requesting_another_grant() {
    refilled_request();
}

fn refilled_request() -> ([Worker; 2], ReceiveSocket) {
    refilled_request_with_gap(false)
}

fn refilled_request_with_gap(gap: bool) -> ([Worker; 2], ReceiveSocket) {
    let (mut workers, rx) = workers();
    for _ in 0..16 {
        queue_frame(&mut workers[1], 1514);
    }
    workers[1].flush_frames(Duration::ZERO).unwrap();
    for _ in 0..8 {
        service(&mut workers);
    }
    let worker = &mut workers[1];
    assert!(worker.sender.job.is_none());
    let batch = worker.sessions[0].batches[1].as_ref().unwrap();
    let retained: Vec<_> = (0..batch.plan.len())
        .map(|i| batch.plan.packet(i).unwrap())
        .collect();
    assert!(!retained.is_empty() && retained.len() <= 16);
    let mut bytes = [0; BASE_UDP_PAYLOAD];
    let mut number = 1;
    while let Ok((length, _)) = rx.recv(&mut bytes) {
        let packet = Datagram::decode(&bytes[..length]).unwrap();
        assert_eq!(packet.header().packet_number, number);
        number += 1;
    }
    assert_eq!(number, retained[0].number);
    let now = Duration::from_millis(2);
    let ack = [AckRange {
        start: 1,
        end: number - 1,
    }];
    worker
        .incoming_ack(0, 1, 2, 0, AckRanges::new(&ack).unwrap(), now)
        .unwrap();
    for _ in 0..8 {
        for worker in &mut workers {
            worker.io_time = Some(Ok);
            worker.drain_queues(now).unwrap();
            worker.service_sender(now).unwrap();
        }
    }
    let worker = &mut workers[1];
    if gap {
        let number = worker.sessions[0].tx[1]
            .as_mut()
            .unwrap()
            .allocate_packets(1)
            .unwrap();
        let index = worker.sender_index(0, 1).unwrap();
        let history = &mut worker.sender.lanes[index].history;
        history.reserve(number, 100, false, false).unwrap();
        assert!(history.cancel(number));
    }
    for _ in 0..16 {
        queue_frame(worker, 1514);
    }
    // Hold pacing so the refilled plan waits with its reservations.
    worker.sender.budgets[0].hold_pacing(Duration::MAX);
    worker.flush_frames(now).unwrap();
    let job = worker.sender.job.unwrap();
    if !gap {
        assert!(
            job.count > retained.len(),
            "retained {}, requested {}",
            retained.len(),
            job.count
        );
    }
    let batch = worker.sessions[0].batches[1].as_ref().unwrap();
    for (index, packet) in retained.iter().enumerate() {
        assert_eq!(batch.plan.packet(index), Some(*packet));
    }
    (workers, rx)
}

#[test]
#[ignore = "kernel descriptor test; run in a disposable container"]
fn canceled_probe_defers_refill_until_reserved_suffix_drains() {
    let (mut workers, rx) = refilled_request_with_gap(true);
    let job = workers[1].sender.job.unwrap();
    let batch = workers[1].sessions[0].batches[1].as_ref().unwrap();
    for index in 0..job.count {
        assert_eq!(
            batch.plan.packet(index).unwrap().number,
            job.first + index as u64,
            "a retained suffix cannot share a Flight with packets beyond a canceled probe"
        );
    }
    let now = Duration::from_millis(2);
    workers[1].sender.budgets[0].hold_pacing(Duration::ZERO);
    for _ in 0..8 {
        for worker in &mut workers {
            worker.drain_queues(now).unwrap();
            worker.service_sender(now).unwrap();
        }
    }
    let mut bytes = [0; BASE_UDP_PAYLOAD];
    let mut count = 0;
    while let Ok((length, _)) = rx.recv(&mut bytes) {
        assert_eq!(
            Datagram::decode(&bytes[..length])
                .unwrap()
                .header()
                .packet_number,
            job.first + count
        );
        count += 1;
    }
    assert_eq!(count as usize, job.count);
    let ranges = [AckRange {
        start: job.first,
        end: job.first + count - 1,
    }];
    let observed = now + Duration::from_micros(100);
    workers[1]
        .incoming_ack(0, 1, 2, 0, AckRanges::new(&ranges).unwrap(), observed)
        .unwrap();
    for _ in 0..8 {
        for worker in &mut workers {
            worker.drain_queues(observed).unwrap();
            worker.service_sender(observed).unwrap();
        }
    }
    let index = workers[1].sender_index(0, 1).unwrap();
    assert_eq!(workers[1].sender.lanes[index].history.inflight_bytes(), 0);
    workers[1].sender.budgets[0].hold_pacing(Duration::MAX);
    workers[1].flush_frames(observed).unwrap();
    assert!(workers[1].sender.job.unwrap().first > job.first + count);
}

#[test]
#[ignore = "kernel descriptor test; run in a disposable container"]
fn repair_reports_budget_whole_packets_and_cleanup_before_handle_reuse() {
    let (mut workers, _rx) = workers();
    let worker = &mut workers[1];
    for _ in 0..64 {
        queue_frame(worker, 60);
    }
    let slots: Vec<_> = worker.sessions[0].batches[1]
        .as_ref()
        .unwrap()
        .pending
        .iter(&worker.frames)
        .collect();
    assert_eq!(slots.len(), 64);
    let mut pieces = [crate::ack::FramePiece::default(); 64];
    for attempt in 0..=2 {
        for (piece, &index) in pieces.iter_mut().zip(&slots) {
            let frame = worker.frames.frame(index).unwrap();
            *piece = crate::ack::FramePiece {
                frame_slot: index as u16,
                frame_id: frame.id,
                start: 0,
                end: 60,
                attempt,
            };
            if attempt == 0 {
                submit_original(worker, index, 0..60);
            } else {
                let state = worker.frames.recovery_mut(index).unwrap();
                assert!(state.lost(0..60, attempt - 1));
                assert!(state.submitted(0..60, attempt));
            }
        }
        assert!(
            worker
                .recovery
                .journal
                .reserve_packet(usize::from(attempt), &pieces)
        );
    }
    let mut report = worker.sender.report.take().unwrap();
    report.kind = ReportKind::Ack {
        received: Duration::from_millis(2),
        delay_us: 0,
        first: true,
        end: None,
    };
    // One additional physical copy keeps two mappings per frame after staging two ACKs.
    assert!(worker.recovery.journal.reserve_packet(3, &pieces));
    for handle in 1..=3 {
        assert_eq!(worker.sender.records.take().unwrap().get(), handle);
    }
    report.count = 3;
    for ordinal in 0..3 {
        report.entries[ordinal] = Entry {
            number: ordinal as u64 + 1,
            sample: NonZeroU16::new(ordinal as u16 + 1),
            settled: true,
            ..Entry::default()
        };
    }
    worker.recovery.staged = Some(report);
    assert!(worker.stage_recovery().unwrap());
    assert_eq!(worker.recovery.cursor, 2);
    assert_eq!(worker.counters.recovery_staging_yields, 1);
    assert_eq!(worker.counters.recovery_staged_reports, 0);
    assert_eq!(worker.counters.recovery_cleanup_peak, 64);
    assert!(
        !worker
            .sender
            .outgoing
            .iter()
            .any(|(_, message)| matches!(message, Message::Report(_)))
    );
    assert!(worker.recovery.journal.packet_empty(0));
    assert!(worker.recovery.journal.packet_empty(1));
    assert!(!worker.recovery.journal.packet_empty(2));
    assert!(
        slots
            .iter()
            .all(|&slot| worker.frames.recovery(slot).unwrap().remaining() == 0)
    );
    assert!(worker.cleanup_frames().unwrap());
    assert_eq!(worker.counters.recovery_cleanup_yields, 1);
    assert!(!worker.cleanup_frames().unwrap());
    assert_eq!(worker.counters.recovery_cleanup_yields, 1);
    assert_eq!(worker.sessions[0].tx_retained, 0);
    assert!(worker.recovery.journal.packet_empty(2));
    assert!(worker.recovery.journal.packet_empty(3));
    assert!(!worker.stage_recovery().unwrap());
    assert_eq!(worker.counters.recovery_staged_reports, 1);
    assert_eq!(worker.counters.recovery_staging_yields, 1);
    assert!(!worker.stage_recovery().unwrap());
    assert_eq!(worker.counters.recovery_staged_reports, 1);
    assert!(
        worker
            .sender
            .outgoing
            .iter()
            .any(|(_, message)| matches!(message, Message::Report(_)))
    );
    assert!(
        slots
            .iter()
            .all(|&slot| worker.frames.frame(slot).is_none())
    );
}

#[test]
#[ignore = "kernel descriptor test; run in a disposable container"]
fn lost_original_repairs_only_missing_bytes_and_late_ack_cancels_prepared_repair() {
    let (mut workers, rx) = workers();
    for worker in &mut workers {
        worker.io_time = Some(Ok);
    }
    queue_frame(&mut workers[1], 1514);
    for _ in 0..12 {
        for worker in &mut workers {
            worker.drain_queues(Duration::from_millis(1)).unwrap();
            worker.service_sender(Duration::from_millis(1)).unwrap();
            worker.flush_frames(Duration::from_millis(1)).unwrap();
        }
    }
    let mut bytes = [0; BASE_UDP_PAYLOAD];
    let first = rx.recv(&mut bytes).unwrap().0;
    let original = Datagram::decode(&bytes[..first]).unwrap();
    let number = original.header().packet_number;
    let Some(Record::Data {
        frame_id,
        offset: 0,
        bytes: head,
        ..
    }) = original.records().next()
    else {
        panic!()
    };
    let end = head.len();
    let tail = rx.recv(&mut bytes).unwrap().0;
    let tail = Datagram::decode(&bytes[..tail])
        .unwrap()
        .header()
        .packet_number;
    let lane = workers[1].sender_index(0, 1).unwrap();
    let sample = workers[1].sender.lanes[lane]
        .history
        .packet(number)
        .unwrap()
        .sample_slot
        .unwrap();
    let piece = workers[1]
        .recovery
        .journal
        .packet(usize::from(sample.get()) - 1)
        .next()
        .unwrap();
    let slot = usize::from(piece.frame_slot);
    assert_eq!(workers[1].frames.frame(slot).unwrap().id, frame_id);
    assert_eq!(workers[1].frames.frame(slot).unwrap().offset, 1514);
    assert!(workers[1].sessions[0].tx_retained != 0);
    let ranges = [AckRange {
        start: tail,
        end: tail,
    }];
    let now = Duration::from_millis(2);
    workers[1]
        .incoming_ack(0, 1, 2, 0, AckRanges::new(&ranges).unwrap(), now)
        .unwrap();
    for _ in 0..8 {
        for worker in &mut workers {
            worker.drain_queues(now).unwrap();
            worker.service_sender(now).unwrap();
        }
    }
    assert_eq!(workers[1].frames.recovery(slot).unwrap().remaining(), end);
    workers[1].sender.lanes[lane].force_loss(Duration::from_millis(3));
    for _ in 0..8 {
        for worker in &mut workers {
            worker.drain_queues(Duration::from_millis(3)).unwrap();
            worker.service_sender(Duration::from_millis(3)).unwrap();
        }
    }
    assert_eq!(
        workers[1].frames.recovery(slot).unwrap().next_repair(9022),
        Some((0..end, 1))
    );
    assert_eq!(
        workers[1].sessions[0].batches[1]
            .as_ref()
            .unwrap()
            .repairs
            .front(),
        Some(slot)
    );
    // Pacing holds the prepared repair so the late ACK can cancel it.
    workers[1].sender.budgets[0].hold_pacing(Duration::MAX);
    workers[1].flush_frames(Duration::from_millis(3)).unwrap();
    let batch = workers[1].sessions[0].batches[1].as_ref().unwrap();
    assert!(batch.repair && !batch.plan.is_empty());
    assert!(
        batch
            .plan
            .pieces(0)
            .unwrap()
            .all(|(_, range)| range.end <= end)
    );
    let ranges = [AckRange {
        start: number,
        end: number,
    }];
    workers[1]
        .incoming_ack(
            0,
            1,
            2,
            0,
            AckRanges::new(&ranges).unwrap(),
            Duration::from_millis(4),
        )
        .unwrap();
    // Apply local coverage before allowing the held repair request to reach its coordinator.
    workers[1].begin_sender_ack().unwrap();
    let mut report = workers[1].sender.report.take().unwrap();
    assert!(
        workers[1].sender.lanes[lane]
            .advance_report(&mut report, Duration::from_millis(4), &mut 64)
            .unwrap()
    );
    workers[1].recovery.staged = Some(report);
    workers[1].stage_recovery().unwrap();
    assert!(
        workers[1].sessions[0].batches[1]
            .as_ref()
            .unwrap()
            .plan
            .is_empty()
    );
    assert_eq!(workers[1].frames.recovery(slot).unwrap().remaining(), 0);
    workers[1].cleanup_frames().unwrap();
    assert!(workers[1].frames.frame(slot).is_none());
    assert!(
        workers[1]
            .recovery
            .journal
            .packet_empty(usize::from(sample.get()) - 1)
    );
    assert_eq!(workers[1].sessions[0].tx_retained, 0);
}

#[test]
#[ignore = "kernel descriptor test; run in a disposable container"]
fn mapping_pressure_preserves_fresh_fifo_and_leaves_reserved_space_for_repair() {
    let (mut workers, rx) = workers();
    let worker = &mut workers[1];
    worker.recovery = recovery::Recovery::new(3).unwrap();
    queue_frame(worker, 9022);
    queue_frame(worker, 60);
    queue_frame(worker, 60);
    let mut batch = worker.sessions[0].batches[1].take().unwrap();
    let old = batch
        .pending
        .pop_front(&mut worker.frames)
        .unwrap()
        .unwrap();
    let id = worker.frames.frame(old).unwrap().id;
    worker.frames.frame_mut(old).unwrap().offset = 9022;
    submit_original(worker, old, 0..9022);
    let state = worker.frames.recovery_mut(old).unwrap();
    assert!(state.lost(0..100, 0));
    worker.recovery.set(old, Place::Retained).unwrap();
    let pieces: Vec<_> = (0..48)
        .map(|piece| crate::ack::FramePiece {
            frame_slot: old as u16,
            frame_id: id,
            start: piece * 100,
            end: (piece + 1) * 100,
            attempt: 0,
        })
        .collect();
    assert!(worker.recovery.journal.reserve_packet(65534, &pieces));
    assert!(!worker.recovery.fresh_capacity());
    let first = batch
        .pending
        .pop_front(&mut worker.frames)
        .unwrap()
        .unwrap();
    let second = batch.pending.front().unwrap();
    batch.frames[0] = Some(first);
    batch.len = 1;
    worker.recovery.set(first, Place::Batch).unwrap();
    batch
        .prepare(
            worker.sessions[0].tx[1].as_mut().unwrap(),
            &worker.frames,
            BASE_UDP_PAYLOAD,
        )
        .unwrap();
    assert!(!worker.reserve_pieces(&batch, 1).unwrap());
    assert_eq!(worker.counters.recovery_mapping_pressure, 1);
    assert_eq!(worker.counters.recovery_selection_changed, 0);
    worker.yield_batch(0, 1, &mut batch).unwrap();
    assert_eq!(
        batch.pending.iter(&worker.frames).collect::<Vec<_>>(),
        [first, second]
    );
    worker.sessions[0].batches[1] = Some(batch);
    worker.queue_recovery(old).unwrap();
    worker.sender.budgets[0].hold_pacing(Duration::MAX);
    worker.flush_frames(Duration::from_millis(1)).unwrap();
    let batch = worker.sessions[0].batches[1].as_ref().unwrap();
    assert!(batch.repair);
    assert_eq!(batch.frames[0], Some(old));
    worker.sender.budgets[0].hold_pacing(Duration::ZERO);
    for _ in 0..8 {
        service(&mut workers);
    }
    let mut bytes = [0; BASE_UDP_PAYLOAD];
    let length = rx.recv(&mut bytes).unwrap().0;
    let packet = Datagram::decode(&bytes[..length]).unwrap();
    assert!(packet.records().any(|record| matches!(record,
        Record::Data { frame_id, offset: 0, bytes, .. } if frame_id == id && bytes.len() == 100)));
    assert_eq!(workers[1].counters.repairs, 1);
    assert_eq!(
        workers[1].frames.recovery(old).unwrap().next_repair(9022),
        None
    );
    assert_eq!(
        workers[1].sessions[0].batches[1]
            .as_ref()
            .unwrap()
            .pending
            .front(),
        Some(first)
    );
}

#[test]
#[ignore = "kernel descriptor test; run in a disposable container"]
fn continuous_backlog_cannot_starve_the_opposite_frame_class() {
    // Keep both full fresh batches below the input gate; test class scheduling separately.
    const FRAME_LEN: usize = 600;
    fn enqueue(worker: &mut Worker, repair: bool) -> u64 {
        queue_frame(worker, FRAME_LEN);
        let batch = worker.sessions[0].batches[1].as_ref().unwrap();
        let slot = batch.pending.iter(&worker.frames).last().unwrap();
        let id = worker.frames.frame(slot).unwrap().id;
        if repair {
            let mut batch = worker.sessions[0].batches[1].take().unwrap();
            assert!(worker.frames.unqueue(&mut batch.pending, slot).unwrap());
            worker.frames.frame_mut(slot).unwrap().offset = FRAME_LEN;
            submit_original(worker, slot, 0..FRAME_LEN);
            let coverage = worker.frames.recovery_mut(slot).unwrap();
            assert!(coverage.lost(0..FRAME_LEN, 0));
            worker.recovery.set(slot, Place::Retained).unwrap();
            worker.sessions[0].batches[1] = Some(batch);
            worker.queue_recovery(slot).unwrap();
        }
        id
    }

    for repair_first in [false, true] {
        let (mut workers, rx) = workers();
        for worker in &mut workers {
            worker.io_time = Some(Ok);
        }
        for _ in 0..pack::BATCH_CAPACITY {
            enqueue(&mut workers[1], repair_first);
        }
        workers[1].flush_frames(Duration::ZERO).unwrap();
        assert_eq!(
            workers[1].sessions[0].batches[1].as_ref().unwrap().len,
            pack::BATCH_CAPACITY
        );
        assert_eq!(
            workers[1].sessions[0].batches[1].as_ref().unwrap().repair,
            repair_first
        );
        let waiting = enqueue(&mut workers[1], !repair_first);
        let mut bytes = [0; BASE_UDP_PAYLOAD];
        let mut packets = 0;
        let mut seen = false;
        let mut retained_suffix = false;
        for tick in 1..2000 {
            let now = Duration::from_micros(tick * 10);
            let queued = {
                let batch = workers[1].sessions[0].batches[1].as_ref().unwrap();
                if repair_first {
                    batch.repairs.len()
                } else {
                    batch.pending.len()
                }
            };
            for _ in queued..pack::BATCH_CAPACITY {
                enqueue(&mut workers[1], repair_first);
            }
            for worker in &mut workers {
                worker.drain_queues(now).unwrap();
                worker.service_sender(now).unwrap();
                worker.flush_frames(now).unwrap();
            }
            let batch = workers[1].sessions[0].batches[1].as_ref().unwrap();
            retained_suffix |= workers[1].counters.tx_outer_bytes != 0 && !batch.plan.is_empty();
            for _ in 0..64 {
                let length = match rx.recv(&mut bytes) {
                    Ok((length, _)) => length,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) => panic!("{e}"),
                };
                packets += 1;
                let datagram = Datagram::decode(&bytes[..length]).unwrap();
                assert_eq!(datagram.header().lane, 2);
                seen |= datagram.records().any(|record| {
                    matches!(record,
                    Record::Data { frame_id, .. } if frame_id == waiting)
                });
                let number = datagram.header().packet_number;
                let ranges = [AckRange {
                    start: number,
                    end: number,
                }];
                let observed = now + Duration::from_micros(1);
                workers[1]
                    .incoming_ack(0, 1, 2, 0, AckRanges::new(&ranges).unwrap(), observed)
                    .unwrap();
                for _ in 0..3 {
                    for worker in &mut workers {
                        worker.drain_queues(observed).unwrap();
                        worker.service_sender(observed).unwrap();
                    }
                }
                // A fixed64-frame batch needs at most two baseline packets/frame.
                assert!(
                    seen || packets <= 2 * pack::BATCH_CAPACITY,
                    "opposite class starved: repair_first={repair_first}, packets={packets}"
                );
            }
            if seen {
                break;
            }
        }
        assert!(
            retained_suffix,
            "fixture must exercise a retained grant suffix"
        );
        assert!(
            seen,
            "opposite class made no progress: repair_first={repair_first}"
        );
        assert_eq!(workers[1].counters.expired_frames, 0);
        assert_eq!(workers[1].counters.cancelled_grants, 0);
    }
}

#[test]
#[ignore = "kernel descriptor test; run in a disposable container"]
fn admitted_acks_finish_then_loss_progresses_under_continuous_ack_input() {
    for (partial_scan, checked) in [(false, false), (true, false), (true, true)] {
        let (mut workers, _rx) = workers();
        let [peer, worker] = &mut workers;
        queue_frame(worker, 60);
        let mut batch = worker.sessions[0].batches[1].take().unwrap();
        let slot = batch
            .pending
            .pop_front(&mut worker.frames)
            .unwrap()
            .unwrap();
        let frame_id = worker.frames.frame(slot).unwrap().id;
        worker.frames.frame_mut(slot).unwrap().offset = 60;
        submit_original(worker, slot, 0..60);
        worker.recovery.set(slot, Place::Retained).unwrap();
        worker.sessions[0].batches[1] = Some(batch);
        assert!(worker.recovery.journal.reserve_packet(
            0,
            &[crate::ack::FramePiece {
                frame_slot: slot as u16,
                frame_id,
                start: 0,
                end: 60,
                attempt: 0,
            }]
        ));
        let index = worker.sender_index(0, 1).unwrap();
        // Packet 1's sampling handle is the worker's first record.
        assert_eq!(worker.sender.records.take(), NonZeroU16::new(1));
        for number in 1..=130 {
            worker.sender.lanes[index]
                .history
                .reserve(number, 1280, true, true)
                .unwrap();
            assert!(worker.sender.lanes[index].submitted(
                number,
                Duration::ZERO,
                if number == 1 {
                    NonZeroU16::new(1)
                } else {
                    None
                }
            ));
        }
        let now = Duration::from_millis(1);
        worker.sessions[1].connection = worker.sessions[0].connection;
        let other = worker.sender_index(1, 1).unwrap();
        worker.sender.lanes[other].install(1).unwrap();
        worker.sender.lanes[other]
            .history
            .reserve(1, 1280, true, true)
            .unwrap();
        assert!(worker.sender.lanes[other].submitted(1, Duration::ZERO, None));
        let covered = [AckRange { start: 1, end: 1 }];
        worker
            .incoming_ack(1, 1, 2, 0, AckRanges::new(&covered).unwrap(), now)
            .unwrap();
        if partial_scan {
            let lane = &mut worker.sender.lanes[index];
            let mut scan = lane.history.outstanding_scan();
            assert_eq!(scan.advance(&lane.history, &mut 64).unwrap().number, 1);
            lane.scan = Some(scan);
        }
        let accepted = [AckRange { start: 2, end: 130 }];
        worker
            .incoming_ack(0, 1, 2, 0, AckRanges::new(&accepted).unwrap(), now)
            .unwrap();
        worker.begin_sender_ack().unwrap();
        assert!(worker.sender.lanes[index].ack.is_some());
        if checked {
            worker
                .sender_message(
                    Message::CheckLoss {
                        group: 0,
                        token: 99,
                        before: now,
                        force: false,
                    },
                    now,
                )
                .unwrap();
        }
        let duplicate = [AckRange {
            start: 130,
            end: 130,
        }];
        let mut accepted_done = false;
        let mut other_done = false;
        let mut chunks = 0;
        let mut lost = false;
        let mut resumed = false;
        let mut held = None;
        let mut checked_done = !checked;
        for turn in 0..64 {
            // Keep input nonempty while the first, already-validated ACK is applied in chunks.
            if !worker.sender.ack_free.is_empty() {
                worker
                    .incoming_ack(0, 1, 2, 0, AckRanges::new(&duplicate).unwrap(), now)
                    .unwrap();
            }
            worker.service_sender(now).unwrap();
            if let Some(report) = held.take() {
                assert!(peer.incoming[1].as_mut().unwrap().try_pop().is_none());
                worker
                    .sender_message(Message::ReportReturn(report), now)
                    .unwrap();
                continue;
            }
            while let Some(event) = peer.incoming[1].as_mut().unwrap().try_pop() {
                let Event::Sender(message) = event else {
                    panic!("unexpected event")
                };
                if let Message::LossChecked { token: 99, .. } = message {
                    assert!(accepted_done && other_done && lost);
                    checked_done = true;
                    continue;
                }
                let Message::Report(report) = message else {
                    panic!("unexpected message")
                };
                match report.kind {
                    ReportKind::Ack { end, .. } => {
                        if report.id.session == 1 {
                            other_done |= end.is_some();
                        } else if !accepted_done {
                            chunks += 1;
                            if end.is_some() {
                                accepted_done = true;
                            }
                        } else if lost && end.is_some() {
                            resumed = true;
                        }
                    }
                    ReportKind::Loss => {
                        assert!(accepted_done && other_done, "loss overtook an accepted ACK");
                        assert_eq!(report.id.session, 0);
                        assert_eq!(report.count, 1);
                        assert_eq!(
                            report.entries[0].number, 1,
                            "ACK-covered packet declared lost"
                        );
                        lost = true;
                    }
                    ReportKind::Retire => panic!("unexpected retirement"),
                }
                if turn == 0 {
                    held = Some(report);
                } else {
                    worker
                        .sender_message(Message::ReportReturn(report), now)
                        .unwrap();
                }
            }
            if resumed {
                break;
            }
        }
        assert!(chunks >= 3, "fixture must span multiple ACK reports");
        assert!(
            lost,
            "continuous ACK admission starved an immediately eligible packet loss"
        );
        assert!(
            resumed,
            "waiting ACKs did not resume after the finite loss pass"
        );
        assert!(checked_done);
        assert_eq!(
            worker.frames.recovery(slot).unwrap().next_repair(9022),
            Some((0..60, 1))
        );
        assert_eq!(
            worker.sessions[0].batches[1]
                .as_ref()
                .unwrap()
                .repairs
                .front(),
            Some(slot)
        );
        assert_eq!(worker.counters.expired_frames, 0);
        // Stop feeding ACKs; neither the fence nor a report may pin generation replacement.
        for _ in 0..64 {
            worker.service_sender(now).unwrap();
            while let Some(Event::Sender(Message::Report(report))) =
                peer.incoming[1].as_mut().unwrap().try_pop()
            {
                worker
                    .sender_message(Message::ReportReturn(report), now)
                    .unwrap();
            }
            if worker.sender.incoming.is_empty()
                && worker.sender.acks_pending == 0
                && worker.sender.report.is_some()
                && worker.sender.lanes[index].scan.is_none()
                && worker.sender.lanes[index].loss_due.is_none()
            {
                break;
            }
        }
        worker.detach_frame(slot).unwrap();
        worker.release_frame(slot).unwrap();
        while worker.cleanup_frames().unwrap() {}
        for index in [index, other] {
            let lane = &mut worker.sender.lanes[index];
            while lane.history.retire_oldest_resolved().unwrap().is_some() {}
            assert!(lane.idle());
            lane.install(2).unwrap();
        }
    }
}

#[test]
#[ignore = "kernel descriptor test; run in a disposable container"]
fn an_empty_history_still_drains_its_ack_loss_fence_before_replacement() {
    let (mut workers, _rx) = workers();
    let [peer, worker] = &mut workers;
    let index = worker.sender_index(0, 1).unwrap();
    worker.sender.lanes[index]
        .history
        .reserve(1, 1280, true, true)
        .unwrap();
    assert!(worker.sender.lanes[index].submitted(1, Duration::ZERO, None));
    let now = Duration::from_millis(1);
    let future = [AckRange { start: 2, end: 2 }];
    worker
        .incoming_ack(0, 1, 2, 0, AckRanges::new(&future).unwrap(), now)
        .unwrap();
    worker.begin_sender_ack().unwrap();
    assert!(worker.sender.lanes[index].ack.is_none());
    let valid = [AckRange { start: 1, end: 1 }];
    worker
        .incoming_ack(0, 1, 2, 0, AckRanges::new(&valid).unwrap(), now)
        .unwrap();
    worker.service_sender(now).unwrap();
    let Some(Event::Sender(Message::Report(report))) = peer.incoming[1].as_mut().unwrap().try_pop()
    else {
        panic!("missing ACK report")
    };
    assert!(matches!(report.kind, ReportKind::Ack { end: Some(_), .. }));
    worker
        .sender_message(Message::ReportReturn(report), now)
        .unwrap();
    let lane = &mut worker.sender.lanes[index];
    // Completing the ACK retired its only entry; the owed scan still pins the lane.
    assert!(lane.history.is_retired());
    assert!(!lane.idle(), "empty history must finish its owed scan");
    assert_eq!(
        lane.install(2).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    for _ in 0..4 {
        worker.service_sender(now).unwrap();
    }
    assert!(worker.sender.lanes[index].idle());
    worker.sender.lanes[index].install(2).unwrap();
}

#[test]
#[ignore = "kernel descriptor test; run in a disposable container"]
fn transmit_expiry_classifies_successful_prefixes_once() {
    let (mut workers, _rx) = workers();
    let worker = &mut workers[1];
    for _ in 0..3 {
        queue_frame(worker, 60);
    }
    let slots: Vec<_> = worker.sessions[0].batches[1]
        .as_ref()
        .unwrap()
        .pending
        .iter(&worker.frames)
        .collect();
    submit_original(worker, slots[1], 0..30);
    submit_original(worker, slots[2], 0..60);
    assert!(!worker.expire_frames(Duration::from_millis(100)).unwrap());
    assert!(!worker.cleanup_frames().unwrap());
    for totals in [
        &worker.counters,
        &worker.sessions[0].totals,
        &worker.sessions[0].tx_totals[1],
    ] {
        assert_eq!(totals.expired_frames, 3);
        assert_eq!(totals.tx_expired_input_frames, 0);
        assert_eq!(totals.tx_expired_unsent_frames, 1);
        assert_eq!(totals.tx_expired_partial_frames, 1);
        assert_eq!(totals.tx_expired_sent_frames, 1);
        assert_eq!(
            totals.tx_expired_input_frames
                + totals.tx_expired_unsent_frames
                + totals.tx_expired_partial_frames
                + totals.tx_expired_sent_frames,
            totals.expired_frames
        );
    }
    assert!(!worker.expire_frames(Duration::from_millis(101)).unwrap());
    assert_eq!(worker.counters.expired_frames, 3);
}

#[test]
#[ignore = "kernel descriptor test; run in a disposable container"]
fn changed_coverage_rejection_is_distinct_from_mapping_pressure() {
    let (mut workers, _rx) = workers();
    let worker = &mut workers[1];
    queue_frame(worker, 60);
    let mut batch = worker.sessions[0].batches[1].take().unwrap();
    let index = batch
        .pending
        .pop_front(&mut worker.frames)
        .unwrap()
        .unwrap();
    batch.frames[0] = Some(index);
    batch.len = 1;
    batch
        .prepare(
            worker.sessions[0].tx[1].as_mut().unwrap(),
            &worker.frames,
            BASE_UDP_PAYLOAD,
        )
        .unwrap();
    submit_original(worker, index, 0..1);
    assert!(!worker.reserve_pieces(&batch, 1).unwrap());
    assert_eq!(worker.counters.recovery_selection_changed, 1);
    assert_eq!(worker.counters.recovery_mapping_pressure, 0);
    assert_eq!(worker.counters.cancelled_grants, 0);
}

#[test]
#[ignore = "kernel descriptor test; run in a disposable container"]
fn sent_mtu_quotes_require_exact_current_successful_history() {
    let (mut workers, rx) = workers();
    queue_frame(&mut workers[0], 1514);
    workers[0].flush_frames(Duration::ZERO).unwrap();
    for _ in 0..8 {
        service(&mut workers);
    }
    let worker = &mut workers[0];
    let mut bytes = [0; BASE_UDP_PAYLOAD];
    let length = rx.recv(&mut bytes).unwrap().0;
    let header = Header::decode(&bytes[..length]).unwrap();
    let quote = &bytes[..wire::HEADER_LEN];
    let proof = Some((length, Duration::from_millis(1)));
    assert_eq!(worker.sent_mtu_quote(0, 0, 0, quote), proof);
    for size in 0..wire::HEADER_LEN {
        assert_eq!(worker.sent_mtu_quote(0, 0, 0, &quote[..size]), None);
    }
    for byte in 0..wire::HEADER_LEN {
        let mut forged = [0; wire::HEADER_LEN];
        forged.copy_from_slice(quote);
        forged[byte] ^= 1;
        assert_eq!(worker.sent_mtu_quote(0, 0, 0, &forged), None, "byte {byte}");
    }
    for (peer, path, lane) in [(1, 0, 0), (0, 1, 0), (0, 0, 1), (0, 0, usize::MAX)] {
        assert_eq!(worker.sent_mtu_quote(peer, path, lane, quote), None);
    }
    let index = worker.sender_index(0, 0).unwrap();
    worker.sender.lanes[index].generation += 1;
    assert_eq!(worker.sent_mtu_quote(0, 0, 0, quote), None);
    worker.sender.lanes[index].generation -= 1;
    let connection = worker.sessions[0].connection.take();
    assert_eq!(worker.sent_mtu_quote(0, 0, 0, quote), None);
    worker.sessions[0].connection = connection;
    assert_eq!(worker.sent_mtu_quote(0, 0, 0, quote), proof);

    let number = worker.sender.lanes[index].history.highest_sent() + 1;
    worker.sender.lanes[index]
        .history
        .reserve(number, (length + 48) as u16, true, true)
        .unwrap();
    let mut unsent = [0; wire::HEADER_LEN];
    Header {
        packet_number: number,
        ..header
    }
    .encode(&mut unsent);
    assert_eq!(worker.sent_mtu_quote(0, 0, 0, &unsent), None);
    assert!(worker.sender.lanes[index].history.cancel(number));
    assert_eq!(worker.sent_mtu_quote(0, 0, 0, &unsent), None);
}

#[test]
#[ignore = "requires a disposable container and network namespace capabilities"]
fn data_quoted_ptb_reduces_only_after_successful_send() {
    // SAFETY: only this test thread and its children enter the disposable namespace.
    cvt(unsafe { libc::unshare(libc::CLONE_NEWNET) }).unwrap();
    assert!(
        std::process::Command::new("ip")
            .args(["link", "set", "lo", "mtu", "9000", "up"])
            .status()
            .unwrap()
            .success()
    );
    let (mut workers, rx) = workers();
    let worker = &mut workers[0];
    worker.transmit[0].paths[0].address = Some(rx.local_addr());
    let tx = &mut worker.transmit[0].paths[0].lanes[0];
    tx.socket.as_mut().unwrap().enable_mtu_discovery().unwrap();
    worker
        .reactor
        .add(tx.socket.as_ref().unwrap().as_fd(), TX_TOKEN, 0)
        .unwrap();
    // Establish a 1500-byte path before exercising the actual DATA/error-queue path.
    tx.mtu = pmtu::Path::new(1452, Duration::ZERO).unwrap();
    let mut now = Duration::ZERO;
    let mut number = 1;
    while tx.mtu.payload_limit() < 1452 {
        let length = tx.mtu.poll(now).unwrap();
        let id = pmtu::ProbeId {
            session: 0,
            tag: 1,
            lane: 1,
            packet: number,
        };
        assert!(tx.mtu.submitted(id, length, now));
        now += Duration::from_micros(1);
        assert!(tx.mtu.reply(id, length, now));
        number += 1;
    }
    queue_frame(worker, 1514);
    worker.flush_frames(Duration::ZERO).unwrap();
    for _ in 0..8 {
        service(&mut workers);
    }
    let worker = &mut workers[0];
    let mut bytes = [0; 1452];
    let (length, source) = rx.recv(&mut bytes).unwrap();
    assert_eq!(length, 1452);
    assert!(
        Datagram::decode(&bytes[..length])
            .unwrap()
            .records()
            .any(|r| matches!(r, Record::Data { .. }))
    );
    for forged in [true, false] {
        if forged {
            bytes[4] ^= 1;
        }
        super::super::tests::inject_ptb(source, rx.local_addr(), &bytes[..length], 1400);
        let mut events = [libc::epoll_event { events: 0, u64: 0 }; 4];
        let count = worker.reactor.wait(&mut events, 100).unwrap();
        assert!(
            events[..count]
                .iter()
                .any(|event| event.u64 == TX_TOKEN && event.events & libc::EPOLLERR as u32 != 0)
        );
        assert!(worker.mtu_error(0, 0, 0, Duration::ZERO).unwrap());
        assert_eq!(
            worker.transmit[0].paths[0].lanes[0].mtu.payload_limit(),
            if forged { 1452 } else { BASE_UDP_PAYLOAD }
        );
        if forged {
            bytes[4] ^= 1;
        }
    }
}

#[test]
#[ignore = "kernel descriptor test; run in a disposable container"]
fn busy_data_flight_preserves_due_liveness_for_each_session() {
    let (mut workers, rx) = workers();
    let connection = workers[0].sessions[0].connection.unwrap();
    for worker in &mut workers {
        worker.io_time = Some(Ok);
        worker
            .command(
                1,
                Command {
                    generation: 1,
                    phase: Phase::Install,
                    connection: Some(connection),
                },
                0,
            )
            .unwrap();
        for control in &mut worker.controls {
            control.retry = Duration::MAX;
            control.probe_due = Duration::ZERO;
        }
    }
    for control in &mut workers[0].controls {
        activate_liveness(control, connection);
    }

    workers[0].shared.bound[0].store(3, Ordering::Release);
    // Pacing keeps the first DATA plan busy until the first turn.
    workers[0].sender.budgets[0].hold_pacing(Duration::from_millis(1));
    frame(&mut workers[0]);
    assert!(workers[0].sender.job.is_some());
    workers[0].timers(Duration::ZERO).unwrap();
    workers[0].setup_due = Duration::MAX;
    workers[0].mtu_due = Duration::MAX;
    assert!(
        workers[0]
            .deadline(Duration::ZERO)
            .is_none_or(|due| due > Duration::ZERO),
        "a held Flight must not spin on overdue liveness"
    );

    let mut probes = [false; 2];

    let mut packets = 0;
    let mut bytes = [0; BASE_UDP_PAYLOAD];
    for turn in 1..=32 {
        let now = Duration::from_millis(turn);
        for worker in &mut workers {
            worker.drain_queues(now).unwrap();
            worker.service_sender(now).unwrap();
        }
        queue_frame(&mut workers[0], 60);
        workers[0].flush_frames(now).unwrap();
        workers[0].timers(now).unwrap();
        while let Ok((length, _)) = rx.recv(&mut bytes) {
            packets += 1;
            let packet = Datagram::decode(&bytes[..length]).unwrap();
            if packet
                .records()
                .any(|record| matches!(record, Record::Probe))
            {
                let session = workers[0]
                    .shared
                    .config
                    .session_for_cookie(packet.header().cookie)
                    .unwrap();
                assert!(
                    !probes[session],
                    "one overdue session must not overtake another"
                );
                probes[session] = true;
            }
        }
        if probes == [true; 2] {
            break;
        }
    }
    assert_eq!(
        probes, [true; 2],
        "busy DATA must not erase due liveness probes"
    );
    assert!(
        packets <= 6,
        "due probes must precede a continuing fresh backlog"
    );
    assert_eq!(workers[0].counters.expired_frames, 0);
    assert!(
        workers[0]
            .sessions
            .iter()
            .all(|s| s.totals.expired_frames == 0)
    );
}

fn activate_liveness(control: &mut SessionControl, connection: Connection) {
    control
        .state
        .reconnect(Duration::ZERO, Tag::new(1))
        .unwrap();
    control
        .state
        .on_record(
            Record::Challenge {
                initiator: 1,
                responder: 2,
                params: connection.peer,
            },
            Duration::ZERO,
            None,
        )
        .unwrap();
    control
        .state
        .on_record(
            Record::Ready {
                initiator: 1,
                responder: 2,
            },
            Duration::ZERO,
            None,
        )
        .unwrap();
    assert!(control.state.connection().is_some());
    control.retry = Duration::MAX;
    control.probe_due = Duration::ZERO;
}

#[test]
#[ignore = "kernel descriptor test; run in a disposable container"]
fn liveness_history_pressure_retries_without_spinning_or_losing_turn() {
    let (mut workers, _rx) = workers();
    let worker = &mut workers[0];
    for control in &mut worker.controls {
        control.retry = Duration::MAX;
    }
    let connection = worker.sessions[0].connection.unwrap();
    activate_liveness(&mut worker.controls[0], connection);
    worker.shared.bound[0].store(3, Ordering::Release);
    worker.setup_due = Duration::MAX;
    worker.mtu_due = Duration::MAX;
    worker.io_time = Some(Ok);
    let index = worker.sender_index(0, 0).unwrap();
    let tx = worker.sessions[0].tx[0].as_mut().unwrap();
    let pinned = tx.allocate_packets(1).unwrap();
    let mut remaining = 65535;
    while remaining != 0 {
        let count = remaining.min(BATCH_SIZE);
        tx.allocate_packets(count).unwrap();
        remaining -= count;
    }
    let history = &mut worker.sender.lanes[index].history;
    history.reserve(pinned, 100, true, true).unwrap();
    assert!(history.submit(pinned, Duration::ZERO));
    let next = worker.sessions[0].tx[0].as_ref().unwrap().next_packet();
    let report = worker.sender.report.take();
    worker.service_sender(Duration::ZERO).unwrap();
    assert!(worker.sender.job.is_none());
    assert!(worker.sender.reclaim.is_some());
    assert_eq!(worker.controls[0].probe_due, RETRY);
    assert_eq!(worker.deadline(Duration::ZERO), Some(RETRY));
    worker.service_sender(RETRY / 2).unwrap();
    assert_eq!(worker.controls[0].probe_due, RETRY);
    assert!(worker.sender.job.is_none());
    // Refused probes must not turn numbers into holes that move the history wall.
    assert_eq!(
        worker.sessions[0].tx[0].as_ref().unwrap().next_packet(),
        next
    );

    assert!(worker.sender.lanes[index].history.lose(pinned).is_some());
    worker.sender.report = report;
    worker.service_sender(RETRY).unwrap();
    // The probe is claimed and sent in the same turn.
    assert!(worker.controls[0].probe.is_some());
    assert_eq!(worker.controls[0].probe_due, RETRY + Duration::from_secs(1));
}

// Regression for a lane that stopped for seconds: its tail's ACK was dropped, DATA
// filled the history behind that tail, and every retry consumed new packet numbers,
// so neither DATA nor a PTO probe could ever reserve a number again.
#[test]
#[ignore = "kernel descriptor test; run in a disposable container"]
fn unacknowledged_tail_leaves_probe_headroom_without_consuming_numbers() {
    let (mut workers, _rx) = workers();
    let worker = &mut workers[1];
    let index = worker.sender_index(0, 1).unwrap();
    let tx = worker.sessions[0].tx[1].as_mut().unwrap();
    let pinned = tx.allocate_packets(1).unwrap();
    let mut remaining = crate::ack::SENT_CAPACITY - 2 * PROBE_HEADROOM as usize;
    while remaining != 0 {
        let count = remaining.min(BATCH_SIZE);
        tx.allocate_packets(count).unwrap();
        remaining -= count;
    }
    let history = &mut worker.sender.lanes[index].history;
    history.reserve(pinned, 1280, true, true).unwrap();
    assert!(history.submit(pinned, Duration::ZERO));
    queue_frame(worker, 1514);
    worker.flush_frames(Duration::ZERO).unwrap();
    assert!(
        worker.sender.job.is_none(),
        "DATA must leave room for a probe"
    );
    assert!(worker.sender.reclaim.is_some());
    let next = worker.sessions[0].tx[1].as_ref().unwrap().next_packet();
    for turn in 1..4 {
        worker.flush_frames(Duration::from_micros(turn)).unwrap();
        assert!(worker.sender.job.is_none());
    }
    assert_eq!(
        worker.sessions[0].tx[1].as_ref().unwrap().next_packet(),
        next,
        "a refused plan keeps its packet numbers"
    );
    worker
        .request_probe(
            0,
            1,
            BASE_UDP_PAYLOAD,
            false,
            None,
            Duration::from_millis(1),
        )
        .unwrap();
    assert!(matches!(
        worker.sender.job.map(|job| job.kind),
        Some(JobKind::Probe { .. })
    ));
}

#[test]
#[ignore = "kernel descriptor test; run in a disposable container"]
fn liveness_waits_for_barrier_and_socket_then_parks_waiting_data() {
    let (mut workers, _rx) = workers();
    let worker = &mut workers[0];
    for control in &mut worker.controls {
        control.retry = Duration::MAX;
    }

    let connection = worker.sessions[0].connection.unwrap();
    activate_liveness(&mut worker.controls[0], connection);
    worker.shared.bound[0].store(3, Ordering::Release);
    worker.setup_due = Duration::MAX;
    worker.mtu_due = Duration::MAX;
    worker.io_time = Some(Ok);
    worker.controls[0].barrier.begin(Some(connection)).unwrap();
    worker.service_sender(Duration::ZERO).unwrap();
    assert!(worker.sender.job.is_none());
    assert!(
        worker
            .deadline(Duration::ZERO)
            .is_none_or(|due| due > Duration::ZERO)
    );
    worker.controls[0].barrier = Barrier::new(2).unwrap();
    let socket = worker.transmit[0].paths[0].lanes[0].socket.take();
    worker.service_sender(Duration::ZERO).unwrap();
    assert!(worker.sender.job.is_none());
    assert_eq!(worker.controls[0].probe_due, RETRY);
    assert!(
        worker
            .deadline(Duration::ZERO)
            .is_none_or(|due| due > Duration::ZERO)
    );
    worker.transmit[0].paths[0].lanes[0].socket = socket;
    // A due probe parks a DATA plan waiting on pacing; the plan keeps its numbers.
    worker.sender.budgets[0].hold_pacing(Duration::MAX);
    queue_frame(worker, 60);
    worker.flush_frames(RETRY).unwrap();
    let data = worker.sender.job.unwrap();
    assert!(matches!(data.kind, JobKind::Data));
    worker.service_sender(RETRY).unwrap();
    assert!(matches!(
        worker.sender.job.map(|job| job.kind),
        Some(JobKind::Probe { .. })
    ));
    let index = worker.sender_index(0, 0).unwrap();
    assert!(worker.sender.lanes[index].history.is_reserved(data.first));
    worker.sender.budgets[0].hold_pacing(Duration::ZERO);
    worker.service_sender(RETRY).unwrap();
    assert!(worker.controls[0].probe.is_some());
    assert!(worker.sender.job.is_none());
}

const OVERSIZED_GSO_FRAMES: usize = FRESH_BYTES / 43 + 1;

fn gso_input(worker: &mut Worker, count: usize, size: u16) -> Box<Buffer> {
    gso_input_with_qinq(worker, count, size, false)
}

fn gso_input_with_qinq(worker: &mut Worker, count: usize, size: u16, qinq: bool) -> Box<Buffer> {
    use crate::tap::test_common::udp;
    use std::os::unix::net::UnixDatagram;
    let mut buffer = worker.free.pop().unwrap();
    let mut input = udp::frame(false, 42, &vec![7; count * usize::from(size)]);
    let mut header = udp::partial(&mut input, false);
    header[1] = 5;
    header[4..6].copy_from_slice(&size.to_ne_bytes());
    if qinq {
        for _ in 0..2 {
            input.splice(12..12, [0x81, 0, 0, 7]);
        }
        header[6..8].copy_from_slice(&42u16.to_ne_bytes());
    }
    let (fd, sender) = UnixDatagram::pair().unwrap();
    sender
        .send(&[header.as_slice(), input.as_slice()].concat())
        .unwrap();
    let mut tap = TapQueue::test_fd(fd.into(), true);
    (buffer.length, buffer.segments) = tap.read_input(&mut buffer.bytes).unwrap();
    buffer.session = 0;
    buffer.lane = worker.id;
    buffer.tag = 1;
    buffer.deadline = reassembly::FRAME_LIFETIME;
    buffer.tx_reserved = true;
    worker.sessions[0].tx_retained += 1;
    worker.sessions[0].tx_bytes += buffer.length;
    buffer
}

fn fill_fresh_budget(worker: &mut Worker) {
    queue_frame(worker, FRESH_BYTES % 43);
    let input = gso_input(worker, FRESH_BYTES / 43, 1);
    worker.input_frame(input, Duration::ZERO).unwrap();
    worker.drain_tap_inputs(Duration::ZERO).unwrap();
    let index = worker.sender_index(0, worker.id).unwrap();
    assert_eq!(worker.sender.lanes[index].unsent_bytes, FRESH_BYTES);
}

#[test]
#[ignore = "kernel descriptor test; run in a disposable container"]
fn fresh_materialization_gates_large_sources_without_losing_order_or_credit() {
    let (mut workers, _rx) = workers();
    let [worker, reader] = &mut workers;
    // This fixture installs directly, without running the coordinator barrier.
    assert!(worker.sessions[0].acknowledged.take().is_some());
    assert!(!worker.has_backpressure());
    let index = worker.sender_index(0, 0).unwrap();
    let limit = BATCH_SIZE * worker.transmit[0].paths[0].lanes[0].mtu.payload_limit();
    for _ in 0..4 {
        let input = gso_input_with_qinq(worker, 44, 1472, true);
        worker.input_frame(input, Duration::ZERO).unwrap();
    }
    while worker.drain_tap_inputs(Duration::ZERO).unwrap() {}
    let materialized =
        worker.sender.lanes[index].unsent_bytes - worker.sessions[0].unexpanded_bytes[0];
    assert!(materialized >= limit && materialized < limit + 1522);
    assert_eq!(worker.tap_capacity(0), 0);
    let sources: Vec<_> = worker.sessions[0].tap_inputs.iter().copied().collect();
    assert!(!worker.drain_tap_inputs(Duration::ZERO).unwrap());
    assert_eq!(
        worker.sessions[0]
            .tap_inputs
            .iter()
            .copied()
            .collect::<Vec<_>>(),
        sources
    );

    let free = reader.free.len();
    let mut input = gso_input(reader, 3, 100);
    input.lane = 0;
    worker.input_frame(input, Duration::ZERO).unwrap();
    assert_eq!(worker.returns[reader.id].len(), 1);
    let credit = &worker.returns[reader.id][0];
    assert_eq!((credit.session, credit.lane, credit.tag), (0, 0, 1));
    assert!(credit.tx_reserved && !credit.rx_reserved);
    assert!(!worker.has_backpressure());

    let mut receive = reader.free.pop().unwrap();
    receive.rx_reserved = true;
    reader.rx_retained += 1;
    worker.release(receive);
    reader.drain_queues(Duration::ZERO).unwrap();
    assert_eq!(reader.rx_retained, 0);
    assert_eq!(reader.free.len(), free - 1);
    worker.flush_returns();
    assert_eq!(worker.returns[reader.id].len(), 1);

    let mut sequence = 1;
    while worker.sender.lanes[index].unsent_bytes != 0 {
        let mut batch = worker.sessions[0].batches[0].take().unwrap();
        while let Some(frame) = batch.pending.pop_front(&mut worker.frames).unwrap() {
            assert_eq!(worker.frames.frame(frame).unwrap().id, (1 << 48) | sequence);
            sequence += 1;
            worker.release_frame(frame).unwrap();
        }
        worker.sessions[0].batches[0] = Some(batch);
        while worker.cleanup_frames().unwrap() {}
        worker.flush_returns();
        reader.drain_queues(Duration::ZERO).unwrap();
        worker.drain_tap_inputs(Duration::ZERO).unwrap();
    }
    assert_eq!(sequence, 4 * 44 + 3 + 1);
    assert_eq!(worker.sessions[0].unexpanded_bytes[0], 0);
    assert!(worker.sessions[0].tap_inputs.is_empty());
    assert!(worker.returns[reader.id].is_empty());
    assert_eq!(reader.free.len(), free);
    assert_eq!(reader.sessions[0].tx_retained, 0);
    assert!(worker.tap_capacity(0) > 0);
    assert_eq!(worker.counters.queue_drops, 0);
}

#[test]
#[ignore = "kernel descriptor test; run in a disposable container"]
fn fresh_materialization_tracks_discovered_mtu_and_resumes_on_expiry() {
    for ceiling in [BASE_UDP_PAYLOAD, 1452, 8952] {
        let (mut workers, _rx) = workers();
        let worker = &mut workers[0];
        let path = &mut worker.transmit[0].paths[0].lanes[0].mtu;
        *path = pmtu::Path::new(ceiling, Duration::ZERO).unwrap();
        let mut packet = 1;
        while path.payload_limit() != ceiling {
            let now = path.deadline();
            let length = path.poll(now).unwrap();
            let id = pmtu::ProbeId {
                session: 0,
                tag: 1,
                lane: 1,
                packet,
            };
            assert!(path.submitted(id, length, now));
            assert!(path.reply(id, length, now + Duration::from_millis(1)));
            packet += 1;
        }
        while !worker.fresh_full(0, 0) {
            queue_frame(worker, 9022);
        }
        let index = worker.sender_index(0, 0).unwrap();
        assert!(worker.sender.lanes[index].unsent_bytes >= BATCH_SIZE * ceiling);
        assert!(worker.sender.lanes[index].unsent_bytes < BATCH_SIZE * ceiling + 9022);
        queue_frame(worker, 60);
        assert_eq!(worker.sessions[0].unexpanded_bytes[0], 60);
        assert_eq!(worker.tap_capacity(0), 0);
        assert!(!worker.drain_tap_inputs(Duration::ZERO).unwrap());
        let now = reassembly::FRAME_LIFETIME;
        while worker.expire_frames(now).unwrap() {}
        while worker.cleanup_frames().unwrap() {}
        worker.drain_tap_inputs(now).unwrap();
        assert_eq!(worker.sender.lanes[index].unsent_bytes, 0);
        assert_eq!(worker.sessions[0].unexpanded_bytes[0], 0);
        assert!(worker.tap_capacity(0) > 0);
    }
}

#[test]
#[ignore = "kernel descriptor test; run in a disposable container"]
fn fresh_materialization_stalled_lane_does_not_block_another_lane() {
    let (mut workers, _rx) = workers_with_lanes(4);
    let worker = &mut workers[0];
    while !worker.fresh_full(0, 0) {
        queue_frame(worker, 9022);
    }
    queue_frame(worker, 60);
    queue_frame(worker, 60);
    let stalled: Vec<_> = worker.sessions[0].tap_inputs.iter().copied().collect();
    let mut input = gso_input(worker, 3, 100);
    input.lane = 2;
    worker.input_frame(input, Duration::ZERO).unwrap();
    assert!(worker.tap_can_expand(0));
    assert!(!worker.drain_tap_inputs(Duration::ZERO).unwrap());
    assert_eq!(
        worker.sessions[0]
            .tap_inputs
            .iter()
            .copied()
            .collect::<Vec<_>>(),
        stalled
    );
    let batch = worker.sessions[0].batches[2].as_ref().unwrap();
    assert_eq!(batch.pending.len(), 3);
    for (sequence, frame) in batch.pending.iter(&worker.frames).enumerate() {
        assert_eq!(
            worker.frames.frame(frame).unwrap().id,
            (3 << 48) | (sequence as u64 + 1)
        );
    }
    worker.sessions[0].connection = None;
    worker.drain_tap_inputs(Duration::ZERO).unwrap();
    assert!(worker.sessions[0].tap_inputs.is_empty());
    assert!(
        worker.sessions[0]
            .unexpanded_bytes
            .iter()
            .all(|&bytes| bytes == 0)
    );
}

#[test]
#[ignore = "kernel descriptor test; run in a disposable container"]
fn fresh_admission_caps_plain_fastpath_and_reopens_after_submission() {
    let (mut workers, _rx) = workers();
    fill_fresh_budget(&mut workers[0]);
    let worker = &mut workers[0];
    let available = worker.frames.available();
    let retained = worker.sessions[0].tx_retained;
    let bytes = worker.sessions[0].tx_bytes;
    queue_frame(worker, 60);
    assert_eq!(worker.frames.available(), available);
    assert_eq!(worker.sessions[0].tx_retained, retained);
    assert_eq!(worker.sessions[0].tx_bytes, bytes);
    for totals in [
        &worker.counters,
        &worker.sessions[0].totals,
        &worker.sessions[0].tx_totals[0],
    ] {
        assert_eq!(totals.tx_backlog_drops, 1);
        assert_eq!(totals.queue_drops, 1);
        assert_eq!(totals.expired_frames, 0);
    }
    // A full lane does not consume another lane's admission allowance.
    queue_frame(&mut workers[1], 60);
    let other = workers[1].sender_index(0, 1).unwrap();
    assert_eq!(workers[1].sender.lanes[other].unsent_bytes, 60);
    workers[0].flush_frames(Duration::ZERO).unwrap();
    for _ in 0..8 {
        service(&mut workers);
    }
    let worker = &mut workers[0];
    let index = worker.sender_index(0, 0).unwrap();
    let remaining = worker.sender.lanes[index].unsent_bytes;
    assert!(remaining < FRESH_BYTES - 60);
    queue_frame(worker, 60);
    assert_eq!(worker.sender.lanes[index].unsent_bytes, remaining + 60);
    assert_eq!(worker.counters.tx_backlog_drops, 1);
}

#[test]
#[ignore = "kernel descriptor test; run in a disposable container"]
fn fresh_admission_charges_unexpanded_gso_and_reclaims_partial_expansion() {
    let (mut workers, _rx) = workers();
    let worker = &mut workers[0];
    let buffer = gso_input(worker, OVERSIZED_GSO_FRAMES, 1);
    assert_eq!(buffer.fresh_bytes().unwrap(), OVERSIZED_GSO_FRAMES * 43);
    worker.input_frame(buffer, Duration::ZERO).unwrap();
    let index = worker.sender_index(0, 0).unwrap();
    assert_eq!(
        worker.sender.lanes[index].unsent_bytes,
        OVERSIZED_GSO_FRAMES * 43
    );
    assert_eq!(worker.counters.tap_gso_frames, 0);
    queue_frame(worker, 60);
    assert_eq!(worker.counters.tx_backlog_drops, 1);
    let source = *worker.sessions[0].tap_inputs.front().unwrap();
    for _ in 0..4 {
        assert_eq!(worker.materialize(source).unwrap(), Some(43));
    }
    // Moving source bytes into views neither charges nor releases fresh credit.
    assert_eq!(
        worker.sender.lanes[index].unsent_bytes,
        OVERSIZED_GSO_FRAMES * 43
    );
    assert_eq!(
        worker.frames.source(source).unwrap().fresh_bytes().unwrap(),
        OVERSIZED_GSO_FRAMES * 43 - 172
    );
    worker.drain_tap_inputs(reassembly::FRAME_LIFETIME).unwrap();
    assert_eq!(worker.sender.lanes[index].unsent_bytes, 172);
    worker.expire_frames(reassembly::FRAME_LIFETIME).unwrap();
    assert_eq!(worker.sender.lanes[index].unsent_bytes, 0);
    assert!(!worker.cleanup_frames().unwrap());
    assert!(!worker.cleanup_frames().unwrap());
    assert_eq!(worker.counters.expired_frames, OVERSIZED_GSO_FRAMES as u64);
    assert_eq!(worker.sessions[0].tx_retained, 0);
    assert_eq!(worker.sessions[0].tx_bytes, 0);
    let buffer = gso_input(worker, OVERSIZED_GSO_FRAMES, 1);
    worker
        .input_frame(buffer, reassembly::FRAME_LIFETIME)
        .unwrap();
    // An already expired source is rejected before charging the newly empty lane.
    assert_eq!(worker.sender.lanes[index].unsent_bytes, 0);
}

#[test]
#[ignore = "kernel descriptor test; run in a disposable container"]
fn fresh_admission_rejects_oversized_gso_when_lane_is_not_empty() {
    let (mut workers, _rx) = workers();
    let worker = &mut workers[0];
    queue_frame(worker, 60);
    let before = worker.frames.available();
    let buffer = gso_input(worker, OVERSIZED_GSO_FRAMES, 1);
    worker.input_frame(buffer, Duration::ZERO).unwrap();
    let index = worker.sender_index(0, 0).unwrap();
    assert_eq!(worker.sender.lanes[index].unsent_bytes, 60);
    assert!(worker.sessions[0].tap_inputs.is_empty());
    assert_eq!(worker.frames.available(), before);
    assert_eq!(worker.counters.tap_gso_frames, 0);
    assert_eq!(worker.counters.tx_backlog_drops, 1);
}

#[test]
#[ignore = "kernel descriptor test; run in a disposable container"]
fn fresh_admission_returns_foreign_source_reservations_without_exchange() {
    let (mut workers, _rx) = workers();
    let [worker, reader] = &mut workers;
    fill_fresh_budget(worker);
    let owner_free = worker.free.len();
    let owner_retained = worker.sessions[0].tx_retained;
    let owner_bytes = worker.sessions[0].tx_bytes;
    let reader_free = reader.free.len();
    let mut buffer = gso_input(reader, 3, 100);
    buffer.lane = 0;
    worker.input_frame(buffer, Duration::ZERO).unwrap();
    assert_eq!(worker.free.len(), owner_free);
    assert_eq!(worker.sessions[0].tx_retained, owner_retained);
    assert_eq!(worker.sessions[0].tx_bytes, owner_bytes);
    assert_eq!(worker.counters.tx_backlog_drops, 1);
    reader.drain_queues(Duration::ZERO).unwrap();
    assert_eq!(reader.free.len(), reader_free);
    assert_eq!(reader.sessions[0].tx_retained, 0);
    assert_eq!(reader.sessions[0].tx_bytes, 0);
}

#[test]
#[ignore = "kernel descriptor test; run in a disposable container"]
fn fresh_admission_cleanup_releases_only_unsent_prefix_remainder_once() {
    let (mut workers, _rx) = workers();
    let worker = &mut workers[0];
    queue_frame(worker, 60);
    let index = worker.sender_index(0, 0).unwrap();
    let slot = worker.sessions[0].batches[0]
        .as_ref()
        .unwrap()
        .pending
        .iter(&worker.frames)
        .next()
        .unwrap();
    submit_original(worker, slot, 0..30);
    assert_eq!(worker.sender.lanes[index].unsent_bytes, 30);
    let coverage = worker.frames.recovery_mut(slot).unwrap();
    assert!(coverage.lost(0..30, 0));
    assert!(coverage.submitted(0..30, 1));
    assert_eq!(worker.sender.lanes[index].unsent_bytes, 30);
    worker.detach_frame(slot).unwrap();
    worker.release_frame(slot).unwrap();
    assert_eq!(worker.sender.lanes[index].unsent_bytes, 0);
    worker.release_frame(slot).unwrap();
    assert_eq!(worker.sender.lanes[index].unsent_bytes, 0);
    worker.cleanup_frames().unwrap();
}

#[test]
#[ignore = "kernel descriptor test; run in a disposable container"]
fn fresh_admission_stale_sources_reclaim_credit_before_lane_reinstall() {
    let (mut workers, _rx) = workers();
    let worker = &mut workers[0];
    let buffer = gso_input(worker, 3, 100);
    worker.input_frame(buffer, Duration::ZERO).unwrap();
    let index = worker.sender_index(0, 0).unwrap();
    assert_eq!(worker.sender.lanes[index].unsent_bytes, 426);
    assert!(!worker.sender.lanes[index].idle());
    assert_eq!(
        worker.sender.lanes[index].install(2).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    let mut connection = worker.sessions[0].connection.unwrap();
    connection.initiator = Tag::new(3).unwrap();
    worker
        .command(
            0,
            Command {
                generation: 2,
                phase: Phase::Quiesce,
                connection: Some(connection),
            },
            0,
        )
        .unwrap();
    for tag in [1, 3] {
        let mut buffer = gso_input(worker, 3, 100);
        buffer.tag = tag;
        worker.input_frame(buffer, Duration::ZERO).unwrap();
        assert_eq!(worker.sender.lanes[index].unsent_bytes, 426);
        assert_eq!(worker.sessions[0].tap_inputs.len(), 1);
    }
    worker.drain_tap_inputs(Duration::ZERO).unwrap();
    while worker.cleanup_frames().unwrap() {}
    assert_eq!(worker.sender.lanes[index].unsent_bytes, 0);
    assert!(worker.sender.lanes[index].idle());
    worker.progress_barriers(Duration::ZERO).unwrap();
    assert!(worker.sessions[0].command.is_none());
    worker
        .command(
            0,
            Command {
                generation: 2,
                phase: Phase::Install,
                connection: Some(connection),
            },
            0,
        )
        .unwrap();
    let mut buffer = gso_input(worker, 3, 100);
    buffer.tag = 3;
    worker.input_frame(buffer, Duration::ZERO).unwrap();
    assert_eq!(worker.sender.lanes[index].unsent_bytes, 426);
    assert_eq!(worker.sender.lanes[index].generation, 2);
}

#[test]
#[ignore = "kernel descriptor test; run in a disposable container"]
fn fresh_admission_preserves_sixteen_flow_qinq_gso_burst_before_bounding_excess() {
    let (mut workers, _rx) = workers();
    let worker = &mut workers[0];
    let index = worker.sender_index(0, 0).unwrap();
    let source_bytes = 44 * 1522;
    for _ in 0..16 {
        let buffer = gso_input_with_qinq(worker, 44, 1472, true);
        assert_eq!(buffer.fresh_bytes().unwrap(), source_bytes);
        worker.input_frame(buffer, Duration::ZERO).unwrap();
    }
    assert_eq!(worker.sender.lanes[index].unsent_bytes, 1_071_488);
    assert_eq!(worker.sessions[0].tap_inputs.len(), 16);
    assert_eq!(worker.counters.tx_backlog_drops, 0);
    assert_eq!(worker.counters.tap_gso_frames, 0);
    let admitted = FRESH_BYTES / source_bytes;
    for _ in 16..=admitted {
        let buffer = gso_input_with_qinq(worker, 44, 1472, true);
        worker.input_frame(buffer, Duration::ZERO).unwrap();
    }
    assert_eq!(worker.sessions[0].tap_inputs.len(), admitted);
    assert_eq!(
        worker.sender.lanes[index].unsent_bytes,
        admitted * source_bytes
    );
    assert_eq!(worker.counters.tx_backlog_drops, 1);
    assert_eq!(worker.counters.queue_drops, 1);
}

#[test]
#[ignore = "kernel descriptor test; run in a disposable container"]
fn queued_ack_validation_precedes_loss_fences_and_future_submissions() {
    let (mut workers, _rx) = workers();
    let index = workers[0].sender_index(0, 0).unwrap();
    for number in 1..=3 {
        let lane = &mut workers[0].sender.lanes[index];
        lane.history.reserve(number, 1280, true, true).unwrap();
        if number == 2 {
            assert!(lane.history.cancel(number));
        } else {
            assert!(lane.submitted(number, Duration::from_millis(1), None));
        }
    }
    workers[0].sender.loss_fences = 1;
    for range in [AckRange { start: 4, end: 4 }, AckRange { start: 1, end: 3 }] {
        workers[0]
            .incoming_ack(
                0,
                1,
                1,
                0,
                AckRanges::new(&[range]).unwrap(),
                Duration::from_millis(2),
            )
            .unwrap();
        assert!(workers[0].sender.incoming.is_empty());
        assert_eq!(workers[0].sender.ack_free.len(), ACK_BUFFERS);
    }
    let mut remote = workers[1].sender.ack_free.pop().unwrap();
    remote.session = 0;
    remote.tag = 1;
    remote.lane = 1;
    remote.count = 1;
    remote.ranges[0] = AckRange { start: 4, end: 4 };
    workers[0]
        .sender_message(Message::Ack(remote), Duration::from_millis(2))
        .unwrap();
    assert!(workers[0].sender.incoming.is_empty());
    let lane = &mut workers[0].sender.lanes[index];
    lane.history.reserve(4, 1280, true, true).unwrap();
    assert!(lane.submitted(4, Duration::from_millis(3), None));
    assert!(workers[0].sender.incoming.is_empty());
    workers[0]
        .incoming_ack(
            0,
            1,
            1,
            0,
            AckRanges::new(&[AckRange { start: 3, end: 4 }]).unwrap(),
            Duration::from_millis(4),
        )
        .unwrap();
    workers[0].begin_sender_ack().unwrap();
    assert_eq!(workers[0].sender.incoming.len(), 1);
    assert!(workers[0].sender.lanes[index].ack.is_none());
}

#[test]
#[ignore = "kernel descriptor test; run in a disposable container"]
fn fully_protected_loss_fence_continues_queued_ack_without_external_wakeup() {
    let (mut workers, _rx) = workers();
    let worker = &mut workers[1];
    let now = Duration::from_millis(2);
    let index = worker.sender_index(0, 1).unwrap();
    let lane = &mut worker.sender.lanes[index];
    lane.history.reserve(1, 1280, true, true).unwrap();
    assert!(lane.submitted(1, Duration::ZERO, None));
    lane.largest_ack = 4;
    lane.loss_due = Some(now);
    lane.loss_passes = 1;
    worker.sender.loss_fences = 1;
    worker
        .incoming_ack(
            0,
            1,
            2,
            0,
            AckRanges::new(&[AckRange { start: 1, end: 1 }]).unwrap(),
            now,
        )
        .unwrap();
    worker.flush_sender_messages(now).unwrap();
    assert!(worker.service_sender(now).unwrap());
    assert_eq!(worker.sender.loss_fences, 0);
    assert_eq!(worker.sender.incoming.len(), 1);
    assert!(worker.sender.lanes[index].ack.is_none());
    assert_eq!(worker.sender.lanes[index].history.inflight_bytes(), 1280);
    assert!(worker.recovery.staged.is_none());
    assert!(worker.sender.report.is_some());
    assert!(worker.service_sender(now).unwrap());
    assert!(worker.sender.incoming.is_empty());
    assert_eq!(worker.sender.lanes[index].history.inflight_bytes(), 0);
}

#[test]
#[ignore = "kernel descriptor test; run in a disposable container"]
fn successful_pmtu_probe_overtakes_retained_suffix_without_reordering_data() {
    let (mut workers, rx) = workers();
    for _ in 0..16 {
        queue_frame(&mut workers[1], 1514);
    }
    workers[1].flush_frames(Duration::ZERO).unwrap();
    for _ in 0..8 {
        service(&mut workers);
    }
    let batch = workers[1].sessions[0].batches[1].as_ref().unwrap();
    let retained_first = batch.plan.packet(0).expect("retained suffix").number;
    assert!(workers[1].sender.job.is_none());
    let mut bytes = [0; BASE_UDP_PAYLOAD];
    let mut last = 0;
    while let Ok((length, _)) = rx.recv(&mut bytes) {
        let packet = Datagram::decode(&bytes[..length]).unwrap();
        assert_eq!(packet.header().packet_number, last + 1);
        last += 1;
    }
    assert_eq!(retained_first, last + 1);
    let now = Duration::from_millis(2);
    workers[1]
        .incoming_ack(
            0,
            1,
            2,
            0,
            AckRanges::new(&[AckRange {
                start: 1,
                end: last,
            }])
            .unwrap(),
            now,
        )
        .unwrap();
    for _ in 0..8 {
        for worker in &mut workers {
            worker.io_time = Some(Ok);
            worker.drain_queues(now).unwrap();
            worker.service_sender(now).unwrap();
        }
    }
    workers[1]
        .request_probe(0, 1, BASE_UDP_PAYLOAD, true, None, now)
        .unwrap();
    let probe = workers[1].sender.job.expect("PMTU request").first;
    assert!(probe > retained_first);
    for _ in 0..8 {
        for worker in &mut workers {
            worker.drain_queues(now).unwrap();
            worker.service_sender(now).unwrap();
        }
    }
    let length = rx.recv(&mut bytes).unwrap().0;
    let packet = Datagram::decode(&bytes[..length]).unwrap();
    assert_eq!(packet.header().packet_number, probe);
    assert!(
        packet
            .records()
            .any(|record| matches!(record, Record::Probe))
    );
    last = probe;
    workers[1]
        .incoming_ack(
            0,
            1,
            2,
            0,
            AckRanges::new(&[AckRange {
                start: probe,
                end: probe,
            }])
            .unwrap(),
            now + Duration::from_micros(100),
        )
        .unwrap();
    let mut data = 0;
    for tick in 1..=32 {
        let now = now + Duration::from_micros(tick * 100);
        for worker in &mut workers {
            worker.drain_queues(now).unwrap();
            worker.service_sender(now).unwrap();
            worker.flush_frames(now).unwrap();
        }
        while let Ok((length, _)) = rx.recv(&mut bytes) {
            let packet = Datagram::decode(&bytes[..length]).unwrap();
            assert!(
                packet.header().packet_number > last,
                "an overtaken suffix must be renumbered before submission"
            );
            last = packet.header().packet_number;
            data += usize::from(
                packet
                    .records()
                    .any(|record| matches!(record, Record::Data { .. })),
            );
        }
    }
    assert!(data != 0, "DATA must resume after the PMTU probe");
}

#[test]
#[ignore = "kernel descriptor test; run in a disposable container"]
fn ack_transfer_returns_before_final_report_and_reuses_without_changing_fences() {
    let (mut workers, _rx) = workers();
    frame(&mut workers[1]);
    for _ in 0..8 {
        service(&mut workers);
    }
    let index = workers[1].sender_index(0, 1).unwrap();
    let number = workers[1].sender.lanes[index].history.highest_sent();
    let ranges = [AckRange {
        start: number,
        end: number,
    }];
    let now = Duration::from_millis(2);
    workers[0]
        .incoming_ack(0, 1, 2, 17, AckRanges::new(&ranges).unwrap(), now)
        .unwrap();
    workers[0].flush_sender_messages(now).unwrap();
    workers[1].drain_queues(now).unwrap();
    workers[1].begin_sender_ack().unwrap();
    workers[1].flush_sender_messages(now).unwrap();
    workers[0].drain_queues(now).unwrap();
    assert_eq!(workers[0].sender.ack_free.len(), ACK_BUFFERS);
    assert_eq!(workers[1].sender.acks_pending, 1);
    assert_eq!(workers[1].sender.loss_fences, 1);
    assert!(workers[1].sender.lanes[index].install(2).is_err());

    let mut report = workers[1].sender.report.take().unwrap();
    assert!(
        workers[1].sender.lanes[index]
            .advance_report(&mut report, now, &mut 64)
            .unwrap()
    );
    assert!(
        matches!(report.kind, ReportKind::Ack { received, delay_us: 17, end: Some(_), .. } if received == now)
    );
    assert_eq!(report.count, 1);
    report.id.generation += 1;
    assert!(
        workers[1].sender.lanes[index]
            .report_completed(&report)
            .is_err()
    );
    assert!(workers[1].sender.lanes[index].report_pending);
    report.id.generation -= 1;

    // Reuse every transfer box while the coordinator's final report is held.
    let later = now + Duration::from_millis(1);
    for _ in 0..ACK_BUFFERS {
        workers[0]
            .incoming_ack(0, 1, 2, 99, AckRanges::new(&ranges).unwrap(), later)
            .unwrap();
    }
    workers[0].flush_sender_messages(later).unwrap();
    workers[1].drain_queues(later).unwrap();
    workers[1].begin_sender_ack().unwrap();
    assert_eq!(workers[1].sender.incoming.len(), ACK_BUFFERS);
    assert_eq!(workers[1].sender.acks_pending, 1);
    assert_eq!(workers[1].sender.loss_fences, 1);
    let timing = workers[1].sender.lanes[index].ack.as_ref().unwrap();
    assert_eq!(timing.received, now);
    assert_eq!(timing.delay_us, 17);
    assert_eq!(workers[0].counters.ack_buffer_drops, 0);

    // Staging copies the send records and settles them before the owner sees the report.
    workers[1].settle_report(&mut report).unwrap();
    workers[0]
        .sender
        .owner
        .as_mut()
        .unwrap()
        .report(report)
        .unwrap();
    for _ in 0..32 {
        for worker in &mut workers {
            worker.drain_queues(later).unwrap();
            worker.service_sender(later).unwrap();
        }
    }
    assert_eq!(workers[0].sender.ack_free.len(), ACK_BUFFERS);
    assert_eq!(workers[1].sender.acks_pending, 0);
    assert_eq!(workers[1].sender.loss_fences, 0);
    assert!(workers[1].sender.lanes[index].ack.is_none());
    assert!(!workers[1].sender.lanes[index].report_pending);
    assert_eq!(workers[1].sender.lanes[index].history.inflight_bytes(), 0);

    // Invalid future coverage and a stale tag still return their recycled boxes.
    let future = workers[1].sender.lanes[index].history.highest_sent() + 1;
    for (tag, number) in [(1, future), (99, number)] {
        workers[0]
            .incoming_ack(
                0,
                tag,
                2,
                0,
                AckRanges::new(&[AckRange {
                    start: number,
                    end: number,
                }])
                .unwrap(),
                later,
            )
            .unwrap();
    }
    workers[0].flush_sender_messages(later).unwrap();
    workers[1].drain_queues(later).unwrap();
    workers[1].flush_sender_messages(later).unwrap();
    workers[0].drain_queues(later).unwrap();
    assert_eq!(workers[0].sender.ack_free.len(), ACK_BUFFERS);
    assert!(workers[1].sender.incoming.is_empty());
    assert_eq!(workers[0].counters.ack_buffer_drops, 0);
}
