//! Lock-free transport statistics, with no allocation or formatting on updates.
//!
//! Keep separate sets for session, peer and lane owners. Publish worker-local
//! batch totals when possible; sharing a cache line among busy writers is costly.
//! Snapshots load each field independently and are not a transport-state barrier.

use std::sync::atomic::{AtomicU64, Ordering};

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
}

macro_rules! metrics {
    ($($(#[$doc:meta])* $name:ident),+ $(,)?) => {
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

        impl Counters {
            /// Creates a zeroed set without allocation.
            pub const fn new() -> Self { Self { $($name: Counter::new(),)+ } }

            /// Reads all fields without locking or allocating.
            pub fn snapshot(&self) -> Snapshot { Snapshot { $($name: self.$name.get(),)+ } }
        }
    };
}

metrics! {
    /// Ethernet bytes successfully written as complete frames to TAP.
    delivered_frame_bytes,
    /// Successfully submitted outer IPv6 bytes, including IPv6 and UDP headers.
    tx_outer_bytes,
    /// Received outer IPv6 bytes, including IPv6 and UDP headers.
    rx_outer_bytes,
    /// Repair DATA records successfully submitted.
    repairs,
    /// Received datagrams already covered by packet history.
    duplicate_packets,
    /// DATA pieces discarded because their frame or bytes were already accepted.
    duplicate_pieces,
    /// Frames discarded at their sender or receiver deadline.
    expired_frames,
    /// Entries rejected because a bounded queue was full.
    queue_drops,
    /// Currently retained incomplete or queued receive frames.
    reassembly_frames,
    /// Currently retained receive payload bytes.
    reassembly_bytes,
    /// Smoothed round-trip time in nanoseconds, zero before initialization.
    rtt_ns,
    /// Latest measured feedback delay in nanoseconds.
    feedback_delay_ns,
    /// Current pacing rate in outer IP bytes per second.
    pacing_bytes_per_second,
    /// Currently charged outer IP bytes in flight.
    bytes_in_flight,
    /// Current outer IP MTU in bytes, zero before lane initialization.
    path_mtu_bytes,
    /// Numeric PathMtuState value.
    path_mtu_state,
    /// Datagrams rejected by complete wire validation.
    malformed_packets,
    /// Datagrams carrying an unconfigured cookie.
    unknown_cookie_packets,
    /// Failed attempts to pin a worker to a CPU.
    affinity_failures,
    /// Operational socket errors, excluding WouldBlock and Interrupted.
    socket_errors,
    /// Operational TAP errors, excluding WouldBlock and Interrupted.
    tap_errors,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

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
