use std::time::Duration;
use unquic::{
    ack::{FramePiece, History, PieceJournal},
    loss::FrameRecovery,
    wire::{AckRange, AckRanges},
};

fn id(sequence: u64) -> u64 {
    (1 << 48) | sequence
}
fn piece(slot: u16, start: u16, end: u16, attempt: u8) -> FramePiece {
    FramePiece {
        frame_slot: slot,
        frame_id: id(u64::from(slot) + 1),
        start,
        end,
        attempt,
    }
}
fn bound(packets: usize, mappings: usize) -> PieceJournal {
    let mut journal = PieceJournal::new(packets, 3, mappings).unwrap();
    for slot in 0..3 {
        assert!(journal.bind_frame(slot, id(slot as u64 + 1), 1514));
    }
    journal
}

#[test]
fn invalid_and_exhausted_reservations_preserve_every_existing_list() {
    assert!(PieceJournal::new(1 << 31, 0, 0).is_err());
    assert!(PieceJournal::new(0, 65537, 0).is_err());
    assert!(PieceJournal::new(0, 0, 1 << 31).is_err());
    let mut empty = PieceJournal::new(0, 0, 0).unwrap();
    assert_eq!(empty.available(), 0);
    assert!(!empty.bind_frame(0, id(1), 60));
    assert!(!empty.reserve_packet(0, &[]));
    assert!(!empty.frame_empty(0, 0));
    assert_eq!(empty.pop_packet(0), None);

    let mut journal = bound(3, 2);
    let first = piece(0, 0, 60, 0);
    assert!(journal.reserve_packet(0, &[first]));
    let invalid = [
        FramePiece {
            frame_slot: 3,
            ..first
        },
        FramePiece {
            frame_id: id(99),
            ..first
        },
        FramePiece { start: 60, ..first },
        FramePiece { end: 1515, ..first },
        FramePiece {
            attempt: 3,
            ..first
        },
    ];
    for bad in invalid {
        assert!(!journal.reserve_packet(1, &[bad]));
        assert_eq!(journal.available(), 1);
        assert_eq!(journal.packet(0).collect::<Vec<_>>(), [first]);
        assert!(journal.packet_empty(1));
    }
    assert!(!journal.reserve_packet(0, &[first]));
    assert!(!journal.reserve_packet(3, &[first]));
    assert!(!journal.reserve_packet(1, &[first, first]));
    assert!(!journal.bind_frame(0, id(99), 60));
    assert_eq!(journal.pop_frame(0, id(99)), None);
    assert!(!journal.bind_frame(1, 0, 60));
    assert!(!journal.bind_frame(1, 1, 60));
    assert!(!journal.bind_frame(1, 1 << 48, 60));
    assert!(!journal.bind_frame(1, id(2), 65_536));
    assert_eq!(journal.pop_packet(0), Some(first));
    assert_eq!(journal.pop_packet(0), None);
    assert_eq!(journal.available(), 2);
    assert!(journal.bind_frame(0, id(99), 60));
    assert!(!journal.reserve_packet(0, &[first]));

    let mut journal = bound(1, 128);
    let mut pieces = [first; 65];
    pieces[1].frame_id = id(99);
    assert!(!journal.reserve_packet(0, &pieces[..2]));
    assert_eq!(journal.available(), 128);
    pieces[1] = first;
    assert!(!journal.reserve_packet(0, &pieces));
    assert!(journal.reserve_packet(0, &pieces[..64]));
    assert_eq!(journal.packet(0).count(), 64);
}

#[test]
fn frame_cleanup_unlinks_packet_heads_and_interiors_without_losing_siblings() {
    let mut journal = bound(4, 8);
    let a = piece(0, 0, 60, 0);
    let b = piece(1, 0, 60, 0);
    let c = piece(2, 0, 60, 0);
    assert!(journal.reserve_packet(0, &[a, b, c]));
    assert!(journal.reserve_packet(1, &[b, a, c]));
    assert!(journal.reserve_packet(2, &[c, a]));
    for expected in [a; 3] {
        assert_eq!(journal.pop_frame(0, id(1)), Some(expected));
    }
    assert_eq!(journal.pop_frame(0, id(1)), None);
    assert_eq!(journal.packet(0).collect::<Vec<_>>(), [b, c]);
    assert_eq!(journal.packet(1).collect::<Vec<_>>(), [b, c]);
    assert_eq!(journal.packet(2).collect::<Vec<_>>(), [c]);
    assert!(journal.bind_frame(0, id(4), 60));
    let replacement = FramePiece {
        frame_id: id(4),
        ..a
    };
    assert!(journal.reserve_packet(3, &[replacement]));
    assert_eq!(journal.pop_frame(0, id(1)), None);
    assert_eq!(journal.pop_packet(3), Some(replacement));
    for packet in 0..3 {
        while journal.pop_packet(packet).is_some() {}
    }
    assert_eq!(journal.available(), 8);
}

