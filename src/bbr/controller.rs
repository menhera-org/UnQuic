//! BBRv3 model and state machine, draft-ietf-ccwg-bbr-07 sections 5.2--5.6.
//! Derived from the draft's code components; see LICENSE-BBR.
use super::{LossSample, RateSample};
use crate::pacing::Limits;
use std::time::Duration;

const INFINITY: u64 = u64::MAX;
/// Shortest RTT used to size windows. RTT samples exclude the receiver's ACK
/// delay, but a lane holds feedback for up to `ack::DELAY` and loaded workers
/// add their own batching, so on sub-millisecond paths the feedback loop is
/// far longer than min_rtt. Deviation from draft 07 (design section 9).
const WINDOW_RTT_FLOOR: Duration = crate::ack::DELAY;
const RTT_WINDOW: Duration = Duration::from_secs(10);
const RTT_PROBE: Duration = Duration::from_millis(200);

/// Current BBRv3 operating phase.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Exponential search for available bandwidth.
    Startup,
    /// Drain the queue left by startup.
    Drain,
    /// Reduce inflight after a bandwidth probe.
    Down,
    /// Maintain the measured delivery rate between probes.
    Cruise,
    /// Refill the path before accelerating.
    Refill,
    /// Accelerate to probe available bandwidth.
    Up,
    /// Drain inflight to refresh the propagation RTT estimate.
    ProbeRtt,
}

impl State {
    fn probe_bw(self) -> bool {
        matches!(self, Self::Down | Self::Cruise | Self::Refill | Self::Up)
    }

    fn probing(self) -> bool {
        matches!(self, Self::Startup | Self::Refill | Self::Up)
    }

    fn pacing_gain(self) -> u64 {
        match self {
            Self::Startup => 277,
            Self::Drain => 50,
            Self::Down => 90,
            Self::Up => 125,
            Self::Cruise | Self::Refill | Self::ProbeRtt => 100,
        }
    }

    fn cwnd_gain(self) -> u64 {
        match self {
            Self::Up => 225,
            Self::ProbeRtt => 50,
            _ => 200,
        }
    }
}

/// Rejected controller input; errors leave controller state unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControllerError {
    /// Invalid MTU, time order, accounting totals or recovery transition.
    Invalid,
    /// A lifetime byte counter cannot be represented.
    Exhausted,
}

impl std::fmt::Display for ControllerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Invalid => "invalid congestion controller input",
            Self::Exhausted => "congestion controller counter exhausted",
        })
    }
}

impl std::error::Error for ControllerError {}

/// One direction of a shared path group; all quantities include outer headers.
///
/// Inputs come from the group's delivery sampler and already-deduplicated
/// accounting reports. Call once per complete ACK, after its bounded sampler
/// transaction finishes. Pass original receive-time RTT separately from owner
/// observation time. Host limitation never substitutes for application limitation.
///
/// This component performs no I/O, allocation or locking. It does not authorize
/// sends by itself: its limits feed the shared grant ledger.
pub struct Controller {
    state: State,
    mss: u16,
    initial_cwnd: u64,
    cwnd: u64,
    pacing_rate: u64,
    quantum: u32,
    delivered: u64,
    lost: u64,
    now: Duration,
    random: u64,

    max_bw: u64,
    bw_filter: [u64; 2],
    bw_index: usize,
    bw: u64,
    bw_shortterm: u64,
    inflight_shortterm: u64,
    inflight_longterm: u64,
    bdp: u64,
    min_rtt: Option<Duration>,
    min_rtt_stamp: Duration,

    round_start: bool,
    cwnd_limited: bool,
    next_round_delivered: u64,
    drain_rounds: u8,
    loss_round_delivered: u64,
    loss_round_start: bool,
    loss_in_round: bool,
    loss_events: u32,
    bw_latest: u64,
    inflight_latest: u64,
    // Deviation switch: only loss rounds above the 2% threshold, or with queueing
    // delay, cut short-term bounds.
    tolerate_random_loss: bool,
    // Largest RTT sample since the current loss round began.
    loss_round_rtt: Duration,

