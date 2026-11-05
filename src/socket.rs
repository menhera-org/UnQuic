//! Nonblocking IPv6-only UDP sockets with separate receive and transmit roles.
//!
//! Device/VRF binding may require CAP_NET_RAW (CAP_NET_ADMIN on older kernels).
//! Callers resolve names before construction. Link-local addresses require an
//! explicit interface index in `SocketAddrV6::scope_id`.

use crate::{
    BASE_UDP_PAYLOAD,
    sys::{cvt, invalid, udp_socket},
};
use std::{
    ffi::{CStr, CString},
    io,
    mem::{size_of, zeroed},
    net::{Ipv6Addr, SocketAddr, SocketAddrV6, UdpSocket},
    os::fd::{AsFd, AsRawFd, BorrowedFd},
};

/// Maximum number of logical datagrams in one batched syscall.
pub const BATCH_SIZE: usize = 64;

/// One outgoing datagram, optionally split into prefix and borrowed payload.
#[derive(Clone, Copy)]
pub struct SendDatagram<'a> {
    /// Wire prefix, or the whole datagram when payload is empty.
    pub prefix: &'a [u8],
    /// Borrowed frame piece; concatenated with prefix by the kernel.
    pub payload: &'a [u8],
}

/// One caller-owned receive buffer and its result metadata.
pub struct ReceiveDatagram<'a> {
    /// Storage filled directly by recvmmsg.
    pub buffer: &'a mut [u8],
    /// Whole datagram length, which can exceed buffer.len() on truncation.
    pub length: usize,
    /// A truncated entry is consumed but must never be parsed.
    pub truncated: bool,
}

impl<'a> ReceiveDatagram<'a> {
    /// Describes writable storage without allocation.
    pub const fn new(buffer: &'a mut [u8]) -> Self {
        Self {
            buffer,
            length: 0,
            truncated: false,
        }
    }
}

/// A fixed-port receive socket. The API provides no send operation.
///
/// Reuseport members must be created before connections become active and kept
/// unchanged until those connections end.
pub struct ReceiveSocket {
    socket: UdpSocket,
    address: SocketAddrV6,
    device: Option<CString>,
}

impl ReceiveSocket {
    /// Binds a fixed nonzero port, optionally within an interface or VRF.
    ///
    /// All workers in a receive group must use the same address and device.
    pub fn bind(address: SocketAddrV6, device: Option<&CStr>) -> io::Result<Self> {
        if address.port() == 0 {
            return Err(invalid("receive port must be nonzero"));
        }
        Self::bind_inner(address, device)
    }

    fn bind_inner(address: SocketAddrV6, device: Option<&CStr>) -> io::Result<Self> {
        validate_address(address)?;
        let socket = configured_socket(device)?;
        set_int(&socket, libc::SOL_SOCKET, libc::SO_REUSEPORT, 1)?;
        bind(&socket, address)?;
        let address = local_address(&socket)?;
        Ok(Self {
            socket,
            address,
            device: device.map(CStr::to_owned),
        })
    }