#[test]
fn packed_tail_head_loss_late_ack_and_partial_submission_keep_accounting_separate() {
    let mut journal = bound(8, 16);
    let mut history = History::new().unwrap();
    let mut frames = [
        FrameRecovery::new(1514).unwrap(),
        FrameRecovery::new(1514).unwrap(),
    ];
    let packets: [&[FramePiece]; 3] = [
        &[piece(0, 0, 1384, 0)],
        &[piece(0, 1384, 1514, 0), piece(1, 0, 1234, 0)],
        &[piece(1, 1234, 1514, 0)],
    ];
    for (slot, pieces) in packets.iter().enumerate() {
        history.reserve(slot as u64 + 1, 1500, true, true).unwrap();
        assert!(journal.reserve_packet(slot + 1, pieces));
    }
    // sendmmsg submitted two packets; the unsent suffix remains a permanent hole.
    for number in 1..=2 {
        assert!(history.submit(number, Duration::ZERO));
        for p in journal.packet(number as usize) {
            assert!(
                frames[usize::from(p.frame_slot)]
                    .submitted(usize::from(p.start)..usize::from(p.end), p.attempt)
            );
        }
    }
    assert!(history.cancel(3));
    while journal.pop_packet(3).is_some() {}
    assert_eq!(frames[0].sent(), 1514);
    assert_eq!(frames[1].sent(), 1234);
    let mut ack = |number: u64, journal: &mut PieceJournal, history: &mut History| {
        history
            .acknowledge(
                AckRanges::new(&[AckRange {
                    start: number,
                    end: number,
                }])
                .unwrap(),
                |_| {
                    while let Some(p) = journal.pop_packet(number as usize) {
                        assert!(
                            frames[usize::from(p.frame_slot)]
                                .acknowledge(usize::from(p.start)..usize::from(p.end), p.attempt)
                                .is_some()
                        );
                    }
                },
            )
            .unwrap()
    };
    assert_eq!(ack(1, &mut journal, &mut history).released_bytes, 1500);
    assert_eq!(history.lose(2).unwrap().outer_bytes, 1500);
    for p in journal.packet(2) {
        assert!(
            frames[usize::from(p.frame_slot)]
                .lost(usize::from(p.start)..usize::from(p.end), p.attempt)
        );
    }
    let repair = [piece(0, 1384, 1514, 1), piece(1, 0, 1234, 1)];
    history.reserve(4, 1500, true, true).unwrap();
    assert!(journal.reserve_packet(4, &repair));
    assert!(history.submit(4, Duration::from_millis(1)));
    for p in journal.packet(4) {
        assert!(
            frames[usize::from(p.frame_slot)]
                .submitted(usize::from(p.start)..usize::from(p.end), p.attempt)
        );
    }
    // A late original ACK settles coverage without releasing the repair's charge.
    let progress = history
        .acknowledge(
            AckRanges::new(&[AckRange { start: 2, end: 2 }]).unwrap(),
            |_| {
                while let Some(p) = journal.pop_packet(2) {
                    assert!(
                        frames[usize::from(p.frame_slot)]
                            .acknowledge(usize::from(p.start)..usize::from(p.end), p.attempt)
                            .is_some()
                    );
                }
            },
        )
        .unwrap();
    assert_eq!(progress.released_bytes, 0);
    assert_eq!(frames[0].remaining(), 0);
    assert_eq!(frames[1].remaining(), 280);
    while journal.pop_frame(0, id(1)).is_some() {}
    assert_eq!(journal.packet(4).collect::<Vec<_>>(), [repair[1]]);
    assert_eq!(history.inflight_bytes(), 1500);
    assert_eq!(journal.pop_packet(4), Some(repair[1]));
    assert_eq!(frames[1].acknowledge(0..1234, 1), Some(0));
    assert_eq!(
        history
            .acknowledge(
                AckRanges::new(&[AckRange { start: 4, end: 4 }]).unwrap(),
                |_| {}
            )
            .unwrap()
            .released_bytes,
        1500
    );
    assert_eq!(history.inflight_bytes(), 0);
    assert_eq!(journal.available(), 16);
}

