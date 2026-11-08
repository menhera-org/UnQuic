use std::time::Duration;
use unquic::bbr::{Delivery, DeliveryError, RateSample};

fn ns(n: u64) -> Duration {
    Duration::from_nanos(n)
}

#[test]
fn pinned_draft_07_delivery_vectors() {
    for (name, fixture) in [
        (
            "compression",
            include_str!("vectors/bbr-07/delivery-compression.txt"),
        ),
        ("idle", include_str!("vectors/bbr-07/delivery-idle.txt")),
        ("loss", include_str!("vectors/bbr-07/delivery-loss.txt")),
        (
            "app",
            include_str!("vectors/bbr-07/delivery-app-limited.txt"),
        ),
    ] {
        let mut delivery = Delivery::new();
        let mut packets = Vec::new();
        for (line, text) in fixture.lines().enumerate() {
            let fields: Vec<_> = text.split_whitespace().collect();
            if fields.is_empty() || fields[0].starts_with('#') {
                continue;
            }
            let n = |i: usize| fields[i].parse::<u64>().unwrap();
            let flag = |i: usize| match fields[i] {
                "0" => false,
                "1" => true,
                _ => panic!("bad flag"),
            };
            match fields[0] {
                "S" => packets.push(
                    delivery
                        .sent(ns(n(1)), n(2).try_into().unwrap(), n(3), flag(4))
                        .unwrap(),
                ),
                "M" => delivery.application_limited(n(1)).unwrap(),
                "L" => assert_eq!(
                    delivery
                        .declared_lost(&mut packets[n(1) as usize])
                        .unwrap()
                        .is_some(),
                    flag(2)
                ),
                "A" | "E" => {
                    let empty = fields[0] == "E";
                    let host = if empty { 2 } else { 3 };
                    delivery.begin_ack(ns(n(1)), flag(host)).unwrap();
                    if fields[host + 1] != "-" {
                        for index in fields[host + 1].split(',') {
                            delivery
                                .acknowledged(&mut packets[index.parse::<usize>().unwrap()])
                                .unwrap();
                        }
                    }
                    let actual = delivery
                        .finish_ack(if empty { Duration::ZERO } else { ns(n(2)) })
                        .unwrap();
                    let expected = (!empty).then(|| RateSample {
                        rate: (fields[5] != "-").then(|| n(5)),
                        interval: ns(n(6)),
                        delivered: n(7),
                        newly_acked: n(8),
                        prior_delivered: n(9),
                        lost: n(10),
                        tx_in_flight: n(11),
                        app_limited: flag(12),
                        host_limited: flag(13),
                        pmtu_only: false,
                        last_packet: n(14),
                    });
                    assert_eq!(actual, expected, "{name}:{}", line + 1);
                }
                _ => panic!("unknown vector operation"),
            }
        }
    }
}

#[test]
fn newest_transmission_is_independent_of_ack_walk_order_and_lane_numbers() {
    for order in [[0, 1, 2], [2, 1, 0], [1, 0, 2], [2, 0, 1]] {
        let mut delivery = Delivery::new();
        let mut packets = [
            delivery.sent(ns(0), 100, 0, false).unwrap(),
            delivery.sent(ns(0), 200, 100, false).unwrap(),
            delivery.sent(ns(0), 300, 300, true).unwrap(),
        ];
        delivery.begin_ack(ns(1000), false).unwrap();
        for index in order {
            assert!(delivery.acknowledged(&mut packets[index]).unwrap());
            assert!(!delivery.acknowledged(&mut packets[index]).unwrap());
        }
        let sample = delivery.finish_ack(Duration::ZERO).unwrap().unwrap();
        assert_eq!(
            (sample.rate, sample.last_packet, sample.tx_in_flight),
            (Some(600_000_000), 3, 600)
        );
        assert_eq!(sample.newly_acked, 600);
        assert!(sample.host_limited);
        assert!(!sample.app_limited);
    }
}