    /// Receives up to BATCH_SIZE datagrams directly into caller-owned buffers.
    ///
    /// Returns the completed prefix of entries; later entries are unchanged.
    /// Each truncated entry is reported separately so valid siblings survive.
    /// Source tuples are not requested: protocol admission uses cookie and state.
    pub fn recv_batch(&self, datagrams: &mut [ReceiveDatagram<'_>]) -> io::Result<usize> {
        recv_batch(self.socket.as_fd(), datagrams)
    }

    /// Returns the fixed receive address and port.
    pub const fn local_addr(&self) -> SocketAddrV6 {
        self.address
    }

    /// Receives one datagram, rejecting truncation with `InvalidData`.
    ///
    /// A rejected datagram is consumed. No partially received payload may be
    /// parsed. Returns `WouldBlock` when the receive queue is empty.
    pub fn recv(&self, payload: &mut [u8]) -> io::Result<(usize, SocketAddrV6)> {
        // SAFETY: all-zero sockaddr_in6 is a valid writable output buffer.
        let mut source: libc::sockaddr_in6 = unsafe { zeroed() };
        let mut len = size_of::<libc::sockaddr_in6>() as libc::socklen_t;
        // SAFETY: both writable buffers and the address length match their storage.
        let n = unsafe {
            libc::recvfrom(
                self.socket.as_raw_fd(),
                payload.as_mut_ptr().cast(),
                payload.len(),
                libc::MSG_TRUNC,
                (&mut source as *mut libc::sockaddr_in6).cast(),
                &mut len,
            )
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        if n as usize > payload.len() {
            return Err(io::ErrorKind::InvalidData.into());
        }
        if len as usize != size_of::<libc::sockaddr_in6>()
            || source.sin6_family != libc::AF_INET6 as libc::sa_family_t
        {
            return Err(io::ErrorKind::InvalidData.into());
        }
        Ok((
            n as usize,
            SocketAddrV6::new(
                Ipv6Addr::from(source.sin6_addr.s6_addr),
                u16::from_be(source.sin6_port),
                source.sin6_flowinfo,
                source.sin6_scope_id,
            ),
        ))
    }
}

impl AsFd for ReceiveSocket {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.socket.as_fd()
    }
}

/// A connected, stable dynamic-port socket targeting the peer's fixed port.
///
/// This foundation sends at the IPv6 baseline size until path-MTU probing is
/// implemented. It supplies no protocol scheduling or congestion control.
pub struct TransmitSocket {
    socket: UdpSocket,
}

impl TransmitSocket {
    /// Creates a lane in the receiver's address and interface/VRF scope.
    ///
    /// The kernel selects a source port distinct from the local receive port.
    /// The caller must pass the configured peer receive port, never an observed
    /// dynamic source port. IPv4-mapped, multicast and unspecified peers fail.
    pub fn connect(receiver: &ReceiveSocket, peer: SocketAddrV6) -> io::Result<Self> {
        validate_address(peer)?;
        if peer.port() == 0 || peer.ip().is_unspecified() {
            return Err(invalid(
                "peer requires a unicast IPv6 address and nonzero port",
            ));
        }
        let socket = configured_socket(receiver.device.as_deref())?;
        set_int(&socket, libc::IPPROTO_IPV6, libc::IPV6_DONTFRAG, 1)?;
        set_int(
            &socket,
            libc::IPPROTO_IPV6,
            libc::IPV6_MTU_DISCOVER,
            libc::IPV6_PMTUDISC_DO,
        )?;
        set_int(&socket, libc::IPPROTO_IPV6, libc::IPV6_TCLASS, 0)?;
        let mut local = receiver.address;
        local.set_port(0);
        bind(&socket, local)?;
        // Binding can differ after source selection; check the port explicitly.
        if local_address(&socket)?.port() == receiver.address.port() {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "dynamic port equals receive port",
            ));
        }
        socket.connect(peer)?;
        Ok(Self { socket })
    }

    /// Returns the selected source address and dynamic port.
    pub fn local_addr(&self) -> io::Result<SocketAddrV6> {
        local_address(&self.socket)
    }

    /// Sends one complete UDP payload of at most 1232 bytes.
    ///
    /// Returns kernel errors, including `WouldBlock` and `EMSGSIZE`, unchanged.
    /// A successful return means socket submission, not remote acceptance.
    pub fn send(&self, payload: &[u8]) -> io::Result<()> {
        if payload.len() > BASE_UDP_PAYLOAD {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let n = self.socket.send(payload)?;
        if n != payload.len() {
            return Err(io::ErrorKind::WriteZero.into());
        }
        Ok(())
    }

    /// Submits up to BATCH_SIZE scatter/gather datagrams in one syscall.
    ///
    /// Each combined payload must fit the 1232-byte baseline. All entries are
    /// checked before submission. A partial result counts only the successful
    /// prefix; the caller retains the unsent suffix and its packet identities.
    pub fn send_batch(&self, datagrams: &[SendDatagram<'_>]) -> io::Result<usize> {
        send_batch(self.socket.as_fd(), datagrams)
    }

    /// Retrieves and clears the socket's pending asynchronous error.
    pub fn take_error(&self) -> io::Result<Option<io::Error>> {
        self.socket.take_error()
    }
}

impl AsFd for TransmitSocket {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.socket.as_fd()
    }
}

