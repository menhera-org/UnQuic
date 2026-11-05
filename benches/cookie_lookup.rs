//! Compare dispatch lookup strategies; timings are observations, not test gates.
use std::{collections::HashMap, hint::black_box, time::Instant};
use unquic::{UnquicUuid, uuid::CookieIndex};

fn random(seed: &mut u128) -> UnquicUuid {
    *seed ^= *seed << 23;
    *seed ^= *seed >> 17;
    *seed ^= *seed << 26;
    UnquicUuid::from_bytes(seed.to_be_bytes())
}
fn measure(mut lookup: impl FnMut(UnquicUuid) -> Option<usize>, probes: &[UnquicUuid]) -> f64 {
    let mut best = f64::MAX;
    for _ in 0..3 {
        let start = Instant::now();
        for i in 0..1_000_000 {
            black_box(lookup(black_box(probes[i & (probes.len() - 1)])));
        }
        best = best.min(start.elapsed().as_nanos() as f64 / 1_000_000.);
    }
    best
}
fn main() {
    for count in [1, 4, 8, 16, 32, 64, 256, 1024, 4096] {
        let mut seed = 0x123456789abcdef_u128;
        let entries: Vec<_> = (0..count).map(|i| (random(&mut seed), i)).collect();
        let index = CookieIndex::new(entries.iter().map(|entry| entry.0)).unwrap();
        let mut sorted = entries.clone();
        sorted.sort_unstable_by_key(|entry| entry.0);
        let hash: HashMap<_, _> = entries.iter().copied().collect();
        for all_hits in [true, false] {
            let probes: Vec<_> = (0..4096)
                .map(|i| {
                    if all_hits || i % 2 == 0 {
                        entries[(i * 331) & (count - 1)].0
                    } else {
                        random(&mut seed)
                    }
                })
                .collect();
            let actual = measure(|key| index.find(key), &probes);
            let binary = measure(
                |key| {
                    sorted
                        .binary_search_by_key(&key, |entry| entry.0)
                        .ok()
                        .map(|i| sorted[i].1)
                },
                &probes,
            );
            let hashed = measure(|key| hash.get(&key).copied(), &probes);
            println!(
                "sessions={count:4} hits={} index={actual:.2} binary={binary:.2} hash={hashed:.2} ns/lookup",
                if all_hits { 100 } else { 50 }
            );
        }
    }
}
