use std::time::Duration;
use unquic::{
    ack::{AckProgress, History, HistoryError, SENT_CAPACITY},
    wire::{AckRange, AckRanges},
};

fn ranges(ranges: &[AckRange]) -> AckRanges<'_> {
    AckRanges::new(ranges).unwrap()
}

fn sent(history: &mut History, number: u64, charged: bool) {
    history.reserve(number, 1500, charged, charged).unwrap();
    assert!(history.submit(number, Duration::from_micros(number)));
}

#[test]
fn reservations_are_not_submissions_and_errors_leave_history_unchanged() {
    let mut history = History::new().unwrap();
    for (number, bytes, eliciting, charged) in [
        (0, 1500, true, true),
        (1, 99, true, true),
        (1, 9001, true, true),
        (1, 1500, true, false),
    ] {
        assert_eq!(
            history.reserve(number, bytes, eliciting, charged),
            Err(HistoryError::Invalid)
        );
    }
    history.reserve(1, 1500, true, true).unwrap();
    assert_eq!(history.highest_sent(), 0);
    assert_eq!(history.packet(1), None);
    assert_eq!(history.inflight_bytes(), 0);
    assert_eq!(
        history.validate(ranges(&[AckRange { start: 1, end: 1 }])),
        Err(HistoryError::Future)
    );
    assert_eq!(history.reset(), Err(HistoryError::Full));
    assert!(history.cancel(1));
    assert!(!history.cancel(1));
    assert!(!history.submit(1, Duration::ZERO));
    assert_eq!(
        history.reserve(1, 1500, true, true),
        Err(HistoryError::Invalid)
    );
    sent(&mut history, 3, true);
    assert!(!history.submit(3, Duration::ZERO));
    assert!(!history.cancel(3));
    assert_eq!(history.inflight_bytes(), 1500);
    assert_eq!(
        history.validate(ranges(&[AckRange { start: 1, end: 3 }])),
        Err(HistoryError::Unsent)
    );
    assert_eq!(
        history.validate(ranges(&[AckRange { start: 2, end: 2 }])),
        Err(HistoryError::Unsent)
    );
    assert_eq!(
        history
            .outstanding()
            .map(|packet| packet.number)
            .collect::<Vec<_>>(),
        [3]
    );
}

#[test]
fn whole_ack_validation_precedes_callbacks_and_mutation() {
    let mut history = History::new().unwrap();
    sent(&mut history, 1, true);
    sent(&mut history, 3, true);
    sent(&mut history, 5, true);
    let mut calls = 0;
    assert_eq!(
        history.acknowledge(
            ranges(&[AckRange { start: 5, end: 5 }, AckRange { start: 1, end: 3 }]),
            |_| calls += 1
        ),
        Err(HistoryError::Unsent)
    );
    assert_eq!(calls, 0);
    assert!(!history.ack_pending());
    assert_eq!(history.inflight_bytes(), 4500);
    let progress = history
        .acknowledge(
            ranges(&[
                AckRange { start: 5, end: 5 },
                AckRange { start: 3, end: 3 },
                AckRange { start: 1, end: 1 },
            ]),
            |_| calls += 1,
        )
        .unwrap();
    assert_eq!(calls, 3);
    assert_eq!(progress.packets, 3);
    assert_eq!(progress.released_bytes, 4500);
    assert_eq!(progress.rtt_sent_at, Some(Duration::from_micros(5)));
    assert_eq!(history.inflight_bytes(), 0);
    assert_eq!(
        history
            .acknowledge(ranges(&[AckRange { start: 5, end: 5 }]), |_| calls += 1)
            .unwrap(),
        AckProgress::default()
    );
    assert_eq!(calls, 3);
}

#[test]
fn late_ack_keeps_metadata_without_releasing_charge_twice() {
    let mut history = History::new().unwrap();
    sent(&mut history, 1, true);
    let packet = history.lose(1).unwrap();
    assert_eq!(packet.outer_bytes, 1500);
    assert_eq!(history.lose(1), None);
    assert_eq!(history.packet(1), Some(packet));
    assert_eq!(history.inflight_bytes(), 0);
    sent(&mut history, 2, true);
    let progress = history
        .acknowledge(ranges(&[AckRange { start: 1, end: 2 }]), |acked| {
            assert_eq!(acked.was_in_flight, acked.packet.number == 2);
        })
        .unwrap();
    assert_eq!(progress.packets, 2);
    assert_eq!(progress.released_bytes, 1500);
    assert_eq!(history.inflight_bytes(), 0);
    assert_eq!(history.lose(2), None);
}

