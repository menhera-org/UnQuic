//! Allocation-free connection commit coordination for up to 64 workers.
//!
//! The session control owner starts a barrier for each commit or retirement.
//! It retries unsent commands while continuing to drain incoming work. Workers
//! acknowledge quiescence only after stopping old sends and settling accounting;
//! installation follows only after every worker has quiesced. Defer READY and
//! carrier changes until installation completes. This module does no I/O.

use super::Connection;
use std::fmt;

/// Work required before acknowledging a barrier command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Stop old-connection work and settle its outstanding accounting reports.
    Quiesce,
    /// Install the new connection, or remove the old connection on retirement.
    Install,
}

/// A command carried over the control owner's ordered worker queue.
///
/// A worker must remember its last generation and phase, ignore stale commands,
/// and acknowledge duplicates without repeating installation. Every queued data
/// event still needs its connection tag checked before use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Command {
    /// Locally monotonic barrier identity; never sent on the wire.
    pub generation: u64,
    /// Action to finish before returning this command as an acknowledgment.
    pub phase: Phase,
    /// Replacement connection, or None for retirement.
    pub connection: Option<Connection>,
}

/// Result of accepting a worker acknowledgment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Progress {
    /// Stale, duplicate, unsent or out-of-range acknowledgment.
    Ignored,
    /// Other workers still need to acknowledge this phase.
    Waiting,
    /// All workers stopped old work. Reset shared lane ownership now, before
    /// publishing any installation commands.
    Quiesced,
    /// All workers installed the target. Apply deferred carrier/send effects.
    Complete,
}

/// Invalid barrier setup or attempt to replace an unfinished barrier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BarrierError {
    /// Worker count must be from 1 through 64.
    Workers,
    /// A previous barrier still awaits acknowledgments.
    Busy,
    /// The generation counter is exhausted; stop this runtime.
    Exhausted,
}

impl fmt::Display for BarrierError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Workers => "barrier requires 1..64 workers",
            Self::Busy => "connection barrier is still active",
            Self::Exhausted => "connection barrier generation exhausted",
        })
    }
}
impl std::error::Error for BarrierError {}

/// Single-owner, two-phase commit state. Each operation takes constant time.
pub struct Barrier {
    workers: u64,
    unsent: u64,
    waiting: u64,
    command: Command,
}

impl Barrier {
    /// Creates an idle coordinator; indices include the control owner itself.
    pub const fn new(workers: usize) -> Result<Self, BarrierError> {
        if workers == 0 || workers > 64 {
            return Err(BarrierError::Workers);
        }
        Ok(Self {
            workers: u64::MAX >> (64 - workers),
            unsent: 0,
            waiting: 0,
            command: Command {
                generation: 0,
                phase: Phase::Quiesce,
                connection: None,
            },
        })
    }

    /// Whether either phase is awaiting work.
    pub const fn is_active(&self) -> bool {
        self.waiting != 0
    }

    /// Starts replacement or retirement without changing any worker state.
    ///
    /// A failed call leaves the existing barrier untouched.
    pub fn begin(&mut self, connection: Option<Connection>) -> Result<(), BarrierError> {
        if self.is_active() {
            return Err(BarrierError::Busy);
        }
        let generation = self
            .command
            .generation
            .checked_add(1)
            .ok_or(BarrierError::Exhausted)?;
        self.command = Command {
            generation,
            phase: Phase::Quiesce,
            connection,
        };
        self.unsent = self.workers;
        self.waiting = self.workers;
        Ok(())
    }

    /// Returns the command still needing publication to this worker.
    ///
    /// A full queue leaves this command pending. Call sent only after the queue
    /// accepts it, or before applying it directly on the control owner.
    pub const fn pending(&self, worker: usize) -> Option<Command> {
        if worker < 64 && self.unsent & (1_u64 << worker) != 0 {
            Some(self.command)
        } else {
            None
        }
    }

    /// Records successful publication. Returns false for stale/duplicate calls.
    pub fn sent(&mut self, worker: usize, command: Command) -> bool {
        if self.pending(worker) != Some(command) {
            return false;
        }
        self.unsent &= !(1_u64 << worker);
        true
    }

