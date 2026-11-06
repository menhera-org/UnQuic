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