#[test]
fn feedback_acks_do_not_create_credit_or_rtt_samples() {
    let mut history = History::new().unwrap();
    sent(&mut history, 1, false);
    let progress = history
        .acknowledge(ranges(&[AckRange { start: 1, end: 1 }]), |acked| {
            assert!(!acked.was_in_flight);
            assert!(!acked.packet.charged);
        })
        .unwrap();
    assert_eq!(
        progress,
        AckProgress {
            packets: 1,
            ..AckProgress::default()
        }
    );
    sent(&mut history, 2, true);
    sent(&mut history, 3, false);
    let progress = history
        .acknowledge(ranges(&[AckRange { start: 2, end: 3 }]), |_| {})
        .unwrap();
    assert_eq!(progress.rtt_sent_at, Some(Duration::from_micros(3)));
    assert!(progress.eliciting);
    assert_eq!(progress.released_bytes, 1500);

    sent(&mut history, 4, true);
    sent(&mut history, 5, true);
    history
        .acknowledge(ranges(&[AckRange { start: 5, end: 5 }]), |_| {})
        .unwrap();
    let progress = history
        .acknowledge(ranges(&[AckRange { start: 4, end: 5 }]), |_| {})
        .unwrap();
    assert_eq!(progress.packets, 1);
    assert_eq!(
        progress.rtt_sent_at, None,
        "largest was already acknowledged"
    );
}

#[test]
fn ring_pressure_never_forgets_pending_or_unresolved_charges() {
    let mut history = History::new().unwrap();
    history.reserve(1, 1500, true, true).unwrap();
    for number in 2..=SENT_CAPACITY as u64 {
        sent(&mut history, number, false);
    }
    let next = SENT_CAPACITY as u64 + 1;
    assert_eq!(
        history.reserve(next, 1500, true, true),
        Err(HistoryError::Full)
    );
    assert_eq!(history.floor(), 1);
    assert!(history.submit(1, Duration::from_micros(1)));
    assert_eq!(
        history.reserve(next, 1500, true, true),
        Err(HistoryError::Full)
    );
    history.lose(1).unwrap();
    history.reserve(next, 1500, true, true).unwrap();
    assert_eq!(history.floor(), 2);
    assert_eq!(history.packet(1), None);
    assert!(history.submit(next, Duration::from_micros(next)));
    assert_eq!(
        history.validate(ranges(&[AckRange {
            start: 1,
            end: next
        }])),
        Ok(())
    );
    let progress = history
        .acknowledge(
            ranges(&[AckRange {
                start: 1,
                end: next,
            }]),
            |_| {},
        )
        .unwrap();
    assert_eq!(progress.packets, SENT_CAPACITY as u32);
    assert_eq!(progress.released_bytes, 1500);
    assert_eq!(history.inflight_bytes(), 0);
}

#[test]
fn feedback_only_window_can_advance_without_feedback_deadlock() {
    let mut history = History::new().unwrap();
    for number in 1..=SENT_CAPACITY as u64 + 130 {
        sent(&mut history, number, false);
    }
    assert_eq!(history.floor(), 131);
    assert_eq!(history.inflight_bytes(), 0);
    assert_eq!(history.outstanding().count(), 0);
    let end = history.highest_sent();
    let progress = history
        .acknowledge(ranges(&[AckRange { start: 1, end }]), |_| {})
        .unwrap();
    assert_eq!(progress.packets, SENT_CAPACITY as u32);
    assert!(!progress.eliciting);
    assert_eq!(progress.rtt_sent_at, None);
}

#[test]
fn failed_large_advance_keeps_every_prior_submission() {
    let mut history = History::new().unwrap();
    for number in 1..=128 {
        sent(&mut history, number, number == 128);
    }
    assert_eq!(
        history.reserve(u64::MAX, 1500, true, true),
        Err(HistoryError::Full)
    );
    assert_eq!(history.floor(), 1);
    assert_eq!(history.highest_sent(), 128);
    let progress = history
        .acknowledge(ranges(&[AckRange { start: 1, end: 128 }]), |_| {})
        .unwrap();
    assert_eq!(progress.packets, 128);
    assert_eq!(progress.released_bytes, 1500);
}

