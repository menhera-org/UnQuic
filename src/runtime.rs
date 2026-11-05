//! Thread ownership and control for the Linux transport.
//!
//! All transport storage is reserved before TAP readiness. Runtime workers own
//! packet state; the control handle reads status and queues cold address updates.

use crate::{
    config::{Config, Validated},
    counters::{Counters, LaneCounters, LaneSnapshot, Snapshot},
    handshake::State,
    reassembly, spsc,
    tap::Tap,
    worker::{Event, Wake, Worker},
};
use std::{
    ffi::CString,
    io,
    mem::size_of,
    net::{Ipv6Addr, SocketAddrV6},
    sync::{
        Arc, Mutex, OnceLock,
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
    /// Last completed local commit barrier; zero before the first connection.
    pub generation: u64,
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

#[derive(Clone, Copy)]
pub(crate) struct AddressRequest {
    pub peer: usize,
    pub address: SocketAddrV6,
}

pub(crate) struct Shared {
    pub config: Validated,
    pub wakes: Vec<Arc<Wake>>,
    pub owners: Vec<Box<[AtomicU8]>>,
    pub status: Vec<AtomicU8>,
    pub generations: Vec<AtomicU64>,
    pub counters: Vec<Counters>,
    pub session_counters: Vec<Counters>,
    pub peer_counters: Vec<Counters>,
    pub receive_counters: Vec<LaneCounters>,
    pub transmit_counters: Vec<Vec<LaneCounters>>,
    pub bound: Vec<AtomicU64>,
    pub requests: Mutex<spsc::Producer<AddressRequest>>,
    pub address_pending: Vec<AtomicBool>,
    pub address_ready: Vec<AtomicU64>,
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

    /// Queues an IPv6 candidate for a configured peer without changing its port.
    ///
    /// Call after readiness. Resolution belongs to the caller. A new address
    /// carries data only after a fresh handshake and worker commit barrier.
    /// A pending candidate preserves the old connection and transmit sockets.
    ///
    /// WouldBlock means startup has not published the control queue, an update
    /// for this peer is already queued, or the bounded command ring is full.
    /// Updates serialize while sessions still use both address slots.
    /// This cold control operation takes a mutex; packet workers never take it.
    pub fn supply_peer_address(
        &self,
        peer: usize,
        address: Ipv6Addr,
        scope_id: u32,
    ) -> io::Result<()> {
        if !crate::config::unicast(address) || (address.is_unicast_link_local() && scope_id == 0) {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        if self.quitting() || self.stopped() {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        let shared = self.0.shared.get().ok_or(io::ErrorKind::WouldBlock)?;
        let configured = shared
            .config
            .config()
            .peers
            .get(peer)
            .ok_or(io::ErrorKind::InvalidInput)?;
        let pending = &shared.address_pending[peer];
        pending
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| io::ErrorKind::WouldBlock)?;
        let request = AddressRequest {
            peer,
            address: SocketAddrV6::new(address, configured.peer_port, 0, scope_id),
        };
        let result = shared
            .requests
            .lock()
            .map_err(|_| io::Error::from(io::ErrorKind::Other))
            .and_then(|mut queue| {
                queue
                    .try_push(request)
                    .map_err(|_| io::ErrorKind::WouldBlock.into())
            });
        if result.is_err() {
            pending.store(false, Ordering::Release);
            return result;
        }
        shared.wakes[0].notify()
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
            generation: self.0.shared.get()?.generations[index].load(Ordering::Acquire),
        })
    }

    /// Aggregates a session's worker totals across all its connections.
    ///
    /// Counts and occupancies sum; RTT/delay use the maximum and MTU the
    /// smallest nonzero value. MTU state prefers failed, probing, baseline,
    /// confirmed, then unknown. Unsupported stage-3 gauges remain zero.
    pub fn session_counters(&self, session: usize) -> Option<Snapshot> {
        let shared = self.0.shared.get()?;
        shared.config.config().sessions.get(session)?;
        let start = session * shared.workers();
        let mut result = Snapshot::default();
        for counters in &shared.session_counters[start..start + shared.workers()] {
            result.merge(counters.snapshot());
        }
        Some(result)
    }

    /// Aggregates this peer's sessions plus peer-owned socket setup errors.
    pub fn peer_counters(&self, peer: usize) -> Option<Snapshot> {
        let shared = self.0.shared.get()?;
        shared.config.config().peers.get(peer)?;
        let mut result = Snapshot::default();
        for session in 0..shared.config.config().sessions.len() {
            if shared.config.peer_for_session(session) == Some(peer) {
                result.merge(self.session_counters(session)?);
            }
        }
        let start = peer * shared.workers();
        for counters in &shared.peer_counters[start..start + shared.workers()] {
            result.merge(counters.snapshot());
        }
        Some(result)
    }

    /// Samples a current local transmit lane, numbered from one.
    pub fn transmit_lane_counters(&self, session: usize, lane: u16) -> Option<LaneSnapshot> {
        self.0
            .shared
            .get()?
            .transmit_counters
            .get(session)?
            .get(usize::from(lane.checked_sub(1)?))?
            .snapshot()
    }

    /// Samples a registered remote receive lane from its current worker owner.
    ///
    /// None means unregistered, retired or changing identity during this read.
    /// Drops before owner admission appear in worker/session totals.
    pub fn receive_lane_counters(&self, session: usize, lane: u16) -> Option<LaneSnapshot> {
        let shared = self.0.shared.get()?;
        let owner = shared.owners.get(session)?[usize::from(lane)].load(Ordering::Acquire);
        if owner == 0 || owner == 255 {
            return None;
        }
        let start = (session * shared.workers() + usize::from(owner - 1)) * LANES;
        shared.receive_counters[start..start + LANES]
            .iter()
            .filter_map(LaneCounters::snapshot)
            .find(|snapshot| snapshot.lane == lane)
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

    pub fn peer_bound(&self, peer: usize) -> bool {
        let endpoint = self.config.endpoint_for_peer(peer).unwrap();
        self.bound[endpoint].load(Ordering::Acquire) == u64::MAX >> (64 - self.workers())
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
/// Initial addresses come from Config. The caller can supply initial or refreshed
/// IPv6 candidates through Control after TAP readiness.
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
    let peers = config.config().peers.len();
    let endpoints = config.config().endpoints.len();
    if peers > (u32::MAX as usize >> 7) {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let (request_tx, request_rx) = spsc::channel();
    let wakes = (0..count)
        .map(|_| Wake::new().map(Arc::new))
        .collect::<io::Result<Vec<_>>>()?;
    let transmit_counters = (0..sessions)
        .map(|session| {
            let peer = config.peer_for_session(session).unwrap();
            (0..config.config().peers[peer].tx_lanes)
                .map(|_| LaneCounters::new())
                .collect()
        })
        .collect();
    let shared = Arc::new(Shared {
        config,
        wakes,
        owners: (0..sessions)
            .map(|_| (0..65_536).map(|_| AtomicU8::new(0)).collect())
            .collect(),
        status: (0..sessions).map(|_| AtomicU8::new(0)).collect(),
        generations: (0..sessions).map(|_| AtomicU64::new(0)).collect(),
        counters: (0..count).map(|_| Counters::new()).collect(),
        session_counters: (0..sessions * count).map(|_| Counters::new()).collect(),
        peer_counters: (0..peers * count).map(|_| Counters::new()).collect(),
        receive_counters: (0..sessions * count * LANES)
            .map(|_| LaneCounters::new())
            .collect(),
        transmit_counters,
        bound: (0..endpoints).map(|_| AtomicU64::new(0)).collect(),
        requests: Mutex::new(request_tx),
        address_pending: (0..peers).map(|_| AtomicBool::new(false)).collect(),
        address_ready: (0..peers * count).map(|_| AtomicU64::new(0)).collect(),
    });
    let mut workers = (0..count)
        .map(|id| Worker::new(id, Arc::clone(&shared), control.clone()))
        .collect::<io::Result<Vec<_>>>()?;
    workers[0].requests = Some(request_rx);
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
    let commands = spsc::CAPACITY * size_of::<AddressRequest>() + 512;
    let queues = workers.checked_mul(workers - 1)?.checked_mul(
        spsc::CAPACITY
            .checked_mul(size_of::<Event>())?
            .checked_add(512)?,
    )?;
    let reassembly = sessions
        .checked_mul(LANES)?
        .checked_mul(reassembly::Lane::storage_bytes())?;
    let lane_counters = sessions
        .checked_mul(workers)?
        .checked_mul(LANES)?
        .checked_mul(size_of::<LaneCounters>())?;
    let totals = sessions
        .checked_add(raw.peers.len())?
        .checked_mul(workers)?
        .checked_mul(size_of::<Counters>())?;
    let transmit = (0..sessions).try_fold(0_usize, |total, session| {
        let peer = config.peer_for_session(session)?;
        total.checked_add(
            raw.peers[peer]
                .tx_lanes
                .checked_mul(size_of::<LaneCounters>())?,
        )
    })?;
    let shared = sessions
        .checked_mul(65_536 + size_of::<Box<[AtomicU8]>>() + 1 + size_of::<AtomicU64>())?
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
        commands,
        lane_counters,
        totals,
        transmit,
        raw.endpoints.len().checked_mul(size_of::<AtomicU64>())?,
        raw.peers
            .len()
            .checked_mul(1 + workers.checked_mul(size_of::<AtomicU64>())?)?,
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
    fn candidate_validation_and_startup_shutdown_errors_precede_queue_access() {
        let control = Control::new();
        for address in [
            Ipv6Addr::UNSPECIFIED,
            Ipv6Addr::from([0xff; 16]),
            "::ffff:127.0.0.1".parse().unwrap(),
            "fe80::1".parse().unwrap(),
        ] {
            assert_eq!(
                control
                    .supply_peer_address(0, address, 0)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidInput
            );
        }
        assert_eq!(
            control
                .supply_peer_address(0, Ipv6Addr::LOCALHOST, 0)
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        control.shutdown();
        assert_eq!(
            control
                .supply_peer_address(0, Ipv6Addr::LOCALHOST, 0)
                .unwrap_err()
                .kind(),
            io::ErrorKind::BrokenPipe
        );
    }

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
