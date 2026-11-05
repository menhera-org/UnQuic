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
    runtime::{BUFFERS, Control, LANES, Shared},
    socket::{BATCH_SIZE, ReceiveDatagram, ReceiveSocket, SendDatagram, TransmitSocket},
    spsc::{Consumer, Producer},
    tap::TapQueue,
    wire::{self, DATA_PREFIX_LEN, Datagram, Header, Record},
};
use std::{collections::VecDeque, sync::atomic::Ordering};

const TAP_TOKEN: u64 = 1 << 32;
const TX_TOKEN: u64 = 2 << 32;
const RETRY: Duration = Duration::from_millis(1);

/// Pool entries move through queues by ownership, including on rejection.
pub(crate) struct Buffer {
    origin: usize,
    session: usize,
    tag: u128,
    lane: usize,
    length: usize,
    deadline: Duration,
    prefixes: [[u8; DATA_PREFIX_LEN]; 8],
    lengths: [u16; 8],
    first: usize,
    count: usize,
    bytes: [u8; MAX_FRAME_LEN + 1],
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
    },
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
}

struct SessionControl {
    state: handshake::Session,
    barrier: Barrier,
    effects: Effects,
    carrier: bool,
    retry: Duration,
    probe_due: Duration,
    probe: Option<(u64, u16)>,
}