#[test]
fn large_jumps_and_maximum_numbers_have_bounded_processing_and_permanent_holes() {
    let mut history = History::new().unwrap();
    sent(&mut history, 1, false);
    sent(&mut history, u64::MAX - 1, true);
    history.reserve(u64::MAX, 9000, true, true).unwrap();
    assert_eq!(
        history.validate(ranges(&[AckRange {
            start: 1,
            end: u64::MAX - 1
        }])),
        Err(HistoryError::Unsent)
    );
    assert_eq!(
        history.validate(ranges(&[AckRange {
            start: u64::MAX,
            end: u64::MAX
        }])),
        Err(HistoryError::Future)
    );
    assert_eq!(
        history
            .acknowledge(ranges(&[AckRange { start: 1, end: 1 }]), |_| panic!(
                "old packet"
            ))
            .unwrap(),
        AckProgress::default()
    );
    assert!(history.submit(u64::MAX, Duration::MAX));
    assert_eq!(
        history
            .outstanding()
            .map(|packet| packet.number)
            .collect::<Vec<_>>(),
        [u64::MAX - 1, u64::MAX]
    );
    let progress = history
        .acknowledge(
            ranges(&[AckRange {
                start: u64::MAX - 1,
                end: u64::MAX,
            }]),
            |_| {},
        )
        .unwrap();
    assert_eq!(progress.packets, 2);
    assert_eq!(progress.released_bytes, 10_500);
    assert_eq!(progress.rtt_sent_at, Some(Duration::MAX));
    assert_eq!(
        history.reserve(u64::MAX, 1500, true, true),
        Err(HistoryError::Invalid)
    );
}

#[test]
fn reset_requires_settlement_and_does_not_leak_old_bits() {
    let mut history = History::new().unwrap();
    sent(&mut history, 65, true);
    history.reserve(66, 100, false, false).unwrap();
    assert_eq!(history.reset(), Err(HistoryError::Full));
    history.lose(65).unwrap();
    assert_eq!(history.reset(), Err(HistoryError::Full));
    assert!(history.cancel(66));
    history.reset().unwrap();
    assert_eq!(history.floor(), 1);
    assert_eq!(history.highest_sent(), 0);
    assert_eq!(history.packet(65), None);
    assert_eq!(history.outstanding().count(), 0);
    sent(&mut history, 66, true);
    assert_eq!(
        history.validate(ranges(&[AckRange { start: 65, end: 66 }])),
        Err(HistoryError::Unsent)
    );
    assert_eq!(
        history
            .acknowledge(ranges(&[AckRange { start: 66, end: 66 }]), |_| {})
            .unwrap()
            .released_bytes,
        1500
    );
}

#[test]
fn partial_submission_cancels_unsent_numbers_and_preserves_prior_credit() {
    let mut history = History::new().unwrap();
    for number in 1..=64 {
        history.reserve(number, 1280, true, true).unwrap();
    }
    for number in 1..=2 {
        assert!(history.submit(number, Duration::ZERO));
    }
    for number in 3..=64 {
        assert!(history.cancel(number));
    }
    sent(&mut history, 65, true);
    history
        .acknowledge(ranges(&[AckRange { start: 65, end: 65 }]), |_| {})
        .unwrap();
    assert_eq!(history.inflight_bytes(), 2560);
    for number in 66..=127 {
        sent(&mut history, number, true);
    }
    assert_eq!(
        history.validate(ranges(&[AckRange { start: 3, end: 64 }])),
        Err(HistoryError::Unsent)
    );
    assert_eq!(history.outstanding().count(), 64);
    assert_eq!(history.highest_sent(), 127);
}

#[test]
fn compact_timestamps_preserve_full_duration_through_loss_ack_and_reset() {
    assert_eq!(
        History::storage_bytes(),
        size_of::<History>() + 16 * SENT_CAPACITY
    );
    let mut history = History::new().unwrap();
    for now in [
        Duration::ZERO,
        Duration::from_nanos(1),
        Duration::new(1, 999_999_999),
        Duration::MAX,
    ] {
        history.reserve(1, 9000, true, true).unwrap();
        assert!(history.submit(1, now));
        let packet = history.packet(1).unwrap();
        assert_eq!(packet.sent_at, now);
        assert_eq!(packet.outer_bytes, 9000);
        assert_eq!(history.outstanding().next(), Some(packet));
        assert_eq!(history.lose(1), Some(packet));
        let progress = history
            .acknowledge(ranges(&[AckRange { start: 1, end: 1 }]), |acked| {
                assert_eq!(acked.packet, packet);
                assert!(!acked.was_in_flight);
            })
            .unwrap();
        assert_eq!(progress.rtt_sent_at, Some(now));
        assert_eq!(progress.released_bytes, 0);
        history.reset().unwrap();
    }
}

