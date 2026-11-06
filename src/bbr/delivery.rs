//! Delivery sampling from draft-ietf-ccwg-bbr-07, sections 2.3 and 4.1.2.
//! Derived from the draft's code components; see LICENSE-BBR.
use std::time::Duration;

/// A successful physical transmission's retained sampling metadata.
/// Keep it through loss and payload expiry, until ACK or sent-history eviction.
/// It belongs to exactly one path group's sampler.
#[derive(Debug)]
pub struct DeliveryPacket {
    snapshot: Snapshot,
    bytes: u16,
    acknowledged: bool,
    declared_lost: bool,
}

/// Delivery observation from all newly acknowledged transmissions in one ACK.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RateSample {
    /// Outer bytes per second; absent for zero/short intervals or an unrepresentable rate.
    pub rate: Option<u64>,
    /// Longer of the send and acknowledgment intervals.
    pub interval: Duration,
    /// Successful transmission bytes delivered during the sampling interval.
    pub delivered: u64,
    /// Bytes newly acknowledged by this ACK, including late originals after loss.
    pub newly_acked: u64,
    /// Shared delivered counter at the newest acknowledged transmission's send.
    pub prior_delivered: u64,
    /// Shared lost bytes since that transmission's send.
    pub lost: u64,
    /// Shared bytes in flight immediately after that transmission's send.
    pub tx_in_flight: u64,
    /// Whether that transmission belonged to an application-limited bubble.
    pub app_limited: bool,
    /// Whether host queues limited that transmission or this ACK's processing.
    pub host_limited: bool,
    /// Shared monotonic identifier of the newest acknowledged transmission.
    pub last_packet: u64,
}

/// One newly declared physical loss, including the transmission's delivery snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LossSample {
    /// Outer bytes in this physical datagram.
    pub bytes: u16,
    /// Shared cumulative lost bytes after this declaration.
    pub total_lost: u64,
    /// Shared lost bytes since this transmission was sent.
    pub lost: u64,
    /// Shared bytes in flight immediately after this transmission was sent.
    pub tx_in_flight: u64,
    /// Whether this transmission belonged to an application-limited bubble.
    pub app_limited: bool,
}

#[derive(Clone, Copy, Debug)]
struct Snapshot {
    id: u64,
    sent_at: Duration,
    first_sent_at: Duration,
    delivered_at: Duration,
    delivered: u64,
    lost: u64,
    in_flight: u64,
    app_limited: bool,
    host_limited: bool,
}

struct Pending {
    at: Duration,
    newly_acked: u64,
    host_limited: bool,
    newest: Option<Snapshot>,
}

/// Invalid sampler transition; rejection leaves its state unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeliveryError {
    /// Invalid size, timestamp order or missing ACK transaction.
    Invalid,
    /// Finish the pending ACK before changing send/loss/application state.
    Busy,
    /// A lifetime byte counter or transmission identity cannot be represented.
    Exhausted,
}

impl std::fmt::Display for DeliveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Invalid => "invalid delivery sampling transition",
            Self::Busy => "delivery sample is still being accumulated",
            Self::Exhausted => "delivery sampling counter exhausted",
        })
    }
}

impl std::error::Error for DeliveryError {}

/// One sending path group's allocation-free delivery-rate sampler.
///
/// Commit successful sends in timestamp order, across all lanes, before applying
/// their feedback. Supply actual pre-syscall timestamps only for successful
/// physical transmissions. Feedback-only packets never enter this sampler.
///
/// ACK observation time is when this owner updates its delivered counter.
/// Preserve original receive time separately for RTT; report-queue delay must
/// not inflate RTT or set application-limited state. One ACK may be accumulated
/// over several bounded turns, but sends/losses wait until finish_ack.
///
/// Runtime grant epochs must bound cumulative issued bytes and await every
/// sent/unused report before applying ACK/loss feedback. The grant ledger's
/// simultaneous-reservation bound alone does not bound an epoch.
pub struct Delivery {
    delivered: u64,
    lost: u64,
    delivered_at: Duration,
    first_sent_at: Duration,
    last_sent_at: Duration,
    next_id: u64,
    app_limited: Option<u64>,
    pending: Option<Pending>,
}

impl Default for Delivery {
    fn default() -> Self {
        Self::new()
    }
}

impl Delivery {
    /// Creates an empty shared sampler without allocating.
    pub const fn new() -> Self {
        Self {
            delivered: 0,
            lost: 0,
            delivered_at: Duration::ZERO,
            first_sent_at: Duration::ZERO,
            last_sent_at: Duration::ZERO,
            next_id: 1,
            app_limited: None,
            pending: None,
        }
    }

