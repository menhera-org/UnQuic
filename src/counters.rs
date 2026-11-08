//! Lock-free transport statistics, with no allocation or formatting on updates.
//!
//! Keep separate sets for session, peer and lane owners. Publish worker-local
//! batch totals when possible; sharing a cache line among busy writers is costly.
//! Snapshots load each field independently and are not a transport-state barrier.

use std::sync::atomic::{AtomicU16, AtomicU64, Ordering, fence};

/// One atomic total or gauge. Updates are Relaxed; observations are Acquire.
#[derive(Debug, Default)]
pub struct Counter(AtomicU64);

impl Counter {
    /// Creates a zero total or gauge.
    pub const fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    /// Adds a batch total. Values wrap modulo 2^64, without blocking or allocation.
    #[inline]
    pub fn add(&self, value: u64) {
        self.0.fetch_add(value, Ordering::Relaxed);
    }

    /// Publishes an owner-computed gauge or cumulative batch total.
    ///
    /// A field using set must have one writer; do not mix set with concurrent add.
    #[inline]
    pub fn set(&self, value: u64) {
        self.0.store(value, Ordering::Relaxed);
    }

    /// Reads one independently sampled value.
    #[inline]
    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Acquire)
    }
}

/// Reported path-MTU state; stored as its numeric value in the corresponding gauge.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u64)]
pub enum PathMtuState {
    /// No lane has been initialized.
    #[default]
    Unknown = 0,
    /// The IPv6 baseline is in use.
    Baseline = 1,
    /// A larger payload is being probed.
    Probing = 2,
    /// A larger payload has been confirmed.
    Confirmed = 3,
    /// The path cannot support the baseline.
    Failed = 4,
    /// A larger ceiling was asserted by configuration, without path confirmation.
    Configured = 5,
}

