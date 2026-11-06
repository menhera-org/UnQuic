//! Worker-owned polling primitives and transport loop support.

use crate::sys::{cvt, invalid};
use std::{
    io,
    mem::{size_of, zeroed},
    os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd},
    sync::Arc,
    time::Duration,
};

const WAKE_TOKEN: u64 = 0;
const TIMER_TOKEN: u64 = 1;

/// Nonblocking eventfd shared only for batch notifications.
pub(crate) struct Wake(OwnedFd);

impl Wake {
    pub(crate) fn new() -> io::Result<Self> {
        // SAFETY: eventfd has no pointer arguments and returns a new descriptor.
        let fd = cvt(unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) })?;
        // SAFETY: fd is a newly created descriptor with one owner.
        Ok(Self(unsafe { OwnedFd::from_raw_fd(fd) }))
    }

    pub(crate) fn notify(&self) -> io::Result<()> {
        let value = 1_u64;
        loop {
            // SAFETY: value contains exactly the eight bytes eventfd consumes.
            let count = unsafe {
                libc::write(
                    self.0.as_raw_fd(),
                    (&value as *const u64).cast(),
                    size_of::<u64>(),
                )
            };
            if count == size_of::<u64>() as isize {
                return Ok(());
            }
            let error = if count < 0 {
                io::Error::last_os_error()
            } else {
                io::ErrorKind::WriteZero.into()
            };
            match error.kind() {
                io::ErrorKind::Interrupted => continue,
                io::ErrorKind::WouldBlock => return Ok(()),
                _ => return Err(error),
            }
        }
    }

    fn drain(&self) -> io::Result<()> {
        drain_counter(self.0.as_fd())
    }
}

/// One worker's epoll and absolute monotonic timer.
pub(crate) struct Reactor {
    poll: OwnedFd,
    timer: OwnedFd,
    wake: Arc<Wake>,
    deadline: Option<Duration>,
}

impl Reactor {
    pub(crate) fn new(wake: Arc<Wake>) -> io::Result<Self> {
        // SAFETY: epoll_create1 has no pointer arguments and returns a new fd.
        let fd = cvt(unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) })?;
        // SAFETY: fd is a new descriptor with one owner.
        let poll = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: timerfd_create has no pointer arguments and returns a new fd.
        let fd = cvt(unsafe {
            libc::timerfd_create(
                libc::CLOCK_MONOTONIC,
                libc::TFD_NONBLOCK | libc::TFD_CLOEXEC,
            )
        })?;
        // SAFETY: fd is a new descriptor with one owner.
        let timer = unsafe { OwnedFd::from_raw_fd(fd) };
        let reactor = Self {
            poll,
            timer,
            wake,
            deadline: None,
        };
        reactor.add(reactor.wake.0.as_fd(), WAKE_TOKEN, libc::EPOLLIN as u32)?;
        reactor.add(reactor.timer.as_fd(), TIMER_TOKEN, libc::EPOLLIN as u32)?;
        Ok(reactor)
    }

    pub(crate) fn add(&self, fd: BorrowedFd<'_>, token: u64, events: u32) -> io::Result<()> {
        self.control(libc::EPOLL_CTL_ADD, fd, token, events)
    }

    pub(crate) fn modify(&self, fd: BorrowedFd<'_>, token: u64, events: u32) -> io::Result<()> {
        self.control(libc::EPOLL_CTL_MOD, fd, token, events)
    }

    pub(crate) fn remove(&self, fd: BorrowedFd<'_>) -> io::Result<()> {
        self.control(libc::EPOLL_CTL_DEL, fd, 0, 0)
    }

    fn control(
        &self,
        operation: i32,
        fd: BorrowedFd<'_>,
        token: u64,
        events: u32,
    ) -> io::Result<()> {
        let mut event = libc::epoll_event { events, u64: token };
        // SAFETY: event is initialized and writable for the syscall's duration.
        cvt(unsafe {
            libc::epoll_ctl(self.poll.as_raw_fd(), operation, fd.as_raw_fd(), &mut event)
        })?;
        Ok(())
    }

    pub(crate) fn wait(
        &self,
        events: &mut [libc::epoll_event],
        timeout_ms: i32,
    ) -> io::Result<usize> {
        if events.is_empty() || events.len() > i32::MAX as usize || timeout_ms < -1 {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        // SAFETY: events is writable for the bounded number of output records.
        let count = cvt(unsafe {
            libc::epoll_wait(
                self.poll.as_raw_fd(),
                events.as_mut_ptr(),
                events.len() as i32,
                timeout_ms,
            )
        })?;
        Ok(count as usize)
    }

    pub(crate) fn drain(&mut self, token: u64) -> io::Result<()> {
        match token {
            WAKE_TOKEN => self.wake.drain(),
            TIMER_TOKEN => {
                drain_counter(self.timer.as_fd())?;
                self.deadline = None;
                Ok(())
            }
            _ => Ok(()),
        }
    }

    pub(crate) fn arm(&mut self, deadline: Option<Duration>) -> io::Result<()> {
        // An earlier wake still services a later deadline; do not postpone it each turn.
        if self.deadline == deadline
            || matches!((self.deadline, deadline), (Some(armed), Some(due)) if armed <= due)
        {
            return Ok(());
        }
        let value = match deadline {
            Some(value) => value.max(Duration::from_nanos(1)),
            None => Duration::ZERO,
        };
        let seconds = value
            .as_secs()
            .try_into()
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        let spec = libc::itimerspec {
            it_interval: libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            it_value: libc::timespec {
                tv_sec: seconds,
                tv_nsec: value.subsec_nanos().into(),
            },
        };
        // SAFETY: spec is initialized; the old value is not requested.
        cvt(unsafe {
            libc::timerfd_settime(
                self.timer.as_raw_fd(),
                libc::TFD_TIMER_ABSTIME,
                &spec,
                std::ptr::null_mut(),
            )
        })?;
        self.deadline = deadline;
        Ok(())
    }
}

fn drain_counter(fd: BorrowedFd<'_>) -> io::Result<()> {
    let mut value = 0_u64;
    loop {
        // SAFETY: value is a writable eight-byte eventfd/timerfd counter.
        let count = unsafe {
            libc::read(
                fd.as_raw_fd(),
                (&mut value as *mut u64).cast(),
                size_of::<u64>(),
            )
        };
        if count == size_of::<u64>() as isize {
            return Ok(());
        }
        let error = if count < 0 {
            io::Error::last_os_error()
        } else {
            io::ErrorKind::UnexpectedEof.into()
        };
        match error.kind() {
            io::ErrorKind::Interrupted => continue,
            io::ErrorKind::WouldBlock => return Ok(()),
            _ => return Err(error),
        }
    }
}

pub(crate) fn monotonic_now() -> io::Result<Duration> {
    let mut value = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: value is writable; CLOCK_MONOTONIC has a nonnegative epoch.
    cvt(unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut value) })?;
    Ok(Duration::new(value.tv_sec as u64, value.tv_nsec as u32))
}

pub(crate) fn pin_to_allowed(index: usize) -> io::Result<()> {
    // SAFETY: a zeroed cpu_set_t is a valid empty set.
    let mut allowed: libc::cpu_set_t = unsafe { zeroed() };
    // SAFETY: allowed is writable for exactly the supplied size; pid 0 is self.
    cvt(unsafe { libc::sched_getaffinity(0, size_of::<libc::cpu_set_t>(), &mut allowed) })?;
    // SAFETY: allowed is initialized and CPU_COUNT only reads the set.
    let count = unsafe { libc::CPU_COUNT(&allowed) } as usize;
    if count == 0 {
        return Err(invalid("empty CPU affinity mask"));
    }
    let cpu = (0..libc::CPU_SETSIZE as usize)
        .filter(|&cpu| {
            // SAFETY: cpu is within the initialized CPU set's bounds.
            unsafe { libc::CPU_ISSET(cpu, &allowed) }
        })
        .nth(index % count)
        .ok_or_else(|| invalid("invalid CPU affinity mask"))?;
    // SAFETY: allowed is writable and cpu is in bounds.
    unsafe {
        libc::CPU_ZERO(&mut allowed);
        libc::CPU_SET(cpu, &mut allowed);
    }
    // SAFETY: the initialized mask applies only to this calling thread.
    cvt(unsafe { libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &allowed) })?;
    Ok(())
}

use crate::{
    BASE_UDP_PAYLOAD, MAX_FRAME_LEN,
    counters::Snapshot,
    handshake::{
        self, Connection, Effects, Tag,
        barrier::{Barrier, Command, Phase, Progress},
    },
    pack, pmtu,
    reassembly::{self, Admission},
    runtime::{AddressRequest, BUFFERS, Control, LANES, Shared},
    socket::{BATCH_SIZE, ReceiveDatagram, ReceiveSocket, SendDatagram, TransmitSocket},
    spsc::{Consumer, Producer},
    tap::TapQueue,
    wire::{self, Datagram, Header, Record},
};
use std::{collections::VecDeque, net::SocketAddrV6, sync::atomic::Ordering};

const TAP_TOKEN: u64 = 1 << 32;
const TX_TOKEN: u64 = 2 << 32;
const RETRY: Duration = Duration::from_millis(1);
const RECEIVE_BYTES: usize = 65_536;
// A bounded GRO backlog must not consume the transmit pool (or vice versa).
const RECEIVE_BUFFERS: usize = 8;
const _: () = assert!(RECEIVE_BUFFERS <= BATCH_SIZE && RECEIVE_BUFFERS < BUFFERS);

// Yield at datagram boundaries; one final datagram may exceed the record/byte target.
#[derive(Default)]
struct ReceiveBudget {
    datagrams: usize,
    records: usize,
    bytes: usize,
}

impl ReceiveBudget {
    fn exhausted(&self) -> bool {
        self.datagrams >= BATCH_SIZE || self.records >= BATCH_SIZE || self.bytes >= 64 * 1024
    }
}
// Receive quotas follow from fixed lane/slot counts, without shared reservations.
const _: () = assert!(LANES * reassembly::FRAME_CAPACITY <= 8192);
const _: () = assert!(LANES * reassembly::FRAME_CAPACITY * MAX_FRAME_LEN <= 64 * 1024 * 1024);

const fn frame_quota(workers: usize) -> usize {
    let bytes_limited = 64 * 1024 * 1024 / workers / MAX_FRAME_LEN;
    if bytes_limited < BUFFERS {
        bytes_limited
    } else {
        BUFFERS
    }
}

fn tap_lane(frame: &[u8], worker: usize, workers: usize, queues: usize, lanes: usize) -> usize {
    // A session with fewer TAP queues still needs its other transmit workers.
    if queues < workers || worker >= lanes || !pack::prefer_local_lane(frame) {
        return pack::flow_hash(frame) as usize % lanes;
    }
    let count = (lanes - 1 - worker) / workers + 1;
    let slot = if count == 1 {
        0
    } else {
        pack::flow_hash(frame) as usize % count
    };
    worker + slot * workers
}

/// Pool entries move through queues by ownership, including on rejection.
pub(crate) struct Buffer {
    origin: usize,
    rx_counted: bool,
    tx_reserved: bool,
    segments: Option<crate::tap::Segments>,
    rx_reserved: bool,
    offset: usize,
    segment_size: usize,
    session: usize,
    tag: u128,
    lane: usize,
    length: usize,
    deadline: Duration,
    frame_id: u64,
    bytes: [u8; RECEIVE_BYTES],
}

impl Buffer {
    fn advance(&mut self) -> bool {
        self.offset = if self.segment_size == 0 {
            self.length
        } else {
            (self.offset + self.segment_size).min(self.length)
        };
        self.rx_counted = false;
        self.offset < self.length
    }
}

pub(crate) enum Event {
    Receive {
        endpoint: usize,
        socket: ReceiveSocket,
    },
    Packet(Box<Buffer>),
    Frame(Box<Buffer>),
    Free(Box<Buffer>),
    Register {
        buffer: Box<Buffer>,
        lane: Box<reassembly::Lane>,
    },
    LaneFree {
        session: usize,
        lane: Box<reassembly::Lane>,
    },
    Barrier {
        session: usize,
        command: Command,
        path: usize,
    },
    Address(AddressChange),
    Acknowledge {
        session: usize,
        worker: usize,
        command: Command,
    },
    ProbeFeedback {
        session: usize,
        tag: u128,
        lane: u16,
        packet: u64,
        length: u16,
    },
    Reply {
        session: usize,
        tag: u128,
        lane: u16,
        packet: u64,
        length: u16,
    },
    Retire {
        session: usize,
        tag: u128,
        lane: u16,
    },
}

struct Session {
    tap: Option<TapQueue>,
    tap_registered: bool,
    tap_writable: bool,
    tap_readable: bool,
    tap_blocked: bool,
    tap_read_limit: usize,
    tap_retry: Duration,
    connection: Option<Connection>,
    path: usize,
    rx_index: Box<[u8]>,
    rx: Vec<Option<Box<reassembly::Lane>>>,
    free: Vec<Box<reassembly::Lane>>,
    returning: u64,
    retirements: Box<[u64]>,
    retire_pending: usize,
    tx: Vec<Option<pack::Lane>>,
    batches: Vec<Option<Box<TxBatch>>>,
    tx_ready: u64,
    command: Option<Command>,
    acknowledged: Option<Command>,
    installed: Option<Command>,
    totals: Snapshot,
    // One retention budget covers both TAP readers and transmit owners.
    tx_workers: usize,
    tx_retained: usize,
    tx_bytes: usize,
    tap_inputs: VecDeque<Box<Buffer>>,
    rx_totals: Vec<Snapshot>,
    tx_totals: Vec<Snapshot>,
    rx_dirty: u64,
    tx_dirty: u64,
}

struct SessionControl {
    state: handshake::Session,
    barrier: Barrier,
    effects: Effects,
    carrier: bool,
    retry: Duration,
    probe_due: Duration,
    probe: Option<(u64, u16)>,
    handshake_path: usize,
    install_path: usize,
}

struct TxBatch {
    plan: pack::Batch,
    frames: [Option<Box<Buffer>>; pack::BATCH_CAPACITY],
    len: usize,
    deferred: u64,
}

impl TxBatch {
    fn new() -> Self {
        Self {
            plan: pack::Batch::new(),
            frames: std::array::from_fn(|_| None),
            len: 0,
            deferred: 0,
        }
    }

    fn prepare(&mut self, lane: &mut pack::Lane, limit: usize) -> Result<(), pack::PackError> {
        let mut inputs = [pack::FrameInput {
            id: 0,
            bytes: &[],
            offset: 0,
        }; pack::BATCH_CAPACITY];
        for (input, frame) in inputs.iter_mut().zip(&self.frames[..self.len]) {
            let frame = frame.as_ref().unwrap();
            *input = pack::FrameInput {
                id: frame.frame_id,
                bytes: &frame.bytes[..frame.length],
                offset: frame.offset,
            };
        }
        self.plan.prepare(lane, &inputs[..self.len], limit)
    }

    fn compact(&mut self) {
        let mut len = 0;
        for index in 0..self.len {
            if self.frames[index].is_some() {
                self.frames.swap(len, index);
                len += 1;
            }
        }
        self.len = len;
        self.plan.clear();
    }
}

struct Tx {
    socket: Option<TransmitSocket>,
    blocked: bool,
    mtu: pmtu::Path,
    failed_payload: usize,
}

#[derive(Clone, Copy)]
pub(crate) struct AddressChange {
    request: AddressRequest,
    path: usize,
    generation: u64,
}

struct Path {
    address: Option<SocketAddrV6>,
    probe_session: Option<usize>,
    lanes: Vec<Tx>,
}

struct PeerPaths {
    paths: [Path; 2],
    generation: u64,
    preparing: Option<AddressChange>,
    totals: Snapshot,
}

#[derive(Default)]
struct PeerControl {
    pending: Option<AddressRequest>,
    change: Option<AddressChange>,
    unsent: u64,
}

pub(crate) struct Worker {
    tap_reader: Option<crate::tap::Reader>,
    tap_start: usize,
    id: usize,
    shared: Arc<Shared>,
    control: Control,
    reactor: Reactor,
    pub outgoing: Vec<Option<Producer<Event>>>,
    pub incoming: Vec<Option<Consumer<Event>>>,
    sessions: Vec<Session>,
    controls: Vec<SessionControl>,
    receive: Vec<Option<ReceiveSocket>>,
    receive_overflow: Vec<u32>,
    pending_receive: VecDeque<(usize, usize, ReceiveSocket)>,
    receive_enabled: bool,
    rx_retained: usize,
    transmit: Vec<PeerPaths>,
    mtu_jobs: Vec<(usize, usize, usize)>,
    mtu_cursor: usize,
    mtu_start: usize,
    mtu_due: Duration,
    mtu_next: Duration,
    probe_slot: Option<u128>,
    pub requests: Option<Consumer<AddressRequest>>,
    peer_controls: Vec<PeerControl>,
    free: Vec<Box<Buffer>>,
    returns: Vec<Vec<Box<Buffer>>>,
    pending: VecDeque<Box<Buffer>>,
    received: VecDeque<Box<Buffer>>,
    ready: VecDeque<(usize, usize)>,
    notifications: u64,
    counters: Snapshot,
    setup_due: Duration,
    retry_due: Option<Duration>,
}

impl Worker {
    pub fn storage_bytes(config: &crate::config::Config, workers: usize) -> Option<usize> {
        let mut per_worker = size_of::<Self>()
            .checked_add(crate::tap::Reader::storage_bytes()?)?
            .checked_add(BUFFERS.checked_mul(size_of::<Buffer>() + size_of::<Box<Buffer>>())?)?
            .checked_add(workers.checked_mul(
                size_of::<Option<Producer<Event>>>() + size_of::<Option<Consumer<Event>>>(),
            )?)?
            .checked_add(
                workers
                    .checked_mul(BUFFERS)?
                    .checked_mul(3 * size_of::<Box<Buffer>>())?,
            )?
            .checked_add(workers.checked_mul(size_of::<Vec<Box<Buffer>>>())?)?
            .checked_add(
                config
                    .endpoints
                    .len()
                    .checked_mul(size_of::<Option<ReceiveSocket>>() + size_of::<u32>())?,
            )?;
        for endpoint in &config.endpoints {
            per_worker = per_worker.checked_add(
                endpoint
                    .bind_device
                    .as_ref()
                    .map_or(0, |d| d.as_bytes_with_nul().len()),
            )?;
        }
        for peer in &config.peers {
            per_worker = per_worker
                .checked_add(size_of::<PeerPaths>())?
                .checked_add(peer.tx_lanes.checked_mul(2 * size_of::<Tx>())?)?;
        }
        let mut batches = config.peers.iter().try_fold(0_usize, |bytes, peer| {
            bytes.checked_add(
                peer.tx_lanes
                    .checked_mul(2 * size_of::<(usize, usize, usize)>())?,
            )
        })?;
        for session in &config.sessions {
            let peer = config.peers.iter().find(|p| p.name == session.peer)?;
            batches = batches.checked_add(
                peer.tx_lanes
                    .checked_mul(size_of::<TxBatch>() + size_of::<(usize, usize)>())?,
            )?;
            per_worker = per_worker
                .checked_add(size_of::<Session>() + 65_536 + 8192 + LANES * size_of::<Snapshot>())?
                .checked_add(BUFFERS * size_of::<Box<Buffer>>())?
                .checked_add(LANES.checked_mul(size_of::<Option<Box<reassembly::Lane>>>())?)?
                .checked_add(peer.tx_lanes.checked_mul(
                    size_of::<Option<pack::Lane>>()
                        + size_of::<Option<Box<TxBatch>>>()
                        + size_of::<Snapshot>(),
                )?)?;
        }
        per_worker
            .checked_mul(workers)?
            .checked_add(batches)?
            .checked_add(
                config
                    .endpoints
                    .len()
                    .checked_mul(workers)?
                    .checked_mul(size_of::<(usize, usize, ReceiveSocket)>())?,
            )?
            .checked_add(config.peers.len().checked_mul(size_of::<PeerControl>())?)?
            .checked_add(config.sessions.len().checked_mul(
                size_of::<SessionControl>() + LANES * size_of::<Box<reassembly::Lane>>(),
            )?)
    }

    pub fn new(id: usize, shared: Arc<Shared>, control: Control) -> io::Result<Self> {
        let count = shared.workers();
        let config = shared.config.config();
        let mut sessions = Vec::with_capacity(config.sessions.len());
        let mut controls = Vec::with_capacity(if id == 0 { config.sessions.len() } else { 0 });
        for (index, session) in config.sessions.iter().enumerate() {
            let peer = shared.config.peer_for_session(index).unwrap();
            let endpoint = shared.config.endpoint_for_peer(peer).unwrap();
            let mut free = Vec::with_capacity(if id == 0 { LANES } else { 0 });
            if id == 0 {
                for _ in 0..LANES {
                    free.push(Box::new(
                        reassembly::Lane::new(1, MAX_FRAME_LEN as u16).unwrap(),
                    ));
                }
                controls.push(SessionControl {
                    state: handshake::Session::new(
                        wire::Params {
                            port: config.endpoints[endpoint].listen_port,
                            max_frame_len: MAX_FRAME_LEN as u16,
                            mandatory_features: 0,
                            optional_features: 0,
                        },
                        config.peers[peer].peer_port,
                        session.inner_mtu + 22,
                    )
                    .map_err(|_| io::ErrorKind::InvalidInput)?,
                    barrier: Barrier::new(count).unwrap(),
                    effects: Effects::default(),
                    carrier: false,
                    retry: Duration::ZERO,
                    probe_due: Duration::ZERO,
                    probe: None,
                    handshake_path: 0,
                    install_path: 0,
                });
            }
            sessions.push(Session {
                tap: None,
                tap_registered: false,
                tap_writable: false,
                tap_readable: true,
                tap_blocked: false,
                tap_read_limit: 8,
                tap_retry: Duration::ZERO,
                connection: None,
                path: 0,
                rx_index: vec![0; 65_536].into_boxed_slice(),
                rx: (0..LANES).map(|_| None).collect(),
                free,
                returning: 0,
                retirements: vec![0; 1024].into_boxed_slice(),
                retire_pending: 0,
                tx: (0..config.peers[peer].tx_lanes).map(|_| None).collect(),
                batches: (0..config.peers[peer].tx_lanes)
                    .map(|lane| (lane % count == id).then(|| Box::new(TxBatch::new())))
                    .collect(),
                tx_ready: 0,
                command: None,
                acknowledged: None,
                installed: None,
                totals: Snapshot::default(),
                tx_workers: session
                    .queue_count
                    .max(config.peers[peer].tx_lanes.min(count)),
                tx_retained: 0,
                tx_bytes: 0,
                tap_inputs: VecDeque::with_capacity(BUFFERS),
                rx_totals: vec![Snapshot::default(); LANES],
                tx_totals: vec![Snapshot::default(); config.peers[peer].tx_lanes],
                rx_dirty: 0,
                tx_dirty: 0,
            });
        }
        let free = (0..BUFFERS)
            .map(|_| {
                Box::new(Buffer {
                    origin: id,
                    rx_counted: false,
                    tx_reserved: false,
                    segments: None,
                    rx_reserved: false,
                    offset: 0,
                    segment_size: 0,
                    session: 0,
                    tag: 0,
                    lane: 0,
                    length: 0,
                    deadline: Duration::ZERO,
                    frame_id: 0,
                    bytes: [0; RECEIVE_BYTES],
                })
            })
            .collect();
        let batch_count = sessions
            .iter()
            .map(|s| s.batches.iter().filter(|b| b.is_some()).count())
            .sum();
        let tap_reader = crate::tap::Reader::new().ok();
        let counters = Snapshot {
            tap_read_fallbacks: u64::from(tap_reader.is_none()),
            ..Snapshot::default()
        };
        let mut mtu_jobs = Vec::with_capacity(
            config
                .peers
                .iter()
                .map(|peer| 2 * (id..peer.tx_lanes).step_by(count).count())
                .sum(),
        );
        for (peer, config) in config.peers.iter().enumerate() {
            for path in 0..2 {
                for lane in (id..config.tx_lanes).step_by(count) {
                    mtu_jobs.push((peer, path, lane));
                }
            }
        }
        Ok(Self {
            tap_reader,
            tap_start: 0,
            id,
            control,
            reactor: Reactor::new(Arc::clone(&shared.wakes[id]))?,
            outgoing: (0..count).map(|_| None).collect(),
            incoming: (0..count).map(|_| None).collect(),
            receive: (0..config.endpoints.len()).map(|_| None).collect(),
            receive_overflow: vec![0; config.endpoints.len()],
            pending_receive: VecDeque::with_capacity(if id == 0 {
                config.endpoints.len() * count
            } else {
                0
            }),
            receive_enabled: true,
            rx_retained: 0,
            transmit: config
                .peers
                .iter()
                .map(|peer| PeerPaths {
                    paths: std::array::from_fn(|slot| Path {
                        address: if slot == 0 { peer.socket_addr() } else { None },
                        probe_session: None,
                        lanes: (0..peer.tx_lanes)
                            .map(|_| Tx {
                                socket: None,
                                blocked: false,
                                mtu: pmtu::Path::new(peer.outer_payload_limit, Duration::ZERO)
                                    .unwrap(),
                                failed_payload: 0,
                            })
                            .collect(),
                    }),
                    generation: 0,
                    preparing: None,
                    totals: Snapshot::default(),
                })
                .collect(),
            mtu_jobs,
            mtu_cursor: 0,
            mtu_start: 0,
            mtu_due: Duration::ZERO,
            mtu_next: Duration::MAX,
            probe_slot: None,
            requests: None,
            peer_controls: (0..if id == 0 { config.peers.len() } else { 0 })
                .map(|_| PeerControl::default())
                .collect(),
            sessions,
            controls,
            free,
            returns: (0..count).map(|_| Vec::with_capacity(BUFFERS)).collect(),
            pending: VecDeque::with_capacity(count * BUFFERS),
            received: VecDeque::with_capacity(count * BUFFERS),
            ready: VecDeque::with_capacity(batch_count),
            notifications: 0,
            counters,
            setup_due: Duration::ZERO,
            retry_due: None,
            shared,
        })
    }

