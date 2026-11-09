use super::*;
use crate::bbr::{Delivery, DeliveryPacket};
use std::sync::Arc;

#[test]
fn publication_is_never_torn_under_concurrent_reads() {
    let budget = Arc::new(Budget::new());
    let writer = {
        let budget = Arc::clone(&budget);
        std::thread::spawn(move || {
            for value in 1..=20_000_u64 {
                budget.publish(&Published {
                    rate: value,
                    window: value,
                    quantum: value,
                    delivered: value,
                    lost: value,
                    delivered_at: Duration::from_nanos(value),
                    first_sent_at: Duration::from_nanos(value),
                    app_limited: value.is_multiple_of(2),
                    acks: value,
                });
            }
        })
    };
    let mut last = 0;
    while last < 20_000 {
        let seen = budget.published();
        let value = seen.rate;
        assert!(value >= last, "publications never go backwards");
        assert_eq!(
            [
                seen.window,
                seen.quantum,
                seen.delivered,
                seen.lost,
                seen.acks
            ],
            [value; 5]
        );
        assert_eq!(seen.delivered_at, Duration::from_nanos(value));
        assert_eq!(seen.first_sent_at, Duration::from_nanos(value));
        assert_eq!(seen.app_limited, value != 0 && value.is_multiple_of(2));
        last = value;
    }
    writer.join().unwrap();
}

