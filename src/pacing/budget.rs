//! Shared send budget for the replacement sender in design section 9.
//!
//! The controller owner publishes limits and delivery state under a sequence
//! check; workers charge inflight and claim pacing credit with atomics and
//! record each transmission's delivery snapshot themselves. A per-ACK report of
//! the newest acknowledged record then yields the draft-07 rate sample that a
//! single chronological sampler would compute. Workers also mark draft-07's
//! application-limited bubble here when new data finds the group idle.
use crate::bbr::{LossSample, RateSample};
use std::{
    io,
    mem::size_of,
    num::NonZeroU16,
    sync::atomic::{AtomicU64, Ordering, fence},
    time::Duration,
};

/// Controller output and delivery state as of its latest feedback pass.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Published {
    /// Pacing rate in outer bytes per second; zero stops all sends.
    pub rate: u64,
    /// Congestion window in outer bytes; zero stops ordinary sends.
    pub window: u64,
    /// Largest pacing claim in outer bytes.
    pub quantum: u64,
    /// Bytes delivered so far.
    pub delivered: u64,
    /// Bytes declared lost so far.
    pub lost: u64,
    /// Time of the latest delivery.
    pub delivered_at: Duration,
    /// Send time of the newest acknowledged transmission.
    pub first_sent_at: Duration,
    /// Whether an application-limited bubble is in flight.
    pub app_limited: bool,
    /// ACK samples applied so far; orders idle restarts against later ACKs.
    pub acks: u64,
}

/// One successful transmission's delivery snapshot, kept by its lane owner.
///
/// Times are monotonic nanoseconds so a record fills one 64-byte line; each
/// worker keeps up to 65,535 of them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SendRecord {
    /// Group-wide send order from 1, for ties between equal timestamps.
    pub order: u64,
    /// Pre-syscall time of the successful attempt.
    pub sent_ns: u64,
    /// First send time of the delivery interval this transmission joined.
    pub first_sent_ns: u64,
    /// Delivery time at send.
    pub delivered_ns: u64,
    /// Bytes delivered at send.
    pub delivered: u64,
    /// Bytes lost at send.
    pub lost: u64,
    /// Inflight bytes immediately after this transmission's own charge.
    pub in_flight: u64,
    /// Whether this transmission joined an application-limited bubble.
    pub app_limited: bool,
    /// Outer bytes, 100 through 9000.
    pub bytes: u16,
    /// Whether this was a path-MTU probe.
    pub pmtu: bool,
    /// Whether host queues limited this transmission, as provenance only.
    pub host_limited: bool,
}

const WORDS: usize = 9;

/// One path group's published state, inflight charge and pacing clock.
///
/// Sends that race another worker's idle restart or an ACK publication by a
/// few instructions may read the preceding state; sequential sends match the
/// chronological draft-07 sampler exactly.
pub struct Budget {
    publication: Publication,
    hot: Hot,
    rare: Rare,
}

// Readers poll the publication on every claim; keep send-time writes off its lines.
#[repr(align(64))]
struct Publication {
    version: AtomicU64,
    words: [AtomicU64; WORDS],
}

// Every send charges, paces, releases and orders here: one line moves per send.
#[repr(align(64))]
struct Hot {
    in_flight: AtomicU64,
    next_send: AtomicU64,
    order: AtomicU64,
}

// Read on each send, written only at idle restarts, PTO rounds and when a
// worker's unsent data starts or drains.
#[repr(align(64))]
struct Rare {
    restart_at: AtomicU64,
    restart_acks: AtomicU64,
    probe_epoch: AtomicU64,
    // One bit per worker holding unsent data for this group.
    unsent: AtomicU64,
    // Draft-07's C.app_limited: sends join the bubble until delivered passes it.
    bubble: AtomicU64,
}

impl Default for Budget {
    fn default() -> Self {
        Self::new()
    }
}

fn nanos(at: Duration) -> u64 {
    u64::try_from(at.as_nanos()).unwrap_or(u64::MAX)
}

impl Budget {
    /// Creates a closed budget: rate and window are zero until the first publication.
    pub fn new() -> Self {
        Self {
            publication: Publication {
                version: AtomicU64::new(0),
                words: std::array::from_fn(|_| AtomicU64::new(0)),
            },
            hot: Hot {
                in_flight: AtomicU64::new(0),
                next_send: AtomicU64::new(0),
                order: AtomicU64::new(1),
            },
            rare: Rare {
                restart_at: AtomicU64::new(0),
                restart_acks: AtomicU64::new(0),
                probe_epoch: AtomicU64::new(0),
                unsent: AtomicU64::new(0),
                bubble: AtomicU64::new(0),
            },
        }
    }