    pub fn attach_tap(&mut self, session: usize, tap: TapQueue) -> io::Result<()> {
        self.reactor.add(
            tap.as_fd(),
            TAP_TOKEN | session as u64,
            libc::EPOLLIN as u32,
        )?;
        self.sessions[session].tap = Some(tap);
        self.sessions[session].tap_registered = true;
        self.sessions[session].tap_readable = true;
        Ok(())
    }

    pub fn run(mut self) -> io::Result<()> {
        struct Stop(Control);
        impl Drop for Stop {
            fn drop(&mut self) {
                self.0.shutdown();
            }
        }
        let _stop = Stop(self.control.clone());
        if pin_to_allowed(self.id).is_err() {
            self.counters.affinity_failures = self.counters.affinity_failures.wrapping_add(1);
        }
        let mut events = [libc::epoll_event { events: 0, u64: 0 }; 128];
        let mut immediate = true;
        let mut event_start = 0;
        let mut publish_due = Duration::ZERO;
        let mut publish_turns = 0_u8;
        while !self.control.quitting() {
            let now = monotonic_now()?;
            let mut more = self.drain_queues(now)?;
            self.flush_returns();
            if now >= self.setup_due {
                self.setup(now)?;
            }
            more |= self.distribute_receivers();
            self.addresses(now)?;
            self.progress_barriers(now)?;
            self.timers(now)?;
            more |= self.mtu_timers(now)?;
            more |= self.drain_packets(now)?;
            more |= self.drain_tap_inputs(now)?;
            self.flush_frames(now)?;
            self.flush_taps(now)?;
            self.flush_notifications()?;
            more |= self
                .pending
                .iter()
                .map(|b| (b.session, b.lane))
                .chain(self.ready.iter().copied())
                .any(|(session, lane)| {
                    let tx = &self.transmit[self.shared.config.peer_for_session(session).unwrap()]
                        .paths[self.sessions[session].path]
                        .lanes[lane];
                    !tx.blocked && tx.mtu.payload_limit() != 0
                });
            self.receive_interest(more)?;
            publish_turns = publish_turns.wrapping_add(1);
            if now >= publish_due || publish_turns == 0 {
                self.publish_counters();
                publish_due = now + Duration::from_millis(1);
                publish_turns = 0;
            }
            let mut deadline = self.deadline(now);
            if self.has_backpressure() {
                let due = self.retry_due.get_or_insert(now + RETRY);
                if *due <= now {
                    *due = now + RETRY;
                }
                deadline = Some(deadline.map_or(*due, |d| d.min(*due)));
            } else {
                self.retry_due = None;
            }
            self.reactor.arm(deadline)?;
            let count = match self
                .reactor
                .wait(&mut events, if more || immediate { 0 } else { -1 })
            {
                Ok(count) => count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            };
            immediate = false;
            let now = monotonic_now()?;
            let start = event_start.min(count);
            event_start = if start + 1 < count { start + 1 } else { 0 };
            for event in events[start..count].iter().chain(&events[..start]) {
                let token = event.u64;
                match token >> 32 {
                    1 => {
                        let session = token as u32 as usize;
                        if event.events & libc::EPOLLOUT as u32 != 0 {
                            self.sessions[session].tap_blocked = false;
                        }
                        if event.events & libc::EPOLLIN as u32 != 0 {
                            self.read_tap(session, now)?;
                        }
                    }
                    2 => {
                        let peer = ((token as u32) >> 7) as usize;
                        let path = ((token as u32) >> 6 & 1) as usize;
                        let lane = token as usize & 63;
                        let tx = &mut self.transmit[peer].paths[path].lanes[lane];
                        if tx.blocked && event.events & libc::EPOLLOUT as u32 != 0 {
                            self.reactor
                                .modify(tx.socket.as_ref().unwrap().as_fd(), token, 0)?;
                            tx.blocked = false;
                        }
                        if event.events & libc::EPOLLERR as u32 != 0 {
                            self.mtu_error(peer, path, lane, now)?;
                        }
                    }
                    _ if token >= 2 => self.read_socket(token as usize - 2, now)?,
                    _ => self.reactor.drain(token)?,
                }
            }
        }
        self.publish_counters();
        Ok(())
    }

    fn publish_counters(&mut self) {
        self.counters.reassembly_frames = self
            .sessions
            .iter()
            .map(|s| s.totals.reassembly_frames)
            .sum();
        self.counters.reassembly_bytes = self
            .sessions
            .iter()
            .map(|s| s.totals.reassembly_bytes)
            .sum();
        self.shared.counters[self.id].publish(&self.counters);
        for (peer, paths) in self.transmit.iter().enumerate() {
            self.shared.peer_counters[peer * self.shared.workers() + self.id]
                .publish(&paths.totals);
        }
        for (session, local) in self.sessions.iter_mut().enumerate() {
            self.shared.session_counters[session * self.shared.workers() + self.id]
                .publish(&local.totals);
            let mut dirty = local.rx_dirty;
            let base = (session * self.shared.workers() + self.id) * LANES;
            while dirty != 0 {
                let slot = dirty.trailing_zeros() as usize;
                dirty &= dirty - 1;
                self.shared.receive_counters[base + slot]
                    .counters
                    .publish(&local.rx_totals[slot]);
            }
            dirty = local.tx_dirty;
            while dirty != 0 {
                let slot = dirty.trailing_zeros() as usize;
                dirty &= dirty - 1;
                self.shared.transmit_counters[session][slot]
                    .counters
                    .publish(&local.tx_totals[slot]);
            }
            local.rx_dirty = 0;
            local.tx_dirty = 0;
        }
    }

    fn install_rx_counters(&mut self, session: usize, slot: usize) {
        let local = &mut self.sessions[session];
        let lane = local.rx[slot].as_ref().unwrap().id();
        local.rx_totals[slot] = Snapshot::default();
        local.rx_dirty &= !(1_u64 << slot);
        let index = (session * self.shared.workers() + self.id) * LANES + slot;
        self.shared.receive_counters[index].replace(
            lane,
            local.installed.unwrap().generation,
            &local.rx_totals[slot],
        );
    }

    fn refresh_rx_counters(&mut self, session: usize, slot: usize) {
        let local = &mut self.sessions[session];
        let lane = local.rx[slot].as_ref().unwrap();
        let totals = &mut local.rx_totals[slot];
        let expired = lane.expired_frames().wrapping_sub(totals.expired_frames);
        let duplicates = lane
            .duplicate_pieces()
            .wrapping_sub(totals.duplicate_pieces);
        self.counters.expired_frames = self.counters.expired_frames.wrapping_add(expired);
        self.counters.duplicate_pieces = self.counters.duplicate_pieces.wrapping_add(duplicates);
        local.totals.expired_frames = local.totals.expired_frames.wrapping_add(expired);
        local.totals.duplicate_pieces = local.totals.duplicate_pieces.wrapping_add(duplicates);
        totals.expired_frames = lane.expired_frames();
        totals.duplicate_pieces = lane.duplicate_pieces();
        let frames = lane.retained_frames() as u64;
        let bytes = lane.retained_bytes() as u64;
        if expired != 0
            || duplicates != 0
            || totals.reassembly_frames != frames
            || totals.reassembly_bytes != bytes
        {
            totals.reassembly_frames = frames;
            totals.reassembly_bytes = bytes;
            local.rx_dirty |= 1_u64 << slot;
        }
    }

    fn push(&mut self, worker: usize, event: Event) -> Result<(), Event> {
        debug_assert_ne!(worker, self.id);
        self.outgoing[worker]
            .as_mut()
            .unwrap()
            .try_push(event)
            .map(|()| {
                self.notifications |= 1_u64 << worker;
            })
    }

    fn release(&mut self, mut buffer: Box<Buffer>) {
        let origin = buffer.origin;
        if origin == self.id {
            if buffer.rx_reserved {
                self.rx_retained -= 1;
                buffer.rx_reserved = false;
            }
            if buffer.tx_reserved {
                self.sessions[buffer.session].tx_retained -= 1;
                self.sessions[buffer.session].tx_bytes -= buffer.length;
                buffer.tx_reserved = false;
            }
            buffer.segments = None;
            buffer.offset = 0;
            buffer.segment_size = 0;
            buffer.rx_counted = false;
            self.free.push(buffer);
        } else if let Err(Event::Free(buffer)) = self.push(origin, Event::Free(buffer)) {
            debug_assert!(self.returns[origin].len() < BUFFERS);
            self.returns[origin].push(buffer);
        }
    }

    fn flush_returns(&mut self) {
        for origin in 0..self.returns.len() {
            while let Some(buffer) = self.returns[origin].pop() {
                if let Err(Event::Free(buffer)) = self.push(origin, Event::Free(buffer)) {
                    self.returns[origin].push(buffer);
                    break;
                }
            }
        }
    }

    fn flush_notifications(&mut self) -> io::Result<()> {
        let mut mask = std::mem::take(&mut self.notifications);
        while mask != 0 {
            let worker = mask.trailing_zeros() as usize;
            mask &= mask - 1;
            self.shared.wakes[worker].notify()?;
        }
        Ok(())
    }

    fn has_backpressure(&self) -> bool {
        self.returns.iter().any(|v| !v.is_empty())
            || self
                .sessions
                .iter()
                .any(|s| s.command.is_some() || s.acknowledged.is_some() || s.retire_pending != 0)
            || self.controls.iter().any(|s| s.barrier.is_active())
            || self.peer_controls.iter().any(|p| p.unsent != 0)
    }

    fn address(&mut self, change: AddressChange) -> io::Result<()> {
        let paths = &mut self.transmit[change.request.peer];
        if change.generation <= paths.generation {
            return Ok(());
        }
        let path = &mut paths.paths[change.path];
        for tx in &mut path.lanes {
            if let Some(socket) = &tx.socket {
                self.reactor.remove(socket.as_fd())?;
            }
            tx.socket = None;
            tx.blocked = false;
            tx.failed_payload = 0;
            tx.mtu = pmtu::Path::new(
                self.shared.config.config().peers[change.request.peer].outer_payload_limit,
                Duration::ZERO,
            )
            .unwrap();
        }
        path.probe_session = None;
        path.address = Some(change.request.address);
        paths.generation = change.generation;
        paths.preparing = Some(change);
        self.setup_due = Duration::ZERO;
        Ok(())
    }

    fn addresses(&mut self, now: Duration) -> io::Result<()> {
        if let Some(requests) = &mut self.requests {
            for _ in 0..BATCH_SIZE {
                let Some(request) = requests.try_pop() else {
                    break;
                };
                debug_assert!(self.peer_controls[request.peer].pending.is_none());
                self.peer_controls[request.peer].pending = Some(request);
            }
        }
        for peer in 0..self.peer_controls.len() {
            if let Some(request) = self.peer_controls[peer].pending {
                let mut used = [false; 2];
                let mut barrier = false;
                for (index, session) in self.sessions.iter().enumerate() {
                    if self.shared.config.peer_for_session(index) == Some(peer) {
                        if session.connection.is_some() {
                            used[session.path] = true;
                        }
                        barrier |= self.controls[index].barrier.is_active();
                    }
                }
                if barrier {
                    continue;
                }
                let selected = self
                    .controls
                    .iter()
                    .enumerate()
                    .find(|(session, _)| {
                        self.shared.config.peer_for_session(*session) == Some(peer)
                    })
                    .map_or(0, |(_, control)| control.handshake_path);
                if (self.peer_controls[peer].change.is_none()
                    && self.transmit[peer].paths[selected].address == Some(request.address))
                    || self.peer_controls[peer]
                        .change
                        .is_some_and(|change| change.request.address == request.address)
                {
                    self.peer_controls[peer].pending = None;
                    self.shared.address_pending[peer].store(false, Ordering::Release);
                    continue;
                }
                if used == [true, true] {
                    continue;
                }
                let path = if used[0] { 1 } else { 0 };
                let generation = self.transmit[peer]
                    .generation
                    .checked_add(1)
                    .ok_or(io::ErrorKind::Other)?;
                let change = AddressChange {
                    request,
                    path,
                    generation,
                };
                for session in 0..self.controls.len() {
                    if self.shared.config.peer_for_session(session) == Some(peer) {
                        self.controls[session].state.cancel_pending(now).unwrap();
                        self.controls[session].handshake_path = self.sessions[session].path;
                        self.controls[session].retry = now + Duration::from_millis(100);
                    }
                }
                self.peer_controls[peer].pending = None;
                self.peer_controls[peer].change = Some(change);
                self.peer_controls[peer].unsent = u64::MAX >> (64 - self.shared.workers());
            }
            let Some(change) = self.peer_controls[peer].change else {
                continue;
            };
            let publishing = self.peer_controls[peer].unsent != 0;
            for worker in 0..self.shared.workers() {
                let bit = 1_u64 << worker;
                if self.peer_controls[peer].unsent & bit == 0 {
                    continue;
                }
                if worker == self.id {
                    self.address(change)?;
                } else if self.push(worker, Event::Address(change)).is_err() {
                    continue;
                }
                self.peer_controls[peer].unsent &= !bit;
            }
            if publishing && self.peer_controls[peer].unsent == 0 {
                self.shared.address_pending[peer].store(false, Ordering::Release);
            }
            if self.peer_controls[peer].unsent == 0
                && (0..self.shared.workers()).all(|worker| {
                    self.shared.address_ready[peer * self.shared.workers() + worker]
                        .load(Ordering::Acquire)
                        == change.generation
                })
            {
                self.peer_controls[peer].change = None;
                for session in 0..self.controls.len() {
                    if self.shared.config.peer_for_session(session) != Some(peer) {
                        continue;
                    }
                    self.controls[session].handshake_path = change.path;
                    let mut effects = self.controls[session].state.reconnect(now, None).unwrap();
                    if effects.needs_tag {
                        effects = self.controls[session]
                            .state
                            .reconnect(now, Tag::generate()?)
                            .unwrap();
                    }
                    self.effects(session, effects, now)?;
                }
            }
        }
        Ok(())
    }

    fn install_receiver(&mut self, endpoint: usize, mut socket: ReceiveSocket) -> io::Result<()> {
        self.counters.receive_buffer_fallbacks += u64::from(socket.buffer_fallback);
        match socket.receive_buffer_limit() {
            Ok(bytes) => self.counters.receive_buffer_bytes += bytes as u64,
            Err(_) => self.counters.socket_errors += 1,
        }
        if socket.enable_gro().is_err() {
            self.counters.socket_errors += 1;
        }
        self.reactor.add(
            socket.as_fd(),
            endpoint as u64 + 2,
            if self.receive_enabled {
                libc::EPOLLIN as u32
            } else {
                0
            },
        )?;
        self.receive[endpoint] = Some(socket);
        self.shared.bound[endpoint].fetch_or(1_u64 << self.id, Ordering::Release);
        Ok(())
    }

    fn distribute_receivers(&mut self) -> bool {
        for _ in 0..BATCH_SIZE {
            let Some((owner, endpoint, socket)) = self.pending_receive.pop_front() else {
                return false;
            };
            if let Err(Event::Receive { endpoint, socket }) =
                self.push(owner, Event::Receive { endpoint, socket })
            {
                self.pending_receive.push_front((owner, endpoint, socket));
                break;
            }
        }
        !self.pending_receive.is_empty()
    }

    fn setup(&mut self, now: Duration) -> io::Result<()> {
        self.setup_due = now + Duration::from_secs(1);
        let shared = Arc::clone(&self.shared);
        let config = shared.config.config();
        if self.id == 0 {
            for (index, endpoint) in config.endpoints.iter().enumerate() {
                if self.receive[index].is_some() {
                    continue;
                }
                let mut group: [Option<ReceiveSocket>; 64] = std::array::from_fn(|_| None);
                let mut failed = false;
                for slot in &mut group[..shared.workers()] {
                    match ReceiveSocket::bind(
                        endpoint.socket_addr(),
                        endpoint.bind_device.as_deref(),
                    ) {
                        Ok(socket) => *slot = Some(socket),
                        Err(_) => {
                            self.counters.socket_errors += 1;
                            failed = true;
                            break;
                        }
                    }
                }
                if failed {
                    continue; // Dropping every member makes a bind retry start a fresh group.
                }
                if group[0]
                    .as_ref()
                    .unwrap()
                    .steer_group(shared.workers())
                    .is_err()
                {
                    // Kernel steering is optional; protocol ownership still routes every packet.
                    self.counters.socket_errors += 1;
                }
                self.install_receiver(index, group[0].take().unwrap())?;
                for (owner, socket) in group.into_iter().enumerate().skip(1) {
                    if let Some(socket) = socket {
                        self.pending_receive.push_back((owner, index, socket));
                    }
                }
            }
        }
        for index in 0..config.peers.len() {
            let endpoint = self.shared.config.endpoint_for_peer(index).unwrap();
            let Some(receiver) = &self.receive[endpoint] else {
                continue;
            };
            let paths = &mut self.transmit[index];
            for (path_index, path) in paths.paths.iter_mut().enumerate() {
                let Some(address) = path.address else {
                    continue;
                };
                for (lane, tx) in path.lanes.iter_mut().enumerate() {
                    if lane % self.shared.workers() != self.id || tx.socket.is_some() {
                        continue;
                    }
                    // A dynamic port must differ from every configured fixed port.
                    for _ in 0..16 {
                        match TransmitSocket::connect(receiver, address) {
                            Ok(mut socket) => {
                                let port = socket.local_addr()?.port();
                                if config.endpoints.iter().any(|e| e.listen_port == port) {
                                    continue;
                                }
                                socket.enable_mtu_discovery()?;
                                let token = TX_TOKEN
                                    | ((index as u64) << 7)
                                    | ((path_index as u64) << 6)
                                    | lane as u64;
                                self.reactor.add(socket.as_fd(), token, 0)?;
                                tx.socket = Some(socket);
                                self.mtu_due = Duration::ZERO;
                                self.mtu_next = Duration::ZERO;
                                break;
                            }
                            Err(_) => {
                                self.counters.socket_errors =
                                    self.counters.socket_errors.wrapping_add(1);
                                paths.totals.socket_errors =
                                    paths.totals.socket_errors.wrapping_add(1);
                                break;
                            }
                        }
                    }
                }
            }
            if let Some(change) = paths.preparing
                && paths.paths[change.path]
                    .lanes
                    .iter()
                    .enumerate()
                    .all(|(lane, tx)| {
                        lane % self.shared.workers() != self.id || tx.socket.is_some()
                    })
            {
                self.shared.address_ready[index * self.shared.workers() + self.id]
                    .store(change.generation, Ordering::Release);
                paths.preparing = None;
                if self.id != 0 {
                    self.notifications |= 1;
                }
            }
        }
        for (index, session) in self.sessions.iter_mut().enumerate() {
            if !session.tap_registered
                && now >= session.tap_retry
                && let Some(tap) = &session.tap
            {
                self.reactor
                    .add(tap.as_fd(), TAP_TOKEN | index as u64, libc::EPOLLIN as u32)?;
                session.tap_registered = true;
                session.tap_writable = false;
                session.tap_readable = true;
                session.tap_blocked = false;
            }
        }
        Ok(())
    }

    fn drain_queues(&mut self, now: Duration) -> io::Result<bool> {
        let mut more = false;
        for from in 0..self.incoming.len() {
            for index in 0..BATCH_SIZE {
                let event = self.incoming[from].as_mut().and_then(Consumer::try_pop);
                let Some(event) = event else { break };
                more |= index + 1 == BATCH_SIZE;
                match event {
                    Event::Receive { endpoint, socket } => {
                        self.install_receiver(endpoint, socket)?;
                        self.setup_due = now;
                    }
                    Event::Packet(buffer) => self.received.push_back(buffer),
                    Event::Frame(buffer) => self.input_frame(buffer, now)?,
                    Event::Free(buffer) => self.release(buffer),
                    Event::Register { mut buffer, lane } => {
                        let session = buffer.session;
                        if self.matches(session, buffer.tag) {
                            let id = lane.id();
                            let slot = self.sessions[session]
                                .rx
                                .iter()
                                .position(Option::is_none)
                                .unwrap();
                            self.sessions[session].rx_index[usize::from(id)] = slot as u8 + 1;
                            self.sessions[session].rx[slot] = Some(lane);
                            self.install_rx_counters(session, slot);
                            self.received.push_back(buffer);
                        } else {
                            self.return_lane(session, lane);
                            if buffer.advance() {
                                self.received.push_back(buffer);
                            } else {
                                self.release(buffer);
                            }
                        }
                    }
                    Event::LaneFree { session, lane } => self.sessions[session].free.push(lane),
                    Event::Barrier {
                        session,
                        command,
                        path,
                    } => self.command(session, command, path),
                    Event::Address(change) => self.address(change)?,
                    Event::Acknowledge {
                        session,
                        worker,
                        command,
                    } => self.acknowledge(session, worker, command, now)?,
                    Event::ProbeFeedback {
                        session,
                        tag,
                        lane,
                        packet,
                        length,
                    } => {
                        if self.matches(session, tag) {
                            self.feedback(session, lane, packet, length, now);
                        }
                    }
                    Event::Reply {
                        session,
                        tag,
                        lane,
                        packet,
                        length,
                    } => {
                        if self.matches(session, tag) {
                            self.send_record(
                                session,
                                Record::ProbeReply {
                                    lane,
                                    packet_number: packet,
                                    payload_len: length,
                                },
                            )?;
                        }
                    }
                    Event::Retire { session, tag, lane } => {
                        if self.matches(session, tag) {
                            self.shared.owners[session][usize::from(lane)]
                                .store(255, Ordering::Release);
                        }
                    }
                }
            }
        }
        Ok(more)
    }

