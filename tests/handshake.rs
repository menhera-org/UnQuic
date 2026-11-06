use std::{collections::VecDeque, time::Duration};
use unquic::{
    handshake::{Connection, Effects, HandshakeError, Params, Session, State, Tag},
    wire::Record,
};

const A: Params = Params {
    port: 40000,
    max_frame_len: 9022,
    mandatory_features: 0,
    optional_features: 0,
};
const B: Params = Params { port: 40001, ..A };
fn time(seconds: u64) -> Duration {
    Duration::from_secs(seconds)
}
fn tag(value: u128) -> Option<Tag> {
    Some(Tag::new(value).unwrap())
}
fn sessions() -> (Session, Session) {
    (
        Session::new(A, B.port, 1514).unwrap(),
        Session::new(B, A.port, 1514).unwrap(),
    )
}
fn receive(session: &mut Session, record: Record<'_>, now: u64, fresh: u128) -> Effects {
    session.on_record(record, time(now), tag(fresh)).unwrap()
}
fn same_pair(a: Connection, b: Connection) {
    assert_eq!((a.initiator, a.responder), (b.initiator, b.responder));
    assert_ne!(a.role, b.role);
    assert_eq!(a.local_tag(), b.remote_tag());
    assert_eq!(a.remote_tag(), b.local_tag());
    assert_eq!(a.local, b.peer);
    assert_eq!(a.peer, b.local);
}
fn establish(a: &mut Session, b: &mut Session, now: u64, seed: u128) -> [Record<'static>; 4] {
    let hello = a.reconnect(time(now), tag(seed)).unwrap().send.unwrap();
    let challenge = receive(b, hello, now, seed + 1).send.unwrap();
    let confirm = receive(a, challenge, now, seed + 2).send.unwrap();
    let reply = receive(b, confirm, now, seed + 3);
    assert!(reply.commit.is_some());
    let ready = reply.send.unwrap();
    assert!(receive(a, ready, now, seed + 4).commit.is_some());
    same_pair(a.connection().unwrap(), b.connection().unwrap());
    [hello, challenge, confirm, ready]
}
fn encoded(record: Record<'_>) -> Vec<u8> {
    let mut bytes = vec![0; 48];
    let length = record.encode(&mut bytes).unwrap();
    bytes.truncate(length);
    bytes
}

#[test]
fn losing_each_flight_and_repeating_messages_commits_once() {
    for lost in [3, 4, 8, 9] {
        let (a, b) = sessions();
        let mut peers = [a, b];
        let mut commits = [0; 2];
        let mut dropped = false;
        let mut queue = VecDeque::new();
        for now in 0..8 {
            for (endpoint, peer) in peers.iter_mut().enumerate() {
                let effects = peer
                    .on_timer(
                        time(now),
                        tag(1 + endpoint as u128 * 100 + now as u128 * 1000),
                    )
                    .unwrap();
                if let Some(record) = effects.send {
                    queue.push_back((endpoint, record));
                }
            }
            let mut iterations = 0;
            while let Some((source, record)) = queue.pop_front() {
                iterations += 1;
                assert!(iterations < 100, "unbounded handshake reply loop");
                if !dropped && encoded(record)[0] == lost {
                    dropped = true;
                    continue;
                }
                let destination = 1 - source;
                let fresh = 50 + destination as u128 * 100 + now as u128 * 1000;
                let effects = receive(&mut peers[destination], record, now, fresh);
                commits[destination] += usize::from(effects.commit.is_some());
                if let Some(reply) = effects.send {
                    queue.push_back((destination, reply));
                }
                let duplicate = receive(&mut peers[destination], record, now, fresh + 1);
                assert!(duplicate.commit.is_none());
                assert!(duplicate.send.is_none());
            }
            if peers.iter().all(|s| s.connection().is_some()) {
                break;
            }
        }
        assert!(dropped);
        assert_eq!(commits, [1, 1], "lost flight {lost}");
        same_pair(
            peers[0].connection().unwrap(),
            peers[1].connection().unwrap(),
        );
    }
}

fn permutations<T: Copy>(items: &mut [T], at: usize, check: &mut impl FnMut(&[T])) {
    if at == items.len() {
        check(items);
        return;
    }
    for j in at..items.len() {
        items.swap(at, j);
        permutations(items, at + 1, check);
        items.swap(at, j);
    }
}

#[test]
fn every_order_of_old_messages_preserves_new_connection() {
    let (mut a, mut b) = sessions();
    let old = establish(&mut a, &mut b, 0, 1);
    // Swap elected roles on replacement.
    establish(&mut b, &mut a, 1, 10);
    let current = [a.connection(), b.connection()];
    permutations(&mut [0, 1, 2, 3], 0, &mut |order| {
        let (mut a, mut b) = (a.clone(), b.clone());
        for &index in order {
            let target = if index % 2 == 0 { &mut b } else { &mut a };
            let effects = receive(target, old[index], 2, 100);
            assert!(effects.commit.is_none());
            assert_eq!([a.connection(), b.connection()], current);
        }
    });
}

