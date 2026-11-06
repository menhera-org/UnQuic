//! Socket discovery checks in a disposable network namespace.
mod common;
use common::*;
use std::{
    io,
    mem::size_of,
    net::{SocketAddrV6, UdpSocket},
    os::fd::{AsFd, AsRawFd, FromRawFd},
    process::Command,
};
use unquic::{
    UnquicUuid,
    socket::{MAX_UDP_PAYLOAD, ReceiveSocket, SendDatagram, TransmitSocket},
    wire::{HEADER_LEN, Header, PROBE_PREFIX_LEN, encode_probe_prefix},
};

fn ip(args: &[&str]) {
    assert!(Command::new("ip").args(args).status().unwrap().success());
}

fn option(tx: &TransmitSocket, name: i32) -> i32 {
    let mut value = 0i32;
    let mut len = size_of::<i32>() as libc::socklen_t;
    // SAFETY: value and len describe writable integer option storage.
    check(unsafe {
        libc::getsockopt(
            tx.as_fd().as_raw_fd(),
            libc::IPPROTO_IPV6,
            name,
            (&mut value as *mut i32).cast(),
            &mut len,
        )
    });
    assert_eq!(len as usize, size_of::<i32>());
    value
}

fn mtu_error(tx: &TransmitSocket) -> unquic::socket::MtuError {
    let mut result = None;
    wait_for(|| match tx.recv_mtu_error() {
        Ok(Some(error)) => {
            result = Some(error);
            true
        }
        Ok(None) => false,
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => false,
        Err(error) => panic!("{error}"),
    });
    result.unwrap()
}

