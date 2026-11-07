use std::time::Duration;
use unquic::{
    ack::History,
    pacing::{GrantError as Error, Grants, Limits},
    wire::{AckRange, AckRanges},
};

fn us(value: u64) -> Duration {
    Duration::from_micros(value)
}
fn limits() -> Limits {
    Limits {
        window: 12_800,
        rate: 128_000_000,
        quantum: 20_480,
        max_datagram: 1280,
    }
}

#[test]
fn expired_grants_hold_credit_until_one_partial_or_unused_report() {
    let mut grants = Grants::new(3).unwrap();
    let limits = Limits {
        window: 7680,
        ..limits()
    };
    let a = grants.reserve(0, 5120, false, limits, us(0)).unwrap();
    assert_eq!(a.bytes(), 5120);
    assert_eq!(a.expires(), us(100));
    assert!(!a.is_probe());
    assert!(a.valid_at(us(0)));
    assert!(a.valid_at(us(100) - Duration::from_nanos(1)));
    assert!(!a.valid_at(us(100)));
    assert_eq!(grants.deadline(us(0)), Some(us(40)));
    assert_eq!(
        grants.reserve(1, 2560, false, limits, us(39)),
        Err(Error::Blocked)
    );
    let b = grants.reserve(1, 2560, false, limits, us(40)).unwrap();
    assert_eq!((grants.reserved(), grants.in_flight()), (7680, 0));
    for worker in 0..3 {
        assert_eq!(
            grants.reserve(worker, 100, false, limits, us(1000)),
            Err(Error::Blocked)
        );
    }
    for (sent, started) in [
        (5121, Some(us(0))),
        (1, None),
        (0, Some(us(0))),
        (1, Some(us(100))),
    ] {
        assert_eq!(grants.reported(a, sent, started), Err(Error::Invalid));
        assert_eq!((grants.reserved(), grants.in_flight()), (7680, 0));
    }
    grants.reported(a, 2560, Some(us(10))).unwrap();
    assert_eq!((grants.reserved(), grants.in_flight()), (2560, 2560));
    let c = grants.reserve(0, 2560, false, limits, us(1000)).unwrap();
    assert_eq!(grants.reported(a, 0, None), Err(Error::Stale));
    grants.reported(b, 0, None).unwrap();
    grants.settled(2560).unwrap();
    assert_eq!(grants.settled(1), Err(Error::Invalid));
    assert_eq!((grants.reserved(), grants.in_flight()), (2560, 0));
    grants.reported(c, 0, None).unwrap();
    assert_eq!((grants.reserved(), grants.in_flight()), (0, 0));
}

#[test]
fn workers_share_quantum_and_sixty_four_datagram_cap_without_idle_catchup() {
    for quantum in [10_240, 65_536, u32::MAX] {
        let mut grants = Grants::new(3).unwrap();
        let limits = Limits {
            window: u64::MAX,
            quantum,
            ..limits()
        };
        let cap = quantum.min(64 * 1280);
        let a = grants.reserve(0, cap - 1280, false, limits, us(0)).unwrap();
        let at = grants.deadline(us(0)).unwrap();
        let b = grants.reserve(1, 1280, false, limits, at).unwrap();
        assert_eq!(grants.reserved(), u64::from(cap));
        assert_eq!(
            grants.reserve(2, 100, false, limits, us(10_000)),
            Err(Error::Blocked)
        );
        grants.reported(a, 0, None).unwrap();
        grants.reported(b, 0, None).unwrap();
        let c = grants.reserve(2, cap, false, limits, us(10_000)).unwrap();
        grants.reported(c, cap, Some(us(10_000))).unwrap();
        assert_eq!(
            grants.reserve(0, 100, false, limits, us(10_000)),
            Err(Error::Blocked)
        );
        assert_eq!(
            grants.deadline(us(10_000)),
            Some(us(10_000) + Duration::from_nanos(u64::from(cap) * 1_000_000_000 / limits.rate))
        );
    }
}

