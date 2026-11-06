//! Linux-only concurrent UDP load tool. See benches/udp_load.md.
use std::{
    io,
    net::{IpAddr, SocketAddr, UdpSocket},
    os::fd::{AsFd, AsRawFd},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

static SNAPSHOT: AtomicBool = AtomicBool::new(false);
extern "C" fn snapshot(_: libc::c_int) {
    SNAPSHOT.store(true, Ordering::Relaxed);
}

const MAGIC: [u8; 4] = *b"UQL1";
// Cover delayed super-frames across the harness's eight 10,000-entry TAP queues.
const WORDS: usize = 1 << 18;
const MAX_BYTES: usize = 65_487;

struct Config {
    send: bool,
    local: IpAddr,
    peer: IpAddr,
    port: u16,
    peer_port: u16,
    flows: usize,
    payload: usize,
    seconds: u64,
    rate: u64,
    run: u32,
    segments: usize,
}
impl Config {
    fn parse(args: &[String]) -> Result<Self, Box<dyn std::error::Error>> {
        if args.len() != 11 || !matches!(args[0].as_str(), "send" | "recv") {
            return Err("usage: udp_load send|recv LOCAL_IP PEER_IP LOCAL_PORT PEER_PORT FLOWS PAYLOAD_BYTES SECONDS BITS_PER_FLOW RUN_ID SEGMENTS".into());
        }
        let c = Self {
            send: args[0] == "send",
            local: args[1].parse()?,
            peer: args[2].parse()?,
            port: args[3].parse()?,
            peer_port: args[4].parse()?,
            flows: args[5].parse()?,
            payload: args[6].parse()?,
            seconds: args[7].parse()?,
            rate: args[8].parse()?,
            run: args[9].parse()?,
            segments: args[10].parse()?,
        };
        Ok(c)
    }

    fn validate(&self) -> io::Result<()> {
        if self.local.is_ipv4() != self.peer.is_ipv4()
            || self.local.is_unspecified()
            || self.peer.is_unspecified()
            || self.local.is_multicast()
            || self.peer.is_multicast()
            || !(1..=64).contains(&self.flows)
            || self.port == 0
            || usize::from(self.port) + self.flows > 65536
            || self.peer_port == 0
            || usize::from(self.peer_port) + self.flows > 65536
            || !(16..=9000).contains(&self.payload)
            || !(1..=3600).contains(&self.seconds)
            || !(1..=64).contains(&self.segments)
            || self.rate > 1_000_000_000_000
            || self.run == 0
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid address, port range, flow count, payload, duration, rate, run ID or segment count",
            ));
        }
        Ok(())
    }
    fn count(&self) -> usize {
        self.segments.min(MAX_BYTES / self.payload)
    }
}

// Tags avoid clearing the entire bitmap on a large sequence gap.
struct Window {
    tags: Box<[u64]>,
    bits: Box<[u64]>,
    next: u64,
    unique: u64,
    duplicates: u64,
    reordered: u64,
    too_old: u64,
}
impl Window {
    fn new() -> Self {
        Self {
            tags: vec![u64::MAX; WORDS].into_boxed_slice(),
            bits: vec![0; WORDS].into_boxed_slice(),
            next: 0,
            unique: 0,
            duplicates: 0,
            reordered: 0,
            too_old: 0,
        }
    }
    fn record(&mut self, seq: u64) {
        let word = seq / 64;
        if self.next != 0 && (self.next - 1) / 64 >= word + WORDS as u64 {
            self.too_old += 1;
            return;
        }
        let index = word as usize % WORDS;
        if self.tags[index] != word {
            self.tags[index] = word;
            self.bits[index] = 0;
        }
        let bit = 1 << (seq % 64);
        if self.bits[index] & bit != 0 {
            self.duplicates += 1;
            return;
        }
        self.bits[index] |= bit;
        self.unique += 1;
        self.reordered += u64::from(seq < self.next);
        self.next = self.next.max(seq + 1);
    }
}

// Integer credits are bit-nanoseconds; retain at most two aggregates of catch-up.
struct Bucket {
    credit: u128,
    last: u64,
    cost: u128,
}
impl Bucket {
    fn new(bytes: usize) -> Self {
        Self {
            credit: 0,
            last: 0,
            cost: bytes as u128 * 8 * 1_000_000_000,
        }
    }
    fn wait(&mut self, now: u64, rate: u64) -> u64 {
        if rate == 0 {
            return 0;
        }
        self.credit =
            (self.credit + u128::from(now - self.last) * u128::from(rate)).min(self.cost * 2);
        self.last = now;
        self.cost
            .saturating_sub(self.credit)
            .div_ceil(u128::from(rate)) as u64
    }
    fn sent(&mut self, rate: u64) {
        if rate != 0 {
            self.credit -= self.cost;
        }
    }
}

