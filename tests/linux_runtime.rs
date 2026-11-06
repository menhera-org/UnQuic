//! End-to-end worker tests; never change the host network namespace.
mod common;
use common::*;
use std::{
    io,
    net::{Ipv6Addr, UdpSocket},
    os::fd::AsRawFd,
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use unquic::{
    config::{Config, Daemon, Endpoint, Peer, Session},
    runtime::{self, Control},
};

use std::{
    alloc::{GlobalAlloc, Layout, System},
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};
struct Allocator;
static COUNTING: AtomicBool = AtomicBool::new(false);
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
static DEALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
#[global_allocator]
static ALLOCATOR: Allocator = Allocator;
// SAFETY: every operation forwards the unchanged allocation contract to System.
unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: layout and returned ownership are forwarded unchanged.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        if COUNTING.load(Ordering::Relaxed) {
            DEALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: pointer and layout describe the caller's live System allocation.
        unsafe { System.dealloc(pointer, layout) }
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: pointer, layout and new size are forwarded unchanged.
        unsafe { System.realloc(pointer, layout, size) }
    }
}

fn config(name: &str, port: u16, remote: u16) -> Config {
    let mut config = Config {
        daemon: Daemon::default(),
        endpoints: vec![Endpoint {
            name: "rx".into(),
            listen_address: Ipv6Addr::UNSPECIFIED,
            listen_port: port,
            scope_id: 0,
            bind_device: None,
        }],
        peers: vec![Peer {
            name: "peer".into(),
            endpoint: "rx".into(),
            address: Some(Ipv6Addr::LOCALHOST),
            peer_port: remote,
            scope_id: 0,
            tx_lanes: 4,
            outer_payload_limit: unquic::BASE_UDP_PAYLOAD,
            path_group: None,
        }],
        sessions: vec![Session {
            name: name.into(),
            peer: "peer".into(),
            cookie: "00112233-4455-4677-8899-aabbccddeeff".parse().unwrap(),
            tap_name: None,
            inner_mtu: 9000,
            mac: None,
            queue_count: 2,
        }],
    };
    let mut second = config.sessions[0].clone();
    second.name = format!("{name}-b");
    second.cookie = unquic::UnquicUuid::from_bytes([2; 16]);
    second.queue_count = 1;
    let mut third = second.clone();
    third.name = format!("{name}-c");
    third.cookie = unquic::UnquicUuid::from_bytes([3; 16]);
    third.peer = "peer2".into();
    let mut peer2 = config.peers[0].clone();
    peer2.name = "peer2".into();
    peer2.tx_lanes = 2;
    peer2.outer_payload_limit = 1452;
    config.peers.push(peer2);
    config.sessions.extend([second, third]);
    config
}

