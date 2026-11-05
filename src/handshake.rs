//! Four-message handshake, driven by monotonic elapsed time and supplied tags.
//!
//! Validate the datagram and cookie on the bound receive socket before dispatch.
//! Sender addresses and ports do not participate in protocol admission.
//! Apply commit/retirement effects through the worker barrier before using a new
//! connection. This module owns no sockets, TAPs, clocks or congestion state.

pub mod barrier;

pub use crate::wire::Params;
use crate::{sys, wire::Record};
use std::{fmt, io, num::NonZeroU128, time::Duration};

const SECOND: Duration = Duration::from_secs(1);
const ATTEMPT: Duration = Duration::from_secs(10);
const TOKEN: Duration = Duration::from_millis(250);

/// Establishment role; both roles carry Ethernet traffic in both directions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// Starts an exchange and retries HELLO or CONFIRM.
    Initiator,
    /// Answers HELLO and CONFIRM.
    Responder,
}

/// A nonzero receive tag, independent of the configured session cookie.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tag(NonZeroU128);

impl Tag {
    /// Checks a tag supplied by a caller or deterministic test fixture.
    pub const fn new(value: u128) -> Option<Self> {
        match NonZeroU128::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    /// Returns the wire value.
    pub const fn get(self) -> u128 {
        self.0.get()
    }

    /// Obtains a tag without waiting for entropy. None means retry later.
    ///
    /// Short reads and interruptions are retried. Zero is discarded.
    pub fn generate() -> io::Result<Option<Self>> {
        Ok(sys::random_tag()?.and_then(Self::new))
    }
}

/// Confirmed identity, elected local role and negotiated parameters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Connection {
    /// Local role for this connection, independent of any pending replacement.
    pub role: Role,
    /// Initiator's receive tag.
    pub initiator: Tag,
    /// Responder's receive tag.
    pub responder: Tag,
    /// Locally advertised parameters.
    pub local: Params,
    /// Peer parameters, including ignored optional bits.
    pub peer: Params,
    /// Smaller advertised ceiling, checked against the local frame requirement.
    pub max_frame_len: u16,
}

impl Connection {
    /// Tag an incoming established packet must carry.
    pub const fn local_tag(self) -> Tag {
        match self.role {
            Role::Initiator => self.initiator,
            Role::Responder => self.responder,
        }
    }

    /// Tag to put in outgoing established packets.
    pub const fn remote_tag(self) -> Tag {
        match self.role {
            Role::Initiator => self.responder,
            Role::Responder => self.initiator,
        }
    }
}

/// Session lifecycle, independent of TAP existence and daemon readiness.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Configured, with no exchange started.
    Configured,
    /// An exchange is pending without an active connection.
    Establishing,
    /// A confirmed connection has recent feedback.
    Active,
    /// No fresh feedback for at least three seconds; carrier remains up.
    Degraded,
    /// The confirmed connection expired after thirty seconds without feedback.
    Inactive,
}

/// Actions for the owner to apply in order: retirement/commit, carrier, send.
#[derive(Clone, Copy, Debug, Default)]
pub struct Effects {
    /// At most one handshake record to send; network loss is handled by retries.
    pub send: Option<Record<'static>>,
    /// Install this connection through the worker barrier; settle the old one.
    pub commit: Option<Connection>,
    /// Retire active transport state through the same barrier as replacement.
    pub retire: bool,
    /// Change TAP carrier, without changing administrative link state.
    pub carrier: Option<bool>,
    /// Obtain a fresh tag, then retry this input; no weak fallback is allowed.
    pub needs_tag: bool,
}

/// Rejected local configuration or handshake input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandshakeError {
    /// A field violates the v1 wire or local parameter requirements.
    Parameters,
    /// The advertised receive port differs from the configured peer port.
    Port,
    /// The peer cannot carry the locally required frame size.
    FrameCeiling,
    /// Expected a structurally valid handshake record.
    Record,
    /// A repeated HELLO changes the parameters under the same initiator tag.
    ChangedHello,
    /// Input time moved backwards.
    Time,
}

impl fmt::Display for HandshakeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Parameters => "invalid handshake parameters",
            Self::Port => "peer receive port mismatch",
            Self::FrameCeiling => "peer frame ceiling is too small",
            Self::Record => "invalid handshake record",
            Self::ChangedHello => "HELLO parameters changed under the same tag",
            Self::Time => "handshake time moved backwards",
        })
    }
}
impl std::error::Error for HandshakeError {}

