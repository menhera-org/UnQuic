//! One path group's reservations; the owner supplies BBR outputs and monotonic time.
use std::{io, mem::size_of, time::Duration};

const LIFETIME: Duration = Duration::from_micros(100);

/// Current controller limits, in outer IPv6 bytes, for one sending path group.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Congestion window. Zero stops ordinary grants without discarding charges.
    pub window: u64,
    /// Pacing rate in bytes per second. Zero stops all grants, including probes.
    pub rate: u64,
    /// BBR's send quantum; outstanding grants share this byte budget.
    pub quantum: u32,
    /// Effective maximum outer datagram size, from 1280 through 9000 bytes.
    pub max_datagram: u16,
}

impl Limits {
    fn burst(self) -> Result<u64, GrantError> {
        if !(1280..=9000).contains(&self.max_datagram) {
            return Err(GrantError::Invalid);
        }
        Ok(u64::from(
            self.quantum.min(64 * u32::from(self.max_datagram)),
        ))
    }
}

/// An opaque permission for one worker to submit a bounded batch.
///
/// Route it with its path-group identity. It is valid only in the issuing
/// group; the worker must reserve report capacity before using it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Grant {
    pub(super) id: u64,
    worker: usize,
    bytes: u32,
    issued: Duration,
    expires: Duration,
    probe_epoch: Option<u64>,
}

impl Grant {
    /// Maximum successful physical transmission bytes, including IP/UDP headers.
    pub const fn bytes(self) -> u32 {
        self.bytes
    }

    /// Exclusive end of this permission's 100-microsecond send interval.
    pub const fn expires(self) -> Duration {
        self.expires
    }

    /// Whether a nonblocking submission may start at this supplied timestamp.
    /// Sample time immediately before the syscall, not at the start of a worker turn.
    pub fn valid_at(self, now: Duration) -> bool {
        self.issued <= now && now < self.expires
    }

    /// A probe grant permits exactly one datagram, consuming one PTO allowance.
    pub const fn is_probe(self) -> bool {
        self.probe_epoch.is_some()
    }
}

/// A rejected grant operation; no rejection changes accounting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrantError {
    /// Invalid worker, size, controller limit or sent/unused report.
    Invalid,
    /// Pacing, window, burst, probe allowance or a pending worker report blocks it.
    Blocked,
    /// The report is a duplicate or names a replaced grant.
    Stale,
    /// A unique identity or timestamp cannot be represented.
    Exhausted,
}

impl std::fmt::Display for GrantError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Invalid => "invalid transmission grant operation",
            Self::Blocked => "transmission grant is blocked",
            Self::Stale => "stale transmission grant report",
            Self::Exhausted => "transmission grant identity or timestamp exhausted",
        })
    }
}

impl std::error::Error for GrantError {}

/// Fixed-capacity, allocation-free grant accounting for one shared path owner.
///
/// Each worker may hold one grant in this group. Reservation precedes handoff;
/// expiry never releases credit. A sent/unused report consumes the grant once,
/// converts successful bytes to inflight charges, and returns only unused bytes.
/// Packet ACK/loss deduplication must precede settling those charges.
///
/// Send/unused reports must precede ACK/loss reports for their transmissions.
/// A full report queue stops sends, not accounting. Keep this object through
/// lane/session replacement while its path group remains unchanged.
pub struct Grants {
    workers: Box<[Option<Grant>]>,
    next_id: u64,
    reserved: u64,
    in_flight: u64,
    next_send: Duration,
    probe_epoch: u64,
    probes: u8,
}