    /// Total newly acknowledged outer bytes, including acknowledged repairs.
    pub const fn delivered(&self) -> u64 {
        self.delivered
    }

    /// Total bytes declared lost; late ACKs do not subtract from this counter.
    pub const fn lost(&self) -> u64 {
        self.lost
    }

    /// Whether an application-limited bubble is still in flight.
    pub const fn is_app_limited(&self) -> bool {
        self.app_limited.is_some()
    }

    /// Marks the draft's application-limited bubble at delivered + inflight.
    ///
    /// For normal idle detection, require no unsent data or pending lower-layer transmissions,
    /// inflight below cwnd, and no unrepaired loss. Check before enqueueing new
    /// data and before ACK/timer processing; host backpressure alone is not enough.
    /// ProbeRTT also calls this explicitly, even with unsent data, to exclude its low rates.
    pub fn application_limited(&mut self, in_flight: u64) -> Result<(), DeliveryError> {
        if self.pending.is_some() {
            return Err(DeliveryError::Busy);
        }
        let end = self
            .delivered
            .checked_add(in_flight)
            .ok_or(DeliveryError::Exhausted)?
            .max(1);
        self.app_limited = Some(end);
        Ok(())
    }

    /// Captures one successfully sent, congestion-charged physical datagram.
    ///
    /// in_flight excludes this packet and includes all earlier successful sends
    /// in the epoch. Never pass a batch's final inflight total for every packet.
    /// Equal timestamps are ordered by these calls, giving each lane a shared ID.
    pub fn sent(
        &mut self,
        at: Duration,
        bytes: u16,
        in_flight: u64,
        host_limited: bool,
    ) -> Result<DeliveryPacket, DeliveryError> {
        if self.pending.is_some() {
            return Err(DeliveryError::Busy);
        }
        if !(100..=9000).contains(&bytes) || at < self.delivered_at || at < self.last_sent_at {
            return Err(DeliveryError::Invalid);
        }
        let next = self
            .next_id
            .checked_add(1)
            .ok_or(DeliveryError::Exhausted)?;
        let in_flight_after = in_flight
            .checked_add(u64::from(bytes))
            .ok_or(DeliveryError::Exhausted)?;
        if in_flight == 0 {
            self.first_sent_at = at;
            self.delivered_at = at;
        }
        let snapshot = Snapshot {
            id: self.next_id,
            sent_at: at,
            first_sent_at: self.first_sent_at,
            delivered_at: self.delivered_at,
            delivered: self.delivered,
            lost: self.lost,
            in_flight: in_flight_after,
            app_limited: self.app_limited.is_some(),
            host_limited,
        };
        let packet = DeliveryPacket {
            snapshot,
            bytes,
            acknowledged: false,
            declared_lost: false,
        };
        self.last_sent_at = at;
        self.next_id = next;
        Ok(packet)
    }

    /// Starts one ACK sample with fresh owner time, independent of wire ACK delay.
    /// host_limited records delayed owner service without marking an application bubble.
    pub fn begin_ack(&mut self, at: Duration, host_limited: bool) -> Result<(), DeliveryError> {
        if self.pending.is_some() {
            return Err(DeliveryError::Busy);
        }
        if at < self.delivered_at || at < self.last_sent_at {
            return Err(DeliveryError::Invalid);
        }
        self.pending = Some(Pending {
            at,
            newly_acked: 0,
            host_limited,
            newest: None,
        });
        Ok(())
    }

    /// Adds one retained transmission after whole-ACK validation by sent history.
    /// Returns false for duplicates. A late ACK after loss still counts delivery.
    pub fn acknowledged(&mut self, packet: &mut DeliveryPacket) -> Result<bool, DeliveryError> {
        let ack = self.pending.as_mut().ok_or(DeliveryError::Invalid)?;
        if packet.acknowledged {
            return Ok(false);
        }
        let snapshot = packet.snapshot;
        if snapshot.sent_at > ack.at
            || snapshot.delivered_at > ack.at
            || snapshot.delivered > self.delivered
            || snapshot.lost > self.lost
            || snapshot.id >= self.next_id
        {
            return Err(DeliveryError::Invalid);
        }
        let delivered = self
            .delivered
            .checked_add(u64::from(packet.bytes))
            .ok_or(DeliveryError::Exhausted)?;
        let newly_acked = ack
            .newly_acked
            .checked_add(u64::from(packet.bytes))
            .ok_or(DeliveryError::Exhausted)?;
        if ack
            .newest
            .is_none_or(|old| (snapshot.sent_at, snapshot.id) > (old.sent_at, old.id))
        {
            ack.newest = Some(snapshot);
            self.first_sent_at = snapshot.sent_at;
        }
        ack.newly_acked = newly_acked;
        self.delivered = delivered;
        self.delivered_at = ack.at;
        packet.acknowledged = true;
        Ok(true)
    }