#[test]
fn prepared_repair_prevents_frame_rebinding_until_cancellation() {
    let mut journal = bound(3, 4);
    let original = piece(0, 0, 60, 0);
    let repair = piece(0, 0, 60, 1);
    assert!(journal.bind_frame(0, id(1), 60));
    let mut state = FrameRecovery::new(60).unwrap();
    assert!(journal.reserve_packet(0, &[original]));
    assert!(state.submitted(0..60, 0));
    assert!(state.lost(0..60, 0));
    assert!(journal.reserve_packet(1, &[repair])); // Prepared, not submitted.
    assert_eq!(journal.pop_packet(0), Some(original));
    assert_eq!(state.acknowledge(0..60, 0), Some(60));
    assert!(!state.can_submit(0..60, 1));
    assert!(!journal.bind_frame(0, id(4), 60));
    assert_eq!(journal.pop_packet(1), Some(repair));
    assert!(journal.bind_frame(0, id(4), 60));
    assert_eq!(journal.pop_frame(0, id(1)), None);
    assert_eq!(journal.available(), 4);
}

#[test]
fn cleanup_of_many_packets_yields_and_reuses_all_slots() {
    let mut journal = bound(257, 257);
    let p = piece(0, 0, 60, 0);
    for packet in 0..257 {
        assert!(journal.reserve_packet(packet, &[p]));
    }
    for turn in 0..5 {
        let mut visits = 0;
        while visits < 64 && journal.pop_frame(0, id(1)).is_some() {
            visits += 1;
        }
        assert_eq!(visits, if turn < 4 { 64 } else { 1 });
        assert_eq!(journal.frame_empty(0, id(1)), turn == 4);
    }
    for packet in 0..257 {
        assert!(journal.packet_empty(packet));
    }
    assert_eq!(journal.available(), 257);
}

#[test]
fn mixed_reclamation_matches_independent_lists_through_slot_reuse() {
    const PACKETS: usize = 16;
    const FRAMES: usize = 8;
    const CAPACITY: usize = 32;
    let mut journal = PieceJournal::new(PACKETS, FRAMES, CAPACITY).unwrap();
    let mut ids: [u64; FRAMES] = std::array::from_fn(|slot| id(slot as u64 + 1));
    for (slot, &id) in ids.iter().enumerate() {
        assert!(journal.bind_frame(slot, id, 60));
    }
    let mut packets: [Vec<FramePiece>; PACKETS] = std::array::from_fn(|_| Vec::new());
    let mut frames: [Vec<(usize, FramePiece)>; FRAMES] = std::array::from_fn(|_| Vec::new());
    let mut rng = 19_u64;
    let mut next = || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng as usize
    };
    for _ in 0..10_000 {
        let slot = next() % PACKETS;
        match next() % 4 {
            0 => {
                let pieces: Vec<_> = (0..1 + next() % 4)
                    .map(|_| {
                        let frame = next() % FRAMES;
                        let start = (next() % 60) as u16;
                        FramePiece {
                            frame_slot: frame as u16,
                            frame_id: ids[frame],
                            start,
                            end: start + 1,
                            attempt: (next() % 3) as u8,
                        }
                    })
                    .collect();
                let expected = packets[slot].is_empty()
                    && packets.iter().map(Vec::len).sum::<usize>() + pieces.len() <= CAPACITY;
                assert_eq!(journal.reserve_packet(slot, &pieces), expected);
                if expected {
                    for &p in pieces.iter().rev() {
                        frames[usize::from(p.frame_slot)].push((slot, p));
                    }
                    packets[slot] = pieces;
                }
            }
            1 => {
                let expected = if packets[slot].is_empty() {
                    None
                } else {
                    Some(packets[slot].remove(0))
                };
                assert_eq!(journal.pop_packet(slot), expected);
                if let Some(p) = expected {
                    let frame = &mut frames[usize::from(p.frame_slot)];
                    let index = frame.iter().rposition(|entry| *entry == (slot, p)).unwrap();
                    frame.remove(index);
                }
            }
            2 => {
                let frame = slot % FRAMES;
                let expected = frames[frame].pop();
                assert_eq!(
                    journal.pop_frame(frame, ids[frame]),
                    expected.map(|(_, p)| p)
                );
                if let Some((packet, p)) = expected {
                    let index = packets[packet]
                        .iter()
                        .position(|piece| *piece == p)
                        .unwrap();
                    packets[packet].remove(index);
                }
            }
            _ => {
                let frame = slot % FRAMES;
                let replacement = ids[frame] + FRAMES as u64;
                assert_eq!(
                    journal.bind_frame(frame, replacement, 60),
                    frames[frame].is_empty()
                );
                if frames[frame].is_empty() {
                    ids[frame] = replacement;
                }
            }
        }
        assert_eq!(
            journal.available(),
            CAPACITY - packets.iter().map(Vec::len).sum::<usize>()
        );
        for (slot, expected) in packets.iter().enumerate() {
            assert_eq!(journal.packet(slot).collect::<Vec<_>>(), *expected);
        }
        for (slot, expected) in frames.iter().enumerate() {
            assert_eq!(journal.frame_empty(slot, ids[slot]), expected.is_empty());
        }
    }
}

