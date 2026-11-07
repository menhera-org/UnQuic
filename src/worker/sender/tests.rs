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
        worker.io_time = Some(|_| Ok(Duration::from_millis(1)));
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
        worker.install_sender(0, 1, true).unwrap();
        worker.transmit[0].paths[0].lanes[index].socket =
            Some(TransmitSocket::connect(&rx, rx.local_addr()).unwrap());
    }
    (workers, rx)
}

fn frame(worker: &mut Worker) {
    let mut frame = worker.free.pop().unwrap();
    frame.session = 0;
    frame.lane = worker.id;
    frame.tag = 1;
    frame.length = 60;
    frame.bytes[..60].fill(0xa5);
    frame.deadline = Duration::from_millis(100);
    frame.tx_reserved = true;
    worker.sessions[0].tx_retained += 1;
    worker.sessions[0].tx_bytes += frame.length;
    worker.input_frame(frame, Duration::ZERO).unwrap();
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
fn control_overtaking_cancels_deferred_data_then_repacks_with_fresh_number() {
    let (mut workers, rx) = workers();
    frame(&mut workers[0]);
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
    assert!(control > old);
    let mut bytes = [0; BASE_UDP_PAYLOAD];
    let length = rx.recv(&mut bytes).unwrap().0;
    assert_eq!(
        Datagram::decode(&bytes[..length])
            .unwrap()
            .header()
            .packet_number,
        control
    );
    for _ in 0..8 {
        service(&mut workers);
    }
    assert_eq!(
        rx.recv(&mut bytes).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    let index = workers[0].sender_index(0, 0).unwrap();
    assert!(workers[0].sender.lanes[index].history.packet(old).is_none());
    assert!(workers[0].sender.job.is_none());

    for worker in &mut workers {
        worker.io_time = Some(|_| Ok(Duration::from_millis(10)));
    }
    workers[0].flush_frames(Duration::from_millis(10)).unwrap();
    let fresh = workers[0].sender.job.unwrap().first;
    assert!(fresh > control);
    for _ in 0..8 {
        service(&mut workers);
    }
    let length = rx.recv(&mut bytes).unwrap().0;
    let datagram = Datagram::decode(&bytes[..length]).unwrap();
    assert_eq!(datagram.header().packet_number, fresh);
    assert!(
        datagram
            .records()
            .any(|record| matches!(record, Record::Data { bytes: data, .. } if data == [0xa5; 60]))
    );
    assert!(
        workers[0].sender.lanes[index]
            .history
            .packet(fresh)
            .unwrap()
            .sample_slot
            .is_some()
    );
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
    let (mut workers, rx) = workers();
    for worker in &mut workers {
        worker.io_time = Some(Ok);
        for _ in 0..32 {
            let mut frame = worker.free.pop().unwrap();
            frame.session = 0;
            frame.lane = worker.id;
            frame.tag = 1;
            frame.length = 1514;
            frame.bytes[..1514].fill(0xa5);
            frame.deadline = Duration::from_millis(100);
            frame.tx_reserved = true;
            worker.sessions[0].tx_retained += 1;
            worker.sessions[0].tx_bytes += frame.length;
            worker.input_frame(frame, Duration::ZERO).unwrap();
        }
    }
    let mut received = [[0usize; 32]; 2];
    let mut complete = 0;
    let mut storage = [0; BASE_UDP_PAYLOAD];
    for tick in 0..100 {
        let now = Duration::from_millis(tick);
        for worker in &mut workers {
            worker.drain_queues(now).unwrap();
            worker.service_sender(now).unwrap();
            worker.flush_frames(now).unwrap();
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
    for worker in &workers {
        assert_eq!(worker.sessions[0].tx_retained, 0);
        assert_eq!(worker.counters.expired_frames, 0);
        assert_eq!(worker.counters.queue_drops, 0);
    }
}
