//! Diagnostics-only event rings, compiled only with the `trace` feature.
//!
//! Never enable this feature in production or acceptance builds. Each worker
//! owns one fixed ring, allocated once when the runtime starts; pushes take no
//! lock and allocate nothing. A slow reader loses old records, never blocks a
//! worker, and discards any record a writer may have overwritten during a copy.
use std::{
    sync::{
        OnceLock,
        atomic::{AtomicU64, Ordering, fence},
    },
    time::Duration,
};

/// Words per record: `kind | worker << 8 | aux << 16`, monotonic ns, six payload words.
pub const WORDS: usize = 8;
/// Records retained per worker before the oldest is overwritten.
pub const RECORDS: usize = 1 << 16;
/// One encoded event.
pub type Record = [u64; WORDS];

/// Worker sent a DATA request: lane key, bytes, packets, earliest frame deadline, unsent bytes.
pub const REQUEST: u64 = 1;
/// Worker received permission: granted bytes, grant expiry.
pub const PERMIT: u64 = 2;
/// Worker finished a send attempt: packets sent, packets permitted, pre-syscall time.
pub const SENT: u64 = 3;
/// Worker regained its flight buffer: completed packet count, or u64::MAX if unsent.
pub const RECYCLE: u64 = 4;
/// Transmit frames expired; aux 0 input, 1 unsent, 2 partial, 3 sent: count, deadline, lane key.
pub const EXPIRE: u64 = 5;
/// Owner granted aux worker: group, bytes, requested bytes, prior next send, reserved, inflight.
pub const GRANT: u64 = 6;
/// Owner left aux worker blocked: reason, requested bytes, next send, reserved, inflight, window.
pub const BLOCK: u64 = 7;
/// Controller: pacing rate, cwnd, inflight, max_bw, short-term bw, min RTT ns; aux = state.
pub const CONTROL: u64 = 8;
/// Controller bounds: short-term inflight, long-term inflight, delivered, lost, quantum, srtt ns.
pub const BOUNDS: u64 = 9;
/// Owner declared a loss: lane key, packet number, bytes; aux 1 when later ACKed.
pub const LOSS: u64 = 10;
/// Owner received an ACK for a packet previously declared lost: lane key, packet number.
pub const LATE_ACK: u64 = 11;
/// Owner issuance phase changed; aux = 0 open, 1 closing, 2 commit, 3 feedback.
pub const PHASE: u64 = 12;
/// Receiving worker dropped an incoming ACK record for lack of a transfer buffer.
pub const ACK_DROP: u64 = 13;
/// Written by a reader, not a worker: monotonic ns and CLOCK_BOOTTIME ns at a marker.
pub const MARK: u64 = 14;
/// A worker skipped requesting a lane; aux = HOLD_* reason: lane key, batch frames, plan packets.
pub const HOLD: u64 = 15;
/// A pressure retirement pass ended: lane key, retired entries, floor, allocated, samples;
/// aux = why the floor stopped (0 budget, 1 reserved, 2 inflight, 3 above allocated, 4 pending
/// ACK, 5 ACK work first, 6 report in use).
pub const RETIRE: u64 = 16;
/// The TX owner rejected a whole ACK; aux = 1 stale tag or lane, 2 future, 3 unsent hole, 4 other:
/// lane key, highest range end, lowest range start, ranges, highest sent, floor.
pub const ACK_REJECT: u64 = 17;
/// The TX owner began applying an ACK: lane key, highest range end, lowest range start,
/// ranges, lane largest ACK after the update, history floor.
pub const ACK_BEGIN: u64 = 18;
/// A TAP source was refused admission; aux = 1 reader frame quota, 2 reader byte quota,
/// 3 owner input queue full, 4 owner buffer reserve, 5 owner frame quota, 6 owner byte quota:
/// lane key, retained frames, retained bytes, free buffers.
pub const ADMIT_DROP: u64 = 19;

/// The lane's next queued frame already passed its deadline; the expiry pass must remove it.
pub const HOLD_FRONT_EXPIRED: u64 = 1;
/// A batched frame passed its deadline; the expiry pass must remove it.
pub const HOLD_BATCH_EXPIRED: u64 = 2;
/// The transmit socket is waiting for EPOLLOUT.
pub const HOLD_SOCKET_BLOCKED: u64 = 3;
/// The lane has no usable path MTU.
pub const HOLD_NO_MTU: u64 = 4;
/// Planning failed and the batch was cleared.
pub const HOLD_PREPARE: u64 = 6;
/// A higher canceled reservation overtook the plan.
pub const HOLD_OVERTAKEN: u64 = 7;
/// Sent history cannot reserve another packet.
pub const HOLD_HISTORY: u64 = 8;
/// A retained plan cannot grow while another request owns the flight.
pub const HOLD_RETAINED: u64 = 11;

/// BLOCK reasons, in the order the owner checks them.
pub const BLOCK_PHASE: u64 = 1;
/// Pacing schedule has not reached the next send time.
pub const BLOCK_PACING: u64 = 2;
/// Not one packet fits the shared burst after outstanding reservations.
pub const BLOCK_BURST: u64 = 3;
/// Not one packet fits the congestion window after inflight and reservations.
pub const BLOCK_WINDOW: u64 = 4;
/// A stalled PTO closed ordinary grants.
pub const BLOCK_STALLED: u64 = 5;
/// Busy worker, sampling slots or another refusal.
pub const BLOCK_OTHER: u64 = 6;

