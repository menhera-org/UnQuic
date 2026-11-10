use std::time::Duration;
use unquic::loss::Rtt;

fn us(value: u64) -> Duration {
    Duration::from_micros(value)
}

fn ms(value: u64) -> Duration {
    Duration::from_millis(value)
}

/// One 33 ms sample: a 100 ms PTO period that doubles to 1 s within four expiries.
fn rtt_100ms() -> Rtt {
    let mut rtt = Rtt::new();
    assert!(rtt.sample(Duration::ZERO, ms(33), 0));
    assert_eq!(rtt.pto_period(0), ms(100));
    rtt
}

#[test]
fn initial_estimates_and_first_sample_ignore_ack_delay() {
    let mut rtt = Rtt::new();
    assert_eq!(rtt.latest(), us(333_000));
    assert_eq!(rtt.minimum(), None);
    assert_eq!(rtt.smoothed(), us(333_000));
    assert_eq!(rtt.variance(), us(166_500));
    assert_eq!(rtt.pto_period(0), us(1_000_000));
    assert_eq!(rtt.frame_lifetime(), us(100_000));
    assert!(rtt.sample(us(10_000), us(17_500), 1000));
    assert_eq!(rtt.latest(), us(7500));
    assert_eq!(rtt.minimum(), Some(us(7500)));
    assert_eq!(rtt.smoothed(), us(7500));
    assert_eq!(rtt.variance(), us(3750));
    assert_eq!(rtt.pto_period(0), us(23_500));
    assert_eq!(rtt.frame_lifetime(), us(30_000));
}

#[test]
fn delay_is_capped_and_variation_uses_previous_smoothed_rtt() {
    let mut rtt = Rtt::default();
    assert!(rtt.sample(us(0), us(10_000), 0));
    assert!(rtt.sample(us(20_000), us(33_000), u32::MAX));
    assert_eq!(rtt.minimum(), Some(us(10_000)));
    assert_eq!(rtt.latest(), us(13_000));
    assert_eq!(rtt.smoothed(), us(10_250));
    assert_eq!(rtt.variance(), us(4250));
    assert_eq!(rtt.pto_period(0), us(28_250));
    assert_eq!(rtt.loss_delay(), us(14_625));

    // Subtracting 1 ms here would fall below the observed raw minimum.
    assert!(rtt.sample(us(40_000), us(50_500), 1000));
    assert_eq!(rtt.smoothed(), us(10_281) + Duration::from_nanos(250));
    assert_eq!(rtt.variance(), us(3250));
    assert_eq!(rtt.minimum(), Some(us(10_000)));

    // A new minimum resets neither the average nor its deviation.
    assert!(rtt.sample(us(60_000), us(64_500), 1000));
    assert_eq!(rtt.minimum(), Some(us(4500)));
    assert_eq!(rtt.latest(), us(4500));
    assert!(rtt.smoothed() > rtt.latest());
    assert_eq!(rtt.loss_delay(), rtt.smoothed() + rtt.smoothed() / 8);
}

#[test]
fn packet_and_time_thresholds_require_a_later_ack_in_the_same_lane() {
    let mut rtt = Rtt::new();
    rtt.sample(us(0), us(8000), 0);
    let sent = us(10_000);
    for largest in [0, 9, 10] {
        assert_eq!(rtt.loss_deadline(10, largest, sent), None);
    }
    assert_eq!(rtt.loss_deadline(0, 10, sent), None);
    assert_eq!(rtt.loss_deadline(10, 11, sent), Some(us(19_000)));
    assert_eq!(rtt.loss_deadline(10, 12, sent), Some(us(19_000)));
    assert_eq!(rtt.loss_deadline(10, 13, sent), Some(sent));
    assert_eq!(rtt.loss_deadline(10, u64::MAX, sent), Some(sent));
    assert_eq!(
        rtt.loss_deadline(u64::MAX - 2, u64::MAX, sent),
        Some(us(19_000))
    );
    assert_eq!(rtt.loss_deadline(u64::MAX, u64::MAX, sent), None);
    assert_eq!(rtt.loss_deadline(10, 11, Duration::MAX), None);
}