struct Tx {
    socket: Option<TransmitSocket>,
    blocked: bool,
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
    transmit: Vec<Vec<Tx>>,
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
                .checked_add(size_of::<Vec<Tx>>())?
                .checked_add(peer.tx_lanes.checked_mul(size_of::<Tx>())?)?;
        }
        for session in &config.sessions {
            let peer = config.peers.iter().find(|p| p.name == session.peer)?;
            per_worker = per_worker
                .checked_add(size_of::<Session>() + 65_536 + 8192)?
                .checked_add(LANES.checked_mul(size_of::<Option<Box<reassembly::Lane>>>())?)?
                .checked_add(peer.tx_lanes.checked_mul(size_of::<Option<pack::Lane>>())?)?;
        }
        per_worker
            .checked_mul(workers)?
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
                });
            }
            sessions.push(Session {
                tap: None,
                tap_registered: false,
                tap_writable: false,
                tap_retry: Duration::ZERO,
                connection: None,
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
            });
        }
        let free = (0..BUFFERS)
            .map(|_| {
                Box::new(Buffer {
                    origin: id,
                    session: 0,
                    tag: 0,
                    lane: 0,
                    length: 0,
                    deadline: Duration::ZERO,
                    prefixes: [[0; DATA_PREFIX_LEN]; 8],
                    lengths: [0; 8],
                    first: 0,
                    count: 0,
                    bytes: [0; MAX_FRAME_LEN + 1],
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
                .map(|p| {
                    (0..p.tx_lanes)
                        .map(|_| Tx {
                            socket: None,
                            blocked: false,
                        })
                        .collect()
                })
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
            self.counters.affinity_failures += 1;
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
            self.progress_barriers(now)?;
            self.timers(now)?;
            self.flush_frames(now)?;
            self.flush_taps(now)?;
            self.flush_notifications()?;
            more |= self.pending.iter().any(|b| {
                !self.transmit[self.shared.config.peer_for_session(b.session).unwrap()][b.lane]
                    .blocked
            });
            self.shared.counters[self.id].publish(&self.counters);
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
                        let peer = ((token as u32) >> 6) as usize;
                        let lane = token as usize & 63;
                        let tx = &mut self.transmit[peer][lane];
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
        Ok(())
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

    fn release(&mut self, buffer: Box<Buffer>) {
        let origin = buffer.origin;
        if origin == self.id {
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
    }

    fn setup(&mut self, now: Duration) -> io::Result<()> {
        self.setup_due = now + Duration::from_secs(1);
        let config = self.shared.config.config();
        for (index, endpoint) in config.endpoints.iter().enumerate() {
            if self.receive[index].is_none() {
                match ReceiveSocket::bind(endpoint.socket_addr(), endpoint.bind_device.as_deref()) {
                    Ok(socket) => {
                        self.reactor
                            .add(socket.as_fd(), index as u64 + 2, libc::EPOLLIN as u32)?;
                        self.receive[index] = Some(socket);
                    }
                    Err(_) => self.counters.socket_errors += 1,
                }
            }
        }
        if self.receive.iter().all(Option::is_some) {
            self.shared
                .bound
                .fetch_or(1_u64 << self.id, Ordering::Release);
        }
        for (index, peer) in config.peers.iter().enumerate() {
            let Some(address) = peer.socket_addr() else {
                continue;
            };
            let endpoint = self.shared.config.endpoint_for_peer(index).unwrap();
            let Some(receiver) = &self.receive[endpoint] else {
                continue;
            };
            for (lane, tx) in self.transmit[index].iter_mut().enumerate() {
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
                            self.counters.socket_errors += 1;
                            break;
                        }
                    }
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
                    Event::Free(buffer) => self.free.push(buffer),
                    Event::Register { buffer, lane } => {
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
                            self.packet(buffer, now)?;
                        } else {
                            self.return_lane(session, lane);
                            self.release(buffer);
                        }
                    }
                    Event::LaneFree { session, lane } => self.sessions[session].free.push(lane),
                    Event::Barrier { session, command } => self.command(session, command),
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
            output.buffer = &mut buffer.as_mut().unwrap().bytes[..BASE_UDP_PAYLOAD];
        }
        let result = self.receive[endpoint]
            .as_ref()
            .unwrap()
            .recv_batch(&mut received[..available]);
        let lengths: [usize; BATCH_SIZE] = std::array::from_fn(|i| received[i].length);
        let truncated: [bool; BATCH_SIZE] = std::array::from_fn(|i| received[i].truncated);
        let count = match result {
            Ok(count) => count,
            Err(error) => {
                if !transient(&error) {
                    self.counters.socket_errors += 1;
                }
                0
            }
        };
        for (index, buffer) in buffers.iter_mut().enumerate().take(available) {
            let mut buffer = buffer.take().unwrap();
            if index < count && !truncated[index] {
                buffer.length = lengths[index];
                buffer.deadline = now;
                self.counters.rx_outer_bytes += (buffer.length + 48) as u64;
                self.packet(buffer, now)?;
            } else {
                if index < count {
                    self.counters.malformed_packets += 1;
                }
                self.release(buffer);
            }
        }
        Ok(())
    }

    fn packet(&mut self, mut buffer: Box<Buffer>, now: Duration) -> io::Result<()> {
        let Ok(datagram) = Datagram::decode(&buffer.bytes[..buffer.length]) else {
            self.counters.malformed_packets += 1;
            self.release(buffer);
            return Ok(());
        };
        let header = datagram.header();
        let Some(session) = self.shared.config.session_for_cookie(header.cookie) else {
            self.counters.unknown_cookie_packets += 1;
            self.release(buffer);
            return Ok(());
        };
        buffer.session = session;
        buffer.tag = header.receive_tag;
        if header.lane == 0 {
            if self.id != 0 {
                if let Err(Event::Packet(buffer)) = self.push(0, Event::Packet(buffer)) {
                    self.release(buffer);
                }
                return Ok(());
            }
            if !self.controls[session].barrier.is_active() && self.shared.all_bound() {
                let record = datagram.records().next().unwrap();
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
            self.release(buffer);
            return Ok(());
        }
        if !self.matches(session, header.receive_tag) {
            self.release(buffer);
            return Ok(());
        }
        let lane_id = usize::from(header.lane);
        let owner = self.shared.owners[session][lane_id].load(Ordering::Acquire);
        if owner == 255 {
            self.release(buffer);
            return Ok(());
        }
        if owner == 0 {
            if self.id != 0 {
                if let Err(Event::Packet(buffer)) = self.push(0, Event::Packet(buffer)) {
                    self.release(buffer);
                }
                return Ok(());
            }
            let Some(mut lane) = self.sessions[session].free.pop() else {
                self.counters.queue_drops += 1;
                self.release(buffer);
                return Ok(());
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
                    self.counters.queue_drops += 1;
                    self.release(buffer);
                }
                return Ok(());
            }
            let slot = self.sessions[session]
                .rx
                .iter()
                .position(Option::is_none)
                .unwrap();
            self.sessions[session].rx_index[lane_id] = slot as u8 + 1;
            self.sessions[session].rx[slot] = Some(lane);
        } else if usize::from(owner - 1) != self.id {
            if let Err(Event::Packet(buffer)) =
                self.push(usize::from(owner - 1), Event::Packet(buffer))
            {
                self.counters.queue_drops += 1;
                self.release(buffer);
            }
            return Ok(());
        }
        let slot = self.sessions[session].rx_index[lane_id];
        if slot == 0 {
            self.release(buffer);
            return Ok(());
        }
        let lane = self.sessions[session].rx[usize::from(slot - 1)]
            .as_mut()
            .unwrap();
        match lane.admit(&datagram, now) {
            Admission::Fresh => {
                for record in datagram.records() {
                    match record {
                        Record::Probe => {
                            let event = Event::Reply {
                                session,
                                tag: header.receive_tag,
                                lane: header.lane,
                                packet: header.packet_number,
                                length: buffer.length as u16,
                            };
                            if self.id == 0 {
                                self.send_record(
                                    session,
                                    Record::ProbeReply {
                                        lane: header.lane,
                                        packet_number: header.packet_number,
                                        payload_len: buffer.length as u16,
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
            Admission::Duplicate => self.counters.duplicate_packets += 1,
            Admission::Pressure => self.counters.queue_drops += 1,
            Admission::Invalid => self.counters.malformed_packets += 1,
        }
        self.release(buffer);
        Ok(())
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
                .read_frame(&mut buffer.bytes);
            match result {
                Ok(length) => {
                    buffer.length = length;
                    buffer.session = session;
                    let Some(connection) = self.sessions[session].connection else {
                        self.release(buffer);
                        continue;
                    };
                    buffer.tag = connection.local_tag().get();
                    buffer.lane = pack::flow_hash(&buffer.bytes[..length]) as usize
                        % self.sessions[session].tx.len();
                    buffer.deadline = now + reassembly::FRAME_LIFETIME;
                    let owner = buffer.lane % self.shared.workers();
                    if owner == self.id {
                        self.frame(buffer, now)?;
                    } else if let Err(Event::Frame(buffer)) = self.push(owner, Event::Frame(buffer))
                    {
                        self.counters.queue_drops += 1;
                        self.release(buffer);
                    }
                }
                Err(error) => {
                    self.release(buffer);
                    if !transient(&error) {
                        self.counters.tap_errors += 1;
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
        if !self.matches(buffer.session, buffer.tag) || now >= buffer.deadline {
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
            if !self.matches(buffer.session, buffer.tag) || now >= buffer.deadline {
                self.counters.expired_frames += 1;
                self.release(buffer);
                continue;
            }
            let peer = self.shared.config.peer_for_session(buffer.session).unwrap();
            let tx = &mut self.transmit[peer][buffer.lane];
            let Some(socket) = &tx.socket else {
                self.release(buffer);
                continue;
            };
            if tx.blocked {
                self.pending.push_back(buffer);
                continue;
            }
            let mut parts = [SendDatagram {
                prefix: &[],
                payload: &[],
            }; 8];
            let mut offset = 0;
            for (index, part) in parts.iter_mut().enumerate().take(buffer.count) {
                let length = usize::from(buffer.lengths[index]);
                *part = SendDatagram {
                    prefix: &buffer.prefixes[index],
                    payload: &buffer.bytes[offset..offset + length],
                };
                offset += length;
            }
            match socket.send_batch(&parts[buffer.first..buffer.count]) {
                Ok(sent) => {
                    for part in &parts[buffer.first..buffer.first + sent] {
                        self.counters.tx_outer_bytes +=
                            (part.prefix.len() + part.payload.len() + 48) as u64;
                    }
                    buffer.first += sent;
                    if buffer.first == buffer.count {
                        self.release(buffer);
                    } else {
                        self.pending.push_back(buffer);
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    let token = TX_TOKEN | ((peer as u64) << 6) | buffer.lane as u64;
                    self.reactor
                        .add(socket.as_fd(), token, libc::EPOLLOUT as u32)?;
                    tx.blocked = true;
                    self.pending.push_back(buffer);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                    self.pending.push_back(buffer)
                }
                Err(_) => {
                    self.counters.socket_errors += 1;
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
                    self.return_lane(session, lane);
                    continue;
                }
                self.counters.expired_frames += lane.expire(now) as u64;
                let Some(tap) = &mut local.tap else { continue };
                for _ in 0..BATCH_SIZE {
                    let Some(frame) = lane.front(now) else { break };
                    let length = frame.bytes.len();
                    match tap.write_frame(frame.bytes) {
                        Ok(()) => self.counters.delivered_frame_bytes += length as u64,
                        Err(error) if transient(&error) => {
                            writable = true;
                            break;
                        }
                        Err(_) => self.counters.tap_errors += 1,
                    }
                    lane.finish_front();
                }
            }
            let local = &mut self.sessions[session];
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
        let Some(socket) = &self.transmit[peer][0].socket else {
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
        match socket.send(&bytes[..packet.length]) {
            Ok(()) => {
                self.counters.tx_outer_bytes += (packet.length + 48) as u64;
                Ok(Some((packet.number, packet.length as u16)))
            }
            Err(error) => {
                if !transient(&error) {
                    self.counters.socket_errors += 1;
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

    fn command(&mut self, session: usize, command: Command) {
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
                let cookie = self.shared.config.config().sessions[session].cookie;
                for (index, lane) in local.tx.iter_mut().enumerate() {
                    *lane = command
                        .connection
                        .filter(|_| index % self.shared.workers() == self.id)
                        .map(|c| pack::Lane::new(cookie, c, index as u16 + 1).unwrap());
                }
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
                        self.command(session, command);
                    } else if self
                        .push(worker, Event::Barrier { session, command })
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
        if !self.shared.all_bound() {
            return Ok(());
        }
        for session in 0..self.controls.len() {
            let peer = self.shared.config.peer_for_session(session).unwrap();
            if self.transmit[peer][0].socket.is_none() || self.controls[session].barrier.is_active()
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
                if effects.needs_tag {
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
            if self.shared.all_bound()
                && self.transmit[peer][0].socket.is_some()
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
    #[ignore = "kernel descriptor test; run in a disposable container"]
    fn full_handoff_rings_return_buffers_and_finish_connection_barriers() {
        use crate::config::{Config, Daemon, Endpoint, Peer, Session as ConfigSession};
        use std::{
            net::Ipv6Addr,
            sync::atomic::{AtomicU8, AtomicU64},
        };
        let config = Config {
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
        }
        .validate()
        .unwrap();
        let shared = Arc::new(Shared {
            config,
            wakes: (0..2).map(|_| Arc::new(Wake::new().unwrap())).collect(),
            owners: vec![(0..65_536).map(|_| AtomicU8::new(0)).collect()],
            status: vec![AtomicU8::new(0)],
            counters: (0..2).map(|_| crate::counters::Counters::new()).collect(),
            bound: AtomicU64::new(0),
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
            let buffer = workers[0].free.pop().unwrap();
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
            worker.command(0, initial);
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
            worker.command(0, command);
            worker.command(
                0,
                Command {
                    phase: Phase::Quiesce,
                    ..command
                },
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
