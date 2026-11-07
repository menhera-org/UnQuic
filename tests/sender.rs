use std::{num::NonZeroU16, time::Duration};
use unquic::{
    loss::{ProbeTimeout, Rtt},
    pacing::{Flight, GrantError as Error, Limits, PacketId, Planned, Sender},
};

fn us(n: u64) -> Duration {
    Duration::from_micros(n)
}
fn limits() -> Limits {
    Limits {
        window: 1_000_000,
        rate: 1_000_000_000,
        quantum: 144_000,
        max_datagram: 9000,
    }
}
fn packet(lane: u16, number: u64, bytes: u16) -> Planned {
    Planned {
        id: PacketId {
            session: 7,
            generation: 3,
            lane,
            number,
        },
        bytes,
    }
}
#[allow(clippy::vec_box)]
fn buffers(flights: Box<[Box<Flight>]>) -> Vec<Box<Flight>> {
    flights.into_vec()
}
fn commit(sender: &mut Sender, group: usize) -> Box<Flight> {
    sender.advance(group, 64).unwrap().unwrap()
}
fn acknowledge(
    sender: &mut Sender,
    group: usize,
    p: Planned,
    handle: NonZeroU16,
    at: u64,
) -> unquic::bbr::RateSample {
    sender.begin_ack(group, us(at), false).unwrap();
    sender.acknowledged(group, p.id, handle).unwrap();
    sender.finish_ack(group, Duration::ZERO).unwrap().unwrap()
}

#[test]
fn reversed_reports_commit_by_time_then_grant_then_ordinal_with_prefix_flight() {
    let (mut sender, flights) = Sender::new(1, 3, 4).unwrap();
    let mut flights = buffers(flights);
    let mut c = flights.pop().unwrap();
    let mut b = flights.pop().unwrap();
    let mut a = flights.pop().unwrap();
    let packets = [
        packet(0, 1, 200),
        packet(0, 2, 300),
        packet(1, 1, 100),
        packet(2, 1, 400),
    ];
    ALLOCATIONS.set(Some((0, 0, 0)));
    a.prepare(&packets[..2]).unwrap();
    b.prepare(&packets[2..3]).unwrap();
    c.prepare(&packets[3..]).unwrap();
    sender.reserve(0, &mut a, false, limits(), us(0)).unwrap();
    sender.reserve(0, &mut b, false, limits(), us(1)).unwrap();
    sender.reserve(0, &mut c, false, limits(), us(2)).unwrap();
    let handles = [
        a.handle(0).unwrap(),
        a.handle(1).unwrap(),
        b.handle(0).unwrap(),
        c.handle(0).unwrap(),
    ];
    a.complete(2, Some(us(90)), false).unwrap();
    b.complete(1, Some(us(10)), false).unwrap();
    c.complete(1, Some(us(90)), true).unwrap();
    sender.reported(c).unwrap();
    sender.reported(a).unwrap();
    assert_eq!(sender.flight_bytes(0), Some((100, 900)));
    assert!(sender.advance(0, 64).unwrap().is_none());
    assert_eq!(sender.begin_ack(0, us(200), false), Err(Error::Blocked));
    sender.reported(b).unwrap();
    assert!(sender.advance(0, 0).unwrap().is_none());
    assert!(!sender.feedback_ready(0));
    let b = sender.advance(0, 1).unwrap().unwrap();
    assert_eq!(b.worker(), 1);
    assert!(sender.advance(0, 1).unwrap().is_none());
    assert!(!sender.feedback_ready(0));
    let a = sender.advance(0, 1).unwrap().unwrap();
    assert_eq!(a.worker(), 0);
    let c = sender.advance(0, 1).unwrap().unwrap();
    assert_eq!(c.worker(), 2);
    assert!(sender.feedback_ready(0));
    assert!(a.grant().is_none() && b.grant().is_none() && c.grant().is_none());
    for (i, (index, flight)) in [(2, 100), (0, 300), (1, 600), (3, 1000)]
        .into_iter()
        .enumerate()
    {
        let sample = acknowledge(
            &mut sender,
            0,
            packets[index],
            handles[index],
            200 + i as u64,
        );
        assert_eq!(sample.last_packet, i as u64 + 1);
        assert_eq!(sample.tx_in_flight, flight);
        assert_eq!(sample.host_limited, index == 3);
        assert_eq!(sample.newly_acked, u64::from(packets[index].bytes));
    }
    assert_eq!(sender.available(), 4);
    assert_eq!(sender.flight_bytes(0), Some((0, 0)));
    assert_eq!(ALLOCATIONS.replace(None), Some((0, 0, 0)));
}