#[test]
fn short_rtt_keeps_timer_and_frame_floors_and_pto_backoff() {
    let mut rtt = Rtt::new();
    rtt.sample(us(0), us(100), 1000);
    assert_eq!(rtt.loss_delay(), us(1000));
    assert_eq!(rtt.pto_period(0), us(2100));
    for backoff in 0..32 {
        assert_eq!(
            rtt.pto_period(backoff),
            us(2100 * (1_u64 << backoff)).min(unquic::loss::MAX_BACKED_OFF_PTO)
        );
    }
    assert_eq!(rtt.pto_period(8), us(537_600));
    assert_eq!(rtt.pto_period(9), ms(1000));
    assert_eq!(rtt.frame_lifetime(), us(20_000));
    assert_eq!(rtt.pto_period(u32::MAX), ms(1000));
    assert_eq!(rtt.pto_period(127), ms(1000));
    // A longer unbacked period is never shortened by the cap.
    let initial = Rtt::new();
    assert_eq!(initial.pto_period(0), ms(1000));
    let mut slow = Rtt::new();
    assert!(slow.sample(Duration::ZERO, ms(2000), 0));
    assert_eq!(slow.pto_period(0), ms(6001));
    assert_eq!(slow.pto_period(5), ms(6001));
}

#[test]
fn invalid_time_does_not_mutate_and_extreme_durations_do_not_panic() {
    let mut rtt = Rtt::new();
    assert!(!rtt.sample(us(2), us(1), 0));
    assert_eq!(rtt.minimum(), None);
    assert_eq!(rtt.smoothed(), us(333_000));
    assert!(rtt.sample(Duration::ZERO, Duration::MAX, u32::MAX));
    assert_eq!(rtt.smoothed(), Duration::MAX);
    assert_eq!(rtt.loss_delay(), Duration::MAX);
    assert_eq!(rtt.pto_period(0), Duration::MAX);
    assert_eq!(rtt.frame_lifetime(), us(100_000));
    assert!(rtt.sample(Duration::ZERO, Duration::MAX, u32::MAX));
    assert_eq!(rtt.smoothed(), Duration::MAX);
    assert!(rtt.sample(Duration::ZERO, Duration::ZERO, u32::MAX));
    assert_eq!(rtt.minimum(), Some(Duration::ZERO));
    assert!(rtt.smoothed() < Duration::MAX);

    let mut zero = Rtt::new();
    assert!(zero.sample(Duration::ZERO, Duration::ZERO, u32::MAX));
    assert_eq!(zero.minimum(), Some(Duration::ZERO));
    assert_eq!(zero.smoothed(), Duration::ZERO);
    assert_eq!(zero.loss_delay(), us(1000));
    assert_eq!(zero.pto_period(0), us(2000));
    assert!(zero.sample(Duration::MAX, Duration::MAX, 0));
    assert_eq!(zero.smoothed(), Duration::ZERO);
}

#[test]
fn weighted_estimates_preserve_fractional_nanoseconds_without_early_rounding() {
    let mut rtt = Rtt::new();
    let ns = Duration::from_nanos;
    rtt.sample(ns(0), ns(7), 0);
    rtt.sample(ns(0), ns(15), 0);
    assert_eq!(rtt.smoothed(), ns(8));
    assert_eq!(rtt.variance(), ns(4));
}

#[test]
fn shared_pto_backs_off_caps_each_allowance_and_declares_loss_once() {
    use unquic::loss::{ProbeTimeout, ProbeTimeoutExpired};
    let rtt = rtt_100ms();
    let mut pto = ProbeTimeout::new();
    assert_eq!(pto.deadline(&rtt, None), None);
    assert_eq!(pto.expire(Duration::MAX, &rtt, None), None);
    let mut outstanding = Some(ms(0));
    // Periods of 100, 200, 400 and 800 ms, then the one-second cap.
    for (count, at) in [100, 300, 700, 1500, 2500].into_iter().enumerate() {
        assert_eq!(pto.deadline(&rtt, outstanding), Some(ms(at)));
        assert_eq!(pto.expire(ms(at) - us(1), &rtt, outstanding), None);
        assert_eq!(
            pto.expire(ms(at), &rtt, outstanding),
            Some(ProbeTimeoutExpired {
                probe_limit: 2,
                declare_lost: count == 2
            })
        );
        assert_eq!(pto.expire(ms(at), &rtt, outstanding), None);
        assert_eq!(pto.expire(ms(0), &rtt, outstanding), None);
        assert_eq!(pto.stalled(), count >= 2);
        if count == 2 {
            // Packet settlement may finish over several turns, without new sends.
            outstanding = None;
        }
    }
    assert!(pto.stalled());
    pto.acknowledged(false);
    assert_eq!(pto.deadline(&rtt, None), Some(ms(3500)));
    assert!(pto.stalled());
    pto.acknowledged(true);
    assert!(!pto.stalled());
    assert_eq!(pto.deadline(&rtt, None), None);
    assert_eq!(pto.deadline(&rtt, Some(ms(4000))), Some(ms(4100)));
}