    /// Publishes the controller's state; only the controller owner may call this.
    pub fn publish(&self, state: &Published) {
        let version = self.publication.version.load(Ordering::Relaxed);
        self.publication
            .version
            .store(version.wrapping_add(1), Ordering::Relaxed);
        // Readers that see any new word must also see the odd version.
        fence(Ordering::Release);
        let values = [
            state.rate,
            state.window,
            state.quantum,
            state.delivered,
            state.lost,
            nanos(state.delivered_at),
            nanos(state.first_sent_at),
            u64::from(state.app_limited),
            state.acks,
        ];
        for (word, value) in self.publication.words.iter().zip(values) {
            word.store(value, Ordering::Relaxed);
        }
        self.publication
            .version
            .store(version.wrapping_add(2), Ordering::Release);
    }

    /// Reads one consistent publication without a lock, retrying a torn read.
    pub fn published(&self) -> Published {
        loop {
            let before = self.publication.version.load(Ordering::Acquire);
            let values: [u64; WORDS] =
                std::array::from_fn(|index| self.publication.words[index].load(Ordering::Relaxed));
            fence(Ordering::Acquire);
            if before.is_multiple_of(2)
                && self.publication.version.load(Ordering::Relaxed) == before
            {
                return Published {
                    rate: values[0],
                    window: values[1],
                    quantum: values[2],
                    delivered: values[3],
                    lost: values[4],
                    delivered_at: Duration::from_nanos(values[5]),
                    first_sent_at: Duration::from_nanos(values[6]),
                    app_limited: values[7] != 0,
                    acks: values[8],
                };
            }
            std::hint::spin_loop();
        }
    }

    /// Current inflight charge.
    pub fn in_flight(&self) -> u64 {
        self.hot.in_flight.load(Ordering::Acquire)
    }

    /// Charges bytes if inflight stays within window; None is unbounded (PTO probes).
    /// Returns inflight before this charge.
    pub fn charge(&self, bytes: u64, window: Option<u64>) -> Option<u64> {
        let mut current = self.hot.in_flight.load(Ordering::Relaxed);
        loop {
            let after = current.checked_add(bytes)?;
            if window.is_some_and(|window| after > window) {
                return None;
            }
            match self.hot.in_flight.compare_exchange_weak(
                current,
                after,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(prior) => return Some(prior),
                Err(actual) => current = actual,
            }
        }
    }

    /// Releases bytes that were never sent, or whose ACK or loss was settled.
    pub fn release(&self, bytes: u64) {
        // Saturation keeps a bookkeeping error from wrapping the shared charge.
        let mut current = self.hot.in_flight.load(Ordering::Relaxed);
        while let Err(actual) = self.hot.in_flight.compare_exchange_weak(
            current,
            current.saturating_sub(bytes),
            Ordering::AcqRel,
            Ordering::Relaxed,
        ) {
            current = actual;
        }
    }

    /// Claims pacing credit for bytes at rate, or returns the earliest retry time.
    /// Like the grant ledger, a claim starts the next interval at now.
    pub fn pace(&self, bytes: u64, rate: u64, now: Duration) -> Result<(), Duration> {
        if rate == 0 {
            return Err(Duration::MAX);
        }
        let now = nanos(now);
        let cost = u64::try_from((u128::from(bytes) * 1_000_000_000).div_ceil(u128::from(rate)))
            .unwrap_or(u64::MAX);
        let mut next = self.hot.next_send.load(Ordering::Relaxed);
        loop {
            if now < next {
                return Err(Duration::from_nanos(next));
            }
            match self.hot.next_send.compare_exchange_weak(
                next,
                now.saturating_add(cost),
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Ok(()),
                Err(actual) => next = actual,
            }
        }
    }

    /// Starts the records of one syscall's `count` successful transmissions,
    /// charged together after `prior` inflight bytes and sent at `at`.
    ///
    /// The first send after inflight reaches zero restarts the delivery interval
    /// at its own time, as draft-07 does. Later sends use that restart until the
    /// controller publishes another ACK, which supersedes it as in the draft.
    pub fn begin(&self, state: &Published, prior: u64, count: u64, at: Duration) -> Batch {
        let (first_sent_at, delivered_at) = if prior == 0 {
            // Restarts and ACK counts only grow, so the newest restart wins both maxima.
            self.rare.restart_at.fetch_max(nanos(at), Ordering::AcqRel);
            self.rare
                .restart_acks
                .fetch_max(state.acks, Ordering::AcqRel);
            (at, at)
        } else if self.rare.restart_acks.load(Ordering::Acquire) >= state.acks
            && self.rare.restart_at.load(Ordering::Acquire) != 0
        {
            let restart = Duration::from_nanos(self.rare.restart_at.load(Ordering::Acquire));
            (restart, restart)
        } else {
            (state.first_sent_at, state.delivered_at)
        };
        Batch {
            order: self.hot.order.fetch_add(count, Ordering::AcqRel),
            sent_at: at,
            first_sent_at,
            delivered_at,
            delivered: state.delivered,
            lost: state.lost,
            in_flight: prior,
            app_limited: state.app_limited || self.app_limited(state.delivered),
        }
    }