#[test]
fn partial_completion_returns_unused_slots_while_other_groups_keep_running() {
    let (mut sender, flights) = Sender::new(2, 2, 2).unwrap();
    let mut flights = buffers(flights);
    let mut b = flights.pop().unwrap();
    let mut a = flights.pop().unwrap();
    let first = packet(0, 1, 1280);
    let unsent = packet(0, 2, 1280);
    a.prepare(&[first, unsent]).unwrap();
    sender.reserve(0, &mut a, false, limits(), us(0)).unwrap();
    let head = a.handle(0).unwrap();
    let tail = a.handle(1).unwrap();
    b.prepare(&[packet(1, 1, 100)]).unwrap();
    assert_eq!(
        sender.reserve(1, &mut b, false, limits(), us(1000)),
        Err(Error::Blocked)
    );
    assert_eq!(sender.available(), 0);
    assert_eq!(a.complete(1, Some(us(100)), false), Err(Error::Invalid));
    a.complete(1, Some(us(99)), false).unwrap();
    sender.reported(a).unwrap();
    assert_eq!(sender.available(), 1);
    sender
        .reserve(1, &mut b, false, limits(), us(1000))
        .unwrap();
    assert_eq!(b.handle(0), Some(tail));
    b.complete(0, None, false).unwrap();
    sender.reported(b).unwrap();
    let _b = commit(&mut sender, 1);
    let _a = commit(&mut sender, 0);
    sender.begin_ack(0, us(2000), false).unwrap();
    assert_eq!(sender.acknowledged(0, unsent.id, tail), Err(Error::Stale));
    sender.acknowledged(0, first.id, head).unwrap();
    let sample = sender.finish_ack(0, Duration::ZERO).unwrap().unwrap();
    assert_eq!(sample.newly_acked, 1280);
    assert_eq!(sender.flight_bytes(0), Some((0, 0)));
    assert_eq!(sender.flight_bytes(1), Some((0, 0)));
    assert_eq!(sender.available(), 2);
}

#[test]
fn full_pool_keeps_pto_loss_retirement_and_late_ack_progress() {
    let (mut sender, flights) = Sender::new(1, 1, 2).unwrap();
    let mut a = buffers(flights).pop().unwrap();
    let first = packet(0, 1, 100);
    let second = packet(0, 2, 100);
    a.prepare(&[first, second]).unwrap();
    sender.reserve(0, &mut a, false, limits(), us(0)).unwrap();
    let h1 = a.handle(0).unwrap();
    let h2 = a.handle(1).unwrap();
    a.complete(2, Some(us(1)), false).unwrap();
    sender.reported(a).unwrap();
    let mut a = commit(&mut sender, 0);
    assert_eq!(sender.available(), 0);
    sender.reopen(0).unwrap();
    a.prepare(&[packet(0, 3, 100)]).unwrap();
    assert_eq!(
        sender.reserve(0, &mut a, false, limits(), us(100)),
        Err(Error::Blocked)
    );
    let mut pto = ProbeTimeout::new();
    let rtt = Rtt::new();
    for expiry in 0..3 {
        let at = pto.deadline(&rtt, Some(us(1))).unwrap();
        let action = pto.expire(at, &rtt, Some(us(1))).unwrap();
        sender.probe_allowance(0, action.probe_limit).unwrap();
        assert_eq!(action.declare_lost, expiry == 2);
        assert_eq!(
            sender.reserve(0, &mut a, true, limits(), at),
            Err(Error::Blocked)
        );
    }
    assert!(pto.stalled());
    sender.close(0).unwrap();
    assert!(sender.advance(0, 1).unwrap().is_none());
    assert!(sender.feedback_ready(0));
    assert_eq!(sender.retired(0, first.id, h1), Err(Error::Blocked));
    let loss = sender.lost(0, first.id, h1).unwrap().unwrap();
    assert_eq!((loss.bytes, loss.total_lost), (100, 100));
    assert!(sender.lost(0, first.id, h1).unwrap().is_none());
    sender.lost(0, second.id, h2).unwrap().unwrap();
    assert_eq!(sender.flight_bytes(0), Some((0, 0)));
    sender.retired(0, first.id, h1).unwrap();
    let sample = acknowledge(&mut sender, 0, second, h2, 10_000_000);
    assert_eq!(sample.newly_acked, 100);
    assert_eq!(sample.lost, 200);
    assert_eq!(sender.flight_bytes(0), Some((0, 0)));
    assert_eq!(sender.available(), 2);
    sender.reopen(0).unwrap();
    sender
        .reserve(
            0,
            &mut a,
            true,
            Limits {
                window: 0,
                ..limits()
            },
            us(10_000_001),
        )
        .unwrap();
    assert!(a.grant().unwrap().is_probe());
    a.complete(0, None, false).unwrap();
    sender.reported(a).unwrap();
    let _a = commit(&mut sender, 0);
}

