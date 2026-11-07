use super::super::tests::test_workers;
use super::*;

fn workers() -> ([Worker; 2], ReceiveSocket) {
    let (_, mut workers) = test_workers();
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
fn fresh_grant_clock_survives_older_worker_turn_through_handoff() {
    let (mut workers, rx) = workers();
    frame(&mut workers[1]);
    let number = workers[1].sender.job.unwrap().first;
    for _ in 0..8 {
        service(&mut workers);
    }
    let mut bytes = [0; BASE_UDP_PAYLOAD];
    let length = rx.recv(&mut bytes).unwrap().0;
    let datagram = Datagram::decode(&bytes[..length]).unwrap();
    assert_eq!(datagram.header().lane, 2);
    assert_eq!(datagram.header().packet_number, number);
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
    assert_eq!(workers[1].sessions[0].tx_retained, 0);
}

#[test]
#[ignore = "kernel descriptor test; run in a disposable container"]
fn steady_control_does_not_starve_local_data_or_consume_another_grant() {
    let (mut workers, rx) = workers();
    for worker in &mut workers {
        worker.io_time = Some(Ok);
    }
    let index = workers[0].sender_index(0, 0).unwrap();
    let mut previous = 0;
    for turn in 1..=8 {
        let now = Duration::from_millis(turn);
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
        assert_eq!(workers[0].sessions[0].tx_retained, 0);
    }
    assert_eq!(workers[0].counters.send_grants, 8);
    assert_eq!(workers[0].counters.cancelled_grants, 0);
}

#[test]
#[ignore = "kernel descriptor test; run in a disposable container"]
fn control_overtaking_a_permit_behind_a_full_message_turn_refreshes_it() {
    let (mut workers, rx) = workers();
    let worker = &mut workers[0];
    frame(worker);
    worker.flush_sender_messages(Duration::ZERO).unwrap();
    let now = Duration::from_millis(1);
    // Model a full bounded message turn before the local permit; allocation is fixture setup.
    worker.sender.outgoing.reserve(BATCH_SIZE + 1);
    for _ in 0..BATCH_SIZE {
        worker.sender.outgoing.push_back((0, Message::Reclaim));
    }
    worker
        .sender
        .owner
        .as_mut()
        .unwrap()
        .advance(now, &mut worker.sender.outgoing)
        .unwrap();
    worker.flush_sender_messages(now).unwrap();
    assert!(!worker.sender.permitted);
    assert!(
        worker
            .sender
            .outgoing
            .iter()
            .any(|(_, m)| matches!(m, Message::Permit(_)))
    );
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
        for _ in 0..32 {
            queue_frame(worker, 1514);
        }
    }
    let mut received = [[0usize; 32]; 2];
    let mut next_packet = [1_u64; 2];
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
            if !feedback {
                assert_eq!(
                    header.packet_number, next_packet[owner],
                    "never-attempted suffix must retain its original identity"
                );
                next_packet[owner] += 1;
                assert_eq!(receiver[owner].admit(&datagram, now), Admission::Fresh);
            }
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
        if complete == 64 {
            break;
        }
    }
    assert_eq!(complete, 64, "received {received:?}");
    if !feedback {
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
    for worker in &workers {
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
    let (mut owner, flights) = Owner::new(2, 2, &descriptors, Duration::ZERO).unwrap();
    for session in 0..2 {
        owner.timers.install(session, 1).unwrap();
    }
    for (worker, flight) in workers.iter_mut().zip(flights.into_vec()) {
        let command = worker.sessions[0].installed.unwrap();
        worker.command(1, command, 0).unwrap();
        worker.install_sender(1, 1, true).unwrap();
        worker.sender.owner = None;
        worker.sender.flight = Some(flight);
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
            worker.sender.queue(0, Message::Reclaim).unwrap();
        }
    }
    assert!(worker.flush_sender_messages(Duration::ZERO).unwrap());
    assert!(
        worker.sender.reclaim.is_some(),
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
    let later = Duration::from_millis(2);
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
    frame(&mut workers[0]);
    let retained = workers[0].sessions[0].tx_retained;
    let old = workers[0].sender.job.unwrap().first;
    let tx = workers[0].sessions[0].tx[0].as_mut().unwrap();
    let mut skip = 65536 - 3;
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
    assert_eq!(control, 65536);
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
    assert_eq!(workers[0].sessions[0].tx_retained, 0);
}

#[test]
#[ignore = "kernel descriptor test; run in a disposable container"]
fn retained_suffix_reservations_survive_recycle_and_leave_on_discard() {
    for cause in 0..3 {
        let (mut workers, _rx) = workers();
        let worker = &mut workers[1];
        for _ in 0..32 {
            queue_frame(worker, 1514);
        }
        worker.flush_frames(Duration::ZERO).unwrap();
        let job = worker.sender.job.unwrap();
        assert!(job.count > 10);
        for _ in 0..8 {
            service(&mut workers);
        }
        let worker = &mut workers[1];
        assert!(worker.sender.job.is_none());
        let index = worker.sender_index(0, 1).unwrap();
        let batch = worker.sessions[0].batches[1].as_ref().unwrap();
        let first = batch.plan.packet(0).expect("ungranted suffix").number;
        assert!(first > job.first && first < job.first + job.count as u64);
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
            for _ in 0..16 {
                for worker in &mut workers {
                    worker.io_time = Some(Ok);
                    worker.drain_queues(Duration::from_millis(2)).unwrap();
                    worker.service_sender(Duration::from_millis(2)).unwrap();
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
    let (mut workers, rx) = workers();
    let worker = &mut workers[1];
    queue_frame(worker, 9022);
    worker.flush_frames(Duration::ZERO).unwrap();
    let job = worker.sender.job.unwrap();
    let (_, Message::Request(request)) = worker.sender.outgoing.pop_front().unwrap() else {
        panic!()
    };
    workers[0]
        .sender
        .owner
        .as_mut()
        .unwrap()
        .request(request)
        .unwrap();
    let mut outgoing = VecDeque::with_capacity(16);
    let now = Duration::from_millis(1);
    workers[0]
        .sender
        .owner
        .as_mut()
        .unwrap()
        .advance(now, &mut outgoing)
        .unwrap();
    let (_, Message::Permit(flight)) = outgoing.pop_front().unwrap() else {
        panic!()
    };
    assert!(flight.packets().len() > 1);
    let worker = &mut workers[1];
    worker.sender_message(Message::Permit(flight), now).unwrap();
    let mut batch = worker.sessions[0].batches[1].take().unwrap();
    let input = worker.frames.input(batch.frames[0].unwrap()).unwrap();
    let mut parts = [&[][..]; 4];
    let bytes: Vec<_> = batch
        .plan
        .slices(0, &[input], &mut parts)
        .unwrap()
        .iter()
        .flat_map(|part| part.iter().copied())
        .collect();
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
    let frame = worker.frames.frame(batch.frames[0].unwrap()).unwrap();
    assert_eq!(
        (frame.offset, frame.deadline),
        (1164, Duration::from_millis(100))
    );
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
    worker
        .flush_batch(0, 1, &mut batch, Duration::from_millis(1))
        .unwrap();
    assert!(batch.plan.is_empty());
    assert!(worker.sender.job.is_none());
    assert!(!worker.sender.lanes[index].history.is_reserved(first.number));
    worker
        .flush_batch(0, 1, &mut batch, Duration::from_millis(1))
        .unwrap();
    assert!(worker.sender.job.unwrap().first > overtaker);
    worker.sessions[0].batches[1] = Some(batch);
}

#[test]
#[ignore = "kernel descriptor test; run in a disposable container"]
fn ack_buffer_pressure_counts_only_excess_and_reuses_returned_storage() {
    let (mut workers, _rx) = workers();
    frame(&mut workers[1]);
    let number = workers[1].sender.job.unwrap().first;
    for _ in 0..8 {
        service(&mut workers);
    }
    let index = workers[1].sender_index(0, 1).unwrap();
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