#[test]
fn bounded_ack_yields_without_losing_callbacks_or_credit() {
    let mut history = History::new().unwrap();
    let end = SENT_CAPACITY as u64;
    for number in 1..=end {
        sent(&mut history, number, true);
    }
    history
        .begin_ack(ranges(&[AckRange { start: 1, end }]))
        .unwrap();
    assert_eq!(history.advance_ack(0, |_| panic!("zero budget")), None);
    assert_eq!(history.inflight_bytes(), end * 1500);
    let mut calls = 0;
    let progress = loop {
        let before = calls;
        let progress = history.advance_ack(64, |acked| {
            calls += 1;
            assert_eq!(acked.packet.number, calls);
            assert!(acked.was_in_flight);
        });
        assert!(calls - before <= 64);
        assert_eq!(history.inflight_bytes(), (end - calls) * 1500);
        if let Some(progress) = progress {
            break progress;
        }
        assert!(history.ack_pending());
    };
    assert_eq!(calls, end);
    assert_eq!(progress.packets, SENT_CAPACITY as u32);
    assert_eq!(progress.released_bytes, end * 1500);
    assert_eq!(progress.rtt_sent_at, Some(Duration::from_micros(end)));
    assert!(!history.ack_pending());
    assert_eq!(history.advance_ack(64, |_| panic!("no pending ACK")), None);

    // An all-duplicate ACK still spends its budget scanning the bitmap.
    history
        .begin_ack(ranges(&[AckRange { start: 1, end }]))
        .unwrap();
    let mut turns = 0;
    loop {
        turns += 1;
        if let Some(progress) = history.advance_ack(1, |_| panic!("duplicate callback")) {
            assert_eq!(progress, AckProgress::default());
            break;
        }
        assert!(turns <= SENT_CAPACITY / 64 + 2);
    }
    // Packet numbers 1..=65536 straddle 1025 words, followed by one transition.
    assert_eq!(turns, SENT_CAPACITY / 64 + 2);
}

#[test]
fn pending_ack_pins_metadata_and_rejects_competing_operations() {
    let mut history = History::new().unwrap();
    sent(&mut history, 1, false);
    let values = [AckRange { start: 1, end: 1 }];
    history.begin_ack(ranges(&values)).unwrap();
    assert_eq!(history.begin_ack(ranges(&values)), Err(HistoryError::Busy));
    assert_eq!(
        history.acknowledge(ranges(&values), |_| panic!("competing ACK")),
        Err(HistoryError::Busy)
    );
    assert_eq!(history.reset(), Err(HistoryError::Busy));
    // Submissions within the pinned window remain possible.
    sent(&mut history, SENT_CAPACITY as u64, false);
    let next = SENT_CAPACITY as u64 + 1;
    assert_eq!(
        history.reserve(next, 1500, false, false),
        Err(HistoryError::Full)
    );
    assert_eq!(history.floor(), 1);
    let mut calls = 0;
    while history.ack_pending() {
        history.advance_ack(1, |acked| {
            calls += 1;
            assert_eq!(acked.packet.number, 1);
        });
    }
    assert_eq!(calls, 1);
    sent(&mut history, next, false);
    assert_eq!(history.floor(), 2);
    assert_eq!(history.packet(1), None);
    history.reset().unwrap();
}

#[test]
fn pending_ack_protects_covered_packets_from_interleaved_loss() {
    let mut history = History::new().unwrap();
    for number in 1..=5 {
        sent(&mut history, number, true);
    }
    history.lose(1).unwrap();
    history
        .begin_ack(ranges(&[
            AckRange { start: 4, end: 5 },
            AckRange { start: 1, end: 2 },
        ]))
        .unwrap();
    assert_eq!(history.lose(4), None);
    assert_eq!(history.lose(2), None);
    assert!(history.lose(3).is_some());
    assert_eq!(history.inflight_bytes(), 4500);
    let mut seen = 0_u8;
    let progress = loop {
        if let Some(progress) = history.advance_ack(1, |acked| {
            let bit = 1 << acked.packet.number;
            assert_eq!(seen & bit, 0);
            seen |= bit;
            assert_eq!(acked.was_in_flight, acked.packet.number != 1);
        }) {
            break progress;
        }
        assert_eq!(history.lose(2), None);
        assert_eq!(history.lose(5), None);
    };
    assert_eq!(seen, 0b110110);
    assert_eq!(progress.packets, 4);
    assert_eq!(progress.released_bytes, 4500);
    assert_eq!(history.inflight_bytes(), 0);
}

#[test]
fn bounded_ack_keeps_rtt_candidate_until_later_eliciting_range() {
    let mut history = History::new().unwrap();
    sent(&mut history, 1, true);
    sent(&mut history, 3, false);
    history
        .begin_ack(ranges(&[
            AckRange { start: 3, end: 3 },
            AckRange { start: 1, end: 1 },
        ]))
        .unwrap();
    // Highest packet is feedback-only; a later range makes this an RTT sample.
    assert_eq!(
        history.advance_ack(2, |acked| assert_eq!(acked.packet.number, 3)),
        None
    );
    assert_eq!(history.inflight_bytes(), 1500);
    let progress = loop {
        if let Some(progress) = history.advance_ack(1, |acked| assert_eq!(acked.packet.number, 1)) {
            break progress;
        }
    };
    assert!(progress.eliciting);
    assert_eq!(progress.rtt_sent_at, Some(Duration::from_micros(3)));
}

