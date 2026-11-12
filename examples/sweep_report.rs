//! Summarizes `benches/tap_transport.sh` UDP_LOAD output trees.
//!
//! Layout: ROOT/BUILD/COMBO/REPEAT/POINT, where POINT is the harness's numbered
//! rate directory. Prints one JSON object per point, then one per
//! (BUILD, COMBO, POINT) group with the median, minimum and maximum over repeats.
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Daemon counters reported per point; deltas over the before→after window.
const COUNTERS: &[&str] = &[
    "tx_expired_unsent_frames",
    "tx_expired_input_frames",
    "tx_expired_partial_frames",
    "tx_expired_sent_frames",
    "rx_expired_incomplete_frames",
    "queue_drops",
    "tx_admission_drops",
    "handoff_drops",
    "tx_backlog_drops",
    "ack_buffer_drops",
    "ack_send_deferred",
    "admission_pressure",
    "socket_overflow",
    "repairs",
    "send_grants",
    "expired_grants",
];

/// Sums every `"key":number` value in text.
fn sum_key(text: &str, key: &str) -> f64 {
    let pattern = format!("\"{key}\":");
    text.match_indices(&pattern)
        .filter_map(|(at, _)| number(&text[at + pattern.len()..]))
        .sum()
}

fn number(text: &str) -> Option<f64> {
    let end = text
        .find(|c: char| !(c.is_ascii_digit() || matches!(c, '.' | '-' | 'e' | 'E' | '+')))
        .unwrap_or(text.len());
    text[..end].parse().ok()
}

/// Latency histogram buckets; keep in sync with examples/udp_load.rs.
const SUB_BITS: usize = 6;
const LATENCY_BUCKETS: usize = (48 - SUB_BITS + 1) << SUB_BITS;

/// Largest latency, in ns, a udp_load histogram bucket holds.
fn latency_upper(bucket: usize) -> u64 {
    let sub = 1 << SUB_BITS;
    if bucket < sub {
        return bucket as u64;
    }
    let shift = (bucket >> SUB_BITS) - 1;
    (((sub + bucket % sub) as u64) << shift) + (1 << shift) - 1
}

/// Adds a receiver's `"histogram":[[bucket,count],...]` to `counts`.
fn merge_histogram(text: &str, counts: &mut [u64]) {
    let Some((_, rest)) = text.split_once("\"histogram\":[") else {
        return;
    };
    let body = rest.split_once("]]").map_or("", |(body, _)| body);
    for pair in body.split('[') {
        let mut values = pair.trim_end_matches([',', ']']).split(',');
        if let (Some(Ok(bucket)), Some(Ok(count))) = (
            values.next().map(str::parse::<usize>),
            values.next().map(str::parse::<u64>),
        ) && let Some(total) = counts.get_mut(bucket)
        {
            *total += count;
        }
    }
}

/// Upper bound, in µs, of the bucket holding quantile `q` of the samples.
fn latency_quantile(counts: &[u64], q: f64) -> f64 {
    let total: u64 = counts.iter().sum();
    let target = (total as f64 * q).ceil().max(1.0) as u64;
    let mut seen = 0;
    for (bucket, &count) in counts.iter().enumerate() {
        seen += count;
        if seen >= target {
            return latency_upper(bucket) as f64 / 1e3;
        }
    }
    0.0
}

/// One direction's load files: client(-k).json and server(-k).json.
struct Direction {
    offered_gbps: f64,
    delivered_gbps: f64,
    sent: f64,
    unique: f64,
    invalid: f64,
    too_old: f64,
    duplicates: f64,
    reordered: f64,
    flows: usize,
    latency: [u64; LATENCY_BUCKETS],
    latency_max_ns: f64,
}
impl Default for Direction {
    fn default() -> Self {
        Self {
            offered_gbps: 0.0,
            delivered_gbps: 0.0,
            sent: 0.0,
            unique: 0.0,
            invalid: 0.0,
            too_old: 0.0,
            duplicates: 0.0,
            reordered: 0.0,
            flows: 0,
            latency: [0; LATENCY_BUCKETS],
            latency_max_ns: 0.0,
        }
    }
}