#[test]
fn responder_restart_requires_a_fresh_exchange() {
    let (mut a, mut b) = sessions();
    let hello = a.on_timer(time(0), tag(1)).unwrap().send.unwrap();
    let challenge = receive(&mut b, hello, 0, 100).send.unwrap();
    let confirm = receive(&mut a, challenge, 0, 2).send.unwrap();
    b = sessions().1;
    assert!(receive(&mut b, confirm, 0, 200).commit.is_none());
    let changed = receive(&mut b, hello, 0, 200).send.unwrap();
    assert!(receive(&mut a, changed, 0, 2).send.is_none());
    let next = a.on_timer(time(10), tag(2)).unwrap().send.unwrap();
    let next_challenge = receive(&mut b, next, 10, 201).send.unwrap();
    let next_confirm = receive(&mut a, next_challenge, 10, 3).send.unwrap();
    let ready = receive(&mut b, next_confirm, 10, 202).send.unwrap();
    assert!(receive(&mut a, ready, 10, 3).commit.is_some());
    assert!(receive(&mut b, confirm, 11, 203).commit.is_none());
    same_pair(a.connection().unwrap(), b.connection().unwrap());
}

#[test]
fn replacement_preserves_active_state_until_each_confirmation() {
    let (mut a, mut b) = sessions();
    establish(&mut a, &mut b, 0, 1);
    let old = [a.connection(), b.connection()];
    let hello = a.reconnect(time(1), tag(10)).unwrap().send.unwrap();
    let challenge = receive(&mut b, hello, 1, 11).send.unwrap();
    assert!(
        receive(
            &mut b,
            Record::Hello {
                initiator: 12,
                params: A
            },
            1,
            13
        )
        .send
        .is_none()
    );
    assert_eq!([a.connection(), b.connection()], old);
    let confirm = receive(&mut a, challenge, 1, 12).send.unwrap();
    let ready = receive(&mut b, confirm, 1, 13);
    assert_eq!(ready.carrier, None);
    assert!(ready.commit.is_some());
    assert_eq!(a.connection(), old[0]);
    assert_ne!(b.connection(), old[1]);
    let repeated = receive(&mut b, confirm, 2, 14);
    assert!(repeated.commit.is_none());
    assert_eq!(
        encoded(repeated.send.unwrap()),
        encoded(ready.send.unwrap())
    );
    assert!(
        receive(&mut a, repeated.send.unwrap(), 2, 15)
            .commit
            .is_some()
    );
    assert!(
        receive(&mut a, repeated.send.unwrap(), 2, 15)
            .commit
            .is_none()
    );
    same_pair(a.connection().unwrap(), b.connection().unwrap());
}

#[test]
fn simultaneous_hello_elects_lower_tag_in_either_order() {
    for (a_tag, b_tag) in [(1, 2), (2, 1)] {
        for a_first in [true, false] {
            let (mut a, mut b) = sessions();
            let ha = a.on_timer(time(0), tag(a_tag)).unwrap().send.unwrap();
            let hb = b.on_timer(time(0), tag(b_tag)).unwrap().send.unwrap();
            let (ea, eb) = if a_first {
                let ea = receive(&mut a, hb, 0, 10);
                (ea, receive(&mut b, ha, 0, 11))
            } else {
                let eb = receive(&mut b, ha, 0, 11);
                (receive(&mut a, hb, 0, 10), eb)
            };
            let (initiator, responder, challenge) = if a_tag < b_tag {
                assert!(ea.send.is_none());
                (&mut a, &mut b, eb.send.unwrap())
            } else {
                assert!(eb.send.is_none());
                (&mut b, &mut a, ea.send.unwrap())
            };
            let confirm = receive(initiator, challenge, 0, 20).send.unwrap();
            let ready = receive(responder, confirm, 0, 21).send.unwrap();
            assert!(receive(initiator, ready, 0, 22).commit.is_some());
            same_pair(a.connection().unwrap(), b.connection().unwrap());
            assert_eq!(a.connection().unwrap().initiator.get(), 1);
            assert!(a.connection().unwrap().responder.get() >= 10);
        }
    }
}

#[test]
fn equal_tags_restart_and_frozen_attempts_ignore_other_hellos() {
    let (mut a, mut b) = sessions();
    let ha = a.on_timer(time(0), tag(1)).unwrap().send.unwrap();
    let hb = b.on_timer(time(0), tag(1)).unwrap().send.unwrap();
    let new_ha = receive(&mut a, hb, 0, 2).send.unwrap();
    let hb = receive(&mut b, ha, 0, 3).send.unwrap();
    assert!(receive(&mut a, hb, 0, 4).send.is_none());
    let challenge = receive(&mut b, new_ha, 0, 5).send.unwrap();
    // Once challenged, a smaller unrelated HELLO cannot change the role or pair.
    assert!(receive(&mut b, ha_for(1, A), 0, 6).send.is_none());
    let confirm = receive(&mut a, challenge, 0, 7).send.unwrap();
    assert!(receive(&mut a, ha_for(1, B), 0, 8).send.is_none());
    let ready = receive(&mut b, confirm, 0, 9).send.unwrap();
    assert!(receive(&mut a, ready, 0, 10).commit.is_some());
    same_pair(a.connection().unwrap(), b.connection().unwrap());
}
fn ha_for(initiator: u128, params: Params) -> Record<'static> {
    Record::Hello { initiator, params }
}