#[test]
fn stale_handles_and_every_identity_component_are_rejected_after_reuse() {
    let (mut sender, flights) = Sender::new(2, 1, 1).unwrap();
    let mut a = buffers(flights).pop().unwrap();
    let old = packet(0, 1, 100);
    a.prepare(&[old]).unwrap();
    sender.reserve(0, &mut a, false, limits(), us(0)).unwrap();
    let handle = a.handle(0).unwrap();
    a.complete(1, Some(us(1)), false).unwrap();
    sender.reported(a).unwrap();
    let mut a = commit(&mut sender, 0);
    acknowledge(&mut sender, 0, old, handle, 2);
    let current = Planned {
        id: PacketId {
            generation: 4,
            ..old.id
        },
        ..old
    };
    a.prepare(&[current]).unwrap();
    sender.reserve(1, &mut a, false, limits(), us(3)).unwrap();
    assert_eq!(a.handle(0), Some(handle));
    a.complete(1, Some(us(4)), false).unwrap();
    sender.reported(a).unwrap();
    let _a = commit(&mut sender, 1);
    sender.begin_ack(0, us(10), false).unwrap();
    assert_eq!(
        sender.acknowledged(0, current.id, handle),
        Err(Error::Stale)
    );
    assert_eq!(sender.finish_ack(0, Duration::ZERO).unwrap(), None);
    sender.begin_ack(1, us(10), true).unwrap();
    for id in [
        old.id,
        PacketId {
            session: current.id.session + 1,
            ..current.id
        },
        PacketId {
            lane: current.id.lane + 1,
            ..current.id
        },
        PacketId {
            number: current.id.number + 1,
            ..current.id
        },
    ] {
        assert_eq!(sender.acknowledged(1, id, handle), Err(Error::Stale));
        assert_eq!(sender.flight_bytes(1), Some((0, 100)));
    }
    assert_eq!(sender.reopen(1), Err(Error::Blocked));
    assert_eq!(sender.lost(1, current.id, handle), Err(Error::Blocked));
    assert_eq!(sender.retired(1, current.id, handle), Err(Error::Blocked));
    sender.acknowledged(1, current.id, handle).unwrap();
    assert_eq!(
        sender.acknowledged(1, current.id, handle),
        Err(Error::Stale)
    );
    let sample = sender.finish_ack(1, Duration::ZERO).unwrap().unwrap();
    assert!(sample.host_limited);
    assert_eq!(sender.retired(1, current.id, handle), Err(Error::Blocked));
    assert_eq!(sender.available(), 1);
}