    extra_filter: [u64; 10],
    extra_index: usize,
    extra_acked: u64,
    extra_start: Duration,
    extra_delivered: u64,
    full_bw: u64,
    full_bw_count: u8,
    full_bw_now: bool,
    full_bw_reached: bool,

    cycle_stamp: Duration,
    probe_wait: Duration,
    rounds_since_probe: u8,
    probe_up_rounds: u8,
    probe_up_acked: u64,
    probe_up_acked_per_inc: u64,
    check_probe_loss: bool,
    probe_stopping: bool,
    prev_probe_too_high: bool,
    prev_probe_precautionary: bool,

    idle_restart: bool,
    probe_rtt_done: Option<Duration>,
    probe_rtt_round_done: bool,
    probe_rtt_cwnd: u64,
    recovering: bool,
    prior_cwnd: u64,
    undo_state: Option<State>,
    undo_bw: u64,
    undo_shortterm: u64,
    undo_longterm: u64,
}

impl Controller {
    /// Starts with ten maximum datagrams and draft-07's startup gains.
    ///
    /// max_datagram is a confirmed outer ceiling in 1280..=9000. A missing or
    /// zero srtt uses the draft's 1-ms initial pacing estimate. seed supplies
    /// per-group randomness for probe scheduling; it is not a cryptographic key.
    pub fn new(
        max_datagram: u16,
        srtt: Option<Duration>,
        now: Duration,
        seed: u64,
    ) -> Result<Self, ControllerError> {
        if !(1280..=9000).contains(&max_datagram) {
            return Err(ControllerError::Invalid);
        }
        let initial_cwnd = 10 * u64::from(max_datagram);
        let srtt = srtt.filter(|rtt| !rtt.is_zero());
        let pacing_rate = scale(
            initial_cwnd,
            2_770_000_000,
            srtt.unwrap_or(Duration::from_millis(1)).as_nanos(),
        );
        let mut controller = Self {
            state: State::Startup,
            mss: max_datagram,
            initial_cwnd,
            cwnd: initial_cwnd,
            pacing_rate,
            quantum: 0,
            delivered: 0,
            lost: 0,
            now,
            random: seed,
            max_bw: 0,
            bw_filter: [0; 2],
            bw_index: 0,
            bw: 0,
            bw_shortterm: INFINITY,
            inflight_shortterm: INFINITY,
            inflight_longterm: INFINITY,
            bdp: 0,
            min_rtt: srtt,
            min_rtt_stamp: now,
            round_start: false,
            cwnd_limited: false,
            next_round_delivered: 0,
            drain_rounds: 0,
            loss_round_delivered: 0,
            loss_round_start: false,
            loss_in_round: false,
            loss_events: 0,
            bw_latest: 0,
            inflight_latest: 0,
            tolerate_random_loss: false,
            loss_round_rtt: Duration::ZERO,
            extra_filter: [0; 10],
            extra_index: 0,
            extra_acked: 0,
            extra_start: now,
            extra_delivered: 0,
            full_bw: 0,
            full_bw_count: 0,
            full_bw_now: false,
            full_bw_reached: false,
            cycle_stamp: now,
            probe_wait: Duration::ZERO,
            rounds_since_probe: 0,
            probe_up_rounds: 0,
            probe_up_acked: 0,
            probe_up_acked_per_inc: INFINITY,
            check_probe_loss: false,
            probe_stopping: false,
            prev_probe_too_high: false,
            prev_probe_precautionary: false,
            idle_restart: false,
            probe_rtt_done: None,
            probe_rtt_round_done: false,
            probe_rtt_cwnd: INFINITY,
            recovering: false,
            prior_cwnd: 0,
            undo_state: None,
            undo_bw: 0,
            undo_shortterm: 0,
            undo_longterm: 0,
        };
        controller.set_quantum();
        Ok(controller)
    }