    /// Notes that `worker`, below 64, now holds unsent data for this group.
    /// True when no worker held any before, the draft's first condition for
    /// an application-limited bubble.
    pub fn supply(&self, worker: usize) -> bool {
        self.rare.unsent.fetch_or(1 << worker, Ordering::AcqRel) == 0
    }

    /// Notes that `worker` sent or discarded all its unsent data for this group.
    pub fn drained(&self, worker: usize) {
        self.rare.unsent.fetch_and(!(1 << worker), Ordering::AcqRel);
    }

    /// Starts draft-07's application-limited bubble at delivered plus inflight,
    /// unless inflight already fills the window. The caller has checked that
    /// nothing was unsent and no loss awaits repair.
    pub fn application_limited(&self, state: &Published) -> bool {
        let in_flight = self.in_flight();
        if in_flight >= state.window {
            return false;
        }
        let end = state.delivered.saturating_add(in_flight).max(1);
        self.rare.bubble.store(end, Ordering::Release);
        true
    }

    /// Whether the application-limited bubble covers a send after `delivered` bytes.
    pub fn app_limited(&self, delivered: u64) -> bool {
        let end = self.rare.bubble.load(Ordering::Acquire);
        end != 0 && delivered <= end
    }

    /// Moves the pacing clock so claims before `until` wait; zero releases it.
    #[cfg(test)]
    pub(crate) fn hold_pacing(&self, until: Duration) {
        self.hot.next_send.store(nanos(until), Ordering::Release);
    }

    /// Publishes the PTO round whose probes workers may still send.
    pub fn set_probe_epoch(&self, epoch: u64) {
        self.rare.probe_epoch.store(epoch, Ordering::Release);
    }

    /// Current PTO round; a probe command from another round is stale.
    pub fn probe_epoch(&self) -> u64 {
        self.rare.probe_epoch.load(Ordering::Acquire)
    }
}

/// Shared delivery state for one syscall's successful prefix.
#[derive(Clone, Copy, Debug)]
pub struct Batch {
    order: u64,
    sent_at: Duration,
    first_sent_at: Duration,
    delivered_at: Duration,
    delivered: u64,
    lost: u64,
    in_flight: u64,
    app_limited: bool,
}

impl Batch {
    /// Records the next successful transmission, in send order.
    pub fn push(&mut self, bytes: u16, pmtu: bool, host_limited: bool) -> SendRecord {
        self.in_flight = self.in_flight.saturating_add(u64::from(bytes));
        let record = SendRecord {
            order: self.order,
            sent_ns: nanos(self.sent_at),
            first_sent_ns: nanos(self.first_sent_at),
            delivered_ns: nanos(self.delivered_at),
            delivered: self.delivered,
            lost: self.lost,
            in_flight: self.in_flight,
            app_limited: self.app_limited,
            bytes,
            pmtu,
            host_limited,
        };
        self.order = self.order.wrapping_add(1);
        record
    }
}

const LIVE: u32 = u32::MAX;

/// One worker's send records, addressed by the nonzero handles its history keeps.
///
/// Startup allocates every slot; taking and releasing allocate nothing.
pub struct Records {
    records: Box<[SendRecord]>,
    next: Box<[u32]>,
    free: u32,
    available: usize,
}

