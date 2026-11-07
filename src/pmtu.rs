//! Bounded per-lane datagram MTU discovery, with no I/O or allocation.
//!
//! The owner supplies monotonic time, records only successful socket submissions,
//! validates ICMP quotes, and limits DATA to payload_limit(). A new socket/path
//! needs a new Path. Independent lanes never share confirmation state.
//! Periodic confirmation detects reductions until DATA-quoted PTB and transport
//! blackhole feedback are connected to discovery.

use crate::{BASE_UDP_PAYLOAD, counters::PathMtuState, socket::MAX_UDP_PAYLOAD};
use std::time::Duration;

const ATTEMPTS: usize = 3;
const INITIAL_TIMEOUT: Duration = Duration::from_secs(16);
const MIN_TIMEOUT: Duration = Duration::from_secs(1);
const RAISE_INTERVAL: Duration = Duration::from_secs(600);

/// Identity of a submitted probe, scoped across sessions sharing one socket.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProbeId {
    /// Owner's session index.
    pub session: usize,
    /// Local receive tag of the connection that sent the probe; never zero.
    pub tag: u128,
    /// Local transmit lane; never zero.
    pub lane: u16,
    /// That connection/lane's transmitted packet number; never zero.
    pub packet: u64,
}

#[derive(Clone, Copy)]
struct Sent {
    id: ProbeId,
    length: usize,
    at: Duration,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Base,
    Search,
    Confirm,
    Complete,
    Failed,
}

/// One transmit socket's confirmed size and at most three live probe identities.
pub struct Path {
    ceiling: usize,
    upper: usize,
    limit: usize,
    target: usize,
    phase: Phase,
    sent: [Sent; ATTEMPTS],
    count: usize,
    due: Duration,
    raise: Duration,
    timeout: Duration,
}

impl Path {
    /// Starts at the IPv6 baseline; rejects ceilings outside the supported bounds.
    pub fn new(ceiling: usize, now: Duration) -> Option<Self> {
        (BASE_UDP_PAYLOAD..=MAX_UDP_PAYLOAD)
            .contains(&ceiling)
            .then_some(Self {
                ceiling,
                upper: ceiling,
                limit: BASE_UDP_PAYLOAD,
                target: BASE_UDP_PAYLOAD,
                phase: Phase::Base,
                sent: [Sent {
                    id: ProbeId {
                        session: 0,
                        tag: 0,
                        lane: 0,
                        packet: 0,
                    },
                    length: 0,
                    at: Duration::ZERO,
                }; ATTEMPTS],
                count: 0,
                due: now,
                raise: Duration::MAX,
                timeout: INITIAL_TIMEOUT,
            })
    }

    /// Maximum DATA payload, or zero while even the baseline is unconfirmed after failure.
    pub fn payload_limit(&self) -> usize {
        if self.phase == Phase::Failed {
            0
        } else {
            self.limit
        }
    }

    /// State for the lane's path-MTU gauge.
    pub fn state(&self) -> PathMtuState {
        match self.phase {
            Phase::Base => PathMtuState::Baseline,
            Phase::Failed => PathMtuState::Failed,
            Phase::Search => PathMtuState::Probing,
            Phase::Confirm | Phase::Complete if self.limit == BASE_UDP_PAYLOAD => {
                PathMtuState::Baseline
            }
            Phase::Confirm | Phase::Complete => PathMtuState::Confirmed,
        }
    }

    /// Next time the owner should call poll, subject to its bounded control budget.
    pub fn deadline(&self) -> Duration {
        self.due
    }

    /// Reissues confirmation when the connection carrying probes changes.
    /// Keeps a failed baseline blocked until a fresh probe succeeds.
    pub(crate) fn reconnect(&mut self) {
        self.count = 0;
        self.due = Duration::ZERO;
        if self.phase == Phase::Complete {
            self.phase = Phase::Confirm;
        }
    }