fn direction(point: &Path, client: &str, server: &str) -> Result<Direction> {
    let mut result = Direction::default();
    for k in 0.. {
        let suffix = if k == 0 {
            String::new()
        } else {
            format!("-{k}")
        };
        let client_path = point.join(format!("{client}{suffix}.json"));
        if !client_path.exists() {
            break;
        }
        let sent_text = fs::read_to_string(client_path)?;
        let received = fs::read_to_string(point.join(format!("{server}{suffix}.json")))?;
        let payload = sum_key(&sent_text, "payload_bytes");
        let sent = sum_key(&sent_text, "sent");
        // The quoted pattern does not match "cpu_seconds"; senders record no samples.
        let seconds = sum_key(&sent_text, "seconds");
        result.offered_gbps += sent * payload * 8.0 / seconds / 1e9;
        result.sent += sent;
        let (flows, samples) = received
            .split_once("\"samples\":")
            .ok_or("missing samples")?;
        // Older generators wrote no latency; their points report none.
        if let Some((_, latency)) = samples.split_once("\"latency\":") {
            merge_histogram(latency, &mut result.latency);
            result.latency_max_ns = result.latency_max_ns.max(sum_key(latency, "max_ns"));
        }
        result.unique += sum_key(flows, "unique");
        result.invalid += sum_key(flows, "invalid");
        result.too_old += sum_key(flows, "too_old");
        result.duplicates += sum_key(flows, "duplicates");
        result.reordered += sum_key(flows, "reordered");
        result.flows += flows.matches("\"port\":").count();
        let times: Vec<f64> = samples
            .match_indices("\"seconds\":")
            .filter_map(|(at, _)| number(&samples[at + 10..]))
            .collect();
        let counts: Vec<f64> = samples
            .match_indices("\"unique\":")
            .filter_map(|(at, _)| number(&samples[at + 9..]))
            .collect();
        if times.len() != 2 || counts.len() != 2 || times[1] <= times[0] {
            return Err(format!("{}: need two receiver samples", point.display()).into());
        }
        result.delivered_gbps +=
            (counts[1] - counts[0]) * payload * 8.0 / (times[1] - times[0]) / 1e9;
    }
    if result.sent == 0.0 {
        return Err(format!("{}: no {client}.json", point.display()).into());
    }
    Ok(result)
}

/// Sums the latest Snapshot line of every worker in one daemon log.
fn counters(log: &str) -> BTreeMap<String, f64> {
    let mut latest = BTreeMap::new();
    for line in log.lines() {
        let Some(rest) = line.strip_prefix("unquic: worker ") else {
            continue;
        };
        let Some((worker, body)) = rest.split_once(" Snapshot { ") else {
            continue;
        };
        latest.insert(worker.to_owned(), body.trim_end_matches(" }").to_owned());
    }
    let mut totals = BTreeMap::new();
    for body in latest.values() {
        for field in body.split(", ") {
            if let Some((name, value)) = field.split_once(": ")
                && let Ok(value) = value.parse::<f64>()
            {
                *totals.entry(name.to_owned()).or_insert(0.0) += value;
            }
        }
    }
    totals
}

/// utime+stime clock ticks after the last ')' of a /proc stat line.
fn ticks(line: &str) -> Option<f64> {
    let fields: Vec<&str> = line.rsplit_once(')')?.1.split_whitespace().collect();
    Some(fields.get(11)?.parse::<f64>().ok()? + fields.get(12)?.parse::<f64>().ok()?)
}

fn first_number(path: &Path) -> Result<f64> {
    number(fs::read_to_string(path)?.trim_start()).ok_or_else(|| "bad number".into())
}

/// Daemon process CPU percent, left and right, over before→sent.
fn cpu(point: &Path) -> Result<[f64; 2]> {
    let tick = first_number(&point.join("clock-ticks"))?;
    let elapsed =
        first_number(&point.join("sent.uptime"))? - first_number(&point.join("before.uptime"))?;
    let before = fs::read_to_string(point.join("before.process"))?;
    let after = fs::read_to_string(point.join("sent.process"))?;
    let mut result = [0.0; 2];
    for (slot, (b, a)) in result.iter_mut().zip(before.lines().zip(after.lines())) {
        *slot =
            (ticks(a).ok_or("bad stat")? - ticks(b).ok_or("bad stat")?) / tick / elapsed * 100.0;
    }
    Ok(result)
}