#[test]
fn hot_lanes_do_not_hide_the_earliest_outstanding_lane_or_multiply_pto() {
    use unquic::loss::ProbeTimeout;
    let rtt = rtt_100ms();
    let mut pto = ProbeTimeout::new();
    let mut lanes = [Some(ms(0)), Some(ms(100)), None, Some(ms(200))];
    let earliest = |lanes: &[Option<Duration>]| lanes.iter().flatten().copied().min();
    assert_eq!(pto.deadline(&rtt, earliest(&lanes)), Some(ms(100)));
    lanes[3] = Some(ms(1000));
    assert_eq!(pto.deadline(&rtt, earliest(&lanes)), Some(ms(100)));
    let mut allowance = 0;
    for _ in 0..lanes.len() {
        allowance += pto
            .expire(ms(100), &rtt, earliest(&lanes))
            .map_or(0, |e| e.probe_limit);
    }
    assert_eq!(allowance, 2);
    lanes[0] = Some(ms(100));
    assert_eq!(pto.deadline(&rtt, earliest(&lanes)), Some(ms(300)));
    // Acknowledged/lost lanes stop contributing, but replacement does not reset the group.
    lanes[0] = None;
    lanes[1] = None;
    assert_eq!(pto.deadline(&rtt, earliest(&lanes)), Some(ms(1200)));
    lanes[2] = Some(ms(200));
    assert_eq!(pto.deadline(&rtt, earliest(&lanes)), Some(ms(400)));
}

#[test]
fn stalled_pto_recovery_uses_new_acks_even_when_their_charge_was_lost() {
    use unquic::{
        ack::History,
        loss::ProbeTimeout,
        wire::{AckRange, AckRanges},
    };
    let mut history = History::new().unwrap();
    history.reserve(1, 1500, true, true).unwrap();
    assert!(history.submit(1, Duration::ZERO));
    let rtt = rtt_100ms();
    let mut pto = ProbeTimeout::new();
    for at in [100, 300, 700] {
        let event = pto.expire(ms(at), &rtt, Some(Duration::ZERO)).unwrap();
        if event.declare_lost {
            assert_eq!(history.lose(1).unwrap().outer_bytes, 1500);
        }
    }
    assert!(pto.stalled());
    assert_eq!(history.inflight_bytes(), 0);
    // Feedback-only delivery cannot reopen ordinary sends.
    history.reserve(2, 100, false, false).unwrap();
    assert!(history.submit(2, ms(800)));
    let ranges = [AckRange { start: 2, end: 2 }];
    let ack = history
        .acknowledge(AckRanges::new(&ranges).unwrap(), |_| {})
        .unwrap();
    pto.acknowledged(ack.eliciting);
    assert!(pto.stalled());
    let ranges = [AckRange { start: 1, end: 2 }];
    let ack = history
        .acknowledge(AckRanges::new(&ranges).unwrap(), |_| {})
        .unwrap();
    assert_eq!(ack.packets, 1);
    assert_eq!(ack.released_bytes, 0);
    pto.acknowledged(ack.eliciting);
    assert!(!pto.stalled());
    assert_eq!(pto.deadline(&rtt, None), None);
    // A duplicate cannot reset an independently started later PTO sequence.
    pto.expire(ms(1000), &rtt, Some(ms(900))).unwrap();
    let ack = history
        .acknowledge(AckRanges::new(&ranges).unwrap(), |_| {})
        .unwrap();
    pto.acknowledged(ack.eliciting);
    assert_eq!(pto.deadline(&rtt, Some(ms(900))), Some(ms(1200)));
}

