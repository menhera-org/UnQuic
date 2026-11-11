//! Diagnostics-only cycle accounting of worker loop phases (feature `profile`).
//! Never enable for production or acceptance runs.
//!
//! Each worker adds the timestamp-counter cycles spent in each phase of its
//! loop, and prints the totals to stderr when it stops. `wait` is a blocking
//! epoll wait, `poll` a zero-timeout one. Signal-based sampling is not used:
//! SIGPROF timers on worker threads crashed the daemon in a way not yet
//! explained, while the same timer in a plain test thread did not.

/// Phase names, indexed as `Worker::run` records them.
pub const NAMES: [&str; PHASES] = [
    "drain_queues",
    "setup",
    "distribute_receivers",
    "addresses",
    "service_sender",
    "progress_barriers",
    "timers",
    "mtu_timers",
    "drain_packets",
    "drain_tap_inputs",
    "flush_frames",
    "flush_returns",
    "tap_can_expand",
    "flush_taps",
    "collect_acks",
    "flush_control",
    "flush_sender_messages",
    "flush_notifications",
    "ready_scan",
    "receive_interest",
    "publish_counters",
    "arm",
    "wait",
    "read_tap",
    "transmit_event",
    "read_socket",
    "reactor_drain",
    "poll",
];

/// Number of accounted worker loop phases.
pub const PHASES: usize = 28;

/// Current timestamp-counter value, or nanoseconds where there is none.
#[inline(always)]
pub fn now() -> u64 {
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: rdtsc has no preconditions.
        unsafe { core::arch::x86_64::_rdtsc() }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        use std::sync::OnceLock;
        static START: OnceLock<std::time::Instant> = OnceLock::new();
        START
            .get_or_init(std::time::Instant::now)
            .elapsed()
            .as_nanos() as u64
    }
}

/// Transmit admission drops by cause: queued inputs full, exchange buffers
/// exhausted, frame quota reached, byte quota reached.
pub static ADMISSION: [std::sync::atomic::AtomicU64; 4] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 4];

/// Per-process syscall totals: cycles, calls and items for sendmmsg.
pub static SYSCALLS: [[std::sync::atomic::AtomicU64; 3]; 1] =
    [const { [const { std::sync::atomic::AtomicU64::new(0) }; 3] }; 1];

/// Adds one call of syscall `kind` that started at `started` and moved `items`.
pub fn syscall(kind: usize, started: u64, items: usize) {
    use std::sync::atomic::Ordering::Relaxed;
    if let Some(counters) = SYSCALLS.get(kind) {
        counters[0].fetch_add(now().wrapping_sub(started), Relaxed);
        counters[1].fetch_add(1, Relaxed);
        counters[2].fetch_add(items as u64, Relaxed);
    }
}

/// Prints one worker's phase totals, largest first, as shares of their sum.
pub fn report(worker: usize, phases: &[u64; PHASES]) {
    let total: u64 = phases.iter().sum();
    let mut order: Vec<usize> = (0..PHASES).collect();
    order.sort_by_key(|&index| std::cmp::Reverse(phases[index]));
    let admission = ADMISSION
        .each_ref()
        .map(|count| count.load(std::sync::atomic::Ordering::Relaxed));
    let sendmmsg = SYSCALLS[0]
        .each_ref()
        .map(|count| count.load(std::sync::atomic::Ordering::Relaxed));
    let mut line = format!(
        "unquic: profile worker {worker} admission inputs/buffers/frames/bytes {admission:?} sendmmsg cycles/calls/datagrams {sendmmsg:?} cycles {total}"
    );
    for index in order.into_iter().filter(|&index| phases[index] != 0) {
        line.push_str(&format!(
            " {}={:.1}%",
            NAMES[index],
            100.0 * phases[index] as f64 / total.max(1) as f64
        ));
    }
    eprintln!("{line}");
}
