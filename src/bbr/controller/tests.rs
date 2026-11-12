use super::*;

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn controller() -> Controller {
    Controller::new(1500, Some(ms(10)), Duration::ZERO, 1).unwrap()
}

fn sample(c: &Controller, rate: u64, bytes: u64) -> RateSample {
    RateSample {
        rate: Some(rate),
        interval: ms(10),
        delivered: bytes,
        newly_acked: bytes,
        prior_delivered: c.delivered,
        lost: 0,
        tx_in_flight: bytes.max(1500),
        app_limited: false,
        host_limited: false,
        pmtu_only: false,
        last_packet: c.delivered + 1,
    }
}

fn ack(c: &mut Controller, at: Duration, flight: u64) -> bool {
    c.on_ack(sample(c, 1_500_000, 15000), Some(ms(10)), flight, false, at)
        .unwrap()
}

fn cruise() -> Controller {
    let mut c = controller();
    for round in 1..=4 {
        ack(&mut c, ms(round * 10), 30000);
    }
    ack(&mut c, ms(50), 0);
    assert_eq!(c.state(), State::Cruise);
    c
}

#[test]
fn pinned_draft_07_cycle_vector() {
    let mut c = controller();
    for (line, text) in include_str!("../../../tests/vectors/bbr-07/controller-cycle.txt")
        .lines()
        .enumerate()
    {
        let f: Vec<_> = text.split_whitespace().collect();
        if f.is_empty() || f[0].starts_with('#') {
            continue;
        }
        let n = |i: usize| f[i].parse::<u64>().unwrap();
        let mut s = sample(&c, n(1), n(2));
        s.app_limited = n(4) != 0;
        assert!(
            !c.on_ack(s, Some(ms(10)), n(3), n(5) != 0, ms(n(0)))
                .unwrap()
        );
        assert_eq!(format!("{:?}", c.state()), f[6], "line {}", line + 1);
        let limits = c.limits();
        assert_eq!(
            (limits.window, limits.rate, u64::from(limits.quantum)),
            (n(7), n(8), n(9)),
            "line {}",
            line + 1
        );
    }
}

#[test]
fn initial_pacing_quantum_and_confirmed_mtu() {
    for mss in [1280, 1500, 9000] {
        let mut c = Controller::new(mss, None, Duration::ZERO, 0).unwrap();
        assert_eq!(c.cwnd, 10 * u64::from(mss));
        assert_eq!(c.pacing_rate, 27700 * u64::from(mss));
        assert_eq!(c.quantum, (c.pacing_rate / 1000).min(65536) as u32);
        c.set_max_datagram(9000).unwrap();
        assert!(c.cwnd >= 36000 && c.quantum >= 18000);
        c.set_max_datagram(1280).unwrap();
        assert_eq!(c.limits().max_datagram, 1280);
    }
    let c = Controller::new(1280, Some(Duration::MAX), Duration::ZERO, 0).unwrap();
    assert_eq!((c.pacing_rate, c.quantum), (0, 2560));
    assert!(Controller::new(1279, None, Duration::ZERO, 0).is_err());
}

#[test]
fn startup_growth_boundary_and_application_limited_rounds() {
    let mut c = controller();
    ack(&mut c, ms(10), 30000);
    for round in 2..=10 {
        let mut s = sample(&c, 1000, 1500);
        s.app_limited = true;
        c.on_ack(s, Some(ms(10)), 30000, false, ms(round * 10))
            .unwrap();
    }
    assert_eq!(c.full_bw_count, 0);
    let s = sample(&c, 1_875_000, 18750); // Exactly 25% growth resets the detector.
    c.on_ack(s, Some(ms(10)), 30000, false, ms(110)).unwrap();
    assert_eq!((c.full_bw, c.full_bw_count), (1_875_000, 0));
    for round in 12..=14 {
        ack(&mut c, ms(round * 10), 30000);
    }
    assert_eq!(c.state(), State::Drain);
    assert_eq!(c.max_bw, 1_875_000);
}

#[test]
fn drain_timeout_uses_strict_pseudocode_boundary() {
    let mut c = controller();
    for round in 1..=7 {
        ack(&mut c, ms(round * 10), 100000);
        if round >= 4 {
            assert_eq!(c.state(), State::Drain);
        }
    }
    ack(&mut c, ms(80), 100000);
    assert_eq!(c.state(), State::Down);
}