#[test]
fn bounded_ack_chunks_block_intervening_transitions_and_keep_one_sample() {
    let mut delivery = Delivery::new();
    let mut first = delivery.sent(ns(0), 1280, 0, false).unwrap();
    let mut second = delivery.sent(ns(1000), 1500, 1280, false).unwrap();
    assert_eq!(
        delivery.acknowledged(&mut first),
        Err(DeliveryError::Invalid)
    );
    assert_eq!(delivery.finish_ack(ns(0)), Err(DeliveryError::Invalid));
    assert_eq!(
        delivery.begin_ack(ns(999), false),
        Err(DeliveryError::Invalid)
    );
    delivery.begin_ack(ns(10000), false).unwrap();
    delivery.acknowledged(&mut first).unwrap();
    assert_eq!(
        delivery.sent(ns(20000), 100, 1500, false).unwrap_err(),
        DeliveryError::Busy
    );
    assert_eq!(
        delivery.begin_ack(ns(20000), false),
        Err(DeliveryError::Busy)
    );
    assert_eq!(delivery.application_limited(1500), Err(DeliveryError::Busy));
    assert_eq!(
        delivery.declared_lost(&mut second),
        Err(DeliveryError::Busy)
    );
    delivery.acknowledged(&mut second).unwrap();
    let sample = delivery.finish_ack(ns(1)).unwrap().unwrap();
    assert_eq!(
        (sample.delivered, sample.newly_acked, sample.rate),
        (2780, 2780, Some(278_000_000))
    );
    assert_eq!(delivery.lost(), 0);
    assert_eq!(
        delivery.sent(ns(9999), 100, 0, false).unwrap_err(),
        DeliveryError::Invalid
    );
    assert_eq!(
        delivery.begin_ack(ns(9999), false),
        Err(DeliveryError::Invalid)
    );
}

#[test]
fn invalid_sends_and_full_duration_ranges_preserve_accounting() {
    let mut delivery = Delivery::new();
    for size in [0, 99, 9001, u16::MAX] {
        assert_eq!(
            delivery.sent(ns(0), size, 0, false).unwrap_err(),
            DeliveryError::Invalid
        );
    }
    assert_eq!(
        delivery.sent(ns(0), 100, u64::MAX, false).unwrap_err(),
        DeliveryError::Exhausted
    );
    let mut packet = delivery.sent(ns(0), 9000, 0, false).unwrap();
    delivery.begin_ack(Duration::MAX, false).unwrap();
    delivery.acknowledged(&mut packet).unwrap();
    let sample = delivery.finish_ack(Duration::MAX).unwrap().unwrap();
    assert_eq!(
        (sample.last_packet, sample.rate, sample.interval),
        (1, Some(0), Duration::MAX)
    );
    assert_eq!(
        delivery.application_limited(u64::MAX),
        Err(DeliveryError::Exhausted)
    );
    assert!(!delivery.is_app_limited());
    let mut packet = delivery.sent(Duration::MAX, 100, 0, false).unwrap();
    delivery.begin_ack(Duration::MAX, true).unwrap();
    delivery.acknowledged(&mut packet).unwrap();
    assert_eq!(delivery.finish_ack(ns(0)).unwrap().unwrap().rate, None);
    assert_eq!(delivery.delivered(), 9100);
}