    /// Deviation from draft 07, off by default. Outside bandwidth probing, a
    /// loss round then cuts the short-term bounds only when the ACK that ends
    /// it shows more than the 2% loss that already stops probing, or when an
    /// RTT sample in that round shows a queue, so steady random loss on an
    /// unqueued path no longer ratchets the rate down round by round while
    /// congestion loss still does.
    pub fn tolerate_random_loss(&mut self, on: bool) {
        self.tolerate_random_loss = on;
    }

    /// Whether the current loss round's RTT samples show queueing delay: more
    /// than an eighth of min_rtt, and at least 250 µs, above min_rtt.
    fn queued_in_round(&self) -> bool {
        self.min_rtt.is_some_and(|min| {
            let margin = (min / 8).max(Duration::from_micros(250));
            self.loss_round_rtt > min.saturating_add(margin)
        })
    }

    /// Current phase, for diagnostics and deterministic tests.
    pub const fn state(&self) -> State {
        self.state
    }

    /// Diagnostics: state code, recovery, max_bw, short-term bw, short-term and
    /// long-term inflight, min RTT ns, delivered and lost bytes.
    #[cfg(feature = "trace")]
    pub fn trace_model(&self) -> [u64; 9] {
        [
            self.state as u64,
            u64::from(self.recovering),
            self.max_bw,
            self.bw_shortterm,
            self.inflight_shortterm,
            self.inflight_longterm,
            self.min_rtt.map_or(0, |rtt| rtt.as_nanos() as u64),
            self.delivered,
            self.lost,
        ]
    }

    /// Shared window, pacing rate and send quantum in outer bytes.
    pub const fn limits(&self) -> Limits {
        Limits {
            window: self.cwnd,
            rate: self.pacing_rate,
            quantum: self.quantum,
            max_datagram: self.mss,
        }
    }

    /// Applies a newly confirmed effective maximum datagram size without resetting the model.
    pub fn set_max_datagram(&mut self, size: u16) -> Result<(), ControllerError> {
        if !(1280..=9000).contains(&size) {
            return Err(ControllerError::Invalid);
        }
        self.mss = size;
        self.cwnd = self.cwnd.max(self.min_cwnd());
        self.set_quantum();
        Ok(())
    }

    /// Handles idle restart before granting a new send batch.
    /// Application limitation comes from the sampler's bubble marker.
    pub fn on_transmit(
        &mut self,
        now: Duration,
        in_flight: u64,
        app_limited: bool,
    ) -> Result<(), ControllerError> {
        self.check_time(now)?;
        self.now = now;
        if in_flight == 0 && app_limited {
            self.cwnd_limited = false;
            self.idle_restart = true;
            self.extra_start = now;
            if self.state.probe_bw() {
                self.set_pacing(100);
            } else if self.state == State::ProbeRtt {
                self.check_probe_rtt_done();
            }
        }
        Ok(())
    }