    /// Limits an upward probe after all prior proofs expire, or before its first submission.
    /// Confirmed DATA size, live proofs and the path ceiling remain unchanged.
    pub(crate) fn limit_probe(&mut self, max_payload: usize, now: Duration) -> bool {
        if self.phase != Phase::Search
            || self.live(now).next().is_some()
            || self.sent[..self.count].iter().any(|entry| now < entry.at)
            || max_payload <= self.limit
            || max_payload >= self.target
        {
            return false;
        }
        self.count = 0;
        self.target = max_payload;
        true
    }

    /// Applies expired probe timers and returns the next probe size, when due.
    /// Calling this does not claim that a packet was sent.
    pub fn poll(&mut self, now: Duration) -> Option<usize> {
        if now < self.due {
            return None;
        }
        if self.count == ATTEMPTS {
            if matches!(self.phase, Phase::Base | Phase::Failed) {
                self.phase = Phase::Failed;
                self.count = 0;
                self.due = now.saturating_add(self.timeout);
                return None;
            }
            self.black_hole(now);
        } else if self.phase == Phase::Search && self.count == ATTEMPTS - 1 {
            // Verify the working size before spending further probes on a narrower search.
            self.upper = self.target - 1;
            self.target = self.limit;
            self.phase = Phase::Confirm;
        } else if self.phase == Phase::Complete {
            if now >= self.raise && self.limit < self.ceiling {
                self.upper = self.ceiling;
                self.advance(now);
            } else {
                self.target = self.limit;
                self.phase = Phase::Confirm;
            }
        }
        Some(self.target)
    }

    /// Retains a proof only after the complete probe reached a successful send syscall.
    /// Invalid, early, duplicate or excess submissions leave state unchanged.
    pub fn submitted(&mut self, id: ProbeId, length: usize, now: Duration) -> bool {
        if id.tag == 0
            || id.lane == 0
            || id.packet == 0
            || length != self.target
            || now < self.due
            || self.phase == Phase::Complete
            || self.count == ATTEMPTS
            || self.sent[..self.count].iter().any(|entry| entry.id == id)
        {
            return false;
        }
        self.sent[self.count] = Sent {
            id,
            length,
            at: now,
        };
        self.count += 1;
        self.due = now.saturating_add(self.timeout);
        true
    }

    /// Defers an unsent probe after backpressure or an exhausted control budget.
    pub fn defer(&mut self, now: Duration) {
        self.due = self.due.max(now.saturating_add(Duration::from_millis(1)));
    }