struct Running {
    control: Control,
    thread: Option<JoinHandle<io::Result<()>>>,
}
impl Running {
    fn start(config: Config) -> Self {
        let control = Control::new();
        let worker_control = control.clone();
        let thread = Some(thread::spawn(move || runtime::run(config, worker_control)));
        Self { control, thread }
    }
    fn stop(&mut self) {
        self.control.shutdown();
        self.thread.take().unwrap().join().unwrap().unwrap();
        assert!(self.control.stopped());
        assert!(!self.control.ready());
    }
}
impl Drop for Running {
    fn drop(&mut self) {
        self.control.shutdown();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[track_caller]
fn until(timeout: Duration, mut predicate: impl FnMut() -> bool) {
    let end = Instant::now() + timeout;
    while !predicate() {
        assert!(Instant::now() < end, "runtime test timed out");
        thread::sleep(Duration::from_millis(1));
    }
}

fn storm_frame(bytes: &mut [u8; 1514], direction: usize, sequence: usize) -> usize {
    let length = [60, 64, 128, 1514][sequence % 4];
    for (index, byte) in bytes[..length].iter_mut().enumerate() {
        *byte = (sequence ^ (sequence >> 8) ^ index) as u8;
    }
    bytes[..12].copy_from_slice(&[2, 0, 0, 0, 0, 1, 2, 0, 0, 0, 0, 0]);
    bytes[11] = ((sequence / 32) % 16) as u8;
    bytes[12..14].copy_from_slice(&0x88b5_u16.to_be_bytes());
    bytes[14..18].copy_from_slice(b"UQST");
    bytes[18] = direction as u8;
    bytes[19..27].copy_from_slice(&(sequence as u64).to_be_bytes());
    length
}

fn uso_delivery_allocates_nothing(net: &UdpSocket) {
    let control = UdpSocket::bind("0.0.0.0:0").unwrap();
    let mut req = request(c"uq-right");
    let mut addr = libc::sockaddr {
        sa_family: libc::AF_INET as _,
        sa_data: [0; 14],
    };
    for (dst, src) in addr.sa_data[2..6].iter_mut().zip([198u8, 18, 0, 2]) {
        *dst = src as _;
    }
    req.ifr_ifru.ifru_addr = addr;
    // SAFETY: this changes only the test's TAP in its private network namespace.
    check(unsafe { libc::ioctl(control.as_raw_fd(), libc::SIOCSIFADDR as _, &req) });
    let receiver = UdpSocket::bind("198.18.0.2:9999").unwrap();
    receiver.set_nonblocking(true).unwrap();
    let gro = 1i32;
    // SAFETY: UDP_GRO reads one initialized integer from its exact ABI buffer.
    check(unsafe {
        libc::setsockopt(
            receiver.as_raw_fd(),
            libc::SOL_UDP,
            libc::UDP_GRO,
            (&gro as *const i32).cast(),
            size_of_val(&gro) as _,
        )
    });
    let source = raw_socket(c"uq-left", libc::ETH_P_IP as u16);
    let destination_mac = mac_address(net, c"uq-right");
    let source_mac = mac_address(net, c"uq-left");
    let frames: Vec<_> = (0..512u16)
        .map(|sequence| {
            let mut payload = [0x5a; 1472];
            payload[..2].copy_from_slice(&sequence.to_be_bytes());
            let mut frame = common::udp::frame(false, sequence, &payload);
            frame[..6].copy_from_slice(destination_mac.as_bytes());
            frame[6..12].copy_from_slice(source_mac.as_bytes());
            frame
        })
        .collect();
    let mut input = [0u8; 65536];
    let mut seen = [false; 512];
    let mut coalesced = false;
    ALLOCATIONS.store(0, Ordering::Relaxed);
    DEALLOCATIONS.store(0, Ordering::Relaxed);
    COUNTING.store(true, Ordering::SeqCst);
    for batch in frames.chunks(32) {
        for frame in batch {
            raw_send(&source, frame);
        }
        let mut received = 0;
        until(Duration::from_secs(2), || match receiver.recv(&mut input) {
            Ok(n) => {
                assert_eq!(n % 1472, 0);
                coalesced |= n > 1472;
                for payload in input[..n].chunks(1472) {
                    let sequence = u16::from_be_bytes([payload[0], payload[1]]) as usize;
                    assert!(sequence < seen.len() && !seen[sequence]);
                    assert_eq!(payload, &frames[sequence][42..]);
                    seen[sequence] = true;
                    received += 1;
                }
                received == batch.len()
            }
            Err(e) => {
                assert_eq!(e.kind(), io::ErrorKind::WouldBlock);
                false
            }
        });
    }
    COUNTING.store(false, Ordering::SeqCst);
    assert!(seen.iter().all(|&v| v));
    assert!(coalesced, "the allocation check must exercise USO delivery");
    assert_eq!(ALLOCATIONS.load(Ordering::Relaxed), 0);
    assert_eq!(DEALLOCATIONS.load(Ordering::Relaxed), 0);
    // AF_PACKET's virtio metadata makes the host TAP receive actual USO inputs.
    let batches: Vec<_> = frames
        .chunks(32)
        .map(|batch| {
            let payload: Vec<_> = batch.iter().flat_map(|f| f[42..].iter().copied()).collect();
            let id = u16::from_be_bytes([batch[0][18], batch[0][19]]);
            let mut input = common::udp::frame(false, id, &payload);
            input[..12].copy_from_slice(&batch[0][..12]);
            let mut header = common::udp::partial(&mut input, false);
            header[1] = 5;
            header[4..6].copy_from_slice(&1472u16.to_ne_bytes());
            [header.as_slice(), input.as_slice()].concat()
        })
        .collect();
    let vnet = 1i32;
    // SAFETY: PACKET_VNET_HDR reads one initialized integer.
    check(unsafe {
        libc::setsockopt(
            source.as_raw_fd(),
            libc::SOL_PACKET,
            libc::PACKET_VNET_HDR,
            (&vnet as *const i32).cast(),
            size_of_val(&vnet) as _,
        )
    });
    seen.fill(false);
    ALLOCATIONS.store(0, Ordering::Relaxed);
    DEALLOCATIONS.store(0, Ordering::Relaxed);
    COUNTING.store(true, Ordering::SeqCst);
    for batch in &batches {
        // SAFETY: use the bound IPv4 protocol; the first ten bytes are metadata.
        assert_eq!(
            unsafe { libc::send(source.as_raw_fd(), batch.as_ptr().cast(), batch.len(), 0) },
            batch.len() as isize
        );
        let mut received = 0;
        until(Duration::from_secs(2), || match receiver.recv(&mut input) {
            Ok(n) => {
                assert_eq!(n % 1472, 0);
                for payload in input[..n].chunks(1472) {
                    let sequence = u16::from_be_bytes([payload[0], payload[1]]) as usize;
                    assert!(sequence < seen.len() && !seen[sequence]);
                    assert_eq!(payload, &frames[sequence][42..]);
                    seen[sequence] = true;
                    received += 1;
                }
                received == 32
            }
            Err(e) => {
                assert_eq!(e.kind(), io::ErrorKind::WouldBlock);
                false
            }
        });
    }
    COUNTING.store(false, Ordering::SeqCst);
    assert!(seen.iter().all(|&v| v));
    assert_eq!(ALLOCATIONS.load(Ordering::Relaxed), 0);
    assert_eq!(DEALLOCATIONS.load(Ordering::Relaxed), 0);
}

fn frame_storm() {
    let sockets = [
        raw_socket(c"uq-left", 0x88b5),
        raw_socket(c"uq-right", 0x88b5),
    ];
    let monitor = raw_socket(c"lo", libc::ETH_P_IPV6 as u16);
    for socket in &sockets {
        let enabled = 1_i32;
        // SAFETY: enabled is a readable integer; this changes only the test capture socket.
        check(unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                libc::SOL_PACKET,
                libc::PACKET_IGNORE_OUTGOING,
                (&enabled as *const i32).cast(),
                size_of_val(&enabled) as _,
            )
        });
    }
    const LIMIT: usize = 1 << 24;
    let mut seen = [vec![0_u64; LIMIT / 64], vec![0_u64; LIMIT / 64]];
    let mut sent = [0; 2];
    let mut received = [0; 2];
    let mut output = [0; 1514];
    let mut input = [0; 65_536];
    let mut packed = false;
    let start = Instant::now();
    let send_until = start + Duration::from_secs(10);
    let drain_until = send_until + Duration::from_millis(200);
    ALLOCATIONS.store(0, Ordering::Relaxed);
    DEALLOCATIONS.store(0, Ordering::Relaxed);
    COUNTING.store(true, Ordering::SeqCst);
    while Instant::now() < drain_until {
        let sending = Instant::now() < send_until;
        if sending {
            for direction in 0..2 {
                for _ in 0..64 {
                    assert!(sent[direction] < LIMIT);
                    let length = storm_frame(&mut output, direction, sent[direction]);
                    // SAFETY: output is readable; the bound AF_PACKET socket supplies the interface/protocol.
                    let n = unsafe {
                        libc::send(
                            sockets[direction].as_raw_fd(),
                            output.as_ptr().cast(),
                            length,
                            libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
                        )
                    };
                    if n < 0 {
                        let error = io::Error::last_os_error();
                        assert!(
                            matches!(
                                error.kind(),
                                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                            ) || error.raw_os_error() == Some(libc::ENOBUFS),
                            "{error}"
                        );
                        break;
                    }
                    assert_eq!(n as usize, length);
                    sent[direction] += 1;
                }
            }
        }
        for direction in 0..2 {
            for _ in 0..256 {
                let Some(length) = raw_read(&sockets[1 - direction], &mut input) else {
                    break;
                };
                assert!(length >= 27);
                assert_eq!(&input[14..18], b"UQST");
                assert_eq!(input[18], direction as u8);
                let sequence = u64::from_be_bytes(input[19..27].try_into().unwrap()) as usize;
                assert!(sequence < sent[direction]);
                let expected = storm_frame(&mut output, direction, sequence);
                assert_eq!(length, expected);
                assert_eq!(&input[..length], &output[..expected]);
                let word = &mut seen[direction][sequence / 64];
                let bit = 1_u64 << (sequence % 64);
                assert_eq!(*word & bit, 0, "duplicate frame");
                *word |= bit;
                received[direction] += 1;
            }
        }
        if !packed {
            for _ in 0..128 {
                let Some(length) = raw_read(&monitor, &mut input) else {
                    break;
                };
                // Capture can contain a GSO aggregate: validate its first logical UDP payload.
                if length < 110
                    || input[20] != 17
                    || !matches!(u16::from_be_bytes([input[56], input[57]]), 40100 | 40101)
                {
                    continue;
                }
                let end = 110 + usize::from(u16::from_be_bytes([input[100], input[101]]));
                if end <= length
                    && let Ok(packet) = unquic::wire::Datagram::decode(&input[62..end])
                {
                    packed |= packet
                        .records()
                        .filter(|record| matches!(record, unquic::wire::Record::Data { .. }))
                        .count()
                        > 1;
                }
            }
        }
        if !sending {
            thread::sleep(Duration::from_millis(1));
        }
    }
    COUNTING.store(false, Ordering::SeqCst);
    assert_eq!(ALLOCATIONS.load(Ordering::Relaxed), 0);
    assert_eq!(DEALLOCATIONS.load(Ordering::Relaxed), 0);
    assert!(packed, "no multi-frame DATA packet observed");
    assert!(received.iter().all(|count| *count > 1000));
    eprintln!(
        "10 s frame storm: sent={sent:?}, delivered={received:?}; no corruption or duplicates"
    );
}