    /// Updates the model after one complete, newly delivering ACK.
    ///
    /// cwnd_limited reports fresh successful window use since the preceding ACK.
    /// The controller retains it through the ACK completing this packet-timed round.
    /// rtt uses the newest ACKed packet's original receive
    /// time, not the time a delayed owner processed its report.
    /// Returns true when the sampler must mark an application-limited bubble
    /// because this ACK ran the ProbeRTT procedure.
    pub fn on_ack(
        &mut self,
        sample: RateSample,
        rtt: Option<Duration>,
        in_flight: u64,
        cwnd_limited: bool,
        now: Duration,
    ) -> Result<bool, ControllerError> {
        self.check_time(now)?;
        let delivered = self
            .delivered
            .checked_add(sample.newly_acked)
            .ok_or(ControllerError::Exhausted)?;
        if sample.newly_acked == 0
            || sample.last_packet == 0
            || sample.prior_delivered > self.delivered
            || sample.delivered != delivered - sample.prior_delivered
            || sample.lost > self.lost
            || sample.tx_in_flight == 0
            || (sample.rate.is_some() && sample.interval.is_zero())
        {
            return Err(ControllerError::Invalid);
        }
        self.now = now;
        self.delivered = delivered;
        let rate = sample.rate.unwrap_or(0);

        self.loss_round_start = sample.prior_delivered >= self.loss_round_delivered;
        if let Some(rtt) = rtt {
            self.loss_round_rtt = self.loss_round_rtt.max(rtt);
        }
        self.bw_latest = self.bw_latest.max(rate);
        self.inflight_latest = self.inflight_latest.max(sample.delivered);
        if self.loss_round_start {
            self.loss_round_delivered = delivered;
        }
        self.update_congestion(&sample);
        self.update_aggregation(sample.newly_acked);
        self.check_full_bw(&sample);
        self.check_startup(&sample);
        if self.state == State::Drain && (in_flight <= self.inflight(100) || self.drain_rounds > 3)
        {
            self.start_down();
        }
        self.cwnd_limited |= cwnd_limited;
        self.update_probe_bw(&sample, in_flight, self.cwnd_limited);
        let expired = self.update_min_rtt(rtt);
        let mark_app_limited = self.check_probe_rtt(in_flight, expired);
        if sample.delivered != 0 {
            self.idle_restart = false;
        }
        if self.loss_round_start {
            self.bw_latest = rate;
            self.inflight_latest = sample.delivered;
            self.loss_events = 0;
        }
        self.bw = self.max_bw.min(self.bw_shortterm);
        self.set_pacing(self.state.pacing_gain());
        self.set_quantum();
        self.set_cwnd(sample.newly_acked);
        if self.round_start {
            // Consume the completed round, retaining fresh evidence for the newly observed flight.
            self.cwnd_limited = cwnd_limited;
        }
        Ok(mark_app_limited)
    }

    /// Begins a transport-declared recovery episode. PTO alone is not an RTO.
    /// timeout selects the draft's RTO response: allow one MSS beyond inflight.
    pub fn enter_recovery(
        &mut self,
        now: Duration,
        in_flight: u64,
        timeout: bool,
    ) -> Result<(), ControllerError> {
        self.check_time(now)?;
        if self.recovering {
            return Err(ControllerError::Invalid);
        }
        self.now = now;
        self.save_loss_state();
        self.recovering = true;
        if timeout {
            self.cwnd = in_flight.saturating_add(u64::from(self.mss));
        }
        Ok(())
    }

    /// Applies one newly declared physical loss after the sampler counted it.
    /// Loss samples must arrive in shared cumulative-counter order, without gaps.
    pub fn on_loss(&mut self, sample: LossSample, now: Duration) -> Result<(), ControllerError> {
        self.check_time(now)?;
        let lost = self
            .lost
            .checked_add(u64::from(sample.bytes))
            .ok_or(ControllerError::Exhausted)?;
        if !(100..=9000).contains(&sample.bytes)
            || sample.total_lost != lost
            || sample.lost < u64::from(sample.bytes)
            || sample.lost > lost
            || sample.tx_in_flight < u64::from(sample.bytes)
        {
            return Err(ControllerError::Invalid);
        }
        self.now = now;
        self.lost = lost;
        if !self.loss_in_round {
            self.loss_round_delivered = self.delivered;
            self.save_loss_state();
        }
        self.loss_in_round = true;
        self.loss_events = self.loss_events.saturating_add(1);
        if self.check_probe_loss && too_high(sample.lost, sample.tx_in_flight) {
            let prior_inflight = sample.tx_in_flight - u64::from(sample.bytes);
            let prior_loss = sample.lost - u64::from(sample.bytes);
            let prefix =
                u128::from(prior_inflight).saturating_sub(50 * u128::from(prior_loss)) / 49;
            let inflight = prior_inflight.saturating_add(prefix.min(u128::from(u64::MAX)) as u64);
            self.prev_probe_too_high = true;
            self.check_probe_loss = false;
            if !sample.app_limited {
                self.inflight_longterm = inflight.max(scale(self.target_inflight(), 70, 100));
            }
            if self.state == State::Up {
                self.undo_state = Some(State::Up);
                self.start_down();
            }
        }
        Ok(())
    }

