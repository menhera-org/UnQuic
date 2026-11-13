//! Pure RFC 9002 timing for established lanes; callers supply monotonic time.
//!
//! ACK validation and sent-packet history must precede these calculations.
//! Packet thresholds apply within one lane. PTO backoff and probe credit belong
//! to the shared path-group owner, not to individual lanes.

mod repair;
pub use repair::{FrameRecovery, WIDE_FRAME_LEN, WidePlanes};

use std::time::Duration;

const INITIAL: Duration = Duration::from_millis(333);
const GRANULARITY: Duration = Duration::from_millis(1);
const MAX_ACK_DELAY: Duration = Duration::from_millis(1);
/// Longest backed-off PTO period. Probes keep the idle liveness cadence through
/// an outage, so a returning path is found within about a second.
pub const MAX_BACKED_OFF_PTO: Duration = Duration::from_secs(1);

/// RTT estimates for one path, without I/O, allocation or internal clock reads.
#[derive(Clone, Copy, Debug)]
pub struct Rtt {
    latest: Duration,
    minimum: Option<Duration>,
    smoothed: Duration,
    variance: Duration,
}

impl Default for Rtt {
    fn default() -> Self {
        Self::new()
    }
}

impl Rtt {
    /// Starts with the specified 333 ms RTT and half that value as variation.
    pub const fn new() -> Self {
        Self {
            latest: INITIAL,
            minimum: None,
            smoothed: INITIAL,
            variance: Duration::from_micros(166_500),
        }
    }

    /// Latest raw sample, or the initial estimate before feedback.
    pub const fn latest(&self) -> Duration {
        self.latest
    }

    /// Minimum raw sample, without subtracting peer-reported ACK delay.
    pub const fn minimum(&self) -> Option<Duration> {
        self.minimum
    }

    /// Smoothed RTT, with validated ACK delay removed when permitted.
    pub const fn smoothed(&self) -> Duration {
        self.smoothed
    }

    /// Smoothed absolute deviation used by the probe timeout.
    pub const fn variance(&self) -> Duration {
        self.variance
    }

    /// Samples the largest newly acknowledged packet's actual submission time.
    ///
    /// Call only after validating the entire ACK, when its largest packet is
    /// newly acknowledged and at least one newly acknowledged packet elicits
    /// ACKs. An ACK of only feedback packets must not update this estimator.
    /// Backward time returns false without changing state. Delay is capped at
    /// 1000 microseconds and ignored for the first sample.
    pub fn sample(&mut self, sent: Duration, now: Duration, delay_us: u32) -> bool {
        let Some(latest) = now.checked_sub(sent) else {
            return false;
        };
        self.latest = latest;
        let Some(minimum) = self.minimum else {
            self.minimum = Some(latest);
            self.smoothed = latest;
            self.variance = latest / 2;
            return true;
        };
        let minimum = minimum.min(latest);
        self.minimum = Some(minimum);
        let delay = Duration::from_micros(u64::from(delay_us)).min(MAX_ACK_DELAY);
        let adjusted = if latest - minimum >= delay {
            latest - delay
        } else {
            latest
        };
        // RFC 9002 erratum 7539: deviation uses the previous smoothed RTT.
        self.variance = weighted(self.variance, self.smoothed.abs_diff(adjusted), 3);
        self.smoothed = weighted(self.smoothed, adjusted, 7);
        true
    }

    /// Time threshold: 9/8 of the greater of latest and smoothed RTT, at least 1 ms.
    pub fn loss_delay(&self) -> Duration {
        let rtt = self.latest.max(self.smoothed);
        rtt.saturating_add(rtt / 8).max(GRANULARITY)
    }

    /// Deadline for an unresolved sent packet below this lane's largest ACK.
    ///
    /// A gap of at least three packet numbers makes it immediately eligible
    /// after submission. Otherwise use the time threshold. Zero identities,
    /// packets without a later ACK and unrepresentable deadlines return None.
    /// The caller must exclude packets already acknowledged or declared lost.
    pub fn loss_deadline(
        &self,
        packet: u64,
        largest_acked: u64,
        sent: Duration,
    ) -> Option<Duration> {
        if packet == 0 || packet >= largest_acked {
            return None;
        }
        if largest_acked - packet >= 3 {
            Some(sent)
        } else {
            sent.checked_add(self.loss_delay())
        }
    }

    /// Established-packet PTO period with the path group's consecutive backoff.
    ///
    /// Starts at srtt + max(4 * variation, 1 ms) + 1 ms and doubles per expiry,
    /// up to [`MAX_BACKED_OFF_PTO`] or the unbacked period if that is longer.
    /// Saturates at Duration::MAX; adding it to a timestamp must be checked.
    /// Expiry alone is not packet loss. The owner grants at most two probes,
    /// and applies the transport's separate three-PTO no-feedback policy.
    pub fn pto_period(&self, backoff: u32) -> Duration {
        let base = self
            .smoothed
            .saturating_add(self.variance.saturating_mul(4).max(GRANULARITY))
            .saturating_add(MAX_ACK_DELAY);
        let nanos = 1_u128
            .checked_shl(backoff)
            .and_then(|factor| base.as_nanos().checked_mul(factor))
            .unwrap_or(u128::MAX);
        from_nanos(nanos.min(base.max(MAX_BACKED_OFF_PTO).as_nanos()))
    }

