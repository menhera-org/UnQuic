//! Live kernel checks in fresh network and UTS namespaces.
use std::{
    ffi::CStr,
    io,
    mem::{size_of, zeroed},
    net::{SocketAddrV6, UdpSocket},
    os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd},
    time::{Duration, Instant},
};
use unquic::{
    BASE_UDP_PAYLOAD, MAX_FRAME_LEN, MacAddress,
    socket::{ReceiveSocket, TransmitSocket},
    tap::Tap,
};

fn check(n: libc::c_int) -> libc::c_int {
    assert!(n >= 0, "{}", io::Error::last_os_error());
    n
}

fn request(name: &CStr) -> libc::ifreq {
    // SAFETY: zero is valid for every ifreq field before populating it.
    let mut req: libc::ifreq = unsafe { zeroed() };
    for (dst, &src) in req.ifr_name.iter_mut().zip(name.to_bytes()) {
        *dst = src as libc::c_char;
    }
    req
}

fn mac_address(control: &UdpSocket, name: &CStr) -> MacAddress {
    let mut req = request(name);
    // SAFETY: SIOCGIFHWADDR writes the hardware address into this ifreq.
    check(unsafe { libc::ioctl(control.as_raw_fd(), libc::SIOCGIFHWADDR as _, &mut req) });
    // SAFETY: the ioctl populated this union field.
    let actual = unsafe { req.ifr_ifru.ifru_hwaddr };
    assert_eq!(actual.sa_family, libc::ARPHRD_ETHER);
    let mut bytes = [0; 6];
    for (dst, &src) in bytes.iter_mut().zip(&actual.sa_data) {
        *dst = src as u8;
    }
    MacAddress::new(bytes).unwrap()
}

fn flags(control: &UdpSocket, name: &CStr) -> libc::c_short {
    let mut req = request(name);
    // SAFETY: the ioctl writes a complete ifreq to valid storage.
    check(unsafe { libc::ioctl(control.as_raw_fd(), libc::SIOCGIFFLAGS as _, &mut req) });
    // SAFETY: SIOCGIFFLAGS populated this union field.
    unsafe { req.ifr_ifru.ifru_flags }
}

fn up(control: &UdpSocket, name: &CStr) {
    let mut req = request(name);
    req.ifr_ifru.ifru_flags = flags(control, name) | libc::IFF_UP as libc::c_short;
    // SAFETY: initialized ifreq with the flags field selected.
    check(unsafe { libc::ioctl(control.as_raw_fd(), libc::SIOCSIFFLAGS as _, &req) });
}

fn raw_socket(name: &CStr, protocol: u16) -> OwnedFd {
    // SAFETY: no pointer arguments, returns a new descriptor.
    let fd = check(unsafe {
        libc::socket(
            libc::AF_PACKET,
            libc::SOCK_RAW | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            i32::from(protocol.to_be()),
        )
    });
    // SAFETY: this is the sole owner of the new fd.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: name is NUL-terminated.
    let index = unsafe { libc::if_nametoindex(name.as_ptr()) };
    assert_ne!(index, 0);
    // SAFETY: zero is valid for sockaddr_ll before populating it.
    let mut address: libc::sockaddr_ll = unsafe { zeroed() };
    address.sll_family = libc::AF_PACKET as _;
    address.sll_protocol = protocol.to_be();
    address.sll_ifindex = index as _;
    // SAFETY: initialized sockaddr_ll and matching length.
    check(unsafe {
        libc::bind(
            fd.as_raw_fd(),
            (&address as *const libc::sockaddr_ll).cast(),
            size_of::<libc::sockaddr_ll>() as _,
        )
    });
    fd
}

fn wait_for(mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !check() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for kernel I/O"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn raw_send(fd: &OwnedFd, frame: &[u8]) {
    // SAFETY: zero is valid for this output address.
    let mut address: libc::sockaddr_ll = unsafe { zeroed() };
    let mut len = size_of::<libc::sockaddr_ll>() as libc::socklen_t;
    // SAFETY: address and len are writable and match their declared sizes.
    check(unsafe {
        libc::getsockname(
            fd.as_raw_fd(),
            (&mut address as *mut libc::sockaddr_ll).cast(),
            &mut len,
        )
    });
    address.sll_protocol = u16::from_ne_bytes([frame[12], frame[13]]);
    // SAFETY: frame and address are readable for their supplied lengths.
    let n = unsafe {
        libc::sendto(
            fd.as_raw_fd(),
            frame.as_ptr().cast(),
            frame.len(),
            0,
            (&address as *const libc::sockaddr_ll).cast(),
            size_of::<libc::sockaddr_ll>() as _,
        )
    };
    assert_eq!(n, frame.len() as isize, "{}", io::Error::last_os_error());
}

fn raw_read(fd: &OwnedFd, buffer: &mut [u8]) -> Option<usize> {
    // SAFETY: buffer is writable for the supplied length.
    let n = unsafe { libc::recv(fd.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len(), 0) };
    if n < 0 {
        assert_eq!(io::Error::last_os_error().kind(), io::ErrorKind::WouldBlock);
        None
    } else {
        Some(n as usize)
    }
}

// AF_PACKET reports a stripped outer VLAN through ancillary data on ingress.
fn capture_frame(fd: &OwnedFd, buffer: &mut [u8]) -> Option<usize> {
    let mut ancillary = [0usize; 8];
    let mut iov = libc::iovec {
        iov_base: buffer.as_mut_ptr().cast(),
        iov_len: buffer.len() - 4,
    };
    // SAFETY: zero is valid for msghdr before populating its buffer fields.
    let mut message: libc::msghdr = unsafe { zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = ancillary.as_mut_ptr().cast();
    message.msg_controllen = size_of_val(&ancillary) as _;
    // SAFETY: all message buffers are writable for their declared lengths.
    let n = unsafe { libc::recvmsg(fd.as_raw_fd(), &mut message, 0) };
    if n < 0 {
        assert_eq!(io::Error::last_os_error().kind(), io::ErrorKind::WouldBlock);
        return None;
    }
    let mut n = n as usize;
    assert_eq!(message.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC), 0);
    // SAFETY: recvmsg initialized ancillary data, aligned for cmsghdr.
    unsafe {
        let mut header = libc::CMSG_FIRSTHDR(&message);
        while !header.is_null() {
            if (*header).cmsg_level == libc::SOL_PACKET
                && (*header).cmsg_type == libc::PACKET_AUXDATA
            {
                assert!(
                    (*header).cmsg_len
                        >= libc::CMSG_LEN(size_of::<libc::tpacket_auxdata>() as _) as _
                );
                let aux = libc::CMSG_DATA(header)
                    .cast::<libc::tpacket_auxdata>()
                    .read_unaligned();
                if aux.tp_status & libc::TP_STATUS_VLAN_VALID != 0 {
                    assert!(n >= 14 && n + 4 <= buffer.len());
                    buffer.copy_within(12..n, 16);
                    let tpid = if aux.tp_status & libc::TP_STATUS_VLAN_TPID_VALID != 0 {
                        aux.tp_vlan_tpid
                    } else {
                        0x8100
                    };
                    buffer[12..14].copy_from_slice(&tpid.to_be_bytes());
                    buffer[14..16].copy_from_slice(&aux.tp_vlan_tci.to_be_bytes());
                    n += 4;
                }
            }
            header = libc::CMSG_NXTHDR(&message, header);
        }
    }
    Some(n)
}

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