#[test]
fn first_unused_report_closes_issuance_without_recycling_its_credit() {
    let (mut sender, flights) = Sender::new(1, 3, 3).unwrap();
    let mut flights = buffers(flights);
    let mut c = flights.pop().unwrap();
    let mut b = flights.pop().unwrap();
    let mut a = flights.pop().unwrap();
    for (lane, flight) in [&mut a, &mut b, &mut c].into_iter().enumerate() {
        flight.prepare(&[packet(lane as u16, 1, 100)]).unwrap();
    }
    let limits = Limits {
        quantum: 200,
        ..limits()
    };
    sender.reserve(0, &mut a, false, limits, us(0)).unwrap();
    sender.reserve(0, &mut b, false, limits, us(1)).unwrap();
    assert_eq!(
        sender.reserve(0, &mut c, false, limits, us(2)),
        Err(Error::Blocked)
    );
    a.complete(0, None, false).unwrap();
    assert_eq!(a.complete(0, None, false), Err(Error::Stale));
    sender.reported(a).unwrap();
    assert_eq!(sender.flight_bytes(0), Some((100, 0)));
    assert_eq!(
        sender.reserve(0, &mut c, false, limits, us(200)),
        Err(Error::Blocked)
    );
    assert!(sender.advance(0, 64).unwrap().is_none());
    b.complete(0, None, false).unwrap();
    sender.reported(b).unwrap();
    let _a = commit(&mut sender, 0);
    let _b = commit(&mut sender, 0);
    assert_eq!(
        sender.reserve(0, &mut c, false, limits, us(201)),
        Err(Error::Blocked)
    );
    sender.reopen(0).unwrap();
    sender.reserve(0, &mut c, false, limits, us(201)).unwrap();
    c.complete(0, None, false).unwrap();
    sender.reported(c).unwrap();
    let _c = commit(&mut sender, 0);
}

#[test]
fn invalid_plans_completions_and_probe_batches_preserve_reservations() {
    let (mut sender, flights) = Sender::new(1, 1, 2).unwrap();
    let mut a = buffers(flights).pop().unwrap();
    let valid = packet(0, 1, 100);
    a.prepare(&[valid]).unwrap();
    for plan in [
        vec![],
        vec![packet(0, 0, 100)],
        vec![packet(0, 1, 99)],
        vec![packet(0, 1, 9001)],
        vec![valid, valid],
        vec![valid, packet(1, 2, 100)],
        vec![valid; 65],
    ] {
        assert_eq!(a.prepare(&plan), Err(Error::Invalid));
    }
    sender.reserve(0, &mut a, false, limits(), us(0)).unwrap();
    assert_eq!(a.prepare(&[valid]), Err(Error::Blocked));
    for (count, at) in [
        (2, Some(us(1))),
        (1, None),
        (0, Some(us(1))),
        (1, Some(us(100))),
    ] {
        assert_eq!(a.complete(count, at, false), Err(Error::Invalid));
        assert_eq!(sender.flight_bytes(0), Some((100, 0)));
    }
    let (error, mut a) = sender.reported(a).unwrap_err();
    assert_eq!(error, Error::Invalid);
    a.complete(0, None, false).unwrap();
    sender.reported(a).unwrap();
    let mut a = commit(&mut sender, 0);
    assert_eq!(sender.available(), 2);
    sender.reopen(0).unwrap();
    a.prepare(&[valid, packet(0, 2, 100)]).unwrap();
    sender.probe_allowance(0, 2).unwrap();
    assert_eq!(
        sender.reserve(0, &mut a, true, limits(), us(1000)),
        Err(Error::Invalid)
    );
    assert_eq!(sender.flight_bytes(0), Some((0, 0)));
}

#[test]
fn storage_is_bounded_and_rejects_invalid_dimensions() {
    for (groups, workers, slots) in [
        (0, 1, 1),
        (1, 0, 1),
        (1, 65, 1),
        (1, 1, 0),
        (1, 1, 65536),
        (usize::MAX, 1, 1),
    ] {
        assert!(Sender::storage_bytes(groups, workers, slots).is_none());
        assert!(Sender::new(groups, workers, slots).is_err());
    }
    let bytes = Sender::storage_bytes(1, 64, 65535).unwrap();
    assert!(bytes < 16 * 1024 * 1024, "{bytes}");
    println!("One group, 64 flights, 65,535 sampling slots: {bytes} reserved bytes");
    let (sender, flights) = Sender::new(1, 64, 65535).unwrap();
    assert_eq!(sender.available(), 65535);
    assert_eq!(flights.len(), 64);
}

