//! Thread ownership and control for the Linux transport.
//!
//! All transport storage is reserved before TAP readiness. Runtime workers own
//! packet state; the control handle only reads published status or requests exit.

use crate::{
    config::{Config, Validated},
    counters::{Counters, Snapshot},
    handshake::State,
    reassembly, spsc,
    tap::Tap,
    worker::{Event, Wake, Worker},
};
use std::{
    ffi::CString,
    io,
    mem::size_of,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
    },
};

pub(crate) const BUFFERS: usize = 128;
pub(crate) const LANES: usize = 64;
pub(crate) const STACK_BYTES: usize = 512 * 1024;

/// Independently observed transport state for one configured session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionStatus {
    /// Handshake and liveness state.
    pub state: State,
    /// Last successfully applied TAP carrier state.
    pub carrier: bool,
}

/// Startup, statistics and shutdown handle. A handle belongs to one run only.
#[derive(Clone, Default)]
pub struct Control(Arc<ControlState>);

#[derive(Default)]
struct ControlState {
    started: AtomicBool,
    ready: AtomicBool,
    shutdown: AtomicBool,
    stopped: AtomicBool,
    shared: OnceLock<Arc<Shared>>,
}

pub(crate) struct Shared {
    pub config: Validated,
    pub wakes: Vec<Arc<Wake>>,
    pub owners: Vec<Box<[AtomicU8]>>,
    pub status: Vec<AtomicU8>,
    pub counters: Vec<Counters>,
    pub bound: AtomicU64,
}

impl Control {
    /// Creates a handle without starting threads or opening devices.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether every configured TAP exists in the running instance.
    pub fn ready(&self) -> bool {
        self.0.ready.load(Ordering::Acquire)
    }

    /// Whether run has returned after joining all its workers.
    pub fn stopped(&self) -> bool {
        self.0.stopped.load(Ordering::Acquire)
    }

    /// Requests exit and wakes all workers. Safe before run or after it returns.
    pub fn shutdown(&self) {
        self.0.shutdown.store(true, Ordering::Release);
        if let Some(shared) = self.0.shared.get() {
            for wake in &shared.wakes {
                let _ = wake.notify();
            }
        }
    }

    pub(crate) fn quitting(&self) -> bool {
        self.0.shutdown.load(Ordering::Acquire)
    }

    /// Samples a session by its index in Config.sessions, without allocation.
    pub fn session(&self, index: usize) -> Option<SessionStatus> {
        let value = self
            .0
            .shared
            .get()?
            .status
            .get(index)?
            .load(Ordering::Acquire);
        Some(SessionStatus {
            state: match value & 7 {
                1 => State::Establishing,
                2 => State::Active,
                3 => State::Degraded,
                4 => State::Inactive,
                _ => State::Configured,
            },
            carrier: value & 8 != 0,
        })
    }

    /// Samples one worker's cumulative totals; fields are sampled independently.
    pub fn worker_counters(&self, worker: usize) -> Option<Snapshot> {
        Some(self.0.shared.get()?.counters.get(worker)?.snapshot())
    }
}

impl Shared {
    pub fn workers(&self) -> usize {
        self.wakes.len()
    }

    pub fn all_bound(&self) -> bool {
        self.bound.load(Ordering::Acquire) == u64::MAX >> (64 - self.workers())
    }

    pub fn publish_state(&self, session: usize, state: State, carrier: bool) {
        let state = match state {
            State::Configured => 0,
            State::Establishing => 1,
            State::Active => 2,
            State::Degraded => 3,
            State::Inactive => 4,
        };
        self.status[session].store(state | (u8::from(carrier) << 3), Ordering::Release);
    }
}