#[test]
fn probes_bypass_only_window_and_share_two_permissions_across_workers() {
    let mut grants = Grants::new(3).unwrap();
    let limits = Limits {
        window: 1280,
        ..limits()
    };
    let data = grants.reserve(0, 1280, false, limits, us(0)).unwrap();
    grants.reported(data, 1280, Some(us(0))).unwrap();
    grants.probe_allowance(2).unwrap();
    assert_eq!(
        grants.reserve(0, 1280, true, limits, us(9)),
        Err(Error::Blocked)
    );
    let a = grants.reserve(0, 1280, true, limits, us(10)).unwrap();
    let b = grants.reserve(1, 100, true, limits, us(20)).unwrap();
    assert!(a.is_probe());
    assert_eq!(
        grants.reserve(2, 100, true, limits, us(30)),
        Err(Error::Blocked)
    );
    assert_eq!(grants.reported(a, 100, Some(us(10))), Err(Error::Invalid));
    grants.reported(a, 1280, Some(us(10))).unwrap();
    grants.reported(b, 0, None).unwrap();
    let retry = grants.reserve(1, 100, true, limits, us(30)).unwrap();
    grants.reported(retry, 100, Some(us(30))).unwrap();
    assert_eq!(
        grants.reserve(2, 100, true, limits, us(40)),
        Err(Error::Blocked)
    );
    assert_eq!(
        grants.reserve(2, 100, false, limits, us(40)),
        Err(Error::Blocked)
    );
    assert_eq!(grants.in_flight(), 2660);
    grants.settled(2660).unwrap();
    assert!(grants.reserve(2, 100, false, limits, us(40)).is_ok());
}

#[test]
fn old_unused_probe_reports_cannot_refill_a_new_or_canceled_round() {
    for allowance in [0, 2] {
        let mut grants = Grants::new(3).unwrap();
        let limits = Limits {
            window: 0,
            ..limits()
        };
        grants.probe_allowance(2).unwrap();
        let old = grants.reserve(0, 1280, true, limits, us(0)).unwrap();
        grants.probe_allowance(allowance).unwrap();
        grants.reported(old, 0, None).unwrap();
        for worker in 0..usize::from(allowance) {
            let grant = grants
                .reserve(worker, 1280, true, limits, us(1000 + worker as u64 * 10))
                .unwrap();
            grants
                .reported(grant, 1280, Some(us(1000 + worker as u64 * 10)))
                .unwrap();
        }
        assert_eq!(
            grants.reserve(2, 1280, true, limits, us(2000)),
            Err(Error::Blocked)
        );
        assert_eq!(
            grants.reserve(2, 100, false, limits, us(2000)),
            Err(Error::Blocked)
        );
    }
}

#[test]
fn invalid_limits_zero_rate_and_time_overflow_preserve_credit() {
    assert!(Grants::new(0).is_err());
    assert!(Grants::new(65).is_err());
    assert_eq!(Grants::storage_bytes(0), None);
    assert_eq!(Grants::storage_bytes(usize::MAX), None);
    assert!(Grants::storage_bytes(64).unwrap() > Grants::storage_bytes(1).unwrap());
    let mut grants = Grants::new(1).unwrap();
    assert_eq!(grants.probe_allowance(3), Err(Error::Invalid));
    assert_eq!(
        grants.reserve(1, 100, false, limits(), us(0)),
        Err(Error::Invalid)
    );
    for bytes in [0, 99] {
        assert_eq!(
            grants.reserve(0, bytes, false, limits(), us(0)),
            Err(Error::Invalid)
        );
    }
    for max_datagram in [0, 1279, 9001, u16::MAX] {
        assert_eq!(
            grants.reserve(
                0,
                100,
                false,
                Limits {
                    max_datagram,
                    ..limits()
                },
                us(0)
            ),
            Err(Error::Invalid)
        );
    }
    grants.probe_allowance(2).unwrap();
    for probe in [false, true] {
        assert_eq!(
            grants.reserve(
                0,
                1280,
                probe,
                Limits {
                    rate: 0,
                    ..limits()
                },
                us(0)
            ),
            Err(Error::Blocked)
        );
        assert_eq!(
            grants.reserve(
                0,
                1280,
                probe,
                Limits {
                    quantum: 0,
                    ..limits()
                },
                us(0)
            ),
            Err(Error::Blocked)
        );
        assert_eq!(
            grants.reserve(0, 1280, probe, limits(), Duration::MAX),
            Err(Error::Exhausted)
        );
        assert_eq!(
            grants.reserve(
                0,
                1280,
                probe,
                Limits {
                    rate: 1,
                    ..limits()
                },
                Duration::MAX - us(100)
            ),
            Err(Error::Exhausted)
        );
    }
    assert_eq!((grants.reserved(), grants.in_flight()), (0, 0));
    assert_eq!(
        grants.reserve(0, 9001, true, limits(), us(0)),
        Err(Error::Invalid)
    );
    let grant = grants
        .reserve(
            0,
            1280,
            false,
            Limits {
                rate: u64::MAX,
                ..limits()
            },
            us(0),
        )
        .unwrap();
    assert_eq!(grants.deadline(us(0)), Some(Duration::from_nanos(1)));
    grants.reported(grant, 0, None).unwrap();
    assert_eq!(
        grants.reserve(0, 1280, false, limits(), us(0)),
        Err(Error::Blocked)
    );
    let grant = grants.reserve(0, 1280, false, limits(), us(1)).unwrap();
    assert!(!grant.valid_at(us(0)));
    assert_eq!(
        grants.reported(grant, 1280, Some(us(0))),
        Err(Error::Invalid)
    );
}