#[test]
fn probe_wait_and_reno_rounds_use_packet_units() {
    let mut c = cruise();
    assert!((Duration::from_secs(2)..Duration::from_secs(3)).contains(&c.probe_wait));
    c.rounds_since_probe = 9; // BDP = ten 1500-byte packets.
    ack(&mut c, ms(60), 15000);
    assert_eq!(c.state(), State::Refill);
    ack(&mut c, ms(70), 15000);
    assert_eq!(c.state(), State::Up);
    assert!(c.check_probe_loss);
    let mut c = cruise();
    c.rounds_since_probe = 0;
    let deadline = c.cycle_stamp + c.probe_wait;
    ack(&mut c, deadline, 15000);
    assert_eq!(c.state(), State::Cruise);
    ack(&mut c, deadline + Duration::from_nanos(1), 15000);
    assert_eq!(c.state(), State::Refill);
}

#[test]
fn bandwidth_filter_ages_by_probe_cycles_and_ignores_low_app_samples() {
    let mut c = cruise();
    let mut s = sample(&c, 500_000, 1500);
    s.app_limited = true;
    c.update_congestion(&s);
    assert_eq!(c.max_bw, 1_500_000);
    s.rate = Some(2_000_000);
    c.update_congestion(&s);
    assert_eq!(c.max_bw, 2_000_000);
    s.app_limited = false;
    s.rate = Some(500_000);
    for expected in [2_000_000, 500_000] {
        c.probe_stopping = true;
        c.round_start = true;
        assert!(!c.adapt_longterm(&s, false));
        c.update_congestion(&s);
        assert_eq!(c.max_bw, expected);
    }
}

#[test]
fn aggregation_window_is_one_startup_round_then_ten_steady_rounds() {
    let mut c = controller();
    c.bw = 1_500_000;
    c.update_aggregation(15000);
    c.now = ms(10);
    c.extra_index = 1;
    c.update_aggregation(1500);
    assert_eq!(c.extra_acked, 1500);
    c.full_bw_reached = true;
    c.extra_filter[1] = 15000;
    for round in 2..=11 {
        c.now = ms(round * 10);
        c.extra_index = round as usize % 10;
        c.extra_filter[c.extra_index] = 0;
        c.update_aggregation(1500);
        assert_eq!(c.extra_acked, if round == 11 { 1500 } else { 15000 });
    }
}

#[test]
fn probe_rtt_requires_expiry_low_inflight_a_round_and_strict_duration() {
    let mut c = cruise();
    ack(&mut c, ms(10000), 15000);
    assert_ne!(c.state(), State::ProbeRtt); // RTT stamp begins at zero.
    assert!(ack(&mut c, ms(10001), 15000));
    assert_eq!(c.state(), State::ProbeRtt);
    assert_eq!(c.probe_rtt_done, None);
    assert_eq!(c.cwnd, 7500);
    assert!(ack(&mut c, ms(10002), 0));
    assert_eq!(c.probe_rtt_done, Some(ms(10202)));
    let mut old_round = sample(&c, 1_500_000, 1500);
    old_round.prior_delivered -= 15000;
    old_round.delivered += 15000;
    c.on_ack(old_round, Some(ms(10)), 0, false, ms(10202))
        .unwrap();
    assert!(!c.probe_rtt_round_done);
    assert!(ack(&mut c, ms(10202), 0)); // A round, but exactly 200 ms.
    assert_eq!(c.state(), State::ProbeRtt);
    assert!(ack(&mut c, ms(10203), 0));
    assert_eq!(c.state(), State::Cruise);
    assert_eq!(c.inflight_shortterm, INFINITY);
}

#[test]
fn idle_restart_skips_rtt_probe_and_can_finish_an_existing_probe() {
    let mut c = cruise();
    c.on_transmit(ms(11000), 0, true).unwrap();
    assert_eq!(c.pacing_rate, 1_485_000);
    assert!(!ack(&mut c, ms(11010), 0));
    assert_ne!(c.state(), State::ProbeRtt);
    assert_eq!(c.min_rtt_stamp, ms(11010));
    assert!(ack(&mut c, ms(22000), 0));
    assert_eq!(c.state(), State::ProbeRtt);
    c.on_transmit(ms(22201), 0, true).unwrap();
    assert_eq!(c.state(), State::Cruise);
}