#[derive(Clone, Copy, Debug)]
struct Pending {
    role: Role,
    started: Duration,
    initiator: Tag,
    responder: Option<Tag>,
    peer: Option<Params>,
    last_send: Option<Duration>,
}

#[derive(Clone, Copy, Debug)]
struct Active {
    connection: Connection,
    feedback: Duration,
    last_challenge: Option<Duration>,
    last_ready: Option<Duration>,
}

/// Bounded session state. All input times use the same monotonic origin.
///
/// Fresh feedback must already be matched to a newly acknowledged transmission
/// or an unanswered probe. Merely receiving an old packet is not feedback.
#[derive(Clone, Debug)]
pub struct Session {
    connecting: bool,
    local: Params,
    peer_port: u16,
    required_frame_len: u16,
    state: State,
    active: Option<Active>,
    pending: Option<Pending>,
    last_tag: Option<Tag>,
    now: Duration,
    budget: Duration,
}

impl Session {
    /// Validates configuration; local optional feature bits must be zero in v1.
    pub fn new(
        local: Params,
        peer_port: u16,
        required_frame_len: u16,
    ) -> Result<Self, HandshakeError> {
        local.validate().map_err(|_| HandshakeError::Parameters)?;
        if local.optional_features != 0
            || peer_port == 0
            || !(1514..=local.max_frame_len).contains(&required_frame_len)
        {
            return Err(HandshakeError::Parameters);
        }
        Ok(Self {
            connecting: true,
            local,
            peer_port,
            required_frame_len,
            state: State::Configured,
            active: None,
            pending: None,
            last_tag: None,
            now: Duration::ZERO,
            budget: SECOND,
        })
    }

    /// Current lifecycle state. A pending replacement does not hide active state.
    pub const fn state(&self) -> State {
        self.state
    }

    /// Confirmed connection, if any.
    pub const fn connection(&self) -> Option<Connection> {
        match self.active {
            Some(active) => Some(active.connection),
            None => None,
        }
    }

    /// Whether an unconfirmed exchange occupies the single pending slot.
    pub const fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Starts a replacement and elects roles, preserving any active connection.
    ///
    /// Repeating this call leaves an existing pending attempt and deadline intact.
    pub fn reconnect(
        &mut self,
        now: Duration,
        fresh: Option<Tag>,
    ) -> Result<Effects, HandshakeError> {
        self.advance(now)?;
        self.connecting = true;
        Ok(self.initiate(fresh))
    }

    /// Expires pending attempts, checks liveness and emits a due retransmission.
    ///
    /// Either endpoint starts or replaces a degraded connection automatically.
    /// Supply a fresh random tag when needs_tag is returned. Call at each due
    /// second and promptly after input; all rate limits are enforced internally.
    pub fn on_timer(
        &mut self,
        now: Duration,
        fresh: Option<Tag>,
    ) -> Result<Effects, HandshakeError> {
        self.advance(now)?;
        let mut effects = Effects::default();
        if let Some(active) = self.active {
            let idle = now - active.feedback;
            if idle >= Duration::from_secs(30) {
                self.active = None;
                self.state = State::Inactive;
                effects.retire = true;
                effects.carrier = Some(false);
            } else if idle >= Duration::from_secs(3) {
                self.state = State::Degraded;
            }
        }
        if self.connecting || self.state != State::Active {
            let next = self.initiate(fresh);
            effects.send = next.send;
            effects.needs_tag = next.needs_tag;
        }
        Ok(effects)
    }

    /// Records validated fresh feedback for the currently confirmed local tag.
    ///
    /// A stale worker event for a replaced tag has no effect.
    pub fn on_feedback(&mut self, tag: Tag, now: Duration) -> Result<(), HandshakeError> {
        self.advance(now)?;
        if let Some(active) = &mut self.active
            && active.connection.local_tag() == tag
            && now - active.feedback < Duration::from_secs(30)
        {
            active.feedback = now;
            self.state = State::Active;
        }
        Ok(())
    }