#[test]
fn history_slot_reuse_cannot_attach_old_feedback_to_new_payload() {
    use unquic::ack::{HistoryError, SENT_CAPACITY};
    let mut journal = bound(SENT_CAPACITY, 4);
    let mut history = History::new().unwrap();
    let old = piece(0, 0, 60, 0);
    history.reserve(1, 1500, true, true).unwrap();
    assert!(journal.reserve_packet(1, &[old]));
    assert!(history.submit(1, Duration::ZERO));
    let next = SENT_CAPACITY as u64 + 1;
    assert_eq!(
        history.reserve(next, 1500, true, true),
        Err(HistoryError::Full)
    );
    assert_eq!(journal.packet(1).collect::<Vec<_>>(), [old]);
    history.lose(1).unwrap();
    history.reserve(next, 1500, true, true).unwrap();
    assert_eq!(journal.pop_packet(1), Some(old)); // Eviction only after reserve succeeds.
    let new = piece(1, 0, 60, 0);
    assert!(journal.reserve_packet(1, &[new]));
    assert!(history.submit(next, Duration::from_millis(1)));
    assert_eq!(
        history
            .acknowledge(
                AckRanges::new(&[AckRange { start: 1, end: 1 }]).unwrap(),
                |_| panic!("evicted packet cannot visit a reused slot")
            )
            .unwrap()
            .packets,
        0
    );
    assert_eq!(journal.packet(1).collect::<Vec<_>>(), [new]);
    history
        .acknowledge(
            AckRanges::new(&[AckRange {
                start: next,
                end: next,
            }])
            .unwrap(),
            |ack| {
                assert_eq!(ack.packet.number, next);
                assert_eq!(journal.pop_packet(1), Some(new));
            },
        )
        .unwrap();
    assert_eq!(journal.available(), 4);
    assert_eq!(history.inflight_bytes(), 0);
}

#[test]
fn expired_payload_does_not_release_charge_or_let_a_late_ack_touch_replacement() {
    let mut journal = bound(2, 2);
    let mut history = History::new().unwrap();
    history.reserve(1, 1500, true, true).unwrap();
    assert!(journal.reserve_packet(0, &[piece(0, 0, 60, 0)]));
    assert!(history.submit(1, Duration::ZERO));
    assert!(journal.pop_frame(0, id(1)).is_some());
    assert_eq!(history.inflight_bytes(), 1500);
    assert!(journal.bind_frame(0, id(4), 60));
    let new = FramePiece {
        frame_id: id(4),
        ..piece(0, 0, 60, 0)
    };
    assert!(journal.reserve_packet(1, &[new]));
    history
        .acknowledge(
            AckRanges::new(&[AckRange { start: 1, end: 1 }]).unwrap(),
            |_| assert_eq!(journal.pop_packet(0), None),
        )
        .unwrap();
    assert_eq!(history.inflight_bytes(), 0);
    assert_eq!(journal.pop_frame(0, id(1)), None);
    assert_eq!(journal.pop_packet(1), Some(new));
    assert_eq!(journal.available(), 2);
}