/// Creates TAPs first, starts transport workers, and blocks until shutdown.
///
/// Readiness means TAP existence; sockets, address availability and handshakes
/// never delay it. The caller manages administrative link state. All workers
/// are joined and newly created TAPs are closed before this returns.
///
/// This initial runtime accepts the raw IPv6 destinations present in Config.
/// Supplying refreshed candidates is a separate, unfinished roadmap item.
pub fn run(config: Config, control: Control) -> io::Result<()> {
    if control.0.started.swap(true, Ordering::AcqRel) {
        return Err(io::ErrorKind::AlreadyExists.into());
    }
    struct Finish(Control);
    impl Drop for Finish {
        fn drop(&mut self) {
            self.0.0.ready.store(false, Ordering::Release);
            self.0.0.stopped.store(true, Ordering::Release);
        }
    }
    let _finish = Finish(control.clone());
    let config = config
        .validate()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let count = config
        .config()
        .sessions
        .iter()
        .map(|s| s.queue_count)
        .max()
        .unwrap_or(1);
    let required = storage_bytes(&config, count).ok_or(io::ErrorKind::OutOfMemory)?;
    if required > config.config().daemon.memory_limit {
        return Err(io::Error::new(
            io::ErrorKind::OutOfMemory,
            "transport storage exceeds memory_limit",
        ));
    }
    let sessions = config.config().sessions.len();
    let wakes = (0..count)
        .map(|_| Wake::new().map(Arc::new))
        .collect::<io::Result<Vec<_>>>()?;
    let shared = Arc::new(Shared {
        config,
        wakes,
        owners: (0..sessions)
            .map(|_| (0..65_536).map(|_| AtomicU8::new(0)).collect())
            .collect(),
        status: (0..sessions).map(|_| AtomicU8::new(0)).collect(),
        counters: (0..count).map(|_| Counters::new()).collect(),
        bound: AtomicU64::new(0),
    });
    let mut workers = (0..count)
        .map(|id| Worker::new(id, Arc::clone(&shared), control.clone()))
        .collect::<io::Result<Vec<_>>>()?;
    for from in 0..count {
        for to in 0..count {
            if from != to {
                let (tx, rx) = spsc::channel();
                workers[from].outgoing[to] = Some(tx);
                workers[to].incoming[from] = Some(rx);
            }
        }
    }
    for (index, session) in shared.config.config().sessions.iter().enumerate() {
        let name = CString::new(session.tap_name()).map_err(|_| io::ErrorKind::InvalidInput)?;
        let tap = Tap::create(&name, session.inner_mtu, session.queue_count, session.mac)?;
        for (worker, queue) in workers.iter_mut().zip(tap.into_queues()) {
            worker.attach_tap(index, queue)?;
        }
    }
    if control.0.shared.set(Arc::clone(&shared)).is_err() {
        return Err(io::ErrorKind::AlreadyExists.into());
    }
    control.0.ready.store(true, Ordering::Release);
    let mut threads = Vec::with_capacity(count);
    let mut result = Ok(());
    for worker in workers {
        match std::thread::Builder::new()
            .stack_size(STACK_BYTES)
            .spawn(move || worker.run())
        {
            Ok(thread) => threads.push(thread),
            Err(error) => {
                control.shutdown();
                result = Err(error);
                break;
            }
        }
    }
    for thread in threads {
        let outcome = thread
            .join()
            .unwrap_or_else(|_| Err(io::ErrorKind::Other.into()));
        if result.is_ok() {
            result = outcome;
        }
    }
    result
}

fn storage_bytes(config: &Validated, workers: usize) -> Option<usize> {
    let raw = config.config();
    let sessions = raw.sessions.len();
    let queues = workers.checked_mul(workers - 1)?.checked_mul(
        spsc::CAPACITY
            .checked_mul(size_of::<Event>())?
            .checked_add(512)?,
    )?;
    let reassembly = sessions
        .checked_mul(LANES)?
        .checked_mul(reassembly::Lane::storage_bytes())?;
    let shared = sessions
        .checked_mul(65_536 + size_of::<Box<[AtomicU8]>>() + 1)?
        .checked_add(workers.checked_mul(STACK_BYTES + size_of::<Counters>() + 256)?)?;
    // Config owns caller-supplied capacities; count those even when mostly empty.
    let mut configuration = raw
        .endpoints
        .capacity()
        .checked_mul(size_of::<crate::config::Endpoint>())?
        .checked_add(
            raw.peers
                .capacity()
                .checked_mul(size_of::<crate::config::Peer>())?,
        )?
        .checked_add(
            raw.sessions
                .capacity()
                .checked_mul(size_of::<crate::config::Session>())?,
        )?;
    for endpoint in &raw.endpoints {
        configuration = configuration
            .checked_add(endpoint.name.capacity())?
            .checked_add(
                endpoint
                    .bind_device
                    .as_ref()
                    .map_or(0, |d| d.as_bytes_with_nul().len()),
            )?;
    }
    for peer in &raw.peers {
        configuration = configuration
            .checked_add(peer.name.capacity())?
            .checked_add(peer.endpoint.capacity())?
            .checked_add(peer.path_group.as_ref().map_or(0, String::capacity))?;
    }
    for session in &raw.sessions {
        configuration = configuration
            .checked_add(session.name.capacity())?
            .checked_add(session.peer.capacity())?
            .checked_add(session.tap_name.as_ref().map_or(0, String::capacity))?;
    }
    // Numeric references and a conservative bound for cookie hash buckets.
    configuration = configuration
        .checked_add(raw.peers.len().checked_mul(size_of::<usize>())?)?
        .checked_add(sessions.checked_mul(128)?)?;
    [
        Worker::storage_bytes(raw, workers)?,
        queues,
        reassembly,
        shared,
        configuration,
    ]
    .into_iter()
    .try_fold(4096_usize, |sum, value| sum.checked_add(value))?
    .checked_mul(5)?
    .checked_div(4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_ceiling_and_single_use_fail_before_opening_devices() {
        let mut config = Config::default();
        config.daemon.memory_limit = 1;
        let control = Control::new();
        assert_eq!(
            run(config, control.clone()).unwrap_err().kind(),
            io::ErrorKind::OutOfMemory
        );
        assert!(!control.ready());
        assert!(control.stopped());
        assert_eq!(
            run(Config::default(), control).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
    }

    #[test]
    fn accounting_includes_reserved_config_capacity_and_worker_queues() {
        let empty = Config::default().validate().unwrap();
        let baseline = storage_bytes(&empty, 1).unwrap();
        let mut reserved = Config::default();
        reserved.peers.reserve(10_000);
        let reserved = reserved.validate().unwrap();
        assert!(storage_bytes(&reserved, 1).unwrap() > baseline + 1_000_000);
        assert!(storage_bytes(&empty, 4).unwrap() > baseline * 4);
    }
}