/// Busiest daemon thread percent, left and right, over before→sent.
fn busiest_thread(point: &Path) -> Result<[f64; 2]> {
    let tick = first_number(&point.join("clock-ticks"))?;
    let elapsed =
        first_number(&point.join("sent.uptime"))? - first_number(&point.join("before.uptime"))?;
    let read = |name: &str| -> Result<BTreeMap<String, f64>> {
        let text = fs::read_to_string(point.join(name))?;
        Ok(text
            .lines()
            .filter_map(|line| Some((line.split_whitespace().next()?.to_owned(), ticks(line)?)))
            .collect())
    };
    let (before, after) = (read("before.threads")?, read("sent.threads")?);
    // The switches file maps /proc/PID/task/TID; its first PID is the left daemon.
    let switches = fs::read_to_string(point.join("sent.switches"))?;
    let mut owner = BTreeMap::new();
    let mut left = None;
    for line in switches.lines().filter(|line| line.starts_with("/proc/")) {
        let parts: Vec<&str> = line.split('/').collect();
        if let (Some(pid), Some(tid)) = (parts.get(2), parts.get(4)) {
            let pid = (*pid).to_owned();
            left.get_or_insert_with(|| pid.clone());
            owner.insert((*tid).to_owned(), Some(&pid) == left.as_ref());
        }
    }
    let mut result = [0.0f64; 2];
    for (tid, a) in &after {
        let (Some(b), Some(is_left)) = (before.get(tid), owner.get(tid)) else {
            continue;
        };
        let slot = &mut result[usize::from(!is_left)];
        *slot = slot.max((a - b) / tick / elapsed * 100.0);
    }
    Ok(result)
}

/// tc "dropped N" total for one qdisc snapshot.
fn qdisc_drops(path: &Path) -> f64 {
    fs::read_to_string(path)
        .map(|text| sum_after(&text, "(dropped "))
        .unwrap_or(0.0)
}

fn sum_after(text: &str, pattern: &str) -> f64 {
    text.match_indices(pattern)
        .filter_map(|(at, _)| number(&text[at + pattern.len()..]))
        .sum()
}

struct Point {
    build: String,
    combo: String,
    repeat: String,
    index: String,
    values: Vec<(String, f64)>,
}

fn point(path: &Path, build: &str, combo: &str, repeat: &str, index: &str) -> Result<Point> {
    let forward = direction(path, "client", "server")?;
    let reverse = direction(path, "reverse-client", "reverse-server")?;
    let mut values = vec![
        (
            "requested_bps_per_flow".to_owned(),
            first_number(&path.join("bitrate"))?,
        ),
        ("flows".to_owned(), forward.flows as f64),
    ];
    for (name, d) in [("lr", &forward), ("rl", &reverse)] {
        values.push((format!("{name}_offered_gbps"), d.offered_gbps));
        values.push((format!("{name}_delivered_gbps"), d.delivered_gbps));
        values.push((
            format!("{name}_missing_pct"),
            (d.sent - d.unique) / d.sent * 100.0,
        ));
        values.push((format!("{name}_invalid"), d.invalid));
        values.push((format!("{name}_too_old"), d.too_old));
        values.push((format!("{name}_duplicates"), d.duplicates));
        values.push((format!("{name}_reordered"), d.reordered));
        if d.latency.iter().any(|&count| count != 0) {
            for (label, q) in [("p50", 0.5), ("p99", 0.99), ("p999", 0.999)] {
                values.push((
                    format!("{name}_latency_{label}_us"),
                    latency_quantile(&d.latency, q),
                ));
            }
            values.push((format!("{name}_latency_max_us"), d.latency_max_ns / 1e3));
        }
    }
    let cpu = cpu(path)?;
    let busiest = busiest_thread(path)?;
    for (i, side) in ["left", "right"].into_iter().enumerate() {
        values.push((format!("{side}_cpu_pct"), cpu[i]));
        values.push((format!("{side}_busiest_thread_pct"), busiest[i]));
        let before = counters(&fs::read_to_string(
            path.join(format!("before.{side}-log")),
        )?);
        let after = counters(&fs::read_to_string(path.join(format!("after.{side}-log")))?);
        for name in COUNTERS {
            let delta = after.get(*name).copied().unwrap_or(0.0)
                - before.get(*name).copied().unwrap_or(0.0);
            values.push((format!("{side}_{name}"), delta));
        }
        // qdisc-left shapes R→L and qdisc-right shapes L→R.
        let drops = qdisc_drops(&path.join(format!("after.qdisc-{side}")))
            - qdisc_drops(&path.join(format!("before.qdisc-{side}")));
        values.push((format!("qdisc_{side}_drops"), drops));
    }
    Ok(Point {
        build: build.to_owned(),
        combo: combo.to_owned(),
        repeat: repeat.to_owned(),
        index: index.to_owned(),
        values,
    })
}