#[test]
fn sparse_outstanding_scan_budgets_empty_words_and_stops_at_its_original_end() {
    let mut history = History::new().unwrap();
    let last = SENT_CAPACITY as u64;
    sent(&mut history, last, true);
    let mut scan = history.outstanding_scan();
    let mut budget = 0;
    assert_eq!(scan.advance(&history, &mut budget), None);
    assert!(!scan.is_complete());
    for _ in 0..16 {
        let mut budget = 64;
        assert_eq!(scan.advance(&history, &mut budget), None);
        assert_eq!(budget, 0);
        assert!(!scan.is_complete());
    }
    // Appending after a pass starts must not extend that pass indefinitely.
    sent(&mut history, last + 1, true);
    let mut budget = 64;
    assert_eq!(scan.advance(&history, &mut budget).unwrap().number, last);
    assert_eq!(budget, 63);
    assert!(scan.is_complete());
    assert_eq!(scan.advance(&history, &mut budget), None);
    assert_eq!(budget, 63);
    assert_eq!(history.outstanding().count(), 2);
}

#[test]
fn outstanding_scan_rechecks_ack_loss_and_eviction_between_steps() {
    let mut history = History::new().unwrap();
    for number in 1..=4 {
        sent(&mut history, number, true);
    }
    let mut scan = history.outstanding_scan();
    assert_eq!(scan.advance(&history, &mut 1).unwrap().number, 1);
    history.lose(1).unwrap();
    history.lose(2).unwrap();
    history
        .acknowledge(ranges(&[AckRange { start: 3, end: 3 }]), |_| {})
        .unwrap();
    assert_eq!(scan.advance(&history, &mut 1).unwrap().number, 4);
    assert!(scan.is_complete());
    history.lose(4).unwrap();
    let next = SENT_CAPACITY as u64 + 4;
    sent(&mut history, next, true);
    // A cursor whose remaining span was evicted cannot visit recycled ring slots.
    let mut scan = history.outstanding_scan();
    assert!(!scan.is_complete());
    history.lose(next).unwrap();
    sent(&mut history, next + SENT_CAPACITY as u64, true);
    let mut budget = 64;
    assert_eq!(scan.advance(&history, &mut budget), None);
    assert_eq!(budget, 64);
    assert!(scan.is_complete());
}

#[test]
fn outstanding_scan_skips_holes_feedback_and_preserves_pending_ack_protection() {
    let mut history = History::new().unwrap();
    sent(&mut history, 1, false);
    history.reserve(2, 1500, true, true).unwrap();
    sent(&mut history, 3, true);
    sent(&mut history, 4, true);
    history.reserve(5, 1500, true, true).unwrap();
    history.cancel(5);
    let mut scan = history.outstanding_scan();
    history
        .begin_ack(ranges(&[AckRange { start: 3, end: 3 }]))
        .unwrap();
    let mut budget = 1;
    assert_eq!(scan.advance(&history, &mut budget).unwrap().number, 3);
    assert_eq!(budget, 0);
    assert_eq!(history.lose(3), None, "validated ACK wins over a loss scan");
    assert_eq!(scan.advance(&history, &mut 1).unwrap().number, 4);
    assert!(history.lose(4).is_some());
    // Only packet 3 remains; packet 5 was never successfully submitted.
    assert_eq!(scan.advance(&history, &mut 1), None);
    assert!(scan.is_complete());
    assert_eq!(history.inflight_bytes(), 1500);
}

#[test]
fn outstanding_scan_handles_empty_full_and_maximum_number_windows() {
    let mut history = History::new().unwrap();
    let mut scan = history.outstanding_scan();
    assert!(scan.is_complete());
    let mut budget = 64;
    assert_eq!(scan.advance(&history, &mut budget), None);
    assert_eq!(budget, 64);
    for number in 1..=SENT_CAPACITY as u64 {
        sent(&mut history, number, true);
    }
    let mut scan = history.outstanding_scan();
    for turn in 0..SENT_CAPACITY / 64 {
        let mut budget = 64;
        for offset in 1..=64 {
            let packet = scan.advance(&history, &mut budget).unwrap();
            assert_eq!(packet.number, (turn * 64 + offset) as u64);
            assert_eq!(budget, 64 - offset);
            assert_eq!(history.lose(packet.number), Some(packet));
        }
        assert_eq!(scan.advance(&history, &mut budget), None);
    }
    assert!(scan.is_complete());
    assert_eq!(history.inflight_bytes(), 0);
    sent(&mut history, u64::MAX - 1, true);
    sent(&mut history, u64::MAX, true);
    let mut scan = history.outstanding_scan();
    let mut numbers = Vec::new();
    let mut turns = 0;
    while !scan.is_complete() {
        let mut budget = 64;
        while let Some(packet) = scan.advance(&history, &mut budget) {
            numbers.push(packet.number);
        }
        turns += 1;
        assert!(turns <= 17);
    }
    assert_eq!(numbers, [u64::MAX - 1, u64::MAX]);
}