struct Flow {
    socket: UdpSocket,
    sequence: u64,
    window: Window,
    bucket: Bucket,
    calls: u64,
    gro: u64,
    blocked: u64,
    invalid: u64,
    wrong_run: u64,
    first_wrong_run: u32,
    overflow: u64,
    overflow_last: u32,
    receive_bytes: i32,
}
fn option(socket: &UdpSocket, level: i32, name: i32, value: i32) -> io::Result<()> {
    // SAFETY: value is an initialized integer with the option's ABI size.
    let n = unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            level,
            name,
            (&value as *const i32).cast(),
            size_of::<i32>() as _,
        )
    };
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
fn flows(c: &Config) -> io::Result<Vec<Flow>> {
    (0..c.flows)
        .map(|i| {
            let port = c.port + i as u16;
            let socket = UdpSocket::bind(SocketAddr::new(c.local, port))?;
            socket.connect(SocketAddr::new(c.peer, c.peer_port + i as u16))?;
            socket.set_nonblocking(true)?;
            let (level, name) = if c.local.is_ipv4() {
                (libc::IPPROTO_IP, libc::IP_MTU_DISCOVER)
            } else {
                (libc::IPPROTO_IPV6, libc::IPV6_MTU_DISCOVER)
            };
            option(&socket, level, name, libc::IP_PMTUDISC_DO)?;
            if c.send && c.segments > 1 {
                option(
                    &socket,
                    libc::IPPROTO_UDP,
                    libc::UDP_SEGMENT,
                    c.payload as i32,
                )?;
            }
            if !c.send {
                option(&socket, libc::IPPROTO_UDP, libc::UDP_GRO, 1)?;
                option(&socket, libc::SOL_SOCKET, libc::SO_RXQ_OVFL, 1)?;
                // Use ordinary capped buffers so the harness needs no extra privileges.
                option(&socket, libc::SOL_SOCKET, libc::SO_RCVBUF, 8 * 1024 * 1024)?;
            }
            let mut receive_bytes = 0i32;
            let mut len = size_of::<i32>() as libc::socklen_t;
            // SAFETY: initialized integer output and exact size.
            if unsafe {
                libc::getsockopt(
                    socket.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_RCVBUF,
                    (&mut receive_bytes as *mut i32).cast(),
                    &mut len,
                )
            } < 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(Flow {
                socket,
                sequence: 0,
                window: Window::new(),
                bucket: Bucket::new(c.count() * c.payload),
                calls: 0,
                gro: 0,
                blocked: 0,
                invalid: 0,
                wrong_run: 0,
                first_wrong_run: 0,
                overflow: 0,
                overflow_last: 0,
                receive_bytes,
            })
        })
        .collect()
}

fn fill(bytes: &mut [u8], size: usize, run: u32, sequence: u64) {
    for (i, p) in bytes.chunks_exact_mut(size).enumerate() {
        p[..4].copy_from_slice(&MAGIC);
        p[4..8].copy_from_slice(&run.to_be_bytes());
        p[8..16].copy_from_slice(&(sequence + i as u64).to_be_bytes());
    }
}
fn consume(flow: &mut Flow, bytes: &[u8], segment: usize, size: usize, run: u32) {
    if bytes.is_empty() || segment == 0 || segment > bytes.len() {
        flow.invalid += 1;
        return;
    }
    for p in bytes.chunks(segment) {
        if p.len() != size
            || p[..4] != MAGIC
            || p[16..].iter().fold(0, |bad, &v| bad | (v ^ 0xa5)) != 0
        {
            flow.invalid += 1;
            continue;
        }
        let observed_run = u32::from_be_bytes(p[4..8].try_into().unwrap());
        if observed_run != run {
            flow.invalid += 1;
            flow.wrong_run += 1;
            if flow.first_wrong_run == 0 {
                flow.first_wrong_run = observed_run;
            }
            continue;
        }
        let seq = u64::from_be_bytes(p[8..16].try_into().unwrap());
        if seq >= 1 << 63 {
            flow.invalid += 1;
        } else {
            flow.window.record(seq);
        }
    }
}