#[test]
#[ignore = "requires disposable container, iproute2 and network namespace capabilities"]
fn probes_exceed_learned_pmtu_without_fragmentation_and_keep_gso() {
    // SAFETY: only this test thread and its children enter the private namespace.
    check(unsafe { libc::unshare(libc::CLONE_NEWNET) });
    ip(&["link", "set", "lo", "mtu", "9000", "up"]);
    for address in ["fd00::1/128", "fd00::2/128"] {
        ip(&["-6", "addr", "add", address, "dev", "lo", "nodad"]);
    }
    let local = ReceiveSocket::bind("[fd00::1]:40100".parse().unwrap(), None).unwrap();
    let remote = ReceiveSocket::bind("[fd00::2]:40101".parse().unwrap(), None).unwrap();
    let mut tx = TransmitSocket::connect(&local, remote.local_addr()).unwrap();
    // Observe an actual remote PTB before switching from kernel PMTU enforcement.
    // SAFETY: the option reads exactly one initialized integer.
    check(unsafe {
        libc::setsockopt(
            tx.as_fd().as_raw_fd(),
            libc::IPPROTO_IPV6,
            libc::IPV6_RECVERR,
            (&1i32 as *const i32).cast(),
            size_of::<i32>() as _,
        )
    });
    let capture = raw_socket(c"lo", libc::ETH_P_IPV6 as u16);
    let mut prefix = [0; PROBE_PREFIX_LEN];
    let header = Header {
        cookie: UnquicUuid::from_bytes([7; 16]),
        receive_tag: 23,
        lane: 9,
        record_area_len: 0,
        packet_number: 13,
    };
    encode_probe_prefix(header, MAX_UDP_PAYLOAD, &mut prefix).unwrap();
    let padding = [0; MAX_UDP_PAYLOAD - PROBE_PREFIX_LEN];
    let parts = [&prefix[..], &padding[..]];
    let datagram = SendDatagram { parts: &parts };
    assert_eq!(tx.send_batch(&[datagram], MAX_UDP_PAYLOAD).unwrap(), 1);
    let mut packet = [0; 65_536];
    wait_for(|| {
        let Some(length) = raw_read(&capture, &mut packet) else {
            return false;
        };
        assert_eq!(length, 14 + 9000);
        assert_eq!(packet[20], libc::IPPROTO_UDP as u8);
        assert_eq!(&packet[62..62 + prefix.len()], prefix);
        true
    });
    let mut payload = [0; MAX_UDP_PAYLOAD];
    assert_eq!(remote.recv(&mut payload).unwrap().0, MAX_UDP_PAYLOAD);

    // The kernel computes the ICMPv6 checksum; quote the actual transmitted packet.
    // SAFETY: socket returns one new fd, whose ownership transfers to UdpSocket.
    let icmp = unsafe {
        UdpSocket::from_raw_fd(check(libc::socket(
            libc::AF_INET6,
            libc::SOCK_RAW | libc::SOCK_CLOEXEC,
            libc::IPPROTO_ICMPV6,
        )))
    };
    let mut too_big = [0; 8 + 1232];
    too_big[0] = 2;
    too_big[4..8].copy_from_slice(&1500u32.to_be_bytes());
    too_big[8..].copy_from_slice(&packet[14..14 + 1232]);
    let destination = SocketAddrV6::new(*tx.local_addr().unwrap().ip(), 0, 0, 0);
    assert_eq!(icmp.send_to(&too_big, destination).unwrap(), too_big.len());
    let error = mtu_error(&tx);
    assert!(!error.local);
    assert_eq!(error.mtu, 1500);
    assert_eq!(error.destination, remote.local_addr());
    assert_eq!(error.quoted_payload(), &prefix[..HEADER_LEN]);
    assert_eq!(option(&tx, libc::IPV6_MTU), 1500);
    assert_eq!(
        tx.send_batch(&[datagram], MAX_UDP_PAYLOAD)
            .unwrap_err()
            .raw_os_error(),
        Some(libc::EMSGSIZE)
    );
    assert!(tx.gso_enabled());
    assert_eq!(mtu_error(&tx).mtu, 1500);

    tx.enable_mtu_discovery().unwrap();
    assert_eq!(option(&tx, libc::IPV6_DONTFRAG), 1);
    assert_eq!(option(&tx, libc::IPV6_MTU), 1500);
    while raw_read(&capture, &mut packet).is_some() {}
    assert_eq!(tx.send_batch(&[datagram], MAX_UDP_PAYLOAD).unwrap(), 1);
    wait_for(|| {
        let Some(length) = raw_read(&capture, &mut packet) else {
            return false;
        };
        assert_eq!(length, 14 + 9000);
        assert_eq!(packet[20], libc::IPPROTO_UDP as u8);
        assert_eq!(&packet[62..62 + prefix.len()], prefix);
        true
    });
    assert_eq!(remote.recv(&mut payload).unwrap().0, MAX_UDP_PAYLOAD);
    assert_eq!(tx.send_batch(&[datagram; 2], MAX_UDP_PAYLOAD).unwrap(), 2);
    assert!(tx.gso_enabled());
    for _ in 0..2 {
        assert_eq!(remote.recv(&mut payload).unwrap().0, MAX_UDP_PAYLOAD);
        assert_eq!(&payload[..prefix.len()], prefix);
    }

    // PROBE still obeys the local interface MTU, including reductions and recovery.
    ip(&["link", "set", "lo", "mtu", "1400"]);
    assert_eq!(
        tx.send_batch(&[datagram], MAX_UDP_PAYLOAD)
            .unwrap_err()
            .raw_os_error(),
        Some(libc::EMSGSIZE)
    );
    let error = mtu_error(&tx);
    assert!(error.local);
    assert_eq!(error.mtu, 1400);
    assert!(error.quoted_payload().is_empty());
    assert!(tx.gso_enabled());
    ip(&["link", "set", "lo", "mtu", "9000"]);
    assert_eq!(tx.send_batch(&[datagram; 2], MAX_UDP_PAYLOAD).unwrap(), 2);
    for _ in 0..2 {
        assert_eq!(remote.recv(&mut payload).unwrap().0, MAX_UDP_PAYLOAD);
    }
}

