//! Disposable-namespace cost probe; no production offload changes.
//! Run only in a disposable container with CAP_NET_ADMIN and iproute2. The
//! process creates its own network namespace before configuring devices.
//! A successful write
//! alone is not delivery: every injected UDP payload is received and checked.
use std::{
    io,
    mem::zeroed,
    net::UdpSocket,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    process::Command,
    time::{Duration, Instant},
};
const PAYLOAD: usize = 1472;
const ROUNDS: usize = 8192;
fn check(n: i32) -> i32 {
    assert!(n >= 0, "{}", io::Error::last_os_error());
    n
}
fn ip(args: &[&str]) {
    assert!(Command::new("ip").args(args).status().unwrap().success());
}
fn cpu() -> u64 {
    let mut t = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock writes the initialized output structure.
    check(unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut t) });
    t.tv_sec as u64 * 1_000_000_000 + t.tv_nsec as u64
}
fn fold(mut sum: u32) -> u16 {
    while sum > 65535 {
        sum = (sum & 65535) + (sum >> 16);
    }
    sum as u16
}
fn sum(bytes: &[u8]) -> u32 {
    bytes
        .chunks(2)
        .map(|p| u16::from_be_bytes([p[0], *p.get(1).unwrap_or(&0)]) as u32)
        .sum()
}
fn payload(index: usize) -> Vec<u8> {
    let mut p = vec![0x5a; PAYLOAD];
    p[..8].copy_from_slice(&(index as u64).to_be_bytes());
    p
}
fn packet(count: usize, partial: bool, index: usize) -> Vec<u8> {
    let mut p = vec![0; 42];
    p[..14].copy_from_slice(&[2, 0, 0, 0, 0, 1, 2, 0, 0, 0, 0, 2, 8, 0]);
    p[14] = 0x45;
    p[16..18].copy_from_slice(&((28 + count * PAYLOAD) as u16).to_be_bytes());
    p[18..20].copy_from_slice(&42u16.to_be_bytes());
    p[20..22].copy_from_slice(&0x4000u16.to_be_bytes());
    p[22] = 64;
    p[23] = 17;
    p[26..34].copy_from_slice(&[198, 18, 0, 2, 198, 18, 0, 1]);
    let c = !fold(sum(&p[14..34]));
    p[24..26].copy_from_slice(&c.to_be_bytes());
    p[34..36].copy_from_slice(&9000u16.to_be_bytes());
    p[36..38].copy_from_slice(&9999u16.to_be_bytes());
    p[38..40].copy_from_slice(&((8 + count * PAYLOAD) as u16).to_be_bytes());
    for i in 0..count {
        p.extend_from_slice(&payload(index + i));
    }
    let pseudo = sum(&p[26..34]) + 17 + (8 + count * PAYLOAD) as u32;
    let c = if partial {
        fold(pseudo)
    } else {
        let c = !fold(pseudo + sum(&p[34..]));
        if c == 0 { 65535 } else { c }
    };
    p[40..42].copy_from_slice(&c.to_be_bytes());
    p
}
fn tap(vnet: bool) -> OwnedFd {
    // SAFETY: constant pathname; successful open transfers a new descriptor.
    let fd = unsafe {
        OwnedFd::from_raw_fd(check(libc::open(
            c"/dev/net/tun".as_ptr(),
            libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )))
    };
    // SAFETY: zeroed ifreq is valid before populating its fields.
    let mut req: libc::ifreq = unsafe { zeroed() };
    for (d, s) in req.ifr_name.iter_mut().zip(b"uq-cost") {
        *d = *s as _;
    }
    req.ifr_ifru.ifru_flags = (libc::IFF_TAP
        | libc::IFF_NO_PI
        | libc::IFF_TUN_EXCL
        | if vnet { libc::IFF_VNET_HDR } else { 0 }) as _;
    // SAFETY: initialized ifreq; offload ioctl takes an integer.
    unsafe {
        check(libc::ioctl(fd.as_raw_fd(), libc::TUNSETIFF, &mut req));
        check(libc::ioctl(
            fd.as_raw_fd(),
            libc::TUNSETOFFLOAD,
            0 as libc::c_ulong,
        ));
        if vnet {
            let size = 10i32;
            check(libc::ioctl(fd.as_raw_fd(), libc::TUNSETVNETHDRSZ, &size));
        }
    }
    ip(&[
        "link",
        "set",
        "uq-cost",
        "address",
        "02:00:00:00:00:01",
        "addrgenmode",
        "none",
    ]);
    ip(&["addr", "add", "198.18.0.1/24", "dev", "uq-cost"]);
    ip(&["link", "set", "uq-cost", "up"]);
    ip(&[
        "neigh",
        "add",
        "198.18.0.2",
        "lladdr",
        "02:00:00:00:00:02",
        "nud",
        "permanent",
        "dev",
        "uq-cost",
    ]);
    fd
}
fn writes(count: usize, vnet: bool, clock_cost: u64) {
    let tap = tap(vnet);
    let receiver = UdpSocket::bind("198.18.0.1:9999").unwrap();
    receiver
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let groups = if count == 1 && !vnet { 64 } else { 1 };
    let packets: Vec<_> = (0..groups).map(|i| packet(count, vnet, i)).collect();
    let mut header = [0u8; 10];
    if vnet {
        header[0] = 1;
        header[1] = 5;
        for (offset, value) in [(2, 42u16), (4, PAYLOAD as u16), (6, 34), (8, 6)] {
            header[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
        }
    }
    let mut iov: Vec<_> = Vec::new();
    if vnet {
        let p = &packets[0];
        iov.push(libc::iovec {
            iov_base: header.as_mut_ptr().cast(),
            iov_len: 10,
        });
        iov.push(libc::iovec {
            iov_base: p.as_ptr().cast_mut().cast(),
            iov_len: 42,
        });
        for piece in p[42..].chunks(PAYLOAD) {
            iov.push(libc::iovec {
                iov_base: piece.as_ptr().cast_mut().cast(),
                iov_len: piece.len(),
            });
        }
    }
    let expected: Vec<_> = (0..count * groups).map(payload).collect();
    let mut received = [0u8; PAYLOAD + 1];
    let mut ns = 0u64;
    let wall = Instant::now();
    for _ in 0..ROUNDS {
        let start = cpu();
        for p in &packets {
            // SAFETY: all referenced packet and iovec storage stays live and readable.
            let n = unsafe {
                if vnet {
                    libc::writev(tap.as_raw_fd(), iov.as_ptr(), iov.len() as _)
                } else {
                    libc::write(tap.as_raw_fd(), p.as_ptr().cast(), p.len())
                }
            };
            assert_eq!(
                n,
                (p.len() + if vnet { 10 } else { 0 }) as isize,
                "{}",
                io::Error::last_os_error()
            );
        }
        ns += (cpu() - start).saturating_sub(clock_cost);
        for p in &expected {
            let n = receiver.recv(&mut received).unwrap();
            assert_eq!(&received[..n], p);
        }
    }
    let total = ROUNDS * groups * count;
    println!(
        "write vnet={vnet} segments={count} frames={total} cpu_ns_per_frame={:.2} wall_seconds={:.3} delivered={total}",
        ns as f64 / total as f64,
        wall.elapsed().as_secs_f64()
    );
}
fn reads(clock_cost: u64, use_ring: bool) {
    let tap = tap(false);
    let sender = UdpSocket::bind("198.18.0.1:0").unwrap();
    sender.connect("198.18.0.2:9999").unwrap();
    let df = libc::IP_PMTUDISC_DO;
    // SAFETY: integer socket option has its exact ABI size.
    check(unsafe {
        libc::setsockopt(
            sender.as_raw_fd(),
            libc::IPPROTO_IP,
            libc::IP_MTU_DISCOVER,
            (&df as *const i32).cast(),
            size_of_val(&df) as _,
        )
    });

    let expected: Vec<_> = (0..64).map(payload).collect();
    let mut received = vec![[0u8; 1515]; 64];
    let mut lengths = [0isize; 64];
    let mut ring = if use_ring {
        Some(Ring::new(tap.as_raw_fd(), &mut received).unwrap())
    } else {
        None
    };
    if let Some(ring) = &mut ring {
        ring.read(&mut lengths);
        assert!(lengths.iter().all(|&n| n == -(libc::EAGAIN as isize)));
        println!("empty_ring_nowait_completions=64");
    }
    let mut ns = 0u64;
    let wall = Instant::now();
    for round in 0..ROUNDS {
        for p in &expected {
            assert_eq!(sender.send(p).unwrap(), PAYLOAD);
        }
        let start = cpu();
        if let Some(ring) = &mut ring {
            ring.read(&mut lengths);
        } else {
            for (p, n) in received.iter_mut().zip(&mut lengths) {
                // SAFETY: each buffer is writable for its full length.
                *n = unsafe { libc::read(tap.as_raw_fd(), p.as_mut_ptr().cast(), p.len()) };
            }
        }
        ns += (cpu() - start).saturating_sub(clock_cost);
        if round == 0 {
            println!(
                "read ring={use_ring} first_ipv4_ids={:?}",
                received[..8]
                    .iter()
                    .map(|p| u16::from_be_bytes([p[18], p[19]]))
                    .collect::<Vec<_>>()
            );
        }
        for ((p, n), e) in received.iter().zip(lengths).zip(&expected) {
            assert_eq!(n, 1514, "{}", io::Error::last_os_error());
            assert_eq!(&p[42..1514], e);
        }
    }
    println!(
        "read ring={use_ring} frames={} cpu_ns_per_frame={:.2} wall_seconds={:.3} tasks={}",
        ROUNDS * 64,
        ns as f64 / (ROUNDS * 64) as f64,
        wall.elapsed().as_secs_f64(),
        std::fs::read_dir("/proc/self/task").unwrap().count()
    );
}
fn main() {
    if !std::env::args().any(|arg| arg == "--run-in-container") {
        eprintln!("TAP cost probe requires --run-in-container in a disposable container");
        return;
    }
    // SAFETY: unshare changes only this process's namespace; never the host.
    check(unsafe { libc::unshare(libc::CLONE_NEWNET) });
    ip(&["link", "set", "lo", "up"]);
    let mut overhead = 0;
    for _ in 0..10000 {
        let start = cpu();
        overhead += cpu() - start;
    }
    overhead /= 10000;
    println!("clock_pair_cost_ns={overhead} rounds={ROUNDS}");
    reads(overhead, false);
    reads(overhead, true);
    writes(1, false, overhead);
    for count in [1, 2, 16, 43, 44] {
        writes(count, true, overhead);
    }
}

#[repr(C)]
#[derive(Default)]
struct SqOffsets {
    head: u32,
    tail: u32,
    mask: u32,
    entries: u32,
    flags: u32,
    dropped: u32,
    array: u32,
    reserved: u32,
    user_addr: u64,
}
#[repr(C)]
#[derive(Default)]
struct CqOffsets {
    head: u32,
    tail: u32,
    mask: u32,
    entries: u32,
    overflow: u32,
    cqes: u32,
    flags: u32,
    reserved: u32,
    user_addr: u64,
}
#[repr(C)]
#[derive(Default)]
struct Params {
    sq_entries: u32,
    cq_entries: u32,
    flags: u32,
    sq_cpu: u32,
    sq_idle: u32,
    features: u32,
    wq_fd: u32,
    reserved: [u32; 3],
    sq: SqOffsets,
    cq: CqOffsets,
}
#[repr(C)]
struct Sqe {
    opcode: u8,
    flags: u8,
    priority: u16,
    fd: i32,
    offset: u64,
    address: u64,
    length: u32,
    rw_flags: u32,
    user: u64,
    unused: [u64; 3],
}
#[repr(C)]
#[derive(Clone, Copy)]
struct Cqe {
    user: u64,
    result: i32,
    flags: u32,
}
const _: () = {
    assert!(size_of::<Params>() == 120);
    assert!(size_of::<Sqe>() == 64);
    assert!(size_of::<Cqe>() == 16);
};
struct Map {
    pointer: *mut u8,
    length: usize,
}
impl Map {
    fn new(fd: i32, length: usize, offset: libc::off_t) -> io::Result<Self> {
        // SAFETY: kernel fd and checked ABI sizes describe the requested mapping.
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                length,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                offset,
            )
        };
        if p == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            pointer: p.cast(),
            length,
        })
    }
    fn at<T>(&self, offset: usize) -> *mut T {
        assert!(offset + size_of::<T>() <= self.length);
        // SAFETY: bounds checked; callers use kernel offsets aligned for their ABI type.
        let p = unsafe { self.pointer.add(offset).cast::<T>() };
        assert_eq!(p as usize % align_of::<T>(), 0);
        p
    }
    fn atomic(&self, offset: u32) -> &std::sync::atomic::AtomicU32 {
        // SAFETY: ring counters are aligned shared u32s; all concurrent access is atomic.
        unsafe { &*self.at(offset as usize) }
    }
}
impl Drop for Map {
    fn drop(&mut self) {
        // SAFETY: this object owns the mapping and drops after all completions.
        check(unsafe { libc::munmap(self.pointer.cast(), self.length) });
    }
}
struct Ring {
    fd: OwnedFd,
    sq: Map,
    cq: Map,
    _sqes: Map,
    params: Params,
    tail: u32,
}
impl Ring {
    fn new(fd: i32, buffers: &mut [[u8; 1515]]) -> io::Result<Self> {
        assert_eq!(buffers.len(), 64);
        let mut p = Params::default();
        // SAFETY: p is the complete zero-initialized io_uring_params ABI structure.
        let n = unsafe { libc::syscall(libc::SYS_io_uring_setup, 64u32, &mut p) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: sole owner of new descriptor.
        let ring = unsafe { OwnedFd::from_raw_fd(n as i32) };
        assert_eq!(p.sq_entries, 64);
        let sq = Map::new(
            ring.as_raw_fd(),
            p.sq.array as usize + 4 * p.sq_entries as usize,
            0,
        )?;
        let cq = Map::new(
            ring.as_raw_fd(),
            p.cq.cqes as usize + 16 * p.cq_entries as usize,
            0x08000000,
        )?;
        let sqes = Map::new(ring.as_raw_fd(), 64 * p.sq_entries as usize, 0x10000000)?;
        for (i, buffer) in buffers.iter_mut().enumerate() {
            // SAFETY: SQ storage is mapped and not submitted yet; buffers never move or resize.
            unsafe {
                sqes.at::<Sqe>(64 * i).write(Sqe {
                    opcode: 22,
                    flags: 0,
                    priority: 0,
                    fd,
                    offset: 0,
                    address: buffer.as_mut_ptr() as u64,
                    length: buffer.len() as u32,
                    rw_flags: libc::RWF_NOWAIT as u32,
                    user: i as u64,
                    unused: [0; 3],
                });
                sq.at::<u32>(p.sq.array as usize + 4 * i).write(i as u32);
            }
        }
        Ok(Self {
            fd: ring,
            sq,
            cq,
            _sqes: sqes,
            params: p,
            tail: 0,
        })
    }
    fn read(&mut self, lengths: &mut [isize; 64]) {
        use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};
        assert_eq!(self.sq.atomic(self.params.sq.head).load(Acquire), self.tail);
        self.tail = self.tail.wrapping_add(64);
        self.sq
            .atomic(self.params.sq.tail)
            .store(self.tail, Release);
        // SAFETY: 64 READ SQEs reference live buffers; RWF_NOWAIT forbids blocking fallback.
        let n = unsafe {
            libc::syscall(
                libc::SYS_io_uring_enter,
                self.fd.as_raw_fd(),
                64u32,
                64u32,
                1u32,
                std::ptr::null::<libc::sigset_t>(),
                0usize,
            )
        };
        assert_eq!(n, 64, "{}", io::Error::last_os_error());
        let head = self.cq.atomic(self.params.cq.head).load(Relaxed);
        let tail = self.cq.atomic(self.params.cq.tail).load(Acquire);
        assert_eq!(tail.wrapping_sub(head), 64);
        let mask = self.cq.atomic(self.params.cq.mask).load(Relaxed);
        let mut seen = 0u64;
        for i in 0..64 {
            // SAFETY: acquired CQ tail publishes these initialized CQEs; head remains unchanged.
            let c = unsafe {
                self.cq
                    .at::<Cqe>(
                        self.params.cq.cqes as usize + 16 * (head.wrapping_add(i) & mask) as usize,
                    )
                    .read()
            };
            assert!(c.user < 64);
            assert_eq!(seen & (1 << c.user), 0);
            seen |= 1 << c.user;
            lengths[c.user as usize] = c.result as isize;
        }
        assert_eq!(seen, u64::MAX);
        self.cq.atomic(self.params.cq.head).store(tail, Release);
        assert_eq!(self.cq.atomic(self.params.cq.overflow).load(Relaxed), 0);
        assert_eq!(self.sq.atomic(self.params.sq.dropped).load(Relaxed), 0);
    }
}