    /// Live successful submissions available for validating quoted ICMP protocol headers.
    pub fn probes(&self, now: Duration) -> impl Iterator<Item = (ProbeId, usize)> + '_ {
        self.live(now).map(|entry| (entry.id, entry.length))
    }

    fn live(&self, now: Duration) -> impl Iterator<Item = &Sent> {
        let lifetime = self.timeout.saturating_mul(ATTEMPTS as u32);
        self.sent[..self.count]
            .iter()
            .filter(move |entry| now >= entry.at && now - entry.at < lifetime)
    }

    /// Accepts only an exact live reply, then searches upward or confirms the current size.
    /// Duplicate, stale, wrong-session/tag/number/length replies have no effect.
    pub fn reply(&mut self, id: ProbeId, length: usize, now: Duration) -> bool {
        let Some(at) = self
            .live(now)
            .find(|entry| entry.id == id && entry.length == length)
            .map(|entry| entry.at)
        else {
            return false;
        };
        // A measured path permits faster black-hole detection than the initial timeout.
        self.timeout = (now - at).saturating_mul(4).max(MIN_TIMEOUT);
        self.limit = self.limit.max(length);
        if length > self.upper {
            self.upper = self.ceiling;
        }
        self.count = 0;
        self.advance(now);
        true
    }

    /// Applies a PTB only after the owner matched its quote and destination to this probe.
    /// A size hint can lower the search bound but never confirms a new DATA size.
    pub fn packet_too_big(&mut self, id: ProbeId, outer_mtu: u32, now: Duration) -> bool {
        let Some(length) = self
            .live(now)
            .find(|entry| entry.id == id)
            .map(|entry| entry.length)
        else {
            return false;
        };
        self.local_too_big(length, outer_mtu, now)
    }

    /// Applies a kernel-local size error for a known send attempt.
    /// The owner must not pass an unvalidated remote ICMP report here.
    pub fn local_too_big(&mut self, attempted: usize, outer_mtu: u32, now: Duration) -> bool {
        let limit = outer_mtu.saturating_sub(48) as usize;
        if attempted == 0 || attempted > MAX_UDP_PAYLOAD || limit >= attempted {
            return false;
        }
        let failed = self.phase == Phase::Failed;
        self.upper = limit.min(if failed { self.ceiling } else { self.upper });
        self.count = 0;
        self.raise = Duration::MAX;
        self.due = now;
        if limit < BASE_UDP_PAYLOAD {
            self.phase = Phase::Failed;
            self.limit = BASE_UDP_PAYLOAD;
            self.target = BASE_UDP_PAYLOAD;
            self.due = now.saturating_add(self.timeout);
        } else if failed || limit < self.limit {
            self.limit = BASE_UDP_PAYLOAD;
            self.target = BASE_UDP_PAYLOAD;
            self.phase = if failed { Phase::Failed } else { Phase::Base };
        } else {
            self.target = self.upper;
            self.phase = if self.target > self.limit {
                Phase::Search
            } else {
                Phase::Confirm
            };
        }
        true
    }

    /// Restarts baseline confirmation after transport feedback indicates a black hole.
    /// Invalidates all old proofs immediately, including delayed replies to larger probes.
    pub fn black_hole(&mut self, now: Duration) {
        self.limit = BASE_UDP_PAYLOAD;
        self.target = BASE_UDP_PAYLOAD;
        self.upper = self.ceiling;
        self.phase = Phase::Base;
        self.count = 0;
        self.raise = Duration::MAX;
        self.due = now;
    }

    fn advance(&mut self, now: Duration) {
        if self.limit < self.upper {
            self.target = self.limit + (self.upper - self.limit).div_ceil(2);
            self.phase = Phase::Search;
            self.raise = Duration::MAX;
            self.due = now;
        } else {
            self.target = self.limit;
            self.phase = Phase::Complete;
            if self.raise == Duration::MAX {
                self.raise = now.saturating_add(RAISE_INTERVAL);
            }
            self.due = now.saturating_add(self.timeout);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RTT: Duration = Duration::from_millis(5);

    fn id(packet: u64) -> ProbeId {
        ProbeId {
            session: 7,
            tag: 11,
            lane: 3,
            packet,
        }
    }

    fn send(path: &mut Path, now: Duration, packet: u64) -> usize {
        let length = path.poll(now).expect("probe must be due");
        assert!(path.submitted(id(packet), length, now));
        length
    }

    fn complete(path: &mut Path, capacity: usize, now: &mut Duration, packet: &mut u64) {
        for _ in 0..256 {
            if path.phase == Phase::Complete {
                assert_eq!(path.payload_limit(), capacity.min(path.ceiling));
                return;
            }
            let length = send(path, *now, *packet);
            if length <= capacity {
                *now = now.saturating_add(RTT);
                assert!(path.reply(id(*packet), length, *now));
            } else {
                *now = path.deadline();
            }
            *packet += 1;
            assert!(path.payload_limit() <= capacity);
        }
        panic!("search did not converge");
    }

    #[test]
    fn every_supported_mtu_converges_without_icmp_or_unconfirmed_data_sizes() {
        for capacity in BASE_UDP_PAYLOAD..=MAX_UDP_PAYLOAD {
            let mut path = Path::new(MAX_UDP_PAYLOAD, Duration::ZERO).unwrap();
            complete(&mut path, capacity, &mut Duration::default(), &mut 1);
        }
        for ceiling in [BASE_UDP_PAYLOAD, 1452, 4096] {
            let mut path = Path::new(ceiling, Duration::ZERO).unwrap();
            complete(&mut path, MAX_UDP_PAYLOAD, &mut Duration::default(), &mut 1);
        }
        for invalid in [0, BASE_UDP_PAYLOAD - 1, MAX_UDP_PAYLOAD + 1, usize::MAX] {
            assert!(Path::new(invalid, Duration::ZERO).is_none());
        }
    }

    #[test]
    fn only_successful_exact_live_probes_confirm_sizes() {
        let mut path = Path::new(MAX_UDP_PAYLOAD, Duration::ZERO).unwrap();
        assert!(!path.reply(id(1), BASE_UDP_PAYLOAD, RTT));
        for invalid in [
            ProbeId { tag: 0, ..id(1) },
            ProbeId { lane: 0, ..id(1) },
            ProbeId { packet: 0, ..id(1) },
        ] {
            assert!(!path.submitted(invalid, BASE_UDP_PAYLOAD, Duration::ZERO));
        }
        assert!(!path.submitted(id(1), BASE_UDP_PAYLOAD + 1, Duration::ZERO));
        send(&mut path, Duration::ZERO, 1);
        assert!(!path.submitted(id(2), BASE_UDP_PAYLOAD, RTT));
        for forged in [
            ProbeId {
                session: 8,
                ..id(1)
            },
            ProbeId { tag: 12, ..id(1) },
            ProbeId { lane: 4, ..id(1) },
            id(2),
        ] {
            assert!(!path.reply(forged, BASE_UDP_PAYLOAD, RTT));
            assert!(!path.packet_too_big(forged, 1280, RTT));
        }
        assert!(!path.reply(id(1), BASE_UDP_PAYLOAD + 1, RTT));
        assert!(!path.packet_too_big(id(1), 1280, RTT));
        assert_eq!(path.deadline(), INITIAL_TIMEOUT);
        assert!(path.reply(id(1), BASE_UDP_PAYLOAD, RTT));
        assert_eq!(path.timeout, MIN_TIMEOUT);
        assert!(!path.reply(id(1), BASE_UDP_PAYLOAD, RTT));
        let length = send(&mut path, RTT, 2);
        assert!(length > BASE_UDP_PAYLOAD);
        assert!(!path.reply(id(2), length, Duration::ZERO));
        let stale = RTT + MIN_TIMEOUT * ATTEMPTS as u32;
        assert_eq!(path.probes(stale).count(), 0);
        assert!(!path.reply(id(2), length, stale));
        assert_eq!(path.payload_limit(), BASE_UDP_PAYLOAD);
    }

    #[test]
    fn reconnect_reprobes_without_reopening_failed_paths_or_accepting_old_proofs() {
        let mut path = Path::new(1452, Duration::ZERO).unwrap();
        let mut now = Duration::ZERO;
        let mut packet = 1;
        complete(&mut path, 1452, &mut now, &mut packet);
        let raise = path.raise;
        path.reconnect();
        assert_eq!(path.payload_limit(), 1452);
        assert_eq!(path.raise, raise);
        assert_eq!(send(&mut path, now, packet), 1452);
        assert!(path.local_too_big(1452, 1279, now));
        now = path.deadline();
        packet += 1;
        assert_eq!(send(&mut path, now, packet), BASE_UDP_PAYLOAD);
        let old = id(packet);
        path.reconnect();
        assert_eq!(path.payload_limit(), 0);
        assert_eq!(path.state(), PathMtuState::Failed);
        assert!(!path.reply(old, BASE_UDP_PAYLOAD, now + RTT));
        assert!(!path.packet_too_big(old, 1280, now + RTT));
        assert_eq!(path.poll(now), Some(BASE_UDP_PAYLOAD));
        let fresh = ProbeId { tag: 13, ..old };
        assert!(path.submitted(fresh, BASE_UDP_PAYLOAD, now));
        assert_eq!(path.payload_limit(), 0);
        // Lose the first reconnect probe: DATA stays blocked through its retry interval.
        let retry = path.deadline();
        assert!(retry > now + Duration::from_millis(100));
        assert_eq!(path.poll(now + Duration::from_millis(100)), None);
        assert_eq!(path.payload_limit(), 0);
        assert_eq!(path.poll(retry), Some(BASE_UDP_PAYLOAD));
        let next = ProbeId {
            packet: fresh.packet + 1,
            ..fresh
        };
        assert!(path.submitted(next, BASE_UDP_PAYLOAD, retry));
        assert_eq!(path.payload_limit(), 0);
        assert!(path.reply(next, BASE_UDP_PAYLOAD, retry + RTT));
        assert_eq!(path.payload_limit(), BASE_UDP_PAYLOAD);
    }

    #[test]
    fn loss_reordering_and_backpressure_keep_a_bounded_proof_history() {
        let mut path = Path::new(MAX_UDP_PAYLOAD, Duration::ZERO).unwrap();
        path.defer(Duration::ZERO);
        assert!(path.poll(Duration::ZERO).is_none());
        let mut now = path.deadline();
        send(&mut path, now, 1);
        now = path.deadline();
        send(&mut path, now, 2);
        assert!(path.reply(id(1), BASE_UDP_PAYLOAD, now + RTT));
        assert!(!path.reply(id(2), BASE_UDP_PAYLOAD, now + RTT));
        assert!(path.timeout >= INITIAL_TIMEOUT * 4);
        // Use the next fast reply to measure this path before testing two lost larger probes.
        now += RTT;
        let first = send(&mut path, now, 3);
        assert!(path.reply(id(3), first, now + RTT));
        now += RTT;
        let larger = send(&mut path, now, 4);
        now = path.deadline();
        assert_eq!(send(&mut path, now, 5), larger);
        now = path.deadline();
        assert_eq!(send(&mut path, now, 6), first);
        assert_eq!(path.probes(now).count(), 3);
        assert!(!path.submitted(id(7), first, now));
        // A delayed larger probe still proves its own size and reopens the upper search.
        assert!(path.reply(id(4), larger, now + RTT));
        assert_eq!(path.payload_limit(), larger);
        assert_eq!(path.upper, MAX_UDP_PAYLOAD);
        assert_eq!(path.probes(now + RTT).count(), 0);
    }

    #[test]
    fn validated_hints_reduce_immediately_but_never_confirm_and_late_proofs_stay_invalid() {
        let mut path = Path::new(MAX_UDP_PAYLOAD, Duration::ZERO).unwrap();
        let mut now = Duration::ZERO;
        let mut packet = 1;
        complete(&mut path, MAX_UDP_PAYLOAD, &mut now, &mut packet);
        now = path.deadline();
        let length = send(&mut path, now, packet);
        assert!(path.packet_too_big(id(packet), 1500, now + RTT));
        assert_eq!(path.payload_limit(), BASE_UDP_PAYLOAD);
        assert_eq!(path.upper, 1452);
        assert!(!path.reply(id(packet), length, now + RTT));
        packet += 1;
        now += RTT;
        complete(&mut path, 1452, &mut now, &mut packet);
        assert!(!path.local_too_big(1452, 1500, now));
        assert!(path.local_too_big(1452, 1279, now));
        assert_eq!(path.payload_limit(), 0);
        assert_eq!(path.state(), PathMtuState::Failed);
        assert!(path.local_too_big(MAX_UDP_PAYLOAD, 1500, now));
        assert_eq!(path.payload_limit(), 0);
        now = path.deadline();
        let length = send(&mut path, now, packet);
        assert_eq!(length, BASE_UDP_PAYLOAD);
        assert!(path.reply(id(packet), length, now + RTT));
        assert_eq!(path.payload_limit(), BASE_UDP_PAYLOAD);
        assert_eq!(path.upper, 1452);
    }

    #[test]
    fn blackholes_fail_at_baseline_and_raise_timer_survives_periodic_confirmation() {
        let mut path = Path::new(MAX_UDP_PAYLOAD, Duration::ZERO).unwrap();
        let mut now = Duration::ZERO;
        let mut packet = 1;
        complete(&mut path, 1452, &mut now, &mut packet);
        let raise = path.raise;
        while path.deadline() < raise {
            now = path.deadline();
            let length = send(&mut path, now, packet);
            assert_eq!(length, 1452);
            assert!(path.reply(id(packet), length, now + RTT));
            assert_eq!(path.raise, raise);
            packet += 1;
        }
        now = path.deadline();
        assert!(path.poll(now).is_some());
        assert!(path.phase == Phase::Search);
        complete(&mut path, MAX_UDP_PAYLOAD, &mut now, &mut packet);
        // Three consecutive failed confirmation probes force a baseline restart.
        for _ in 0..ATTEMPTS {
            now = path.deadline();
            assert_eq!(send(&mut path, now, packet), MAX_UDP_PAYLOAD);
            packet += 1;
        }
        now = path.deadline();
        assert_eq!(path.poll(now), Some(BASE_UDP_PAYLOAD));
        assert_eq!(path.payload_limit(), BASE_UDP_PAYLOAD);
        for _ in 0..ATTEMPTS {
            assert_eq!(send(&mut path, now, packet), BASE_UDP_PAYLOAD);
            packet += 1;
            now = path.deadline();
        }
        assert_eq!(path.poll(now), None);
        assert_eq!(path.payload_limit(), 0);
        assert_eq!(path.state(), PathMtuState::Failed);
    }

    #[test]
    fn lanes_are_independent_and_time_arithmetic_saturates() {
        let mut a = Path::new(MAX_UDP_PAYLOAD, Duration::ZERO).unwrap();
        let mut b = Path::new(MAX_UDP_PAYLOAD, Duration::ZERO).unwrap();
        complete(&mut a, 1452, &mut Duration::default(), &mut 1);
        complete(&mut b, MAX_UDP_PAYLOAD, &mut Duration::default(), &mut 1);
        a.black_hole(Duration::from_secs(100));
        assert_eq!(a.payload_limit(), BASE_UDP_PAYLOAD);
        assert_eq!(b.payload_limit(), MAX_UDP_PAYLOAD);
        let mut path = Path::new(MAX_UDP_PAYLOAD, Duration::MAX).unwrap();
        send(&mut path, Duration::MAX, u64::MAX);
        assert_eq!(path.deadline(), Duration::MAX);
        assert!(path.reply(id(u64::MAX), BASE_UDP_PAYLOAD, Duration::MAX));
        path.defer(Duration::MAX);
        assert_eq!(path.deadline(), Duration::MAX);
    }
}

#[cfg(test)]
mod grant_tests {
    use super::*;

    const RTT: Duration = Duration::from_millis(5);

    fn id(packet: u64) -> ProbeId {
        ProbeId {
            session: 1,
            tag: 1,
            lane: 1,
            packet,
        }
    }

    #[test]
    fn every_mtu_converges_with_two_mss_grants_with_or_without_ptb() {
        for ptb in [false, true] {
            for capacity in BASE_UDP_PAYLOAD..=MAX_UDP_PAYLOAD {
                let mut path = Path::new(MAX_UDP_PAYLOAD, Duration::ZERO).unwrap();
                let mut now = Duration::ZERO;
                for packet in 1..=256 {
                    let budget = 2 * (path.payload_limit() + 48) - 48;
                    let original_limit = path.payload_limit();
                    path.limit_probe(budget, now);
                    assert_eq!(path.payload_limit(), original_limit);
                    let length = path.poll(now).unwrap();
                    assert!(length <= budget);
                    assert!(path.submitted(id(packet), length, now));
                    if length <= capacity {
                        now += RTT;
                        assert!(path.reply(id(packet), length, now));
                    } else if ptb {
                        now += RTT;
                        assert!(path.packet_too_big(id(packet), (capacity + 48) as u32, now));
                    } else {
                        now = path.deadline();
                    }
                    assert!(path.payload_limit() <= capacity);
                    assert_eq!(path.ceiling, MAX_UDP_PAYLOAD);
                    if path.phase == Phase::Complete {
                        break;
                    }
                }
                assert!(path.phase == Phase::Complete);
                assert_eq!(path.payload_limit(), capacity);
            }
        }
    }

    #[test]
    fn grant_limit_changes_only_unsent_search_target() {
        let mut path = Path::new(MAX_UDP_PAYLOAD, Duration::ZERO).unwrap();
        assert!(!path.limit_probe(1500, Duration::ZERO));
        assert!(path.submitted(id(1), BASE_UDP_PAYLOAD, Duration::ZERO));
        assert!(path.reply(id(1), BASE_UDP_PAYLOAD, RTT));
        let deadline = path.deadline();
        let upper = path.upper;
        let target = path.target;
        for invalid in [0, BASE_UDP_PAYLOAD, target, usize::MAX] {
            assert!(!path.limit_probe(invalid, RTT));
            assert_eq!(path.target, target);
        }
        assert!(path.limit_probe(1500, RTT));
        assert_eq!(path.target, 1500);
        assert_eq!(path.upper, upper);
        assert_eq!(path.deadline(), deadline);
        assert_eq!(path.payload_limit(), BASE_UDP_PAYLOAD);
        assert!(path.submitted(id(2), 1500, RTT));
        assert!(!path.limit_probe(1400, RTT));
        assert_eq!(path.target, 1500);
        assert_eq!(path.probes(RTT).collect::<Vec<_>>(), [(id(2), 1500)]);
        assert!(path.reply(id(2), 1500, RTT * 2));
        path.black_hole(RTT * 3);
        assert!(!path.limit_probe(1400, RTT * 3));
        assert_eq!(path.poll(RTT * 3), Some(BASE_UDP_PAYLOAD));
        assert!(!path.reply(id(2), 1500, RTT * 3));
        assert!(path.submitted(id(3), BASE_UDP_PAYLOAD, RTT * 3));
        assert!(path.reply(id(3), BASE_UDP_PAYLOAD, RTT * 4));
        assert!(path.limit_probe(1400, RTT * 4));
        assert_eq!(path.target, 1400);
    }
}

#[cfg(test)]
mod expiry_grant_tests {
    use super::*;

    #[test]
    fn smaller_probe_waits_for_all_proofs_to_expire_without_confirming_data() {
        let mut path = Path::new(MAX_UDP_PAYLOAD, Duration::ZERO).unwrap();
        let mut id = ProbeId {
            session: 1,
            tag: 1,
            lane: 1,
            packet: 1,
        };
        assert!(path.submitted(id, BASE_UDP_PAYLOAD, Duration::ZERO));
        let now = Duration::from_millis(5);
        assert!(path.reply(id, BASE_UDP_PAYLOAD, now));
        let target = path.poll(now).unwrap();
        id.packet += 1;
        assert!(path.submitted(id, target, now));
        let expiry = now + path.timeout * ATTEMPTS as u32;
        for at in [Duration::ZERO, now, expiry - Duration::from_nanos(1)] {
            assert!(!path.limit_probe(1500, at));
            assert_eq!(path.target, target);
            assert_eq!(path.count, 1);
        }
        let upper = path.upper;
        assert!(path.limit_probe(1500, expiry));
        assert_eq!(path.count, 0);
        assert_eq!(path.upper, upper);
        assert_eq!(path.payload_limit(), BASE_UDP_PAYLOAD);
        assert!(!path.reply(id, target, expiry));
        assert!(!path.packet_too_big(id, 1500, expiry));
        assert_eq!(path.poll(expiry), Some(1500));
        id.packet += 1;
        assert!(path.submitted(id, 1500, expiry));
        assert!(path.reply(id, 1500, expiry + Duration::from_millis(5)));
        assert_eq!(path.payload_limit(), 1500);
    }
}