#[test]
fn backward_time_is_rejected_before_grant_and_after_empty_late_completion() {
    let (mut sender, flights) = Sender::new(1, 1, 1).unwrap();
    let mut a = buffers(flights).pop().unwrap();
    let p = packet(0, 1, 100);
    a.prepare(&[p]).unwrap();
    sender.reserve(0, &mut a, false, limits(), us(1)).unwrap();
    let h = a.handle(0).unwrap();
    a.complete(1, Some(us(99)), false).unwrap();
    sender.reported(a).unwrap();
    let mut a = commit(&mut sender, 0);
    sender.reopen(0).unwrap();
    a.prepare(&[packet(0, 2, 100)]).unwrap();
    assert_eq!(
        sender.reserve(0, &mut a, false, limits(), us(98)),
        Err(Error::Invalid)
    );
    sender.close(0).unwrap();
    sender.advance(0, 1).unwrap();
    assert_eq!(sender.begin_ack(0, us(98), false), Err(Error::Invalid));
    acknowledge(&mut sender, 0, p, h, 200);
    sender.reopen(0).unwrap();
    assert_eq!(
        sender.reserve(0, &mut a, false, limits(), us(199)),
        Err(Error::Invalid)
    );
    sender.reserve(0, &mut a, false, limits(), us(300)).unwrap();
    a.complete(0, None, false).unwrap();
    sender.reported(a).unwrap();
    let _a = commit(&mut sender, 0);
    assert_eq!(sender.begin_ack(0, us(299), false), Err(Error::Invalid));
    sender.begin_ack(0, us(300), false).unwrap();
    assert_eq!(sender.finish_ack(0, Duration::ZERO).unwrap(), None);
}

thread_local! {
    static ALLOCATIONS: std::cell::Cell<Option<(usize, usize, usize)>> = const { std::cell::Cell::new(None) };
}
struct CountingAllocator;
// SAFETY: all operations forward the same allocation/layout to System.
unsafe impl std::alloc::GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|count| {
            if let Some((alloc, dealloc, bytes)) = count.get() {
                count.set(Some((alloc + 1, dealloc, bytes + layout.size())));
            }
        });
        // SAFETY: forward the caller's valid layout.
        unsafe { std::alloc::System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
        let _ = ALLOCATIONS.try_with(|count| {
            if let Some((alloc, dealloc, bytes)) = count.get() {
                count.set(Some((alloc, dealloc + 1, bytes)));
            }
        });
        // SAFETY: pointer and layout were allocated by System.
        unsafe { std::alloc::System.dealloc(ptr, layout) }
    }
}
#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

