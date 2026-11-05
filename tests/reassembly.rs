use std::time::Duration;
use unquic::{
    MAX_FRAME_LEN, UnquicUuid,
    reassembly::{Admission, FRAME_CAPACITY, FRAME_LIFETIME, Lane, WINDOW_LEN},
    wire::{Datagram, Header, Record, encode_datagram},
};

fn id(lane: u16, sequence: u64) -> u64 {
    (u64::from(lane) << 48) | sequence
}
fn bytes(lane: u16, packet: u64, records: &[Record<'_>]) -> Vec<u8> {
    let mut buffer = vec![0; 16_384];
    let length = encode_datagram(
        Header {
            cookie: UnquicUuid::from_bytes([1; 16]),
            receive_tag: 2,
            lane,
            record_area_len: 0,
            packet_number: packet,
        },
        records,
        &mut buffer,
    )
    .unwrap();
    buffer.truncate(length);
    buffer
}
fn piece(sequence: u64, offset: u32, payload: &[u8]) -> Record<'_> {
    Record::Data {
        frame_id: id(1, sequence),
        total_len: 60,
        offset,
        bytes: payload,
    }
}
fn admit(lane: &mut Lane, packet: u64, records: &[Record<'_>], now: Duration) -> Admission {
    lane.admit(
        &Datagram::decode(&bytes(lane.id(), packet, records)).unwrap(),
        now,
    )
}
fn permutations<T: Copy>(items: &mut [T], at: usize, check: &mut impl FnMut(&[T])) {
    if at == items.len() {
        check(items);
        return;
    }
    for next in at..items.len() {
        items.swap(at, next);
        permutations(items, at + 1, check);
        items.swap(at, next);
    }
}

#[test]
fn originals_and_repairs_deliver_once_in_every_order() {
    permutations(&mut [0, 0, 30, 30], 0, &mut |offsets| {
        let mut lane = Lane::new(1, 9022).unwrap();
        let mut delivered = 0;
        for (packet, &offset) in offsets.iter().enumerate() {
            assert_eq!(
                admit(
                    &mut lane,
                    packet as u64 + 1,
                    &[piece(1, offset, &[3; 30])],
                    Duration::ZERO
                ),
                Admission::Fresh
            );
            if let Some(frame) = lane.front(Duration::ZERO) {
                assert_eq!(frame.bytes, &[3; 60]);
                delivered += 1;
                assert!(lane.finish_front());
            }
        }
        assert_eq!(delivered, 1);
        assert_eq!(lane.retained_frames(), 0);
    });
}

#[test]
fn packet_replay_and_frame_repair_cannot_redeliver_after_window_reuse() {
    let mut lane = Lane::new(1, 9022).unwrap();
    assert_eq!(
        admit(&mut lane, 1, &[piece(1, 0, &[1; 60])], Duration::ZERO),
        Admission::Fresh
    );
    assert_eq!(
        admit(&mut lane, 1, &[piece(2, 0, &[2; 60])], Duration::ZERO),
        Admission::Duplicate
    );
    assert_eq!(lane.front(Duration::ZERO).unwrap().bytes, &[1; 60]);
    assert!(lane.finish_front());
    assert_eq!(
        admit(&mut lane, 2, &[piece(2, 0, &[2; 30])], Duration::ZERO),
        Admission::Fresh
    );
    assert_eq!(lane.expire(FRAME_LIFETIME), 1);
    let high = WINDOW_LEN as u64 * 3;
    assert_eq!(
        admit(&mut lane, high, &[piece(high, 0, &[3; 60])], FRAME_LIFETIME),
        Admission::Fresh
    );
    assert_eq!(lane.lower_bounds(), (high - 65535, high - 65535));
    assert_eq!(lane.front(FRAME_LIFETIME).unwrap().bytes, &[3; 60]);
    lane.finish_front();
    assert_eq!(
        admit(
            &mut lane,
            3,
            &[piece(high + 1, 0, &[4; 60])],
            FRAME_LIFETIME
        ),
        Admission::Duplicate
    );
    for (packet, seq) in [(high + 1, 1), (high + 2, 2), (high + 3, high)] {
        assert_eq!(
            admit(
                &mut lane,
                packet,
                &[piece(seq, 0, &[5; 60])],
                FRAME_LIFETIME
            ),
            Admission::Fresh
        );
    }
    assert!(lane.front(FRAME_LIFETIME).is_none());
    assert_eq!(lane.retained_frames(), 0);
}

#[test]
fn pressure_rejects_whole_datagram_without_ack_or_window_advancement() {
    let mut lane = Lane::new(1, 9022).unwrap();
    for sequence in 1..=FRAME_CAPACITY as u64 {
        assert_eq!(
            admit(
                &mut lane,
                sequence,
                &[piece(sequence, 0, &[7; 30])],
                Duration::ZERO
            ),
            Admission::Fresh
        );
    }
    let high = WINDOW_LEN as u64 * 2;
    let records = [piece(1, 30, &[7; 30]), piece(high, 0, &[9; 30])];
    assert_eq!(
        admit(&mut lane, high, &records, Duration::ZERO),
        Admission::Pressure
    );
    assert_eq!(lane.lower_bounds(), (1, 1));
    assert!(lane.front(Duration::ZERO).is_none());
    assert_eq!(lane.retained_frames(), FRAME_CAPACITY);
    // A packet rejected by pressure has not consumed its packet number.
    assert_eq!(
        admit(&mut lane, high, &[piece(1, 30, &[7; 30])], Duration::ZERO),
        Admission::Fresh
    );
    assert_eq!(lane.front(Duration::ZERO).unwrap().bytes, &[7; 60]);
    lane.finish_front();
    assert_eq!(
        admit(
            &mut lane,
            high + 1,
            &[piece(high, 0, &[9; 60])],
            Duration::ZERO
        ),
        Admission::Fresh
    );
    assert_eq!(lane.retained_frames(), 1);
    assert_eq!(lane.front(Duration::ZERO).unwrap().bytes, &[9; 60]);
}

#[test]
fn retirement_and_lane_identity_prevent_redelivery_until_explicit_reset() {
    let mut lane = Lane::new(1, 9022).unwrap();
    let packet = bytes(1, 1, &[piece(1, 0, &[1; 60])]);
    let datagram = Datagram::decode(&packet).unwrap();
    assert_eq!(lane.admit(&datagram, Duration::ZERO), Admission::Fresh);
    lane.retire();
    assert_eq!(lane.admit(&datagram, Duration::ZERO), Admission::Invalid);
    assert!(lane.front(Duration::ZERO).is_none());
    lane.reset(2, 9022).unwrap();
    assert_eq!(lane.admit(&datagram, Duration::ZERO), Admission::Invalid);
    let packet = bytes(
        2,
        1,
        &[Record::Data {
            frame_id: id(2, 1),
            total_len: 60,
            offset: 0,
            bytes: &[2; 60],
        }],
    );
    assert_eq!(
        lane.admit(&Datagram::decode(&packet).unwrap(), Duration::ZERO),
        Admission::Fresh
    );
    assert_eq!(lane.front(Duration::ZERO).unwrap().bytes, &[2; 60]);
    // A new connection may deliberately restart the old lane's counters.
    lane.reset(1, 9022).unwrap();
    assert_eq!(lane.admit(&datagram, Duration::ZERO), Admission::Fresh);
}

#[test]
fn abandonment_before_data_and_duplicate_controls_remain_terminal() {
    let mut lane = Lane::new(1, 9022).unwrap();
    for sequence in [1, WINDOW_LEN as u64 * 2] {
        let abandon = Record::FrameAbandon {
            lane: 1,
            origin: 0,
            reason: 1,
            frame_id: id(1, sequence),
        };
        assert_eq!(
            admit(&mut lane, sequence, &[abandon], Duration::ZERO),
            Admission::Fresh
        );
        assert_eq!(
            admit(
                &mut lane,
                sequence + 1,
                &[piece(sequence, 0, &[1; 60])],
                Duration::ZERO
            ),
            Admission::Fresh
        );
        assert!(lane.front(Duration::ZERO).is_none());
    }
    assert!(!lane.abandon(id(2, 1)));
    assert!(!lane.abandon(id(1, 0)));
    assert_eq!(
        admit(
            &mut lane,
            1_000_000,
            &[piece(1, 0, &[1; 60])],
            Duration::ZERO
        ),
        Admission::Fresh
    );
    assert!(lane.front(Duration::ZERO).is_none());
}

#[test]
fn tap_backpressure_keeps_one_frame_and_deadline_never_moves() {
    let mut lane = Lane::new(1, 9022).unwrap();
    assert_eq!(
        admit(&mut lane, 1, &[piece(1, 0, &[1; 30])], Duration::ZERO),
        Admission::Fresh
    );
    let near_expiry = FRAME_LIFETIME - Duration::from_nanos(1);
    assert_eq!(
        admit(&mut lane, 2, &[piece(1, 30, &[1; 30])], near_expiry),
        Admission::Fresh
    );
    assert_eq!(
        admit(&mut lane, 3, &[piece(1, 0, &[1; 60])], near_expiry),
        Admission::Fresh
    );
    for _ in 0..10 {
        assert_eq!(lane.front(near_expiry).unwrap().id, id(1, 1));
    }
    assert_eq!(lane.retained_frames(), 1);
    assert_eq!(lane.deadline(), Some(FRAME_LIFETIME));
    assert!(lane.front(FRAME_LIFETIME).is_none());
    assert_eq!(
        admit(&mut lane, 4, &[piece(1, 0, &[1; 60])], FRAME_LIFETIME),
        Admission::Fresh
    );
    assert!(lane.front(FRAME_LIFETIME).is_none());
    assert!(!lane.finish_front());
}

#[test]
fn length_conflicts_and_negotiated_ceiling_reject_before_any_effect() {
    let mut lane = Lane::new(1, 1514).unwrap();
    let first = piece(1, 0, &[1; 60]);
    let wrong = Record::Data {
        frame_id: id(1, 1),
        total_len: 61,
        offset: 0,
        bytes: &[2; 30],
    };
    let huge = Record::Data {
        frame_id: id(1, 2),
        total_len: 1515,
        offset: 0,
        bytes: &[2; 30],
    };
    for records in [[first, wrong], [first, huge]] {
        assert_eq!(
            admit(&mut lane, 1, &records, Duration::ZERO),
            Admission::Invalid
        );
        assert_eq!(lane.retained_frames(), 0);
        assert_eq!(lane.lower_bounds(), (1, 1));
    }
    assert_eq!(
        admit(&mut lane, 1, &[first], Duration::ZERO),
        Admission::Fresh
    );
    assert_eq!(
        admit(&mut lane, 2, &[wrong], Duration::ZERO),
        Admission::Invalid
    );
    assert_eq!(lane.front(Duration::ZERO).unwrap().bytes, &[1; 60]);
}

#[test]
fn arbitrary_overlap_keeps_first_bytes_and_all_frame_sizes_reassemble() {
    let mut lane = Lane::new(1, 9022).unwrap();
    let mut packet = 0;
    for length in [14, 60, 63, 64, 65, 1514, 1518, MAX_FRAME_LEN] {
        lane.reset(1, 9022).unwrap();
        let expected: Vec<_> = (0..length).map(|i| i as u8).collect();
        // Reverse, overlapping pieces exercise partial words, full words and tails.
        for start in (0..length).step_by(31).rev() {
            let end = (start + 97).min(length);
            packet += 1;
            let record = Record::Data {
                frame_id: id(1, 1),
                total_len: length as u32,
                offset: start as u32,
                bytes: &expected[start..end],
            };
            assert_eq!(
                admit(&mut lane, packet, &[record], Duration::ZERO),
                Admission::Fresh
            );
        }
        assert_eq!(lane.front(Duration::ZERO).unwrap().bytes, expected);
        lane.finish_front();
    }
    lane.reset(1, 9022).unwrap();
    assert_eq!(
        admit(&mut lane, 1, &[piece(1, 7, &[3; 40])], Duration::ZERO),
        Admission::Fresh
    );
    assert_eq!(
        admit(&mut lane, 2, &[piece(1, 0, &[2; 60])], Duration::ZERO),
        Admission::Fresh
    );
    let frame = lane.front(Duration::ZERO).unwrap();
    assert_eq!(&frame.bytes[..7], &[2; 7]);
    assert_eq!(&frame.bytes[7..47], &[3; 40]);
    assert_eq!(&frame.bytes[47..], &[2; 13]);
}

#[test]
fn many_pieces_in_one_packet_reserve_each_frame_once_and_keep_completion_order() {
    let mut lane = Lane::new(1, 9022).unwrap();
    let records = [
        piece(1, 0, &[1; 30]),
        piece(2, 0, &[2; 60]),
        piece(1, 30, &[1; 30]),
    ];
    assert_eq!(
        admit(&mut lane, 1, &records, Duration::ZERO),
        Admission::Fresh
    );
    assert_eq!(lane.front(Duration::ZERO).unwrap().id, id(1, 2));
    lane.finish_front();
    assert_eq!(lane.front(Duration::ZERO).unwrap().id, id(1, 1));
    lane.finish_front();
    assert_eq!(lane.retained_frames(), 0);
}

#[test]
fn full_ready_queue_rejects_new_frames_but_accepts_duplicates_without_reenqueueing() {
    let mut lane = Lane::new(1, 9022).unwrap();
    for sequence in 1..=FRAME_CAPACITY as u64 {
        assert_eq!(
            admit(
                &mut lane,
                sequence,
                &[piece(sequence, 0, &[7; 60])],
                Duration::ZERO
            ),
            Admission::Fresh
        );
    }
    assert_eq!(
        admit(&mut lane, 100, &[piece(65, 0, &[7; 60])], Duration::ZERO),
        Admission::Pressure
    );
    assert_eq!(
        admit(&mut lane, 101, &[piece(1, 0, &[8; 60])], Duration::ZERO),
        Admission::Fresh
    );
    for sequence in 1..=FRAME_CAPACITY as u64 {
        let frame = lane.front(Duration::ZERO).unwrap();
        assert_eq!(frame.id, id(1, sequence));
        assert_eq!(frame.bytes, &[7; 60]);
        lane.finish_front();
    }
    assert!(lane.front(Duration::ZERO).is_none());
    assert_eq!(
        admit(&mut lane, 100, &[piece(65, 0, &[7; 60])], Duration::ZERO),
        Admission::Fresh
    );
}
