//! Attributes sender waits in an UnQuic `trace` feature file.
//!
//! Usage: trace_report FILE POINT LIFETIME_MS. POINT selects the harness's
//! before→sent load window (markers 3*POINT+1 and 3*POINT+2). Each frame that
//! expired before any submission is attributed to the states its worker
//! occupied during the LIFETIME_MS before its deadline. Prints one JSON object.
use std::{collections::BTreeMap, fs};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

// Record kinds; keep in sync with src/trace.rs.
const REQUEST: u64 = 1;
const PERMIT: u64 = 2;
const SENT: u64 = 3;
const RECYCLE: u64 = 4;
const EXPIRE: u64 = 5;
const GRANT: u64 = 6;
const BLOCK: u64 = 7;
const CONTROL: u64 = 8;
const BOUNDS: u64 = 9;
const LOSS: u64 = 10;
const LATE_ACK: u64 = 11;
const PHASE: u64 = 12;
const ACK_DROP: u64 = 13;
const MARK: u64 = 14;
const HOLD: u64 = 15;
const ADMIT_DROP: u64 = 19;
const ADMIT_REASONS: [&str; 7] = [
    "admit_other",
    "admit_reader_frames",
    "admit_reader_bytes",
    "admit_owner_queue",
    "admit_owner_buffers",
    "admit_owner_frames",
    "admit_owner_bytes",
];

// States 11 + reason refine "held" by the worker's last HOLD reason.
const WORKER_STATES: [&str; 23] = [
    "held",
    "queued",
    "phase",
    "pacing",
    "burst",
    "window",
    "stalled",
    "other",
    "handoff",
    "permitted",
    "commit",
    "held_0",
    "held_front_expired",
    "held_batch_expired",
    "held_socket",
    "held_mtu",
    "held_5",
    "held_prepare",
    "held_overtaken",
    "held_history",
    "held_9",
    "held_10",
    "held_retained",
];
const PHASES: [&str; 4] = ["open", "closing", "commit", "feedback"];
const CONTROLLER: [&str; 7] = [
    "startup",
    "drain",
    "down",
    "cruise",
    "refill",
    "up",
    "probe_rtt",
];

/// A piecewise-constant state per series, with time spent in each state.
struct Timeline {
    changes: Vec<(u64, usize)>,
}

impl Timeline {
    fn new() -> Self {
        Self {
            changes: vec![(0, 0)],
        }
    }

    fn set(&mut self, at: u64, state: usize) {
        if self.changes.last().is_some_and(|&(_, last)| last != state) {
            self.changes.push((at, state));
        }
    }

    /// Adds weight * time in each state over [start, end) to totals.
    fn accumulate(&self, start: u64, end: u64, weight: f64, totals: &mut [f64]) {
        let first = self.changes.partition_point(|&(at, _)| at <= start).max(1) - 1;
        for (index, &(at, state)) in self.changes.iter().enumerate().skip(first) {
            if at >= end {
                break;
            }
            let next = self
                .changes
                .get(index + 1)
                .map_or(end, |&(at, _)| at.min(end));
            let from = at.max(start);
            if next > from {
                totals[state] += (next - from) as f64 * weight;
            }
        }
    }
}