fn loss(c: &mut Controller, bytes: u16, interval_lost: u64, flight: u64, app: bool) {
    c.on_loss(
        LossSample {
            bytes,
            total_lost: c.lost + u64::from(bytes),
            lost: interval_lost,
            tx_in_flight: flight,
            app_limited: app,
        },
        c.now,
    )
    .unwrap();
}

#[test]
fn startup_loss_requires_six_events_and_strict_two_percent_even_when_app_limited() {
    for (events, flight, exits) in [(5, 10000, false), (6, 30000, false), (6, 29999, true)] {
        let mut c = controller();
        ack(&mut c, ms(10), 30000);
        c.enter_recovery(ms(10), 30000, false).unwrap();
        for i in 1..=events {
            loss(&mut c, 100, i * 100, flight, true);
        }
        let mut s = sample(&c, 1_500_000, 15000);
        s.app_limited = true;
        s.lost = c.lost;
        s.tx_in_flight = flight;
        c.on_ack(s, Some(ms(10)), 30000, false, ms(20)).unwrap();
        assert_eq!(c.full_bw_reached, exits);
        if exits {
            assert_eq!(c.state(), State::Drain);
            assert_eq!(c.inflight_longterm, 15000);
            c.recovery_finished(ms(20), true).unwrap();
            assert_eq!(c.state(), State::Startup);
            assert_eq!(c.inflight_longterm, INFINITY);
        }
    }
}

#[test]
fn probe_loss_reacts_once_and_does_not_reduce_cap_for_app_limited_samples() {
    for app in [false, true] {
        let mut c = cruise();
        c.start_refill();
        c.start_up(1_500_000);
        c.check_probe_loss = true;
        c.enter_recovery(c.now, 15000, false).unwrap();
        loss(&mut c, 1500, 1500, 15000, app);
        assert_eq!(c.state(), State::Down);
        assert_eq!(c.inflight_longterm, if app { INFINITY } else { 13775 });
        assert!(!c.check_probe_loss);
        loss(&mut c, 1500, 3000, 30000, app);
        assert_eq!(c.inflight_longterm, if app { INFINITY } else { 13775 });
    }
}

#[test]
fn a_raised_loss_threshold_bounds_probing_only_while_random_loss_is_tolerated() {
    // 1500 of 15000 bytes is 10%: too high for draft 07's 2%, not above 10%.
    for (tolerate, reacts) in [(false, true), (true, false)] {
        let mut c = cruise();
        c.tolerate_random_loss(tolerate);
        c.set_loss_threshold(100);
        c.start_refill();
        c.start_up(1_500_000);
        c.check_probe_loss = true;
        c.enter_recovery(c.now, 15000, false).unwrap();
        loss(&mut c, 1500, 1500, 15000, false);
        assert_eq!(c.check_probe_loss, !reacts);
        assert_eq!(c.inflight_longterm, if reacts { 13775 } else { INFINITY });
    }
    // Above it, the bound is the inflight at which loss reached 10%:
    // 13500 + (13500 * 100 - 1000 * 100) / 900 = 14888.
    let mut c = cruise();
    c.tolerate_random_loss(true);
    c.set_loss_threshold(100);
    c.start_refill();
    c.start_up(1_500_000);
    c.check_probe_loss = true;
    c.enter_recovery(c.now, 15000, false).unwrap();
    loss(&mut c, 100, 100, 15000, false);
    assert!(c.check_probe_loss);
    loss(&mut c, 1500, 1600, 15000, false);
    assert_eq!(c.state(), State::Down);
    assert_eq!(c.inflight_longterm, 14888);
    // Settings outside 2%..=20% are clamped.
    c.set_loss_threshold(5);
    assert_eq!(c.loss_threshold, 20);
    c.set_loss_threshold(1000);
    assert_eq!(c.loss_threshold, 200);
}

#[test]
fn loss_outside_probing_reduces_shortterm_once_per_loss_round() {
    let mut c = cruise();
    c.bw_latest = 0;
    c.inflight_latest = 0;
    c.enter_recovery(c.now, 30000, false).unwrap();
    loss(&mut c, 1500, 1500, 30000, false);
    let s = sample(&c, 100_000, 1500);
    c.on_ack(s, Some(ms(10)), 15000, false, ms(60)).unwrap();
    assert_eq!((c.bw_shortterm, c.inflight_shortterm), (1_050_000, 31500));
    assert_eq!(c.pacing_rate, 1_039_500);
    let s = sample(&c, 100_000, 1500);
    c.on_ack(s, Some(ms(10)), 15000, false, ms(70)).unwrap();
    assert_eq!((c.bw_shortterm, c.inflight_shortterm), (1_050_000, 31500));
    c.recovery_finished(ms(70), true).unwrap();
    assert_eq!((c.bw_shortterm, c.inflight_shortterm), (INFINITY, INFINITY));
    assert_eq!(c.cwnd, 45000);
}