fn receive(flow: &mut Flow, bytes: &mut [u8], c: &Config) -> io::Result<bool> {
    let count = bytes.len().div_ceil(MAX_BYTES).min(16);
    let mut chunks = bytes.chunks_mut(MAX_BYTES);
    let mut packets: [unquic::socket::ReceiveDatagram<'_>; 16] = std::array::from_fn(|_| {
        unquic::socket::ReceiveDatagram::new(chunks.next().unwrap_or(&mut []))
    });
    flow.calls += 1;
    let received = match unquic::socket::recv_batch(flow.socket.as_fd(), &mut packets[..count]) {
        Ok(n) => n,
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
            ) =>
        {
            return Ok(false);
        }
        Err(e) => return Err(e),
    };
    for packet in &packets[..received] {
        if packet.truncated {
            flow.invalid += 1;
            continue;
        }
        if let Some(value) = packet.overflow {
            flow.overflow += u64::from(value.wrapping_sub(flow.overflow_last));
            flow.overflow_last = value;
        }
        flow.gro += u64::from(packet.segment_size != 0);
        consume(
            flow,
            &packet.buffer[..packet.length],
            if packet.segment_size == 0 {
                packet.length
            } else {
                packet.segment_size
            },
            c.payload,
            c.run,
        );
    }
    Ok(received != 0)
}