fn fractions(names: &[&str], totals: &[f64]) -> String {
    let sum: f64 = totals.iter().sum();
    names
        .iter()
        .zip(totals)
        .map(|(name, value)| {
            format!(
                "\"{name}\":{:.4}",
                if sum > 0.0 { value / sum } else { 0.0 }
            )
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Optional WORKER FROM_MS COUNT dumps that worker's events, and owner events naming it.
    let (path, point, lifetime, dump) = match args.as_slice() {
        [path, point, lifetime] => (path, point, lifetime, None),
        [path, point, lifetime, worker, from, count] => (
            path,
            point,
            lifetime,
            Some((
                worker.parse::<u64>()?,
                from.parse::<f64>()?,
                count.parse::<usize>()?,
            )),
        ),
        _ => {
            return Err("usage: trace_report FILE POINT LIFETIME_MS [WORKER FROM_MS COUNT]".into());
        }
    };
    let point: u64 = point.parse()?;
    let lifetime = (lifetime.parse::<f64>()? * 1e6) as u64;
    let bytes = fs::read(path)?;
    let mut records: Vec<[u64; 8]> = bytes
        .as_chunks::<64>()
        .0
        .iter()
        .map(|chunk| {
            let (words, _) = chunk.as_chunks::<8>();
            std::array::from_fn(|word| u64::from_le_bytes(words[word]))
        })
        .collect();
    records.sort_by_key(|record| record[1]);
    let marks: BTreeMap<u64, u64> = records
        .iter()
        .filter(|r| r[0] & 0xff == MARK && r[0] >> 16 != 0)
        .map(|r| (r[0] >> 16, r[1]))
        .collect();
    let lost: u64 = records
        .iter()
        .filter(|r| r[0] & 0xff == MARK)
        .map(|r| r[3])
        .max()
        .unwrap_or(0);
    let (Some(&start), Some(&end)) = (marks.get(&(3 * point + 1)), marks.get(&(3 * point + 2)))
    else {
        return Err(format!("markers for point {point} are missing").into());
    };
    if let Some((worker, from, count)) = dump {
        let from = start + (from * 1e6) as u64;
        let names = [
            "",
            "REQUEST",
            "PERMIT",
            "SENT",
            "RECYCLE",
            "EXPIRE",
            "GRANT",
            "BLOCK",
            "CONTROL",
            "BOUNDS",
            "LOSS",
            "LATE_ACK",
            "PHASE",
            "ACK_DROP",
            "MARK",
            "HOLD",
            "RETIRE",
            "ACK_REJECT",
            "ACK_BEGIN",
            "ADMIT_DROP",
        ];
        for r in records
            .iter()
            .filter(|r| r[1] >= from)
            .filter(|r| {
                let (kind, owner, aux) = (r[0] & 0xff, (r[0] >> 8) & 0xff, r[0] >> 16);
                owner == worker
                    || (matches!(kind, GRANT | BLOCK) && aux & 0xff == worker)
                    || matches!(kind, CONTROL | PHASE)
            })
            .take(count)
        {
            println!(
                "{:>12.3}us {:>8} w{} aux={} {:?}",
                (r[1] - start) as f64 / 1e3,
                names.get((r[0] & 0xff) as usize).copied().unwrap_or("?"),
                (r[0] >> 8) & 0xff,
                r[0] >> 16,
                &r[2..]
            );
        }
        return Ok(());
    }
    let workers = records
        .iter()
        .filter(|r| r[0] & 0xff != MARK)
        .map(|r| ((r[0] >> 8) & 0xff) as usize + 1)
        .max()
        .unwrap_or(0);
    let mut worker_states: Vec<Timeline> = (0..workers).map(|_| Timeline::new()).collect();
    let mut phase = Timeline::new();
    let mut controller = Timeline::new();
    let mut expiries = Vec::new();
    let mut counts = BTreeMap::<&str, f64>::new();
    // Time-weighted pacing rate and cwnd, in bytes/s and bytes.
    let mut last_control: Option<(u64, u64, u64)> = None;
    let (mut rate_area, mut cwnd_area, mut control_time) = (0.0, 0.0, 0.0);
    let (mut granted, mut lateness, mut late_grants) = (0.0, 0.0, 0.0);
    let mut grant_bytes = Vec::new();
    // Per worker: requests, requested bytes, held gaps (RECYCLE→REQUEST) total and count, unsent expiry.
    let mut per_worker = vec![[0.0f64; 5]; workers];
    let mut recycled_at = vec![None; workers];
    for record in &records {
        let kind = record[0] & 0xff;
        let worker = ((record[0] >> 8) & 0xff) as usize;
        let aux = record[0] >> 16;
        let at = record[1];
        let inside = (start..end).contains(&at);
        match kind {
            REQUEST => {
                worker_states[worker].set(at, 1);
                if inside {
                    per_worker[worker][0] += 1.0;
                    per_worker[worker][1] += record[3] as f64;
                    if let Some(from) = recycled_at[worker].take() {
                        per_worker[worker][2] += (at - from) as f64;
                        per_worker[worker][3] += 1.0;
                    }
                }
            }
            BLOCK => {
                let target = (aux & 0xff) as usize;
                // Reasons 1..=6 map to states 2..=7.
                if (1..=6).contains(&record[2]) && target < workers {
                    worker_states[target].set(at, record[2] as usize + 1);
                }
            }
            GRANT => {
                let target = (aux & 0xff) as usize;
                let paced = worker_states
                    .get(target)
                    .and_then(|timeline| timeline.changes.last())
                    .is_some_and(|&(_, state)| state == 3);
                if target < workers {
                    worker_states[target].set(at, 8);
                }
                if inside {
                    granted += record[3] as f64;
                    grant_bytes.push(record[3]);
                    // A request already waiting on pacing loses this owner delay for good.
                    if paced && record[2] > record[5] {
                        lateness += (record[2] - record[5]) as f64;
                        late_grants += 1.0;
                    }
                }
            }
            PERMIT => worker_states[worker].set(at, 9),
            SENT => worker_states[worker].set(at, 10),
            RECYCLE => {
                worker_states[worker].set(at, 0);
                recycled_at[worker] = Some(at);
            }
            HOLD => {
                let held = worker_states[worker]
                    .changes
                    .last()
                    .is_some_and(|&(_, state)| state == 0 || state >= 11);
                if held && aux <= 11 {
                    worker_states[worker].set(at, 11 + aux as usize);
                }
            }
            PHASE => phase.set(at, (aux as usize).min(3)),
            CONTROL => {
                controller.set(at, ((aux & 0xf) as usize).min(6));
                if let Some((from, rate, cwnd)) = last_control {
                    let (a, b) = (from.max(start), at.min(end));
                    if b > a {
                        rate_area += rate as f64 * (b - a) as f64;
                        cwnd_area += cwnd as f64 * (b - a) as f64;
                        control_time += (b - a) as f64;
                    }
                }
                last_control = Some((at, record[2], record[3]));
            }
            EXPIRE if inside => {
                let name = [
                    "expire_input",
                    "expire_unsent",
                    "expire_partial",
                    "expire_sent",
                ][(aux as usize).min(3)];
                *counts.entry(name).or_default() += record[2] as f64;
                if aux == 1 || aux == 0 {
                    expiries.push((worker, record[3], record[2] as f64));
                    per_worker[worker][4] += record[2] as f64;
                }
            }
            LOSS if inside => *counts.entry("losses").or_default() += 1.0,
            LATE_ACK if inside => *counts.entry("late_acks").or_default() += 1.0,
            ACK_DROP if inside => *counts.entry("ack_drops").or_default() += 1.0,
            ADMIT_DROP if inside => {
                *counts
                    .entry(ADMIT_REASONS[(aux as usize).min(6)])
                    .or_default() += 1.0
            }
            BOUNDS | MARK => {}
            _ => {}
        }
    }
    if let Some((from, rate, cwnd)) = last_control {
        let a = from.max(start);
        if end > a {
            rate_area += rate as f64 * (end - a) as f64;
            cwnd_area += cwnd as f64 * (end - a) as f64;
            control_time += (end - a) as f64;
        }
    }
    let mut expired_states = vec![0.0; WORKER_STATES.len()];
    let mut expired_phase = vec![0.0; PHASES.len()];
    let mut expired_controller = vec![0.0; CONTROLLER.len()];
    for &(worker, deadline, count) in &expiries {
        let from = deadline.saturating_sub(lifetime);
        worker_states[worker].accumulate(from, deadline, count, &mut expired_states);
        phase.accumulate(from, deadline, count, &mut expired_phase);
        controller.accumulate(from, deadline, count, &mut expired_controller);
    }
    let mut all_states = vec![0.0; WORKER_STATES.len()];
    let mut workers_json = Vec::new();
    for (worker, timeline) in worker_states.iter().enumerate() {
        timeline.accumulate(start, end, 1.0, &mut all_states);
        let mut own = vec![0.0; WORKER_STATES.len()];
        timeline.accumulate(start, end, 1.0, &mut own);
        let [requests, bytes, held, gaps, expired] = per_worker[worker];
        workers_json.push(format!(
            "{{\"worker\":{worker},\"requests_per_s\":{:.0},\"mean_request_bytes\":{:.0},\
\"mean_held_gap_us\":{:.1},\"never_sent_expired\":{expired},\"states\":{{{}}}}}",
            requests / ((end - start) as f64 / 1e9),
            if requests > 0.0 {
                bytes / requests
            } else {
                0.0
            },
            if gaps > 0.0 { held / gaps / 1e3 } else { 0.0 },
            fractions(&WORKER_STATES, &own),
        ));
    }
    let mut all_phase = vec![0.0; PHASES.len()];
    phase.accumulate(start, end, 1.0, &mut all_phase);
    let mut all_controller = vec![0.0; CONTROLLER.len()];
    controller.accumulate(start, end, 1.0, &mut all_controller);
    let seconds = (end - start) as f64 / 1e9;
    grant_bytes.sort_unstable();
    let median_grant = grant_bytes.get(grant_bytes.len() / 2).copied().unwrap_or(0);
    println!(
        "{{\"point\":{point},\"seconds\":{seconds:.3},\"records_lost\":{lost},\"counts\":{{{}}},\
\"granted_gbps\":{:.4},\"grants\":{},\"median_grant_bytes\":{median_grant},\
\"mean_owner_lateness_us\":{:.2},\"lateness_share\":{:.4},\
\"mean_pacing_gbps\":{:.4},\"mean_cwnd_bytes\":{:.0},\
\"window\":{{\"worker\":{{{}}},\"phase\":{{{}}},\"controller\":{{{}}}}},\
\"expired_lifetimes\":{{\"frames\":{},\"worker\":{{{}}},\"phase\":{{{}}},\"controller\":{{{}}}}},\
\"workers\":[{}]}}",
        counts
            .iter()
            .map(|(name, value)| format!("\"{name}\":{value}"))
            .collect::<Vec<_>>()
            .join(","),
        granted * 8.0 / seconds / 1e9,
        grant_bytes.len(),
        if late_grants > 0.0 {
            lateness / late_grants / 1e3
        } else {
            0.0
        },
        lateness / (end - start) as f64,
        if control_time > 0.0 {
            rate_area / control_time * 8.0 / 1e9
        } else {
            0.0
        },
        if control_time > 0.0 {
            cwnd_area / control_time
        } else {
            0.0
        },
        fractions(&WORKER_STATES, &all_states),
        fractions(&PHASES, &all_phase),
        fractions(&CONTROLLER, &all_controller),
        expiries.iter().map(|e| e.2).sum::<f64>(),
        fractions(&WORKER_STATES, &expired_states),
        fractions(&PHASES, &expired_phase),
        fractions(&CONTROLLER, &expired_controller),
        workers_json.join(","),
    );
    Ok(())
}
