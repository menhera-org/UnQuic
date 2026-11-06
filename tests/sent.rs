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