#[test]
fn tolerated_random_loss_cuts_shortterm_only_above_the_loss_threshold_or_with_a_queue() {
    // 600 of 30,000 bytes is exactly 2%, which is not too high. With a 10 ms
    // min_rtt, RTT samples above 11.25 ms show a queue, which still cuts. A
    // 10% threshold moves the boundary to 3,000 bytes only while tolerating.
    let rtt = Duration::from_micros;
    for (tolerate, threshold, lost, sampled, cut) in [
        (false, 20, 600, rtt(10_000), true),
        (true, 20, 600, rtt(10_000), false),
        (true, 20, 601, rtt(10_000), true),
        (true, 20, 600, rtt(11_250), false),
        (true, 20, 600, rtt(11_251), true),
        (true, 100, 3000, rtt(10_000), false),
        (true, 100, 3001, rtt(10_000), true),
        (true, 100, 1500, rtt(11_251), true),
        (false, 100, 601, rtt(10_000), true),
    ] {
        let mut c = cruise();
        c.tolerate_random_loss(tolerate);
        c.set_loss_threshold(threshold);
        c.bw_latest = 0;
        c.inflight_latest = 0;
        c.enter_recovery(c.now, 30000, false).unwrap();
        loss(&mut c, lost, u64::from(lost), 30000, false);
        let mut s = sample(&c, 100_000, 1500);
        s.lost = u64::from(lost);
        s.tx_in_flight = 30000;
        c.on_ack(s, Some(sampled), 15000, false, ms(60)).unwrap();
        let bounds = (c.bw_shortterm, c.inflight_shortterm);
        if cut {
            assert_eq!(bounds, (1_050_000, 31500));
        } else {
            assert_eq!(bounds, (INFINITY, INFINITY));
            assert_eq!(c.pacing_rate, 1_485_000);
        }
    }
}

#[test]
fn finite_probe_cap_grows_only_with_full_window_and_precaution_retries() {
    let mut c = cruise();
    c.inflight_longterm = c.cwnd;
    c.start_up(1_500_000);
    c.round_start = true;
    let s = sample(&c, 1_500_000, 45000);
    c.adapt_longterm(&s, false);
    assert_eq!(c.inflight_longterm, 45000);
    c.adapt_longterm(&s, true);
    assert_eq!(c.inflight_longterm, 46500);
    assert_eq!(c.probe_up_acked_per_inc, 22500);
    c.prev_probe_too_high = true;
    c.update_probe_bw(&s, 46500, true);
    assert_eq!(c.state(), State::Down);
    assert!(c.prev_probe_precautionary && !c.prev_probe_too_high);
    c.round_start = true;
    assert!(c.adapt_longterm(&s, true));
    assert_eq!(c.state(), State::Refill);
}

#[test]
fn timeout_recovery_restores_window_and_preserves_active_rtt_probe_on_undo() {
    let mut c = cruise();
    c.enter_recovery(c.now, 1500, true).unwrap();
    assert_eq!(c.cwnd, 3000); // RTO allows one additional maximum datagram.
    assert_eq!(
        c.enter_recovery(c.now, 0, true),
        Err(ControllerError::Invalid)
    );
    c.recovery_finished(c.now, false).unwrap();
    assert_eq!(c.cwnd, 45000);
    assert_eq!(
        c.recovery_finished(c.now, false),
        Err(ControllerError::Invalid)
    );
    c.enter_recovery(c.now, 1500, false).unwrap();
    c.undo_state = Some(State::Up);
    c.state = State::ProbeRtt;
    c.recovery_finished(c.now, true).unwrap();
    assert_eq!(c.state(), State::ProbeRtt);
}