fn send_batch(fd: BorrowedFd<'_>, datagrams: &[SendDatagram<'_>]) -> io::Result<usize> {
    if datagrams.is_empty()
        || datagrams.len() > BATCH_SIZE
        || datagrams
            .iter()
            .any(|d| d.prefix.len().saturating_add(d.payload.len()) > BASE_UDP_PAYLOAD)
    {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let mut vectors = [libc::iovec {
        iov_base: std::ptr::null_mut(),
        iov_len: 0,
    }; BATCH_SIZE * 2];
    // SAFETY: zeroed message headers represent empty addresses/control buffers.
    let mut messages: [libc::mmsghdr; BATCH_SIZE] = unsafe { zeroed() };
    for (i, datagram) in datagrams.iter().enumerate() {
        vectors[i * 2] = libc::iovec {
            iov_base: datagram.prefix.as_ptr().cast_mut().cast(),
            iov_len: datagram.prefix.len(),
        };
        vectors[i * 2 + 1] = libc::iovec {
            iov_base: datagram.payload.as_ptr().cast_mut().cast(),
            iov_len: datagram.payload.len(),
        };
        messages[i].msg_hdr.msg_iov = vectors[i * 2..].as_mut_ptr();
        messages[i].msg_hdr.msg_iovlen = if datagram.payload.is_empty() { 1 } else { 2 };
    }
    // SAFETY: each initialized header points at live immutable datagram slices;
    // sendmmsg reads their bytes and writes only the message length fields.
    let count = cvt(unsafe {
        libc::sendmmsg(
            fd.as_raw_fd(),
            messages.as_mut_ptr(),
            datagrams.len() as u32,
            (libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL) as _,
        )
    })? as usize;
    debug_assert!(
        messages[..count]
            .iter()
            .zip(datagrams)
            .all(|(message, data)| message.msg_len as usize
                == data.prefix.len() + data.payload.len())
    );
    Ok(count)
}

fn recv_batch(fd: BorrowedFd<'_>, datagrams: &mut [ReceiveDatagram<'_>]) -> io::Result<usize> {
    if datagrams.is_empty() || datagrams.len() > BATCH_SIZE {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let mut vectors = [libc::iovec {
        iov_base: std::ptr::null_mut(),
        iov_len: 0,
    }; BATCH_SIZE];
    // SAFETY: zeroed headers request no source addresses or ancillary data.
    let mut messages: [libc::mmsghdr; BATCH_SIZE] = unsafe { zeroed() };
    for (i, datagram) in datagrams.iter_mut().enumerate() {
        vectors[i] = libc::iovec {
            iov_base: datagram.buffer.as_mut_ptr().cast(),
            iov_len: datagram.buffer.len(),
        };
        messages[i].msg_hdr.msg_iov = &mut vectors[i];
        messages[i].msg_hdr.msg_iovlen = 1;
    }
    // SAFETY: every header names one exclusive writable slice. MSG_DONTWAIT
    // makes timeout unnecessary; MSG_TRUNC reports the whole datagram length.
    let count = cvt(unsafe {
        libc::recvmmsg(
            fd.as_raw_fd(),
            messages.as_mut_ptr(),
            datagrams.len() as u32,
            (libc::MSG_DONTWAIT | libc::MSG_TRUNC) as _,
            std::ptr::null_mut(),
        )
    })? as usize;
    for (message, datagram) in messages[..count].iter().zip(datagrams) {
        datagram.length = message.msg_len as usize;
        datagram.truncated = message.msg_hdr.msg_flags & libc::MSG_TRUNC != 0
            || datagram.length > datagram.buffer.len();
    }
    Ok(count)
}

fn validate_address(address: SocketAddrV6) -> io::Result<()> {
    if address.ip().to_ipv4_mapped().is_some()
        || address.ip().is_multicast()
        || address.flowinfo() != 0
    {
        return Err(invalid(
            "expected an IPv6 address with zero flow information",
        ));
    }
    if address.ip().is_unicast_link_local() && address.scope_id() == 0 {
        return Err(invalid(
            "link-local IPv6 address requires an interface index",
        ));
    }
    Ok(())
}

fn configured_socket(device: Option<&CStr>) -> io::Result<UdpSocket> {
    if device.is_some_and(|name| name.is_empty() || name.to_bytes().len() >= libc::IFNAMSIZ) {
        return Err(invalid("binding device name must contain 1..15 bytes"));
    }
    let socket = UdpSocket::from(udp_socket()?);
    set_int(&socket, libc::IPPROTO_IPV6, libc::IPV6_V6ONLY, 1)?;
    if let Some(device) = device {
        // SAFETY: device includes a terminating NUL and lives through setsockopt.
        cvt(unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_BINDTODEVICE,
                device.as_ptr().cast(),
                device.to_bytes_with_nul().len() as libc::socklen_t,
            )
        })?;
    }
    Ok(socket)
}