struct Ring {
    head: AtomicU64,
    words: Box<[AtomicU64]>,
}

impl Ring {
    fn new() -> Self {
        Self {
            head: AtomicU64::new(0),
            words: (0..RECORDS * WORDS).map(|_| AtomicU64::new(0)).collect(),
        }
    }

    fn push(&self, record: Record) {
        let head = self.head.load(Ordering::Relaxed);
        let base = (head as usize % RECORDS) * WORDS;
        // A reader that observes these stores must also observe the preceding head.
        fence(Ordering::Release);
        for (slot, word) in self.words[base..base + WORDS].iter().zip(record) {
            slot.store(word, Ordering::Relaxed);
        }
        self.head.store(head + 1, Ordering::Release);
    }

    fn drain(&self, cursor: &mut u64, out: &mut Vec<Record>) -> u64 {
        let head = self.head.load(Ordering::Acquire);
        let start = (*cursor).max(head.saturating_sub(RECORDS as u64));
        let first = out.len();
        for index in start..head {
            let base = (index as usize % RECORDS) * WORDS;
            out.push(std::array::from_fn(|word| {
                self.words[base + word].load(Ordering::Relaxed)
            }));
        }
        fence(Ordering::Acquire);
        // Record i survives only while the writer has not started record i + RECORDS.
        let after = self.head.load(Ordering::Relaxed);
        let valid = after.saturating_sub(RECORDS as u64 - 1).clamp(start, head);
        out.drain(first..first + (valid - start) as usize);
        let lost = valid - *cursor;
        *cursor = head;
        lost
    }
}

static RINGS: OnceLock<Box<[Ring]>> = OnceLock::new();
// Last HOLD reason per worker, written only by that worker; REQUEST clears it.
static HELD: OnceLock<Box<[AtomicU64]>> = OnceLock::new();

/// Allocates one ring per worker for this process; later calls keep the first rings.
pub fn init(workers: usize) {
    RINGS.get_or_init(|| (0..workers).map(|_| Ring::new()).collect());
    HELD.get_or_init(|| (0..workers).map(|_| AtomicU64::new(0)).collect());
}

/// Records a HOLD reason once until the worker's next REQUEST or a different reason.
pub fn hold(worker: usize, reason: u64, words: [u64; 6]) {
    if let Some(last) = HELD.get().and_then(|held| held.get(worker))
        && last.swap(reason, Ordering::Relaxed) != reason
    {
        push(worker, HOLD, reason, words);
    }
}

/// Number of rings, or zero before init.
pub fn workers() -> usize {
    RINGS.get().map_or(0, |rings| rings.len())
}

/// Monotonic time for markers written outside workers.
pub fn now() -> Duration {
    clock(libc::CLOCK_MONOTONIC)
}

/// CLOCK_BOOTTIME, which /proc/uptime reports, for aligning harness snapshots.
pub fn boottime() -> Duration {
    clock(libc::CLOCK_BOOTTIME)
}

fn clock(id: libc::clockid_t) -> Duration {
    let mut value = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: value is writable; failure leaves zero, which the reader treats as unknown.
    unsafe { libc::clock_gettime(id, &mut value) };
    Duration::new(value.tv_sec as u64, value.tv_nsec as u32)
}

/// Appends one event; only worker `worker`'s thread may call this for its ring.
pub fn push(worker: usize, kind: u64, aux: u64, words: [u64; 6]) {
    if kind == REQUEST
        && let Some(last) = HELD.get().and_then(|held| held.get(worker))
    {
        last.store(0, Ordering::Relaxed);
    }
    if let Some(ring) = RINGS.get().and_then(|rings| rings.get(worker)) {
        ring.push(record(worker, kind, aux, now(), words));
    }
}

fn record(worker: usize, kind: u64, aux: u64, at: Duration, words: [u64; 6]) -> Record {
    let mut record = [0; WORDS];
    record[0] = kind | (worker as u64) << 8 | aux << 16;
    record[1] = at.as_nanos() as u64;
    record[2..].copy_from_slice(&words);
    record
}

/// Copies records after cursor into out and advances cursor; returns records lost.
pub fn drain(worker: usize, cursor: &mut u64, out: &mut Vec<Record>) -> u64 {
    RINGS
        .get()
        .and_then(|rings| rings.get(worker))
        .map_or(0, |ring| ring.drain(cursor, out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drain_keeps_order_and_reports_overwritten_records() {
        let ring = Ring::new();
        let push = |value| ring.push(record(1, GRANT, 7, Duration::ZERO, [value, 0, 0, 0, 0, 0]));
        let mut cursor = 0;
        let mut out = Vec::new();
        for value in 0..3 {
            push(value);
        }
        assert_eq!(ring.drain(&mut cursor, &mut out), 0);
        assert_eq!(out.iter().map(|r| r[2]).collect::<Vec<_>>(), [0, 1, 2]);
        assert_eq!(out[0][0], GRANT | 1 << 8 | 7 << 16);
        out.clear();
        for value in 0..RECORDS as u64 + 5 {
            push(value);
        }
        let lost = ring.drain(&mut cursor, &mut out);
        // The oldest surviving slot may be rewritten next, so it is discarded too.
        assert_eq!(lost, 6);
        assert_eq!(out.len(), RECORDS - 1);
        assert_eq!(out[0][2], 6);
        assert_eq!(out.last().unwrap()[2], RECORDS as u64 + 4);
        out.clear();
        assert_eq!(ring.drain(&mut cursor, &mut out), 0);
        assert!(out.is_empty());
    }
}