#[test]
fn low_bdp_model_floors_and_saturating_volume_math() {
    let mut c = cruise();
    c.bw = 1;
    c.inflight_longterm = 1;
    c.inflight_shortterm = 1;
    c.cwnd = 90000;
    c.set_cwnd(1500);
    assert_eq!(c.cwnd, 6000);
    c.state = State::Up;
    // Three 3,000-byte send quanta exceed the 6,000-byte pipe floor; Up adds two datagrams.
    assert_eq!(c.quantized(0), 12000);
    assert_eq!(bytes_in(u64::MAX, Duration::MAX, true), u64::MAX);
    assert_eq!(bytes_in(1, Duration::from_nanos(1), true), 1);
    assert_eq!(bytes_in(1, Duration::from_nanos(1), false), 0);
    assert!(!too_high(u64::MAX / 50, u64::MAX, 20));
    assert!(too_high(u64::MAX / 50 + 1, u64::MAX, 20));
    c.probe_up_rounds = 30;
    for _ in 0..100 {
        c.raise_slope();
    }
    assert_eq!(c.probe_up_rounds, 30);
}

#[test]
fn windows_cover_the_receivers_ack_delay_on_sub_millisecond_paths() {
    let mut c = cruise();
    let bw = 1_250_000_000 / 8; // bytes per second at 1.25 Gbit/s
    c.bw = bw;
    // A 30 µs path: the window still covers one ACK delay of delivery.
    c.min_rtt = Some(Duration::from_micros(30));
    let floored = bytes_in(bw, WINDOW_RTT_FLOOR, true);
    assert_eq!(c.bdp_multiple(200), 2 * floored);
    assert_eq!(c.bdp, floored);
    assert_eq!(c.inflight_at(bw, 100), c.quantized(floored));
    // Paths longer than the floor size windows from their own RTT, as before.
    c.min_rtt = Some(ms(10));
    assert_eq!(c.bdp_multiple(100), bytes_in(bw, ms(10), true));
}

#[test]
fn invalid_events_leave_model_and_counters_unchanged() {
    let mut c = cruise();
    let before = c.limits();
    let mut s = sample(&c, 1, 100);
    s.delivered += 1;
    assert_eq!(
        c.on_ack(s, None, 0, false, c.now),
        Err(ControllerError::Invalid)
    );
    let s = sample(&c, 1, 100);
    assert_eq!(
        c.on_ack(s, None, 0, false, ms(0)),
        Err(ControllerError::Invalid)
    );
    assert_eq!(c.set_max_datagram(9001), Err(ControllerError::Invalid));
    assert_eq!(
        c.on_loss(
            LossSample {
                bytes: 100,
                total_lost: 101,
                lost: 100,
                tx_in_flight: 100,
                app_limited: false
            },
            c.now
        ),
        Err(ControllerError::Invalid)
    );
    assert_eq!((c.delivered, c.lost, c.now), (75000, 0, ms(50)));
    let after = c.limits();
    assert_eq!(
        (after.window, after.rate, after.quantum, after.max_datagram),
        (
            before.window,
            before.rate,
            before.quantum,
            before.max_datagram
        )
    );
    c.delivered = u64::MAX - 99;
    let s = sample(&c, 1, 100);
    assert_eq!(
        c.on_ack(s, None, 0, false, c.now),
        Err(ControllerError::Exhausted)
    );
    c.lost = u64::MAX - 99;
    assert_eq!(
        c.on_loss(
            LossSample {
                bytes: 100,
                total_lost: 0,
                lost: 100,
                tx_in_flight: 100,
                app_limited: false
            },
            c.now
        ),
        Err(ControllerError::Exhausted)
    );
}

#[test]
fn spurious_probe_loss_restarts_refill_without_retaining_reduced_bounds() {
    let mut c = cruise();
    c.start_refill();
    c.start_up(1_500_000);
    c.check_probe_loss = true;
    c.enter_recovery(c.now, 15000, false).unwrap();
    loss(&mut c, 1500, 1500, 15000, false);
    assert_eq!(c.inflight_longterm, 13775);
    c.recovery_finished(c.now, true).unwrap();
    assert_eq!(c.state(), State::Refill);
    assert_eq!(c.inflight_longterm, INFINITY);
    assert_eq!(c.cwnd, 45000);
}