    fn matches(&self, session: usize, tag: u128) -> bool {
        self.sessions[session]
            .connection
            .is_some_and(|c| c.local_tag().get() == tag)
    }

    fn receive_interest(&mut self, more: bool) -> io::Result<()> {
        let enabled = self.rx_retained < RECEIVE_BUFFERS && !self.free.is_empty();
        // Only suppress readiness when waiting for foreign buffers; local work already yields.
        if enabled == self.receive_enabled || (!enabled && more) {
            return Ok(());
        }
        for (index, socket) in self.receive.iter().enumerate() {
            if let Some(socket) = socket {
                self.reactor.modify(
                    socket.as_fd(),
                    index as u64 + 2,
                    if enabled { libc::EPOLLIN as u32 } else { 0 },
                )?;
            }
        }
        self.receive_enabled = enabled;
        Ok(())
    }

    fn read_socket(&mut self, endpoint: usize, now: Duration) -> io::Result<()> {
        let available = (RECEIVE_BUFFERS - self.rx_retained).min(self.free.len());
        let mut buffers: [Option<Box<Buffer>>; RECEIVE_BUFFERS] = std::array::from_fn(|index| {
            if index >= available {
                return None;
            }
            let mut buffer = self.free.pop().unwrap();
            buffer.rx_reserved = true;
            self.rx_retained += 1;
            Some(buffer)
        });
        let available = buffers.iter().take_while(|b| b.is_some()).count();
        if available == 0 {
            return Ok(());
        }
        let mut received: [ReceiveDatagram<'_>; RECEIVE_BUFFERS] =
            std::array::from_fn(|_| ReceiveDatagram::new(&mut []));
        for (output, buffer) in received.iter_mut().zip(&mut buffers).take(available) {
            output.buffer = &mut buffer.as_mut().unwrap().bytes;
        }
        let result = self.receive[endpoint]
            .as_ref()
            .unwrap()
            .recv_batch(&mut received[..available]);
        let lengths: [usize; RECEIVE_BUFFERS] = std::array::from_fn(|i| received[i].length);
        let truncated: [bool; RECEIVE_BUFFERS] = std::array::from_fn(|i| received[i].truncated);
        let segments: [usize; RECEIVE_BUFFERS] = std::array::from_fn(|i| received[i].segment_size);
        let overflows: [Option<u32>; RECEIVE_BUFFERS] =
            std::array::from_fn(|i| received[i].overflow);
        let count = match result {
            Ok(count) => count,
            Err(error) => {
                if !transient(&error) {
                    self.counters.socket_errors = self.counters.socket_errors.wrapping_add(1);
                }
                0
            }
        };
        for (index, buffer) in buffers.iter_mut().enumerate().take(available) {
            let mut buffer = buffer.take().unwrap();
            if index < count
                && let Some(total) = overflows[index]
            {
                self.counters.socket_overflow = self.counters.socket_overflow.wrapping_add(
                    u64::from(total.wrapping_sub(self.receive_overflow[endpoint])),
                );
                self.receive_overflow[endpoint] = total;
            }
            if index < count && !truncated[index] {
                buffer.length = lengths[index];
                buffer.rx_counted = false;
                buffer.offset = 0;
                buffer.segment_size = segments[index];
                buffer.deadline = now;
                let packets = if buffer.segment_size == 0 {
                    1
                } else {
                    buffer.length.div_ceil(buffer.segment_size).max(1)
                };
                self.counters.rx_outer_bytes = self
                    .counters
                    .rx_outer_bytes
                    .wrapping_add((buffer.length + 48 * packets) as u64);
                self.received.push_back(buffer);
            } else {
                if index < count {
                    self.counters.malformed_packets =
                        self.counters.malformed_packets.wrapping_add(1);
                }
                self.release(buffer);
            }
        }
        Ok(())
    }

    fn drain_packets(&mut self, now: Duration) -> io::Result<bool> {
        let mut budget = ReceiveBudget::default();
        while !budget.exhausted() {
            let Some(buffer) = self.received.pop_front() else {
                break;
            };
            if let Some(mut buffer) = self.packet_one(buffer, now, &mut budget)? {
                if buffer.advance() {
                    self.received.push_front(buffer);
                } else {
                    self.release(buffer);
                }
            }
        }
        Ok(!self.received.is_empty())
    }

    fn packet_one(
        &mut self,
        mut buffer: Box<Buffer>,
        now: Duration,
        budget: &mut ReceiveBudget,
    ) -> io::Result<Option<Box<Buffer>>> {
        let end = if buffer.segment_size == 0 {
            buffer.length
        } else {
            (buffer.offset + buffer.segment_size).min(buffer.length)
        };
        let length = end - buffer.offset;
        budget.datagrams += 1;
        budget.bytes += length;
        if length > crate::socket::MAX_UDP_PAYLOAD {
            self.counters.malformed_packets = self.counters.malformed_packets.wrapping_add(1);
            return Ok(Some(buffer));
        }
        let Ok(datagram) = Datagram::decode(&buffer.bytes[buffer.offset..end]) else {
            budget.records += wire::MAX_RECORDS;
            self.counters.malformed_packets = self.counters.malformed_packets.wrapping_add(1);
            return Ok(Some(buffer));
        };
        budget.records += datagram.record_count;
        let header = datagram.header();
        let Some(session) = self.shared.config.session_for_cookie(header.cookie) else {
            self.counters.unknown_cookie_packets =
                self.counters.unknown_cookie_packets.wrapping_add(1);
            return Ok(Some(buffer));
        };
        buffer.session = session;
        if !buffer.rx_counted {
            let total = &mut self.sessions[session].totals.rx_outer_bytes;
            *total = total.wrapping_add((length + 48) as u64);
            buffer.rx_counted = true;
        }
        buffer.tag = header.receive_tag;
        if header.lane == 0 {
            if self.id != 0 {
                if let Err(Event::Packet(buffer)) = self.push(0, Event::Packet(buffer)) {
                    self.counters.queue_drops = self.counters.queue_drops.wrapping_add(1);
                    self.sessions[session].totals.queue_drops =
                        self.sessions[session].totals.queue_drops.wrapping_add(1);
                    return Ok(Some(buffer));
                }
                return Ok(None);
            }
            let peer = self.shared.config.peer_for_session(session).unwrap();
            if !self.controls[session].barrier.is_active() && self.shared.peer_bound(peer) {
                let record = datagram.records().next().unwrap();
                if self.peer_controls[peer].change.is_some()
                    && !active_flight(self.sessions[session].connection, record)
                {
                    return Ok(Some(buffer));
                }
                if let Ok(mut effects) = self.controls[session].state.on_record(record, now, None) {
                    if effects.needs_tag {
                        effects = self.controls[session]
                            .state
                            .on_record(record, now, Tag::generate()?)
                            .unwrap_or_default();
                    }
                    self.effects(session, effects, now)?;
                }
            }
            return Ok(Some(buffer));
        }
        if !self.matches(session, header.receive_tag) {
            return Ok(Some(buffer));
        }
        let lane_id = usize::from(header.lane);
        let owner = self.shared.owners[session][lane_id].load(Ordering::Acquire);
        if owner == 255 {
            return Ok(Some(buffer));
        }
        if owner == 0 {
            if self.id != 0 {
                if let Err(Event::Packet(buffer)) = self.push(0, Event::Packet(buffer)) {
                    self.counters.queue_drops = self.counters.queue_drops.wrapping_add(1);
                    self.sessions[session].totals.queue_drops =
                        self.sessions[session].totals.queue_drops.wrapping_add(1);
                    return Ok(Some(buffer));
                }
                return Ok(None);
            }
            let Some(mut lane) = self.sessions[session].free.pop() else {
                self.counters.queue_drops = self.counters.queue_drops.wrapping_add(1);
                self.sessions[session].totals.queue_drops =
                    self.sessions[session].totals.queue_drops.wrapping_add(1);
                return Ok(Some(buffer));
            };
            lane.reset(
                header.lane,
                self.sessions[session].connection.unwrap().max_frame_len,
            )
            .unwrap();
            let cookie_low = u32::from_be_bytes(header.cookie.as_bytes()[12..].try_into().unwrap());
            let owner = cookie_low.wrapping_add(u32::from(header.lane) - 1) as usize
                % self.shared.config.config().sessions[session].queue_count;
            // Publication may precede installation; an early packet is dropped without ACK.
            self.shared.owners[session][lane_id].store(owner as u8 + 1, Ordering::Release);
            if owner != self.id {
                if let Err(Event::Register { buffer, lane }) =
                    self.push(owner, Event::Register { buffer, lane })
                {
                    self.shared.owners[session][lane_id].store(0, Ordering::Release);
                    self.sessions[session].free.push(lane);
                    self.counters.queue_drops = self.counters.queue_drops.wrapping_add(1);
                    self.sessions[session].totals.queue_drops =
                        self.sessions[session].totals.queue_drops.wrapping_add(1);
                    return Ok(Some(buffer));
                }
                return Ok(None);
            }
            let slot = self.sessions[session]
                .rx
                .iter()
                .position(Option::is_none)
                .unwrap();
            self.sessions[session].rx_index[lane_id] = slot as u8 + 1;
            self.sessions[session].rx[slot] = Some(lane);
            self.install_rx_counters(session, slot);
        } else if usize::from(owner - 1) != self.id {
            if let Err(Event::Packet(buffer)) =
                self.push(usize::from(owner - 1), Event::Packet(buffer))
            {
                self.counters.queue_drops = self.counters.queue_drops.wrapping_add(1);
                self.sessions[session].totals.queue_drops =
                    self.sessions[session].totals.queue_drops.wrapping_add(1);
                return Ok(Some(buffer));
            }
            return Ok(None);
        }
        let slot = self.sessions[session].rx_index[lane_id];
        if slot == 0 {
            return Ok(Some(buffer));
        }
        let slot = usize::from(slot - 1);
        let local = &mut self.sessions[session];
        local.rx_totals[slot].rx_outer_bytes = local.rx_totals[slot]
            .rx_outer_bytes
            .wrapping_add((length + 48) as u64);
        local.rx_dirty |= 1_u64 << slot;
        let mut admission = local.rx[slot].as_mut().unwrap().admit(&datagram, now);
        if admission == Admission::Pressure {
            let retained = local.rx[slot].as_ref().unwrap().retained_frames();
            self.flush_rx(session, slot, now);
            if self.sessions[session].rx[slot]
                .as_ref()
                .unwrap()
                .retained_frames()
                < retained
            {
                admission = self.sessions[session].rx[slot]
                    .as_mut()
                    .unwrap()
                    .admit(&datagram, now);
            }
        }
        match admission {
            Admission::Fresh => {
                for record in datagram.records() {
                    match record {
                        Record::Probe => {
                            let event = Event::Reply {
                                session,
                                tag: header.receive_tag,
                                lane: header.lane,
                                packet: header.packet_number,
                                length: length as u16,
                            };
                            if self.id == 0 {
                                self.send_record(
                                    session,
                                    Record::ProbeReply {
                                        lane: header.lane,
                                        packet_number: header.packet_number,
                                        payload_len: length as u16,
                                    },
                                )?;
                            } else {
                                let _ = self.push(0, event);
                            }
                        }
                        Record::ProbeReply {
                            lane,
                            packet_number,
                            payload_len,
                        } if lane != 0 && usize::from(lane) <= self.sessions[session].tx.len() => {
                            let owner = (usize::from(lane) - 1) % self.shared.workers();
                            if self.id == owner {
                                self.feedback(session, lane, packet_number, payload_len, now);
                            } else {
                                // Carry the exact proof; arbitrary valid DATA cannot refresh liveness.
                                let _ = self.push(
                                    owner,
                                    Event::ProbeFeedback {
                                        session,
                                        tag: header.receive_tag,
                                        lane,
                                        packet: packet_number,
                                        length: payload_len,
                                    },
                                );
                            }
                        }
                        Record::LaneRetire { lane } => {
                            if self.id == 0 {
                                self.shared.owners[session][usize::from(lane)]
                                    .store(255, Ordering::Release);
                            } else {
                                let local = &mut self.sessions[session];
                                let bit = 1_u64 << (lane % 64);
                                let word = &mut local.retirements[usize::from(lane / 64)];
                                if *word & bit == 0 {
                                    *word |= bit;
                                    local.retire_pending += 1;
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            Admission::Duplicate => {
                self.counters.duplicate_packets = self.counters.duplicate_packets.wrapping_add(1);
                self.sessions[session].totals.duplicate_packets = self.sessions[session]
                    .totals
                    .duplicate_packets
                    .wrapping_add(1);
                self.sessions[session].rx_totals[slot].duplicate_packets = self.sessions[session]
                    .rx_totals[slot]
                    .duplicate_packets
                    .wrapping_add(1);
            }
            Admission::Pressure => {
                self.counters.admission_pressure = self.counters.admission_pressure.wrapping_add(1);
                self.sessions[session].totals.admission_pressure = self.sessions[session]
                    .totals
                    .admission_pressure
                    .wrapping_add(1);
                self.sessions[session].rx_totals[slot].admission_pressure = self.sessions[session]
                    .rx_totals[slot]
                    .admission_pressure
                    .wrapping_add(1);
            }
            Admission::Invalid => {
                self.counters.malformed_packets = self.counters.malformed_packets.wrapping_add(1);
                self.sessions[session].totals.malformed_packets = self.sessions[session]
                    .totals
                    .malformed_packets
                    .wrapping_add(1);
                self.sessions[session].rx_totals[slot].malformed_packets = self.sessions[session]
                    .rx_totals[slot]
                    .malformed_packets
                    .wrapping_add(1);
            }
        }
        if self.sessions[session].rx[slot]
            .as_ref()
            .unwrap()
            .ready_count()
            >= 16
        {
            self.flush_rx(session, slot, now);
        }
        self.refresh_rx_counters(session, slot);
        Ok(Some(buffer))
    }

    fn tap_capacity(&self, session: usize) -> usize {
        let local = &self.sessions[session];
        if !local.tap_inputs.is_empty() {
            return 0;
        }
        let workers = local.tx_workers;
        // Leave one output buffer and byte credit so retained super-frames can progress.
        self.free
            .len()
            .saturating_sub(RECEIVE_BUFFERS - self.rx_retained + 1)
            .min(frame_quota(workers).saturating_sub(local.tx_retained + 1))
            .min(
                (64 * 1024 * 1024 / workers).saturating_sub(local.tx_bytes + MAX_FRAME_LEN) / 65535,
            )
    }

    fn read_tap(&mut self, session: usize, now: Duration) -> io::Result<()> {
        const _: () = assert!(BATCH_SIZE <= 64);
        let checksums = self.sessions[session]
            .tap
            .as_ref()
            .unwrap()
            .completed_checksums;
        if self.tap_reader.as_ref().is_some_and(|r| r.enabled) {
            let count = self.sessions[session]
                .tap_read_limit
                .min(self.tap_capacity(session));
            if count == 0 {
                return Ok(());
            }
            let mut buffers: [Option<Box<Buffer>>; BATCH_SIZE] =
                std::array::from_fn(|i| if i < count { self.free.pop() } else { None });
            let mut slices: [&mut [u8]; BATCH_SIZE] = std::array::from_fn(|_| &mut [][..]);
            for (slice, buffer) in slices.iter_mut().zip(&mut buffers).take(count) {
                *slice = &mut buffer.as_mut().unwrap().bytes;
            }
            let reader = self.tap_reader.as_mut().unwrap();
            let batch = reader.read(
                self.sessions[session].tap.as_mut().unwrap(),
                &mut slices[..count],
            );
            let frames = batch.results[..count].iter().filter(|r| r.is_ok()).count() as u64;
            let limit = &mut self.sessions[session].tap_read_limit;
            // A full read limited by pool availability says nothing about TAP occupancy.
            *limit = if batch.segments.iter().any(Option::is_some) {
                1
            } else if frames as usize == count {
                (*limit * 2).min(BATCH_SIZE)
            } else {
                (frames as usize * 2).clamp(1, BATCH_SIZE)
            };
            for totals in [&mut self.counters, &mut self.sessions[session].totals] {
                totals.tap_read_calls = totals.tap_read_calls.wrapping_add(batch.calls);
                totals.tap_read_requests = totals.tap_read_requests.wrapping_add(batch.requests);
                totals.tap_read_batches = totals.tap_read_batches.wrapping_add(1);
                totals.tap_ring_frames = totals.tap_ring_frames.wrapping_add(frames);
                totals.tap_read_fallbacks = totals
                    .tap_read_fallbacks
                    .wrapping_add(u64::from(!reader.enabled));
            }
            for ((buffer, result), segments) in buffers
                .into_iter()
                .zip(batch.results)
                .zip(batch.segments)
                .take(count)
            {
                let mut buffer = buffer.unwrap();
                buffer.segments = segments;
                self.read_tap_result(session, now, buffer, result)?;
            }
        } else {
            for _ in 0..BATCH_SIZE {
                if self.tap_capacity(session) == 0 {
                    break;
                }
                let Some(mut buffer) = self.free.pop() else {
                    break;
                };
                self.counters.tap_read_calls = self.counters.tap_read_calls.wrapping_add(1);
                self.counters.tap_read_requests = self.counters.tap_read_requests.wrapping_add(1);
                self.sessions[session].totals.tap_read_requests = self.sessions[session]
                    .totals
                    .tap_read_requests
                    .wrapping_add(1);
                self.sessions[session].totals.tap_read_calls =
                    self.sessions[session].totals.tap_read_calls.wrapping_add(1);
                let result = self.sessions[session]
                    .tap
                    .as_mut()
                    .unwrap()
                    .read_input(&mut buffer.bytes)
                    .map(|(length, segments)| {
                        buffer.segments = segments;
                        length
                    });
                if self.read_tap_result(session, now, buffer, result)? {
                    break;
                }
            }
        }
        let completed = self.sessions[session]
            .tap
            .as_ref()
            .unwrap()
            .completed_checksums
            .wrapping_sub(checksums);
        self.counters.tap_checksums = self.counters.tap_checksums.wrapping_add(completed);
        self.sessions[session].totals.tap_checksums = self.sessions[session]
            .totals
            .tap_checksums
            .wrapping_add(completed);
        Ok(())
    }

    fn read_tap_result(
        &mut self,
        session: usize,
        now: Duration,
        mut buffer: Box<Buffer>,
        result: io::Result<usize>,
    ) -> io::Result<bool> {
        match result {
            Ok(length) => {
                buffer.length = length;
                buffer.session = session;
                let Some(connection) = self.sessions[session].connection else {
                    self.release(buffer);
                    return Ok(false);
                };
                let local = &mut self.sessions[session];
                let workers = local.tx_workers;
                if local.tx_retained >= frame_quota(workers)
                    || local.tx_bytes + length > 64 * 1024 * 1024 / workers
                {
                    self.counters.queue_drops = self.counters.queue_drops.wrapping_add(1);
                    local.totals.queue_drops = local.totals.queue_drops.wrapping_add(1);
                    self.release(buffer);
                    return Ok(false);
                }
                local.tx_retained += 1;
                local.tx_bytes += length;
                buffer.tx_reserved = true;
                buffer.tag = connection.local_tag().get();
                buffer.lane = tap_lane(
                    &buffer.bytes[..length],
                    self.id,
                    self.shared.workers(),
                    self.shared.config.config().sessions[session].queue_count,
                    self.sessions[session].tx.len(),
                );
                buffer.deadline = now + reassembly::FRAME_LIFETIME;
                if buffer.segments.is_some() {
                    self.counters.tap_gso_reads = self.counters.tap_gso_reads.wrapping_add(1);
                    self.sessions[session].totals.tap_gso_reads =
                        self.sessions[session].totals.tap_gso_reads.wrapping_add(1);
                }
                self.dispatch_frame(buffer, now)?;
            }
            Err(error) => {
                self.release(buffer);
                if !transient(&error) {
                    self.counters.tap_errors = self.counters.tap_errors.wrapping_add(1);
                    self.sessions[session].totals.tap_errors =
                        self.sessions[session].totals.tap_errors.wrapping_add(1);
                    let local = &mut self.sessions[session];
                    if local.tap_registered {
                        self.reactor.remove(local.tap.as_ref().unwrap().as_fd())?;
                    }
                    local.tap_registered = false;
                    local.tap_retry = now + Duration::from_secs(1);
                }
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn dispatch_frame(&mut self, buffer: Box<Buffer>, now: Duration) -> io::Result<()> {
        let session = buffer.session;
        let owner = buffer.lane % self.shared.workers();
        if owner == self.id {
            self.input_frame(buffer, now)?;
        } else if let Err(Event::Frame(buffer)) = self.push(owner, Event::Frame(buffer)) {
            self.counters.queue_drops = self.counters.queue_drops.wrapping_add(1);
            self.sessions[session].totals.queue_drops =
                self.sessions[session].totals.queue_drops.wrapping_add(1);
            self.release(buffer);
        }
        Ok(())
    }

    fn input_frame(&mut self, buffer: Box<Buffer>, now: Duration) -> io::Result<()> {
        let session = buffer.session;
        if buffer.segments.is_none() && self.sessions[session].tap_inputs.is_empty() {
            return self.frame(buffer, now);
        }
        if !self.matches(session, buffer.tag) || self.expired(&buffer, now) {
            self.release(buffer);
        } else if self.sessions[session].tap_inputs.len() == BUFFERS {
            self.counters.queue_drops = self.counters.queue_drops.wrapping_add(1);
            self.sessions[session].totals.queue_drops =
                self.sessions[session].totals.queue_drops.wrapping_add(1);
            self.release(buffer);
        } else {
            self.sessions[session].tap_inputs.push_back(buffer);
        }
        Ok(())
    }

    fn tap_can_expand(&self, session: usize) -> bool {
        let local = &self.sessions[session];
        let workers = local.tx_workers;
        self.free.len() > RECEIVE_BUFFERS - self.rx_retained
            && local.tx_retained < frame_quota(workers)
            && local.tx_bytes + MAX_FRAME_LEN <= 64 * 1024 * 1024 / workers
    }

    fn drain_tap_inputs(&mut self, now: Duration) -> io::Result<bool> {
        let mut frames = 0;
        let mut bytes = 0;
        let count = self.sessions.len();
        if count == 0 {
            return Ok(false);
        }
        let start = self.tap_start;
        self.tap_start = (start + 1) % count;
        for index in 0..count {
            let session = (start + index) % count;
            while frames < BATCH_SIZE && bytes < 64 * 1024 {
                let Some(mut input) = self.sessions[session].tap_inputs.pop_front() else {
                    break;
                };
                if !self.matches(session, input.tag) || self.expired(&input, now) {
                    self.release(input);
                    frames += 1;
                    continue;
                }
                let Some(mut segments) = input.segments else {
                    frames += 1;
                    bytes += input.length;
                    self.frame(input, now)?;
                    continue;
                };
                if !self.tap_can_expand(session) {
                    self.sessions[session].tap_inputs.push_front(input);
                    break;
                }
                let mut output = self.free.pop().unwrap();
                output.length = segments.write(&input.bytes[..input.length], &mut output.bytes);
                output.session = session;
                output.tag = input.tag;
                output.lane = input.lane;
                output.deadline = input.deadline;
                output.tx_reserved = true;
                self.sessions[session].tx_retained += 1;
                self.sessions[session].tx_bytes += output.length;
                frames += 1;
                bytes += output.length;
                for totals in [&mut self.counters, &mut self.sessions[session].totals] {
                    totals.tap_gso_frames = totals.tap_gso_frames.wrapping_add(1);
                    totals.tap_checksums = totals.tap_checksums.wrapping_add(1);
                }
                if segments.done(input.length) {
                    self.release(input);
                } else {
                    input.segments = Some(segments);
                    self.sessions[session].tap_inputs.push_front(input);
                }
                self.frame(output, now)?;
            }
        }
        Ok(self.sessions.iter().enumerate().any(|(i, s)| {
            s.tap_inputs
                .front()
                .is_some_and(|b| b.segments.is_none() || self.tap_can_expand(i))
        }))
    }

    fn expired(&mut self, buffer: &Buffer, now: Duration) -> bool {
        if now < buffer.deadline {
            return false;
        }
        let count = buffer.segments.map_or(1, |s| s.remaining(buffer.length)) as u64;
        self.counters.expired_frames = self.counters.expired_frames.wrapping_add(count);
        let local = &mut self.sessions[buffer.session];
        local.totals.expired_frames = local.totals.expired_frames.wrapping_add(count);
        local.tx_totals[buffer.lane].expired_frames = local.tx_totals[buffer.lane]
            .expired_frames
            .wrapping_add(count);
        local.tx_dirty |= 1_u64 << buffer.lane;
        true
    }

    fn frame(&mut self, mut buffer: Box<Buffer>, now: Duration) -> io::Result<()> {
        if !self.matches(buffer.session, buffer.tag) || self.expired(&buffer, now) {
            self.release(buffer);
            return Ok(());
        }
        let Some(lane) = &mut self.sessions[buffer.session].tx[buffer.lane] else {
            self.release(buffer);
            return Ok(());
        };
        let Ok(id) = lane.start_frame(buffer.length) else {
            self.release(buffer);
            return Ok(());
        };
        buffer.frame_id = id;
        buffer.offset = 0;
        self.pending.push_back(buffer);
        Ok(())
    }

    fn clear_batch(&mut self, batch: &mut TxBatch) {
        for frame in &mut batch.frames[..batch.len] {
            if let Some(buffer) = frame.take() {
                self.release(buffer);
            }
        }
        batch.compact();
    }

    fn flush_frames(&mut self, now: Duration) -> io::Result<()> {
        for _ in 0..self.pending.len().min(BATCH_SIZE) {
            let buffer = self.pending.pop_front().unwrap();
            if !self.matches(buffer.session, buffer.tag) || self.expired(&buffer, now) {
                if let Some(batch) = &mut self.sessions[buffer.session].batches[buffer.lane]
                    && batch.deferred == buffer.frame_id
                {
                    batch.deferred = 0;
                }
                self.release(buffer);
                continue;
            }
            let session = buffer.session;
            let lane = buffer.lane;
            let local = &mut self.sessions[session];
            let batch = local.batches[lane].as_mut().unwrap();
            // Queue rotation must not let a newer frame overtake a deferred one.
            if batch.deferred != 0 && batch.deferred != buffer.frame_id {
                self.pending.push_back(buffer);
                continue;
            }
            if !batch.plan.is_empty() || batch.len == pack::BATCH_CAPACITY {
                batch.deferred = buffer.frame_id;
                self.pending.push_back(buffer);
                continue;
            }
            batch.deferred = 0;
            batch.frames[batch.len] = Some(buffer);
            batch.len += 1;
            if local.tx_ready & (1_u64 << lane) == 0 {
                local.tx_ready |= 1_u64 << lane;
                self.ready.push_back((session, lane));
            }
        }
        for _ in 0..self.ready.len().min(BATCH_SIZE) {
            let (session, lane) = self.ready.pop_front().unwrap();
            let mut batch = self.sessions[session].batches[lane].take().unwrap();
            let result = self.flush_batch(session, lane, &mut batch, now);
            if batch.len != 0 {
                self.ready.push_back((session, lane));
            } else {
                self.sessions[session].tx_ready &= !(1_u64 << lane);
            }
            self.sessions[session].batches[lane] = Some(batch);
            result?;
        }
        Ok(())
    }

    fn flush_batch(
        &mut self,
        session: usize,
        lane: usize,
        batch: &mut TxBatch,
        now: Duration,
    ) -> io::Result<()> {
        let tag = batch.frames[..batch.len]
            .iter()
            .flatten()
            .next()
            .unwrap()
            .tag;
        if !self.matches(session, tag) {
            self.clear_batch(batch);
            return Ok(());
        }
        let mut expired = false;
        for frame in &mut batch.frames[..batch.len] {
            if frame.as_ref().is_some_and(|frame| self.expired(frame, now)) {
                self.release(frame.take().unwrap());
                expired = true;
            }
        }
        if expired {
            batch.compact();
        }
        if batch.len == 0 {
            return Ok(());
        }
        let peer = self.shared.config.peer_for_session(session).unwrap();
        let path = self.sessions[session].path;
        let tx = &self.transmit[peer].paths[path].lanes[lane];
        let limit = tx.mtu.payload_limit();
        if tx.socket.is_none() {
            self.clear_batch(batch);
            return Ok(());
        }
        if tx.blocked || limit == 0 {
            return Ok(());
        }
        if batch.plan.is_empty()
            && batch
                .prepare(self.sessions[session].tx[lane].as_mut().unwrap(), limit)
                .is_err()
        {
            self.clear_batch(batch);
            return Ok(());
        }
        let result = {
            let frames: [&[u8]; pack::BATCH_CAPACITY] = std::array::from_fn(|index| {
                batch.frames[index]
                    .as_ref()
                    .map_or(&[][..], |frame| &frame.bytes[..frame.length])
            });
            let mut slices = [&[][..]; pack::BATCH_SLICES];
            let mut starts = [0; BATCH_SIZE + 1];
            let count = batch.plan.len();
            for index in 0..count {
                let length = batch
                    .plan
                    .slices(index, &frames, &mut slices[starts[index]..])
                    .unwrap()
                    .len();
                starts[index + 1] = starts[index] + length;
            }
            let packets: [SendDatagram<'_>; BATCH_SIZE] =
                std::array::from_fn(|index| SendDatagram {
                    parts: if index < count {
                        &slices[starts[index]..starts[index + 1]]
                    } else {
                        &[]
                    },
                });
            self.transmit[peer].paths[path].lanes[lane]
                .socket
                .as_ref()
                .unwrap()
                .send_batch(&packets[..count], limit)
        };
        match result {
            Ok(sent) => self.submitted_batch(session, lane, batch, sent),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                let tx = &mut self.transmit[peer].paths[path].lanes[lane];
                let token = TX_TOKEN | ((peer as u64) << 7) | ((path as u64) << 6) | lane as u64;
                self.reactor.modify(
                    tx.socket.as_ref().unwrap().as_fd(),
                    token,
                    libc::EPOLLOUT as u32,
                )?;
                tx.blocked = true;
                batch.compact();
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => batch.compact(),
            Err(error) if error.raw_os_error() == Some(libc::EMSGSIZE) => {
                let attempted = (0..batch.plan.len())
                    .map(|index| batch.plan.packet(index).unwrap().length)
                    .max()
                    .unwrap();
                self.mtu_send_error(peer, path, lane, attempted, now)?;
                batch.compact();
            }
            Err(_) => {
                self.counters.socket_errors = self.counters.socket_errors.wrapping_add(1);
                let local = &mut self.sessions[session];
                local.totals.socket_errors = local.totals.socket_errors.wrapping_add(1);
                local.tx_totals[lane].socket_errors =
                    local.tx_totals[lane].socket_errors.wrapping_add(1);
                local.tx_dirty |= 1_u64 << lane;
                self.clear_batch(batch);
            }
        }
        Ok(())
    }

    fn submitted_batch(&mut self, session: usize, lane: usize, batch: &mut TxBatch, sent: usize) {
        let mut bytes = 0;
        for packet in 0..sent {
            bytes += (batch.plan.packet(packet).unwrap().length + 48) as u64;
            for (frame, range) in batch.plan.pieces(packet).unwrap() {
                batch.frames[frame].as_mut().unwrap().offset = range.end;
            }
        }
        self.counters.tx_outer_bytes = self.counters.tx_outer_bytes.wrapping_add(bytes);
        let local = &mut self.sessions[session];
        local.totals.tx_outer_bytes = local.totals.tx_outer_bytes.wrapping_add(bytes);
        local.tx_totals[lane].tx_outer_bytes =
            local.tx_totals[lane].tx_outer_bytes.wrapping_add(bytes);
        local.tx_dirty |= 1_u64 << lane;
        for frame in &mut batch.frames[..batch.len] {
            if frame
                .as_ref()
                .is_some_and(|frame| frame.offset == frame.length)
            {
                self.release(frame.take().unwrap());
            }
        }
        // Every retry gets new packet numbers; retain only unsent frame byte ranges.
        batch.compact();
    }

    fn return_lane(&mut self, session: usize, mut lane: Box<reassembly::Lane>) {
        lane.retire();
        if self.id == 0 {
            self.sessions[session].free.push(lane);
        } else if let Err(Event::LaneFree { session, lane }) =
            self.push(0, Event::LaneFree { session, lane })
        {
            let local = &mut self.sessions[session];
            let slot = local.rx.iter().position(Option::is_none).unwrap();
            local.rx[slot] = Some(lane);
            local.returning |= 1_u64 << slot;
        }
    }

    fn flush_taps(&mut self, now: Duration) -> io::Result<()> {
        for session in 0..self.sessions.len() {
            let readable = self.tap_capacity(session) != 0;
            let mut writable = false;
            for slot in 0..LANES {
                let local = &mut self.sessions[session];
                let Some(lane) = &mut local.rx[slot] else {
                    continue;
                };
                if local.connection.is_none()
                    || local.returning & (1_u64 << slot) != 0
                    || self.shared.owners[session][usize::from(lane.id())].load(Ordering::Acquire)
                        == 255
                {
                    let lane = local.rx[slot].take().unwrap();
                    local.rx_index[usize::from(lane.id())] = 0;
                    local.returning &= !(1_u64 << slot);
                    local.rx_totals[slot] = Snapshot::default();
                    local.rx_dirty &= !(1_u64 << slot);
                    let index = (session * self.shared.workers() + self.id) * LANES + slot;
                    self.shared.receive_counters[index].replace(0, 0, &Snapshot::default());
                    self.return_lane(session, lane);
                    continue;
                }
                lane.expire(now);
                if lane.ready_count() != 0 {
                    writable |= self.flush_rx(session, slot, now);
                }
                self.refresh_rx_counters(session, slot);
            }
            let local = &mut self.sessions[session];
            local.totals.reassembly_frames =
                local.rx_totals.iter().map(|s| s.reassembly_frames).sum();
            local.totals.reassembly_bytes =
                local.rx_totals.iter().map(|s| s.reassembly_bytes).sum();
            if !writable {
                local.tap_blocked = false;
            }
            if local.tap_registered
                && (local.tap_writable != writable || local.tap_readable != readable)
            {
                self.reactor.modify(
                    local.tap.as_ref().unwrap().as_fd(),
                    TAP_TOKEN | session as u64,
                    (if readable { libc::EPOLLIN } else { 0 }
                        | if writable { libc::EPOLLOUT } else { 0 }) as u32,
                )?;
                local.tap_writable = writable;
                local.tap_readable = readable;
            }
        }
        Ok(())
    }

    fn flush_rx(&mut self, session: usize, slot: usize, now: Duration) -> bool {
        let local = &mut self.sessions[session];
        if local.tap_blocked {
            return true;
        }
        let lane = local.rx[slot].as_mut().unwrap();
        let mut writable = false;
        if let Some(tap) = &mut local.tap {
            let mut budget = BATCH_SIZE;
            while budget != 0 {
                let uso = tap.uso_enabled();
                let (mask, result) = tap.write_batch(lane.ready_frames(now).take(budget));
                if mask == 0 {
                    break;
                }
                let attempted = mask.count_ones() as usize;
                debug_assert!(attempted <= budget);
                let fallback = u64::from(uso && !tap.uso_enabled());
                for totals in [
                    &mut self.counters,
                    &mut local.totals,
                    &mut local.rx_totals[slot],
                ] {
                    totals.tap_write_calls = totals.tap_write_calls.wrapping_add(1 + fallback);
                    totals.tap_uso_fallbacks = totals.tap_uso_fallbacks.wrapping_add(fallback);
                    if attempted > 1 && result.is_ok() {
                        totals.tap_uso_batches = totals.tap_uso_batches.wrapping_add(1);
                        totals.tap_uso_frames =
                            totals.tap_uso_frames.wrapping_add(attempted as u64);
                    }
                }
                local.rx_dirty |= 1_u64 << slot;
                match result {
                    Ok(length) => {
                        self.counters.delivered_frame_bytes = self
                            .counters
                            .delivered_frame_bytes
                            .wrapping_add(length as u64);
                        local.totals.delivered_frame_bytes = local
                            .totals
                            .delivered_frame_bytes
                            .wrapping_add(length as u64);
                        local.rx_totals[slot].delivered_frame_bytes = local.rx_totals[slot]
                            .delivered_frame_bytes
                            .wrapping_add(length as u64);
                        local.rx_dirty |= 1_u64 << slot;
                    }
                    Err(error) if transient(&error) => {
                        local.tap_blocked = true;
                        writable = true;
                        break;
                    }
                    Err(_) => {
                        self.counters.tap_errors =
                            self.counters.tap_errors.wrapping_add(attempted as u64);
                        local.totals.tap_errors =
                            local.totals.tap_errors.wrapping_add(attempted as u64);
                        local.rx_totals[slot].tap_errors = local.rx_totals[slot]
                            .tap_errors
                            .wrapping_add(attempted as u64);
                        local.rx_dirty |= 1_u64 << slot;
                    }
                }
                lane.finish_ready(mask);
                budget -= attempted;
            }
        }
        writable
    }

    // Nominally one 9000-byte probe per 3 ms globally, without a shared counter or lock.
    // shortcut: static worker slots waste idle shares; replace with control grants in pacing.rs.
    fn probe_time(&self, now: Duration) -> Duration {
        let slot = now.as_millis() / 3;
        let workers = self.shared.workers() as u128;
        let owner = slot % workers;
        let mut distance = (self.id as u128 + workers - owner) % workers;
        if distance == 0 && self.probe_slot != Some(slot) {
            return now;
        }
        if distance == 0 {
            distance = workers;
        }
        Duration::from_millis(((slot + distance) * 3).min(u64::MAX as u128) as u64)
    }

    fn mtu_timers(&mut self, now: Duration) -> io::Result<bool> {
        if now < self.mtu_due || self.mtu_jobs.is_empty() {
            return Ok(false);
        }
        if self.mtu_cursor == 0 {
            self.mtu_next = Duration::MAX;
        }
        let end = (self.mtu_cursor + BATCH_SIZE).min(self.mtu_jobs.len());
        while self.mtu_cursor < end {
            let (peer, path, lane) =
                self.mtu_jobs[(self.mtu_start + self.mtu_cursor) % self.mtu_jobs.len()];
            self.mtu_cursor += 1;
            let Some(session) = self.transmit[peer].paths[path].probe_session else {
                continue;
            };
            let tx = &mut self.transmit[peer].paths[path].lanes[lane];
            if tx.socket.is_none() {
                continue;
            }
            let before = (tx.mtu.payload_limit(), tx.mtu.state());
            let length = tx.mtu.poll(now);
            if let Some(length) = length
                && now >= self.probe_time(now)
            {
                self.probe_slot = Some(now.as_millis() / 3);
                self.send_mtu_probe(session, peer, path, lane, length, now)?;
            }
            self.mtu_changed(peer, path, lane, before, now);
            let due = self.transmit[peer].paths[path].lanes[lane].mtu.deadline();
            self.mtu_next = self.mtu_next.min(due.max(self.probe_time(now)));
        }
        if self.mtu_cursor == self.mtu_jobs.len() {
            self.mtu_cursor = 0;
            self.mtu_start = (self.mtu_start + 1) % self.mtu_jobs.len();
            self.mtu_due = self.mtu_next;
            Ok(false)
        } else {
            Ok(true)
        }
    }

    fn send_mtu_probe(
        &mut self,
        session: usize,
        peer: usize,
        path: usize,
        lane: usize,
        length: usize,
        now: Duration,
    ) -> io::Result<()> {
        let mut prefix = [0; wire::PROBE_PREFIX_LEN];
        let Ok(piece) = self.sessions[session].tx[lane]
            .as_mut()
            .unwrap()
            .probe(length, &mut prefix)
        else {
            self.transmit[peer].paths[path].lanes[lane].mtu.defer(now);
            return Ok(());
        };
        let parts = [&prefix[..], piece.bytes];
        let tx = &mut self.transmit[peer].paths[path].lanes[lane];
        match tx
            .socket
            .as_ref()
            .unwrap()
            .send_batch(&[SendDatagram { parts: &parts }], length)
        {
            Ok(1) => {
                let local = &mut self.sessions[session];
                assert!(tx.mtu.submitted(
                    pmtu::ProbeId {
                        session,
                        tag: local.connection.unwrap().local_tag().get(),
                        lane: lane as u16 + 1,
                        packet: piece.packet.number,
                    },
                    length,
                    now
                ));
                for totals in [
                    &mut self.counters,
                    &mut local.totals,
                    &mut local.tx_totals[lane],
                ] {
                    totals.tx_outer_bytes =
                        totals.tx_outer_bytes.wrapping_add((length + 48) as u64);
                }
                local.tx_dirty |= 1_u64 << lane;
            }
            Err(error) if error.raw_os_error() == Some(libc::EMSGSIZE) => {
                self.mtu_send_error(peer, path, lane, length, now)?;
            }
            result => {
                tx.mtu.defer(now);
                if result.is_err_and(|error| !transient(&error)) {
                    self.counters.socket_errors = self.counters.socket_errors.wrapping_add(1);
                }
            }
        }
        Ok(())
    }

    fn mtu_send_error(
        &mut self,
        peer: usize,
        path: usize,
        lane: usize,
        attempted: usize,
        now: Duration,
    ) -> io::Result<()> {
        self.transmit[peer].paths[path].lanes[lane].failed_payload = attempted;
        let remote = self.mtu_error(peer, path, lane, now)?;
        let tx = &mut self.transmit[peer].paths[path].lanes[lane];
        if !remote && tx.failed_payload != 0 {
            // An error without a usable hint still cannot justify this DATA size.
            let before = (tx.mtu.payload_limit(), tx.mtu.state());
            tx.mtu
                .local_too_big(attempted, (attempted + 47) as u32, now);
            self.mtu_changed(peer, path, lane, before, now);
        }
        Ok(())
    }

    // Return whether a remote error could have caused EMSGSIZE; it is not a local size proof.
    fn mtu_error(
        &mut self,
        peer: usize,
        path: usize,
        lane: usize,
        now: Duration,
    ) -> io::Result<bool> {
        let tx = &self.transmit[peer].paths[path].lanes[lane];
        let error = match tx.socket.as_ref().unwrap().recv_mtu_error() {
            Ok(Some(error)) => error,
            Ok(None) => return Ok(false),
            Err(error) if transient(&error) => return Ok(false),
            Err(error) => return Err(error),
        };
        let address = self.transmit[peer].paths[path].address.unwrap();
        if error.destination.ip() != address.ip() || error.destination.port() != address.port() {
            return Ok(!error.local);
        }
        let before = (tx.mtu.payload_limit(), tx.mtu.state());
        let proof = if error.local {
            None
        } else {
            tx.mtu.probes(now).find_map(|(id, length)| {
                let local = &self.sessions[id.session];
                let connection = local.connection?;
                if local.path != path || connection.local_tag().get() != id.tag {
                    return None;
                }
                let mut header = [0; wire::HEADER_LEN];
                Header {
                    cookie: self.shared.config.config().sessions[id.session].cookie,
                    receive_tag: connection.remote_tag().get(),
                    lane: id.lane,
                    packet_number: id.packet,
                    record_area_len: (length - wire::HEADER_LEN) as u16,
                }
                .encode(&mut header);
                (error.quoted_payload() == header).then_some(id)
            })
        };
        let tx = &mut self.transmit[peer].paths[path].lanes[lane];
        if error.local {
            let attempted = std::mem::take(&mut tx.failed_payload);
            tx.mtu.local_too_big(attempted, error.mtu, now);
        } else if let Some(id) = proof {
            tx.mtu.packet_too_big(id, error.mtu, now);
        }
        self.mtu_changed(peer, path, lane, before, now);
        Ok(!error.local)
    }

    fn mtu_changed(
        &mut self,
        peer: usize,
        path: usize,
        lane: usize,
        before: (usize, crate::counters::PathMtuState),
        now: Duration,
    ) {
        let mtu = &self.transmit[peer].paths[path].lanes[lane].mtu;
        let limit = mtu.payload_limit();
        let state = mtu.state();
        let due = mtu.deadline().max(self.probe_time(now));
        self.mtu_due = self.mtu_due.min(due);
        self.mtu_next = self.mtu_next.min(due);
        if before == (limit, state) {
            return;
        }
        for session in 0..self.sessions.len() {
            let local = &mut self.sessions[session];
            if self.shared.config.peer_for_session(session) != Some(peer)
                || local.path != path
                || local.connection.is_none()
            {
                continue;
            }
            if limit < before.0
                && let Some(batch) = &mut local.batches[lane]
            {
                batch.compact();
            }
            local.tx_totals[lane].path_mtu_bytes = if limit == 0 { 0 } else { (limit + 48) as u64 };
            local.tx_totals[lane].path_mtu_state = state as u64;
            local.tx_dirty |= 1_u64 << lane;
            Self::session_mtu(local);
        }
    }

    fn session_mtu(local: &mut Session) {
        let mut total = Snapshot::default();
        for (lane, tx) in local.tx.iter().enumerate() {
            if tx.is_some() {
                total.merge(Snapshot {
                    path_mtu_bytes: local.tx_totals[lane].path_mtu_bytes,
                    path_mtu_state: local.tx_totals[lane].path_mtu_state,
                    ..Snapshot::default()
                });
            }
        }
        local.totals.path_mtu_bytes = total.path_mtu_bytes;
        local.totals.path_mtu_state = total.path_mtu_state;
    }

    fn feedback(&mut self, session: usize, lane: u16, packet: u64, length: u16, now: Duration) {
        let Some(connection) = self.sessions[session].connection else {
            return;
        };
        let peer = self.shared.config.peer_for_session(session).unwrap();
        let path = self.sessions[session].path;
        let index = usize::from(lane) - 1;
        let mtu = &mut self.transmit[peer].paths[path].lanes[index].mtu;
        let before = (mtu.payload_limit(), mtu.state());
        if mtu.reply(
            pmtu::ProbeId {
                session,
                tag: connection.local_tag().get(),
                lane,
                packet,
            },
            usize::from(length),
            now,
        ) {
            self.mtu_changed(peer, path, index, before, now);
        }
        if self.id == 0 && lane == 1 && self.controls[session].probe == Some((packet, length)) {
            self.controls[session].probe = None;
            if let Some(connection) = self.sessions[session].connection {
                let _ = self.controls[session]
                    .state
                    .on_feedback(connection.local_tag(), now);
            }
        }
    }

    fn send_record(
        &mut self,
        session: usize,
        record: Record<'static>,
    ) -> io::Result<Option<(u64, u16)>> {
        let peer = self.shared.config.peer_for_session(session).unwrap();
        let path =
            if record.is_handshake() && !active_flight(self.sessions[session].connection, record) {
                self.controls[session].handshake_path
            } else {
                self.sessions[session].path
            };
        let Some(socket) = &self.transmit[peer].paths[path].lanes[0].socket else {
            return Ok(None);
        };
        let limit = BASE_UDP_PAYLOAD;
        let mut storage = [0; BASE_UDP_PAYLOAD];
        let bytes = &mut storage[..];
        let packet = if record.is_handshake() {
            let header = Header {
                cookie: self.shared.config.config().sessions[session].cookie,
                receive_tag: 0,
                lane: 0,
                record_area_len: 0,
                packet_number: 0,
            };
            wire::encode_datagram(header, &[record], bytes)
                .ok()
                .map(|length| pack::Packet { number: 0, length })
        } else {
            self.sessions[session].tx[0]
                .as_mut()
                .and_then(|lane| lane.record(record, bytes).ok())
        };
        let Some(packet) = packet else {
            return Ok(None);
        };
        match socket.send(&bytes[..packet.length], limit) {
            Ok(()) => {
                self.counters.tx_outer_bytes = self
                    .counters
                    .tx_outer_bytes
                    .wrapping_add((packet.length + 48) as u64);
                let local = &mut self.sessions[session];
                local.totals.tx_outer_bytes = local
                    .totals
                    .tx_outer_bytes
                    .wrapping_add((packet.length + 48) as u64);
                if !record.is_handshake() {
                    local.tx_totals[0].tx_outer_bytes = local.tx_totals[0]
                        .tx_outer_bytes
                        .wrapping_add((packet.length + 48) as u64);
                    local.tx_dirty |= 1;
                }
                Ok(Some((packet.number, packet.length as u16)))
            }
            Err(error) => {
                if !transient(&error) {
                    self.counters.socket_errors = self.counters.socket_errors.wrapping_add(1);
                    let local = &mut self.sessions[session];
                    local.totals.socket_errors = local.totals.socket_errors.wrapping_add(1);
                    if !record.is_handshake() {
                        local.tx_totals[0].socket_errors =
                            local.tx_totals[0].socket_errors.wrapping_add(1);
                        local.tx_dirty |= 1;
                    }
                }
                Ok(None)
            }
        }
    }

    fn effects(&mut self, session: usize, effects: Effects, now: Duration) -> io::Result<()> {
        if effects.commit.is_some() || effects.retire {
            self.controls[session]
                .barrier
                .begin(effects.commit)
                .map_err(|_| io::ErrorKind::Other)?;
            self.controls[session].effects = effects;
            self.controls[session].install_path = self.controls[session].handshake_path;
        } else {
            self.apply_effects(session, effects, now)?;
        }
        Ok(())
    }

    fn apply_effects(&mut self, session: usize, effects: Effects, now: Duration) -> io::Result<()> {
        if let Some(carrier) = effects.carrier {
            self.sessions[session]
                .tap
                .as_ref()
                .unwrap()
                .set_carrier(carrier)?;
            self.controls[session].carrier = carrier;
        }
        if let Some(record) = effects.send {
            self.send_record(session, record)?;
        }
        self.controls[session].retry = now + Duration::from_millis(100);
        self.shared.publish_state(
            session,
            self.controls[session].state.state(),
            self.controls[session].carrier,
        );
        Ok(())
    }

    fn command(&mut self, session: usize, command: Command, path: usize) {
        let local = &mut self.sessions[session];
        if local.installed.is_some_and(|old| {
            old.generation > command.generation
                || (old.generation == command.generation
                    && old.phase == Phase::Install
                    && command.phase == Phase::Quiesce)
        }) {
            return;
        }
        if local.installed == Some(command) {
            local.acknowledged = Some(command);
            return;
        }
        match command.phase {
            Phase::Quiesce => {
                local.connection = None;
                local.retirements.fill(0);
                local.retire_pending = 0;
                local.command = Some(command);
            }
            Phase::Install => {
                local.rx_index.fill(0);
                for batch in local.batches.iter_mut().flatten() {
                    batch.deferred = 0;
                }
                local.connection = command.connection;
                local.path = path;
                let config = self.shared.config.config();
                let cookie = config.sessions[session].cookie;
                let peer = self.shared.config.peer_for_session(session).unwrap();
                for (index, lane) in local.tx.iter_mut().enumerate() {
                    *lane = command
                        .connection
                        .filter(|_| index % self.shared.workers() == self.id)
                        .map(|c| pack::Lane::new(cookie, c, index as u16 + 1).unwrap());
                    if index % self.shared.workers() == self.id {
                        let mtu = &self.transmit[peer].paths[path].lanes[index].mtu;
                        let limit = mtu.payload_limit();
                        local.tx_totals[index] = Snapshot {
                            path_mtu_bytes: if lane.is_none() || limit == 0 {
                                0
                            } else {
                                (limit + 48) as u64
                            },
                            path_mtu_state: if lane.is_none() {
                                0
                            } else {
                                mtu.state() as u64
                            },
                            ..Snapshot::default()
                        };
                        self.shared.transmit_counters[session][index].replace(
                            if lane.is_some() { index as u16 + 1 } else { 0 },
                            command.generation,
                            &local.tx_totals[index],
                        );
                    }
                }
                local.tx_dirty = 0;
                Self::session_mtu(local);
                local.installed = Some(command);
                local.acknowledged = Some(command);
            }
        }
        let peer = self.shared.config.peer_for_session(session).unwrap();
        for path in 0..2 {
            self.transmit[peer].paths[path].probe_session = self
                .sessions
                .iter()
                .enumerate()
                .find(|(index, local)| {
                    self.shared.config.peer_for_session(*index) == Some(peer)
                        && local.path == path
                        && local.connection.is_some()
                })
                .map(|(index, _)| index);
        }
        self.mtu_due = Duration::ZERO;
        self.mtu_next = Duration::ZERO;
    }

    fn acknowledge(
        &mut self,
        session: usize,
        worker: usize,
        command: Command,
        now: Duration,
    ) -> io::Result<()> {
        match self.controls[session].barrier.acknowledge(worker, command) {
            Progress::Quiesced => {
                for owner in &self.shared.owners[session] {
                    owner.store(0, Ordering::Release);
                }
                self.controls[session].probe = None;
            }
            Progress::Complete => {
                self.shared.generations[session].store(command.generation, Ordering::Release);
                let effects = std::mem::take(&mut self.controls[session].effects);
                self.apply_effects(session, effects, now)?;
            }
            _ => {}
        }
        Ok(())
    }

    fn progress_barriers(&mut self, now: Duration) -> io::Result<()> {
        for session in 0..self.sessions.len() {
            if self.sessions[session].retire_pending == 0 {
                continue;
            }
            let tag = self.sessions[session].connection.unwrap().local_tag().get();
            for word in 0..1024 {
                while self.sessions[session].retirements[word] != 0 {
                    let bit = self.sessions[session].retirements[word].trailing_zeros();
                    let lane = (word * 64 + bit as usize) as u16;
                    if self.push(0, Event::Retire { session, tag, lane }).is_err() {
                        break;
                    }
                    self.sessions[session].retirements[word] &= !(1_u64 << bit);
                    self.sessions[session].retire_pending -= 1;
                }
            }
        }
        for session in 0..self.controls.len() {
            if !self.controls[session].barrier.is_active() {
                continue;
            }
            for worker in 0..self.shared.workers() {
                if let Some(command) = self.controls[session].barrier.pending(worker) {
                    if worker == self.id {
                        self.controls[session].barrier.sent(worker, command);
                        self.command(session, command, self.controls[session].install_path);
                    } else if self
                        .push(
                            worker,
                            Event::Barrier {
                                session,
                                command,
                                path: self.controls[session].install_path,
                            },
                        )
                        .is_ok()
                    {
                        self.controls[session].barrier.sent(worker, command);
                    }
                }
            }
        }
        for session in 0..self.sessions.len() {
            let local = &mut self.sessions[session];
            if let Some(command) = local.command
                && local.rx.iter().all(Option::is_none)
                && local.tx_ready == 0
                && local.tap_inputs.is_empty()
                && !self.pending.iter().any(|buffer| buffer.session == session)
            {
                local.command = None;
                local.installed = Some(command);
                local.acknowledged = Some(command);
            }
            if let Some(command) = local.acknowledged {
                if self.id == 0 {
                    local.acknowledged = None;
                    self.acknowledge(session, 0, command, now)?;
                } else if self
                    .push(
                        0,
                        Event::Acknowledge {
                            session,
                            worker: self.id,
                            command,
                        },
                    )
                    .is_ok()
                {
                    self.sessions[session].acknowledged = None;
                }
            }
        }
        Ok(())
    }

    fn timers(&mut self, now: Duration) -> io::Result<()> {
        for session in 0..self.controls.len() {
            let peer = self.shared.config.peer_for_session(session).unwrap();
            if !self.shared.peer_bound(peer)
                || self.transmit[peer].paths[self.controls[session].handshake_path].lanes[0]
                    .socket
                    .is_none()
                || self.controls[session].barrier.is_active()
            {
                continue;
            }
            let due = self.controls[session]
                .state
                .deadline()
                .unwrap_or(Duration::MAX)
                .max(self.controls[session].retry);
            if now >= due {
                let mut effects = self.controls[session].state.on_timer(now, None).unwrap();
                if effects.needs_tag && self.peer_controls[peer].change.is_none() {
                    let next = self.controls[session]
                        .state
                        .on_timer(now, Tag::generate()?)
                        .unwrap();
                    effects.send = next.send;
                    effects.commit = next.commit;
                    effects.needs_tag = next.needs_tag;
                }
                self.effects(session, effects, now)?;
            }
            if self.sessions[session].connection.is_some()
                && now >= self.controls[session].probe_due
            {
                self.controls[session].probe_due = now + Duration::from_secs(1);
                self.controls[session].probe = self.send_record(session, Record::Probe)?;
            }
            self.shared.publish_state(
                session,
                self.controls[session].state.state(),
                self.controls[session].carrier,
            );
        }
        Ok(())
    }

    fn deadline(&self, now: Duration) -> Option<Duration> {
        let mut due = self.setup_due.min(self.mtu_due);
        for (index, control) in self.controls.iter().enumerate() {
            let peer = self.shared.config.peer_for_session(index).unwrap();
            if self.shared.peer_bound(peer)
                && self.transmit[peer].paths[control.handshake_path].lanes[0]
                    .socket
                    .is_some()
                && !control.barrier.is_active()
            {
                if let Some(deadline) = control.state.deadline() {
                    due = due.min(deadline.max(control.retry));
                }
                if control.state.connection().is_some() {
                    due = due.min(control.probe_due);
                }
            }
        }
        for session in &self.sessions {
            if let Some(input) = session.tap_inputs.front() {
                due = due.min(input.deadline);
            }
            for lane in session.rx.iter().flatten() {
                if let Some(deadline) = lane.deadline() {
                    due = due.min(deadline);
                }
            }
        }
        for frame in &self.pending {
            due = due.min(frame.deadline);
        }
        for &(session, lane) in &self.ready {
            let batch = self.sessions[session].batches[lane].as_ref().unwrap();
            for frame in batch.frames[..batch.len].iter().flatten() {
                due = due.min(frame.deadline);
            }
        }
        Some(due.max(now))
    }
}

fn active_flight(connection: Option<Connection>, record: Record<'_>) -> bool {
    let Some(connection) = connection else {
        return false;
    };
    match record {
        Record::Hello { initiator, .. } => initiator == connection.initiator.get(),
        Record::Challenge {
            initiator,
            responder,
            ..
        }
        | Record::Confirm {
            initiator,
            responder,
        }
        | Record::Ready {
            initiator,
            responder,
        } => initiator == connection.initiator.get() && responder == connection.responder.get(),
        _ => false,
    }
}

fn transient(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // Datagram sockets give deterministic full-write/WouldBlock behavior without
    // depending on the kernel's TAP ingress drop policy.
    fn receive_worker() -> (Worker, std::os::unix::net::UnixDatagram) {
        let (shared, [mut worker, _]) = test_workers();
        let params = wire::Params {
            port: 40000,
            max_frame_len: 9022,
            mandatory_features: 0,
            optional_features: 0,
        };
        let connection = Connection {
            role: handshake::Role::Initiator,
            initiator: Tag::new(1).unwrap(),
            responder: Tag::new(2).unwrap(),
            local: params,
            peer: wire::Params {
                port: 40001,
                ..params
            },
            max_frame_len: 9022,
        };
        worker.command(
            0,
            Command {
                generation: 1,
                phase: Phase::Install,
                connection: Some(connection),
            },
            0,
        );
        let mut lane = worker.sessions[0].free.pop().unwrap();
        lane.reset(7, 9022).unwrap();
        worker.sessions[0].rx[0] = Some(lane);
        worker.sessions[0].rx_index[7] = 1;
        shared.owners[0][7].store(1, Ordering::Release);
        worker.install_rx_counters(0, 0);
        let (tx, rx) = std::os::unix::net::UnixDatagram::pair().unwrap();
        tx.set_nonblocking(true).unwrap();
        rx.set_nonblocking(true).unwrap();
        worker
            .attach_tap(0, TapQueue::test_fd(tx.into(), false))
            .unwrap();
        (worker, rx)
    }

    fn receive_bytes(number: u64, records: &[Record<'_>], output: &mut [u8]) -> usize {
        wire::encode_datagram(
            Header {
                cookie: crate::UnquicUuid::from_bytes([1; 16]),
                receive_tag: 1,
                lane: 7,
                record_area_len: 0,
                packet_number: number,
            },
            records,
            output,
        )
        .unwrap()
    }

    fn receive_piece(sequence: u64, offset: u32, bytes: &[u8]) -> Record<'_> {
        Record::Data {
            frame_id: (7 << 48) | sequence,
            total_len: 60,
            offset,
            bytes,
        }
    }

    #[test]
    #[ignore = "kernel descriptor test; run in a disposable container"]
    fn receive_group_retries_binds_and_retains_sockets_when_handoff_is_full() {
        let (shared, mut workers) = test_workers();
        let (tx, rx) = crate::spsc::channel();
        workers[0].outgoing[1] = Some(tx);
        workers[1].incoming[0] = Some(rx);
        let occupied = std::net::UdpSocket::bind("[::1]:40000").unwrap();
        workers[0].setup(Duration::ZERO).unwrap();
        assert!(workers[0].receive[0].is_none());
        assert!(workers[0].pending_receive.is_empty());
        assert_eq!(shared.bound[0].load(Ordering::Acquire), 0);
        drop(occupied);
        for _ in 0..crate::spsc::CAPACITY {
            assert!(
                workers[0]
                    .push(
                        1,
                        Event::ProbeFeedback {
                            session: 0,
                            tag: 0,
                            lane: 1,
                            packet: 0,
                            length: 0,
                        }
                    )
                    .is_ok()
            );
        }
        workers[0].setup(Duration::from_secs(1)).unwrap();
        assert!(workers[0].receive[0].is_some());
        assert_eq!(workers[0].pending_receive.len(), 1);
        assert!(!shared.peer_bound(0));
        assert!(workers[0].distribute_receivers());
        assert_eq!(workers[0].pending_receive.len(), 1);
        for _ in 0..32 {
            workers[1].drain_queues(Duration::from_secs(1)).unwrap();
            workers[0].distribute_receivers();
        }
        assert!(workers[0].pending_receive.is_empty());
        assert!(workers[1].receive[0].is_some());
        assert!(shared.peer_bound(0));
        assert_eq!(workers[0].counters.queue_drops, 0);
        assert_eq!(workers[1].counters.queue_drops, 0);
        let descriptors: Vec<_> = workers
            .iter()
            .map(|w| w.receive[0].as_ref().unwrap().as_fd().as_raw_fd())
            .collect();
        for worker in &mut workers {
            worker.setup(Duration::from_secs(2)).unwrap();
        }
        assert_eq!(
            descriptors,
            workers
                .iter()
                .map(|w| w.receive[0].as_ref().unwrap().as_fd().as_raw_fd())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    #[ignore = "kernel descriptor test; run in a disposable container"]
    fn colliding_receive_flows_register_on_distinct_owners() {
        let (first, _tap) = receive_worker();
        let mut second = Worker::new(1, Arc::clone(&first.shared), Control::new()).unwrap();
        second.command(
            0,
            Command {
                generation: 1,
                phase: Phase::Install,
                connection: first.sessions[0].connection,
            },
            0,
        );
        let mut workers = [first, second];
        for from in 0..2 {
            let (tx, rx) = crate::spsc::channel();
            workers[from].outgoing[1 - from] = Some(tx);
            workers[1 - from].incoming[from] = Some(rx);
        }
        // Both new lanes arrive on the same socket owner, as with reuseport collisions.
        for lane in [8_u16, 9] {
            let mut buffer = workers[0].free.pop().unwrap();
            buffer.length = wire::encode_datagram(
                Header {
                    cookie: crate::UnquicUuid::from_bytes([1; 16]),
                    receive_tag: 1,
                    lane,
                    packet_number: 1,
                    record_area_len: 0,
                },
                &[Record::Data {
                    frame_id: (u64::from(lane) << 48) | 1,
                    total_len: 60,
                    offset: 0,
                    bytes: &[5; 30],
                }],
                &mut buffer.bytes,
            )
            .unwrap();
            workers[0].received.push_back(buffer);
        }
        for _ in 0..4 {
            for worker in &mut workers {
                worker.drain_queues(Duration::ZERO).unwrap();
                worker.drain_packets(Duration::ZERO).unwrap();
                worker.flush_returns();
            }
        }
        for (lane, owner) in [(8, 0), (9, 1)] {
            assert_eq!(
                workers[0].shared.owners[0][lane].load(Ordering::Acquire),
                owner as u8 + 1
            );
            let slot = workers[owner].sessions[0].rx_index[lane];
            assert_ne!(slot, 0);
            assert_eq!(
                workers[owner].sessions[0].rx[usize::from(slot - 1)]
                    .as_ref()
                    .unwrap()
                    .retained_frames(),
                1
            );
        }
        assert_eq!(workers[0].free.len(), BUFFERS);
        assert_eq!(workers[1].free.len(), BUFFERS);
    }

    #[test]
    #[ignore = "kernel descriptor test; run in a disposable container"]
    fn receive_and_transmit_cannot_exhaust_each_others_buffers() {
        let (mut worker, tap) = receive_worker();
        worker.sessions[0].tx.truncate(1);
        let socket = ReceiveSocket::bind("[::1]:40002".parse().unwrap(), None).unwrap();
        let sender = std::net::UdpSocket::bind("[::1]:0").unwrap();
        let mut bytes = [0; BASE_UDP_PAYLOAD];
        for number in 1..=32 {
            let length = receive_bytes(number, &[receive_piece(number, 0, &[9; 60])], &mut bytes);
            sender
                .send_to(&bytes[..length], socket.local_addr())
                .unwrap();
        }
        worker
            .reactor
            .add(socket.as_fd(), 2, libc::EPOLLIN as u32)
            .unwrap();
        worker.receive[0] = Some(socket);
        worker.read_socket(0, Duration::ZERO).unwrap();
        assert_eq!(worker.rx_retained, RECEIVE_BUFFERS);
        worker.read_socket(0, Duration::ZERO).unwrap();
        assert_eq!(worker.received.len(), RECEIVE_BUFFERS);
        let mut events = [libc::epoll_event { events: 0, u64: 0 }; 4];
        worker.receive_interest(true).unwrap();
        assert_eq!(worker.reactor.wait(&mut events, 0).unwrap(), 1);
        worker.receive_interest(false).unwrap();
        assert_eq!(worker.reactor.wait(&mut events, 0).unwrap(), 0);
        worker.shared.wakes[0].notify().unwrap();
        assert_eq!(worker.reactor.wait(&mut events, 0).unwrap(), 1);
        assert_eq!({ events[0].u64 }, WAKE_TOKEN);
        worker.reactor.drain(WAKE_TOKEN).unwrap();

        for _ in 0..BUFFERS {
            tap.send(&[0; 60]).unwrap();
        }
        for _ in 0..8 {
            worker.read_tap(0, Duration::ZERO).unwrap();
        }
        assert_eq!(worker.pending.len(), BUFFERS - RECEIVE_BUFFERS - 1);
        assert_eq!(worker.free.len(), 1);
        worker.flush_taps(Duration::ZERO).unwrap();
        assert!(!worker.sessions[0].tap_readable);
        assert_eq!(worker.reactor.wait(&mut events, 0).unwrap(), 0);

        worker.drain_packets(Duration::ZERO).unwrap();
        assert_eq!(worker.rx_retained, 0);
        worker.receive_interest(true).unwrap();
        let count = worker.reactor.wait(&mut events, 0).unwrap();
        assert!(events[..count].iter().any(|event| event.u64 == 2));
        assert!(!events[..count].iter().any(|event| event.u64 == TAP_TOKEN));
        assert_eq!(worker.free.len(), RECEIVE_BUFFERS + 1);
        worker.read_tap(0, Duration::ZERO).unwrap();
        assert_eq!(worker.free.len(), RECEIVE_BUFFERS + 1);
        for _ in 0..3 {
            worker.read_socket(0, Duration::ZERO).unwrap();
            assert_eq!(worker.rx_retained, RECEIVE_BUFFERS);
            worker.drain_packets(Duration::ZERO).unwrap();
        }
        assert_eq!(worker.rx_retained, 0);
        while let Some(buffer) = worker.pending.pop_front() {
            worker.release(buffer);
        }
        assert_eq!(worker.free.len(), BUFFERS);
        worker.flush_taps(Duration::ZERO).unwrap();
        assert!(worker.sessions[0].tap_readable);
        let count = worker.reactor.wait(&mut events, 0).unwrap();
        assert!(events[..count].iter().any(|event| event.u64 == TAP_TOKEN));
        worker.read_tap(0, Duration::ZERO).unwrap();
        assert_eq!(worker.pending.len(), RECEIVE_BUFFERS + 1);
        assert_eq!(worker.counters.admission_pressure, 0);
        assert_eq!(worker.counters.expired_frames, 0);
    }

    #[test]
    #[ignore = "kernel seccomp test; run in a disposable container"]
    fn denied_read_ring_setup_keeps_plain_tap_reads() {
        let mut filter = [
            libc::sock_filter {
                code: 0x20,
                jt: 0,
                jf: 0,
                k: 0,
            },
            libc::sock_filter {
                code: 0x15,
                jt: 0,
                jf: 1,
                k: libc::SYS_io_uring_setup as u32,
            },
            libc::sock_filter {
                code: 0x06,
                jt: 0,
                jf: 0,
                k: libc::SECCOMP_RET_ERRNO | libc::EPERM as u32,
            },
            libc::sock_filter {
                code: 0x06,
                jt: 0,
                jf: 0,
                k: libc::SECCOMP_RET_ALLOW,
            },
        ];
        let program = libc::sock_fprog {
            len: filter.len() as _,
            filter: filter.as_mut_ptr(),
        };
        // SAFETY: restricts only this test thread; the initialized BPF program denies one syscall.
        unsafe {
            assert_eq!(libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0), 0);
            assert_eq!(
                libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER, &program),
                0
            );
        }
        let (mut worker, tap) = receive_worker();
        assert!(worker.tap_reader.is_none());
        assert_eq!(worker.counters.tap_read_fallbacks, 1);
        worker.sessions[0].tx.truncate(1);
        tap.send(&[0; 60]).unwrap();
        worker.read_tap(0, Duration::ZERO).unwrap();
        assert_eq!(worker.pending.len(), 1);
        assert_eq!(worker.counters.tap_read_batches, 0);
        assert_eq!(worker.counters.tap_ring_frames, 0);
        assert_eq!(worker.counters.tap_read_calls, 2);
        assert_eq!(worker.counters.tap_read_requests, 2);
    }

    #[test]
    fn tap_lanes_stay_local_when_available_and_bound_every_configuration() {
        let frames = [
            crate::tap::test_common::udp::frame(false, 1, &[7; 22]),
            crate::tap::test_common::udp::frame(true, 1, &[9; 22]),
        ];
        let stacked: Vec<_> = frames
            .iter()
            .map(|frame| {
                let mut tagged = frame.clone();
                tagged.splice(12..12, [0x88, 0xa8, 0, 42, 0x81, 0, 0, 7]);
                tagged
            })
            .collect();
        for workers in 1..=64 {
            for lanes in 1..=64 {
                for worker in 0..workers {
                    for frame in &stacked {
                        assert_eq!(
                            tap_lane(frame, worker, workers, workers, lanes),
                            pack::flow_hash(frame) as usize % lanes
                        );
                    }
                    for frame in &frames {
                        let lane = tap_lane(frame, worker, workers, workers, lanes);
                        assert!(lane < lanes);
                        if worker < lanes {
                            assert_eq!(lane % workers, worker);
                        } else {
                            assert_eq!(lane, pack::flow_hash(frame) as usize % lanes);
                        }
                        assert_eq!(lane, tap_lane(frame, worker, workers, workers, lanes));
                        if workers > 1 && worker < workers - 1 {
                            assert_eq!(
                                tap_lane(frame, worker, workers, workers - 1, lanes),
                                pack::flow_hash(frame) as usize % lanes
                            );
                        }
                    }
                }
            }
        }
        for mut frame in frames {
            let at = if frame[12..14] == [8, 0] { 23 } else { 20 };
            frame[at] = 6;
            for worker in 0..2 {
                assert_eq!(
                    tap_lane(&frame, worker, 2, 2, 2),
                    pack::flow_hash(&frame) as usize % 2
                );
            }
        }
    }

    #[test]
    #[ignore = "kernel descriptor test; run in a disposable container"]
    fn tap_expansion_yields_preserves_order_and_progresses_under_pressure() {
        use crate::tap::test_common::udp;
        use std::os::unix::net::UnixDatagram;
        for ring in [false, true] {
            for size in [22, 1472] {
                let (mut worker, _) = receive_worker();
                worker.sessions[0].tx.truncate(1);
                if !ring {
                    worker.tap_reader = None;
                }
                let (fd, sender) = UnixDatagram::pair().unwrap();
                fd.set_nonblocking(true).unwrap();
                worker
                    .reactor
                    .remove(worker.sessions[0].tap.as_ref().unwrap().as_fd())
                    .unwrap();
                worker
                    .attach_tap(0, TapQueue::test_fd(fd.into(), true))
                    .unwrap();
                let count = if size == 22 { 64 } else { 44 };
                let payload: Vec<_> = (0..size * count).map(|i| (i * 13) as u8).collect();
                let mut input = udp::frame(false, 65530, &payload);
                let mut header = udp::partial(&mut input, false);
                header[1] = 5;
                header[4..6].copy_from_slice(&(size as u16).to_ne_bytes());
                let input = [header.as_slice(), input.as_slice()].concat();
                let plain = udp::frame(false, 123, &[7; 60]);
                for _ in 0..3 {
                    sender.send(&input).unwrap();
                }
                sender
                    .send(&[[0u8; 10].as_slice(), plain.as_slice()].concat())
                    .unwrap();
                let mut seen = 0;
                for _ in 0..20 {
                    worker.read_tap(0, Duration::ZERO).unwrap();
                    let retained = worker.sessions[0].tap_inputs.len();
                    if retained != 0 {
                        assert_eq!(worker.tap_capacity(0), 0);
                        worker.read_tap(0, Duration::ZERO).unwrap();
                        assert_eq!(worker.sessions[0].tap_inputs.len(), retained);
                    }
                    worker.drain_tap_inputs(Duration::ZERO).unwrap();
                    assert!(worker.pending.len() <= BATCH_SIZE + 1);
                    assert!(
                        worker.pending.iter().map(|b| b.length).sum::<usize>()
                            < 64 * 1024 + crate::MAX_FRAME_LEN
                    );
                    while let Some(buffer) = worker.pending.pop_front() {
                        let expected = if seen == count * 3 {
                            plain.clone()
                        } else {
                            let part = seen % count;
                            udp::frame(
                                false,
                                65530u16.wrapping_add(part as u16),
                                &payload[part * size..(part + 1) * size],
                            )
                        };
                        assert_eq!(&buffer.bytes[..buffer.length], expected);
                        assert_eq!(buffer.deadline, reassembly::FRAME_LIFETIME);
                        seen += 1;
                        worker.release(buffer);
                    }
                    if seen == count * 3 + 1 {
                        break;
                    }
                }
                assert_eq!(seen, count * 3 + 1);
                assert_eq!(worker.free.len(), BUFFERS);
                assert_eq!(worker.sessions[0].tx_bytes, 0);
                assert_eq!(worker.sessions[0].tx_retained, 0);
                assert_eq!(worker.counters.tap_gso_frames, (count * 3) as u64);
                assert_eq!(worker.counters.queue_drops, 0);

                // One spare output buffer must suffice; no wait for the whole expansion.
                let held = worker.free.split_off(RECEIVE_BUFFERS + 2);
                sender.send(&input).unwrap();
                worker.read_tap(0, Duration::ZERO).unwrap();
                assert!(!worker.drain_tap_inputs(Duration::ZERO).unwrap());
                assert_eq!(worker.pending.len(), 1);
                assert_eq!(worker.free.len(), RECEIVE_BUFFERS);
                let output = worker.pending.pop_front().unwrap();
                worker.release(output);
                worker.drain_tap_inputs(Duration::ZERO).unwrap();
                assert_eq!(worker.pending.len(), 1);
                assert!(!worker.sessions[0].tap_inputs.is_empty());
                worker.free.extend(held);
                let expiry = reassembly::FRAME_LIFETIME;
                worker.drain_tap_inputs(expiry).unwrap();
                let output = worker.pending.pop_front().unwrap();
                worker.release(output);
                assert!(worker.sessions[0].tap_inputs.is_empty());
                assert_eq!(worker.sessions[0].tx_bytes, 0);
                assert_eq!(worker.free.len(), BUFFERS);
                assert_eq!(worker.counters.expired_frames, (count - 2) as u64);

                sender.send(&input).unwrap();
                worker.read_tap(0, expiry).unwrap();
                worker.command(
                    0,
                    Command {
                        generation: 2,
                        phase: Phase::Quiesce,
                        connection: None,
                    },
                    0,
                );
                worker.drain_tap_inputs(expiry).unwrap();
                assert!(worker.sessions[0].tap_inputs.is_empty());
                assert_eq!(worker.sessions[0].tx_retained, 0);
                assert_eq!(worker.free.len(), BUFFERS);
            }
        }
    }

    #[test]
    #[ignore = "kernel descriptor test; run in a disposable container"]
    fn super_frame_handoff_expands_only_on_its_owner_and_keeps_order() {
        use crate::tap::test_common::udp;
        use std::os::unix::net::UnixDatagram;
        for (size, count) in [(1472, 44), (1, 10_000)] {
            let (_, mut workers) = test_workers();
            let params = wire::Params {
                port: 40000,
                max_frame_len: 9022,
                mandatory_features: 0,
                optional_features: 0,
            };
            for from in 0..2 {
                let (tx, rx) = crate::spsc::channel();
                workers[from].outgoing[1 - from] = Some(tx);
                workers[1 - from].incoming[from] = Some(rx);
                workers[from].command(
                    0,
                    Command {
                        generation: 1,
                        phase: Phase::Install,
                        connection: Some(Connection {
                            role: handshake::Role::Initiator,
                            initiator: Tag::new(1).unwrap(),
                            responder: Tag::new(2).unwrap(),
                            local: params,
                            peer: params,
                            max_frame_len: 9022,
                        }),
                    },
                    0,
                );
            }
            let tags = [0x88, 0xa8, 0, 7, 0x81, 0, 0, 42];
            let payload: Vec<_> = (0..size * count).map(|i| (i * 13) as u8).collect();
            let mut input = udp::frame(false, 65530, &payload);
            let mut header = udp::partial(&mut input, false);
            input.splice(12..12, tags);
            header[1] = 5;
            header[4..6].copy_from_slice(&(size as u16).to_ne_bytes());
            header[6..8].copy_from_slice(&42u16.to_ne_bytes());
            input[11] = (0..=255)
                .find(|&mac| {
                    input[11] = mac;
                    pack::flow_hash(&input) % 2 == 1
                })
                .unwrap();
            let mut plain = udp::frame(false, 123, &[7; 60]);
            plain[..12].copy_from_slice(&input[..12]);
            plain.splice(12..12, tags);
            let (fd, sender) = UnixDatagram::pair().unwrap();
            fd.set_nonblocking(true).unwrap();
            workers[0]
                .attach_tap(0, TapQueue::test_fd(fd.into(), true))
                .unwrap();
            sender
                .send(&[header.as_slice(), input.as_slice()].concat())
                .unwrap();
            workers[0].read_tap(0, Duration::ZERO).unwrap();
            assert!(workers[0].sessions[0].tap_inputs.is_empty());
            let incoming = workers[1].incoming[0].as_mut().unwrap();
            let Some(Event::Frame(source)) = incoming.try_pop() else {
                panic!("missing source handoff")
            };
            assert!(incoming.try_pop().is_none());
            assert_eq!(source.length, input.len());
            assert!(source.segments.is_some());
            assert_eq!(source.lane, 1);
            let segments = source.segments;
            workers[1].input_frame(source, Duration::ZERO).unwrap();
            sender
                .send(&[[0; 10].as_slice(), plain.as_slice()].concat())
                .unwrap();
            workers[0].read_tap(0, Duration::ZERO).unwrap();
            workers[1].drain_queues(Duration::ZERO).unwrap();
            assert_eq!(workers[1].sessions[0].tap_inputs.len(), 2);
            let mut seen = 0;
            while seen < count + 1 {
                workers[1].drain_tap_inputs(Duration::ZERO).unwrap();
                assert!(workers[1].pending.len() <= BATCH_SIZE);
                assert!(
                    workers[1].pending.iter().map(|b| b.length).sum::<usize>()
                        < 64 * 1024 + MAX_FRAME_LEN
                );
                assert!(!workers[1].pending.is_empty());
                while let Some(frame) = workers[1].pending.pop_front() {
                    let expected = if seen == count {
                        plain.clone()
                    } else {
                        let mut expected = udp::frame(
                            false,
                            65530u16.wrapping_add(seen as u16),
                            &payload[seen * size..(seen + 1) * size],
                        );
                        expected[..12].copy_from_slice(&input[..12]);
                        expected.splice(12..12, tags);
                        expected
                    };
                    assert_eq!(&frame.bytes[..frame.length], expected);
                    assert_eq!(frame.frame_id, (2 << 48) | (seen as u64 + 1));
                    assert_eq!(frame.deadline, reassembly::FRAME_LIFETIME);
                    assert_eq!(frame.origin, usize::from(seen != count));
                    workers[1].release(frame);
                    seen += 1;
                }
                workers[0].drain_queues(Duration::ZERO).unwrap();
            }
            assert_eq!(workers[0].counters.tap_gso_reads, 1);
            assert_eq!(workers[0].counters.tap_gso_frames, 0);
            assert_eq!(workers[1].counters.tap_gso_frames, count as u64);
            for worker in &workers {
                assert_eq!(worker.free.len(), BUFFERS);
                assert_eq!(worker.sessions[0].tx_retained, 0);
                assert_eq!(worker.sessions[0].tx_bytes, 0);
                assert_eq!(worker.counters.queue_drops, 0);
            }

            // Foreign inputs share a fixed queue; quiesce must return every source.
            for _ in 0..BUFFERS {
                let mut source = workers[0].free.pop().unwrap();
                source.session = 0;
                source.tag = 1;
                source.lane = 1;
                source.length = input.len();
                source.segments = segments;
                source.deadline = reassembly::FRAME_LIFETIME;
                source.tx_reserved = true;
                workers[0].sessions[0].tx_retained += 1;
                workers[0].sessions[0].tx_bytes += source.length;
                workers[1].input_frame(source, Duration::ZERO).unwrap();
            }
            let mut extra = workers[1].free.pop().unwrap();
            extra.session = 0;
            extra.tag = 1;
            extra.lane = 1;
            extra.length = 60;
            extra.deadline = reassembly::FRAME_LIFETIME;
            workers[1].input_frame(extra, Duration::ZERO).unwrap();
            assert_eq!(workers[1].sessions[0].tap_inputs.len(), BUFFERS);
            assert_eq!(workers[1].sessions[0].tap_inputs.capacity(), BUFFERS);
            assert_eq!(workers[1].counters.queue_drops, 1);
            for _ in 0..crate::spsc::CAPACITY {
                assert!(
                    workers[1]
                        .push(
                            0,
                            Event::ProbeFeedback {
                                session: 0,
                                tag: 0,
                                lane: 1,
                                packet: 0,
                                length: 0,
                            }
                        )
                        .is_ok()
                );
            }
            workers[1].command(
                0,
                Command {
                    generation: 2,
                    phase: Phase::Quiesce,
                    connection: None,
                },
                0,
            );
            for _ in 0..BUFFERS / BATCH_SIZE {
                workers[1].drain_tap_inputs(Duration::ZERO).unwrap();
            }
            assert!(workers[1].sessions[0].tap_inputs.is_empty());
            assert_eq!(workers[1].returns[0].len(), BUFFERS);
            for _ in 0..32 {
                workers[0].drain_queues(Duration::ZERO).unwrap();
                workers[1].flush_returns();
            }
            for worker in &workers {
                assert_eq!(worker.free.len(), BUFFERS);
                assert_eq!(worker.sessions[0].tx_retained, 0);
                assert_eq!(worker.sessions[0].tx_bytes, 0);
            }
        }
    }

    #[test]
    #[ignore = "kernel ring test; run in a disposable container"]
    fn pool_pressure_does_not_shrink_the_tap_read_budget() {
        let (mut worker, tap) = receive_worker();
        assert!(worker.tap_reader.as_ref().unwrap().enabled);
        worker.sessions[0].tx.truncate(1);
        worker.sessions[0].tap_read_limit = BATCH_SIZE;
        let held = worker.free.split_off(RECEIVE_BUFFERS + 2);
        tap.send(&[0; 60]).unwrap();
        worker.read_tap(0, Duration::ZERO).unwrap();
        assert_eq!(worker.sessions[0].tap_read_limit, BATCH_SIZE);
        assert_eq!(worker.pending.len(), 1);
        assert_eq!(worker.free.len(), RECEIVE_BUFFERS + 1);
        worker.free.extend(held);
        for _ in 0..BATCH_SIZE {
            tap.send(&[0; 60]).unwrap();
        }
        worker.read_tap(0, Duration::ZERO).unwrap();
        assert_eq!(worker.pending.len(), BATCH_SIZE + 1);
        assert_eq!(worker.counters.tap_read_requests, (BATCH_SIZE + 1) as u64);
        assert_eq!(worker.counters.tap_ring_frames, (BATCH_SIZE + 1) as u64);
        assert_eq!(worker.counters.tap_read_calls, 2);
        worker.read_tap(0, Duration::ZERO).unwrap();
        assert_eq!(worker.sessions[0].tap_read_limit, 1);
        for _ in 0..2 {
            tap.send(&[0; 60]).unwrap();
        }
        worker.read_tap(0, Duration::ZERO).unwrap();
        assert_eq!(worker.sessions[0].tap_read_limit, 2);
        worker.read_tap(0, Duration::ZERO).unwrap();
        assert_eq!(worker.pending.len(), BATCH_SIZE + 3);
        assert_eq!(worker.counters.queue_drops, 0);
    }

    #[test]
    #[ignore = "kernel descriptor test; run in a disposable container"]
    fn receive_bursts_budget_expanded_datagrams_records_and_bytes() {
        // Exercise ordinary bursts, GRO bursts and record-heavy packed datagrams.
        for (segments, records) in [(1, 1), (100, 1), (20, 16)] {
            let (mut worker, rx) = receive_worker();
            let mut number = 1;
            for _ in 0..if segments == 1 { 100 } else { 2 } {
                let mut buffer = worker.free.pop().unwrap();
                for _ in 0..segments {
                    let data: Vec<_> = (0..records)
                        .map(|r| receive_piece((number - 1) * records + r + 1, 0, &[5; 60]))
                        .collect();
                    let length = receive_bytes(number, &data, &mut buffer.bytes[buffer.length..]);
                    if segments != 1 {
                        buffer.segment_size = length;
                    }
                    buffer.length += length;
                    number += 1;
                }
                worker.received.push_back(buffer);
            }
            assert!(worker.drain_packets(Duration::ZERO).unwrap());
            let first_bytes = worker.counters.delivered_frame_bytes;
            assert_eq!(first_bytes, 64 * 60);
            // Remaining work must yield to the caller, even inside one aggregate.
            assert!(!worker.received.is_empty());
            let mut delivered = 0;
            loop {
                let mut bytes = [0; 60];
                while let Ok(n) = rx.recv(&mut bytes) {
                    assert_eq!(n, 60);
                    assert_eq!(bytes, [5; 60]);
                    delivered += 1;
                }
                if !worker.drain_packets(Duration::ZERO).unwrap() {
                    worker.flush_taps(Duration::ZERO).unwrap();
                    while rx.recv(&mut bytes).is_ok() {
                        delivered += 1;
                    }
                    break;
                }
            }
            assert_eq!(delivered, (number - 1) * records);
            assert_eq!(worker.counters.admission_pressure, 0);
            assert_eq!(worker.counters.expired_frames, 0);
            assert_eq!(worker.free.len(), BUFFERS);
        }

        // The byte budget must yield before the datagram or record budgets.
        let (mut worker, _) = receive_worker();
        let mut number = 1;
        for _ in 0..2 {
            let mut buffer = worker.free.pop().unwrap();
            for _ in 0..30 {
                let length = receive_bytes(
                    number,
                    &[Record::Data {
                        frame_id: (7 << 48) | number,
                        total_len: 1514,
                        offset: 0,
                        bytes: &[3; 1164],
                    }],
                    &mut buffer.bytes[buffer.length..],
                );
                assert_eq!(length, 1232);
                buffer.segment_size = length;
                buffer.length += length;
                number += 1;
            }
            worker.received.push_back(buffer);
        }
        assert!(worker.drain_packets(Duration::ZERO).unwrap());
        assert_eq!(
            worker.sessions[0].rx[0].as_ref().unwrap().retained_frames(),
            54
        );
        assert_eq!(worker.received.front().unwrap().offset, 24 * 1232);
        assert!(!worker.drain_packets(Duration::ZERO).unwrap());
        assert_eq!(worker.counters.admission_pressure, 0);
    }

    #[test]
    #[ignore = "kernel descriptor test; run in a disposable container"]
    fn blocked_tap_keeps_tails_but_never_acks_an_unretained_head() {
        use wire::{AckRange, MAX_ACK_RANGES};
        for blocked in [false, true] {
            let (mut worker, rx) = receive_worker();
            let now = Duration::ZERO;
            let mut bytes = [0; 256];
            for number in 1..=64 {
                let length =
                    receive_bytes(number, &[receive_piece(number, 0, &[7; 30])], &mut bytes);
                assert_eq!(
                    worker.sessions[0].rx[0]
                        .as_mut()
                        .unwrap()
                        .admit(&Datagram::decode(&bytes[..length]).unwrap(), now),
                    Admission::Fresh
                );
            }
            // Fill the simulated TAP's send queue before the completing piece arrives.
            let tap = worker.sessions[0].tap.as_mut().unwrap();
            if blocked {
                while tap.write_frame(&[0; 60]).is_ok() {}
            }
            let records = [
                receive_piece(1, 30, &[7; 30]),
                receive_piece(65, 0, &[9; 30]),
            ];
            for _ in 0..2 {
                let mut buffer = worker.free.pop().unwrap();
                buffer.length = receive_bytes(65, &records, &mut buffer.bytes);
                worker.received.push_back(buffer);
                worker.drain_packets(now).unwrap();
            }
            assert_eq!(
                worker.counters.admission_pressure,
                if blocked { 2 } else { 0 }
            );
            let lane = worker.sessions[0].rx[0].as_mut().unwrap();
            assert_eq!(lane.retained_frames(), 64);
            if blocked {
                assert_eq!(lane.front(now).unwrap().bytes, &[7; 60]);
            }
            let mut ranges = [AckRange { start: 0, end: 0 }; MAX_ACK_RANGES];
            let Some(Record::Ack {
                ranges: accepted, ..
            }) = lane.take_ack(now, &mut ranges)
            else {
                panic!()
            };
            assert_eq!(
                accepted.iter().collect::<Vec<_>>(),
                [AckRange {
                    start: 1,
                    end: if blocked { 64 } else { 65 }
                }]
            );
            if blocked {
                let retained = std::mem::take(&mut worker.free);
                worker.flush_taps(now).unwrap();
                assert!(!worker.sessions[0].tap_readable);
                assert!(worker.sessions[0].tap_writable);
                let mut events = [libc::epoll_event { events: 0, u64: 0 }; 4];
                assert_eq!(worker.reactor.wait(&mut events, 0).unwrap(), 0);
                while rx.recv(&mut bytes).is_ok() {}
                assert_eq!(worker.reactor.wait(&mut events, 0).unwrap(), 1);
                assert_eq!({ events[0].u64 }, TAP_TOKEN);
                assert_ne!(events[0].events & libc::EPOLLOUT as u32, 0);
                assert_eq!(events[0].events & libc::EPOLLIN as u32, 0);
                worker.free = retained;
            }
            worker.sessions[0].tap_blocked = false; // Simulate EPOLLOUT after draining.
            worker.flush_taps(now).unwrap();
            assert_eq!(rx.recv(&mut bytes).unwrap(), 60);
            assert_eq!(&bytes[..60], &[7; 60]);
            // Retry identical bytes and packet number: only the new head is retained.
            let mut buffer = worker.free.pop().unwrap();
            buffer.length = receive_bytes(65, &records, &mut buffer.bytes);
            worker.received.push_back(buffer);
            worker.drain_packets(now).unwrap();
            let mut buffer = worker.free.pop().unwrap();
            buffer.length =
                receive_bytes(66, &[receive_piece(65, 30, &[9; 30])], &mut buffer.bytes);
            worker.received.push_back(buffer);
            worker.drain_packets(now).unwrap();
            worker.flush_taps(now).unwrap();
            assert_eq!(rx.recv(&mut bytes).unwrap(), 60);
            assert_eq!(&bytes[..60], &[9; 60]);
            assert_eq!(
                rx.recv(&mut bytes).unwrap_err().kind(),
                io::ErrorKind::WouldBlock
            );
            assert_eq!(worker.counters.delivered_frame_bytes, 120);
            assert_eq!(worker.free.len(), BUFFERS);
            let lane = worker.sessions[0].rx[0].as_mut().unwrap();
            let Some(Record::Ack {
                ranges: accepted, ..
            }) = lane.take_ack(Duration::from_millis(1), &mut ranges)
            else {
                panic!()
            };
            assert_eq!(
                accepted.iter().collect::<Vec<_>>(),
                [AckRange { start: 1, end: 66 }]
            );
        }
    }

    #[test]
    fn origin_reservations_bound_every_session_direction() {
        for workers in 1..=64 {
            assert!(frame_quota(workers) * workers <= 8192);
            assert!(frame_quota(workers) * workers * MAX_FRAME_LEN <= 64 * 1024 * 1024);
            assert!(frame_quota(workers) <= BUFFERS);
        }
        assert_eq!(frame_quota(1), BUFFERS);
        assert_eq!(frame_quota(64), 116);
    }

    #[test]
    #[ignore = "kernel descriptor test; run in a disposable container"]
    fn mtu_probes_route_to_lane_owners_and_survive_silent_size_filters() {
        let (first, _tap) = receive_worker();
        let second = Worker::new(1, Arc::clone(&first.shared), Control::new()).unwrap();
        let mut workers = [first, second];
        let connection = workers[0].sessions[0].connection;
        let remote = ReceiveSocket::bind("[::1]:40101".parse().unwrap(), None).unwrap();
        let mut payload = [0; crate::socket::MAX_UDP_PAYLOAD];
        for (index, worker) in workers.iter_mut().enumerate() {
            for session in 0..2 {
                worker.command(
                    session,
                    Command {
                        generation: 1,
                        phase: Phase::Install,
                        connection,
                    },
                    0,
                );
                worker.sessions[session].acknowledged = None;
            }
            let path = &mut worker.transmit[0].paths[0];
            path.address = Some(remote.local_addr());
            let tx = &mut path.lanes[index];
            tx.mtu = pmtu::Path::new(crate::socket::MAX_UDP_PAYLOAD, Duration::ZERO).unwrap();
            tx.socket = Some(TransmitSocket::connect(&remote, remote.local_addr()).unwrap());
        }
        let (tx, rx) = crate::spsc::channel();
        workers[0].outgoing[1] = Some(tx);
        workers[1].incoming[0] = Some(rx);
        let mut reply_number = 0;
        let mut sent = [0; 2];
        let mut now = Duration::ZERO;
        // The reverse path remains usable; oversized forward probes disappear without ICMP.
        for capacity in [
            [1452, 4096],
            [BASE_UDP_PAYLOAD, crate::socket::MAX_UDP_PAYLOAD],
        ] {
            let end = now + Duration::from_secs(650);
            loop {
                for worker in &mut workers {
                    assert!(!worker.mtu_timers(now).unwrap());
                }
                while let Ok((length, _)) = remote.recv(&mut payload) {
                    let datagram = Datagram::decode(&payload[..length]).unwrap();
                    assert!(matches!(datagram.records().next(), Some(Record::Probe)));
                    let header = datagram.header();
                    let index = usize::from(header.lane) - 1;
                    sent[index] += 1;
                    if length > capacity[index] {
                        continue;
                    }
                    reply_number += 1;
                    let mut reply = workers[0].free.pop().unwrap();
                    reply.length = receive_bytes(
                        reply_number,
                        &[Record::ProbeReply {
                            lane: header.lane,
                            packet_number: header.packet_number,
                            payload_len: length as u16,
                        }],
                        &mut reply.bytes,
                    );
                    workers[0].received.push_back(reply);
                    workers[0]
                        .drain_packets(now + Duration::from_millis(1))
                        .unwrap();
                }
                workers[1]
                    .drain_queues(now + Duration::from_millis(1))
                    .unwrap();
                if workers.iter().enumerate().all(|(index, worker)| {
                    let mtu = &worker.transmit[0].paths[0].lanes[index].mtu;
                    mtu.payload_limit() == capacity[index]
                        && matches!(
                            mtu.state(),
                            crate::counters::PathMtuState::Baseline
                                | crate::counters::PathMtuState::Confirmed
                        )
                }) {
                    break;
                }
                assert!(now < end, "silent size filter did not converge");
                let due = workers.iter().map(|w| w.mtu_due).min().unwrap();
                now = due.max(now + Duration::from_millis(2));
            }
            for (index, worker) in workers.iter().enumerate() {
                assert!(sent[index] > 1);
                // One physical path is shared by both sessions, with independent lane limits.
                for session in &worker.sessions {
                    assert_eq!(
                        session.tx_totals[index].path_mtu_bytes,
                        (capacity[index] + 48) as u64
                    );
                }
            }
            now += Duration::from_secs(1);
        }
        assert_eq!(workers[0].free.len(), BUFFERS);
    }

    #[test]
    #[ignore = "kernel descriptor test; run in a disposable container"]
    fn mtu_reduction_repacks_unsent_ranges_in_every_shared_session() {
        let (mut worker, _tap) = receive_worker();
        let now = Duration::from_millis(10);
        let connection = worker.sessions[0].connection;
        worker.command(
            1,
            Command {
                generation: 1,
                phase: Phase::Install,
                connection,
            },
            0,
        );
        let mtu = &mut worker.transmit[0].paths[0].lanes[0].mtu;
        *mtu = pmtu::Path::new(1452, Duration::ZERO).unwrap();
        for (number, at) in [(1, Duration::ZERO), (2, now)] {
            let length = mtu.poll(at).unwrap();
            let id = pmtu::ProbeId {
                session: 0,
                tag: 1,
                lane: 1,
                packet: number,
            };
            assert!(mtu.submitted(id, length, at));
            assert!(mtu.reply(id, length, at + Duration::from_millis(1)));
        }
        let old_limit = mtu.payload_limit();
        assert!(old_limit > BASE_UDP_PAYLOAD);
        let mut expected = [(0, 0, 0); 2];
        for (session, expected) in expected.iter_mut().enumerate() {
            let mut frame = worker.free.pop().unwrap();
            frame.length = 9022;
            frame.session = session;
            frame.lane = 0;
            frame.tag = 1;
            frame.deadline = now + Duration::from_millis(100);
            let lane = worker.sessions[session].tx[0].as_mut().unwrap();
            frame.frame_id = lane.start_frame(frame.length).unwrap();
            let batch = worker.sessions[session].batches[0].as_mut().unwrap();
            batch.frames[0] = Some(frame);
            batch.len = 1;
            let mut batch = worker.sessions[session].batches[0].take().unwrap();
            batch
                .prepare(worker.sessions[session].tx[0].as_mut().unwrap(), old_limit)
                .unwrap();
            let last = batch.plan.packet(batch.plan.len() - 1).unwrap().number;
            worker.submitted_batch(session, 0, &mut batch, 1);
            let frame = batch.frames[0].as_ref().unwrap();
            *expected = (frame.frame_id, frame.offset, last);
            worker.sessions[session].batches[0] = Some(batch);
        }
        let mtu = &mut worker.transmit[0].paths[0].lanes[0].mtu;
        let before = (mtu.payload_limit(), mtu.state());
        assert!(mtu.local_too_big(old_limit, 1280, now));
        worker.mtu_changed(0, 0, 0, before, now);
        for (session, (id, offset, last)) in expected.into_iter().enumerate() {
            let local = &mut worker.sessions[session];
            let batch = local.batches[0].as_mut().unwrap();
            assert!(batch.plan.is_empty());
            assert_eq!(batch.frames[0].as_ref().unwrap().frame_id, id);
            assert_eq!(batch.frames[0].as_ref().unwrap().offset, offset);
            batch
                .prepare(local.tx[0].as_mut().unwrap(), BASE_UDP_PAYLOAD)
                .unwrap();
            assert!(batch.plan.packet(0).unwrap().number > last);
            assert_eq!(
                batch.plan.pieces(0).unwrap().next().unwrap().1.start,
                offset
            );
            assert!(
                (0..batch.plan.len())
                    .all(|i| batch.plan.packet(i).unwrap().length <= BASE_UDP_PAYLOAD)
            );
        }
    }

    #[test]
    #[ignore = "requires a disposable container and network namespace capabilities"]
    fn mtu_error_queue_rejects_forged_quotes_and_accepts_live_probe_ptb() {
        use std::{
            net::{SocketAddrV6, UdpSocket},
            process::Command,
        };
        // SAFETY: only this test thread and its children enter the disposable namespace.
        cvt(unsafe { libc::unshare(libc::CLONE_NEWNET) }).unwrap();
        assert!(
            Command::new("ip")
                .args(["link", "set", "lo", "mtu", "9000", "up"])
                .status()
                .unwrap()
                .success()
        );
        let (mut worker, _tap) = receive_worker();
        let remote = ReceiveSocket::bind("[::1]:40101".parse().unwrap(), None).unwrap();
        let mut socket = TransmitSocket::connect(&remote, remote.local_addr()).unwrap();
        socket.enable_mtu_discovery().unwrap();
        worker.reactor.add(socket.as_fd(), TX_TOKEN, 0).unwrap();
        worker.transmit[0].paths[0].address = Some(remote.local_addr());
        worker.transmit[0].paths[0].lanes[0].socket = Some(socket);
        worker.transmit[0].paths[0].lanes[0].mtu =
            pmtu::Path::new(crate::socket::MAX_UDP_PAYLOAD, Duration::ZERO).unwrap();
        let mut bytes = [0; crate::socket::MAX_UDP_PAYLOAD];
        worker.mtu_timers(Duration::ZERO).unwrap();
        let (length, _) = remote.recv(&mut bytes).unwrap();
        let baseline = Header::decode(&bytes[..length]).unwrap();
        worker.feedback(
            0,
            1,
            baseline.packet_number,
            length as u16,
            Duration::from_millis(1),
        );
        let now = Duration::from_millis(6);
        worker.mtu_timers(now).unwrap();
        let (length, source) = remote.recv(&mut bytes).unwrap();
        assert!(length > 1452);
        let large = Header::decode(&bytes[..length]).unwrap();
        // SAFETY: socket returns a new descriptor transferred to its sole owner.
        let icmp = unsafe {
            UdpSocket::from_raw_fd(
                cvt(libc::socket(
                    libc::AF_INET6,
                    libc::SOCK_RAW | libc::SOCK_CLOEXEC,
                    libc::IPPROTO_ICMPV6,
                ))
                .unwrap(),
            )
        };
        let mut message = [0; 8 + 40 + 8 + wire::HEADER_LEN];
        message[0] = 2;
        message[4..8].copy_from_slice(&1500_u32.to_be_bytes());
        let quoted = &mut message[8..];
        quoted[0] = 0x60;
        quoted[4..6].copy_from_slice(&((length + 8) as u16).to_be_bytes());
        quoted[6] = libc::IPPROTO_UDP as u8;
        quoted[7] = 64;
        quoted[8..24].copy_from_slice(&source.ip().octets());
        quoted[24..40].copy_from_slice(&remote.local_addr().ip().octets());
        quoted[40..42].copy_from_slice(&source.port().to_be_bytes());
        quoted[42..44].copy_from_slice(&remote.local_addr().port().to_be_bytes());
        quoted[44..46].copy_from_slice(&((length + 8) as u16).to_be_bytes());
        quoted[48..].copy_from_slice(&bytes[..wire::HEADER_LEN]);
        let destination = SocketAddrV6::new(*source.ip(), 0, 0, 0);
        for forged in [true, false] {
            if forged {
                message[8 + 48 + 4] ^= 1;
            }
            assert_eq!(icmp.send_to(&message, destination).unwrap(), message.len());
            let mut events = [libc::epoll_event { events: 0, u64: 0 }; 4];
            let count = worker.reactor.wait(&mut events, 100).unwrap();
            assert!(events[..count].iter().any(|event| event.u64 == TX_TOKEN
                && event.events & libc::EPOLLERR as u32 != 0));
            if forged {
                let socket = worker.transmit[0].paths[0].lanes[0]
                    .socket
                    .as_ref()
                    .unwrap();
                assert_eq!(
                    socket
                        .send(&bytes[..BASE_UDP_PAYLOAD], BASE_UDP_PAYLOAD)
                        .unwrap_err()
                        .raw_os_error(),
                    Some(libc::EMSGSIZE)
                );
                worker
                    .mtu_send_error(0, 0, 0, BASE_UDP_PAYLOAD, now)
                    .unwrap();
            } else {
                assert!(worker.mtu_error(0, 0, 0, now).unwrap());
            }
            let mtu = &worker.transmit[0].paths[0].lanes[0].mtu;
            assert_eq!(mtu.probes(now).count(), usize::from(forged));
            assert_eq!(mtu.payload_limit(), BASE_UDP_PAYLOAD);
            if forged {
                message[8 + 48 + 4] ^= 1;
            }
        }
        // An old success cannot undo an accepted reduction, and hints alone never raise DATA.
        worker.feedback(0, 1, large.packet_number, length as u16, now);
        assert_eq!(
            worker.transmit[0].paths[0].lanes[0].mtu.payload_limit(),
            BASE_UDP_PAYLOAD
        );
        worker.mtu_timers(Duration::from_millis(12)).unwrap();
        let (length, _) = remote.recv(&mut bytes).unwrap();
        assert_eq!(length, 1452);
        let packet = Header::decode(&bytes[..length]).unwrap();
        worker.feedback(
            0,
            1,
            packet.packet_number,
            length as u16,
            Duration::from_millis(13),
        );
        assert_eq!(
            worker.transmit[0].paths[0].lanes[0].mtu.payload_limit(),
            1452
        );
        assert!(
            worker.transmit[0].paths[0].lanes[0]
                .socket
                .as_ref()
                .unwrap()
                .gso_enabled()
        );
        assert!(
            Command::new("ip")
                .args(["link", "set", "lo", "mtu", "1400"])
                .status()
                .unwrap()
                .success()
        );
        let now = Duration::from_millis(20);
        let mut frame = worker.free.pop().unwrap();
        frame.session = 0;
        frame.tag = 1;
        frame.lane = 0;
        frame.length = 9022;
        frame.deadline = Duration::from_secs(1);
        worker.frame(frame, now).unwrap();
        worker.flush_frames(now).unwrap();
        let batch = worker.sessions[0].batches[0].as_ref().unwrap();
        assert_eq!(batch.len, 1);
        assert!(batch.plan.is_empty());
        assert_eq!(batch.frames[0].as_ref().unwrap().offset, 0);
        let tx = &worker.transmit[0].paths[0].lanes[0];
        assert_eq!(tx.mtu.payload_limit(), BASE_UDP_PAYLOAD);
        assert!(tx.socket.as_ref().unwrap().gso_enabled());
        worker.flush_frames(now).unwrap();
        let mut delivered = 0;
        while let Ok((length, _)) = remote.recv(&mut bytes) {
            assert!(length <= BASE_UDP_PAYLOAD);
            for record in Datagram::decode(&bytes[..length]).unwrap().records() {
                if let Record::Data { bytes, .. } = record {
                    delivered += bytes.len();
                }
            }
        }
        assert_eq!(delivered, 9022);
        assert!(worker.ready.is_empty());
        assert_eq!(worker.free.len(), BUFFERS);
    }

    #[test]
    #[ignore = "kernel descriptor test; run in a disposable container"]
    fn transmit_pending_keeps_fifo_across_full_batches() {
        for expired in [0, 65, 66] {
            let (_, mut workers) = test_workers();
            let mut other = workers[1].free.pop().unwrap();
            let worker = &mut workers[0];
            let (tx, _rx) = crate::spsc::channel();
            worker.outgoing[1] = Some(tx);
            let params = wire::Params {
                port: 40000,
                max_frame_len: 1514,
                mandatory_features: 0,
                optional_features: 0,
            };
            for session in 0..2 {
                worker.command(
                    session,
                    Command {
                        generation: 0,
                        phase: Phase::Install,
                        connection: Some(Connection {
                            role: handshake::Role::Initiator,
                            initiator: Tag::new(1).unwrap(),
                            responder: Tag::new(2).unwrap(),
                            local: params,
                            peer: params,
                            max_frame_len: 1514,
                        }),
                    },
                    0,
                );
            }
            let rx = ReceiveSocket::bind("[::1]:40001".parse().unwrap(), None).unwrap();
            worker.transmit[0].paths[0].lanes[0].socket =
                Some(TransmitSocket::connect(&rx, rx.local_addr()).unwrap());
            worker.transmit[0].paths[0].lanes[0].blocked = true;
            for _ in 0..128 {
                let mut buffer = worker.free.pop().unwrap();
                buffer.tag = 1;
                buffer.length = 60;
                buffer.deadline = Duration::from_secs(1);
                worker.frame(buffer, Duration::ZERO).unwrap();
            }
            // Leave one batch slot and one pending frame beyond this turn's budget.
            let batch = worker.sessions[0].batches[0].as_mut().unwrap();
            for frame in &mut batch.frames[..63] {
                *frame = worker.pending.pop_front();
            }
            batch.len = 63;
            worker.sessions[0].tx_ready = 1;
            worker.ready.push_back((0, 0));
            other.session = 1;
            other.tag = 1;
            other.length = 60;
            other.deadline = Duration::from_secs(1);
            worker.frame(other, Duration::ZERO).unwrap();
            worker.flush_frames(Duration::ZERO).unwrap();
            let now = Duration::from_millis(10);
            if expired != 0 {
                worker
                    .pending
                    .iter_mut()
                    .find(|b| b.session == 0 && b.frame_id == (1 << 48) | expired)
                    .unwrap()
                    .deadline = now;
            }
            for _ in 0..3 {
                worker.flush_frames(now).unwrap();
                assert_eq!(worker.sessions[0].batches[0].as_ref().unwrap().len, 64);
                assert_eq!(worker.pending.len(), 64 - usize::from(expired != 0));
                assert_eq!(worker.sessions[1].batches[0].as_ref().unwrap().len, 1);
            }
            worker.transmit[0].paths[0].lanes[0].blocked = false;
            let mut received = Vec::new();
            let mut bytes = [0; BASE_UDP_PAYLOAD];
            for _ in 0..4 {
                worker.flush_frames(now).unwrap();
                while let Ok((length, _)) = rx.recv(&mut bytes) {
                    let datagram = Datagram::decode(&bytes[..length]).unwrap();
                    if datagram.header().cookie != crate::UnquicUuid::from_bytes([1; 16]) {
                        continue;
                    }
                    for record in datagram.records() {
                        if let Record::Data {
                            frame_id,
                            offset: 0,
                            ..
                        } = record
                        {
                            received.push(frame_id);
                        }
                    }
                }
            }
            assert_eq!(
                received,
                (1..=128)
                    .filter(|id| *id != expired)
                    .map(|id| (1 << 48) | id)
                    .collect::<Vec<_>>()
            );
            assert!(worker.pending.is_empty());
            let command = worker.sessions[0].installed.unwrap();
            worker.sessions[0].batches[0].as_mut().unwrap().deferred = 1;
            worker.command(
                0,
                Command {
                    generation: command.generation + 1,
                    ..command
                },
                0,
            );
            assert_eq!(worker.sessions[0].batches[0].as_ref().unwrap().deferred, 0);
        }
    }

    #[test]
    #[ignore = "kernel descriptor test; run in a disposable container"]
    fn transmit_batches_retain_partial_sends_expire_frames_and_quiesce() {
        let (_, mut workers) = test_workers();
        let params = wire::Params {
            port: 40000,
            max_frame_len: 9022,
            mandatory_features: 0,
            optional_features: 0,
        };
        for session in 0..2 {
            workers[0].command(
                session,
                Command {
                    generation: 0,
                    phase: Phase::Install,
                    connection: Some(Connection {
                        role: handshake::Role::Initiator,
                        initiator: Tag::new(1 + session as u128 * 10).unwrap(),
                        responder: Tag::new(2).unwrap(),
                        local: params,
                        peer: wire::Params {
                            port: 40001,
                            ..params
                        },
                        max_frame_len: 9022,
                    }),
                },
                0,
            );
            workers[0].sessions[session].acknowledged = None;
        }
        let rx = ReceiveSocket::bind("[::1]:40001".parse().unwrap(), None).unwrap();
        let tx = TransmitSocket::connect(&rx, rx.local_addr()).unwrap();
        workers[0].transmit[0].paths[0].lanes[0].socket = Some(tx);
        workers[0].transmit[0].paths[0].lanes[0].blocked = true;
        for index in 0..pack::BATCH_CAPACITY {
            let mut buffer = workers[0].free.pop().unwrap();
            buffer.session = 0;
            buffer.lane = 0;
            buffer.tag = 1;
            buffer.length = MAX_FRAME_LEN;
            buffer.bytes[..MAX_FRAME_LEN].fill(index as u8);
            buffer.deadline = Duration::from_millis(if index == 0 { 10 } else { 100 });
            buffer.tx_reserved = true;
            workers[0].sessions[0].tx_retained += 1;
            workers[0].sessions[0].tx_bytes += buffer.length;
            workers[0].frame(buffer, Duration::ZERO).unwrap();
        }
        workers[0].flush_frames(Duration::ZERO).unwrap();
        assert!(workers[0].pending.is_empty());
        assert_eq!(workers[0].ready.len(), 1);
        let mut batch = workers[0].sessions[0].batches[0].take().unwrap();
        batch
            .prepare(
                workers[0].sessions[0].tx[0].as_mut().unwrap(),
                BASE_UDP_PAYLOAD,
            )
            .unwrap();
        assert!(batch.plan.len() > 1);
        let mut last_packet = batch.plan.packet(batch.plan.len() - 1).unwrap().number;
        // Feed a successful one-packet prefix, then hold the socket unwritable.
        workers[0].submitted_batch(0, 0, &mut batch, 1);
        assert_eq!(batch.frames[0].as_ref().unwrap().offset, 1164);
        workers[0]
            .flush_batch(0, 0, &mut batch, Duration::from_millis(1))
            .unwrap();
        assert!(batch.plan.is_empty());
        workers[0]
            .flush_batch(0, 0, &mut batch, Duration::from_millis(10))
            .unwrap();
        assert_eq!(batch.len, 63);
        assert!(batch.plan.is_empty());
        assert_eq!(batch.frames[0].as_ref().unwrap().frame_id, (1 << 48) | 2);
        assert_eq!(batch.frames[0].as_ref().unwrap().offset, 0);
        assert_eq!(workers[0].sessions[0].tx_retained, 63);
        workers[0].sessions[0].batches[0] = Some(batch);
        workers[0].transmit[0].paths[0].lanes[0].blocked = false;
        let mut reassembly = reassembly::Lane::new(1, 9022).unwrap();
        let mut delivered = 0;
        let mut physical_bytes = 0;
        for _ in 0..32 {
            workers[0].flush_frames(Duration::from_millis(10)).unwrap();
            loop {
                let mut bytes = [0; BASE_UDP_PAYLOAD];
                let length = match rx.recv(&mut bytes) {
                    Ok((length, _)) => length,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(error) => panic!("{error}"),
                };
                let datagram = Datagram::decode(&bytes[..length]).unwrap();
                assert!(datagram.header().packet_number > last_packet);
                last_packet = datagram.header().packet_number;
                physical_bytes += (length + 48) as u64;
                assert_eq!(
                    reassembly.admit(&datagram, Duration::from_millis(10)),
                    Admission::Fresh
                );
                while let Some(frame) = reassembly.front(Duration::from_millis(10)) {
                    delivered += 1;
                    assert_eq!(frame.bytes, &[delivered as u8; MAX_FRAME_LEN]);
                    reassembly.finish_front();
                }
            }
            if workers[0].ready.is_empty() {
                break;
            }
        }
        assert_eq!(delivered, 63);
        assert_eq!(workers[0].free.len(), BUFFERS);
        assert_eq!(workers[0].sessions[0].tx_retained, 0);
        assert_eq!(workers[0].sessions[0].totals.expired_frames, 1);
        assert_eq!(
            workers[0].sessions[0].totals.tx_outer_bytes,
            physical_bytes + 1280
        );

        // A partial send can free an early slot while a later packet still names another.
        workers[0].transmit[0].paths[0].lanes[0].blocked = true;
        for index in 0..2 {
            let mut buffer = workers[0].free.pop().unwrap();
            buffer.session = 0;
            buffer.lane = 0;
            buffer.tag = 1;
            buffer.length = 1514;
            buffer.bytes[..1514].fill(index as u8);
            buffer.deadline = Duration::from_millis(100);
            buffer.tx_reserved = true;
            workers[0].sessions[0].tx_retained += 1;
            workers[0].sessions[0].tx_bytes += buffer.length;
            workers[0].frame(buffer, Duration::ZERO).unwrap();
        }
        workers[0].flush_frames(Duration::ZERO).unwrap();
        let mut batch = workers[0].sessions[0].batches[0].take().unwrap();
        batch
            .prepare(
                workers[0].sessions[0].tx[0].as_mut().unwrap(),
                BASE_UDP_PAYLOAD,
            )
            .unwrap();
        let tail = batch.plan.packet(2).unwrap();
        workers[0].submitted_batch(0, 0, &mut batch, 2);
        assert_eq!(batch.len, 1);
        assert_eq!(batch.frames[0].as_ref().unwrap().offset, 794);
        assert!(batch.plan.is_empty());
        batch
            .prepare(
                workers[0].sessions[0].tx[0].as_mut().unwrap(),
                BASE_UDP_PAYLOAD,
            )
            .unwrap();
        let unused = batch.plan.packet(0).unwrap().number;
        assert!(unused > tail.number);
        let charged = workers[0].sessions[0].totals.tx_outer_bytes;
        workers[0].submitted_batch(0, 0, &mut batch, 0);
        assert!(batch.plan.is_empty());
        assert_eq!(batch.frames[0].as_ref().unwrap().offset, 794);
        assert_eq!(workers[0].sessions[0].totals.tx_outer_bytes, charged);
        // A control transmission may overtake the discarded packet-number suffix.
        let control = workers[0].send_record(0, Record::Probe).unwrap().unwrap();
        assert!(control.0 > unused);
        let mut bytes = [0; BASE_UDP_PAYLOAD];
        let length = rx.recv(&mut bytes).unwrap().0;
        assert_eq!(
            Datagram::decode(&bytes[..length])
                .unwrap()
                .header()
                .packet_number,
            control.0
        );
        workers[0].sessions[0].batches[0] = Some(batch);
        workers[0].transmit[0].paths[0].lanes[0].blocked = false;
        workers[0].flush_frames(Duration::ZERO).unwrap();
        let length = rx.recv(&mut bytes).unwrap().0;
        let datagram = Datagram::decode(&bytes[..length]).unwrap();
        assert!(datagram.header().packet_number > control.0);
        assert!(matches!(datagram.records().next(), Some(Record::Data {
            offset: 794, bytes, ..
        }) if bytes == [1; 720]));
        assert!(workers[0].ready.is_empty());
        assert_eq!(workers[0].free.len(), BUFFERS);
        assert_eq!(workers[0].sessions[0].tx_retained, 0);

        // Quiesce two sessions with queued batches and a full foreign return ring.
        let (tx, rx) = crate::spsc::channel();
        workers[0].outgoing[1] = Some(tx);
        workers[1].incoming[0] = Some(rx);
        for _ in 0..crate::spsc::CAPACITY {
            assert!(
                workers[0]
                    .push(
                        1,
                        Event::ProbeFeedback {
                            session: 0,
                            tag: 0,
                            lane: 1,
                            packet: 0,
                            length: 0,
                        }
                    )
                    .is_ok()
            );
        }
        workers[0].transmit[0].paths[0].lanes[0].blocked = true;
        for (origin, session) in [(0, 0), (1, 0), (0, 1)] {
            let mut buffer = workers[origin].free.pop().unwrap();
            buffer.session = session;
            buffer.lane = 0;
            buffer.tag = 1 + session as u128 * 10;
            buffer.length = 60;
            buffer.deadline = Duration::from_millis(100);
            buffer.tx_reserved = true;
            workers[origin].sessions[session].tx_retained += 1;
            workers[origin].sessions[session].tx_bytes += buffer.length;
            workers[0].frame(buffer, Duration::ZERO).unwrap();
        }
        workers[0].flush_frames(Duration::ZERO).unwrap();
        assert_eq!(workers[0].ready.len(), 2);
        for session in 0..2 {
            let local = &mut workers[0].sessions[session];
            local.batches[0]
                .as_mut()
                .unwrap()
                .prepare(local.tx[0].as_mut().unwrap(), BASE_UDP_PAYLOAD)
                .unwrap();
            workers[0].command(
                session,
                Command {
                    generation: 1,
                    phase: Phase::Quiesce,
                    connection: None,
                },
                0,
            );
        }
        workers[0].progress_barriers(Duration::ZERO).unwrap();
        assert!(workers[0].sessions.iter().all(|s| s.command.is_some()));
        workers[0].flush_frames(Duration::ZERO).unwrap();
        workers[0].progress_barriers(Duration::ZERO).unwrap();
        assert!(
            workers[0]
                .sessions
                .iter()
                .all(|s| s.command.is_none() && s.tx_ready == 0)
        );
        assert!(workers[0].ready.is_empty());
        assert_eq!(workers[0].returns[1].len(), 1);
        for _ in 0..32 {
            workers[1].drain_queues(Duration::ZERO).unwrap();
            workers[0].flush_returns();
        }
        assert!(workers[0].returns[1].is_empty());
        assert_eq!(workers[1].sessions[0].tx_retained, 0);
        assert_eq!(workers[1].free.len(), BUFFERS);
        assert_eq!(workers[0].free.len(), BUFFERS);
    }

    fn test_workers() -> (Arc<Shared>, [Worker; 2]) {
        use crate::config::{Config, Daemon, Endpoint, Peer, Session as ConfigSession};
        use std::{
            net::Ipv6Addr,
            sync::atomic::{AtomicU8, AtomicU64},
        };
        let mut config = Config {
            daemon: Daemon::default(),
            endpoints: vec![Endpoint {
                name: "rx".into(),
                listen_address: Ipv6Addr::LOCALHOST,
                listen_port: 40000,
                scope_id: 0,
                bind_device: None,
            }],
            peers: vec![Peer {
                name: "peer".into(),
                endpoint: "rx".into(),
                address: Some(Ipv6Addr::LOCALHOST),
                peer_port: 40001,
                scope_id: 0,
                tx_lanes: 2,
                outer_payload_limit: BASE_UDP_PAYLOAD,
                path_group: None,
            }],
            sessions: vec![ConfigSession {
                name: "tap".into(),
                peer: "peer".into(),
                cookie: crate::UnquicUuid::from_bytes([1; 16]),
                tap_name: None,
                inner_mtu: 1500,
                mac: None,
                queue_count: 2,
            }],
        };
        let mut second = config.sessions[0].clone();
        second.name = "other".into();
        second.cookie = crate::UnquicUuid::from_bytes([2; 16]);
        config.sessions.push(second);
        let config = config.validate().unwrap();
        let shared = Arc::new(Shared {
            config,
            wakes: (0..2).map(|_| Arc::new(Wake::new().unwrap())).collect(),
            owners: (0..2)
                .map(|_| (0..65_536).map(|_| AtomicU8::new(0)).collect())
                .collect(),
            status: (0..2).map(|_| AtomicU8::new(0)).collect(),
            generations: (0..2).map(|_| AtomicU64::new(0)).collect(),
            counters: (0..2).map(|_| crate::counters::Counters::new()).collect(),
            session_counters: (0..4).map(|_| crate::counters::Counters::new()).collect(),
            peer_counters: (0..2).map(|_| crate::counters::Counters::new()).collect(),
            receive_counters: (0..4 * LANES)
                .map(|_| crate::counters::LaneCounters::new())
                .collect(),
            transmit_counters: (0..2)
                .map(|_| {
                    (0..2)
                        .map(|_| crate::counters::LaneCounters::new())
                        .collect()
                })
                .collect(),
            bound: vec![AtomicU64::new(0)],
            requests: std::sync::Mutex::new(crate::spsc::channel().0),
            address_pending: vec![std::sync::atomic::AtomicBool::new(false)],
            address_ready: (0..2).map(|_| AtomicU64::new(0)).collect(),
        });
        let workers: [Worker; 2] =
            std::array::from_fn(|id| Worker::new(id, Arc::clone(&shared), Control::new()).unwrap());

        (shared, workers)
    }

    #[test]
    #[ignore = "kernel descriptor test; run in a disposable container"]
    fn full_handoff_rings_return_buffers_and_finish_connection_barriers() {
        let (shared, mut workers) = test_workers();
        for from in 0..2 {
            let to = 1 - from;
            let (tx, rx) = crate::spsc::channel();
            workers[from].outgoing[to] = Some(tx);
            workers[to].incoming[from] = Some(rx);
            for _ in 0..crate::spsc::CAPACITY {
                assert!(
                    workers[from]
                        .push(
                            to,
                            Event::ProbeFeedback {
                                session: 0,
                                tag: 0,
                                lane: 1,
                                packet: 0,
                                length: 0,
                            }
                        )
                        .is_ok()
                );
            }
        }
        for _ in 0..BUFFERS {
            let mut buffer = workers[0].free.pop().unwrap();
            buffer.tx_reserved = true;
            workers[0].sessions[0].tx_retained += 1;
            workers[0].sessions[0].tx_bytes += buffer.length;
            workers[1].release(buffer);
        }
        assert!(workers[0].free.is_empty());
        assert_eq!(workers[1].returns[0].len(), BUFFERS);
        let params = wire::Params {
            port: 40000,
            max_frame_len: 9022,
            mandatory_features: 0,
            optional_features: 0,
        };
        let connection = Connection {
            role: handshake::Role::Initiator,
            initiator: Tag::new(1).unwrap(),
            responder: Tag::new(2).unwrap(),
            local: params,
            peer: wire::Params {
                port: 40001,
                ..params
            },
            max_frame_len: 9022,
        };
        let initial = Command {
            generation: 0,
            phase: Phase::Install,
            connection: Some(connection),
        };
        for worker in &mut workers {
            worker.command(0, initial, 0);
            worker.sessions[0].acknowledged = None;
        }
        let mut lane = workers[0].sessions[0].free.pop().unwrap();
        lane.reset(7, 9022).unwrap();
        workers[1].sessions[0].rx[2] = Some(lane);
        workers[1].sessions[0].rx_index[7] = 3;
        shared.owners[0][7].store(2, Ordering::Release);
        let replacement = Connection {
            initiator: Tag::new(3).unwrap(),
            ..connection
        };
        workers[0].controls[0]
            .barrier
            .begin(Some(replacement))
            .unwrap();
        for tick in 0..2000 {
            let now = Duration::from_millis(tick);
            for worker in &mut workers {
                worker.drain_queues(now).unwrap();
                worker.drain_packets(now).unwrap();
                worker.flush_returns();
                worker.progress_barriers(now).unwrap();
                worker.flush_frames(now).unwrap();
                worker.flush_taps(now).unwrap();
            }
            if !workers[0].controls[0].barrier.is_active()
                && workers[0].free.len() == BUFFERS
                && workers[0].sessions[0].free.len() == LANES
            {
                break;
            }
        }
        assert!(!workers[0].controls[0].barrier.is_active());
        assert_eq!(workers[0].free.len(), BUFFERS);
        assert_eq!(workers[0].sessions[0].tx_retained, 0);
        assert_eq!(workers[0].sessions[0].free.len(), LANES);
        assert_eq!(shared.owners[0][7].load(Ordering::Acquire), 0);
        for worker in &mut workers {
            assert_eq!(worker.sessions[0].connection, Some(replacement));
            let command = worker.sessions[0].installed.unwrap();
            let own_lane = worker.id;
            let mut bytes = [0; 128];
            assert_eq!(
                worker.sessions[0].tx[own_lane]
                    .as_mut()
                    .unwrap()
                    .record(Record::Probe, &mut bytes)
                    .unwrap()
                    .number,
                1
            );
            worker.command(0, command, 0);
            worker.command(
                0,
                Command {
                    phase: Phase::Quiesce,
                    ..command
                },
                0,
            );
            assert_eq!(worker.sessions[0].connection, Some(replacement));
            assert_eq!(
                worker.sessions[0].tx[own_lane]
                    .as_mut()
                    .unwrap()
                    .record(Record::Probe, &mut bytes)
                    .unwrap()
                    .number,
                2
            );
        }
        let worker = &mut workers[0];
        let generation = worker.sessions[0].installed.unwrap().generation;
        for (number, admission_count) in [(1, 1), (1, 2), (2, 3)] {
            let mut buffer = worker.free.pop().unwrap();
            buffer.rx_counted = false;
            buffer.length = wire::encode_datagram(
                Header {
                    cookie: crate::UnquicUuid::from_bytes([1; 16]),
                    receive_tag: replacement.local_tag().get(),
                    lane: 8,
                    record_area_len: 0,
                    packet_number: number,
                },
                &[Record::Data {
                    frame_id: (8_u64 << 48) | 1,
                    total_len: 60,
                    offset: 0,
                    bytes: &[5; 30],
                }],
                &mut buffer.bytes,
            )
            .unwrap();
            worker.received.push_back(buffer);
            worker.drain_packets(Duration::ZERO).unwrap();
            worker.flush_taps(Duration::ZERO).unwrap();
            worker.publish_counters();
            let lane = shared
                .receive_counters
                .iter()
                .filter_map(|c| c.snapshot())
                .find(|c| c.lane == 8)
                .unwrap();
            assert_eq!(lane.generation, generation);
            assert_eq!(
                lane.counters.rx_outer_bytes,
                admission_count * (68 + 30 + 48)
            );
            assert_eq!(lane.counters.reassembly_frames, 1);
            assert_eq!(lane.counters.reassembly_bytes, 60);
        }
        worker.flush_taps(reassembly::FRAME_LIFETIME).unwrap();
        worker.publish_counters();
        let lane = shared
            .receive_counters
            .iter()
            .filter_map(|c| c.snapshot())
            .find(|c| c.lane == 8)
            .unwrap();
        let totals = shared.session_counters[0].snapshot();
        assert_eq!(totals.rx_outer_bytes, lane.counters.rx_outer_bytes);
        assert_eq!(totals.duplicate_packets, 1);
        assert_eq!(totals.duplicate_pieces, 1);
        assert_eq!(totals.expired_frames, 1);
        assert_eq!(totals.reassembly_frames, 0);
        assert_eq!(totals.reassembly_bytes, 0);
        assert_eq!(lane.counters.expired_frames, 1);
        assert_eq!(
            shared.transmit_counters[0][0]
                .snapshot()
                .unwrap()
                .generation,
            generation
        );
        worker.command(
            0,
            Command {
                generation: generation + 1,
                phase: Phase::Quiesce,
                connection: None,
            },
            0,
        );
        worker.flush_taps(reassembly::FRAME_LIFETIME).unwrap();
        worker.publish_counters();
        assert!(
            shared
                .receive_counters
                .iter()
                .all(|c| c.snapshot().is_none())
        );
        assert_eq!(shared.session_counters[0].snapshot().expired_frames, 1);
        // One GRO aggregate crosses cookies and owners; invalid siblings cannot hide valid DATA.
        let connections = [
            replacement,
            Connection {
                initiator: Tag::new(11).unwrap(),
                ..replacement
            },
        ];
        for worker in &mut workers {
            for (session, connection) in connections.iter().enumerate() {
                worker.command(
                    session,
                    Command {
                        generation: generation + 2,
                        phase: Phase::Install,
                        connection: Some(*connection),
                    },
                    0,
                );
                worker.sessions[session].acknowledged = None;
            }
        }
        for (session, id, owner, slot) in [(0, 7, 0, 0), (0, 8, 1, 0), (1, 7, 1, 0)] {
            let mut lane = workers[0].sessions[session].free.pop().unwrap();
            lane.reset(id, 9022).unwrap();
            workers[owner].sessions[session].rx[slot] = Some(lane);
            workers[owner].sessions[session].rx_index[usize::from(id)] = slot as u8 + 1;
            shared.owners[session][usize::from(id)].store(owner as u8 + 1, Ordering::Release);
            workers[owner].install_rx_counters(session, slot);
        }
        let before: [u64; 2] = std::array::from_fn(|session| {
            workers
                .iter()
                .map(|w| w.sessions[session].totals.rx_outer_bytes)
                .sum()
        });
        let mut buffer = workers[0].free.pop().unwrap();
        buffer.length = 0;
        buffer.segment_size = 98;
        for (index, (cookie, lane, tag, packet)) in [
            (1, 7, 3, 1),
            (2, 7, 11, 1),
            (1, 7, 3, 2),
            (9, 7, 3, 1),
            (1, 8, 3, 1),
            (2, 7, 999, 2),
            (1, 7, 3, 2),
            (2, 7, 11, 2),
        ]
        .into_iter()
        .enumerate()
        {
            let short = index == 7;
            let start = buffer.length;
            let length = wire::encode_datagram(
                Header {
                    cookie: crate::UnquicUuid::from_bytes([cookie; 16]),
                    receive_tag: tag,
                    lane,
                    packet_number: packet,
                    record_area_len: 0,
                },
                &[Record::Data {
                    frame_id: (u64::from(lane) << 48) | if short { 2 } else { 1 },
                    total_len: if short { 14 } else { 60 },
                    offset: 0,
                    bytes: if short { &[5; 14] } else { &[5; 30] },
                }],
                &mut buffer.bytes[start..],
            )
            .unwrap();
            assert_eq!(length, if short { 82 } else { 98 });
            if index == 2 {
                buffer.bytes[start] ^= 1;
            }
            buffer.length += length;
        }
        let pointer = (&*buffer as *const Buffer) as usize;
        let original = buffer.bytes[..buffer.length].to_vec();
        workers[0].received.push_back(buffer);
        workers[0].drain_packets(Duration::ZERO).unwrap();
        for _ in 0..8 {
            for worker in &mut workers {
                worker.drain_queues(Duration::ZERO).unwrap();
                worker.drain_packets(Duration::ZERO).unwrap();
                worker.flush_taps(Duration::ZERO).unwrap();
                worker.flush_returns();
            }
        }
        let returned = workers[0]
            .free
            .iter()
            .find(|b| (&***b as *const Buffer) as usize == pointer)
            .unwrap();
        assert_eq!(&returned.bytes[..original.len()], &original);
        assert_eq!(workers[0].free.len(), BUFFERS);
        let after: [u64; 2] = std::array::from_fn(|session| {
            workers
                .iter()
                .map(|w| w.sessions[session].totals.rx_outer_bytes)
                .sum()
        });
        assert_eq!(after[0] - before[0], 3 * 146);
        assert_eq!(after[1] - before[1], 2 * 146 + 130);
        assert_eq!(
            workers
                .iter()
                .map(|w| w.counters.malformed_packets)
                .sum::<u64>(),
            1
        );
        assert_eq!(
            workers
                .iter()
                .map(|w| w.counters.unknown_cookie_packets)
                .sum::<u64>(),
            1
        );
        assert_eq!(
            workers[0].sessions[0].rx[0]
                .as_ref()
                .unwrap()
                .retained_frames(),
            1
        );
        assert_eq!(
            workers[1].sessions[0].rx[0]
                .as_ref()
                .unwrap()
                .retained_frames(),
            1
        );
        assert_eq!(
            workers[1].sessions[1].rx[0]
                .as_ref()
                .unwrap()
                .retained_frames(),
            2
        );
        assert_eq!(
            workers[1].sessions[1].rx_totals[0].rx_outer_bytes,
            146 + 130
        );
    }

    #[test]
    #[ignore = "kernel polling test; run in a disposable container"]
    fn notifications_timers_and_fd_interests_remain_ready_until_consumed() {
        let wake = Arc::new(Wake::new().unwrap());
        let mut reactor = Reactor::new(Arc::clone(&wake)).unwrap();
        let mut events = [libc::epoll_event { events: 0, u64: 0 }; 4];
        assert_eq!(reactor.wait(&mut events, 0).unwrap(), 0);
        for _ in 0..100 {
            wake.notify().unwrap();
        }
        assert_eq!(reactor.wait(&mut events, 0).unwrap(), 1);
        assert_eq!({ events[0].u64 }, WAKE_TOKEN);
        reactor.drain(WAKE_TOKEN).unwrap();
        assert_eq!(reactor.wait(&mut events, 0).unwrap(), 0);
        // A notification sent after draining remains visible to the next wait.
        wake.notify().unwrap();
        assert_eq!(reactor.wait(&mut events, 1000).unwrap(), 1);
        reactor.drain(WAKE_TOKEN).unwrap();
        let now = monotonic_now().unwrap();
        reactor.arm(Some(now + Duration::from_secs(2))).unwrap();
        let armed = reactor.deadline.unwrap();
        assert_eq!(armed, now + Duration::from_secs(2));
        reactor.arm(Some(now + Duration::from_secs(3))).unwrap();
        assert_eq!(reactor.deadline, Some(armed));
        reactor.arm(Some(now + Duration::from_secs(1))).unwrap();
        assert!(reactor.deadline.unwrap() < armed);
        reactor.arm(None).unwrap();
        assert_eq!(reactor.deadline, None);
        assert_eq!(reactor.wait(&mut events, 0).unwrap(), 0);
        reactor
            .arm(Some(monotonic_now().unwrap() + Duration::from_millis(1)))
            .unwrap();
        assert_eq!(reactor.wait(&mut events, 1000).unwrap(), 1);
        assert_eq!({ events[0].u64 }, TIMER_TOKEN);
        reactor.drain(TIMER_TOKEN).unwrap();
        assert_eq!(reactor.deadline, None);
        // A fired timer must be armed again even when the next deadline is later.
        reactor
            .arm(Some(monotonic_now().unwrap() + Duration::from_millis(1)))
            .unwrap();
        assert_eq!(reactor.wait(&mut events, 1000).unwrap(), 1);
        assert_eq!({ events[0].u64 }, TIMER_TOKEN);
        reactor.drain(TIMER_TOKEN).unwrap();
        reactor.arm(None).unwrap();
        assert_eq!(reactor.wait(&mut events, 0).unwrap(), 0);
        let extra = Wake::new().unwrap();
        reactor
            .add(extra.0.as_fd(), 2, libc::EPOLLIN as u32)
            .unwrap();
        reactor
            .modify(extra.0.as_fd(), 3, libc::EPOLLIN as u32)
            .unwrap();
        extra.notify().unwrap();
        assert_eq!(reactor.wait(&mut events, 0).unwrap(), 1);
        assert_eq!({ events[0].u64 }, 3);
        reactor.remove(extra.0.as_fd()).unwrap();
        assert_eq!(reactor.wait(&mut events, 0).unwrap(), 0);
        pin_to_allowed(usize::MAX).unwrap();
    }
}