fn namespace() -> std::fs::File {
    // A namespace descriptor keeps it alive without a named mount or helper process.
    std::thread::spawn(|| {
        // SAFETY: only this temporary thread enters the new namespace.
        check(unsafe { libc::unshare(libc::CLONE_NEWNET) });
        std::fs::File::open("/proc/thread-self/ns/net").unwrap()
    })
    .join()
    .unwrap()
}

fn enter(namespace: &std::fs::File) {
    // SAFETY: the descriptor refers to a live network namespace; only this thread moves.
    check(unsafe { libc::setns(namespace.as_raw_fd(), libc::CLONE_NEWNET) });
}

fn tc(args: &[&str]) -> String {
    let output = Command::new("tc").args(args).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn routed_config(local: &str, remote: &str) -> unquic::config::Config {
    use unquic::config::{Config, Daemon, Endpoint, Peer, Session};
    Config {
        daemon: Daemon::default(),
        endpoints: vec![Endpoint {
            name: "rx".into(),
            listen_address: local.parse().unwrap(),
            listen_port: 40100,
            scope_id: 0,
            bind_device: None,
        }],
        peers: vec![Peer {
            name: "peer".into(),
            endpoint: "rx".into(),
            address: Some(remote.parse().unwrap()),
            peer_port: 40100,
            scope_id: 0,
            tx_lanes: 2,
            outer_payload_limit: MAX_UDP_PAYLOAD,
            path_group: None,
        }],
        sessions: vec![Session {
            name: "uq-route".into(),
            peer: "peer".into(),
            cookie: UnquicUuid::from_bytes([11; 16]),
            tap_name: None,
            inner_mtu: 9000,
            mac: None,
            queue_count: 2,
        }],
    }
}

#[derive(Default)]
struct Observed {
    ptb: usize,
    largest_payload: usize,
}

impl Observed {
    fn drain(&mut self, socket: &std::os::fd::OwnedFd) {
        let mut bytes = [0; 65_536];
        while let Some(length) = raw_read(socket, &mut bytes) {
            assert!(length >= 54);
            assert_eq!(bytes[14] >> 4, 6);
            assert_ne!(bytes[20], 44, "outer IPv6 Fragment header");
            if bytes[20] == libc::IPPROTO_ICMPV6 as u8 && length > 54 {
                self.ptb += usize::from(bytes[54] == 2);
            }
            if bytes[20] == libc::IPPROTO_UDP as u8 && length >= 62 + HEADER_LEN {
                // Packet sockets may expose a GSO aggregate; validate its first logical packet.
                let payload =
                    HEADER_LEN + usize::from(u16::from_be_bytes([bytes[100], bytes[101]]));
                assert!(payload <= MAX_UDP_PAYLOAD);
                assert!(62 + payload <= length);
                unquic::wire::Datagram::decode(&bytes[62..62 + payload]).unwrap();
                self.largest_payload = self.largest_payload.max(payload);
            }
        }
    }
}

fn routed_mtu(
    runtimes: &[common::runtime::Running],
    captures: &[std::os::fd::OwnedFd],
    observations: &mut [Observed],
    mtu: u64,
    seconds: u64,
) {
    use std::time::{Duration, Instant};
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut stable = None;
    loop {
        for (observation, capture) in observations.iter_mut().zip(captures) {
            observation.drain(capture);
        }
        let matches = runtimes.iter().all(|runtime| {
            (1..=2).all(|lane| {
                runtime
                    .control
                    .transmit_lane_counters(0, lane)
                    .is_some_and(|snapshot| {
                        snapshot.counters.path_mtu_bytes == mtu
                            && snapshot.counters.path_mtu_state == if mtu == 1280 { 1 } else { 3 }
                    })
            })
        });
        if matches {
            let since = stable.get_or_insert_with(Instant::now);
            // Outlast baseline confirmation retries; a transient fallback is not convergence.
            if since.elapsed() >= Duration::from_secs(if mtu == 1280 { 5 } else { 1 }) {
                eprintln!("routed outer MTU {mtu} confirmed on every lane");
                return;
            }
        } else {
            stable = None;
        }
        assert!(
            Instant::now() < deadline,
            "MTU {mtu} did not converge: {:?} / {:?}",
            runtimes[0].control.session_counters(0),
            runtimes[1].control.session_counters(0)
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn routed_frames(taps: &[std::os::fd::OwnedFd], sources: &[std::os::fd::OwnedFd]) {
    let mut frame = [0x5a; unquic::MAX_FRAME_LEN];
    frame[..12].copy_from_slice(&[2, 0, 0, 0, 0, 1, 2, 0, 0, 0, 0, 2]);
    frame[12..22].copy_from_slice(&[0x81, 0, 0, 7, 0x81, 0, 0, 8, 0x88, 0xb5]);
    let mut capture = [0; 10_000];
    let mut outgoing = frame;
    for length in [60, 1518, unquic::MAX_FRAME_LEN] {
        outgoing[12..length - 4].copy_from_slice(&frame[16..length]);
        for (from, to) in [(0, 1), (1, 0)] {
            while capture_frame(&taps[to], &mut capture).is_some() {}
            raw_send(&sources[from], &outgoing[..length - 4]);
            wait_for(|| {
                capture_frame(&taps[to], &mut capture)
                    .is_some_and(|n| n == length && capture[..n] == frame[..length])
            });
        }
    }
}

#[test]
#[ignore = "requires isolated container, tun, iproute2/tc and network capabilities; about 11 minutes"]
fn routed_discovery_handles_returned_filtered_ptb_and_upward_retry() {
    use std::{
        ffi::CString,
        time::{Duration, Instant},
    };
    // SAFETY: the test and its children use only fresh namespaces.
    check(unsafe { libc::unshare(libc::CLONE_NEWNET) });
    let router = std::fs::File::open("/proc/thread-self/ns/net").unwrap();
    let clients = [namespace(), namespace()];
    let addresses = ["fd12:0::2", "fd12:1::2"];
    let mut captures = Vec::with_capacity(2);
    let mut taps = Vec::with_capacity(2);
    let mut sources = Vec::with_capacity(2);
    let mut runtimes = Vec::with_capacity(2);
    ip(&["link", "set", "lo", "up"]);
    std::fs::write("/proc/sys/net/ipv6/conf/all/forwarding", "1").unwrap();
    for (index, client) in clients.iter().enumerate() {
        let r = format!("r{index}");
        let e = format!("e{index}");
        let prefix = format!("fd12:{index}::");
        let ns_path = format!("/proc/{}/fd/{}", std::process::id(), client.as_raw_fd());
        ip(&["link", "add", &r, "type", "veth", "peer", "name", &e]);
        ip(&["link", "set", &r, "mtu", "9000", "up"]);
        ip(&["link", "set", &e, "netns", &ns_path]);
        ip(&[
            "-6",
            "addr",
            "add",
            &format!("{prefix}1/64"),
            "dev",
            &r,
            "nodad",
        ]);
        enter(client);
        ip(&["link", "set", "lo", "up"]);
        ip(&["link", "set", &e, "mtu", "9000", "up"]);
        ip(&[
            "-6",
            "addr",
            "add",
            &format!("{prefix}2/64"),
            "dev",
            &e,
            "nodad",
        ]);
        ip(&[
            "-6",
            "route",
            "add",
            "default",
            "via",
            &format!("{prefix}1"),
            "dev",
            &e,
        ]);
        captures.push(raw_socket(
            &CString::new(e).unwrap(),
            libc::ETH_P_IPV6 as u16,
        ));
        runtimes.push(common::runtime::Running::start(routed_config(
            addresses[index],
            addresses[1 - index],
        )));
        wait_for(|| runtimes[index].control.ready());
        let control = UdpSocket::bind("[::]:0").unwrap();
        up(&control, c"uq-route");
        taps.push(raw_socket(c"uq-route", libc::ETH_P_ALL as u16));
        let enabled = 1_i32;
        // SAFETY: the option reads exactly one initialized integer.
        check(unsafe {
            libc::setsockopt(
                taps[index].as_raw_fd(),
                libc::SOL_PACKET,
                libc::PACKET_AUXDATA,
                (&enabled as *const i32).cast(),
                size_of::<i32>() as _,
            )
        });
        add_vlan(&control, c"uq-route", 7);
        up(&control, c"uq-route.7");
        sources.push(raw_socket(c"uq-route.7", libc::ETH_P_ALL as u16));
        enter(&router);
    }
    let mut observations = [Observed::default(), Observed::default()];
    let change_mtu = |mtu: &str| {
        for index in 0..2 {
            ip(&[
                "-6",
                "route",
                "replace",
                &format!("fd12:{index}::/64"),
                "dev",
                &format!("r{index}"),
                "metric",
                "256",
                "mtu",
                mtu,
            ]);
        }
    };
    routed_mtu(&runtimes, &captures, &mut observations, 9000, 40);
    routed_frames(&taps, &sources);
    change_mtu("1500");
    routed_mtu(&runtimes, &captures, &mut observations, 1500, 15);
    assert!(observations.iter().all(|observation| observation.ptb > 0));
    routed_frames(&taps, &sources);

    // Drop only generated PTB, preserving ND and ordinary transport traffic.
    for index in 0..2 {
        let r = format!("r{index}");
        tc(&["qdisc", "add", "dev", &r, "clsact"]);
        tc(&[
            "filter", "add", "dev", &r, "egress", "protocol", "ipv6", "pref", "1", "u32", "match",
            "u8", "58", "0xff", "at", "6", "match", "u8", "2", "0xff", "at", "40", "action",
            "drop",
        ]);
        observations[index].drain(&captures[index]);
        observations[index].ptb = 0;
    }
    change_mtu("1280");
    routed_mtu(&runtimes, &captures, &mut observations, 1280, 70);
    assert!(observations.iter().all(|observation| observation.ptb == 0));
    for index in 0..2 {
        let statistics = tc(&[
            "-s",
            "filter",
            "show",
            "dev",
            &format!("r{index}"),
            "egress",
        ]);
        let drops = statistics
            .split_whitespace()
            .skip_while(|word| *word != "(dropped")
            .nth(1)
            .unwrap()
            .trim_end_matches(',')
            .parse::<u64>()
            .unwrap();
        assert!(drops > 0, "PTB filter was not exercised");
    }
    routed_frames(&taps, &sources);
    change_mtu("9000");
    for index in 0..2 {
        tc(&["qdisc", "del", "dev", &format!("r{index}"), "clsact"]);
    }
    let raised = Instant::now();
    routed_mtu(&runtimes, &captures, &mut observations, 9000, 650);
    assert!(
        raised.elapsed() >= Duration::from_secs(500),
        "upward retry was bypassed"
    );
    routed_frames(&taps, &sources);
    assert!(
        observations
            .iter()
            .all(|observation| observation.largest_payload == MAX_UDP_PAYLOAD)
    );
    for capture in &captures {
        let mut statistics = libc::tpacket_stats {
            tp_packets: 0,
            tp_drops: 0,
        };
        let mut length = size_of::<libc::tpacket_stats>() as libc::socklen_t;
        // SAFETY: statistics and length describe writable packet-socket counter storage.
        check(unsafe {
            libc::getsockopt(
                capture.as_raw_fd(),
                libc::SOL_PACKET,
                libc::PACKET_STATISTICS,
                (&mut statistics as *mut libc::tpacket_stats).cast(),
                &mut length,
            )
        });
        assert_eq!(statistics.tp_drops, 0, "outer capture lost packets");
    }
    for runtime in &mut runtimes {
        runtime.stop();
    }
}