#[test]
fn prefix_retirement_stops_at_reservations_and_charges_without_settling_them() {
    let mut history = History::new().unwrap();
    assert_eq!(history.retire_oldest_resolved(), Ok(None));
    history.reserve(1, 1500, true, true).unwrap();
    sent(&mut history, 2, true);
    sent(&mut history, 3, false);
    assert_eq!(history.retire_oldest_resolved(), Ok(None));
    assert_eq!(history.floor(), 1);
    assert!(history.cancel(1));
    assert_eq!(
        history.retire_oldest_resolved(),
        Ok(Some(unquic::ack::RetiredEntry {
            number: 1,
            sample_slot: None
        }))
    );
    assert_eq!(history.retire_oldest_resolved(), Ok(None));
    assert_eq!(history.inflight_bytes(), 1500);
    assert!(history.lose(2).is_some());
    assert_eq!(
        history.retire_oldest_resolved(),
        Ok(Some(unquic::ack::RetiredEntry {
            number: 2,
            sample_slot: None
        }))
    );
    assert_eq!(
        history.retire_oldest_resolved(),
        Ok(Some(unquic::ack::RetiredEntry {
            number: 3,
            sample_slot: None
        }))
    );
    assert_eq!(history.retire_oldest_resolved(), Ok(None));
    assert_eq!(history.floor(), 4);
    assert_eq!(history.highest_sent(), 3);
    let progress = history
        .acknowledge(ranges(&[AckRange { start: 1, end: 3 }]), |_| {
            panic!("retired delivery")
        })
        .unwrap();
    assert_eq!(progress.packets, 0); // The retired canceled hole is outside validation.
    assert_eq!(history.inflight_bytes(), 0);
    assert_eq!(history.packet(2), None);
}

#[test]
fn pending_ack_pins_retirement_until_all_chunks_and_callbacks_complete() {
    let mut history = History::new().unwrap();
    sent(&mut history, 1, true);
    sent(&mut history, 2, true);
    history
        .begin_ack(ranges(&[AckRange { start: 1, end: 2 }]))
        .unwrap();
    let mut delivered = Vec::new();
    assert!(
        history
            .advance_ack(2, |a| delivered.push(a.packet.number))
            .is_none()
    );
    assert_eq!(delivered, [1]);
    assert_eq!(history.retire_oldest_resolved(), Err(HistoryError::Busy));
    assert_eq!(history.floor(), 1);
    assert!(
        history
            .advance_ack(4, |a| delivered.push(a.packet.number))
            .is_some()
    );
    assert_eq!(delivered, [1, 2]);
    assert_eq!(
        history.retire_oldest_resolved(),
        Ok(Some(unquic::ack::RetiredEntry {
            number: 1,
            sample_slot: None
        }))
    );
    assert_eq!(
        history.retire_oldest_resolved(),
        Ok(Some(unquic::ack::RetiredEntry {
            number: 2,
            sample_slot: None
        }))
    );
    assert_eq!(history.inflight_bytes(), 0);
}

#[test]
fn prefix_retirement_crosses_bitmap_and_ring_boundaries_and_preserves_unsent_checks() {
    let mut history = History::new().unwrap();
    for number in 1..=SENT_CAPACITY as u64 + 70 {
        sent(&mut history, number, false);
        assert_eq!(
            history.retire_oldest_resolved(),
            Ok(Some(unquic::ack::RetiredEntry {
                number,
                sample_slot: None
            }))
        );
        assert_eq!(history.packet(number), None);
        assert_eq!(history.floor(), number + 1);
    }
    let floor = history.floor();
    sent(&mut history, floor + 2, false);
    assert_eq!(
        history.validate(ranges(&[AckRange {
            start: floor,
            end: floor + 2
        }])),
        Err(HistoryError::Unsent)
    );
    assert_eq!(
        history.retire_oldest_resolved(),
        Ok(Some(unquic::ack::RetiredEntry {
            number: floor,
            sample_slot: None
        }))
    );
    assert_eq!(
        history.retire_oldest_resolved(),
        Ok(Some(unquic::ack::RetiredEntry {
            number: floor + 1,
            sample_slot: None
        }))
    );
    assert!(
        history
            .validate(ranges(&[AckRange {
                start: floor,
                end: floor + 2
            }]))
            .is_ok()
    );
    assert_eq!(
        history.retire_oldest_resolved(),
        Ok(Some(unquic::ack::RetiredEntry {
            number: floor + 2,
            sample_slot: None
        }))
    );
}