#[test]
fn pto_uses_current_rtt_and_does_not_catch_up_expiries_after_a_long_stall() {
    use unquic::loss::ProbeTimeout;
    let mut rtt = Rtt::new();
    let mut pto = ProbeTimeout::new();
    let sec = Duration::from_secs;
    // One late timer service produces one allowance, never an accumulated burst.
    let event = pto.expire(sec(100), &rtt, Some(Duration::ZERO)).unwrap();
    assert!(!event.declare_lost);
    assert_eq!(event.probe_limit, 2);
    // The initial period already equals the one-second cap, so it does not grow.
    assert_eq!(pto.deadline(&rtt, Some(Duration::ZERO)), Some(sec(101)));
    assert!(rtt.sample(sec(100), sec(100) + us(4500), 0));
    pto.acknowledged(true);
    assert_eq!(
        pto.deadline(&rtt, Some(sec(100))),
        Some(sec(100) + us(14_500))
    );
    assert_eq!(pto.expire(sec(100), &rtt, Some(sec(100))), None);
    assert!(
        pto.expire(sec(100) + us(14_500), &rtt, Some(sec(100)))
            .is_some()
    );
    // Successful probes shift the next deadline; failed ones leave the expiry floor.
    assert_eq!(
        pto.deadline(&rtt, Some(sec(100) + us(15_000))),
        Some(sec(100) + us(44_000))
    );
    assert_eq!(
        pto.deadline(&rtt, Some(sec(100))),
        Some(sec(100) + us(43_500))
    );
    assert_eq!(pto.deadline(&rtt, None), None);
}

#[test]
fn pto_deadline_overflow_is_disarmed_and_backoff_stays_capped() {
    use unquic::loss::{MAX_BACKED_OFF_PTO, ProbeTimeout};
    let rtt = rtt_100ms();
    let mut pto = ProbeTimeout::new();
    assert_eq!(pto.deadline(&rtt, Some(Duration::MAX)), None);
    assert_eq!(pto.expire(Duration::MAX, &rtt, Some(Duration::MAX)), None);
    let mut now = Duration::ZERO;
    // Far more expiries than a shift could hold: the period never wraps or grows past the cap.
    for count in 1..=200 {
        let deadline = pto.deadline(&rtt, Some(Duration::ZERO)).unwrap();
        assert!(deadline > now && deadline - now <= MAX_BACKED_OFF_PTO);
        let event = pto.expire(deadline, &rtt, Some(Duration::ZERO)).unwrap();
        assert_eq!(event.declare_lost, count == 3);
        now = deadline;
    }
    assert!(pto.stalled());
    assert_eq!(pto.deadline(&rtt, None), Some(now + MAX_BACKED_OFF_PTO));
    assert_eq!(pto.deadline(&rtt, Some(Duration::MAX - ms(500))), None);
}

#[test]
fn fresh_probe_replies_clear_silence_without_resetting_backoff() {
    use unquic::loss::ProbeTimeout;
    let rtt = rtt_100ms();
    let mut pto = ProbeTimeout::new();
    let last_sent = Some(Duration::ZERO);
    for at in [100, 300, 700, 1500] {
        let event = pto.expire(ms(at), &rtt, last_sent).unwrap();
        assert!(!event.declare_lost);
        let before = pto.deadline(&rtt, last_sent);
        pto.probe_replied();
        assert!(!pto.stalled());
        assert_eq!(pto.deadline(&rtt, last_sent), before);
    }
    for (count, at) in [2500, 3500, 4500].into_iter().enumerate() {
        let event = pto.expire(ms(at), &rtt, last_sent).unwrap();
        assert_eq!(event.declare_lost, count == 2);
    }
    assert!(pto.stalled());
    assert_eq!(pto.deadline(&rtt, None), Some(ms(5500)));
    pto.probe_replied();
    assert!(!pto.stalled());
    assert_eq!(pto.deadline(&rtt, last_sent), Some(ms(5500)));
    assert_eq!(pto.deadline(&rtt, None), None);
    // Ordinary traffic may resume, with its new sends retaining the existing backoff.
    assert_eq!(pto.deadline(&rtt, Some(ms(4600))), Some(ms(5600)));
    pto.acknowledged(true);
    assert_eq!(pto.deadline(&rtt, Some(ms(4600))), Some(ms(4700)));
}