#[test]
#[ignore = "requires disposable container with tun and network namespace capabilities"]
fn workers_forward_both_directions_and_reconnect_after_restart() {
    // SAFETY: this test thread and its subsequently spawned workers use a fresh namespace.
    check(unsafe { libc::unshare(libc::CLONE_NEWNET) });
    let net = UdpSocket::bind("[::]:0").unwrap();
    up(&net, c"lo");
    let mut left_config = config("uq-left", 40100, 40101);
    left_config.peers[0].address = None;
    left_config.peers[1].outer_payload_limit = unquic::socket::MAX_UDP_PAYLOAD;
    left_config.endpoints.push(Endpoint {
        name: "unavailable".into(),
        listen_address: "fd00::ffff".parse().unwrap(),
        listen_port: 40120,
        scope_id: 0,
        bind_device: None,
    });
    let mut left = Running::start(left_config);
    let mut right = Running::start(config("uq-right", 40101, 40100));
    until(Duration::from_secs(5), || {
        left.control.ready() && right.control.ready()
    });
    assert_eq!(flags(&net, c"uq-left") & libc::IFF_UP as i16, 0);
    assert_eq!(flags(&net, c"uq-right") & libc::IFF_UP as i16, 0);
    up(&net, c"uq-left");
    up(&net, c"uq-right");
    for name in [c"uq-left-b", c"uq-left-c", c"uq-right-b", c"uq-right-c"] {
        up(&net, name);
    }
    assert!(!left.control.session(0).unwrap().carrier);
    left.control
        .supply_peer_address(0, Ipv6Addr::LOCALHOST, 0)
        .unwrap();
    until(Duration::from_secs(5), || {
        (0..3).all(|session| {
            left.control.session(session).unwrap().carrier
                && right.control.session(session).unwrap().carrier
        })
    });
    uso_delivery_allocates_nothing(&net);
    until(Duration::from_secs(2), || {
        (0..2)
            .map(|worker| left.control.worker_counters(worker).unwrap().tap_gso_frames)
            .sum::<u64>()
            >= 512
    });
    let a = raw_socket(c"uq-left", libc::ETH_P_ALL as u16);
    let b = raw_socket(c"uq-right", libc::ETH_P_ALL as u16);
    for fd in [&a, &b] {
        let enabled = 1_i32;
        // SAFETY: enabled is readable for the supplied option length.
        check(unsafe {
            libc::setsockopt(
                fd.as_raw_fd(),
                libc::SOL_PACKET,
                libc::PACKET_AUXDATA,
                (&enabled as *const i32).cast(),
                size_of_val(&enabled) as _,
            )
        });
    }
    add_vlan(&net, c"uq-left", 7);
    add_vlan(&net, c"uq-right", 7);
    up(&net, c"uq-left.7");
    up(&net, c"uq-right.7");
    let a_vlan = raw_socket(c"uq-left.7", libc::ETH_P_ALL as u16);
    let b_vlan = raw_socket(c"uq-right.7", libc::ETH_P_ALL as u16);
    let mut capture = [0; 10_000];
    for length in [60, 1514, 1518, 9022] {
        let mut frame = vec![0x5a; length];
        frame[..6].copy_from_slice(&[2, 0, 0, 0, 0, 1]);
        frame[6..12].copy_from_slice(&[2, 0, 0, 0, 0, 2]);
        frame[12..14].copy_from_slice(&0x88b5_u16.to_be_bytes());
        if length == 1518 {
            frame[12..18].copy_from_slice(&[0x81, 0, 0, 7, 0x88, 0xb5]);
        }
        if length == 9022 {
            frame[12..22].copy_from_slice(&[0x81, 0, 0, 7, 0x81, 0, 0, 8, 0x88, 0xb5]);
        }
        let outgoing = if length == 9022 {
            [&frame[..12], &frame[16..]].concat()
        } else {
            frame.clone()
        };
        let sources = if length == 9022 {
            [&a_vlan, &b_vlan]
        } else {
            [&a, &b]
        };
        ALLOCATIONS.store(0, Ordering::Relaxed);
        DEALLOCATIONS.store(0, Ordering::Relaxed);
        COUNTING.store(true, Ordering::SeqCst);
        for (source, target) in [(sources[0], &b), (sources[1], &a)] {
            while capture_frame(target, &mut capture).is_some() {}
            raw_send(source, &outgoing);
            until(Duration::from_secs(2), || {
                let Some(n) = capture_frame(target, &mut capture) else {
                    return false;
                };
                capture[..n] == frame
            });
        }
        COUNTING.store(false, Ordering::SeqCst);
        assert_eq!(ALLOCATIONS.load(Ordering::Relaxed), 0);
        assert_eq!(DEALLOCATIONS.load(Ordering::Relaxed), 0);
    }
    frame_storm();
    for control in [&left.control, &right.control] {
        let mut ring_frames = 0;
        let mut fallbacks = 0;
        for worker in 0..2 {
            let counters = control.worker_counters(worker).unwrap();
            assert_eq!(counters.queue_drops, 0);
            ring_frames += counters.tap_ring_frames;
            fallbacks += counters.tap_read_fallbacks;
        }
        if fallbacks == 0 {
            assert!(
                ring_frames > 0,
                "the allocation check must exercise available read rings"
            );
        }
        println!("TAP ring frames={ring_frames} fallbacks={fallbacks}");
    }
    // The earlier capture sockets also accumulated storm traffic; free space before reuse.
    for socket in [&a, &b] {
        while capture_frame(socket, &mut capture).is_some() {}
    }
    for (left_name, right_name) in [(c"uq-left-b", c"uq-right-b"), (c"uq-left-c", c"uq-right-c")] {
        let source = raw_socket(left_name, 0x88b5);
        let target = raw_socket(right_name, 0x88b5);
        let mut frame = [0x5a; 9014];
        frame[12..14].copy_from_slice(&0x88b5_u16.to_be_bytes());
        for length in [60, 1514, 9014] {
            let frame = &frame[..length];
            ALLOCATIONS.store(0, Ordering::Relaxed);
            DEALLOCATIONS.store(0, Ordering::Relaxed);
            COUNTING.store(true, Ordering::SeqCst);
            raw_send(&source, frame);
            until(Duration::from_secs(2), || {
                raw_read(&target, &mut capture).is_some_and(|n| &capture[..n] == frame)
            });
            raw_send(&target, frame);
            until(Duration::from_secs(2), || {
                raw_read(&source, &mut capture).is_some_and(|n| &capture[..n] == frame)
            });
            COUNTING.store(false, Ordering::SeqCst);
            assert_eq!(ALLOCATIONS.load(Ordering::Relaxed), 0);
            assert_eq!(DEALLOCATIONS.load(Ordering::Relaxed), 0);
        }
    }
    until(Duration::from_secs(2), || {
        [&left.control, &right.control].iter().all(|control| {
            (0..3).all(|session| {
                control
                    .session_counters(session)
                    .unwrap()
                    .delivered_frame_bytes
                    >= 60
            })
        })
    });
    for (control, configured_mtu) in [(&left.control, 9000), (&right.control, 1500)] {
        // A lost startup probe retains the RFC-oriented 16-second initial timeout.
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let totals = control.session_counters(2).unwrap();
            if totals.path_mtu_bytes == configured_mtu && totals.path_mtu_state == 3 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "MTU {configured_mtu}: {totals:?}; lanes={:?}/{:?}",
                control.transmit_lane_counters(2, 1),
                control.transmit_lane_counters(2, 2)
            );
            thread::sleep(Duration::from_millis(1));
        }
        for session in 0..3 {
            let totals = control.session_counters(session).unwrap();
            assert!(totals.tx_outer_bytes > 60);
            assert!(totals.rx_outer_bytes > 60);
            let mtu = if session == 2 { configured_mtu } else { 1280 };
            let state = if session == 2 { 3 } else { 1 };
            assert_eq!(totals.path_mtu_bytes, mtu);
            assert_eq!(totals.path_mtu_state, state);
            let lanes = if session == 2 { 2 } else { 4 };
            assert!((1..=lanes).any(|lane| {
                control
                    .receive_lane_counters(session, lane)
                    .is_some_and(|s| s.counters.delivered_frame_bytes >= 60)
            }));
            for lane in 1..=lanes {
                let snapshot = control.transmit_lane_counters(session, lane).unwrap();
                assert_eq!(
                    snapshot.generation,
                    control.session(session).unwrap().generation
                );
                assert_eq!(snapshot.counters.path_mtu_bytes, mtu);
                assert_eq!(snapshot.counters.path_mtu_state, state);
            }
        }
        assert!(control.session_counters(3).is_none());
        assert!(control.peer_counters(2).is_none());
        assert!(control.transmit_lane_counters(0, 0).is_none());
        assert!(control.receive_lane_counters(0, 0).is_none());
    }
    let before = left.control.session_counters(2).unwrap().tx_outer_bytes;
    ALLOCATIONS.store(0, Ordering::Relaxed);
    DEALLOCATIONS.store(0, Ordering::Relaxed);
    COUNTING.store(true, Ordering::SeqCst);
    thread::sleep(Duration::from_secs(2));
    COUNTING.store(false, Ordering::SeqCst);
    assert_eq!(ALLOCATIONS.load(Ordering::Relaxed), 0);
    assert_eq!(DEALLOCATIONS.load(Ordering::Relaxed), 0);
    assert!(left.control.session_counters(2).unwrap().tx_outer_bytes - before >= 9000);
    let old_generation = left.control.session(0).unwrap().generation;
    left.control
        .supply_peer_address(0, "fd99::1".parse().unwrap(), 0)
        .unwrap();
    thread::sleep(Duration::from_millis(150));
    assert_eq!(left.control.session(0).unwrap().generation, old_generation);
    assert!(left.control.session(0).unwrap().carrier);
    let frame = [0x5a; 60];
    raw_send(&a, &frame);
    until(Duration::from_secs(2), || {
        capture_frame(&b, &mut capture).is_some_and(|n| capture[..n] == frame)
    });
    let candidate = "fd00::2".parse().unwrap();
    add_ipv6(&net, c"lo", candidate);
    until(Duration::from_secs(2), || {
        match left.control.supply_peer_address(0, candidate, 0) {
            Ok(()) => true,
            Err(error) => {
                assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
                false
            }
        }
    });
    until(Duration::from_secs(5), || {
        left.control.session(0).unwrap().generation > old_generation
    });
    assert!(left.control.session(0).unwrap().carrier);
    raw_send(&a, &frame);
    until(Duration::from_secs(2), || {
        capture_frame(&b, &mut capture).is_some_and(|n| capture[..n] == frame)
    });
    let migrated = left.control.session(0).unwrap().generation;
    for lane in 1..=4 {
        assert_eq!(
            left.control
                .transmit_lane_counters(0, lane)
                .unwrap()
                .generation,
            migrated
        );
    }
    left.control
        .supply_peer_address(0, Ipv6Addr::LOCALHOST, 0)
        .unwrap();
    until(Duration::from_secs(5), || {
        left.control.session(0).unwrap().generation > migrated
    });
    let restored = left.control.session(0).unwrap().generation;
    left.control
        .supply_peer_address(0, Ipv6Addr::LOCALHOST, 0)
        .unwrap();
    thread::sleep(Duration::from_millis(150));
    assert_eq!(left.control.session(0).unwrap().generation, restored);
    right.stop();
    until(Duration::from_secs(32), || {
        !left.control.session(0).unwrap().carrier
    });
    let mut replacement = Running::start(config("uq-right", 40101, 40100));
    until(Duration::from_secs(5), || replacement.control.ready());
    up(&net, c"uq-right");
    until(Duration::from_secs(5), || {
        left.control.session(0).unwrap().carrier && replacement.control.session(0).unwrap().carrier
    });
    replacement.stop();
    left.stop();
    for control in [&left.control, &right.control] {
        for (peer, sessions) in [(0, &[0, 1][..]), (1, &[2][..])] {
            let totals = control.peer_counters(peer).unwrap();
            let delivered: u64 = sessions
                .iter()
                .map(|&s| control.session_counters(s).unwrap().delivered_frame_bytes)
                .sum();
            assert_eq!(totals.delivered_frame_bytes, delivered);
            assert!(delivered >= 60);
        }
    }

    let mut unavailable = config("uq-wait", 40110, 40111);
    unavailable.endpoints[0].listen_address = "fd00::1".parse().unwrap();
    unavailable.peers[0].address = None;
    let mut waiting = Running::start(unavailable);
    until(Duration::from_secs(5), || waiting.control.ready());
    until(Duration::from_secs(2), || {
        waiting.control.worker_counters(0).unwrap().socket_errors != 0
    });
    assert!(waiting.control.ready());
    assert!(!waiting.control.session(0).unwrap().carrier);
    assert_eq!(flags(&net, c"uq-wait") & libc::IFF_UP as i16, 0);
    waiting.stop();
}