#[test]
fn history_validation_and_partial_submission_feed_only_successful_transmissions() {
    use unquic::ack::History;
    use unquic::wire::{AckRange, AckRanges};
    let mut history = History::new().unwrap();
    let mut delivery = Delivery::new();
    for number in 1..=4 {
        history
            .reserve(number, 1280, number != 3, number != 3)
            .unwrap();
    }
    assert!(history.submit(1, ns(10)));
    assert!(history.submit(2, ns(10)));
    assert!(history.submit(3, ns(10))); // Feedback-only; never sampled.
    assert!(history.cancel(4)); // Unsent prefix tail; never sampled.
    let mut packets = [
        delivery.sent(ns(10), 1280, 0, false).unwrap(),
        delivery.sent(ns(10), 1280, 1280, false).unwrap(),
    ];
    let invalid = [AckRange { start: 1, end: 4 }];
    assert!(
        history
            .begin_ack(AckRanges::new(&invalid).unwrap())
            .is_err()
    );
    assert_eq!(delivery.delivered(), 0);
    assert!(history.lose(1).is_some());
    assert!(delivery.declared_lost(&mut packets[0]).unwrap().is_some());
    let ranges = [AckRange { start: 1, end: 3 }];
    for expected in [Some(2560), None] {
        history.begin_ack(AckRanges::new(&ranges).unwrap()).unwrap();
        delivery.begin_ack(ns(10010), false).unwrap();
        loop {
            let progress = history.advance_ack(1, |acked| {
                if acked.packet.charged {
                    assert!(
                        delivery
                            .acknowledged(&mut packets[acked.packet.number as usize - 1])
                            .unwrap()
                    );
                }
            });
            if let Some(progress) = progress {
                assert_eq!(
                    progress.released_bytes,
                    if expected.is_some() { 1280 } else { 0 }
                );
                break;
            }
        }
        assert_eq!(
            delivery
                .finish_ack(ns(0))
                .unwrap()
                .map(|sample| sample.newly_acked),
            expected
        );
    }
    assert_eq!((delivery.delivered(), delivery.lost()), (2560, 1280));
}

#[test]
fn delayed_send_reports_do_not_move_delivery_snapshots_or_rtt_into_the_future() {
    use unquic::ack::History;
    use unquic::wire::{AckRange, AckRanges};
    let mut histories = [History::new().unwrap(), History::new().unwrap()];
    let starts = [Duration::from_micros(99), Duration::from_micros(101)];
    let received = Duration::from_micros(300);
    let report_received = Duration::from_micros(500);
    let observed = Duration::from_micros(600);
    let mut delivery = Delivery::new();
    let mut packets = Vec::new();
    // Commit successful prefixes in send order after both reports arrive.
    for (lane, start) in starts.into_iter().enumerate() {
        assert!(start < received && received < report_received);
        histories[lane].reserve(1, 1280, true, true).unwrap();
        assert!(histories[lane].submit(1, start));
        packets.push(
            delivery
                .sent(start, 1280, lane as u64 * 1280, false)
                .unwrap(),
        );
    }
    // Each lane uses packet number one; sampling IDs belong to the shared owner.
    for lane in [1, 0] {
        let ranges = [AckRange { start: 1, end: 1 }];
        delivery.begin_ack(observed, true).unwrap();
        let progress = histories[lane]
            .acknowledge(AckRanges::new(&ranges).unwrap(), |_| {
                delivery.acknowledged(&mut packets[lane]).unwrap();
            })
            .unwrap();
        assert_eq!(
            received - progress.rtt_sent_at.unwrap(),
            Duration::from_micros(if lane == 0 { 201 } else { 199 })
        );
        let sample = delivery.finish_ack(Duration::ZERO).unwrap().unwrap();
        assert_eq!(sample.interval, Duration::from_micros(501));
        assert_eq!(
            sample.rate,
            Some(if lane == 1 { 2_554_890 } else { 5_109_780 })
        );
        assert!(sample.host_limited);
        assert!(!sample.app_limited);
    }
}

#[test]
fn loss_snapshots_preserve_physical_bytes_and_send_time_counters() {
    use unquic::bbr::LossSample;
    let mut d = Delivery::new();
    let mut a = d.sent(ns(0), 1280, 0, false).unwrap();
    d.application_limited(1280).unwrap();
    let mut b = d.sent(ns(1), 1500, 1280, false).unwrap();
    assert_eq!(
        d.declared_lost(&mut a).unwrap(),
        Some(LossSample {
            bytes: 1280,
            total_lost: 1280,
            lost: 1280,
            tx_in_flight: 1280,
            app_limited: false,
        })
    );
    let mut repair = d.sent(ns(2), 9000, 1500, false).unwrap();
    assert_eq!(
        d.declared_lost(&mut b).unwrap(),
        Some(LossSample {
            bytes: 1500,
            total_lost: 2780,
            lost: 2780,
            tx_in_flight: 2780,
            app_limited: true,
        })
    );
    assert_eq!(
        d.declared_lost(&mut repair).unwrap(),
        Some(LossSample {
            bytes: 9000,
            total_lost: 11780,
            lost: 10500,
            tx_in_flight: 10500,
            app_limited: true,
        })
    );
    assert_eq!(d.declared_lost(&mut a).unwrap(), None);
}