#[test]
fn concurrent_claims_never_exceed_the_window_or_share_a_pacing_slot() {
    const WINDOW: u64 = 10_000;
    let budget = Arc::new(Budget::new());
    let peak = Arc::new(AtomicU64::new(0));
    let slots = Arc::new(AtomicU64::new(0));
    let threads: Vec<_> = (0..4_u64)
        .map(|worker| {
            let (budget, peak, slots) =
                (Arc::clone(&budget), Arc::clone(&peak), Arc::clone(&slots));
            std::thread::spawn(move || {
                for round in 0..20_000_u64 {
                    let bytes = 100 + (round * 7 + worker * 13) % 1400;
                    if let Some(prior) = budget.charge(bytes, Some(WINDOW)) {
                        peak.fetch_max(prior + bytes, Ordering::Relaxed);
                        budget.release(bytes);
                    }
                    // Every worker races for the same instant; only one claim may win it.
                    let at = Duration::from_millis(round + 1);
                    if budget.pace(1000, 1_000_000_000, at).is_ok() {
                        slots.fetch_add(1, Ordering::Relaxed);
                    }
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
    assert!(peak.load(Ordering::Relaxed) <= WINDOW);
    assert_eq!(budget.in_flight(), 0);
    assert!(slots.load(Ordering::Relaxed) <= 20_000);
}

#[test]
fn charges_respect_the_window_and_pacing_starts_at_the_claim() {
    let budget = Budget::new();
    assert_eq!(budget.charge(1000, Some(1500)), Some(0));
    assert_eq!(budget.charge(1000, Some(1500)), None, "window refuses");
    assert_eq!(budget.charge(1000, None), Some(1000), "probes bypass it");
    budget.release(1500);
    assert_eq!(budget.in_flight(), 500);
    budget.release(9999);
    assert_eq!(budget.in_flight(), 0, "release saturates");

    let at = Duration::from_micros(10);
    assert_eq!(budget.pace(1000, 0, at), Err(Duration::MAX));
    assert_eq!(budget.pace(1000, 1_000_000, at), Ok(()));
    // 1000 bytes at 1 MB/s occupy one millisecond after the claim.
    let next = at + Duration::from_millis(1);
    assert_eq!(budget.pace(1, 1_000_000, at), Err(next));
    assert_eq!(budget.pace(1, 1_000_000, next), Ok(()));
}

struct Packet {
    lane: usize,
    record: SendRecord,
    reference: DeliveryPacket,
    acked: bool,
    lost: bool,
}

/// Runs one scenario through the reference sampler, fed per packet in
/// chronological order, and through per-ACK reports from lane owners.
fn compare(seed: u64, workers: usize, steps: usize) {
    let mut random = seed | 1;
    let mut next = move |bound: u64| {
        random ^= random << 13;
        random ^= random >> 7;
        random ^= random << 17;
        random % bound
    };
    let mut reference = Delivery::new();
    let budget = Budget::new();
    let mut sampler = Sampler::default();
    budget.publish(&sampler.published(1, u64::MAX, 1));
    let mut packets: Vec<Packet> = Vec::new();
    let mut now = Duration::from_micros(1);
    for _ in 0..steps {
        now += Duration::from_micros(1 + next(50));
        let lane = next(workers as u64) as usize;
        match next(10) {
            0..=4 => {
                // One syscall's successful prefix shares a timestamp and one batch charge.
                let count = 1 + next(4) as usize;
                let sizes: Vec<u16> = (0..count).map(|_| 100 + next(1401) as u16).collect();
                let total: u64 = sizes.iter().map(|&b| u64::from(b)).sum();
                let state = budget.published();
                let mut prior = budget.charge(total, None).unwrap();
                for bytes in sizes {
                    let (pmtu, host) = (next(8) == 0, next(8) == 0);
                    let mut packet = reference.sent(now, bytes, prior, host).unwrap();
                    packet.pmtu = pmtu;
                    let record = budget.record(&state, prior, bytes, now, pmtu, host);
                    packets.push(Packet {
                        lane,
                        record,
                        reference: packet,
                        acked: false,
                        lost: false,
                    });
                    prior += u64::from(bytes);
                }
            }
            5..=7 => {
                // A lane's ACK newly covers an arbitrary subset, including late originals.
                let min_rtt = Duration::from_micros(next(3) * 20);
                let host = next(8) == 0;
                reference.begin_ack(now, host).unwrap();
                let mut newly = 0;
                let mut newest: Option<SendRecord> = None;
                let mut pmtu_only = true;
                for packet in packets
                    .iter_mut()
                    .filter(|p| p.lane == lane && !p.acked)
                    .filter(|_| next(3) != 0)
                {
                    assert!(reference.acknowledged(&mut packet.reference).unwrap());
                    packet.acked = true;
                    newly += u64::from(packet.record.bytes);
                    pmtu_only &= packet.record.pmtu;
                    if !packet.lost {
                        budget.release(u64::from(packet.record.bytes));
                    }
                    let key = (packet.record.sent_at, packet.record.order);
                    if newest.is_none_or(|old| key > (old.sent_at, old.order)) {
                        newest = Some(packet.record);
                    }
                }
                let expected = reference.finish_ack(min_rtt).unwrap();
                let actual =
                    newest.map(|record| sampler.ack(now, newly, &record, pmtu_only, host, min_rtt));
                assert_eq!(actual, expected, "seed {seed}");
                budget.publish(&sampler.published(1, u64::MAX, 1));
            }
            8 => {
                if let Some(packet) = packets
                    .iter_mut()
                    .filter(|p| p.lane == lane && !p.acked && !p.lost)
                    .last()
                {
                    let expected = reference.declared_lost(&mut packet.reference).unwrap();
                    packet.lost = true;
                    budget.release(u64::from(packet.record.bytes));
                    assert_eq!(Some(sampler.lost(&packet.record)), expected, "seed {seed}");
                    budget.publish(&sampler.published(1, u64::MAX, 1));
                }
            }
            _ => {
                let in_flight = budget.in_flight();
                reference.application_limited(in_flight).unwrap();
                sampler.application_limited(in_flight);
                budget.publish(&sampler.published(1, u64::MAX, 1));
            }
        }
        assert_eq!(reference.delivered(), sampler.delivered, "seed {seed}");
        assert_eq!(reference.lost(), sampler.lost, "seed {seed}");
    }
}

// Equivalence oracle for design section 9's replacement sender: per-ACK reports
// of the newest record must reproduce every chronological draft-07 sample.
#[test]
fn per_ack_reports_match_the_chronological_sampler() {
    for seed in 1..=400 {
        compare(seed * 0x9e37_79b9, 1 + (seed % 4) as usize, 300);
    }
}