    /// Handles one decoded handshake record. Unexpected identities are ignored.
    ///
    /// The caller must first validate the complete datagram and session cookie.
    /// A missing fresh tag leaves HELLO unaccepted; retry with a tag or let the
    /// peer retransmit. Errors never install or remove a confirmed connection.
    pub fn on_record(
        &mut self,
        record: Record<'_>,
        now: Duration,
        fresh: Option<Tag>,
    ) -> Result<Effects, HandshakeError> {
        if !record.is_handshake() || record.validate().is_err() {
            return Err(HandshakeError::Record);
        }
        self.advance(now)?;
        match record {
            Record::Hello { initiator, params } => {
                self.hello(Tag::new(initiator).unwrap(), params, fresh)
            }
            Record::Challenge {
                initiator,
                responder,
                params,
            } => {
                let Some(mut pending) = self
                    .pending
                    .filter(|p| p.role == Role::Initiator && p.initiator.get() == initiator)
                else {
                    return Ok(Effects::default());
                };
                let responder = Tag::new(responder).unwrap();
                if self.retained_tag(responder) {
                    return Ok(Effects::default());
                }
                if let Some(fixed) = pending.responder {
                    if fixed != responder || pending.peer != Some(params) {
                        return Ok(Effects::default());
                    }
                } else {
                    self.check_peer(params)?;
                    pending.responder = Some(responder);
                    pending.peer = Some(params);
                    pending.last_send = None;
                    self.pending = Some(pending);
                }
                Ok(self.send_pending())
            }
            Record::Confirm {
                initiator,
                responder,
            } => {
                if let Some(mut active) = self.active.filter(|a| {
                    a.connection.role == Role::Responder
                        && matches_pair(a.connection, initiator, responder)
                }) {
                    let send = self.reply(
                        &mut active.last_ready,
                        Record::Ready {
                            initiator,
                            responder,
                        },
                    );
                    self.active = Some(active);
                    return Ok(Effects {
                        send,
                        ..Effects::default()
                    });
                }
                let Some(connection) = self.pending_connection().filter(|c| {
                    c.role == Role::Responder && matches_pair(*c, initiator, responder)
                }) else {
                    return Ok(Effects::default());
                };
                let mut effects = self.install(connection);
                let mut active = self.active.unwrap();
                effects.send = self.reply(
                    &mut active.last_ready,
                    Record::Ready {
                        initiator,
                        responder,
                    },
                );
                self.active = Some(active);
                Ok(effects)
            }
            Record::Ready {
                initiator,
                responder,
            } => Ok(
                match self
                    .pending_connection()
                    .filter(|c| c.role == Role::Initiator && matches_pair(*c, initiator, responder))
                {
                    Some(connection) => self.install(connection),
                    None => Effects::default(),
                },
            ),
            _ => unreachable!(),
        }
    }

    fn advance(&mut self, now: Duration) -> Result<(), HandshakeError> {
        let elapsed = now.checked_sub(self.now).ok_or(HandshakeError::Time)?;
        self.budget = self.budget.saturating_add(elapsed).min(SECOND);
        self.now = now;
        if self.pending.is_some_and(|p| now - p.started >= ATTEMPT) {
            self.pending = None;
        }
        Ok(())
    }

    fn check_peer(&self, params: Params) -> Result<(), HandshakeError> {
        params.validate().map_err(|_| HandshakeError::Parameters)?;
        if params.port != self.peer_port {
            return Err(HandshakeError::Port);
        }
        if params.max_frame_len < self.required_frame_len {
            return Err(HandshakeError::FrameCeiling);
        }
        Ok(())
    }

    fn fresh(&self, tag: Option<Tag>, other: Option<Tag>) -> Option<Tag> {
        tag.filter(|t| Some(*t) != self.last_tag && Some(*t) != other && !self.retained_tag(*t))
    }

    fn retained_tag(&self, tag: Tag) -> bool {
        self.active
            .is_some_and(|a| tag == a.connection.initiator || tag == a.connection.responder)
    }

    fn initiate(&mut self, fresh: Option<Tag>) -> Effects {
        if self.pending.is_none() {
            let Some(tag) = self.fresh(fresh, None) else {
                return Effects {
                    needs_tag: true,
                    ..Effects::default()
                };
            };
            self.last_tag = Some(tag);
            self.connecting = true;
            self.pending = Some(Pending {
                role: Role::Initiator,
                started: self.now,
                initiator: tag,
                responder: None,
                peer: None,
                last_send: None,
            });
            if self.active.is_none() && self.state != State::Inactive {
                self.state = State::Establishing;
            }
        }
        if self.pending.is_some_and(|p| p.role == Role::Responder) {
            Effects::default()
        } else {
            self.send_pending()
        }
    }