#[test]
fn pending_deadline_is_not_extended_by_duplicates_or_election() {
    let (mut a, mut b) = sessions();
    let hello = a.on_timer(time(0), tag(1)).unwrap().send.unwrap();
    for now in 0..10 {
        receive(&mut b, hello, now, 100);
    }
    let expired = b
        .on_record(
            Record::Confirm {
                initiator: 1,
                responder: 100,
            },
            time(10),
            None,
        )
        .unwrap();
    assert!(expired.commit.is_none());
    assert!(!b.is_pending());
    assert!(b.on_timer(time(10), tag(101)).unwrap().send.is_some());

    let (mut a, _) = sessions();
    a.on_timer(time(0), tag(20)).unwrap();
    let challenge = receive(&mut a, ha_for(1, B), 9, 30).send.unwrap();
    assert!(matches!(
        challenge,
        Record::Challenge {
            initiator: 1,
            responder: 30,
            ..
        }
    ));
    let expired = receive(
        &mut a,
        Record::Confirm {
            initiator: 1,
            responder: 30,
        },
        10,
        31,
    );
    assert!(expired.commit.is_none());
    assert!(!a.is_pending());
}

#[test]
fn parameter_mismatches_preserve_active_and_fixed_challenge_is_immutable() {
    let (mut a, mut b) = sessions();
    establish(&mut a, &mut b, 0, 1);
    let old = [a.connection(), b.connection()];
    for (params, error) in [
        (Params { port: 1, ..A }, HandshakeError::Port),
        (
            Params {
                max_frame_len: 1513,
                ..A
            },
            HandshakeError::Record,
        ),
        (
            Params {
                mandatory_features: 1,
                ..A
            },
            HandshakeError::Record,
        ),
    ] {
        assert_eq!(
            b.on_record(ha_for(10, params), time(1), tag(11))
                .unwrap_err(),
            error
        );
        assert_eq!([a.connection(), b.connection()], old);
    }
    let hello = a.reconnect(time(1), tag(10)).unwrap().send.unwrap();
    let challenge = receive(&mut b, hello, 1, 11).send.unwrap();
    let changed_hello = ha_for(
        10,
        Params {
            optional_features: 1,
            ..A
        },
    );
    assert_eq!(
        b.on_record(changed_hello, time(1), tag(12)).unwrap_err(),
        HandshakeError::ChangedHello
    );
    receive(&mut a, challenge, 1, 12);
    for changed in [
        Record::Challenge {
            initiator: 10,
            responder: 12,
            params: B,
        },
        Record::Challenge {
            initiator: 10,
            responder: 11,
            params: Params {
                optional_features: 1,
                ..B
            },
        },
    ] {
        assert!(receive(&mut a, changed, 2, 13).send.is_none());
    }
    assert_eq!([a.connection(), b.connection()], old);
    assert_eq!(
        encoded(receive(&mut a, challenge, 2, 14).send.unwrap()),
        encoded(Record::Confirm {
            initiator: 10,
            responder: 11
        })
    );
}

#[test]
fn negotiation_enforces_required_size_and_ignores_unknown_optional_bits() {
    let mut b = Session::new(B, A.port, 2422).unwrap();
    assert_eq!(
        b.on_record(
            ha_for(
                1,
                Params {
                    max_frame_len: 1514,
                    ..A
                }
            ),
            time(0),
            tag(2)
        )
        .unwrap_err(),
        HandshakeError::FrameCeiling
    );
    let peer = Params {
        max_frame_len: 3000,
        optional_features: u32::MAX,
        ..A
    };
    receive(&mut b, ha_for(1, peer), 0, 2);
    let effects = receive(
        &mut b,
        Record::Confirm {
            initiator: 1,
            responder: 2,
        },
        0,
        3,
    );
    let connection = effects.commit.unwrap();
    assert_eq!(connection.max_frame_len, 3000);
    assert_eq!(connection.peer, peer);
    assert!(
        Session::new(
            Params {
                optional_features: 1,
                ..A
            },
            B.port,
            1514
        )
        .is_err()
    );
    assert!(Session::new(A, 0, 1514).is_err());
    assert!(Session::new(A, B.port, 9023).is_err());
}

#[test]
fn retries_use_one_second_and_bucket_allows_four_then_one_per_quarter_second() {
    let (mut a, _) = sessions();
    assert!(a.on_timer(time(0), tag(1)).unwrap().send.is_some());
    assert!(
        a.on_timer(Duration::from_millis(999), None)
            .unwrap()
            .send
            .is_none()
    );
    assert!(a.on_timer(time(1), None).unwrap().send.is_some());
    let (mut a, _) = sessions();
    a.on_timer(time(0), tag(1)).unwrap();
    for current in 1..=4 {
        let effects = receive(&mut a, ha_for(current, B), 0, current + 1);
        assert_eq!(effects.send.is_some(), current < 4);
    }
    assert!(
        a.on_timer(Duration::from_millis(249), None)
            .unwrap()
            .send
            .is_none()
    );
    assert!(
        a.on_timer(Duration::from_millis(250), None)
            .unwrap()
            .send
            .is_some()
    );
    assert!(
        a.on_timer(Duration::from_millis(500), None)
            .unwrap()
            .send
            .is_none()
    );
    assert!(
        a.on_timer(Duration::from_millis(1250), None)
            .unwrap()
            .send
            .is_some()
    );
}