macro_rules! metrics {
    ($($(#[$doc:meta])* $name:ident : $merge:ident),+ $(,)?) => {
        /// Atomic statistics for one session, peer or lane owner.
        ///
        /// Alignment prevents adjacent owner sets from sharing a 64-byte cache
        /// line. The runtime accounts for this storage within its memory ceiling.
        #[derive(Debug, Default)]
        #[repr(align(64))]
        pub struct Counters { $($(#[$doc])* pub $name: Counter,)+ }

        /// Independently sampled values; fields may represent different instants.
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
        pub struct Snapshot { $($(#[$doc])* pub $name: u64,)+ }

        impl Snapshot {
            pub(crate) fn merge(&mut self, other: Self) {
                $(self.$name = $merge(self.$name, other.$name);)+
            }
        }

        impl Counters {
            /// Creates a zeroed set without allocation.
            pub const fn new() -> Self { Self { $($name: Counter::new(),)+ } }

            /// Publishes cumulative totals from this set's exclusive worker owner.
            pub(crate) fn publish(&self, values: &Snapshot) {
                $(self.$name.set(values.$name);)+
            }

            /// Reads all fields without locking or allocating.
            pub fn snapshot(&self) -> Snapshot { Snapshot { $($name: self.$name.get(),)+ } }
        }
    };
}

metrics! {
    /// Ethernet bytes successfully written as complete frames to TAP.
    delivered_frame_bytes: sum,
    /// Successfully submitted outer IPv6 bytes, including IPv6 and UDP headers.
    tx_outer_bytes: sum,
    /// Received outer IPv6 bytes, including IPv6 and UDP headers.
    rx_outer_bytes: sum,
    /// Repair DATA records successfully submitted.
    repairs: sum,
    /// Received datagrams already covered by packet history.
    duplicate_packets: sum,
    /// DATA pieces discarded because their frame or bytes were already accepted.
    duplicate_pieces: sum,
    /// Frames discarded at their sender or receiver deadline.
    expired_frames: sum,
    /// Source children expired before materialization; a subset of expired_frames.
    tx_expired_input_frames: sum,
    /// Materialized frames expired before any successful DATA submission; a subset of expired_frames.
    tx_expired_unsent_frames: sum,
    /// Frames expired after submitting only a prefix; a subset of expired_frames.
    tx_expired_partial_frames: sum,
    /// Fully submitted frames expired before ACK completion; a subset of expired_frames.
    tx_expired_sent_frames: sum,
    /// Incomplete receive frames expired; a subset of expired_frames.
    rx_expired_incomplete_frames: sum,
    /// Completed receive frames expired while awaiting TAP; a subset of expired_frames.
    rx_expired_ready_frames: sum,
    /// Cleanup calls exhausting their budget with work remaining; worker totals only.
    recovery_cleanup_yields: sum,
    /// Maximum cleanup queue length, including mapping removal; worker gauge only.
    recovery_cleanup_peak: max,
    /// Reports whose packet mappings finished staging for shared accounting; worker totals only.
    recovery_staged_reports: sum,
    /// Staging calls exhausting their budget before report dispatch; worker totals only.
    recovery_staging_yields: sum,
    /// Granted DATA prefixes rejected for insufficient journal capacity; worker totals only.
    recovery_mapping_pressure: sum,
    /// Granted DATA prefixes rejected after selected coverage changed; worker totals only.
    recovery_selection_changed: sum,
    /// Entries rejected because a bounded queue was full.
    queue_drops: sum,
    /// TAP sources rejected by fresh-unsent byte admission; a subset of queue_drops.
    /// One GSO source can represent many Ethernet frames.
    tx_backlog_drops: sum,
    /// ACK records dropped for lack of a reserved receive buffer; a subset of queue_drops, not additive.
    ack_buffer_drops: sum,
    /// Datagrams not fully admitted because receive frame storage was full.
    admission_pressure: sum,
    /// Socket receive drops reported by SO_RXQ_OVFL, in kernel skb units.
    socket_overflow: sum,
    /// Receive sockets that fell back from SO_RCVBUFFORCE to SO_RCVBUF.
    receive_buffer_fallbacks: sum,
    /// Sum of receive-socket kernel accounting limits, including overhead allowance.
    receive_buffer_bytes: sum,
    /// Currently retained incomplete or queued receive frames.
    reassembly_frames: sum,
    /// Sum of full frame lengths reserved by retained receive slots.
    reassembly_bytes: sum,
    /// Smoothed round-trip time in nanoseconds, zero before initialization.
    rtt_ns: max,
    /// Latest measured feedback delay in nanoseconds.
    feedback_delay_ns: max,
    /// Current pacing rate in outer IP bytes per second.
    pacing_bytes_per_second: sum,
    /// Issued send permissions received by transmit workers.
    send_grants: sum,
    /// Send permissions rejected after their submission interval elapsed.
    expired_grants: sum,
    /// Send permissions canceled by a changed plan, path or connection.
    cancelled_grants: sum,
    /// Currently charged outer IP bytes in flight.
    bytes_in_flight: sum,
    /// Current outer IP size ceiling; consult path_mtu_state for its basis.
    path_mtu_bytes: min_nonzero,
    /// Numeric PathMtuState value.
    path_mtu_state: mtu_state,
    /// Datagrams rejected by wire validation or lane-specific admission checks.
    malformed_packets: sum,
    /// Datagrams carrying an unconfigured cookie.
    unknown_cookie_packets: sum,
    /// Failed attempts to pin a worker to a CPU.
    affinity_failures: sum,
    /// Operational socket errors, excluding WouldBlock and Interrupted.
    socket_errors: sum,
    /// TAP read/readv/io_uring_enter syscalls, including unsuccessful attempts.
    tap_read_calls: sum,
    /// Submitted TAP READV requests or plain read calls, including empty reads.
    tap_read_requests: sum,
    /// Bounded io_uring TAP read batches attempted.
    tap_read_batches: sum,
    /// Valid frames read through io_uring, before transmit admission.
    tap_ring_frames: sum,
    /// Partial Internet checksums completed before transmit admission.
    tap_checksums: sum,
    /// UDP super-frames retained from TAP reads.
    tap_gso_reads: sum,
    /// Complete wire frames produced from TAP super-frames.
    tap_gso_frames: sum,
    /// Workers without a read ring at startup, plus rings disabled after rejection.
    tap_read_fallbacks: sum,
    /// TAP frame write syscalls, including unsuccessful attempts and fallback retries.
    tap_write_calls: sum,
    /// Successful TAP USO writes representing two or more original frames.
    tap_uso_batches: sum,
    /// Original frames delivered in successful multi-frame TAP USO writes.
    tap_uso_frames: sum,
    /// TAP queues that disabled USO after a kernel capability rejection.
    tap_uso_fallbacks: sum,
    /// Frames discarded after permanent TAP write errors, plus TAP read errors.
    tap_errors: sum,
}

fn sum(a: u64, b: u64) -> u64 {
    a.wrapping_add(b)
}
fn max(a: u64, b: u64) -> u64 {
    a.max(b)
}
fn min_nonzero(a: u64, b: u64) -> u64 {
    if a == 0 {
        b
    } else if b == 0 {
        a
    } else {
        a.min(b)
    }
}
fn mtu_state(a: u64, b: u64) -> u64 {
    // Aggregate precedence: failed, probing, baseline, configured, confirmed, unknown.
    let rank = |state| match state {
        4 => 5,
        2 => 4,
        1 => 3,
        5 => 2,
        3 => 1,
        _ => 0,
    };
    if rank(a) >= rank(b) { a } else { b }
}

/// Metrics for one lane in a particular completed or installing connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LaneSnapshot {
    /// Nonzero wire lane ID.
    pub lane: u16,
    /// Local worker commit generation identifying this connection.
    pub generation: u64,
    /// Independently sampled totals and gauges for this lane lifetime.
    pub counters: Snapshot,
}

pub(crate) struct LaneCounters {
    version: AtomicU64,
    lane: AtomicU16,
    generation: AtomicU64,
    pub counters: Counters,
}

impl LaneCounters {
    pub const fn new() -> Self {
        Self {
            version: AtomicU64::new(0),
            lane: AtomicU16::new(0),
            generation: AtomicU64::new(0),
            counters: Counters::new(),
        }
    }

    pub fn replace(&self, lane: u16, generation: u64, counters: &Snapshot) {
        let previous = self.version.load(Ordering::Relaxed);
        assert!(previous <= u64::MAX - 2, "counter identity exhausted");
        self.version.fetch_add(1, Ordering::Relaxed);
        // A reader observing reset fields must also observe the odd identity version.
        fence(Ordering::Release);
        self.lane.store(lane, Ordering::Relaxed);
        self.generation.store(generation, Ordering::Relaxed);
        self.counters.publish(counters);
        self.version.store(previous + 2, Ordering::Release);
    }

    pub fn snapshot(&self) -> Option<LaneSnapshot> {
        let before = self.version.load(Ordering::Acquire);
        if before & 1 != 0 {
            return None;
        }
        let value = LaneSnapshot {
            lane: self.lane.load(Ordering::Relaxed),
            generation: self.generation.load(Ordering::Relaxed),
            counters: self.counters.snapshot(),
        };
        fence(Ordering::Acquire);
        (value.lane != 0 && before == self.version.load(Ordering::Relaxed)).then_some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn aggregation_preserves_units_and_mtu_precedence() {
        let mut total = Snapshot {
            tx_outer_bytes: u64::MAX,
            rtt_ns: 50,
            path_mtu_state: 3,
            path_mtu_bytes: 1500,
            ..Snapshot::default()
        };
        total.merge(Snapshot {
            path_mtu_state: PathMtuState::Configured as u64,
            ..Snapshot::default()
        });
        assert_eq!(total.path_mtu_state, 5);
        total.merge(Snapshot {
            path_mtu_state: PathMtuState::Confirmed as u64,
            ..Snapshot::default()
        });
        assert_eq!(total.path_mtu_state, 5);
        total.merge(Snapshot {
            tx_outer_bytes: 1,
            rtt_ns: 20,
            reassembly_bytes: 9000,
            path_mtu_state: 1,
            path_mtu_bytes: 1280,
            ..Snapshot::default()
        });
        assert_eq!(total.tx_outer_bytes, 0);
        assert_eq!(total.rtt_ns, 50);
        assert_eq!(total.reassembly_bytes, 9000);
        assert_eq!(total.path_mtu_bytes, 1280);
        assert_eq!(total.path_mtu_state, 1);
        for state in [2, 4] {
            total.merge(Snapshot {
                path_mtu_state: state,
                ..Snapshot::default()
            });
            total.merge(Snapshot::default());
            assert_eq!(total.path_mtu_state, state);
            assert_eq!(total.path_mtu_bytes, 1280);
        }
    }

    #[test]
    fn lane_reuse_never_reports_metrics_from_another_identity() {
        let counters = LaneCounters::new();
        assert!(counters.snapshot().is_none());
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for generation in 1..10_000 {
                    counters.replace(
                        7,
                        generation,
                        &Snapshot {
                            tx_outer_bytes: generation,
                            ..Snapshot::default()
                        },
                    );
                }
            });
            for _ in 0..10_000 {
                if let Some(snapshot) = counters.snapshot() {
                    assert_eq!(snapshot.lane, 7);
                    assert_eq!(snapshot.counters.tx_outer_bytes, snapshot.generation);
                }
            }
        });
        counters.replace(0, 0, &Snapshot::default());
        assert!(counters.snapshot().is_none());
    }

    #[test]
    fn concurrent_updates_are_not_lost_and_snapshots_preserve_units() {
        let counters = Arc::new(Counters::new());
        std::thread::scope(|scope| {
            for _ in 0..4 {
                let counters = Arc::clone(&counters);
                scope.spawn(move || {
                    for _ in 0..10_000 {
                        counters.delivered_frame_bytes.add(1514);
                        counters.tx_outer_bytes.add(1280);
                    }
                });
            }
        });
        counters.rtt_ns.set(1_500_000);
        counters.path_mtu_bytes.set(1280);
        counters.path_mtu_state.set(PathMtuState::Baseline as u64);
        let snapshot = counters.snapshot();
        assert_eq!(snapshot.delivered_frame_bytes, 40_000 * 1514);
        assert_eq!(snapshot.tx_outer_bytes, 40_000 * 1280);
        assert_eq!(snapshot.rx_outer_bytes, 0);
        assert_eq!(snapshot.rtt_ns, 1_500_000);
        assert_eq!(snapshot.path_mtu_bytes, 1280);
        assert_eq!(snapshot.path_mtu_state, PathMtuState::Baseline as u64);
        assert_eq!(std::mem::align_of::<Counters>(), 64);
        assert_eq!(std::mem::size_of::<Counters>() % 64, 0);
    }

    #[test]
    fn totals_wrap_and_gauges_replace_without_panicking() {
        let value = Counter::new();
        value.add(u64::MAX);
        value.add(1);
        assert_eq!(value.get(), 0);
        value.set(42);
        assert_eq!(value.get(), 42);
        assert_eq!(Counters::new().snapshot(), Snapshot::default());
    }
}