    /// Finishes the ACK, keeping its metadata even when its rate is unusable.
    /// min_rtt is the lifetime minimum raw RTT; zero disables the short-interval filter.
    /// RTT estimation and congestion-charge settlement remain the caller's work.
    pub fn finish_ack(&mut self, min_rtt: Duration) -> Result<Option<RateSample>, DeliveryError> {
        let ack = self.pending.take().ok_or(DeliveryError::Invalid)?;
        if self.app_limited.is_some_and(|end| self.delivered > end) {
            self.app_limited = None;
        }
        let Some(packet) = ack.newest else {
            return Ok(None);
        };
        let interval = (packet.sent_at - packet.first_sent_at).max(ack.at - packet.delivered_at);
        let delivered = self.delivered - packet.delivered;
        let rate = if interval.is_zero() || interval < min_rtt {
            None
        } else {
            u64::try_from(u128::from(delivered) * 1_000_000_000 / interval.as_nanos()).ok()
        };
        Ok(Some(RateSample {
            rate,
            interval,
            delivered,
            newly_acked: ack.newly_acked,
            prior_delivered: packet.delivered,
            lost: self.lost - packet.lost,
            tx_in_flight: packet.in_flight,
            app_limited: packet.app_limited,
            host_limited: packet.host_limited || ack.host_limited,
            last_packet: packet.id,
        }))
    }

    /// Declares one transmission lost once; already acknowledged packets do nothing.
    /// Retain its metadata for a possible late ACK. Payload expiry is not packet loss.
    pub fn declared_lost(
        &mut self,
        packet: &mut DeliveryPacket,
    ) -> Result<Option<LossSample>, DeliveryError> {
        if self.pending.is_some() {
            return Err(DeliveryError::Busy);
        }
        if packet.declared_lost || packet.acknowledged {
            return Ok(None);
        }
        if packet.snapshot.lost > self.lost || packet.snapshot.id >= self.next_id {
            return Err(DeliveryError::Invalid);
        }
        let lost = self
            .lost
            .checked_add(u64::from(packet.bytes))
            .ok_or(DeliveryError::Exhausted)?;
        self.lost = lost;
        packet.declared_lost = true;
        Ok(Some(LossSample {
            bytes: packet.bytes,
            total_lost: lost,
            lost: lost - packet.snapshot.lost,
            tx_in_flight: packet.snapshot.in_flight,
            app_limited: packet.snapshot.app_limited,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unrepresentable_rate_does_not_discard_delivered_bytes() {
        let mut delivery = Delivery::new();
        let mut packet = delivery.sent(Duration::ZERO, 100, 0, false).unwrap();
        delivery.delivered = u64::MAX - 100;
        delivery.begin_ack(Duration::from_nanos(1), false).unwrap();
        assert!(delivery.acknowledged(&mut packet).unwrap());
        let sample = delivery.finish_ack(Duration::ZERO).unwrap().unwrap();
        assert_eq!(sample.rate, None);
        assert_eq!(sample.delivered, u64::MAX);
        assert_eq!(sample.newly_acked, 100);
    }

    #[test]
    fn exhausted_identity_delivery_and_loss_counters_do_not_wrap() {
        let mut delivery = Delivery::new();
        delivery.next_id = u64::MAX;
        assert_eq!(
            delivery.sent(Duration::ZERO, 100, 0, false).unwrap_err(),
            DeliveryError::Exhausted
        );
        assert_eq!(delivery.next_id, u64::MAX);
        delivery.next_id = 1;
        let mut packet = delivery.sent(Duration::ZERO, 100, 0, false).unwrap();
        delivery.lost = u64::MAX - 99;
        assert_eq!(
            delivery.declared_lost(&mut packet),
            Err(DeliveryError::Exhausted)
        );
        assert!(!packet.declared_lost);
        assert_eq!(delivery.lost(), u64::MAX - 99);
        delivery.delivered = u64::MAX - 99;
        delivery.begin_ack(Duration::from_nanos(1), false).unwrap();
        assert_eq!(
            delivery.acknowledged(&mut packet),
            Err(DeliveryError::Exhausted)
        );
        assert!(!packet.acknowledged);
        assert_eq!(delivery.delivered(), u64::MAX - 99);
        assert_eq!(delivery.finish_ack(Duration::ZERO), Ok(None));
    }
}