#[test]
fn maximum_number_retirement_never_wraps_or_reuses_identity() {
    let mut history = History::new().unwrap();
    sent(&mut history, u64::MAX, false);
    let start = history.floor();
    for number in start..u64::MAX {
        assert_eq!(
            history.retire_oldest_resolved(),
            Ok(Some(unquic::ack::RetiredEntry {
                number,
                sample_slot: None
            }))
        );
    }
    assert_eq!(
        history.retire_oldest_resolved(),
        Ok(Some(unquic::ack::RetiredEntry {
            number: u64::MAX,
            sample_slot: None,
        }))
    );
    assert_eq!(history.retire_oldest_resolved(), Ok(None));
    assert_eq!(history.floor(), u64::MAX);
    assert_eq!(history.packet(u64::MAX), None);
    assert_eq!(
        history
            .acknowledge(
                ranges(&[AckRange {
                    start: u64::MAX,
                    end: u64::MAX
                }]),
                |_| panic!("retired maximum packet")
            )
            .unwrap()
            .packets,
        0
    );
    assert_eq!(
        history.reserve(u64::MAX, 1500, false, false),
        Err(HistoryError::Invalid)
    );
    assert_eq!(
        history.reserve(1, 1500, false, false),
        Err(HistoryError::Invalid)
    );
    history.reset().unwrap();
    assert_eq!(history.retire_oldest_resolved(), Ok(None));
}

#[test]
fn sampled_history_keeps_sixteen_byte_entries_and_all_size_flags() {
    use std::num::NonZeroU16;
    assert_eq!(
        History::storage_bytes(),
        size_of::<History>() + 16 * SENT_CAPACITY
    );
    let mut history = History::new().unwrap();
    for bytes in [100, 1280, 1500, 9000] {
        for eliciting in [false, true] {
            history.reserve(1, bytes, eliciting, true).unwrap();
            let slot = NonZeroU16::new(u16::MAX).unwrap();
            assert!(history.submit_sampled(1, Duration::MAX, slot));
            assert!(!history.submit_sampled(1, Duration::ZERO, slot));
            let packet = history.packet(1).unwrap();
            assert_eq!(
                (packet.outer_bytes, packet.eliciting, packet.charged),
                (bytes, eliciting, true)
            );
            assert_eq!(
                (packet.sent_at, packet.sample_slot),
                (Duration::MAX, Some(slot))
            );
            history
                .acknowledge(ranges(&[AckRange { start: 1, end: 1 }]), |ack| {
                    assert_eq!(ack.packet.sample_slot, Some(slot));
                })
                .unwrap();
            assert_eq!(history.packet(1).unwrap().sample_slot, None);
            history.reset().unwrap();
        }
    }
    history.reserve(1, 100, false, false).unwrap();
    assert!(!history.submit_sampled(1, Duration::ZERO, NonZeroU16::new(1).unwrap()));
    assert!(history.cancel(1)); // Invalid sampled submission leaves its reservation intact.
}

#[test]
fn ack_takes_sample_ownership_once_and_later_retirement_cannot_alias_reused_slot() {
    use std::num::NonZeroU16;
    let mut history = History::new().unwrap();
    let slot = NonZeroU16::new(7).unwrap();
    history.reserve(1, 1500, true, true).unwrap();
    assert!(history.submit_sampled(1, Duration::ZERO, slot));
    assert_eq!(history.lose(1).unwrap().sample_slot, Some(slot));
    assert_eq!(history.packet(1).unwrap().sample_slot, Some(slot)); // Loss only copies.
    let mut report = None;
    history
        .acknowledge(ranges(&[AckRange { start: 1, end: 1 }]), |ack| {
            assert!(!ack.was_in_flight);
            report = ack.packet.sample_slot;
        })
        .unwrap();
    assert_eq!(report, Some(slot));
    assert_eq!(history.packet(1).unwrap().sample_slot, None);
    // After consuming the ACK report the owner may lend the same slot to packet two.
    history.reserve(2, 1500, true, true).unwrap();
    assert!(history.submit_sampled(2, Duration::ZERO, slot));
    assert_eq!(
        history
            .retire_oldest_resolved()
            .unwrap()
            .unwrap()
            .sample_slot,
        None
    );
    assert_eq!(history.packet(2).unwrap().sample_slot, Some(slot));
    assert_eq!(
        history
            .acknowledge(ranges(&[AckRange { start: 1, end: 1 }]), |_| panic!(
                "duplicate ACK cannot return a stale handle"
            ))
            .unwrap()
            .packets,
        0
    );
    assert_eq!(history.retire_oldest_resolved(), Ok(None));
}

