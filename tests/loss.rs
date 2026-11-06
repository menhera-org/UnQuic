use std::time::Duration;
use unquic::loss::Rtt;

fn us(value: u64) -> Duration {
    Duration::from_micros(value)
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
        assert_eq!(rtt.pto_period(backoff), us(2100 * (1_u64 << backoff)));
    }
    assert_eq!(rtt.frame_lifetime(), us(20_000));
    assert_eq!(rtt.pto_period(u32::MAX), Duration::MAX);
    assert_eq!(rtt.pto_period(127), Duration::MAX);
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