fn set_int(
    socket: &UdpSocket,
    level: libc::c_int,
    option: libc::c_int,
    value: libc::c_int,
) -> io::Result<()> {
    // SAFETY: value is readable for exactly sizeof(c_int).
    cvt(unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            level,
            option,
            (&value as *const libc::c_int).cast(),
            size_of::<libc::c_int>() as libc::socklen_t,
        )
    })?;
    Ok(())
}

fn bind(socket: &UdpSocket, address: SocketAddrV6) -> io::Result<()> {
    let raw = libc::sockaddr_in6 {
        sin6_family: libc::AF_INET6 as libc::sa_family_t,
        sin6_port: address.port().to_be(),
        sin6_flowinfo: 0,
        sin6_addr: libc::in6_addr {
            s6_addr: address.ip().octets(),
        },
        sin6_scope_id: address.scope_id(),
    };
    // SAFETY: raw is initialized and its length and family agree.
    cvt(unsafe {
        libc::bind(
            socket.as_raw_fd(),
            (&raw as *const libc::sockaddr_in6).cast(),
            size_of::<libc::sockaddr_in6>() as libc::socklen_t,
        )
    })?;
    Ok(())
}

fn local_address(socket: &UdpSocket) -> io::Result<SocketAddrV6> {
    match socket.local_addr()? {
        SocketAddr::V6(address) => Ok(address),
        SocketAddr::V4(_) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected IPv6 socket",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn receiver() -> ReceiveSocket {
        ReceiveSocket::bind_inner(SocketAddrV6::new(Ipv6Addr::LOCALHOST, 0, 0, 0), None).unwrap()
    }

    fn option(socket: BorrowedFd<'_>, level: i32, name: i32) -> i32 {
        let mut value = 0i32;
        let mut len = size_of::<i32>() as libc::socklen_t;
        // SAFETY: value and len are writable and describe an integer option.
        assert_eq!(
            unsafe {
                libc::getsockopt(
                    socket.as_raw_fd(),
                    level,
                    name,
                    (&mut value as *mut i32).cast(),
                    &mut len,
                )
            },
            0
        );
        assert_eq!(len as usize, size_of::<i32>());
        value
    }

    #[test]
    fn stable_dynamic_ports_bidirectional_fixed_port_delivery_and_truncation() {
        let a = receiver();
        let b = receiver();
        let lanes: Vec<_> = (0..4)
            .map(|_| TransmitSocket::connect(&a, b.local_addr()).unwrap())
            .collect();
        let reply = TransmitSocket::connect(&b, a.local_addr()).unwrap();
        let mut ports = Vec::new();
        for lane in &lanes {
            let source = lane.local_addr().unwrap();
            assert_ne!(source.port(), a.local_addr().port());
            assert!(!ports.contains(&source.port()));
            ports.push(source.port());
            lane.send(b"hello").unwrap();
            let mut bytes = [0; 32];
            let (n, from) = b.recv(&mut bytes).unwrap();
            assert_eq!(&bytes[..n], b"hello");
            assert_eq!(from, source);
            lane.send(b"again").unwrap();
            assert_eq!(b.recv(&mut bytes).unwrap().1, source);
        }
        reply.send(b"reply").unwrap();
        let mut bytes = [0; 32];
        assert_eq!(a.recv(&mut bytes).unwrap().1, reply.local_addr().unwrap());
        assert_eq!(
            a.recv(&mut bytes).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        lanes[0].send(&[0; BASE_UDP_PAYLOAD]).unwrap();
        assert_eq!(
            b.recv(&mut bytes).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(
            b.recv(&mut bytes).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(
            lanes[0]
                .send(&[0; BASE_UDP_PAYLOAD + 1])
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn socket_options_enforce_wire_and_descriptor_policy() {
        let rx = receiver();
        let tx = TransmitSocket::connect(&rx, rx.local_addr()).unwrap();
        assert_eq!(option(rx.as_fd(), libc::SOL_SOCKET, libc::SO_REUSEPORT), 1);
        for fd in [rx.as_fd(), tx.as_fd()] {
            assert_eq!(option(fd, libc::IPPROTO_IPV6, libc::IPV6_V6ONLY), 1);
            // SAFETY: these fcntl operations take no pointer arguments.
            assert_ne!(
                unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) } & libc::O_NONBLOCK,
                0
            );
            assert_ne!(
                unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
                0
            );
        }
        assert_eq!(
            option(tx.as_fd(), libc::IPPROTO_IPV6, libc::IPV6_DONTFRAG),
            1
        );
        assert_eq!(
            option(tx.as_fd(), libc::IPPROTO_IPV6, libc::IPV6_MTU_DISCOVER),
            libc::IPV6_PMTUDISC_DO
        );
        assert_eq!(option(tx.as_fd(), libc::IPPROTO_IPV6, libc::IPV6_TCLASS), 0);
        assert!(tx.take_error().unwrap().is_none());
    }

    #[test]
    fn batch_scatter_send_and_truncation_preserve_datagram_boundaries() {
        let rx = receiver();
        let tx = TransmitSocket::connect(&rx, rx.local_addr()).unwrap();
        let outgoing = [
            SendDatagram {
                prefix: b"head",
                payload: b"tail",
            },
            SendDatagram {
                prefix: &[7; BASE_UDP_PAYLOAD],
                payload: &[],
            },
            SendDatagram {
                prefix: b"last",
                payload: &[],
            },
        ];
        assert_eq!(tx.send_batch(&outgoing).unwrap(), 3);
        let mut buffers = [[0; 8]; 4];
        let mut incoming: Vec<_> = buffers
            .iter_mut()
            .map(|b| ReceiveDatagram::new(b))
            .collect();
        incoming[3].length = 99;
        assert_eq!(rx.recv_batch(&mut incoming).unwrap(), 3);
        assert_eq!(incoming[0].buffer, b"headtail");
        assert_eq!(incoming[0].length, 8);
        assert!(!incoming[0].truncated);
        assert_eq!(incoming[1].length, BASE_UDP_PAYLOAD);
        assert!(incoming[1].truncated);
        assert_eq!(&incoming[2].buffer[..incoming[2].length], b"last");
        assert!(!incoming[2].truncated);
        assert_eq!(incoming[3].length, 99);
        assert_eq!(
            rx.recv_batch(&mut incoming).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(incoming[3].length, 99);
        let invalid = [
            outgoing[0],
            SendDatagram {
                prefix: &[0; BASE_UDP_PAYLOAD],
                payload: &[1],
            },
        ];
        assert_eq!(
            tx.send_batch(&invalid).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            rx.recv_batch(&mut incoming).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert!(tx.send_batch(&[]).is_err());
        assert!(rx.recv_batch(&mut []).is_err());
    }

    #[test]
    fn partial_sendmmsg_returns_only_sent_prefix_and_retries_preserve_order() {
        use std::os::unix::net::UnixDatagram;
        let (tx, rx) = UnixDatagram::pair().unwrap();
        tx.set_nonblocking(true).unwrap();
        rx.set_nonblocking(true).unwrap();
        let capacity = 2048_i32;
        // SAFETY: capacity is a readable integer socket option.
        cvt(unsafe {
            libc::setsockopt(
                tx.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                (&capacity as *const i32).cast(),
                size_of::<i32>() as libc::socklen_t,
            )
        })
        .unwrap();
        let prefixes: Vec<_> = (0..BATCH_SIZE as u8).map(|id| [id; 16]).collect();
        let payload = [255; 1000];
        let outgoing: Vec<_> = prefixes
            .iter()
            .map(|prefix| SendDatagram {
                prefix,
                payload: &payload,
            })
            .collect();
        let mut sent = 0;
        let mut calls = 0;
        while sent < BATCH_SIZE {
            let count = send_batch(tx.as_fd(), &outgoing[sent..]).unwrap();
            assert!(count > 0 && count < BATCH_SIZE);
            let mut input = [0; 1016];
            for expected in sent..sent + count {
                assert_eq!(rx.recv(&mut input).unwrap(), input.len());
                assert_eq!(&input[..16], &[expected as u8; 16]);
                assert_eq!(&input[16..], &payload);
            }
            sent += count;
            calls += 1;
        }
        assert!(calls > 1);
        assert_eq!(
            rx.recv(&mut [0; 1]).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn rejects_invalid_addresses_before_io() {
        let rx = receiver();
        for text in [
            "[::]:42",
            "[::1]:0",
            "[::ffff:127.0.0.1]:42",
            "[ff02::1]:42",
            "[fe80::1]:42",
        ] {
            assert!(TransmitSocket::connect(&rx, text.parse().unwrap()).is_err());
        }
        assert!(ReceiveSocket::bind("[::1]:0".parse().unwrap(), None).is_err());
        assert!(ReceiveSocket::bind("[::1]:40000".parse().unwrap(), Some(c"")).is_err());
        assert!(
            ReceiveSocket::bind(SocketAddrV6::new(Ipv6Addr::LOCALHOST, 40000, 1, 0), None).is_err()
        );
    }
}