#[test]
fn controller_limits_feed_shared_grants_after_partial_submission_and_late_ack() {
    use unquic::bbr::Controller;
    use unquic::pacing::Grants;
    let mut c = Controller::new(1280, Some(Duration::from_millis(1)), ns(0), 1).unwrap();
    let mut d = Delivery::new();
    let mut g = Grants::new(2).unwrap();
    let grant = g.reserve(0, 2560, false, c.limits(), ns(0)).unwrap();
    // Only the first datagram is submitted; unsent bytes earn no sampler identity.
    g.reported(grant, 1280, Some(ns(0))).unwrap();
    let mut original = d.sent(ns(0), 1280, 0, false).unwrap();
    assert_eq!(g.in_flight(), 1280);
    c.enter_recovery(ns(1_000_000), g.in_flight(), false)
        .unwrap();
    c.on_loss(
        d.declared_lost(&mut original).unwrap().unwrap(),
        ns(1_000_000),
    )
    .unwrap();
    g.settled(1280).unwrap();
    let grant = g
        .reserve(1, 1280, false, c.limits(), ns(1_000_000))
        .unwrap();
    g.reported(grant, 1280, Some(ns(1_000_000))).unwrap();
    let mut repair = d.sent(ns(1_000_000), 1280, 0, false).unwrap();
    d.begin_ack(ns(2_000_000), false).unwrap();
    d.acknowledged(&mut original).unwrap();
    d.acknowledged(&mut repair).unwrap();
    let sample = d.finish_ack(ns(1_000_000)).unwrap().unwrap();
    assert_eq!(sample.newly_acked, 2560);
    g.settled(1280).unwrap(); // The lost original was settled once, at loss.
    c.recovery_finished(ns(2_000_000), false).unwrap();
    c.on_ack(
        sample,
        Some(ns(1_000_000)),
        g.in_flight(),
        false,
        ns(2_000_000),
    )
    .unwrap();
    assert_eq!(g.in_flight(), 0);
    assert_eq!(c.limits().window, 15360);
    assert_eq!(c.limits().rate, 35_456_000); // Startup pacing never decreases.
}

#[test]
fn retired_loss_metadata_is_reusable_only_after_the_ack_identity_is_gone() {
    use unquic::ack::History;
    use unquic::wire::{AckRange, AckRanges};
    let mut history = History::new().unwrap();
    let mut delivery = Delivery::new();
    // A single sampling slot models storage pressure independently of history depth.
    history.reserve(1, 1280, true, true).unwrap();
    assert!(history.submit(1, ns(0)));
    let mut slot = delivery.sent(ns(0), 1280, 0, false).unwrap();
    assert_eq!(history.retire_oldest_resolved(), Ok(None));
    assert!(history.lose(1).is_some());
    assert!(delivery.declared_lost(&mut slot).unwrap().is_some());
    assert_eq!(
        history.retire_oldest_resolved(),
        Ok(Some(unquic::ack::RetiredEntry {
            number: 1,
            sample_slot: None
        }))
    );
    history.reserve(2, 1280, true, true).unwrap();
    assert!(history.submit(2, ns(1)));
    slot = delivery.sent(ns(1), 1280, 0, false).unwrap();
    let ranges = [AckRange { start: 1, end: 2 }];
    delivery.begin_ack(ns(10000), false).unwrap();
    let progress = history
        .acknowledge(AckRanges::new(&ranges).unwrap(), |a| {
            assert_eq!(a.packet.number, 2);
            assert!(delivery.acknowledged(&mut slot).unwrap());
        })
        .unwrap();
    let sample = delivery.finish_ack(ns(0)).unwrap().unwrap();
    assert_eq!((progress.packets, progress.released_bytes), (1, 1280));
    assert_eq!(
        (sample.newly_acked, delivery.delivered(), delivery.lost()),
        (1280, 1280, 1280)
    );
}