#[test]
fn samples_block_implicit_eviction_and_reset_until_explicit_ordered_retirement() {
    use std::num::NonZeroU16;
    let mut history = History::new().unwrap();
    for number in 1..=2 {
        history.reserve(number, 1500, true, true).unwrap();
        assert!(history.submit_sampled(
            number,
            Duration::ZERO,
            NonZeroU16::new(number as u16).unwrap()
        ));
        assert!(history.lose(number).is_some());
    }
    assert_eq!(history.reset(), Err(HistoryError::Full));
    let next = SENT_CAPACITY as u64 + 1;
    assert_eq!(
        history.reserve(next, 1500, true, true),
        Err(HistoryError::Full)
    );
    assert_eq!(history.floor(), 1);
    let retired = history.retire_oldest_resolved().unwrap().unwrap();
    assert_eq!(
        (retired.number, retired.sample_slot),
        (1, NonZeroU16::new(1))
    );
    history.reserve(next, 1500, true, true).unwrap(); // No implicit floor movement needed.
    assert_eq!(history.packet(2).unwrap().sample_slot, NonZeroU16::new(2));
    assert!(history.cancel(next));
    assert_eq!(history.reset(), Err(HistoryError::Full));
    assert_eq!(
        history
            .retire_oldest_resolved()
            .unwrap()
            .unwrap()
            .sample_slot,
        NonZeroU16::new(2)
    );
    history.reset().unwrap();
}

#[test]
fn partial_ack_transfers_only_reported_handles_and_keeps_lost_tail_pinned() {
    use std::num::NonZeroU16;
    let mut history = History::new().unwrap();
    for number in 1..=3 {
        history.reserve(number, 1280, true, true).unwrap();
        assert!(history.submit_sampled(
            number,
            Duration::ZERO,
            NonZeroU16::new(number as u16).unwrap()
        ));
    }
    history
        .begin_ack(ranges(&[AckRange { start: 1, end: 2 }]))
        .unwrap();
    let mut report = None;
    assert!(
        history
            .advance_ack(2, |ack| report = ack.packet.sample_slot)
            .is_none()
    );
    assert_eq!(report, NonZeroU16::new(1));
    assert_eq!(history.packet(1).unwrap().sample_slot, None);
    assert_eq!(history.packet(2).unwrap().sample_slot, NonZeroU16::new(2));
    assert_eq!(history.retire_oldest_resolved(), Err(HistoryError::Busy));
    assert_eq!(history.lose(3).unwrap().sample_slot, NonZeroU16::new(3));
    assert!(
        history
            .advance_ack(4, |ack| report = ack.packet.sample_slot)
            .is_some()
    );
    assert_eq!(report, NonZeroU16::new(2));
    for _ in 0..2 {
        assert_eq!(
            history
                .retire_oldest_resolved()
                .unwrap()
                .unwrap()
                .sample_slot,
            None
        );
    }
    assert_eq!(
        history
            .retire_oldest_resolved()
            .unwrap()
            .unwrap()
            .sample_slot,
        NonZeroU16::new(3)
    );
    history.reset().unwrap();
}

#[test]
fn maximum_sample_can_retire_and_reset_but_canceled_maximum_stays_unsent() {
    use std::num::NonZeroU16;
    let mut history = History::new().unwrap();
    history.reserve(u64::MAX, 1500, true, true).unwrap();
    assert!(history.submit_sampled(u64::MAX, Duration::MAX, NonZeroU16::new(1).unwrap()));
    assert!(history.lose(u64::MAX).is_some());
    assert_eq!(history.reset(), Err(HistoryError::Full));
    for _ in 0..SENT_CAPACITY - 1 {
        history.retire_oldest_resolved().unwrap().unwrap();
    }
    let retired = history.retire_oldest_resolved().unwrap().unwrap();
    assert_eq!(
        (retired.number, retired.sample_slot),
        (u64::MAX, NonZeroU16::new(1))
    );
    assert_eq!(history.retire_oldest_resolved(), Ok(None));
    assert_eq!(
        history
            .acknowledge(
                ranges(&[AckRange {
                    start: 1,
                    end: u64::MAX
                }]),
                |_| panic!("old ACK")
            )
            .unwrap()
            .packets,
        0
    );
    history.reset().unwrap();
    sent(&mut history, u64::MAX - 1, false);
    history.reserve(u64::MAX, 1500, true, true).unwrap();
    assert!(history.cancel(u64::MAX));
    for _ in 0..SENT_CAPACITY {
        history.retire_oldest_resolved().unwrap().unwrap();
    }
    assert_eq!(
        history.validate(ranges(&[AckRange {
            start: u64::MAX,
            end: u64::MAX
        }])),
        Err(HistoryError::Future)
    );
}
