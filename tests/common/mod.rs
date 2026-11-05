//! Shared helpers for disposable network-namespace tests.
#![allow(dead_code)]
use std::{
    ffi::CStr,
    io,
    mem::{size_of, zeroed},
    net::UdpSocket,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    time::{Duration, Instant},
};
use unquic::MacAddress;

pub fn check(n: libc::c_int) -> libc::c_int {
    assert!(n >= 0, "{}", io::Error::last_os_error());
    n
}

pub fn request(name: &CStr) -> libc::ifreq {
    // SAFETY: zero is valid for every ifreq field before populating it.
    let mut req: libc::ifreq = unsafe { zeroed() };
    for (dst, &src) in req.ifr_name.iter_mut().zip(name.to_bytes()) {
        *dst = src as libc::c_char;
    }
    req
}

pub fn mac_address(control: &UdpSocket, name: &CStr) -> MacAddress {
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

pub fn flags(control: &UdpSocket, name: &CStr) -> libc::c_short {
    let mut req = request(name);
    // SAFETY: the ioctl writes a complete ifreq to valid storage.
    check(unsafe { libc::ioctl(control.as_raw_fd(), libc::SIOCGIFFLAGS as _, &mut req) });
    // SAFETY: SIOCGIFFLAGS populated this union field.
    unsafe { req.ifr_ifru.ifru_flags }
}

pub fn up(control: &UdpSocket, name: &CStr) {
    let mut req = request(name);
    req.ifr_ifru.ifru_flags = flags(control, name) | libc::IFF_UP as libc::c_short;
    // SAFETY: initialized ifreq with the flags field selected.
    check(unsafe { libc::ioctl(control.as_raw_fd(), libc::SIOCSIFFLAGS as _, &req) });
}

pub fn raw_socket(name: &CStr, protocol: u16) -> OwnedFd {
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

pub fn wait_for(mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !check() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for kernel I/O"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

pub fn raw_send(fd: &OwnedFd, frame: &[u8]) {
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

pub fn raw_read(fd: &OwnedFd, buffer: &mut [u8]) -> Option<usize> {
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
pub fn capture_frame(fd: &OwnedFd, buffer: &mut [u8]) -> Option<usize> {
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

// Linux UAPI linux/if_vlan.h; the data union's largest member is 24 bytes.
pub fn add_vlan(control: &UdpSocket, lower: &CStr, vid: u16) {
    #[repr(C)]
    struct Request {
        command: i32,
        device: [u8; 24],
        data: [u32; 6],
        qos: i16,
    }
    // SAFETY: every field in this ioctl argument accepts zero.
    let mut request: Request = unsafe { zeroed() };
    assert_eq!(size_of::<Request>(), 56);
    request.command = 6; // SET_VLAN_NAME_TYPE_CMD
    request.data[0] = 3; // VLAN_NAME_TYPE_RAW_PLUS_VID_NO_PAD
    // SAFETY: request has the UAPI layout and is writable for this ioctl.
    check(unsafe { libc::ioctl(control.as_raw_fd(), 0x8983 as _, &mut request) });
    request.command = 0; // ADD_VLAN_CMD
    request.device[..lower.to_bytes().len()].copy_from_slice(lower.to_bytes());
    request.data[0] = u32::from(vid);
    // SAFETY: initialized ADD_VLAN_CMD argument with a terminated lower name.
    check(unsafe { libc::ioctl(control.as_raw_fd(), 0x8983 as _, &mut request) });
}