    /// Ends recovery, restoring cwnd before the next ACK bounds it by the model.
    /// A spurious episode additionally restores the saved bounds and probing phase.
    pub fn recovery_finished(
        &mut self,
        now: Duration,
        spurious: bool,
    ) -> Result<(), ControllerError> {
        self.check_time(now)?;
        if !self.recovering {
            return Err(ControllerError::Invalid);
        }
        self.now = now;
        self.cwnd = self.cwnd.max(self.prior_cwnd);
        if spurious {
            self.loss_in_round = false;
            self.loss_events = 0;
            self.reset_full_bw();
            self.bw_shortterm = self.bw_shortterm.max(self.undo_bw);
            self.inflight_shortterm = self.inflight_shortterm.max(self.undo_shortterm);
            self.inflight_longterm = self.inflight_longterm.max(self.undo_longterm);
            if self.undo_state == Some(State::Startup) && self.state != State::Startup {
                self.full_bw_reached = false;
                if self.state != State::ProbeRtt {
                    self.state = State::Startup;
                }
            } else if self.undo_state == Some(State::Up)
                && self.state != State::Up
                && self.state != State::ProbeRtt
            {
                self.start_refill();
            }
            self.undo_state = None;
        }
        self.recovering = false;
        Ok(())
    }

    fn check_time(&self, now: Duration) -> Result<(), ControllerError> {
        if now < self.now {
            Err(ControllerError::Invalid)
        } else {
            Ok(())
        }
    }

    fn min_cwnd(&self) -> u64 {
        4 * u64::from(self.mss)
    }
    fn start_round(&mut self) {
        self.next_round_delivered = self.delivered;
    }
    fn target_inflight(&self) -> u64 {
        self.bdp.min(self.cwnd)
    }

    fn update_congestion(&mut self, sample: &RateSample) {
        self.round_start = sample.prior_delivered >= self.next_round_delivered;
        if self.round_start {
            self.start_round();
            self.rounds_since_probe = self.rounds_since_probe.saturating_add(1);
            if self.state == State::Drain {
                self.drain_rounds = self.drain_rounds.saturating_add(1);
            }
            self.extra_index = (self.extra_index + 1) % self.extra_filter.len();
            self.extra_filter[self.extra_index] = 0;
        }
        let rate = sample.rate.unwrap_or(0);
        if rate > 0 && (rate >= self.max_bw || !sample.app_limited) {
            self.bw_filter[self.bw_index] = self.bw_filter[self.bw_index].max(rate);
            self.max_bw = self.bw_filter[0].max(self.bw_filter[1]);
        }
        if self.loss_round_start {
            if !self.state.probing()
                && self.loss_in_round
                && (!self.tolerate_random_loss
                    || too_high(sample.lost, sample.tx_in_flight)
                    || self.queued_in_round())
            {
                if self.bw_shortterm == INFINITY {
                    self.bw_shortterm = self.max_bw;
                }
                if self.inflight_shortterm == INFINITY {
                    self.inflight_shortterm = self.cwnd;
                }
                self.bw_shortterm = self.bw_latest.max(scale(self.bw_shortterm, 70, 100));
                self.inflight_shortterm =
                    self.inflight_latest
                        .max(scale(self.inflight_shortterm, 70, 100));
            }
            self.loss_in_round = false;
            self.loss_round_rtt = Duration::ZERO;
        }
    }

    fn update_aggregation(&mut self, newly_acked: u64) {
        let mut expected = bytes_in(self.bw, self.now.saturating_sub(self.extra_start), false);
        if self.extra_delivered <= expected {
            self.extra_delivered = 0;
            self.extra_start = self.now;
            expected = 0;
        }
        self.extra_delivered = self.extra_delivered.saturating_add(newly_acked);
        let extra = self.extra_delivered.saturating_sub(expected).min(self.cwnd);
        if !self.full_bw_reached {
            let current = self.extra_filter[self.extra_index];
            self.extra_filter.fill(0);
            self.extra_filter[self.extra_index] = current;
        }
        self.extra_filter[self.extra_index] = self.extra_filter[self.extra_index].max(extra);
        self.extra_acked = self.extra_filter.iter().copied().max().unwrap_or(0);
    }