    /// Sender payload lifetime: four smoothed RTTs, clamped to 20..100 ms.
    ///
    /// Expiring a payload does not retire its outstanding packet accounting.
    pub fn frame_lifetime(&self) -> Duration {
        self.smoothed
            .saturating_mul(4)
            .clamp(Duration::from_millis(20), Duration::from_millis(100))
    }
}

/// One path group's PTO expiry, independent of packet/frame retirement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProbeTimeoutExpired {
    /// Aggregate probe allowance across every session and lane in the group.
    pub probe_limit: u8,
    /// Start one bounded pass declaring outstanding transmissions lost.
    pub declare_lost: bool,
}

/// Shared established-traffic PTO backoff for one sending path group.
///
/// RFC 9002 supplies timing/backoff; the three-PTO stop/settlement rule follows
/// this transport's initial-design.md section 8.3.
///
/// The owner serializes this with ACK/loss reports and ordinary byte grants.
/// A timeout grants at most two probes in total, including liveness/PMTU probes;
/// workers must not each turn this event into their own allowance. Those probes
/// remain paced and charged to inflight bytes, even when exceeding the window.
///
/// After three expiries without fresh directional feedback, stop ordinary grants
/// and declare remaining outstanding transmissions lost once. A first matching
/// PROBE_REPLY clears that silence count; a new ACK of eliciting traffic also
/// resets the backoff. Duplicate/feedback-only ACKs and payload expiry do neither.
/// Packet charges remain independent of this timer state.
#[derive(Debug, Default)]
pub struct ProbeTimeout {
    backoff: u32,
    unanswered: u8,
    expired_at: Option<Duration>,
}

impl ProbeTimeout {
    /// Starts an idle path group with no PTO backoff.
    pub const fn new() -> Self {
        Self {
            backoff: 0,
            unanswered: 0,
            expired_at: None,
        }
    }

    /// Ordinary transmissions must stop until fresh directional feedback.
    pub const fn stalled(&self) -> bool {
        self.unanswered == 3
    }

    /// Next PTO deadline, or None when idle or the timestamp cannot be represented.
    ///
    /// last_sent is the earliest of each outstanding lane's most recent eliciting
    /// send times. Pass None when no eliciting charge remains. Stalled groups keep
    /// probing even after their old charges have all been settled.
    ///
    /// A failed/deferred probe must not trigger repeated immediate expiry: wait
    /// one backed-off period after the last expiry before issuing another allowance.
    pub fn deadline(&self, rtt: &Rtt, last_sent: Option<Duration>) -> Option<Duration> {
        let sent = match last_sent {
            Some(sent) => sent,
            None if self.stalled() => self.expired_at?,
            None => return None,
        };
        sent.max(self.expired_at.unwrap_or(sent))
            .checked_add(rtt.pto_period(self.backoff))
    }

    /// Consumes a due PTO once; early/backward polls change nothing.
    ///
    /// Time-threshold loss detection takes precedence: service its timer and
    /// pending bounded loss work before polling PTO. Replace any unissued allowance
    /// from the previous expiry; allowances must not accumulate. Retain the one-time
    /// loss instruction until the owner has scheduled its bounded settlement pass.
    /// This method itself neither sends packets nor releases congestion credit.
    pub fn expire(
        &mut self,
        now: Duration,
        rtt: &Rtt,
        last_sent: Option<Duration>,
    ) -> Option<ProbeTimeoutExpired> {
        if now < self.deadline(rtt, last_sent)? {
            return None;
        }
        let declare_lost = self.unanswered == 2;
        self.unanswered = (self.unanswered + 1).min(3);
        self.backoff = self.backoff.saturating_add(1);
        self.expired_at = Some(now);
        Some(ProbeTimeoutExpired {
            probe_limit: 2,
            declare_lost,
        })
    }

    /// Ends silence on the first validated matching reply to an unanswered probe.
    ///
    /// The caller validates connection, lane, packet and size and rejects replay.
    /// This neither resets PTO backoff nor settles any packet charge.
    pub fn probe_replied(&mut self) {
        self.unanswered = 0;
    }

    /// Resets only on validated, newly acknowledged ACK-eliciting traffic.
    ///
    /// A late ACK after loss qualifies even when it releases no remaining charge.
    /// Cancel unissued probe allowances from prior expiries; already reserved
    /// grants still require sent/unused reports before their credit can be reused.
    pub fn acknowledged(&mut self, newly_acked_eliciting: bool) {
        if newly_acked_eliciting {
            *self = Self::new();
        }
    }
}

fn weighted(old: Duration, sample: Duration, weight: u128) -> Duration {
    // Duration::MAX * 7 fits u128; averaging before conversion avoids saturation bias.
    from_nanos((old.as_nanos() * weight + sample.as_nanos()) / (weight + 1))
}

fn from_nanos(nanos: u128) -> Duration {
    Duration::new(
        (nanos / 1_000_000_000) as u64,
        (nanos % 1_000_000_000) as u32,
    )
}