fn cpu_ns() -> io::Result<u64> {
    let mut t = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: t is writable; the process clock includes all threads.
    if unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut t) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(t.tv_sec as u64 * 1_000_000_000 + t.tv_nsec as u64)
}
fn run(c: &Config) -> io::Result<()> {
    c.validate()?;
    let mut flows = flows(c)?;
    let mut bytes = vec![0xa5; MAX_BYTES * if c.send { 1 } else { 16 }];
    let mut polls: Vec<_> = flows
        .iter()
        .map(|f| libc::pollfd {
            fd: f.socket.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        })
        .collect();
    if !c.send {
        // SAFETY: sigaction contains a plain function handler and an initialized empty mask.
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = snapshot as *const () as usize;
        unsafe {
            libc::sigemptyset(&mut action.sa_mask);
            if libc::sigaction(libc::SIGUSR1, &action, std::ptr::null_mut()) < 0 {
                return Err(io::Error::last_os_error());
            }
        }
    }
    let mut samples = [(0.0f64, 0u64); 2];
    let mut sample_count = 0;
    let mut poll_calls = 0u64;
    let start_cpu = cpu_ns()?;
    let start = Instant::now();
    let end = Duration::from_secs(c.seconds);
    let length = c.count() * c.payload;
    while start.elapsed() < end {
        let now = start.elapsed().as_nanos() as u64;
        if SNAPSHOT.load(Ordering::Relaxed)
            && SNAPSHOT.swap(false, Ordering::Relaxed)
            && sample_count < samples.len()
        {
            samples[sample_count] = (
                now as f64 / 1e9,
                flows.iter().map(|f| f.window.unique).sum(),
            );
            sample_count += 1;
        }
        let mut progress = false;
        let mut delay = 1_000_000;
        if !c.send {
            poll_calls += 1;
            // SAFETY: polls contains initialized live descriptors and writable revents.
            let n = unsafe { libc::poll(polls.as_mut_ptr(), polls.len() as _, 1) };
            if n < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
        }
        for (index, f) in flows.iter_mut().enumerate() {
            if c.send {
                let wait = f.bucket.wait(now, c.rate);
                if wait != 0 {
                    delay = delay.min(wait);
                    continue;
                }
                fill(&mut bytes[..length], c.payload, c.run, f.sequence);
                f.calls += 1;
                match f.socket.send(&bytes[..length]) {
                    Ok(n) if n == length => {
                        f.sequence += c.count() as u64;
                        f.bucket.sent(c.rate);
                        progress = true;
                    }
                    Ok(_) => return Err(io::Error::other("partial UDP send")),
                    Err(e)
                        if matches!(
                            e.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                        ) || e.raw_os_error() == Some(libc::ENOBUFS) =>
                    {
                        f.blocked += 1
                    }
                    Err(e) => return Err(e),
                }
            } else if polls[index].revents != 0 {
                receive(f, &mut bytes, c)?;
            }
        }
        if c.send && !progress {
            std::thread::sleep(Duration::from_nanos(delay));
        }
    }
    let elapsed = start.elapsed().as_secs_f64();
    let cpu = (cpu_ns()? - start_cpu) as f64 / 1e9;
    println!(
        "{{\"mode\":\"{}\",\"payload_bytes\":{},\"segments_per_send\":{},\"requested_bps_per_flow\":{},\"run_id\":{},\"seconds\":{elapsed:.9},\"cpu_seconds\":{cpu:.9},\"poll_calls\":{poll_calls},\"flows\":[",
        if c.send { "send" } else { "recv" },
        c.payload,
        c.count(),
        c.rate,
        c.run
    );
    for (i, f) in flows.iter().enumerate() {
        let w = &f.window;
        println!(
            "{}{{\"port\":{},\"sent\":{},\"unique\":{},\"next_sequence\":{},\"prefix_missing\":{},\"duplicates\":{},\"reordered\":{},\"too_old\":{},\"invalid\":{},\"wrong_run\":{},\"first_wrong_run\":{},\"calls\":{},\"gro_messages\":{},\"blocked\":{},\"socket_overflow\":{},\"receive_buffer_bytes\":{}}}",
            if i == 0 { "" } else { "," },
            c.port + i as u16,
            f.sequence,
            w.unique,
            w.next,
            w.next - w.unique,
            w.duplicates,
            w.reordered,
            w.too_old,
            f.invalid,
            f.wrong_run,
            f.first_wrong_run,
            f.calls,
            f.gro,
            f.blocked,
            f.overflow,
            f.receive_bytes
        );
    }
    print!("],\"samples\":[");
    for (i, (seconds, unique)) in samples[..sample_count].iter().enumerate() {
        print!(
            "{}{{\"seconds\":{seconds:.9},\"unique\":{unique}}}",
            if i == 0 { "" } else { "," }
        );
    }
    println!("]}}");
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let c = Config::parse(&args)?;
    run(&c)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;
    thread_local! {
        static ALLOCATIONS: Cell<Option<(usize, usize)>> = const { Cell::new(None) };
    }
    struct Allocator;
    #[global_allocator]
    static ALLOCATOR: Allocator = Allocator;
    fn allocation(free: bool) {
        let _ = ALLOCATIONS.try_with(|c| {
            if let Some((a, d)) = c.get() {
                c.set(Some((a + usize::from(!free), d + usize::from(free))));
            }
        });
    }
    // SAFETY: forwards each allocation contract unchanged to System.
    unsafe impl GlobalAlloc for Allocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            allocation(false);
            unsafe { System.alloc(layout) }
        }
        unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
            allocation(true);
            unsafe { System.dealloc(p, layout) }
        }
        unsafe fn realloc(&self, p: *mut u8, layout: Layout, size: usize) -> *mut u8 {
            allocation(false);
            unsafe { System.realloc(p, layout, size) }
        }
    }
    fn without_allocations(f: impl FnOnce()) {
        ALLOCATIONS.with(|c| c.set(Some((0, 0))));
        f();
        let counts = ALLOCATIONS.with(|c| c.replace(None));
        assert_eq!(counts, Some((0, 0)));
    }

    #[test]
    fn sequence_window_counts_gaps_reordering_duplicates_and_old_frames() {
        let mut w = Window::new();
        for n in [0, 3, 1, 3, 2, 2] {
            w.record(n);
        }
        assert_eq!((w.unique, w.next, w.duplicates, w.reordered), (4, 4, 2, 2));
        let span = (WORDS * 64) as u64;
        w.record(span);
        w.record(1);
        w.record(span);
        w.record(64);
        w.record(64);
        assert_eq!(
            (w.unique, w.next, w.too_old, w.duplicates),
            (6, span + 1, 1, 4)
        );
        w.record(1 << 50);
        w.record((1 << 50) - 1);
        w.record(1 << 50);
        assert_eq!((w.unique, w.duplicates), (8, 5));
    }

    #[test]
    fn delayed_super_frames_are_counted_once_without_hot_allocations() {
        let mut w = Window::new();
        without_allocations(|| {
            let far = 8 * 10_000 * 64;
            for n in [0, far, 8192, 1, far - 1, 8192, far, 1] {
                w.record(n);
            }
        });
        assert_eq!((w.unique, w.duplicates, w.too_old), (5, 3, 0));
    }

    #[test]
    fn token_bucket_caps_catchup_and_charges_only_successful_sends() {
        let mut b = Bucket::new(1000);
        assert_eq!(b.wait(0, 8000), 1_000_000_000);
        assert_eq!(b.wait(500_000_000, 8000), 500_000_000);
        assert_eq!(b.wait(1_000_000_000, 8000), 0);
        assert_eq!(b.wait(1_000_000_000, 8000), 0);
        b.sent(8000);
        assert_eq!(b.wait(1_000_000_000, 8000), 1_000_000_000);
        assert_eq!(b.wait(9_000_000_000, 8000), 0);
        b.sent(8000);
        b.sent(8000);
        assert_eq!(b.wait(9_000_000_000, 8000), 1_000_000_000);
        assert_eq!(b.wait(9_000_000_000, 0), 0);
        b.sent(0);
    }
    #[test]
    fn cli_rejects_invalid_bounds_and_address_families() {
        let a: Vec<String> = "send 127.0.0.1 127.0.0.2 51000 52000 16 1472 20 1312500000 1 64"
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        let c = Config::parse(&a).unwrap();
        c.validate().unwrap();
        assert_eq!(c.count(), 44);
        for (i, value) in [
            (2, "::1"),
            (3, "0"),
            (4, "65535"),
            (5, "65"),
            (6, "15"),
            (7, "0"),
            (8, "1000000000001"),
            (9, "0"),
            (10, "65"),
        ] {
            let mut bad = a.clone();
            bad[i] = value.to_owned();
            assert!(
                Config::parse(&bad).unwrap().validate().is_err(),
                "field {i}"
            );
        }
        assert!(Config::parse(&a[..10]).is_err());
    }

    #[test]
    #[ignore = "UDP offload test; run in a disposable container"]
    fn ipv4_ipv6_gso_gro_preserve_payload_counts_and_reject_bad_input() {
        // SAFETY: only this test thread and its child enter the private namespace.
        assert_eq!(unsafe { libc::unshare(libc::CLONE_NEWNET) }, 0);
        assert!(
            std::process::Command::new("ip")
                .args(["link", "set", "lo", "up"])
                .status()
                .unwrap()
                .success()
        );
        for address in ["127.0.0.1", "::1"] {
            for payload in [18, 1472, 8972] {
                for segments in [1, 2, 44, 64] {
                    let mut c = Config {
                        send: false,
                        local: address.parse().unwrap(),
                        peer: address.parse().unwrap(),
                        port: 52000,
                        peer_port: 51000,
                        flows: 4,
                        payload,
                        seconds: 1,
                        rate: 0,
                        run: 7,
                        segments,
                    };
                    let mut receivers = flows(&c).unwrap();
                    c.send = true;
                    std::mem::swap(&mut c.port, &mut c.peer_port);
                    let senders = flows(&c).unwrap();
                    let count = c.count();
                    let mut bytes = vec![0xa5; count * payload];
                    let mut storage = vec![0; MAX_BYTES];
                    for (sender, receiver) in senders.iter().zip(&mut receivers) {
                        without_allocations(|| {
                            fill(&mut bytes, payload, c.run, 0);
                            sender.socket.send(&bytes).unwrap();
                            while receive(receiver, &mut storage, &c).unwrap() {}
                        });
                        assert_eq!(receiver.window.unique, count as u64);
                        assert_eq!(
                            (receiver.invalid, receiver.window.too_old, receiver.overflow),
                            (0, 0, 0)
                        );
                        if count > 1 {
                            assert!(receiver.gro > 0);
                        }
                        consume(receiver, &bytes, payload, payload, c.run);
                        assert_eq!(receiver.window.duplicates, count as u64);
                        if payload == 1472 && segments == 1 {
                            for i in 16..payload {
                                bytes[i] ^= 1;
                                consume(receiver, &bytes[..payload], payload, payload, c.run);
                                bytes[i] ^= 1;
                            }
                            assert_eq!(receiver.invalid, (payload - 16) as u64);
                            receiver.invalid = 0;
                        }
                        consume(receiver, &bytes[..payload], payload, payload, c.run + 1);
                        assert_eq!(receiver.wrong_run, 1);
                        assert_eq!(receiver.first_wrong_run, c.run);
                        bytes[0] ^= 1;
                        consume(receiver, &bytes[..payload], payload, payload, c.run);
                        consume(receiver, &bytes[..payload - 1], payload - 1, payload, c.run);
                        consume(receiver, &[], 0, payload, c.run);
                        assert_eq!(receiver.invalid, 4);
                        if payload == 1472 && segments == 1 {
                            storage.resize(MAX_BYTES * 16, 0);
                            let calls = receiver.calls;
                            without_allocations(|| {
                                for i in 0..3 {
                                    fill(&mut bytes, payload, c.run, count as u64 + i);
                                    let length = payload - usize::from(i == 1);
                                    sender.socket.send(&bytes[..length]).unwrap();
                                }
                                assert!(receive(receiver, &mut storage, &c).unwrap());
                            });
                            assert_eq!(receiver.calls, calls + 1);
                            assert_eq!(receiver.window.unique, count as u64 + 2);
                            assert_eq!(receiver.invalid, 5);
                        }
                    }
                }
            }
        }
    }

    #[test]
    #[ignore = "TAP offload delivery probe; run in a disposable container"]
    fn segmented_sends_follow_available_tap_offloads() {
        use std::os::fd::AsFd;
        // SAFETY: the test thread alone enters a disposable network namespace.
        assert_eq!(unsafe { libc::unshare(libc::CLONE_NEWNET) }, 0);
        let tap = unquic::tap::Tap::create(c"uq-prereq", 1500, 1, None).unwrap();
        let queues = tap.into_queues();
        queues[0].set_carrier(true).unwrap();
        for args in [
            vec!["link", "set", "uq-prereq", "up"],
            vec!["addr", "add", "198.18.0.1/24", "dev", "uq-prereq"],
            vec![
                "neigh",
                "add",
                "198.18.0.2",
                "lladdr",
                "02:00:00:00:00:02",
                "nud",
                "permanent",
                "dev",
                "uq-prereq",
            ],
        ] {
            assert!(
                std::process::Command::new("ip")
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let socket = UdpSocket::bind("198.18.0.1:51000").unwrap();
        socket.connect("198.18.0.2:52000").unwrap();
        option(
            &socket,
            libc::IPPROTO_IP,
            libc::IP_MTU_DISCOVER,
            libc::IP_PMTUDISC_DO,
        )
        .unwrap();
        option(&socket, libc::IPPROTO_UDP, libc::UDP_SEGMENT, 1472).unwrap();
        let bytes = [0xa5; 2944];
        let fd = queues[0].as_fd().as_raw_fd();
        let read_segments = || {
            let mut frame = [0u8; 65536];
            let mut count = 0;
            loop {
                // SAFETY: frame provides writable storage for the complete read.
                let n = unsafe { libc::read(fd, frame.as_mut_ptr().cast(), frame.len()) };
                if n < 0 {
                    assert_eq!(io::Error::last_os_error().kind(), io::ErrorKind::WouldBlock);
                    break;
                }
                assert!(n >= 24);
                if frame[22..24] == [8, 0] {
                    assert_eq!(n, 1524);
                    assert_eq!(&frame[52..1524], &bytes[..1472]);
                    count += 1;
                }
            }
            count
        };
        // Kernels may segment in software without TAP checksum offload.
        match socket.send(&bytes) {
            Ok(n) => {
                assert_eq!(n, bytes.len());
                assert_eq!(read_segments(), 2);
            }
            Err(error) => {
                assert_eq!(error.raw_os_error(), Some(libc::EIO));
                assert_eq!(read_segments(), 0);
            }
        }
        // SAFETY: TUNSETOFFLOAD takes an integer; only this test's TAP is changed.
        assert_eq!(
            unsafe { libc::ioctl(fd, libc::TUNSETOFFLOAD, libc::TUN_F_CSUM as libc::c_ulong) },
            0
        );
        assert_eq!(socket.send(&bytes).unwrap(), bytes.len());
        assert_eq!(read_segments(), 2);
        // Linux 6.8 requires both TUN_F_USO4 (0x20) and TUN_F_USO6 (0x40).
        assert_eq!(
            unsafe {
                libc::ioctl(
                    fd,
                    libc::TUNSETOFFLOAD,
                    (libc::TUN_F_CSUM | 0x60) as libc::c_ulong,
                )
            },
            0
        );
        assert_eq!(socket.send(&bytes).unwrap(), bytes.len());
        let mut frame = [0u8; 65536];
        let n = unsafe { libc::read(fd, frame.as_mut_ptr().cast(), frame.len()) };
        assert_eq!(n, 10 + 42 + bytes.len() as isize);
        assert_eq!(frame[1], 5);
        assert_eq!(&frame[52..n as usize], &bytes);
    }
}