impl Grants {
    /// Fallibly reserves one report slot per worker; v1 supports 1..=64 workers.
    pub fn new(workers: usize) -> io::Result<Self> {
        if !(1..=64).contains(&workers) {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let mut slots = Vec::new();
        slots.try_reserve_exact(workers).map_err(io::Error::other)?;
        slots.resize(workers, None);
        Ok(Self {
            workers: slots.into_boxed_slice(),
            next_id: 1,
            reserved: 0,
            in_flight: 0,
            next_send: Duration::ZERO,
            probe_epoch: 0,
            probes: 0,
        })
    }

    /// Reserved storage, including inline state but excluding allocator overhead.
    pub fn storage_bytes(workers: usize) -> Option<usize> {
        (1..=64).contains(&workers).then_some(())?;
        size_of::<Option<Grant>>()
            .checked_mul(workers)?
            .checked_add(size_of::<Self>())
    }

    /// Credit held by grants, including expired grants awaiting worker reports.
    pub const fn reserved(&self) -> u64 {
        self.reserved
    }

    /// Submitted outer bytes not yet settled by an ACK or loss report.
    pub const fn in_flight(&self) -> u64 {
        self.in_flight
    }

    /// Earliest pacing retry, if pacing alone blocks a request at now.
    ///
    /// The owner arms this only while work is waiting. Other blocked requests
    /// require a worker report, ACK/loss settlement or new controller limits.
    pub fn deadline(&self, now: Duration) -> Option<Duration> {
        (now < self.next_send).then_some(self.next_send)
    }

    /// Replaces unissued PTO allowance with zero, one or two aggregate probes.
    ///
    /// Call once per PTO expiry; use zero on fresh qualifying feedback. Old
    /// grants still need reports, but their unused probes cannot refill this
    /// new round. Ordinary grants may be stopped with Limits::window = 0.
    pub fn probe_allowance(&mut self, count: u8) -> Result<(), GrantError> {
        if count > 2 {
            return Err(GrantError::Invalid);
        }
        let epoch = self
            .probe_epoch
            .checked_add(1)
            .ok_or(GrantError::Exhausted)?;
        self.probe_epoch = epoch;
        self.probes = count;
        Ok(())
    }

    /// Reserves an exact byte budget before handing it to a worker.
    ///
    /// Requests share min(send quantum, 64 maximum datagrams), even across
    /// workers. Probes bypass only the congestion window: they still consume
    /// pacing/burst credit and become charged inflight bytes on submission.
    /// A probe grant describes one complete datagram, up to the 9000-byte
    /// protocol ceiling. PMTU probes may exceed the current maximum datagram
    /// estimate, but must fit the shared burst budget; do not inflate that
    /// estimate to authorize an unconfirmed probe.
    ///
    /// Pacing charges reserved bytes conservatively; unused reports do not
    /// rewind the schedule or create a catch-up burst after a delayed worker.
    pub fn reserve(
        &mut self,
        worker: usize,
        bytes: u32,
        probe: bool,
        limits: Limits,
        now: Duration,
    ) -> Result<Grant, GrantError> {
        let slot = self.workers.get(worker).ok_or(GrantError::Invalid)?;
        let burst = limits.burst()?;
        if bytes < 100 || (probe && bytes > 9000) {
            return Err(GrantError::Invalid);
        }
        let reserved = self
            .reserved
            .checked_add(u64::from(bytes))
            .ok_or(GrantError::Blocked)?;
        let charged = self
            .in_flight
            .checked_add(reserved)
            .ok_or(GrantError::Blocked)?;
        if slot.is_some()
            || limits.rate == 0
            || now < self.next_send
            || reserved > burst
            || (probe && self.probes == 0)
            || (!probe && charged > limits.window)
        {
            return Err(GrantError::Blocked);
        }
        let next_id = self.next_id.checked_add(1).ok_or(GrantError::Exhausted)?;
        let expires = now.checked_add(LIFETIME).ok_or(GrantError::Exhausted)?;
        let nanos = (u64::from(bytes) * 1_000_000_000).div_ceil(limits.rate);
        let next_send = now
            .checked_add(Duration::from_nanos(nanos))
            .ok_or(GrantError::Exhausted)?;
        let grant = Grant {
            id: self.next_id,
            worker,
            bytes,
            issued: now,
            expires,
            probe_epoch: probe.then_some(self.probe_epoch),
        };
        self.workers[worker] = Some(grant);
        self.next_id = next_id;
        self.reserved = reserved;
        self.next_send = next_send;
        if probe {
            self.probes -= 1;
        }
        Ok(grant)
    }

    /// Consumes one complete sent/unused report, possibly long after grant expiry.
    ///
    /// Sent bytes sum successful physical transmissions only. A partial syscall
    /// reports its successful prefix; report the remainder as unused by passing
    /// fewer bytes. Positive bytes require the last syscall's start timestamp
    /// within the grant interval; zero bytes require None. A call may return
    /// after expiry; retain its pre-syscall time for successful packets in history.
    /// A probe reports either zero or the full datagram. Stale reports do nothing.
    pub fn reported(
        &mut self,
        grant: Grant,
        sent_bytes: u32,
        last_started: Option<Duration>,
    ) -> Result<(), GrantError> {
        if self.workers.get(grant.worker).copied().flatten() != Some(grant) {
            return Err(GrantError::Stale);
        }
        if sent_bytes > grant.bytes
            || (grant.is_probe() && sent_bytes != 0 && sent_bytes != grant.bytes)
            || match last_started {
                Some(at) => sent_bytes == 0 || !grant.valid_at(at),
                None => sent_bytes != 0,
            }
        {
            return Err(GrantError::Invalid);
        }
        self.workers[grant.worker] = None;
        self.reserved -= u64::from(grant.bytes);
        self.in_flight += u64::from(sent_bytes);
        if sent_bytes == 0 && grant.probe_epoch == Some(self.probe_epoch) {
            self.probes += 1;
        }
        Ok(())
    }

    /// Releases already-deduplicated successful transmissions exactly once.
    ///
    /// A late ACK after loss settles zero bytes. Payload expiry settles nothing.
    /// The packet history remains authoritative; this aggregate cannot detect
    /// a duplicate settlement smaller than its outstanding total.
    pub fn settled(&mut self, bytes: u64) -> Result<(), GrantError> {
        self.in_flight = self
            .in_flight
            .checked_sub(bytes)
            .ok_or(GrantError::Invalid)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_and_credit_exhaustion_do_not_wrap_or_modify_reservations() {
        let mut grants = Grants::new(1).unwrap();
        let limits = Limits {
            window: u64::MAX,
            rate: 1_000_000,
            quantum: 1280,
            max_datagram: 1280,
        };
        grants.next_id = u64::MAX;
        assert_eq!(
            grants.reserve(0, 100, false, limits, Duration::ZERO),
            Err(GrantError::Exhausted)
        );
        assert_eq!(grants.reserved(), 0);
        assert_eq!(grants.workers[0], None);
        grants.next_id = 1;
        grants.in_flight = u64::MAX - 50;
        assert_eq!(
            grants.reserve(0, 100, false, limits, Duration::ZERO),
            Err(GrantError::Blocked)
        );
        assert_eq!(grants.reserved(), 0);
        assert_eq!(grants.in_flight(), u64::MAX - 50);
        grants.probes = 1;
        grants.probe_epoch = u64::MAX;
        assert_eq!(grants.probe_allowance(2), Err(GrantError::Exhausted));
        assert_eq!(grants.probes, 1);
    }
}
