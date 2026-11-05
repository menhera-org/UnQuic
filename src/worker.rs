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
        if self.deadline == deadline {
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
    pack,
    reassembly::{self, Admission},
    runtime::{AddressRequest, BUFFERS, Control, LANES, Shared},
    socket::{BATCH_SIZE, ReceiveDatagram, ReceiveSocket, SendDatagram, TransmitSocket},
    spsc::{Consumer, Producer},
    tap::TapQueue,
    wire::{self, DATA_PREFIX_LEN, Datagram, Header, Record},
};
use std::{collections::VecDeque, net::SocketAddrV6, sync::atomic::Ordering};

const TAP_TOKEN: u64 = 1 << 32;
const TX_TOKEN: u64 = 2 << 32;
const RETRY: Duration = Duration::from_millis(1);
const RECEIVE_BYTES: usize = 65_536;
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

/// Pool entries move through queues by ownership, including on rejection.
pub(crate) struct Buffer {
    origin: usize,
    rx_counted: bool,
    tx_reserved: bool,
    offset: usize,
    segment_size: usize,
    session: usize,
    tag: u128,
    lane: usize,
    length: usize,
    deadline: Duration,
    prefixes: [[u8; DATA_PREFIX_LEN]; 8],
    lengths: [u16; 8],
    first: usize,
    count: usize,
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
    command: Option<Command>,
    acknowledged: Option<Command>,
    installed: Option<Command>,
    totals: Snapshot,
    tx_retained: usize,
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

struct Tx {
    socket: Option<TransmitSocket>,
    blocked: bool,
}

#[derive(Clone, Copy)]
pub(crate) struct AddressChange {
    request: AddressRequest,
    path: usize,
    generation: u64,
}

struct Path {
    address: Option<SocketAddrV6>,
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
    id: usize,
    shared: Arc<Shared>,
    control: Control,
    reactor: Reactor,
    pub outgoing: Vec<Option<Producer<Event>>>,
    pub incoming: Vec<Option<Consumer<Event>>>,
    sessions: Vec<Session>,
    controls: Vec<SessionControl>,
    receive: Vec<Option<ReceiveSocket>>,
    transmit: Vec<PeerPaths>,
    pub requests: Option<Consumer<AddressRequest>>,
    peer_controls: Vec<PeerControl>,
    free: Vec<Box<Buffer>>,
    returns: Vec<Vec<Box<Buffer>>>,
    pending: VecDeque<Box<Buffer>>,
    notifications: u64,
    counters: Snapshot,
    setup_due: Duration,
    retry_due: Option<Duration>,
}

impl Worker {
    pub fn storage_bytes(config: &crate::config::Config, workers: usize) -> Option<usize> {
        let mut per_worker = size_of::<Self>()
            .checked_add(BUFFERS.checked_mul(size_of::<Buffer>() + size_of::<Box<Buffer>>())?)?
            .checked_add(workers.checked_mul(
                size_of::<Option<Producer<Event>>>() + size_of::<Option<Consumer<Event>>>(),
            )?)?
            .checked_add(
                workers
                    .checked_mul(BUFFERS)?
                    .checked_mul(2 * size_of::<Box<Buffer>>())?,
            )?
            .checked_add(workers.checked_mul(size_of::<Vec<Box<Buffer>>>())?)?
            .checked_add(
                config
                    .endpoints
                    .len()
                    .checked_mul(size_of::<Option<ReceiveSocket>>())?,
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
        for session in &config.sessions {
            let peer = config.peers.iter().find(|p| p.name == session.peer)?;
            per_worker = per_worker
                .checked_add(size_of::<Session>() + 65_536 + 8192 + LANES * size_of::<Snapshot>())?
                .checked_add(LANES.checked_mul(size_of::<Option<Box<reassembly::Lane>>>())?)?
                .checked_add(
                    peer.tx_lanes
                        .checked_mul(size_of::<Option<pack::Lane>>() + size_of::<Snapshot>())?,
                )?;
        }
        per_worker
            .checked_mul(workers)?
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
                command: None,
                acknowledged: None,
                installed: None,
                totals: Snapshot::default(),
                tx_retained: 0,
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
                    offset: 0,
                    segment_size: 0,
                    session: 0,
                    tag: 0,
                    lane: 0,
                    length: 0,
                    deadline: Duration::ZERO,
                    prefixes: [[0; DATA_PREFIX_LEN]; 8],
                    lengths: [0; 8],
                    first: 0,
                    count: 0,
                    bytes: [0; RECEIVE_BYTES],
                })
            })
            .collect();
        Ok(Self {
            id,
            control,
            reactor: Reactor::new(Arc::clone(&shared.wakes[id]))?,
            outgoing: (0..count).map(|_| None).collect(),
            incoming: (0..count).map(|_| None).collect(),
            receive: (0..config.endpoints.len()).map(|_| None).collect(),
            transmit: config
                .peers
                .iter()
                .map(|peer| PeerPaths {
                    paths: std::array::from_fn(|slot| Path {
                        address: if slot == 0 { peer.socket_addr() } else { None },
                        lanes: (0..peer.tx_lanes)
                            .map(|_| Tx {
                                socket: None,
                                blocked: false,
                            })
                            .collect(),
                    }),
                    generation: 0,
                    preparing: None,
                    totals: Snapshot::default(),
                })
                .collect(),
            requests: None,
            peer_controls: (0..if id == 0 { config.peers.len() } else { 0 })
                .map(|_| PeerControl::default())
                .collect(),
            sessions,
            controls,
            free,
            returns: (0..count).map(|_| Vec::with_capacity(BUFFERS)).collect(),
            pending: VecDeque::with_capacity(count * BUFFERS),
            notifications: 0,
            counters: Snapshot::default(),
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
        while !self.control.quitting() {
            let now = monotonic_now()?;
            let mut more = self.drain_queues(now)?;
            self.flush_returns();
            if now >= self.setup_due {
                self.setup(now)?;
            }
            self.addresses(now)?;
            self.progress_barriers(now)?;
            self.timers(now)?;
            self.flush_frames(now)?;
            self.flush_taps(now)?;
            self.flush_notifications()?;
            more |= self.pending.iter().any(|b| {
                !self.transmit[self.shared.config.peer_for_session(b.session).unwrap()].paths
                    [self.sessions[b.session].path]
                    .lanes[b.lane]
                    .blocked
            });
            self.publish_counters();
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
            for event in &events[..count] {
                let token = event.u64;
                match token >> 32 {
                    1 => self.read_tap(token as u32 as usize, now)?,
                    2 => {
                        let peer = ((token as u32) >> 7) as usize;
                        let path = ((token as u32) >> 6 & 1) as usize;
                        let lane = token as usize & 63;
                        let tx = &mut self.transmit[peer].paths[path].lanes[lane];
                        if tx.blocked {
                            self.reactor.remove(tx.socket.as_ref().unwrap().as_fd())?;
                            tx.blocked = false;
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
            if buffer.tx_reserved {
                self.sessions[buffer.session].tx_retained -= 1;
                buffer.tx_reserved = false;
            }
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
            if tx.blocked {
                self.reactor.remove(tx.socket.as_ref().unwrap().as_fd())?;
            }
            tx.socket = None;
            tx.blocked = false;
        }
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

    fn setup(&mut self, now: Duration) -> io::Result<()> {
        self.setup_due = now + Duration::from_secs(1);
        let config = self.shared.config.config();
        for (index, endpoint) in config.endpoints.iter().enumerate() {
            if self.receive[index].is_none() {
                match ReceiveSocket::bind(endpoint.socket_addr(), endpoint.bind_device.as_deref()) {
                    Ok(mut socket) => {
                        if socket.enable_gro().is_err() {
                            self.counters.socket_errors =
                                self.counters.socket_errors.wrapping_add(1);
                        }
                        self.reactor
                            .add(socket.as_fd(), index as u64 + 2, libc::EPOLLIN as u32)?;
                        self.receive[index] = Some(socket);
                        self.shared.bound[index].fetch_or(1_u64 << self.id, Ordering::Release);
                    }
                    Err(_) => self.counters.socket_errors += 1,
                }
            }
        }
        for index in 0..config.peers.len() {
            let endpoint = self.shared.config.endpoint_for_peer(index).unwrap();
            let Some(receiver) = &self.receive[endpoint] else {
                continue;
            };
            let paths = &mut self.transmit[index];
            for path in &mut paths.paths {
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
                            Ok(socket) => {
                                let port = socket.local_addr()?.port();
                                if config.endpoints.iter().any(|e| e.listen_port == port) {
                                    continue;
                                }
                                tx.socket = Some(socket);
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
                    Event::Packet(buffer) => self.packet(buffer, now)?,
                    Event::Frame(buffer) => self.frame(buffer, now)?,
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
                            self.packet(buffer, now)?;
                        } else {
                            self.return_lane(session, lane);
                            if buffer.advance() {
                                self.packet(buffer, now)?;
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
                        packet,
                        length,
                    } => {
                        if self.matches(session, tag) {
                            self.feedback(session, packet, length, now);
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

    fn read_socket(&mut self, endpoint: usize, now: Duration) -> io::Result<()> {
        let mut buffers: [Option<Box<Buffer>>; BATCH_SIZE] =
            std::array::from_fn(|_| self.free.pop());
        let available = buffers.iter().take_while(|b| b.is_some()).count();
        if available == 0 {
            return Ok(());
        }
        let mut received: [ReceiveDatagram<'_>; BATCH_SIZE] =
            std::array::from_fn(|_| ReceiveDatagram::new(&mut []));
        for (output, buffer) in received.iter_mut().zip(&mut buffers).take(available) {
            output.buffer = &mut buffer.as_mut().unwrap().bytes;
        }
        let result = self.receive[endpoint]
            .as_ref()
            .unwrap()
            .recv_batch(&mut received[..available]);
        let lengths: [usize; BATCH_SIZE] = std::array::from_fn(|i| received[i].length);
        let truncated: [bool; BATCH_SIZE] = std::array::from_fn(|i| received[i].truncated);
        let segments: [usize; BATCH_SIZE] = std::array::from_fn(|i| received[i].segment_size);
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
                self.packet(buffer, now)?;
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

    fn packet(&mut self, mut buffer: Box<Buffer>, now: Duration) -> io::Result<()> {
        loop {
            let Some(mut next) = self.packet_one(buffer, now)? else {
                return Ok(());
            };
            if !next.advance() {
                self.release(next);
                return Ok(());
            }
            buffer = next;
        }
    }

    fn packet_one(
        &mut self,
        mut buffer: Box<Buffer>,
        now: Duration,
    ) -> io::Result<Option<Box<Buffer>>> {
        let end = if buffer.segment_size == 0 {
            buffer.length
        } else {
            (buffer.offset + buffer.segment_size).min(buffer.length)
        };
        let length = end - buffer.offset;
        if length > crate::socket::MAX_UDP_PAYLOAD {
            self.counters.malformed_packets = self.counters.malformed_packets.wrapping_add(1);
            return Ok(Some(buffer));
        }
        let Ok(datagram) = Datagram::decode(&buffer.bytes[buffer.offset..end]) else {
            self.counters.malformed_packets = self.counters.malformed_packets.wrapping_add(1);
            return Ok(Some(buffer));
        };
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
            let owner = buffer.origin % self.shared.config.config().sessions[session].queue_count;
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
        match local.rx[slot].as_mut().unwrap().admit(&datagram, now) {
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
                            lane: 1,
                            packet_number,
                            payload_len,
                        } => {
                            // Probe history belongs to the control owner, which also owns TX lane 1.
                            if self.id == 0 {
                                self.feedback(session, packet_number, payload_len, now);
                            } else {
                                // Carry the exact proof; arbitrary valid DATA cannot refresh liveness.
                                let _ = self.push(
                                    0,
                                    Event::ProbeFeedback {
                                        session,
                                        tag: header.receive_tag,
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
                self.counters.queue_drops = self.counters.queue_drops.wrapping_add(1);
                self.sessions[session].totals.queue_drops =
                    self.sessions[session].totals.queue_drops.wrapping_add(1);
                self.sessions[session].rx_totals[slot].queue_drops = self.sessions[session]
                    .rx_totals[slot]
                    .queue_drops
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
        self.refresh_rx_counters(session, slot);
        Ok(Some(buffer))
    }

    fn read_tap(&mut self, session: usize, now: Duration) -> io::Result<()> {
        for _ in 0..BATCH_SIZE {
            let Some(mut buffer) = self.free.pop() else {
                break;
            };
            let result = self.sessions[session]
                .tap
                .as_mut()
                .unwrap()
                .read_frame(&mut buffer.bytes[..MAX_FRAME_LEN + 1]);
            match result {
                Ok(length) => {
                    buffer.length = length;
                    buffer.session = session;
                    let Some(connection) = self.sessions[session].connection else {
                        self.release(buffer);
                        continue;
                    };
                    let local = &mut self.sessions[session];
                    if local.tx_retained
                        >= frame_quota(self.shared.config.config().sessions[session].queue_count)
                    {
                        self.counters.queue_drops = self.counters.queue_drops.wrapping_add(1);
                        local.totals.queue_drops = local.totals.queue_drops.wrapping_add(1);
                        self.release(buffer);
                        continue;
                    }
                    local.tx_retained += 1;
                    buffer.tx_reserved = true;
                    buffer.tag = connection.local_tag().get();
                    buffer.lane = pack::flow_hash(&buffer.bytes[..length]) as usize
                        % self.sessions[session].tx.len();
                    buffer.deadline = now + reassembly::FRAME_LIFETIME;
                    let owner = buffer.lane % self.shared.workers();
                    if owner == self.id {
                        self.frame(buffer, now)?;
                    } else if let Err(Event::Frame(buffer)) = self.push(owner, Event::Frame(buffer))
                    {
                        self.counters.queue_drops = self.counters.queue_drops.wrapping_add(1);
                        self.sessions[session].totals.queue_drops =
                            self.sessions[session].totals.queue_drops.wrapping_add(1);
                        self.release(buffer);
                    }
                }
                Err(error) => {
                    self.release(buffer);
                    if !transient(&error) {
                        self.counters.tap_errors = self.counters.tap_errors.wrapping_add(1);
                        self.sessions[session].totals.tap_errors =
                            self.sessions[session].totals.tap_errors.wrapping_add(1);
                        let local = &mut self.sessions[session];
                        self.reactor.remove(local.tap.as_ref().unwrap().as_fd())?;
                        local.tap_registered = false;
                        local.tap_retry = now + Duration::from_secs(1);
                    }
                    break;
                }
            }
        }
        Ok(())
    }

    fn frame(&mut self, mut buffer: Box<Buffer>, now: Duration) -> io::Result<()> {
        if !self.matches(buffer.session, buffer.tag) {
            self.release(buffer);
            return Ok(());
        }
        if now >= buffer.deadline {
            self.counters.expired_frames = self.counters.expired_frames.wrapping_add(1);
            let local = &mut self.sessions[buffer.session];
            local.totals.expired_frames = local.totals.expired_frames.wrapping_add(1);
            local.tx_totals[buffer.lane].expired_frames =
                local.tx_totals[buffer.lane].expired_frames.wrapping_add(1);
            local.tx_dirty |= 1_u64 << buffer.lane;
            self.release(buffer);
            return Ok(());
        }
        let Some(lane) = &mut self.sessions[buffer.session].tx[buffer.lane] else {
            self.release(buffer);
            return Ok(());
        };
        let Ok(mut frame) = lane.frame(&buffer.bytes[..buffer.length]) else {
            self.release(buffer);
            return Ok(());
        };
        buffer.first = 0;
        buffer.count = 0;
        for prefix in &mut buffer.prefixes {
            match frame.encode_next_split(prefix) {
                Ok(Some(piece)) => {
                    buffer.lengths[buffer.count] = piece.bytes.len() as u16;
                    buffer.count += 1;
                }
                Ok(None) => break,
                Err(_) => {
                    self.release(buffer);
                    return Ok(());
                }
            }
        }
        self.pending.push_back(buffer);
        Ok(())
    }

    fn flush_frames(&mut self, now: Duration) -> io::Result<()> {
        for _ in 0..self.pending.len().min(BATCH_SIZE) {
            let mut buffer = self.pending.pop_front().unwrap();
            if !self.matches(buffer.session, buffer.tag) {
                self.release(buffer);
                continue;
            }
            if now >= buffer.deadline {
                self.counters.expired_frames = self.counters.expired_frames.wrapping_add(1);
                let local = &mut self.sessions[buffer.session];
                local.totals.expired_frames = local.totals.expired_frames.wrapping_add(1);
                local.tx_totals[buffer.lane].expired_frames =
                    local.tx_totals[buffer.lane].expired_frames.wrapping_add(1);
                local.tx_dirty |= 1_u64 << buffer.lane;
                self.release(buffer);
                continue;
            }
            let peer = self.shared.config.peer_for_session(buffer.session).unwrap();
            let path = self.sessions[buffer.session].path;
            let tx = &mut self.transmit[peer].paths[path].lanes[buffer.lane];
            let Some(socket) = &tx.socket else {
                self.release(buffer);
                continue;
            };
            if tx.blocked {
                self.pending.push_back(buffer);
                continue;
            }
            let mut slices = [[&[][..]; 2]; 8];
            let mut offset = 0;
            for (index, parts) in slices.iter_mut().enumerate().take(buffer.count) {
                let length = usize::from(buffer.lengths[index]);
                *parts = [
                    &buffer.prefixes[index],
                    &buffer.bytes[offset..offset + length],
                ];
                offset += length;
            }
            let parts: [SendDatagram<'_>; 8] = std::array::from_fn(|index| SendDatagram {
                parts: &slices[index],
            });
            match socket.send_batch(&parts[buffer.first..buffer.count], BASE_UDP_PAYLOAD) {
                Ok(sent) => {
                    let bytes = parts[buffer.first..buffer.first + sent]
                        .iter()
                        .map(|part| {
                            (part.parts.iter().map(|bytes| bytes.len()).sum::<usize>() + 48) as u64
                        })
                        .sum::<u64>();
                    self.counters.tx_outer_bytes = self.counters.tx_outer_bytes.wrapping_add(bytes);
                    let local = &mut self.sessions[buffer.session];
                    local.totals.tx_outer_bytes = local.totals.tx_outer_bytes.wrapping_add(bytes);
                    local.tx_totals[buffer.lane].tx_outer_bytes = local.tx_totals[buffer.lane]
                        .tx_outer_bytes
                        .wrapping_add(bytes);
                    local.tx_dirty |= 1_u64 << buffer.lane;
                    buffer.first += sent;
                    if buffer.first == buffer.count {
                        self.release(buffer);
                    } else {
                        self.pending.push_back(buffer);
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    let token =
                        TX_TOKEN | ((peer as u64) << 7) | ((path as u64) << 6) | buffer.lane as u64;
                    self.reactor
                        .add(socket.as_fd(), token, libc::EPOLLOUT as u32)?;
                    tx.blocked = true;
                    self.pending.push_back(buffer);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                    self.pending.push_back(buffer)
                }
                Err(_) => {
                    self.counters.socket_errors = self.counters.socket_errors.wrapping_add(1);
                    let local = &mut self.sessions[buffer.session];
                    local.totals.socket_errors = local.totals.socket_errors.wrapping_add(1);
                    local.tx_totals[buffer.lane].socket_errors =
                        local.tx_totals[buffer.lane].socket_errors.wrapping_add(1);
                    local.tx_dirty |= 1_u64 << buffer.lane;
                    self.release(buffer);
                }
            }
        }
        Ok(())
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
                if let Some(tap) = &mut local.tap {
                    for _ in 0..BATCH_SIZE {
                        let Some(frame) = lane.front(now) else { break };
                        let length = frame.bytes.len();
                        match tap.write_frame(frame.bytes) {
                            Ok(()) => {
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
                                writable = true;
                                break;
                            }
                            Err(_) => {
                                self.counters.tap_errors = self.counters.tap_errors.wrapping_add(1);
                                local.totals.tap_errors = local.totals.tap_errors.wrapping_add(1);
                                local.rx_totals[slot].tap_errors =
                                    local.rx_totals[slot].tap_errors.wrapping_add(1);
                                local.rx_dirty |= 1_u64 << slot;
                            }
                        }
                        lane.finish_front();
                    }
                }
                self.refresh_rx_counters(session, slot);
            }
            let local = &mut self.sessions[session];
            local.totals.reassembly_frames =
                local.rx_totals.iter().map(|s| s.reassembly_frames).sum();
            local.totals.reassembly_bytes =
                local.rx_totals.iter().map(|s| s.reassembly_bytes).sum();
            if local.tap_registered && local.tap_writable != writable {
                self.reactor.modify(
                    local.tap.as_ref().unwrap().as_fd(),
                    TAP_TOKEN | session as u64,
                    (libc::EPOLLIN | if writable { libc::EPOLLOUT } else { 0 }) as u32,
                )?;
                local.tap_writable = writable;
            }
        }
        Ok(())
    }

    fn feedback(&mut self, session: usize, packet: u64, length: u16, now: Duration) {
        if self.controls[session].probe == Some((packet, length)) {
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
        let mut bytes = [0; BASE_UDP_PAYLOAD];
        let packet = if record.is_handshake() {
            let header = Header {
                cookie: self.shared.config.config().sessions[session].cookie,
                receive_tag: 0,
                lane: 0,
                record_area_len: 0,
                packet_number: 0,
            };
            wire::encode_datagram(header, &[record], &mut bytes)
                .ok()
                .map(|length| pack::Packet { number: 0, length })
        } else {
            self.sessions[session].tx[0]
                .as_mut()
                .and_then(|lane| lane.record(record, &mut bytes).ok())
        };
        let Some(packet) = packet else {
            return Ok(None);
        };
        match socket.send(&bytes[..packet.length], BASE_UDP_PAYLOAD) {
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
                local.connection = command.connection;
                local.path = path;
                let cookie = self.shared.config.config().sessions[session].cookie;
                for (index, lane) in local.tx.iter_mut().enumerate() {
                    *lane = command
                        .connection
                        .filter(|_| index % self.shared.workers() == self.id)
                        .map(|c| pack::Lane::new(cookie, c, index as u16 + 1).unwrap());
                    if index % self.shared.workers() == self.id {
                        local.tx_totals[index] = Snapshot {
                            path_mtu_bytes: if lane.is_some() { 1280 } else { 0 },
                            path_mtu_state: u64::from(lane.is_some()),
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
                local.totals.path_mtu_bytes = if command.connection.is_some() {
                    1280
                } else {
                    0
                };
                local.totals.path_mtu_state = u64::from(command.connection.is_some());
                local.installed = Some(command);
                local.acknowledged = Some(command);
            }
        }
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
        let mut due = self.setup_due;
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
            for lane in session.rx.iter().flatten() {
                if let Some(deadline) = lane.deadline() {
                    due = due.min(deadline);
                }
            }
        }
        for frame in &self.pending {
            due = due.min(frame.deadline);
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
    fn full_handoff_rings_return_buffers_and_finish_connection_barriers() {
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
        let mut workers: [Worker; 2] =
            std::array::from_fn(|id| Worker::new(id, Arc::clone(&shared), Control::new()).unwrap());
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
                    lane: 7,
                    record_area_len: 0,
                    packet_number: number,
                },
                &[Record::Data {
                    frame_id: (7_u64 << 48) | 1,
                    total_len: 60,
                    offset: 0,
                    bytes: &[5; 30],
                }],
                &mut buffer.bytes,
            )
            .unwrap();
            worker.packet(buffer, Duration::ZERO).unwrap();
            worker.flush_taps(Duration::ZERO).unwrap();
            worker.publish_counters();
            let lane = shared
                .receive_counters
                .iter()
                .filter_map(|c| c.snapshot())
                .find(|c| c.lane == 7)
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
            .find(|c| c.lane == 7)
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
        workers[0].packet(buffer, Duration::ZERO).unwrap();
        for _ in 0..8 {
            for worker in &mut workers {
                worker.drain_queues(Duration::ZERO).unwrap();
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