#[test]
fn reserved_memory_and_history_integration_allocate_nothing_during_reuse() {
    use unquic::{
        ack::History,
        wire::{AckRange, AckRanges},
    };
    ALLOCATIONS.set(Some((0, 0, 0)));
    let (mut sender, flights) = Sender::new(2, 1, 2).unwrap();
    let allocated = ALLOCATIONS.replace(None).unwrap();
    assert!(allocated.0 > 0);
    assert_eq!(allocated.1, 0);
    assert_eq!(
        allocated.2 + std::mem::size_of::<Sender>(),
        Sender::storage_bytes(2, 1, 2).unwrap()
    );
    let mut a = buffers(flights).pop().unwrap();
    let mut history = History::new().unwrap();
    ALLOCATIONS.set(Some((0, 0, 0)));
    for number in 1..=66_000 {
        let p = packet(0, number, 100);
        let at = number * 1000;
        history.reserve(number, 100, true, true).unwrap();
        a.prepare(&[p]).unwrap();
        sender.reserve(0, &mut a, false, limits(), us(at)).unwrap();
        let handle = a.handle(0).unwrap();
        assert!(history.submit_sampled(number, us(at + 1), handle));
        a.complete(1, Some(us(at + 1)), false).unwrap();
        sender.reported(a).unwrap();
        a = commit(&mut sender, 0);
        if number % 2 == 0 {
            let lost = history.lose(number).unwrap();
            assert_eq!(lost.sample_slot, Some(handle));
            assert!(sender.lost(0, p.id, handle).unwrap().is_some());
        }
        let ranges = [AckRange {
            start: number,
            end: number,
        }];
        history.begin_ack(AckRanges::new(&ranges).unwrap()).unwrap();
        sender.begin_ack(0, us(at + 2), false).unwrap();
        loop {
            let progress = history.advance_ack(1, |acked| {
                sender
                    .acknowledged(
                        0,
                        PacketId {
                            number: acked.packet.number,
                            ..p.id
                        },
                        acked.packet.sample_slot.unwrap(),
                    )
                    .unwrap();
            });
            if progress.is_some() {
                break;
            }
        }
        assert_eq!(
            sender
                .finish_ack(0, Duration::ZERO)
                .unwrap()
                .unwrap()
                .newly_acked,
            100
        );
        assert!(
            history
                .retire_oldest_resolved()
                .unwrap()
                .unwrap()
                .sample_slot
                .is_none()
        );
        assert_eq!(sender.available(), 2);
        assert_eq!(sender.flight_bytes(0), Some((0, 0)));
        sender.reopen(0).unwrap();
    }
    assert_eq!(ALLOCATIONS.replace(None), Some((0, 0, 0)));
}

#[test]
fn all_workers_complete_equal_timestamp_runs_in_grant_order_without_allocations() {
    let (mut sender, flights) = Sender::new(1, 64, 64).unwrap();
    let mut flights = buffers(flights);
    let mut handles = [None; 64];
    ALLOCATIONS.set(Some((0, 0, 0)));
    for (worker, flight) in flights.iter_mut().enumerate() {
        flight.prepare(&[packet(worker as u16, 1, 100)]).unwrap();
        sender
            .reserve(0, flight, false, limits(), us(worker as u64))
            .unwrap();
        handles[worker] = flight.handle(0);
        flight.complete(1, Some(us(90)), false).unwrap();
    }
    while let Some(flight) = flights.pop() {
        sender.reported(flight).unwrap();
    }
    assert_eq!(sender.flight_bytes(0), Some((0, 6400)));
    for worker in 0..64 {
        let flight = sender.advance(0, 1).unwrap().unwrap();
        assert_eq!(flight.worker(), worker);
        flights.push(flight);
        assert_eq!(sender.feedback_ready(0), worker == 63);
    }
    sender.begin_ack(0, us(1000), false).unwrap();
    for (worker, handle) in handles.into_iter().enumerate() {
        sender
            .acknowledged(0, packet(worker as u16, 1, 100).id, handle.unwrap())
            .unwrap();
    }
    let sample = sender.finish_ack(0, Duration::ZERO).unwrap().unwrap();
    assert_eq!(
        (sample.last_packet, sample.newly_acked, sample.tx_in_flight),
        (64, 6400, 6400)
    );
    assert_eq!(sender.available(), 64);
    assert_eq!(ALLOCATIONS.replace(None), Some((0, 0, 0)));
}

#[test]
fn request_prefix_trimming_is_atomic_whole_packet_and_allocation_free() {
    let (_sender, flights) = Sender::new(1, 1, 3).unwrap();
    let mut flight = buffers(flights).pop().unwrap();
    let plan = [packet(7, 10, 100), packet(7, 12, 200), packet(7, 13, 300)];
    ALLOCATIONS.set(Some((0, 0, 0)));
    assert_eq!(flight.truncate(1000), Err(Error::Invalid));
    assert_eq!(flight.packets().len(), 0);
    flight.prepare(&plan).unwrap();
    assert_eq!(flight.completion(), None);
    for budget in [0, 99] {
        assert_eq!(flight.truncate(budget), Err(Error::Blocked));
        assert_eq!(flight.packets().len(), 3);
    }
    assert_eq!(flight.truncate(u32::MAX), Ok(3));
    for (actual, expected) in flight.packets().zip(plan) {
        assert_eq!((actual.id, actual.bytes), (expected.id, expected.bytes));
    }
    assert_eq!(flight.truncate(299), Ok(1));
    assert_eq!(flight.truncate(u32::MAX), Ok(1)); // Trimming cannot restore a suffix.
    flight.prepare(&plan).unwrap();
    assert_eq!(flight.truncate(300), Ok(2));
    assert_eq!(
        flight.packets().map(|p| u32::from(p.bytes)).sum::<u32>(),
        300
    );
    assert_eq!(flight.truncate(99), Err(Error::Blocked));
    assert_eq!(flight.packets().len(), 2);
    assert_eq!(ALLOCATIONS.replace(None), Some((0, 0, 0)));
}