#[test]
fn rtt_refresh_uses_previous_bdp_and_returns_to_startup_before_pipe_is_full() {
    let mut c = controller();
    ack(&mut c, ms(10), 0);
    let mut s = sample(&c, 1_500_000, 15000);
    s.app_limited = true;
    assert!(c.on_ack(s, Some(ms(100)), 0, false, ms(11000)).unwrap());
    assert_eq!(c.min_rtt, Some(ms(100)));
    assert_eq!(c.probe_rtt_cwnd, 7500); // New 100-ms RTT must not inflate the probe window.
    assert_eq!(c.cwnd, 7500);
    let mut s = sample(&c, 1_500_000, 15000);
    s.app_limited = true;
    c.on_ack(s, Some(ms(100)), 0, false, ms(11201)).unwrap();
    assert_eq!(c.state(), State::Startup);
}

#[test]
fn successive_loss_rounds_follow_a_tenfold_capacity_reduction() {
    let mut c = cruise();
    c.bw_latest = 150_000;
    c.inflight_latest = 1500;
    for _ in 0..10 {
        loss(&mut c, 1500, 1500, 15000, false);
        let s = sample(&c, 150_000, 1500);
        c.loss_round_start = true;
        c.update_congestion(&s);
    }
    assert_eq!(c.bw_shortterm, 150_000);
    assert_eq!(c.inflight_shortterm, 1500);
    c.bw = c.max_bw.min(c.bw_shortterm);
    c.set_pacing(100);
    c.set_cwnd(1500);
    assert_eq!(c.pacing_rate, 148_500);
    assert_eq!(c.cwnd, 6000); // Minimum pipelining window still applies.
    c.start_refill();
    assert_eq!((c.bw_shortterm, c.inflight_shortterm), (INFINITY, INFINITY));
}

fn limited_probe() -> Controller {
    let mut c = controller();
    c.state = State::Up;
    c.full_bw_reached = true;
    c.inflight_longterm = 30_000;
    c.cwnd = 30_000;
    c.probe_up_acked_per_inc = 1500;
    c
}

#[test]
fn first_full_flight_keeps_window_evidence_through_its_packet_round() {
    for fresh_on_boundary in [false, true] {
        let mut c = limited_probe();
        for (turn, prior, fresh, expected, boundary) in [
            (1, 0, true, 0, true),
            (2, 0, false, 1500, false),
            (3, 1500, fresh_on_boundary, 3000, true),
            (
                4,
                1500,
                false,
                if fresh_on_boundary { 4500 } else { 3000 },
                false,
            ),
        ] {
            let mut s = sample(&c, 1_500_000, 1500);
            s.prior_delivered = prior;
            s.delivered = c.delivered + s.newly_acked - prior;
            c.on_ack(s, Some(ms(10)), 28_500, fresh, ms(turn)).unwrap();
            assert_eq!(c.round_start, boundary);
            assert_eq!(
                c.probe_up_acked, expected,
                "ACK {turn}, fresh boundary={fresh_on_boundary}"
            );
        }
    }
}

#[test]
fn drained_transmission_preserves_round_evidence_without_idle_proof() {
    for app_limited in [false, true] {
        let mut c = limited_probe();
        let s = sample(&c, 1_500_000, 1500);
        c.on_ack(s, Some(ms(10)), 28_500, true, ms(1)).unwrap();
        assert!(c.cwnd_limited);
        c.on_transmit(ms(2), 0, app_limited).unwrap();
        assert_eq!(c.cwnd_limited, !app_limited);
        let s = sample(&c, 1_500_000, 1500);
        c.on_ack(s, Some(ms(10)), 1500, false, ms(3)).unwrap();
        assert!(c.round_start);
        assert_eq!(c.probe_up_acked, if app_limited { 0 } else { 1500 });
        assert!(!c.cwnd_limited);
    }
}

#[test]
fn sparse_pmtu_samples_do_not_end_startup_but_data_still_does() {
    let mut c = controller();
    for round in 1..=16 {
        let mut s = sample(&c, 150_000, 1500);
        s.pmtu_only = true;
        c.on_ack(s, Some(ms(10)), 30_000, false, ms(round * 100))
            .unwrap();
        assert_eq!(c.state(), State::Startup);
        assert_eq!(c.full_bw_count, 0);
        assert_eq!(c.delivered, round * 1500);
    }
    for round in 17..=20 {
        let s = sample(&c, 150_000, 1500);
        c.on_ack(s, Some(ms(10)), 30_000, false, ms(round * 100))
            .unwrap();
    }
    assert!(c.full_bw_reached);
    assert_eq!(c.state(), State::Drain);
    assert_eq!(c.delivered, 30_000);
}