#[test]
fn entropy_absence_and_tag_reuse_never_commit_or_erase_active_state() {
    let (mut a, mut b) = sessions();
    assert_eq!(Tag::new(0), None);
    assert!(a.on_timer(time(0), None).unwrap().needs_tag);
    let hello = a.on_timer(time(0), tag(1)).unwrap().send.unwrap();
    assert!(b.on_record(hello, time(0), None).unwrap().needs_tag);
    assert!(!b.is_pending());
    assert!(receive(&mut b, hello, 0, 1).needs_tag);
    let challenge = receive(&mut b, hello, 0, 2).send.unwrap();
    let confirm = receive(&mut a, challenge, 0, 3).send.unwrap();
    let ready = receive(&mut b, confirm, 0, 4).send.unwrap();
    receive(&mut a, ready, 0, 5);
    let old = a.connection();
    assert!(a.reconnect(time(1), tag(1)).unwrap().needs_tag);
    assert!(a.reconnect(time(1), tag(2)).unwrap().needs_tag);
    assert_eq!(a.connection(), old);
    a.reconnect(time(1), tag(10)).unwrap();
    assert!(
        receive(
            &mut a,
            Record::Challenge {
                initiator: 10,
                responder: 2,
                params: B
            },
            1,
            11
        )
        .send
        .is_none()
    );
    assert!(
        receive(
            &mut a,
            Record::Ready {
                initiator: 10,
                responder: 2
            },
            1,
            11
        )
        .commit
        .is_none()
    );
}

#[test]
fn liveness_uses_fresh_feedback_and_stale_events_cannot_revive_a_retired_tag() {
    let (mut a, mut b) = sessions();
    establish(&mut a, &mut b, 0, 1);
    let old_tag = a.connection().unwrap().local_tag();
    assert_eq!(a.on_timer(time(3), None).unwrap().carrier, None);
    assert_eq!(a.state(), State::Degraded);
    a.on_feedback(tag(999).unwrap(), time(4)).unwrap();
    assert_eq!(a.state(), State::Degraded);
    a.on_feedback(old_tag, time(5)).unwrap();
    assert_eq!(a.state(), State::Active);
    a.on_feedback(old_tag, time(35)).unwrap();
    let effects = a.on_timer(time(35), None).unwrap();
    assert!(effects.retire);
    assert_eq!(effects.carrier, Some(false));
    assert_eq!(a.state(), State::Inactive);
    assert!(a.connection().is_none());
    a.on_feedback(old_tag, time(36)).unwrap();
    assert_eq!(a.state(), State::Inactive);
    assert_eq!(
        a.on_timer(time(34), None).unwrap_err(),
        HandshakeError::Time
    );
    assert_eq!(a.state(), State::Inactive);
}

#[test]
fn reflected_flights_and_non_handshake_records_do_not_install() {
    let (mut a, mut b) = sessions();
    let records = establish(&mut a, &mut b, 0, 1);
    for record in records {
        assert!(receive(&mut a, record, 1, 10).commit.is_none());
        assert!(receive(&mut b, record, 1, 10).commit.is_none());
    }
    assert_eq!(
        a.on_record(Record::Probe, time(1), None).unwrap_err(),
        HandshakeError::Record
    );
    assert_eq!(
        a.on_record(ha_for(0, B), time(1), None).unwrap_err(),
        HandshakeError::Record
    );
}

#[test]
fn either_endpoint_and_both_endpoints_can_restart() {
    for restarted in [1, 2, 3] {
        let (mut a, mut b) = sessions();
        let old = establish(&mut a, &mut b, 0, 1);
        if restarted & 1 != 0 {
            a = sessions().0;
        }
        if restarted & 2 != 0 {
            b = sessions().1;
        }
        let mut peers = [a, b];
        let mut queue = VecDeque::new();
        let mut commits = [0; 2];
        // Delayed confirmations cannot reconstruct lost state.
        assert!(receive(&mut peers[0], old[3], 1, 30).commit.is_none());
        assert!(receive(&mut peers[1], old[2], 1, 31).commit.is_none());
        for now in 1..8 {
            for (endpoint, peer) in peers.iter_mut().enumerate() {
                let effects = peer
                    .on_timer(time(now), tag(100 + 10 * now as u128 + endpoint as u128))
                    .unwrap();
                if let Some(record) = effects.send {
                    queue.push_back((endpoint, record));
                }
            }
            let mut count = 0;
            while let Some((source, record)) = queue.pop_front() {
                count += 1;
                assert!(count < 100);
                let target = 1 - source;
                let effects = receive(
                    &mut peers[target],
                    record,
                    now,
                    1000 + now as u128 * 10 + target as u128,
                );
                commits[target] += usize::from(effects.commit.is_some());
                if let Some(record) = effects.send {
                    queue.push_back((target, record));
                }
            }
            if commits == [1, 1] {
                break;
            }
        }
        assert_eq!(commits, [1, 1]);
        let a = peers[0].connection().unwrap();
        let b = peers[1].connection().unwrap();
        same_pair(a, b);
        assert!(a.initiator.get() >= 100 && a.responder.get() >= 100);
    }
}

#[test]
fn healthy_replacement_retries_after_expiry_and_duplicate_ready_is_not_feedback() {
    let (mut a, mut b) = sessions();
    let records = establish(&mut a, &mut b, 0, 1);
    a.reconnect(time(1), tag(10)).unwrap();
    a.on_feedback(a.connection().unwrap().local_tag(), time(10))
        .unwrap();
    let hello = a.on_timer(time(11), tag(20)).unwrap().send.unwrap();
    assert!(matches!(hello, Record::Hello { initiator: 20, .. }));
    assert_eq!(a.state(), State::Active);
    assert!(receive(&mut a, records[3], 12, 21).commit.is_none());
    a.on_timer(time(13), None).unwrap();
    assert_eq!(a.state(), State::Degraded);
}

