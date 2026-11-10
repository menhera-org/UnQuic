//! End-to-end worker tests; never change the host network namespace.
mod common;
use common::runtime::Running;
use common::*;
use std::{
    io,
    net::{Ipv6Addr, UdpSocket},
    os::fd::AsRawFd,
    thread,
    time::{Duration, Instant},
};
use unquic::config::{Config, Daemon, Endpoint, Peer, Session};

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
        // Three sessions retain independent payloads, histories and recovery state.
        daemon: Daemon {
            memory_limit: 512 * 1024 * 1024,
            ..Daemon::default()
        },
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

fn uso_delivery_allocates_nothing(
    net: &UdpSocket,
    left: &unquic::runtime::Control,
    right: &unquic::runtime::Control,
) {
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
        let wait = Instant::now();
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
                assert!(
                    wait.elapsed() < Duration::from_millis(1900),
                    "delivered {received}/{}: left {:?}, right {:?}",
                    batch.len(),
                    left.session_counters(0),
                    right.session_counters(0)
                );
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

fn tso_delivery_allocates_nothing(net: &UdpSocket) {
    let source = raw_socket(c"uq-left", libc::ETH_P_IP as u16);
    let receiver = raw_socket(c"uq-right", libc::ETH_P_IP as u16);
    for fd in [&source, &receiver] {
        let vnet = 1i32;
        // SAFETY: PACKET_VNET_HDR reads one initialized integer.
        check(unsafe {
            libc::setsockopt(
                fd.as_raw_fd(),
                libc::SOL_PACKET,
                libc::PACKET_VNET_HDR,
                (&vnet as *const i32).cast(),
                size_of_val(&vnet) as _,
            )
        });
    }
    let destination_mac = mac_address(net, c"uq-right");
    let source_mac = mac_address(net, c"uq-left");
    let frames: Vec<_> = (0..512u16)
        .map(|id| {
            let mut payload = [0x5a; 1460];
            payload[..2].copy_from_slice(&id.to_be_bytes());
            let mut frame =
                common::tcp::frame(false, id, u32::from(id) * 1460, 0x10, &[], &payload);
            frame[..6].copy_from_slice(destination_mac.as_bytes());
            frame[6..12].copy_from_slice(source_mac.as_bytes());
            frame
        })
        .collect();
    let batches: Vec<_> = frames
        .chunks(32)
        .enumerate()
        .map(|(batch, frames)| {
            let payload: Vec<_> = frames
                .iter()
                .flat_map(|f| f[54..].iter().copied())
                .collect();
            let id = (batch * 32) as u16;
            let mut frame =
                common::tcp::frame(false, id, u32::from(id) * 1460, 0x10, &[], &payload);
            frame[..12].copy_from_slice(&frames[0][..12]);
            let header = common::tcp::partial(&mut frame, false, 1460);
            [header.as_slice(), frame.as_slice()].concat()
        })
        .collect();
    let mut input = [0u8; 65536];
    let mut seen = [false; 512];
    let mut coalesced = false;
    ALLOCATIONS.store(0, Ordering::Relaxed);
    DEALLOCATIONS.store(0, Ordering::Relaxed);
    COUNTING.store(true, Ordering::SeqCst);
    for batch in &batches {
        // SAFETY: send reads the complete virtio header and frame on the bound socket.
        assert_eq!(
            unsafe { libc::send(source.as_raw_fd(), batch.as_ptr().cast(), batch.len(), 0) },
            batch.len() as isize
        );
        let mut received = 0;
        until(Duration::from_secs(2), || {
            let Some(n) = raw_read(&receiver, &mut input) else {
                return false;
            };
            if n < 64 || input[16..22] != source_mac.as_bytes()[..] {
                return false;
            }
            assert_eq!((n - 64) % 1460, 0);
            if n > 64 + 1460 {
                coalesced = true;
                assert_eq!(input[1], 1);
                assert_eq!(u16::from_ne_bytes([input[4], input[5]]), 1460);
            }
            let sequence = u32::from_be_bytes(input[48..52].try_into().unwrap());
            for (index, payload) in input[64..n].chunks(1460).enumerate() {
                let id = u16::from_be_bytes([payload[0], payload[1]]) as usize;
                assert!(id < seen.len() && !seen[id]);
                assert_eq!(sequence + index as u32 * 1460, id as u32 * 1460);
                assert_eq!(payload, &frames[id][54..]);
                seen[id] = true;
                received += 1;
            }
            received == 32
        });
    }
    COUNTING.store(false, Ordering::SeqCst);
    assert!(seen.iter().all(|&v| v));
    assert!(coalesced, "the allocation check must exercise TSO delivery");
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
    uso_delivery_allocates_nothing(&net, &left.control, &right.control);
    tso_delivery_allocates_nothing(&net);
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
        let mut queue_drops = 0;
        for worker in 0..2 {
            let counters = control.worker_counters(worker).unwrap();
            queue_drops += counters.queue_drops;
            ring_frames += counters.tap_ring_frames;
            fallbacks += counters.tap_read_fallbacks;
        }
        if fallbacks == 0 {
            assert!(
                ring_frames > 0,
                "the allocation check must exercise available read rings"
            );
        }
        println!("TAP ring frames={ring_frames} fallbacks={fallbacks} queue drops={queue_drops}");
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
    // Offset ten-second attempts can require two expiries before tags converge.
    let migration_wait = Duration::from_secs(30);
    let old_generation = left.control.session(0).unwrap().generation;
    let old_remote_generation = right.control.session(0).unwrap().generation;
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
    until(migration_wait, || {
        left.control.session(0).unwrap().generation > old_generation
            && right.control.session(0).unwrap().generation > old_remote_generation
    });
    assert!(left.control.session(0).unwrap().carrier);
    let frame_started = Instant::now();
    raw_send(&a, &frame);
    until(Duration::from_secs(2), || {
        let delivered = capture_frame(&b, &mut capture).is_some_and(|n| capture[..n] == frame);
        assert!(
            delivered || frame_started.elapsed() < Duration::from_millis(1900),
            "post-migration frame: left {:?}, right {:?}, left TX {:?}, right RX {:?}",
            left.control.session_counters(0),
            right.control.session_counters(0),
            [1, 2, 3, 4].map(|lane| left.control.transmit_lane_counters(0, lane)),
            [1, 2, 3, 4].map(|lane| right.control.receive_lane_counters(0, lane))
        );
        delivered
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
    until(migration_wait, || {
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

#[test]
#[ignore = "requires isolated container, tun, iproute2 and network capabilities"]
fn foreign_rename_and_namespace_move_preserve_running_transport() {
    // SAFETY: all links, sockets and child threads belong to disposable namespaces.
    check(unsafe { libc::unshare(libc::CLONE_NEWNET) });
    let original = std::fs::File::open("/proc/thread-self/ns/net").unwrap();
    let destination = namespace();
    let control = UdpSocket::bind("[::]:0").unwrap();
    up(&control, c"lo");
    let single = |name, port, remote| {
        let mut config = config(name, port, remote);
        config.sessions.truncate(1);
        config.peers.truncate(1);
        config
    };
    let mut left = Running::start(single("uq-move", 40100, 40101));
    let mut right = Running::start(single("uq-stay", 40101, 40100));
    until(Duration::from_secs(5), || {
        left.control.ready() && right.control.ready()
    });
    up(&control, c"uq-move");
    up(&control, c"uq-stay");
    until(Duration::from_secs(5), || {
        left.control.session(0).unwrap().carrier && right.control.session(0).unwrap().carrier
    });
    let mac = mac_address(&control, c"uq-move");
    // SAFETY: the name is NUL-terminated and the device is live.
    let original_index = unsafe { libc::if_nametoindex(c"uq-move".as_ptr()) };
    assert_ne!(original_index, 0);
    let remote = raw_socket(c"uq-stay", 0x88b5);

    // Occupy the original index: an ifindex is namespace-local, not durable identity.
    enter(&destination);
    for index in 0..original_index {
        ip(&["link", "add", &format!("occupy{index}"), "type", "dummy"]);
    }
    enter(&original);
    let ns_path = format!(
        "/proc/{}/fd/{}",
        std::process::id(),
        destination.as_raw_fd()
    );
    ip(&["link", "set", "uq-move", "down"]);
    ip(&["link", "set", "uq-move", "name", "uq-renamed"]);
    ip(&["link", "set", "uq-renamed", "netns", &ns_path]);
    ip(&["link", "add", "uq-move", "type", "dummy"]);
    let unrelated_mac = mac_address(&control, c"uq-move");
    enter(&destination);
    let moved_control = UdpSocket::bind("[::]:0").unwrap();
    assert_eq!(mac_address(&moved_control, c"uq-renamed"), mac);
    // SAFETY: the name is NUL-terminated in this namespace.
    assert_ne!(
        unsafe { libc::if_nametoindex(c"uq-renamed".as_ptr()) },
        original_index
    );
    up(&moved_control, c"uq-renamed");
    let moved = raw_socket(c"uq-renamed", 0x88b5);
    let forward = |phase: &str,
                   name: &std::ffi::CStr,
                   moved: &std::os::fd::OwnedFd,
                   remote: &std::os::fd::OwnedFd,
                   peer: &unquic::runtime::Control| {
        let running = (libc::IFF_UP | libc::IFF_RUNNING) as i16;
        until(Duration::from_secs(3), || {
            flags(&moved_control, name) & running == running
                && flags(&control, c"uq-stay") & running == running
        });
        // Carrier confirms the handshake; failed PMTU still blocks DATA until a probe succeeds.
        for control in [&left.control, peer] {
            let deadline = Instant::now() + Duration::from_secs(5);
            for lane in 1..=4 {
                loop {
                    let snapshot = control.transmit_lane_counters(0, lane);
                    if snapshot.is_some_and(|s| {
                        s.generation == control.session(0).unwrap().generation
                            && s.counters.path_mtu_bytes >= 1280
                            && s.counters.path_mtu_state != 4
                    }) {
                        break;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "{phase}, lane {lane}: {snapshot:?}"
                    );
                    thread::sleep(Duration::from_millis(1));
                }
            }
        }
        if phase == "reconnect" {
            // Connection replacement preserves the path group's backed-off PTO.
            let started = Instant::now();
            until(Duration::from_secs(90), || {
                [&left.control, peer].into_iter().all(|control| {
                    let generation = control.session(0).unwrap().generation;
                    (1..=4).any(|lane| {
                        control
                            .transmit_lane_counters(0, lane)
                            .is_some_and(|s| s.generation == generation && s.counters.rtt_ns != 0)
                    })
                })
            });
            eprintln!("reconnect ACK readiness after {:?}", started.elapsed());
        }
        let mut frame = [0x5a; 1514];
        frame[..14].copy_from_slice(&[2, 0, 0, 0, 0, 1, 2, 0, 0, 0, 0, 2, 0x88, 0xb5]);
        let mut capture = [0; 2000];
        for (direction, (source, target)) in
            [(moved, remote), (remote, moved)].into_iter().enumerate()
        {
            for length in [60, 1514] {
                while raw_read(target, &mut capture).is_some() {}
                let before = [left.control.session_counters(0), peer.session_counters(0)];
                raw_send(source, &frame[..length]);
                let sent = Instant::now();
                loop {
                    if raw_read(target, &mut capture)
                        .is_some_and(|n| n == length && capture[..n] == frame[..length])
                    {
                        break;
                    }
                    assert!(
                        sent.elapsed() < Duration::from_secs(3),
                        "{phase}, direction {direction}, length {length}: flags {:?}, states {:?}, before {before:?}, after {:?}",
                        [flags(&moved_control, name), flags(&control, c"uq-stay")],
                        [left.control.session(0), peer.session(0)],
                        [left.control.session_counters(0), peer.session_counters(0)]
                    );
                    thread::sleep(Duration::from_millis(1));
                }
            }
        }
    };
    forward("move", c"uq-renamed", &moved, &remote, &right.control);
    // A second external rename must not disturb the already moved queues.
    ip(&["link", "set", "uq-renamed", "down"]);
    ip(&["link", "set", "uq-renamed", "name", "uq-again"]);
    up(&moved_control, c"uq-again");
    // AF_PACKET reports ENETDOWN after a link-down event; reopen the test injector.
    let moved = raw_socket(c"uq-again", 0x88b5);
    forward("rename", c"uq-again", &moved, &remote, &right.control);

    // Carrier operations also use the preserved TAP fd, from the original namespace.
    right.stop();
    until(Duration::from_secs(35), || {
        !left.control.session(0).unwrap().carrier
    });
    // IFF_RUNNING follows deferred linkwatch work after TUNSETCARRIER returns.
    until(Duration::from_secs(3), || {
        flags(&moved_control, c"uq-again") & libc::IFF_RUNNING as i16 == 0
    });
    assert_eq!(mac_address(&moved_control, c"uq-again"), mac);
    enter(&original);
    assert_eq!(mac_address(&control, c"uq-move"), unrelated_mac);
    assert_eq!(flags(&control, c"uq-move") & libc::IFF_UP as i16, 0);
    let mut replacement = Running::start(single("uq-stay", 40101, 40100));
    until(Duration::from_secs(5), || replacement.control.ready());
    up(&control, c"uq-stay");
    until(Duration::from_secs(5), || {
        left.control.session(0).unwrap().carrier && replacement.control.session(0).unwrap().carrier
    });

    // The old peer's packet socket was bound to its deleted test TAP.
    let remote = raw_socket(c"uq-stay", 0x88b5);
    forward(
        "reconnect",
        c"uq-again",
        &moved,
        &remote,
        &replacement.control,
    );
    left.stop();
    replacement.stop();
}

#[test]
#[ignore = "requires disposable container with tun and network namespace capabilities"]
fn outer_loss_repairs_twice_and_late_original_never_duplicates_tap_delivery() {
    struct Relay {
        sockets: [UdpSocket; 2],
        target: [Option<u64>; 2],
        drops: [usize; 2],
        original: [[u8; 9000]; 2],
        lengths: [usize; 2],
    }
    impl Relay {
        fn pump(&mut self) {
            let mut bytes = [0; 9000];
            for direction in 0..2 {
                for _ in 0..128 {
                    let n = match self.sockets[direction].recv(&mut bytes) {
                        Ok(n) => n,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                        Err(error) => panic!("{error}"),
                    };
                    let packet = unquic::wire::Datagram::decode(&bytes[..n]).unwrap();
                    // The relay also translates the port advertised by the handshake.
                    let translated = packet.records().find_map(|record| match record {
                        unquic::wire::Record::Hello {
                            initiator,
                            mut params,
                        } => {
                            params.port = if direction == 0 { 41203 } else { 41202 };
                            Some(unquic::wire::Record::Hello { initiator, params })
                        }
                        unquic::wire::Record::Challenge {
                            initiator,
                            responder,
                            mut params,
                        } => {
                            params.port = if direction == 0 { 41203 } else { 41202 };
                            Some(unquic::wire::Record::Challenge {
                                initiator,
                                responder,
                                params,
                            })
                        }
                        _ => None,
                    });
                    let header = packet.header();
                    let prefix = packet.records().find_map(|record| match record {
                        unquic::wire::Record::Data {
                            frame_id,
                            offset: 0,
                            bytes,
                            ..
                        } if bytes.get(14..22) == Some(&b"UQREPAIR"[..])
                            && bytes.get(22) == Some(&200) =>
                        {
                            Some(frame_id)
                        }
                        _ => None,
                    });
                    if let Some(id) = prefix {
                        let target = self.target[direction].get_or_insert(id);
                        assert_eq!(*target, id);
                        if self.drops[direction] < 2 {
                            if self.drops[direction] == 0 {
                                self.original[direction][..n].copy_from_slice(&bytes[..n]);
                                self.lengths[direction] = n;
                            }
                            self.drops[direction] += 1;
                            continue;
                        }
                    }
                    let n = if let Some(record) = translated {
                        unquic::wire::encode_datagram(header, &[record], &mut bytes).unwrap()
                    } else {
                        n
                    };
                    let port = if direction == 0 { 41201 } else { 41200 };
                    assert_eq!(
                        self.sockets[direction]
                            .send_to(&bytes[..n], (Ipv6Addr::LOCALHOST, port))
                            .unwrap(),
                        n
                    );
                }
            }
        }

        fn replay_originals(&self) {
            for direction in 0..2 {
                let n = self.lengths[direction];
                assert!(n != 0);
                let port = if direction == 0 { 41201 } else { 41200 };
                assert_eq!(
                    self.sockets[direction]
                        .send_to(&self.original[direction][..n], (Ipv6Addr::LOCALHOST, port))
                        .unwrap(),
                    n
                );
            }
        }
    }

    // SAFETY: only this test thread and its newly spawned daemons enter the namespace.
    check(unsafe { libc::unshare(libc::CLONE_NEWNET) });
    let net = UdpSocket::bind("[::]:0").unwrap();
    up(&net, c"lo");
    let mut relay = Relay {
        sockets: [
            UdpSocket::bind("[::1]:41202").unwrap(),
            UdpSocket::bind("[::1]:41203").unwrap(),
        ],
        target: [None; 2],
        drops: [0; 2],
        original: [[0; 9000]; 2],
        lengths: [0; 2],
    };
    for socket in &relay.sockets {
        socket.set_nonblocking(true).unwrap();
    }
    let single = |name, listen, remote| {
        let mut config = config(name, listen, remote);
        config.sessions.truncate(1);
        config.peers.truncate(1);
        config.daemon.memory_limit = Daemon::default().memory_limit;
        config.sessions[0].queue_count = 1;
        config.peers[0].tx_lanes = 1;
        config
    };
    let mut left = Running::start(single("uq-repair-left", 41200, 41202));
    let mut right = Running::start(single("uq-repair-right", 41201, 41203));
    until(Duration::from_secs(5), || {
        relay.pump();
        left.control.ready() && right.control.ready()
    });
    up(&net, c"uq-repair-left");
    up(&net, c"uq-repair-right");
    until(Duration::from_secs(5), || {
        relay.pump();
        left.control.session(0).unwrap().carrier && right.control.session(0).unwrap().carrier
    });
    let sockets = [
        raw_socket(c"uq-repair-left", 0x88b5),
        raw_socket(c"uq-repair-right", 0x88b5),
    ];
    let mut frames = [[0x5a; 1514]; 2];
    for (direction, frame) in frames.iter_mut().enumerate() {
        frame[..12].copy_from_slice(&[2, 0, 0, 0, 0, 1, 2, 0, 0, 0, 0, 2]);
        frame[12..14].copy_from_slice(&0x88b5_u16.to_be_bytes());
        frame[14..22].copy_from_slice(b"UQREPAIR");
        frame[22] = 0;
        frame[23] = direction as u8;
        for _ in 0..4 {
            raw_send(&sockets[direction], frame);
        }
    }
    // Establish measured RTT before the two losses; initial RTT is intentionally 333 ms.
    until(Duration::from_secs(3), || {
        relay.pump();
        [&left.control, &right.control].iter().all(|control| {
            let counters = control.worker_counters(0).unwrap();
            counters.delivered_frame_bytes >= 4 * 1514 && counters.bytes_in_flight == 0
        })
    });
    let mut input = [0; 9022];
    for socket in &sockets {
        while raw_read(socket, &mut input).is_some() {}
    }
    for frame in &mut frames {
        frame[22] = 200;
    }
    let mut seen = [0; 2];
    let mut fillers = frames;
    for frame in &mut fillers {
        frame[22] = 1;
    }
    ALLOCATIONS.store(0, Ordering::Relaxed);
    DEALLOCATIONS.store(0, Ordering::Relaxed);
    COUNTING.store(true, Ordering::SeqCst);
    for direction in 0..2 {
        raw_send(&sockets[direction], &frames[direction]);
    }
    let mut rounds = 0;
    until(Duration::from_secs(2), || {
        // Later packet numbers drive loss detection even if a repair has no RTT sample yet.
        if rounds < 64 {
            for direction in 0..2 {
                raw_send(&sockets[direction], &fillers[direction]);
            }
            rounds += 1;
        }
        relay.pump();
        for direction in 0..2 {
            while let Some(n) = raw_read(&sockets[1 - direction], &mut input) {
                if input.get(22) == Some(&200) {
                    assert_eq!(&input[..n], &frames[direction]);
                    seen[direction] += 1;
                    assert_eq!(seen[direction], 1, "duplicate repaired frame");
                }
            }
        }
        seen == [1, 1]
    });
    relay.replay_originals();
    let end = Instant::now() + Duration::from_millis(50);
    while Instant::now() < end {
        relay.pump();
        for socket in &sockets {
            while raw_read(socket, &mut input).is_some() {
                assert_ne!(input[22], 200, "late original duplicated repaired frame");
            }
        }
        thread::sleep(Duration::from_millis(1));
    }
    COUNTING.store(false, Ordering::SeqCst);
    assert_eq!(relay.drops, [2, 2]);
    assert_eq!(ALLOCATIONS.load(Ordering::Relaxed), 0);
    assert_eq!(DEALLOCATIONS.load(Ordering::Relaxed), 0);
    left.stop();
    right.stop();
}

#[test]
#[ignore = "requires disposable container with tun, iproute2 and network capabilities"]
fn bridge_vlan_qinq_macvlan_and_non_ip_frames_cross_unchanged() {
    // SAFETY: only this test thread and its runtimes enter the namespace.
    check(unsafe { libc::unshare(libc::CLONE_NEWNET) });
    let net = UdpSocket::bind("[::]:0").unwrap();
    up(&net, c"lo");
    let single = |name, listen, remote| {
        let mut config = config(name, listen, remote);
        config.sessions.truncate(1);
        config.peers.truncate(1);
        config.daemon.memory_limit = Daemon::default().memory_limit;
        config.sessions[0].queue_count = 1;
        config.peers[0].tx_lanes = 1;
        config
    };
    let mut left = Running::start(single("uqc-l", 41400, 41401));
    let mut right = Running::start(single("uqc-r", 41401, 41400));
    until(Duration::from_secs(5), || {
        left.control.ready() && right.control.ready()
    });
    // Left: the TAP is a bridge port next to a veth; frames enter from the veth's peer.
    ip(&["link", "add", "uqc-br", "type", "bridge"]);
    ip(&[
        "link", "add", "uqc-v0", "mtu", "9000", "type", "veth", "peer", "name", "uqc-v1", "mtu",
        "9000",
    ]);
    ip(&["link", "set", "uqc-v0", "master", "uqc-br"]);
    ip(&["link", "set", "uqc-l", "master", "uqc-br"]);
    // Right: an 802.1Q upper, an 802.1ad/802.1Q stack and a macvlan on the TAP.
    ip(&[
        "link",
        "add",
        "link",
        "uqc-r",
        "name",
        "uqc-r.100",
        "type",
        "vlan",
        "id",
        "100",
    ]);
    ip(&[
        "link",
        "add",
        "link",
        "uqc-r",
        "name",
        "uqc-r.200",
        "type",
        "vlan",
        "proto",
        "802.1ad",
        "id",
        "200",
    ]);
    ip(&[
        "link",
        "add",
        "link",
        "uqc-r.200",
        "name",
        "uqc-r.200.300",
        "type",
        "vlan",
        "id",
        "300",
    ]);
    ip(&[
        "link",
        "add",
        "link",
        "uqc-r",
        "name",
        "uqc-mv",
        "address",
        "02:00:00:00:aa:01",
        "type",
        "macvlan",
        "mode",
        "bridge",
    ]);
    for device in [
        "uqc-br",
        "uqc-v0",
        "uqc-v1",
        "uqc-l",
        "uqc-r",
        "uqc-r.100",
        "uqc-r.200",
        "uqc-r.200.300",
        "uqc-mv",
    ] {
        ip(&["link", "set", device, "up"]);
    }
    until(Duration::from_secs(5), || {
        left.control.session(0).unwrap().carrier && right.control.session(0).unwrap().carrier
    });
    let input = raw_socket(c"uqc-v1", 0x88b5);
    const ALL: u16 = libc::ETH_P_ALL as u16;
    let frame = |destination: [u8; 6], source: u8, kind: &[u8], length: usize| {
        let mut frame = vec![0; length];
        frame[..6].copy_from_slice(&destination);
        frame[6..12].copy_from_slice(&[2, 0, 0, 0, 0, source]);
        frame[12..12 + kind.len()].copy_from_slice(kind);
        for (index, byte) in frame.iter_mut().enumerate().skip(12 + kind.len()) {
            *byte = (index as u8) ^ source;
        }
        frame
    };
    let unknown = [2, 0, 0, 0, 0xbb, 1];
    let ethertype = 0x88b5_u16.to_be_bytes();
    // Each case: what enters the bridge, where it leaves, and what must arrive there.
    let mut cases: Vec<(Vec<u8>, &std::ffi::CStr, Vec<u8>)> = Vec::new();
    for source in [0x11, 0x12, 0x13] {
        let sent = frame(unknown, source, &ethertype, 100);
        cases.push((sent.clone(), c"uqc-r", sent));
    }
    let broadcast = frame([0xff; 6], 0x14, &ethertype, 60);
    cases.push((broadcast.clone(), c"uqc-r", broadcast));
    let multicast = frame([1, 0, 0x5e, 0, 0, 0xfb], 0x15, &ethertype, 200);
    cases.push((multicast.clone(), c"uqc-r", multicast));
    // 802.3 length 38 with an LLC header, padded to the minimum frame.
    let llc = frame(unknown, 0x16, &[0, 38, 0x42, 0x42, 0x03], 60);
    cases.push((llc.clone(), c"uqc-r", llc));
    let macvlan = frame([2, 0, 0, 0, 0xaa, 1], 0x17, &ethertype, 300);
    cases.push((macvlan.clone(), c"uqc-mv", macvlan));
    // Full-size tagged frames in the TAP-read direction: 9000 behind one tag,
    // 1500 behind two. Each upper delivers the frame without its tags.
    let mut tagged = frame(unknown, 0x18, &[0x81, 0, 0, 100], 9018);
    tagged[16..18].copy_from_slice(&ethertype);
    let mut untagged = tagged.clone();
    untagged.drain(12..16);
    cases.push((tagged, c"uqc-r.100", untagged));
    let mut stacked = frame(unknown, 0x19, &[0x88, 0xa8, 0, 200, 0x81, 0, 1, 0x2c], 1522);
    stacked[20..22].copy_from_slice(&ethertype);
    let mut untagged = stacked.clone();
    untagged.drain(12..20);
    cases.push((stacked, c"uqc-r.200.300", untagged));
    let mut bytes = [0; 9100];
    for (sent, device, expected) in &cases {
        let output = raw_socket(device, ALL);
        until(Duration::from_secs(5), || {
            raw_send(&input, sent);
            while let Some(n) = raw_read(&output, &mut bytes) {
                if bytes[..n] == expected[..] {
                    return true;
                }
            }
            false
        });
    }
    left.stop();
    right.stop();
}

#[test]
#[ignore = "requires disposable container with tun, iproute2, the vrf module and network capabilities"]
fn endpoints_bound_to_a_vrf_forward_through_its_table() {
    // SAFETY: only this test thread and its runtimes enter the namespace.
    check(unsafe { libc::unshare(libc::CLONE_NEWNET) });
    let net = UdpSocket::bind("[::]:0").unwrap();
    up(&net, c"lo");
    ip(&["link", "add", "uq-vrf", "type", "vrf", "table", "10"]);
    ip(&["link", "set", "uq-vrf", "up"]);
    let addresses: [Ipv6Addr; 2] = ["fd00:10::1".parse().unwrap(), "fd00:10::2".parse().unwrap()];
    for address in addresses {
        let address = format!("{address}/128");
        ip(&["-6", "addr", "add", &address, "dev", "uq-vrf", "nodad"]);
    }
    // The main table has no route, so only VRF-bound sockets reach these addresses.
    let unbound = UdpSocket::bind("[::]:0").unwrap();
    assert!(unbound.send_to(&[0], (addresses[1], 41301)).is_err());
    let bound = |name, side: usize, port, remote| {
        let mut config = config(name, port, remote);
        config.sessions.truncate(1);
        config.peers.truncate(1);
        config.daemon.memory_limit = Daemon::default().memory_limit;
        config.sessions[0].queue_count = 1;
        config.peers[0].tx_lanes = 1;
        config.endpoints[0].listen_address = addresses[side];
        config.endpoints[0].bind_device = Some(c"uq-vrf".into());
        config.peers[0].address = Some(addresses[1 - side]);
        config
    };
    let mut left = Running::start(bound("uq-vrf-left", 0, 41300, 41301));
    let mut right = Running::start(bound("uq-vrf-right", 1, 41301, 41300));
    until(Duration::from_secs(5), || {
        left.control.ready() && right.control.ready()
    });
    up(&net, c"uq-vrf-left");
    up(&net, c"uq-vrf-right");
    until(Duration::from_secs(5), || {
        left.control.session(0).unwrap().carrier && right.control.session(0).unwrap().carrier
    });
    let sockets = [
        raw_socket(c"uq-vrf-left", 0x88b5),
        raw_socket(c"uq-vrf-right", 0x88b5),
    ];
    let mut input = [0; 2000];
    for direction in 0..2 {
        let mut frame = [0x5a; 60];
        frame[..12].copy_from_slice(&[2, 0, 0, 0, 0, 1, 2, 0, 0, 0, 0, 2]);
        frame[12..14].copy_from_slice(&0x88b5_u16.to_be_bytes());
        frame[14] = direction as u8;
        until(Duration::from_secs(5), || {
            raw_send(&sockets[direction], &frame);
            while let Some(n) = raw_read(&sockets[1 - direction], &mut input) {
                if input[..n] == frame {
                    return true;
                }
            }
            false
        });
    }
    left.stop();
    right.stop();
}

#[test]
#[ignore = "requires disposable container with tun and network namespace capabilities"]
fn runtime_adopts_handed_over_queues_of_the_configured_count() {
    use unquic::{
        runtime::{Control, run_adopting},
        tap::Tap,
    };
    // SAFETY: only this test thread and its runtime enter the namespace.
    check(unsafe { libc::unshare(libc::CLONE_NEWNET) });
    let net = UdpSocket::bind("[::]:0").unwrap();
    up(&net, c"lo");
    let mut config = config("uq-adopt-rt", 41500, 41501);
    config.sessions.truncate(1);
    config.peers.truncate(1);
    config.daemon.memory_limit = Daemon::default().memory_limit;
    config.sessions[0].queue_count = 2;
    let handed = |count| {
        let tap = Tap::create(c"uq-adopt-rt", 9000, count, None).unwrap();
        let fds = tap.into_queues().into_iter().map(|q| q.into_fd()).collect();
        Tap::adopt(fds, 9000).unwrap()
    };
    let refused = |adopted| {
        run_adopting(config.clone(), Control::new(), adopted)
            .unwrap_err()
            .kind()
    };
    assert_eq!(refused(vec![Some(handed(1))]), io::ErrorKind::InvalidInput);
    assert_eq!(refused(vec![None, None]), io::ErrorKind::InvalidInput);
    // SAFETY: the name is NUL-terminated; the call only queries this namespace.
    let index = unsafe { libc::if_nametoindex(c"uq-adopt-rt".as_ptr()) };
    assert_ne!(index, 0);
    let control = Control::new();
    let runtime = {
        let control = control.clone();
        let adopted = vec![Some(handed(2))];
        thread::spawn(move || run_adopting(config, control, adopted))
    };
    until(Duration::from_secs(5), || control.ready());
    // SAFETY: as above.
    assert_eq!(
        unsafe { libc::if_nametoindex(c"uq-adopt-rt".as_ptr()) },
        index
    );
    control.shutdown();
    runtime.join().unwrap().unwrap();
    unquic::tap::remove(c"uq-adopt-rt").unwrap();
}