#[test]
fn granted_requests_cannot_trim_and_completion_survives_return_until_reprepared() {
    let (mut sender, flights) = Sender::new(1, 1, 3).unwrap();
    let mut flight = buffers(flights).pop().unwrap();
    let plan = [packet(4, 5, 100), packet(4, 6, 200), packet(4, 7, 300)];
    flight.prepare(&plan).unwrap();
    flight.truncate(300).unwrap();
    sender
        .reserve(0, &mut flight, false, limits(), us(0))
        .unwrap();
    let handle = flight.handle(0).unwrap();
    ALLOCATIONS.set(Some((0, 0, 0)));
    assert_eq!(flight.truncate(100), Err(Error::Blocked));
    assert_eq!(flight.packets().len(), 2);
    assert_eq!(flight.completion(), None);
    flight.complete(1, Some(us(1)), true).unwrap();
    assert_eq!(flight.completion(), Some((1, Some(us(1)), true)));
    assert_eq!(flight.packets().len(), 2);
    sender.reported(flight).unwrap();
    let mut flight = commit(&mut sender, 0);
    assert_eq!(flight.completion(), Some((1, Some(us(1)), true)));
    assert_eq!(flight.packets().len(), 1);
    let actual = flight.packets().next().unwrap();
    assert_eq!((actual.id, actual.bytes), (plan[0].id, plan[0].bytes));
    assert_eq!(flight.truncate(100), Err(Error::Invalid));
    assert_eq!(sender.available(), 2);
    flight.prepare(&plan[2..]).unwrap();
    assert_eq!(flight.completion(), None);
    assert_eq!(flight.packets().next().unwrap().id, plan[2].id);
    acknowledge(&mut sender, 0, plan[0], handle, 2);
    sender.reopen(0).unwrap();
    sender
        .reserve(0, &mut flight, false, limits(), us(3))
        .unwrap();
    flight.complete(0, None, false).unwrap();
    sender.reported(flight).unwrap();
    let flight = commit(&mut sender, 0);
    assert_eq!(flight.completion(), Some((0, None, false)));
    assert_eq!(flight.packets().len(), 0);
    assert_eq!(sender.available(), 3);
    assert_eq!(ALLOCATIONS.replace(None), Some((0, 0, 0)));
}

#[test]
fn application_limited_marker_is_read_from_each_groups_sampler() {
    let (mut sender, flights) = Sender::new(2, 1, 1).unwrap();
    let mut flight = buffers(flights).pop().unwrap();
    assert_eq!(sender.is_app_limited(2), None);
    assert_eq!(sender.is_app_limited(0), Some(false));
    sender.close(0).unwrap();
    assert!(sender.advance(0, 1).unwrap().is_none());
    sender.application_limited(0).unwrap();
    assert_eq!(sender.is_app_limited(0), Some(true));
    assert_eq!(sender.is_app_limited(1), Some(false));
    sender.reopen(0).unwrap();
    let planned = packet(0, 1, 100);
    flight.prepare(&[planned]).unwrap();
    sender
        .reserve(0, &mut flight, false, limits(), us(0))
        .unwrap();
    let handle = flight.handle(0).unwrap();
    flight.complete(1, Some(us(1)), false).unwrap();
    sender.reported(flight).unwrap();
    let _flight = commit(&mut sender, 0);
    assert!(acknowledge(&mut sender, 0, planned, handle, 2).app_limited);
    assert_eq!(sender.is_app_limited(0), Some(false));
}