    fn reset_full_bw(&mut self) {
        self.full_bw = 0;
        self.full_bw_count = 0;
        self.full_bw_now = false;
    }

    fn check_full_bw(&mut self, sample: &RateSample) {
        // Probe cadence cannot establish that an application's path is full.
        if self.full_bw_now || !self.round_start || sample.app_limited || sample.pmtu_only {
            return;
        }
        let rate = sample.rate.unwrap_or(0);
        if u128::from(rate) * 4 >= u128::from(self.full_bw) * 5 {
            self.reset_full_bw();
            self.full_bw = rate;
        } else {
            self.full_bw_count = self.full_bw_count.saturating_add(1);
            self.full_bw_now = self.full_bw_count >= 3;
            self.full_bw_reached |= self.full_bw_now;
        }
    }

    fn check_startup(&mut self, sample: &RateSample) {
        if !self.full_bw_reached
            && self.loss_round_start
            && self.recovering
            && self.loss_events >= 6
            && too_high(sample.lost, sample.tx_in_flight)
        {
            self.undo_state = Some(State::Startup);
            self.full_bw_reached = true;
            self.inflight_longterm = self.bdp.max(self.inflight_latest);
        }
        if self.state == State::Startup && self.full_bw_reached {
            self.state = State::Drain;
            self.drain_rounds = 0;
        }
    }

    fn reset_congestion(&mut self) {
        self.loss_in_round = false;
        self.loss_events = 0;
        self.bw_latest = 0;
        self.inflight_latest = 0;
    }

    fn reset_shortterm(&mut self) {
        self.bw_shortterm = INFINITY;
        self.inflight_shortterm = INFINITY;
    }