#[test]
fn partial_submission_late_completion_and_history_dedup_settle_exact_bytes() {
    let mut grants = Grants::new(1).unwrap();
    let mut history = History::new().unwrap();
    let grant = grants.reserve(0, 3840, false, limits(), us(0)).unwrap();
    for number in 1..=3 {
        history.reserve(number, 1280, true, true).unwrap();
    }
    // The call starts before expiry, but completes after a scheduling pause.
    let started = us(99);
    let completed = us(500);
    assert!(grant.valid_at(started));
    assert!(!grant.valid_at(completed));
    assert!(history.submit(1, started));
    assert!(history.submit(2, started));
    assert!(history.cancel(3));
    grants.reported(grant, 2560, Some(started)).unwrap();
    assert_eq!(grants.in_flight(), history.inflight_bytes());
    let lost = history.lose(2).unwrap();
    grants.settled(u64::from(lost.outer_bytes)).unwrap();
    assert!(history.lose(2).is_none());
    let ranges = [AckRange { start: 1, end: 2 }];
    for expected in [1280, 0] {
        let ack = history
            .acknowledge(AckRanges::new(&ranges).unwrap(), |_| {})
            .unwrap();
        assert_eq!(ack.released_bytes, expected);
        grants.settled(ack.released_bytes).unwrap();
        assert_eq!(grants.in_flight(), history.inflight_bytes());
    }
    assert_eq!((grants.reserved(), grants.in_flight()), (0, 0));
    assert_eq!(
        grants.reported(grant, 2560, Some(started)),
        Err(Error::Stale)
    );
}

#[test]
fn pmtu_probes_can_exceed_confirmed_size_without_inflating_burst_or_window() {
    let mut grants = Grants::new(2).unwrap();
    let limits = Limits {
        window: 0,
        quantum: 9000,
        ..limits()
    };
    grants.probe_allowance(2).unwrap();
    let probe = grants.reserve(0, 9000, true, limits, us(0)).unwrap();
    assert_eq!(
        grants.reserve(1, 1280, true, limits, us(100)),
        Err(Error::Blocked)
    );
    grants.reported(probe, 9000, Some(us(0))).unwrap();
    assert_eq!(grants.in_flight(), 9000);
    assert_eq!(
        grants.reserve(1, 100, false, limits, us(100)),
        Err(Error::Blocked)
    );
    let small_quantum = Limits {
        quantum: 2560,
        ..limits
    };
    assert_eq!(
        grants.reserve(1, 9000, true, small_quantum, us(100)),
        Err(Error::Blocked)
    );
    assert!(
        grants
            .reserve(1, 2560, true, small_quantum, us(100))
            .is_ok()
    );
}

#[test]
fn controller_reductions_preserve_existing_reservations_until_reports() {
    let mut grants = Grants::new(2).unwrap();
    let grant = grants.reserve(0, 2560, false, limits(), us(0)).unwrap();
    let reduced = Limits {
        window: 1280,
        quantum: 1280,
        rate: 1,
        ..limits()
    };
    assert_eq!(
        grants.reserve(1, 100, false, reduced, us(1000)),
        Err(Error::Blocked)
    );
    assert_eq!(grants.settled(2560), Err(Error::Invalid));
    grants.reported(grant, 2560, Some(us(0))).unwrap();
    assert_eq!(
        grants.reserve(1, 100, false, reduced, us(1000)),
        Err(Error::Blocked)
    );
    grants.settled(2560).unwrap();
    assert!(grants.reserve(1, 1280, false, reduced, us(1000)).is_ok());
}
