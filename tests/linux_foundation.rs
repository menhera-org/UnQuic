//! Live kernel checks in fresh network and UTS namespaces.
use std::{
    io,
    mem::size_of,
    net::{SocketAddrV6, UdpSocket},
    os::fd::{AsFd, AsRawFd},
};
use unquic::{
    BASE_UDP_PAYLOAD, MAX_FRAME_LEN, MacAddress,
    socket::{ReceiveSocket, TransmitSocket},
    tap::Tap,
};

mod common;
use common::*;

#[test]
#[ignore = "requires isolated container, /dev/net/tun, CAP_SYS_ADMIN, CAP_NET_ADMIN and CAP_NET_RAW"]
fn kernel_socket_and_tap_invariants() {
    // SAFETY: unshare changes only this test thread's network and UTS namespaces.
    check(unsafe { libc::unshare(libc::CLONE_NEWNET | libc::CLONE_NEWUTS) });
    let hostname = b"unquic-test-host";
    // SAFETY: hostname is readable for its length; the UTS namespace is private.
    check(unsafe { libc::sethostname(hostname.as_ptr().cast(), hostname.len()) });
    let control = UdpSocket::bind("[::]:0").unwrap();
    up(&control, c"lo");

    let rx = ReceiveSocket::bind("[::1]:40000".parse().unwrap(), Some(c"lo")).unwrap();
    let tx = TransmitSocket::connect(&rx, rx.local_addr()).unwrap();
    let capture = raw_socket(c"lo", libc::ETH_P_IPV6 as u16);
    tx.send(&[0x5a; BASE_UDP_PAYLOAD]).unwrap();
    let mut packet = [0; 2048];
    wait_for(|| {
        let Some(n) = raw_read(&capture, &mut packet) else {
            return false;
        };
        assert_eq!(n, 14 + 1280);
        assert_eq!(packet[14] >> 4, 6);
        assert_eq!(packet[20], libc::IPPROTO_UDP as u8); // No Fragment/extension header.
        assert_eq!(u16::from_be_bytes([packet[18], packet[19]]), 1240);
        assert_eq!(
            u16::from_be_bytes([packet[54], packet[55]]),
            tx.local_addr().unwrap().port()
        );
        assert_eq!(u16::from_be_bytes([packet[56], packet[57]]), 40000);
        true
    });
    let (n, source) = rx.recv(&mut packet).unwrap();
    assert_eq!(n, BASE_UDP_PAYLOAD);
    assert_ne!(source.port(), 40000);
    assert_eq!(source, tx.local_addr().unwrap());
    // IPv6-only wildcard sockets must leave the corresponding IPv4 port free.
    let v6 = ReceiveSocket::bind("[::]:40001".parse::<SocketAddrV6>().unwrap(), None).unwrap();
    let v4 = UdpSocket::bind("0.0.0.0:40001").unwrap();
    drop((v6, v4));

    let mac = "02:aa:bb:cc:dd:ee".parse::<unquic::MacAddress>().unwrap();
    let mut tap = Tap::create(c"uq-test", 9000, 2, Some(mac)).unwrap();
    assert_eq!(flags(&control, tap.name()) & libc::IFF_UP as i16, 0);
    assert_eq!(mac_address(&control, tap.name()), mac);
    assert_eq!(tap.name(), c"uq-test");
    assert_eq!(tap.queues().len(), 2);
    assert!(Tap::create(c"uq-test", 1500, 1, None).is_err());
    let mut req = request(tap.name());
    // SAFETY: output ifreq is valid.
    check(unsafe { libc::ioctl(control.as_raw_fd(), libc::SIOCGIFMTU as _, &mut req) });
    assert_eq!(unsafe { req.ifr_ifru.ifru_mtu }, 9000);
    for queue in tap.queues() {
        let fd = queue.as_fd().as_raw_fd();
        let mut req = request(c"uq-test");
        // SAFETY: TUNGETIFF writes an ifreq into valid storage.
        check(unsafe { libc::ioctl(fd, libc::TUNGETIFF, &mut req) });
        let queue_flags = unsafe { req.ifr_ifru.ifru_flags } as i32;
        assert_eq!(
            queue_flags & (libc::IFF_TAP | libc::IFF_NO_PI | libc::IFF_MULTI_QUEUE),
            libc::IFF_TAP | libc::IFF_NO_PI | libc::IFF_MULTI_QUEUE
        );
        assert_eq!(queue_flags & libc::IFF_VNET_HDR, 0);
        assert_ne!(
            unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
        assert_ne!(
            unsafe { libc::fcntl(fd, libc::F_GETFL) } & libc::O_NONBLOCK,
            0
        );
    }
    up(&control, tap.name());
    wait_for(|| flags(&control, tap.name()) & libc::IFF_RUNNING as i16 == 0);
    tap.set_carrier(true).unwrap();
    wait_for(|| flags(&control, tap.name()) & libc::IFF_RUNNING as i16 != 0);

    let ethernet = raw_socket(tap.name(), libc::ETH_P_ALL as u16);
    let enabled = 1i32;
    // SAFETY: PACKET_AUXDATA reads one integer option.
    check(unsafe {
        libc::setsockopt(
            ethernet.as_raw_fd(),
            libc::SOL_PACKET,
            libc::PACKET_AUXDATA,
            (&enabled as *const i32).cast(),
            size_of::<i32>() as _,
        )
    });
    let mut buffer = [0; MAX_FRAME_LEN + 1];
    for (length, tags) in [
        (60, 0),
        (1514, 0),
        (1518, 1),
        (1522, 2),
        (9018, 1),
        (MAX_FRAME_LEN, 2),
    ] {
        let mut frame = vec![0x5a; length];
        frame[..12].copy_from_slice(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 2, 0, 0, 0, 0, 1]);
        frame[12..14].copy_from_slice(&0x88b5u16.to_be_bytes());
        for tag in 0..tags {
            let at = 12 + tag * 4;
            frame[at..at + 2].copy_from_slice(
                &(if tags == 2 && tag == 0 {
                    0x88a8u16
                } else {
                    0x8100u16
                })
                .to_be_bytes(),
            );
            frame[at + 2..at + 4].copy_from_slice(&0xa123u16.to_be_bytes());
            frame[at + 4..at + 6].copy_from_slice(&0x88b5u16.to_be_bytes());
        }
        // AF_PACKET allows only one VLAN header beyond the interface MTU.
        // shortcut: maximum QinQ is write-only here; add VLAN-upper egress in composition tests.
        if length <= 9018 {
            raw_send(&ethernet, &frame);
            wait_for(|| {
                for queue in tap.queues() {
                    match queue.read_frame(&mut buffer) {
                        Ok(n) if buffer[..n] == frame => return true,
                        Ok(_) => {}
                        Err(e) => assert_eq!(e.kind(), io::ErrorKind::WouldBlock),
                    }
                }
                false
            });
        }
        // Userspace to kernel: TAP write must arrive as one unchanged frame.
        tap.queues()[0].write_frame(&frame).unwrap();
        wait_for(|| capture_frame(&ethernet, &mut buffer).is_some_and(|n| buffer[..n] == frame));
    }
    assert_eq!(
        tap.queues()[0]
            .read_frame(&mut [0; MAX_FRAME_LEN])
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidInput
    );
    assert_eq!(
        tap.queues()[0]
            .write_frame(&[0; MAX_FRAME_LEN + 1])
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidInput
    );
    assert_eq!(
        tap.queues()[0].write_frame(&[0; 13]).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    tap.set_carrier(false).unwrap();
    wait_for(|| flags(&control, tap.name()) & libc::IFF_RUNNING as i16 == 0);
    let queues = tap.into_queues();
    queues[1].set_carrier(true).unwrap();
    wait_for(|| flags(&control, c"uq-test") & libc::IFF_RUNNING as i16 != 0);
    queues[1].set_carrier(false).unwrap();
    wait_for(|| flags(&control, c"uq-test") & libc::IFF_RUNNING as i16 == 0);
    drop(queues);
    assert_eq!(unsafe { libc::if_nametoindex(c"uq-test".as_ptr()) }, 0);
    // Recreating a TAP with the same names must reproduce the exact address.
    let expected: MacAddress = "60:a4:34:49:63:0a".parse().unwrap();
    for _ in 0..2 {
        let generated = Tap::create(c"uq-test", 1500, 1, None).unwrap();
        assert_eq!(flags(&control, generated.name()) & libc::IFF_UP as i16, 0);
        assert_eq!(mac_address(&control, generated.name()), expected);
    }
    let other = Tap::create(c"uq-other", 1500, 1, None).unwrap();
    assert_eq!(
        mac_address(&control, other.name()).to_string(),
        "60:a4:34:4b:28:5e"
    );
    let hostname = b"another-test-host";
    // SAFETY: hostname is readable for its length; the UTS namespace is private.
    check(unsafe { libc::sethostname(hostname.as_ptr().cast(), hostname.len()) });
    // A hostname change affects future creation, not an existing device.
    assert_eq!(
        mac_address(&control, other.name()).to_string(),
        "60:a4:34:4b:28:5e"
    );
    let changed = Tap::create(c"uq-test", 1500, 1, None).unwrap();
    assert_eq!(
        mac_address(&control, changed.name()).to_string(),
        "60:a4:34:48:1b:53"
    );
    let hostname = [b'a'; 64];
    // SAFETY: hostname is readable for its length; the UTS namespace is private.
    check(unsafe { libc::sethostname(hostname.as_ptr().cast(), hostname.len()) });
    let maximum = Tap::create(c"bbbbbbbbbbbbbbb", 1500, 1, None).unwrap();
    assert_eq!(
        mac_address(&control, maximum.name()).to_string(),
        "60:a4:34:4d:c9:8a"
    );
}