    fn random_word(&mut self) -> u64 {
        // SplitMix64 is used only to desynchronize probes; wrapping is intentional.
        self.random = self.random.wrapping_add(0x9e3779b97f4a7c15);
        let mut value = self.random;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d049bb133111eb);
        value ^ (value >> 31)
    }

    fn start_down(&mut self) {
        self.reset_congestion();
        self.probe_up_acked_per_inc = INFINITY;
        self.rounds_since_probe = (self.random_word() & 1) as u8;
        let nanos = ((u128::from(self.random_word()) * 1_000_000_000) >> 64) as u64;
        self.probe_wait = Duration::from_secs(2) + Duration::from_nanos(nanos);
        self.cycle_stamp = self.now;
        self.probe_stopping = true;
        self.start_round();
        self.state = State::Down;
    }

    fn start_refill(&mut self) {
        self.reset_shortterm();
        self.probe_up_rounds = 0;
        self.probe_up_acked = 0;
        self.prev_probe_precautionary = false;
        self.probe_stopping = false;
        self.start_round();
        self.state = State::Refill;
    }

    fn start_up(&mut self, rate: u64) {
        self.start_round();
        self.reset_full_bw();
        self.full_bw = rate;
        self.state = State::Up;
        self.raise_slope();
    }

    fn raise_slope(&mut self) {
        let growth = 1_u64 << self.probe_up_rounds;
        self.probe_up_rounds = self.probe_up_rounds.saturating_add(1).min(30);
        self.probe_up_acked_per_inc = (self.cwnd / growth).max(u64::from(self.mss));
    }

    fn adapt_longterm(&mut self, sample: &RateSample, cwnd_limited: bool) -> bool {
        if self.probe_stopping && self.round_start {
            self.check_probe_loss = false;
            self.probe_stopping = false;
            if self.state.probe_bw() && !sample.app_limited {
                self.bw_index ^= 1;
                self.bw_filter[self.bw_index] = 0;
            }
            if self.state.probe_bw() && self.prev_probe_precautionary && !self.prev_probe_too_high {
                self.start_refill();
                return true;
            }
        }
        if !too_high(sample.lost, sample.tx_in_flight) {
            if self.inflight_longterm == INFINITY {
                return false;
            }
            self.inflight_longterm = self.inflight_longterm.max(sample.tx_in_flight);
            if self.state == State::Up && cwnd_limited && self.cwnd >= self.inflight_longterm {
                self.probe_up_acked = self.probe_up_acked.saturating_add(sample.newly_acked);
                let delta = self.probe_up_acked / self.probe_up_acked_per_inc;
                self.probe_up_acked %= self.probe_up_acked_per_inc;
                self.inflight_longterm = self
                    .inflight_longterm
                    .saturating_add(delta.saturating_mul(u64::from(self.mss)));
                if self.round_start {
                    self.raise_slope();
                }
            }
        }
        false
    }

    fn update_probe_bw(&mut self, sample: &RateSample, in_flight: u64, cwnd_limited: bool) {
        if !self.full_bw_reached || self.adapt_longterm(sample, cwnd_limited) {
            return;
        }
        match self.state {
            State::Down | State::Cruise => {
                // Reno's round count is in packets, while our volume model uses bytes.
                let reno_rounds = self.target_inflight().div_ceil(u64::from(self.mss)).min(63);
                if self.now.saturating_sub(self.cycle_stamp) > self.probe_wait
                    || u64::from(self.rounds_since_probe) >= reno_rounds
                {
                    self.start_refill();
                } else if self.state == State::Down
                    && in_flight <= self.inflight_with_headroom()
                    && in_flight <= self.inflight_at(self.max_bw, 100)
                {
                    self.state = State::Cruise;
                }
            }
            State::Refill if self.round_start => {
                self.check_probe_loss = true;
                self.start_up(sample.rate.unwrap_or(0));
            }
            State::Up => {
                let down = if self.prev_probe_too_high && in_flight >= self.inflight_longterm {
                    self.prev_probe_precautionary = true;
                    true
                } else if cwnd_limited && self.cwnd >= self.inflight_longterm {
                    self.reset_full_bw();
                    self.full_bw = sample.rate.unwrap_or(0);
                    false
                } else {
                    self.full_bw_now
                };
                if down {
                    self.prev_probe_too_high = false;
                    self.start_down();
                }
            }
            _ => {}
        }
    }

    fn update_min_rtt(&mut self, rtt: Option<Duration>) -> bool {
        let expired = self.now.saturating_sub(self.min_rtt_stamp) > RTT_WINDOW;
        if expired {
            self.probe_rtt_cwnd = self.bdp_multiple(50);
        }
        if let Some(rtt) = rtt
            && (self.min_rtt.is_none_or(|old| rtt < old) || expired)
        {
            self.min_rtt = Some(rtt);
            self.min_rtt_stamp = self.now;
        }
        expired
    }

    fn check_probe_rtt(&mut self, in_flight: u64, expired: bool) -> bool {
        if self.state != State::ProbeRtt && expired && !self.idle_restart {
            self.save_cwnd();
            self.state = State::ProbeRtt;
            self.probe_rtt_done = None;
            self.probe_stopping = true;
            self.start_round();
        }
        if self.state != State::ProbeRtt {
            return false;
        }
        if self.probe_rtt_done.is_none() && in_flight <= self.probe_rtt_window() {
            self.probe_rtt_done = Some(self.now.saturating_add(RTT_PROBE));
            self.probe_rtt_round_done = false;
            self.start_round();
        } else if self.probe_rtt_done.is_some() {
            self.probe_rtt_round_done |= self.round_start;
            if self.probe_rtt_round_done {
                self.check_probe_rtt_done();
            }
        }
        true
    }

    fn check_probe_rtt_done(&mut self) {
        if self.probe_rtt_done.is_some_and(|done| self.now > done) {
            self.min_rtt_stamp = self.now;
            self.cwnd = self.cwnd.max(self.prior_cwnd);
            self.reset_shortterm();
            if self.full_bw_reached {
                self.start_down();
                self.state = State::Cruise;
            } else {
                self.state = State::Startup;
            }
        }
    }

    fn save_cwnd(&mut self) {
        self.prior_cwnd = if !self.recovering && self.state != State::ProbeRtt {
            self.cwnd
        } else {
            self.prior_cwnd.max(self.cwnd)
        };
    }

    fn save_loss_state(&mut self) {
        self.save_cwnd();
        self.undo_state = None;
        self.undo_bw = self.bw_shortterm;
        self.undo_shortterm = self.inflight_shortterm;
        self.undo_longterm = self.inflight_longterm;
    }

    fn set_pacing(&mut self, gain: u64) {
        let rate = scale(self.bw, u128::from(gain) * 99, 10_000);
        if self.full_bw_reached || rate > self.pacing_rate {
            self.pacing_rate = rate;
        }
    }

    fn set_quantum(&mut self) {
        self.quantum = (self.pacing_rate / 1000)
            .min(65536)
            .max(2 * u64::from(self.mss)) as u32;
    }

    fn bdp_multiple(&mut self, gain: u64) -> u64 {
        let Some(rtt) = self.min_rtt else {
            return self.initial_cwnd;
        };
        self.bdp = bytes_in(self.bw, rtt.max(WINDOW_RTT_FLOOR), true);
        scale(self.bdp, u128::from(gain), 100)
    }

    fn quantized(&self, cap: u64) -> u64 {
        // Draft 07's TCP offload budget: one quantum queued, one being sent, one at the receiver.
        let cap = cap.max(3 * u64::from(self.quantum)).max(self.min_cwnd());
        if self.state == State::Up {
            cap.saturating_add(2 * u64::from(self.mss))
        } else {
            cap
        }
    }

    fn inflight(&mut self, gain: u64) -> u64 {
        let cap = self.bdp_multiple(gain);
        self.quantized(cap)
    }

    fn inflight_at(&self, bw: u64, gain: u64) -> u64 {
        let cap = self.min_rtt.map_or(self.initial_cwnd, |rtt| {
            scale(
                bytes_in(bw, rtt.max(WINDOW_RTT_FLOOR), true),
                u128::from(gain),
                100,
            )
        });
        self.quantized(cap)
    }

    fn inflight_with_headroom(&self) -> u64 {
        if self.inflight_longterm == INFINITY {
            return INFINITY;
        }
        let headroom = scale(self.inflight_longterm, 15, 100).max(u64::from(self.mss));
        self.inflight_longterm
            .saturating_sub(headroom)
            .max(self.min_cwnd())
    }

    fn probe_rtt_window(&mut self) -> u64 {
        self.bdp_multiple(50)
            .min(self.probe_rtt_cwnd)
            .max(self.min_cwnd())
    }

    fn set_cwnd(&mut self, newly_acked: u64) {
        let cap = self
            .bdp_multiple(self.state.cwnd_gain())
            .saturating_add(self.extra_acked);
        let max_inflight = self.quantized(cap);
        if self.full_bw_reached {
            self.cwnd = self.cwnd.saturating_add(newly_acked).min(max_inflight);
        } else if self.cwnd < max_inflight || self.delivered < self.initial_cwnd {
            self.cwnd = self.cwnd.saturating_add(newly_acked);
        }
        self.cwnd = self.cwnd.max(self.min_cwnd());
        if self.state == State::ProbeRtt {
            self.cwnd = self.cwnd.min(self.probe_rtt_window());
        }
        let cap = if self.state.probe_bw() && self.state != State::Cruise {
            self.inflight_longterm
        } else if matches!(self.state, State::ProbeRtt | State::Cruise) {
            self.inflight_with_headroom()
        } else {
            INFINITY
        };
        self.cwnd = self
            .cwnd
            .min(cap.min(self.inflight_shortterm).max(self.min_cwnd()));
    }
}

fn too_high(lost: u64, in_flight: u64) -> bool {
    u128::from(lost) * 50 > u128::from(in_flight)
}

fn scale(value: u64, multiplier: u128, divisor: u128) -> u64 {
    (u128::from(value).saturating_mul(multiplier) / divisor).min(u128::from(u64::MAX)) as u64
}

fn bytes_in(rate: u64, time: Duration, ceil: bool) -> u64 {
    let product = u128::from(rate).saturating_mul(time.as_nanos());
    let bytes = if ceil {
        product.div_ceil(1_000_000_000)
    } else {
        product / 1_000_000_000
    };
    bytes.min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests;