    fn hello(
        &mut self,
        initiator: Tag,
        params: Params,
        fresh: Option<Tag>,
    ) -> Result<Effects, HandshakeError> {
        if let Some(mut active) = self
            .active
            .filter(|a| a.connection.role == Role::Responder && a.connection.initiator == initiator)
        {
            if params != active.connection.peer {
                return Err(HandshakeError::ChangedHello);
            }
            let send = self.reply(
                &mut active.last_challenge,
                Record::Challenge {
                    initiator: initiator.get(),
                    responder: active.connection.responder.get(),
                    params: self.local,
                },
            );
            self.active = Some(active);
            return Ok(Effects {
                send,
                ..Effects::default()
            });
        }
        if self.retained_tag(initiator) {
            return Ok(Effects::default());
        }
        let mut started = self.now;
        if let Some(pending) = self.pending {
            started = pending.started;
            if pending.role == Role::Responder {
                if pending.initiator != initiator {
                    return Ok(Effects::default());
                }
                if pending.peer != Some(params) {
                    return Err(HandshakeError::ChangedHello);
                }
                return Ok(self.send_pending());
            }
            if pending.responder.is_some() {
                return Ok(Effects::default());
            }
            self.check_peer(params)?;
            match initiator.get().cmp(&pending.initiator.get()) {
                std::cmp::Ordering::Greater => return Ok(self.send_pending()),
                std::cmp::Ordering::Equal => {
                    self.pending = None;
                    return Ok(self.initiate(fresh));
                }
                std::cmp::Ordering::Less => {}
            }
        } else {
            self.check_peer(params)?;
        }
        let Some(responder) = self.fresh(fresh, Some(initiator)) else {
            return Ok(Effects {
                needs_tag: true,
                ..Effects::default()
            });
        };
        self.last_tag = Some(responder);
        self.connecting = true;
        self.pending = Some(Pending {
            role: Role::Responder,
            started,
            initiator,
            responder: Some(responder),
            peer: Some(params),
            last_send: None,
        });
        if self.active.is_none() && self.state != State::Inactive {
            self.state = State::Establishing;
        }
        Ok(self.send_pending())
    }

    fn send_pending(&mut self) -> Effects {
        let mut pending = self.pending.unwrap();
        let initiator = pending.initiator.get();
        let record = match (pending.role, pending.responder) {
            (Role::Initiator, None) => Record::Hello {
                initiator,
                params: self.local,
            },
            (Role::Initiator, Some(responder)) => Record::Confirm {
                initiator,
                responder: responder.get(),
            },
            (Role::Responder, Some(responder)) => Record::Challenge {
                initiator,
                responder: responder.get(),
                params: self.local,
            },
            (Role::Responder, None) => unreachable!(),
        };
        let send = self.reply(&mut pending.last_send, record);
        self.pending = Some(pending);
        Effects {
            send,
            ..Effects::default()
        }
    }

    fn reply(
        &mut self,
        last: &mut Option<Duration>,
        record: Record<'static>,
    ) -> Option<Record<'static>> {
        if last.is_some_and(|last| self.now - last < SECOND) || self.budget < TOKEN {
            return None;
        }
        self.budget -= TOKEN;
        *last = Some(self.now);
        Some(record)
    }

    fn pending_connection(&self) -> Option<Connection> {
        let pending = self.pending?;
        let peer = pending.peer?;
        Some(Connection {
            role: pending.role,
            initiator: pending.initiator,
            responder: pending.responder?,
            local: self.local,
            peer,
            max_frame_len: self.local.max_frame_len.min(peer.max_frame_len),
        })
    }

    fn install(&mut self, connection: Connection) -> Effects {
        let pending = self.pending.take().unwrap();
        self.connecting = false;
        let carrier = self.active.is_none().then_some(true);
        self.active = Some(Active {
            connection,
            feedback: self.now,
            last_challenge: if pending.role == Role::Responder {
                pending.last_send
            } else {
                None
            },
            last_ready: None,
        });
        self.state = State::Active;
        Effects {
            commit: Some(connection),
            carrier,
            ..Effects::default()
        }
    }
}

fn matches_pair(connection: Connection, initiator: u128, responder: u128) -> bool {
    connection.initiator.get() == initiator && connection.responder.get() == responder
}