fn children(path: &Path) -> Result<Vec<(String, PathBuf)>> {
    let mut entries: Vec<_> = fs::read_dir(path)?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_dir())
        .map(|entry| {
            (
                entry.file_name().to_string_lossy().into_owned(),
                entry.path(),
            )
        })
        .collect();
    entries.sort();
    Ok(entries)
}

fn json_values(values: &[(String, f64)]) -> String {
    values
        .iter()
        .map(|(name, value)| format!("\"{name}\":{}", round(*value)))
        .collect::<Vec<_>>()
        .join(",")
}

fn round(value: f64) -> f64 {
    (value * 1e4).round() / 1e4
}

fn median(sorted: &[f64]) -> f64 {
    let n = sorted.len();
    if n % 2 == 1 {
        sorted[n / 2]
    } else {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    }
}

fn main() -> Result<()> {
    let root = std::env::args().nth(1).ok_or("usage: sweep_report ROOT")?;
    let mut points = Vec::new();
    for (build, build_path) in children(Path::new(&root))? {
        for (combo, combo_path) in children(&build_path)? {
            for (repeat, repeat_path) in children(&combo_path)? {
                for (index, point_path) in children(&repeat_path)? {
                    if point_path.join("bitrate").exists() {
                        points.push(point(&point_path, &build, &combo, &repeat, &index)?);
                    }
                }
            }
        }
    }
    println!("{{\"points\":[");
    for (i, p) in points.iter().enumerate() {
        let comma = if i == 0 { "" } else { "," };
        println!(
            "{comma}{{\"build\":\"{}\",\"combo\":\"{}\",\"repeat\":\"{}\",\"point\":\"{}\",{}}}",
            p.build,
            p.combo,
            p.repeat,
            p.index,
            json_values(&p.values)
        );
    }
    println!("],\"groups\":[");
    let mut groups: BTreeMap<(&str, &str, &str), Vec<&Point>> = BTreeMap::new();
    for p in &points {
        groups
            .entry((&p.build, &p.combo, &p.index))
            .or_default()
            .push(p);
    }
    for (i, ((build, combo, index), members)) in groups.iter().enumerate() {
        let comma = if i == 0 { "" } else { "," };
        let mut stats = Vec::new();
        for (column, (name, _)) in members[0].values.iter().enumerate() {
            let mut column: Vec<f64> = members.iter().map(|p| p.values[column].1).collect();
            column.sort_by(f64::total_cmp);
            stats.push(format!(
                "\"{name}\":[{},{},{}]",
                round(median(&column)),
                round(column[0]),
                round(column[column.len() - 1])
            ));
        }
        println!(
            "{comma}{{\"build\":\"{build}\",\"combo\":\"{combo}\",\"point\":\"{index}\",\"repeats\":{},\"median_min_max\":{{{}}}}}",
            members.len(),
            stats.join(",")
        );
    }
    println!("]}}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latency_histograms_merge_and_give_bucket_quantiles() {
        let mut counts = [0; LATENCY_BUCKETS];
        merge_histogram(
            "{\"count\":4,\"max_ns\":9,\"histogram\":[[3,2],[100,1]]}",
            &mut counts,
        );
        merge_histogram("\"histogram\":[[100,1],[9999,5]]}", &mut counts);
        merge_histogram("\"histogram\":[]}", &mut counts);
        assert_eq!((counts[3], counts[100]), (2, 2));
        assert_eq!(counts.iter().sum::<u64>(), 4);
        assert_eq!(latency_quantile(&counts, 0.5), 0.003);
        assert_eq!(
            latency_quantile(&counts, 0.99),
            latency_upper(100) as f64 / 1e3
        );
        assert_eq!(latency_quantile(&[0; LATENCY_BUCKETS], 0.5), 0.0);
    }
}