impl Records {
    /// Allocates 1..=65,535 records with every handle free.
    pub fn new(capacity: usize) -> io::Result<Self> {
        if !(1..=usize::from(u16::MAX)).contains(&capacity) {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let mut records = Vec::new();
        records
            .try_reserve_exact(capacity)
            .map_err(io::Error::other)?;
        records.resize(capacity, SendRecord::default());
        let mut next = Vec::new();
        next.try_reserve_exact(capacity).map_err(io::Error::other)?;
        // Handle h is index h - 1; each free slot links to the following handle, zero ends.
        next.extend((2..=capacity as u32).chain([0]));
        Ok(Self {
            records: records.into_boxed_slice(),
            next: next.into_boxed_slice(),
            free: 1,
            available: capacity,
        })
    }

    /// Reserved bytes for capacity records, excluding allocator overhead.
    pub fn storage_bytes(capacity: usize) -> Option<usize> {
        capacity
            .checked_mul(size_of::<SendRecord>() + size_of::<u32>())?
            .checked_add(size_of::<Self>())
    }

    /// Free handles.
    pub const fn available(&self) -> usize {
        self.available
    }

    /// Takes a free handle; its record stays default until written.
    pub fn take(&mut self) -> Option<NonZeroU16> {
        let handle = NonZeroU16::new(u16::try_from(self.free).ok()?)?;
        let index = usize::from(handle.get()) - 1;
        self.free = self.next[index];
        self.next[index] = LIVE;
        self.records[index] = SendRecord::default();
        self.available -= 1;
        Some(handle)
    }

    /// Returns a live handle; a free or unknown handle changes nothing.
    pub fn release(&mut self, handle: NonZeroU16) -> bool {
        let index = usize::from(handle.get()) - 1;
        if self.next.get(index) != Some(&LIVE) {
            return false;
        }
        self.next[index] = self.free;
        self.free = u32::from(handle.get());
        self.available += 1;
        true
    }

    /// Stores a live handle's record.
    pub fn set(&mut self, handle: NonZeroU16, record: SendRecord) -> bool {
        let index = usize::from(handle.get()) - 1;
        if self.next.get(index) != Some(&LIVE) {
            return false;
        }
        self.records[index] = record;
        true
    }

    /// A live handle's record.
    pub fn get(&self, handle: NonZeroU16) -> Option<SendRecord> {
        let index = usize::from(handle.get()) - 1;
        (self.next.get(index) == Some(&LIVE)).then(|| self.records[index])
    }
}

/// Controller-side delivery state fed by per-ACK reports from lane owners.
#[derive(Clone, Copy, Debug, Default)]
pub struct Sampler {
    delivered: u64,
    lost: u64,
    delivered_at: Duration,
    first_sent_at: Duration,
    app_limited: Option<u64>,
    acks: u64,
}

impl Sampler {
    /// State to publish with the controller's limits.
    pub const fn published(&self, rate: u64, window: u64, quantum: u64) -> Published {
        Published {
            rate,
            window,
            quantum,
            delivered: self.delivered,
            lost: self.lost,
            delivered_at: self.delivered_at,
            first_sent_at: self.first_sent_at,
            app_limited: self.app_limited.is_some(),
            acks: self.acks,
        }
    }

    /// Whether an application-limited bubble is still in flight.
    pub const fn is_app_limited(&self) -> bool {
        self.app_limited.is_some()
    }

    /// Marks the draft's application-limited bubble at delivered + inflight.
    pub fn application_limited(&mut self, in_flight: u64) {
        self.app_limited = Some(self.delivered.saturating_add(in_flight).max(1));
    }

    /// Applies one ACK: newly acknowledged bytes and its newest transmission's record.
    ///
    /// newest is the latest-sent of the packets this ACK newly acknowledges; the
    /// lane owner has already removed duplicates. at is owner observation time.
    pub fn ack(
        &mut self,
        at: Duration,
        newly_acked: u64,
        newest: &SendRecord,
        pmtu_only: bool,
        host_limited: bool,
        min_rtt: Duration,
    ) -> RateSample {
        let sent_at = Duration::from_nanos(newest.sent_ns);
        self.delivered = self.delivered.saturating_add(newly_acked);
        self.delivered_at = at;
        self.first_sent_at = sent_at;
        self.acks = self.acks.wrapping_add(1);
        if self.app_limited.is_some_and(|end| self.delivered > end) {
            self.app_limited = None;
        }
        let interval = sent_at
            .saturating_sub(Duration::from_nanos(newest.first_sent_ns))
            .max(at.saturating_sub(Duration::from_nanos(newest.delivered_ns)));
        let delivered = self.delivered.saturating_sub(newest.delivered);
        let rate = if interval.is_zero() || interval < min_rtt {
            None
        } else {
            u64::try_from(u128::from(delivered) * 1_000_000_000 / interval.as_nanos()).ok()
        };
        RateSample {
            rate,
            interval,
            delivered,
            newly_acked,
            prior_delivered: newest.delivered,
            lost: self.lost.saturating_sub(newest.lost),
            tx_in_flight: newest.in_flight,
            app_limited: newest.app_limited,
            host_limited: newest.host_limited || host_limited,
            pmtu_only,
            last_packet: newest.order,
        }
    }

    /// Applies one newly declared loss of a transmission not yet acknowledged.
    pub fn lost(&mut self, record: &SendRecord) -> LossSample {
        self.lost = self.lost.saturating_add(u64::from(record.bytes));
        LossSample {
            bytes: record.bytes,
            total_lost: self.lost,
            lost: self.lost.saturating_sub(record.lost),
            tx_in_flight: record.in_flight,
            app_limited: record.app_limited,
        }
    }
}

#[cfg(test)]
mod tests;