#[test]
fn equal_tag_without_entropy_defers_and_does_not_reuse_the_collision() {
    let (mut a, _) = sessions();
    a.on_timer(time(0), tag(1)).unwrap();
    let effects = a.on_record(ha_for(1, B), time(0), None).unwrap();
    assert!(effects.needs_tag);
    assert!(!a.is_pending());
    assert!(a.on_timer(time(0), tag(1)).unwrap().needs_tag);
    assert!(matches!(
        a.on_timer(time(0), tag(2)).unwrap().send,
        Some(Record::Hello { initiator: 2, .. })
    ));
}

thread_local! {
    static ALLOCATION_COUNT: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

struct CountingAllocator;

// SAFETY: allocation and deallocation use the same System allocator unchanged.
unsafe impl std::alloc::GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        let _ = ALLOCATION_COUNT.try_with(|count| {
            if let Some(value) = count.get() {
                count.set(Some(value + 1));
            }
        });
        // SAFETY: forward the caller's valid layout to System.
        unsafe { std::alloc::System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: std::alloc::Layout) {
        // SAFETY: pointer and layout came from the same System allocator.
        unsafe { std::alloc::System.dealloc(pointer, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

#[test]
fn state_transitions_allocate_nothing() {
    let (mut a, mut b) = sessions();
    ALLOCATION_COUNT.set(Some(0));
    // Check that this thread's counter observes allocations.
    drop(std::hint::black_box(Box::new(1_u64)));
    let observed = ALLOCATION_COUNT.replace(Some(0));
    for round in 0..100 {
        let now = round * 2;
        establish(&mut a, &mut b, now, 10 + round as u128 * 10);
        a.on_feedback(a.connection().unwrap().local_tag(), time(now + 1))
            .unwrap();
        b.on_feedback(b.connection().unwrap().local_tag(), time(now + 1))
            .unwrap();
        a.on_timer(time(now + 1), None).unwrap();
        b.on_timer(time(now + 1), None).unwrap();
    }
    a.on_timer(time(240), tag(10000)).unwrap();
    let count = ALLOCATION_COUNT.replace(None);
    assert_eq!(observed, Some(1));
    assert_eq!(count, Some(0));
}

#[test]
fn sent_history_and_loss_timing_allocate_nothing() {
    use unquic::{
        ack::{History, SENT_CAPACITY},
        loss::Rtt,
        wire::{AckRange, AckRanges},
    };
    let mut history = History::new().unwrap();
    let mut rtt = Rtt::new();
    ALLOCATION_COUNT.set(Some(0));
    for number in 1..=SENT_CAPACITY as u64 + 128 {
        history.reserve(number, 1500, true, true).unwrap();
        assert!(history.submit(number, Duration::ZERO));
        if number % 3 == 0 {
            history.lose(number).unwrap();
        }
        let values = [AckRange {
            start: number,
            end: number,
        }];
        let ranges = AckRanges::new(&values).unwrap();
        history.begin_ack(ranges).unwrap();
        assert_eq!(history.advance_ack(1, |_| panic!("word scan only")), None);
        let result = history.advance_ack(2, |_| {}).unwrap();
        assert!(rtt.sample(result.rtt_sent_at.unwrap(), Duration::from_millis(5), 1000));
        std::hint::black_box((rtt.loss_delay(), rtt.pto_period(3), rtt.frame_lifetime()));
        assert_eq!(history.outstanding().count(), 0);
    }
    history.reset().unwrap();
    let count = ALLOCATION_COUNT.replace(None);
    assert_eq!(count, Some(0));
}

#[test]
fn cookie_lookups_allocate_nothing_in_both_storage_modes() {
    use unquic::{UnquicUuid, uuid::CookieIndex};
    for count in [4, 1024] {
        let keys: Vec<_> = (0..count)
            .map(|i: u128| UnquicUuid::from_bytes(i.to_be_bytes()))
            .collect();
        let index = CookieIndex::new(keys.iter().copied()).unwrap();
        ALLOCATION_COUNT.set(Some(0));
        for (expected, cookie) in keys.into_iter().enumerate() {
            assert_eq!(std::hint::black_box(index.find(cookie)), Some(expected));
            assert_eq!(
                index.find(UnquicUuid::from_bytes(u128::MAX.to_be_bytes())),
                None
            );
        }
        let allocations = ALLOCATION_COUNT.replace(None);
        assert_eq!(allocations, Some(0));
    }
}

#[test]
fn commit_barriers_allocate_nothing() {
    use unquic::handshake::barrier::{Barrier, Progress};
    let mut barrier = Barrier::new(64).unwrap();
    ALLOCATION_COUNT.set(Some(0));
    for _ in 0..100 {
        barrier.begin(None).unwrap();
        for phase in [Progress::Quiesced, Progress::Complete] {
            for worker in 0..64 {
                let command = barrier.pending(worker).unwrap();
                assert!(barrier.sent(worker, command));
                assert_eq!(
                    barrier.acknowledge(worker, command),
                    if worker == 63 {
                        phase
                    } else {
                        Progress::Waiting
                    }
                );
            }
        }
    }
    let allocations = ALLOCATION_COUNT.replace(None);
    assert_eq!(allocations, Some(0));
}

#[test]
fn reassembly_admission_expiry_delivery_and_reset_allocate_nothing() {
    use unquic::{
        UnquicUuid,
        reassembly::{Admission, Lane},
        wire::{Datagram, Header, encode_datagram},
    };
    let mut lane = Lane::new(1, 9022).unwrap();
    let mut packet = [0; 1232];
    let header = Header {
        cookie: UnquicUuid::from_bytes([1; 16]),
        receive_tag: 2,
        lane: 1,
        record_area_len: 0,
        packet_number: 1,
    };
    let record = Record::Data {
        frame_id: (1 << 48) | 1,
        total_len: 60,
        offset: 0,
        bytes: &[3; 60],
    };
    let len = encode_datagram(header, &[record], &mut packet).unwrap();
    let datagram = Datagram::decode(&packet[..len]).unwrap();
    let mut ranges = [unquic::wire::AckRange { start: 0, end: 0 }; unquic::wire::MAX_ACK_RANGES];
    let mut feedback = [0; 1232];
    ALLOCATION_COUNT.set(Some(0));
    for _ in 0..100 {
        assert_eq!(lane.admit(&datagram, Duration::ZERO), Admission::Fresh);
        assert_eq!(lane.front(Duration::ZERO).unwrap().bytes, &[3; 60]);
        assert!(lane.finish_front());
        assert_eq!(lane.ack_deadline(), Some(Duration::from_millis(1)));
        let ack = lane
            .take_ack(Duration::from_millis(1), &mut ranges)
            .unwrap();
        let length = encode_datagram(header, &[ack], &mut feedback).unwrap();
        assert!(Datagram::decode(&feedback[..length]).is_ok());
        lane.reset(1, 9022).unwrap();
        assert_eq!(lane.admit(&datagram, Duration::ZERO), Admission::Fresh);
        assert_eq!(lane.expire(Duration::from_millis(100)), 1);
        lane.retire();
        lane.reset(1, 9022).unwrap();
    }
    for sequence in 1..=64 {
        let record = Record::Data {
            frame_id: (1 << 48) | sequence,
            bytes: &[3; 30],
            total_len: 60,
            offset: 0,
        };
        let len = encode_datagram(
            Header {
                packet_number: sequence,
                ..header
            },
            &[record],
            &mut packet,
        )
        .unwrap();
        assert_eq!(
            lane.admit(&Datagram::decode(&packet[..len]).unwrap(), Duration::ZERO),
            Admission::Fresh
        );
    }
    let records = [
        Record::Data {
            frame_id: (1 << 48) | 1,
            total_len: 60,
            offset: 30,
            bytes: &[3; 30],
        },
        Record::Data {
            frame_id: (1 << 48) | 65,
            total_len: 60,
            offset: 0,
            bytes: &[3; 30],
        },
    ];
    let len = encode_datagram(
        Header {
            packet_number: 65,
            ..header
        },
        &records,
        &mut packet,
    )
    .unwrap();
    let datagram = Datagram::decode(&packet[..len]).unwrap();
    assert_eq!(lane.admit(&datagram, Duration::ZERO), Admission::Pressure);
    assert_eq!(lane.front(Duration::ZERO).unwrap().bytes, &[3; 60]);
    lane.finish_front();
    assert_eq!(lane.admit(&datagram, Duration::ZERO), Admission::Fresh);
    let allocations = ALLOCATION_COUNT.replace(None);
    assert_eq!(allocations, Some(0));
}

#[test]
fn segmentation_and_flow_selection_allocate_nothing() {
    use unquic::{UnquicUuid, pack};
    let (mut a, mut b) = sessions();
    establish(&mut a, &mut b, 0, 1);
    let mut lane =
        pack::Lane::new(UnquicUuid::from_bytes([1; 16]), a.connection().unwrap(), 1).unwrap();
    let mut frame = [0_u8; 9022];
    frame[12..14].copy_from_slice(&0x86dd_u16.to_be_bytes());
    frame[14] = 0x60;
    frame[18..20].copy_from_slice(&8_u16.to_be_bytes());
    frame[20] = 17;
    let mut output = [0; 1232];
    ALLOCATION_COUNT.set(Some(0));
    for _ in 0..100 {
        std::hint::black_box(pack::flow_hash(&frame));
        let mut pieces = lane.frame(&frame).unwrap();
        let mut count = 0;
        while pieces.encode_next(&mut output).unwrap().is_some() {
            count += 1;
        }
        assert_eq!(count, 8);
        let mut pieces = lane.frame(&frame).unwrap();
        let mut prefix = [0; unquic::wire::DATA_PREFIX_LEN];
        let mut count = 0;
        while pieces.encode_next_split(&mut prefix).unwrap().is_some() {
            count += 1;
        }
        assert_eq!(count, 8);
    }
    let frames = [&frame[..60]; pack::BATCH_CAPACITY];
    let mut inputs =
        std::array::from_fn::<_, { pack::BATCH_CAPACITY }, _>(|index| pack::FrameInput {
            id: lane.start_frame(frames[index].len()).unwrap(),
            header: &frames[index][..42],
            bytes: &frames[index][42..],
            offset: 0,
            end: frames[index].len(),
        });
    let mut batch = pack::Batch::new();
    for (start, end) in [(0, 60), (40, 44)] {
        for input in &mut inputs {
            input.offset = start;
            input.end = end;
        }
        batch
            .prepare(&mut lane, &inputs, unquic::BASE_UDP_PAYLOAD)
            .unwrap();
        assert!(batch.len() < 6);
        for packet in 0..batch.len() {
            let mut slices = [&[][..]; pack::PACKED_SLICES];
            assert_eq!(
                batch
                    .slices(packet, &inputs, &mut slices)
                    .unwrap()
                    .iter()
                    .map(|part| part.len())
                    .sum::<usize>(),
                batch.packet(packet).unwrap().length
            );
        }
    }
    let allocations = ALLOCATION_COUNT.replace(None);
    assert_eq!(allocations, Some(0));
}

#[test]
fn batched_socket_io_and_rejection_paths_allocate_nothing() {
    use unquic::socket::{ReceiveDatagram, ReceiveSocket, SendDatagram, TransmitSocket};
    // Select an ephemeral test port, then bind the public fixed-port API.
    let mut rx = (0..16)
        .find_map(|_| {
            let probe = std::net::UdpSocket::bind("[::1]:0").unwrap();
            let std::net::SocketAddr::V6(address) = probe.local_addr().unwrap() else {
                unreachable!()
            };
            drop(probe);
            match ReceiveSocket::bind(address, None) {
                Ok(rx) => Some(rx),
                Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => None,
                Err(error) => panic!("{error}"),
            }
        })
        .expect("available loopback test port");
    let tx = TransmitSocket::connect(&rx, rx.local_addr()).unwrap();
    let mut buffers = [[0; 8]; 4];
    let [a, b, c, d] = &mut buffers;
    let mut incoming = [
        ReceiveDatagram::new(a),
        ReceiveDatagram::new(b),
        ReceiveDatagram::new(c),
        ReceiveDatagram::new(d),
    ];
    let outgoing = [SendDatagram {
        parts: &[b"head", b"tail"],
    }; 4];
    ALLOCATION_COUNT.set(Some(0));
    for _ in 0..100 {
        assert_eq!(
            tx.send_batch(&outgoing, unquic::BASE_UDP_PAYLOAD).unwrap(),
            4
        );
        assert_eq!(rx.recv_batch(&mut incoming).unwrap(), 4);
        for packet in &incoming {
            assert_eq!(packet.length, 8);
            assert!(!packet.truncated);
            assert_eq!(packet.buffer, b"headtail");
        }
        assert_eq!(
            rx.recv_batch(&mut incoming).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        assert_eq!(
            tx.send_batch(&[], unquic::BASE_UDP_PAYLOAD)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::InvalidInput
        );
        tx.send(b"too long", unquic::BASE_UDP_PAYLOAD).unwrap();
        assert_eq!(
            rx.recv(&mut [0; 1]).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
    }
    rx.enable_gro().unwrap();
    let mut buffers = [[0; 64]; 4];
    let [a, b, c, d] = &mut buffers;
    let mut incoming = [
        ReceiveDatagram::new(a),
        ReceiveDatagram::new(b),
        ReceiveDatagram::new(c),
        ReceiveDatagram::new(d),
    ];
    for fallback in [false, true] {
        if fallback {
            use std::os::fd::{AsFd, AsRawFd};
            for (fd, option) in [
                (tx.as_fd(), libc::UDP_NO_CHECK6_TX),
                (rx.as_fd(), libc::UDP_NO_CHECK6_RX),
            ] {
                let enabled = 1_i32;
                // SAFETY: this readable integer changes only the isolated test socket.
                assert_eq!(
                    unsafe {
                        libc::setsockopt(
                            fd.as_raw_fd(),
                            libc::IPPROTO_UDP,
                            option,
                            (&enabled as *const i32).cast(),
                            std::mem::size_of::<i32>() as _,
                        )
                    },
                    0
                );
            }
        }
        for _ in 0..100 {
            assert_eq!(
                tx.send_batch(&outgoing, unquic::BASE_UDP_PAYLOAD).unwrap(),
                4
            );
            let count = rx.recv_batch(&mut incoming).unwrap();
            let mut logical = 0;
            for received in &incoming[..count] {
                for packet in received.segments().unwrap() {
                    assert_eq!(packet, b"headtail");
                    logical += 1;
                }
            }
            assert_eq!(logical, 4);
        }
    }
    assert!(!tx.gso_enabled());
    let allocations = ALLOCATION_COUNT.replace(None);
    assert_eq!(allocations, Some(0));
}

#[test]
fn deadlines_track_retries_pending_expiry_and_feedback_without_polling() {
    let (mut a, mut b) = sessions();
    assert_eq!(a.deadline(), Some(Duration::ZERO));
    let hello = a.on_timer(time(10), tag(1)).unwrap().send.unwrap();
    assert_eq!(a.deadline(), Some(time(11)));
    let challenge = receive(&mut b, hello, 10, 2).send.unwrap();
    assert_eq!(b.deadline(), Some(time(20)));
    let confirm = receive(&mut a, challenge, 10, 3).send.unwrap();
    assert_eq!(a.deadline(), Some(time(11)));
    let ready = receive(&mut b, confirm, 10, 4).send.unwrap();
    receive(&mut a, ready, 10, 5);
    assert_eq!(a.deadline(), Some(time(13)));
    a.on_feedback(a.connection().unwrap().local_tag(), time(12))
        .unwrap();
    assert_eq!(a.deadline(), Some(time(15)));
    a.on_timer(time(15), None).unwrap();
    assert_eq!(a.state(), State::Degraded);
    assert_eq!(a.deadline(), Some(time(15))); // Owner must retry entropy with backoff.
    a.on_timer(time(15), tag(7)).unwrap();
    assert_eq!(a.deadline(), Some(time(16)));
}

#[test]
fn cancelling_an_old_path_preserves_active_state_and_rejects_its_pending_flights() {
    let (mut waiting, _) = sessions();
    waiting.reconnect(time(1), tag(100)).unwrap();
    waiting.cancel_pending(time(2)).unwrap();
    assert_eq!(waiting.state(), State::Configured);
    assert!(!waiting.is_pending());
    assert_eq!(waiting.deadline(), Some(time(2)));
    let (mut a, mut b) = sessions();
    establish(&mut a, &mut b, 10, 1);
    let active = a.connection().unwrap();
    let hello = a.reconnect(time(11), tag(10)).unwrap().send.unwrap();
    let challenge = receive(&mut b, hello, 11, 11).send.unwrap();
    a.cancel_pending(time(12)).unwrap();
    assert_eq!(a.connection(), Some(active));
    assert!(!a.is_pending());
    assert_eq!(a.deadline(), Some(time(13)));
    assert!(
        a.on_record(challenge, time(12), tag(20))
            .unwrap()
            .commit
            .is_none()
    );
    assert!(!a.is_pending());
    let new_hello = a.reconnect(time(12), tag(20)).unwrap().send.unwrap();
    assert_ne!(encoded(hello), encoded(new_hello));
    assert_eq!(a.connection(), Some(active));
}

#[test]
fn repair_coverage_allocates_nothing() {
    use unquic::{MAX_FRAME_LEN, loss::FrameRecovery};
    let mut frame = FrameRecovery::new(MAX_FRAME_LEN).unwrap();
    ALLOCATION_COUNT.set(Some(0));
    for _ in 0..100 {
        assert!(frame.reset(MAX_FRAME_LEN));
        assert!(frame.submitted(0..MAX_FRAME_LEN, 0));
        for attempt in 0..=2 {
            assert!(frame.lost(0..MAX_FRAME_LEN, attempt));
            if attempt < 2 {
                while let Some((range, next)) = frame.next_repair(1232) {
                    assert_eq!(next, attempt + 1);
                    assert!(frame.submitted(range, next));
                }
            }
        }
        assert_eq!(frame.next_repair(MAX_FRAME_LEN), None);
        assert_eq!(frame.acknowledge(0..MAX_FRAME_LEN, 0), Some(MAX_FRAME_LEN));
    }
    assert_eq!(ALLOCATION_COUNT.replace(None), Some(0));
}

#[test]
fn piece_journal_reservation_feedback_and_expiry_allocate_nothing() {
    use unquic::ack::{FramePiece, PieceJournal};
    let mut journal = PieceJournal::new(4, 2, 128).unwrap();
    ALLOCATION_COUNT.set(Some(0));
    for number in 1..=10_000 {
        let id = (1 << 48) | number;
        assert!(journal.bind_frame(0, id, 60));
        assert!(journal.bind_frame(1, id + 1, 60));
        let pieces = [
            FramePiece {
                frame_slot: 0,
                frame_id: id,
                start: 0,
                end: 60,
                attempt: 0,
            },
            FramePiece {
                frame_slot: 1,
                frame_id: id + 1,
                start: 0,
                end: 60,
                attempt: 0,
            },
        ];
        assert!(journal.reserve_packet(0, &pieces));
        assert!(journal.reserve_packet(1, &pieces));
        assert!(!journal.reserve_packet(0, &pieces));
        assert_eq!(journal.packet(0).count(), 2);
        assert_eq!(journal.pop_frame(0, id), Some(pieces[0]));
        assert_eq!(journal.pop_packet(0), Some(pieces[0]));
        assert_eq!(journal.pop_packet(0), Some(pieces[1]));
        assert_eq!(journal.pop_packet(1), Some(pieces[1]));
        assert!(journal.frame_empty(0, id));
        assert!(journal.frame_empty(1, id + 1));
        assert_eq!(journal.available(), 128);
    }
    let count = ALLOCATION_COUNT.replace(None);
    assert_eq!(count, Some(0));
}

#[test]
fn bounded_outstanding_scans_allocate_nothing() {
    use unquic::ack::{History, SENT_CAPACITY};
    let mut history = History::new().unwrap();
    let mut number = 0;
    ALLOCATION_COUNT.set(Some(0));
    for _ in 0..100 {
        number += SENT_CAPACITY as u64;
        assert!(history.reserve(number, 1500, true, true).is_ok());
        assert!(history.submit(number, Duration::ZERO));
        let mut scan = history.outstanding_scan();
        let mut seen = 0;
        while !scan.is_complete() {
            let mut budget = 64;
            while let Some(packet) = scan.advance(&history, &mut budget) {
                assert_eq!(packet.number, number);
                assert!(history.lose(packet.number).is_some());
                seen += 1;
            }
        }
        assert_eq!(seen, 1);
    }
    let count = ALLOCATION_COUNT.replace(None);
    assert_eq!(count, Some(0));
}