    /// Applies an acknowledgment after the worker has finished its command.
    ///
    /// The owner must process Quiesced before publishing the next commands.
    /// Complete means every worker has installed the same target, including
    /// the control owner. No timeout bypasses a missing acknowledgment.
    pub fn acknowledge(&mut self, worker: usize, command: Command) -> Progress {
        if worker >= 64 || command != self.command {
            return Progress::Ignored;
        }
        let bit = 1_u64 << worker;
        if self.waiting & bit == 0 || self.unsent & bit != 0 {
            return Progress::Ignored;
        }
        self.waiting &= !bit;
        if self.waiting != 0 {
            return Progress::Waiting;
        }
        match self.command.phase {
            Phase::Quiesce => {
                self.command.phase = Phase::Install;
                self.unsent = self.workers;
                self.waiting = self.workers;
                Progress::Quiesced
            }
            Phase::Install => Progress::Complete,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        handshake::{Role, Tag},
        spsc,
        wire::Params,
    };

    fn connection(n: u128) -> Connection {
        let params = Params {
            port: 4444,
            max_frame_len: 9022,
            mandatory_features: 0,
            optional_features: 0,
        };
        Connection {
            role: Role::Initiator,
            initiator: Tag::new(n).unwrap(),
            responder: Tag::new(n + 1).unwrap(),
            local: params,
            peer: params,
            max_frame_len: 9022,
        }
    }

    #[test]
    fn requires_every_worker_in_both_phases_and_rejects_stale_acks() {
        for count in [1, 4, 64] {
            let mut barrier = Barrier::new(count).unwrap();
            for target in [Some(connection(1)), Some(connection(3)), None] {
                barrier.begin(target).unwrap();
                assert_eq!(barrier.begin(None), Err(BarrierError::Busy));
                for phase in [Phase::Quiesce, Phase::Install] {
                    for worker in (0..count).rev() {
                        let cmd = barrier.pending(worker).unwrap();
                        assert_eq!(cmd.connection, target);
                        assert_eq!(cmd.phase, phase);
                        assert_eq!(barrier.acknowledge(worker, cmd), Progress::Ignored);
                        assert!(!barrier.sent(count, cmd));
                        let mut stale = cmd;
                        stale.generation -= 1;
                        assert!(!barrier.sent(worker, stale));
                        assert_eq!(barrier.acknowledge(worker, stale), Progress::Ignored);
                        assert!(barrier.sent(worker, cmd));
                        assert!(!barrier.sent(worker, cmd));
                        assert!(barrier.pending(worker).is_none());
                        let expected = if worker != 0 {
                            Progress::Waiting
                        } else if phase == Phase::Quiesce {
                            Progress::Quiesced
                        } else {
                            Progress::Complete
                        };
                        assert_eq!(barrier.acknowledge(worker, cmd), expected);
                        assert_eq!(barrier.acknowledge(worker, cmd), Progress::Ignored);
                    }
                }
                assert!(!barrier.is_active());
            }
        }
        assert!(matches!(Barrier::new(0), Err(BarrierError::Workers)));
        assert!(matches!(Barrier::new(65), Err(BarrierError::Workers)));
        let mut barrier = Barrier::new(1).unwrap();
        barrier.command.generation = u64::MAX;
        assert_eq!(barrier.begin(None), Err(BarrierError::Exhausted));
        assert!(!barrier.is_active());
    }

    #[test]
    fn full_queues_retain_commands_and_acks_while_draining() {
        // Three remote workers and a control owner, with both directions full.
        let mut commands: Vec<_> = (0..3).map(|_| spsc::channel::<Option<Command>>()).collect();
        let mut replies: Vec<_> = (0..3).map(|_| spsc::channel::<Option<Command>>()).collect();
        for (tx, _) in commands.iter_mut().chain(replies.iter_mut()) {
            for _ in 0..spsc::CAPACITY {
                tx.try_push(None).unwrap();
            }
        }
        let mut barrier = Barrier::new(4).unwrap();
        barrier.begin(Some(connection(1))).unwrap();
        let mut pending_acks = [None; 3];
        let mut completed = false;
        let mut quiesced = false;
        let mut blocked_commands = 0;
        let mut blocked_acks = 0;
        for _ in 0..(spsc::CAPACITY * 4) {
            for (index, (tx, _)) in commands.iter_mut().enumerate() {
                if let Some(command) = barrier.pending(index + 1) {
                    if tx.try_push(Some(command)).is_ok() {
                        assert!(barrier.sent(index + 1, command));
                    } else {
                        blocked_commands += 1;
                    }
                }
            }
            if let Some(command) = barrier.pending(0) {
                assert!(barrier.sent(0, command));
                assert_eq!(barrier.acknowledge(0, command), Progress::Waiting);
            }
            for index in 0..3 {
                if pending_acks[index].is_none() {
                    pending_acks[index] = commands[index].1.try_pop().flatten();
                }
                if let Some(command) = pending_acks[index] {
                    // Deliberately keep the reply ring full until a command arrives.
                    if replies[index].0.try_push(Some(command)).is_ok() {
                        pending_acks[index] = None;
                    } else {
                        blocked_acks += 1;
                    }
                }
                if blocked_acks != 0
                    && let Some(Some(command)) = replies[index].1.try_pop()
                {
                    match barrier.acknowledge(index + 1, command) {
                        Progress::Quiesced => {
                            assert!(!quiesced);
                            quiesced = true;
                        }
                        Progress::Complete => {
                            assert!(quiesced);
                            completed = true;
                        }
                        Progress::Waiting => {}
                        Progress::Ignored => panic!("valid acknowledgment was lost"),
                    }
                }
            }
            if completed {
                break;
            }
        }
        assert!(completed);
        assert!(blocked_commands >= 3);
        assert!(blocked_acks >= 3);
        assert!(!barrier.is_active());
    }
}
