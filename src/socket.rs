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
    cell::Cell,
    ffi::{CStr, CString},
    io,
    mem::{MaybeUninit, size_of, zeroed},
    net::{Ipv6Addr, SocketAddr, SocketAddrV6, UdpSocket},
    os::fd::{AsFd, AsRawFd, BorrowedFd},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

/// Maximum number of logical datagrams in one batched syscall.
pub const BATCH_SIZE: usize = 64;
/// UDP payload ceiling for a 9000-byte outer IPv6 packet without extensions.
/// This bounds storage and probing; it does not establish a usable path MTU.
pub const MAX_UDP_PAYLOAD: usize = 9000 - 40 - 8;
const MAX_VECTORS: usize = 1024;
const RECEIVE_QUEUE_BYTES: i32 = 8 * 1024 * 1024;
pub(crate) const GSO_MAX_BYTES: usize = 65_535 - 40 - 8;

#[repr(C)]
struct Ancillary {
    header: libc::cmsghdr,
    data: [u8; size_of::<usize>()],
}

#[repr(C)]
struct ErrorControl {
    header: libc::cmsghdr,
    error: libc::sock_extended_err,
    offender: libc::sockaddr_in6,
}

/// A size error consumed from the transmit socket's IPv6 error queue.
///
/// Remote reports are untrusted. Match the destination and quoted protocol
/// identity against retained successful sends before applying their size hint.
pub struct MtuError {
    /// Reported outer IPv6 MTU, including the IP header.
    pub mtu: u32,
    /// Whether the kernel generated this error locally instead of receiving ICMP.
    pub local: bool,
    /// Original outgoing destination, not the ICMP sender.
    pub destination: SocketAddrV6,
    quote: [u8; crate::wire::HEADER_LEN],
    quote_len: usize,
}

impl MtuError {
    /// Available prefix of the original UDP payload, at most one wire header.
    /// Local errors can have an empty quote; short remote quotes cannot prove identity.
    pub fn quoted_payload(&self) -> &[u8] {
        &self.quote[..self.quote_len]
    }
}

/// One outgoing datagram gathered from borrowed wire prefixes and frame pieces.
#[derive(Clone, Copy)]
pub struct SendDatagram<'a> {
    /// Slices concatenated in order by the kernel, without a payload copy.
    pub parts: &'a [&'a [u8]],
}

/// One caller-owned receive buffer and its result metadata.
pub struct ReceiveDatagram<'a> {
    /// Storage filled directly by recvmmsg.
    pub buffer: &'a mut [u8],
    /// Whole datagram length, which can exceed buffer.len() on truncation.
    pub length: usize,
    /// A truncated entry is consumed but must never be parsed.
    pub truncated: bool,
    /// GRO segment size, or zero for an ordinary datagram.
    pub segment_size: usize,
    /// Cumulative socket drops in kernel skb units, when supplied by SO_RXQ_OVFL.
    pub overflow: Option<u32>,
}

impl<'a> ReceiveDatagram<'a> {
    /// Describes writable storage without allocation.
    pub const fn new(buffer: &'a mut [u8]) -> Self {
        Self {
            buffer,
            length: 0,
            truncated: false,
            segment_size: 0,
            overflow: None,
        }
    }

    /// Borrows each logical UDP payload, including a shorter final GRO segment.
    ///
    /// Rejects payload or ancillary truncation; each returned slice still needs
    /// independent wire/cookie/state validation. Empty UDP is one empty slice.
    pub fn segments(&self) -> io::Result<impl ExactSizeIterator<Item = &[u8]>> {
        if self.truncated || self.length > self.buffer.len() {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let size = if self.segment_size == 0 {
            self.length.max(1)
        } else {
            self.segment_size
        };
        Ok((0..self.length.div_ceil(size).max(1))
            .map(move |index| &self.buffer[index * size..((index + 1) * size).min(self.length)]))
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
    gro: bool,
    pub(crate) buffer_fallback: bool,
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
        set_int(&socket, libc::SOL_SOCKET, libc::SO_RXQ_OVFL, 1)?;
        let buffer_fallback = match set_int(
            &socket,
            libc::SOL_SOCKET,
            libc::SO_RCVBUFFORCE,
            RECEIVE_QUEUE_BYTES,
        ) {
            Ok(()) => false,
            Err(error) if error.raw_os_error() == Some(libc::EPERM) => {
                set_int(
                    &socket,
                    libc::SOL_SOCKET,
                    libc::SO_RCVBUF,
                    RECEIVE_QUEUE_BYTES,
                )?;
                true
            }
            Err(error) => return Err(error),
        };
        bind(&socket, address)?;
        let address = local_address(&socket)?;
        Ok(Self {
            socket,
            address,
            device: device.map(CStr::to_owned),
            gro: false,
            buffer_fallback,
        })
    }

    /// Kernel receive-queue accounting limit, including Linux's doubled overhead allowance.
    ///
    /// Setup requests 8 MiB with SO_RCVBUFFORCE; on EPERM it uses SO_RCVBUF,
    /// which may be capped by the kernel's rmem_max.
    pub fn receive_buffer_limit(&self) -> io::Result<usize> {
        let mut value = 0_i32;
        let mut length = size_of::<i32>() as libc::socklen_t;
        // SAFETY: value and length describe writable integer option storage.
        cvt(unsafe {
            libc::getsockopt(
                self.socket.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVBUF,
                (&mut value as *mut i32).cast(),
                &mut length,
            )
        })?;
        if length as usize != size_of::<i32>() || value < 0 {
            return Err(io::ErrorKind::InvalidData.into());
        }
        Ok(value as usize)
    }

    pub(crate) fn steer_group(&self, members: usize) -> io::Result<()> {
        if !(1..=64).contains(&members) {
            return Err(invalid("receive group requires 1..64 sockets"));
        }
        let instruction = |code: u32, k| libc::sock_filter {
            code: code as u16,
            jt: 0,
            jf: 0,
            k,
        };
        // UDP payload offsets; cBPF arithmetic wraps at 32 bits.
        let mut code = [
            instruction(libc::BPF_LD | libc::BPF_H | libc::BPF_ABS, 36),
            instruction(libc::BPF_ALU | libc::BPF_SUB | libc::BPF_K, 1),
            instruction(libc::BPF_MISC | libc::BPF_TAX, 0),
            instruction(libc::BPF_LD | libc::BPF_W | libc::BPF_ABS, 16),
            instruction(libc::BPF_ALU | libc::BPF_ADD | libc::BPF_X, 0),
            instruction(libc::BPF_ALU | libc::BPF_MOD | libc::BPF_K, members as u32),
            instruction(libc::BPF_RET | libc::BPF_A, 0),
        ];
        let program = libc::sock_fprog {
            len: code.len() as u16,
            filter: code.as_mut_ptr(),
        };
        // SAFETY: the initialized program and instructions live through setsockopt.
        cvt(unsafe {
            libc::setsockopt(
                self.socket.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_ATTACH_REUSEPORT_CBPF,
                (&program as *const libc::sock_fprog).cast(),
                size_of::<libc::sock_fprog>() as libc::socklen_t,
            )
        })?;
        Ok(())
    }

    /// Enables GRO when supported; false leaves ordinary receives in use.
    ///
    /// Call during socket setup. Thereafter use recv_batch and split each result
    /// with ReceiveDatagram::segments; recv cannot represent a GRO aggregate.
    pub fn enable_gro(&mut self) -> io::Result<bool> {
        if self.gro {
            return Ok(true);
        }
        match set_int(&self.socket, libc::IPPROTO_UDP, libc::UDP_GRO, 1) {
            Ok(()) => {
                self.gro = true;
                Ok(true)
            }
            Err(error) if offload_unavailable(&error) => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Whether GRO was enabled successfully on this socket.
    pub const fn gro_enabled(&self) -> bool {
        self.gro
    }

    /// Receives up to BATCH_SIZE buffers, each containing one or more datagrams.
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
        if self.gro {
            return Err(io::ErrorKind::InvalidInput.into());
        }
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
/// The caller supplies a validated per-lane payload limit. This API supplies no
/// path probing, protocol scheduling or congestion control.
pub struct TransmitSocket {
    socket: UdpSocket,
    gso: AtomicBool,
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
        Ok(Self {
            socket,
            gso: AtomicBool::new(true),
        })
    }

    /// Returns the selected source address and dynamic port.
    pub fn local_addr(&self) -> io::Result<SocketAddrV6> {
        local_address(&self.socket)
    }

    /// Enables application MTU discovery while continuing to forbid fragmentation.
    ///
    /// Call during lane setup. PROBE bypasses cached path-MTU enforcement, so the
    /// owner must restrict DATA to its confirmed size and track padded probes.
    /// Drain size errors with recv_mtu_error when the socket reports EPOLLERR.
    pub fn enable_mtu_discovery(&mut self) -> io::Result<()> {
        set_int(&self.socket, libc::IPPROTO_IPV6, libc::IPV6_RECVERR, 1)?;
        set_int(
            &self.socket,
            libc::IPPROTO_IPV6,
            libc::IPV6_MTU_DISCOVER,
            libc::IPV6_PMTUDISC_PROBE,
        )
    }

    /// Consumes one error-queue entry without allocation, retaining its header quote.
    ///
    /// None means an unrelated or malformed entry was consumed. WouldBlock means
    /// the queue is empty. Bound calls per worker turn even under an ICMP flood.
    /// Payload truncation is intentional; truncated ancillary metadata is rejected.
    pub fn recv_mtu_error(&self) -> io::Result<Option<MtuError>> {
        let mut quote = [0; crate::wire::HEADER_LEN];
        // SAFETY: all-zero headers and sockaddr_in6 are valid writable ABI storage.
        let (mut source, mut control, mut message): (
            libc::sockaddr_in6,
            ErrorControl,
            libc::msghdr,
        ) = unsafe { zeroed() };
        let mut vector = libc::iovec {
            iov_base: quote.as_mut_ptr().cast(),
            iov_len: quote.len(),
        };
        message.msg_name = (&mut source as *mut libc::sockaddr_in6).cast();
        message.msg_namelen = size_of_val(&source) as _;
        message.msg_iov = &mut vector;
        message.msg_iovlen = 1;
        message.msg_control = (&mut control as *mut ErrorControl).cast();
        message.msg_controllen = size_of_val(&control) as _;
        // SAFETY: the initialized message describes live, correctly sized output buffers.
        let length = unsafe {
            libc::recvmsg(
                self.socket.as_raw_fd(),
                &mut message,
                libc::MSG_ERRQUEUE | libc::MSG_DONTWAIT | libc::MSG_TRUNC,
            )
        };
        if length < 0 {
            return Err(io::Error::last_os_error());
        }
        let Some((mtu, local)) = mtu_error(&message, &control) else {
            return Ok(None);
        };
        if message.msg_namelen as usize != size_of_val(&source)
            || source.sin6_family != libc::AF_INET6 as libc::sa_family_t
        {
            return Ok(None);
        }
        Ok(Some(MtuError {
            mtu,
            local,
            destination: SocketAddrV6::new(
                Ipv6Addr::from(source.sin6_addr.s6_addr),
                u16::from_be(source.sin6_port),
                source.sin6_flowinfo,
                source.sin6_scope_id,
            ),
            quote_len: (length as usize).min(quote.len()),
            quote,
        }))
    }

    /// Whether GSO attempts remain enabled; support is learned on actual sends.
    pub fn gso_enabled(&self) -> bool {
        self.gso.load(Ordering::Relaxed)
    }

    /// Sends one complete UDP payload within a caller-supplied lane limit.
    ///
    /// Limits must be 1232 through 8952 bytes; callers start at 1232 and raise
    /// them after path confirmation, an explicit operator assertion, or for a probe.
    ///
    /// Returns kernel errors, including `WouldBlock` and `EMSGSIZE`, unchanged.
    /// A successful return means socket submission, not remote acceptance.
    pub fn send(&self, payload: &[u8], limit: usize) -> io::Result<()> {
        if !(BASE_UDP_PAYLOAD..=MAX_UDP_PAYLOAD).contains(&limit) || payload.len() > limit {
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
    /// All entries must fit the supplied 1232..=8952-byte lane limit. Adjacent
    /// equal-sized datagrams (with an optional shorter last one) share a GSO
    /// message without payload copies. Offload capability errors disable GSO and
    /// retry through ordinary sendmmsg. Partial results count logical datagrams,
    /// never GSO messages; retain the unsent suffix with its packet identities.
    /// Each call submits at most 1024 slices, possibly shortening the batch.
    /// A single datagram with more slices is rejected before any submission.
    pub fn send_batch(&self, datagrams: &[SendDatagram<'_>], limit: usize) -> io::Result<usize> {
        self.send_batch_checked(datagrams, limit, || Ok(Duration::ZERO))
            .map(|(sent, _)| sent)
    }

    /// Checks permission and captures time immediately before each nonblocking syscall.
    ///
    /// The callback runs after validation and message construction, including on
    /// an ordinary-send retry after GSO failure. Return an error to prevent that
    /// attempt. Callback errors never change the learned GSO capability.
    /// The returned timestamp belongs only to the successful packet prefix.
    /// Use it for sent history; a post-syscall timestamp can be later than an ACK.
    pub fn send_batch_checked(
        &self,
        datagrams: &[SendDatagram<'_>],
        limit: usize,
        mut before: impl FnMut() -> io::Result<Duration>,
    ) -> io::Result<(usize, Duration)> {
        let gso = self.gso_enabled();
        let attempted = Cell::new(false);
        let mut check = || {
            let at = before()?;
            attempted.set(true);
            Ok(at)
        };
        match send_batch(self.socket.as_fd(), datagrams, limit, gso, &mut check) {
            Err(error) if attempted.get() && gso && offload_unavailable(&error) => {
                self.gso.store(false, Ordering::Relaxed);
                send_batch(self.socket.as_fd(), datagrams, limit, false, &mut check)
            }
            result => result,
        }
    }

    /// Retrieves and clears the socket's pending asynchronous error.
    pub fn take_error(&self) -> io::Result<Option<io::Error>> {
        self.socket.take_error()
    }
}

fn mtu_error(message: &libc::msghdr, control: &ErrorControl) -> Option<(u32, bool)> {
    // SAFETY: only computes the minimum ABI length for the fixed error record.
    let minimum = unsafe { libc::CMSG_LEN(size_of::<libc::sock_extended_err>() as _) } as usize;
    let length = control.header.cmsg_len as usize;
    if message.msg_flags & libc::MSG_CTRUNC != 0
        || length < minimum
        || length > message.msg_controllen as usize
        || message.msg_controllen as usize > size_of::<ErrorControl>()
        || control.header.cmsg_level != libc::IPPROTO_IPV6
        || control.header.cmsg_type != libc::IPV6_RECVERR
        || control.error.ee_errno != libc::EMSGSIZE as u32
    {
        return None;
    }
    let error = &control.error;
    match (error.ee_origin, error.ee_type, error.ee_code) {
        (libc::SO_EE_ORIGIN_LOCAL, 0, 0) => Some((error.ee_info, true)),
        (libc::SO_EE_ORIGIN_ICMP6, 2, 0) => Some((error.ee_info, false)),
        _ => None,
    }
}

impl AsFd for TransmitSocket {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.socket.as_fd()
    }
}

fn offload_unavailable(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::ENOPROTOOPT | libc::EOPNOTSUPP | libc::EINVAL | libc::EIO)
    )
}

fn send_batch(
    fd: BorrowedFd<'_>,
    datagrams: &[SendDatagram<'_>],
    limit: usize,
    gso: bool,
    mut before: impl FnMut() -> io::Result<Duration>,
) -> io::Result<(usize, Duration)> {
    if datagrams.is_empty()
        || datagrams.len() > BATCH_SIZE
        || !(BASE_UDP_PAYLOAD..=MAX_UDP_PAYLOAD).contains(&limit)
    {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let mut lengths = [0; BATCH_SIZE];
    for (data, length) in datagrams.iter().zip(&mut lengths) {
        if data.parts.len() > MAX_VECTORS {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        *length = data
            .parts
            .iter()
            .try_fold(0_usize, |sum, part| {
                sum.checked_add(part.len()).filter(|n| *n <= limit)
            })
            .ok_or(io::ErrorKind::InvalidInput)?;
    }
    let mut vectors = [MaybeUninit::<libc::iovec>::uninit(); MAX_VECTORS];
    // SAFETY: zeroed headers/control buffers represent empty messages.
    let mut messages: [libc::mmsghdr; BATCH_SIZE] = unsafe { zeroed() };
    // SAFETY: all-zero cmsghdr and bytes are valid writable storage.
    let mut controls: [Ancillary; BATCH_SIZE] = unsafe { zeroed() };
    let mut ends = [0; BATCH_SIZE];
    let mut count = 0;
    let mut first = 0;
    let mut used = 0;
    while first < datagrams.len() && used + datagrams[first].parts.len() <= MAX_VECTORS {
        let segment = lengths[first];
        let base = used;
        let mut slices = datagrams[first].parts.len();
        let mut end = first + 1;
        let mut bytes = segment;
        if gso && segment != 0 {
            while end < datagrams.len() {
                let next = lengths[end];
                if next == 0
                    || next > segment
                    || bytes + next > GSO_MAX_BYTES
                    || used + slices + datagrams[end].parts.len() > MAX_VECTORS
                {
                    break;
                }
                bytes += next;
                slices += datagrams[end].parts.len();
                end += 1;
                if next < segment {
                    break;
                }
            }
        }
        for data in &datagrams[first..end] {
            for part in data.parts {
                vectors[used].write(libc::iovec {
                    iov_base: part.as_ptr().cast_mut().cast(),
                    iov_len: part.len(),
                });
                used += 1;
            }
        }
        let message = &mut messages[count].msg_hdr;
        message.msg_iov = vectors[base..].as_mut_ptr().cast();
        message.msg_iovlen = slices as _;
        if end - first > 1 {
            let control = &mut controls[count];
            // SAFETY: constant length fits this aligned ancillary structure.
            control.header.cmsg_len = unsafe { libc::CMSG_LEN(2) } as _;
            control.header.cmsg_level = libc::IPPROTO_UDP;
            control.header.cmsg_type = libc::UDP_SEGMENT;
            control.data[..2].copy_from_slice(&(segment as u16).to_ne_bytes());
            message.msg_control = (control as *mut Ancillary).cast();
            message.msg_controllen = size_of::<Ancillary>() as _;
        }
        ends[count] = end;
        count += 1;
        first = end;
    }
    let at = before()?;
    // SAFETY: each header borrows live datagram slices and aligned ancillary
    // storage. All referenced iovecs were initialized above; unused slots are
    // outside msg_iovlen. The kernel reads payloads and writes only message lengths.
    let sent = cvt(unsafe {
        libc::sendmmsg(
            fd.as_raw_fd(),
            messages.as_mut_ptr(),
            count as u32,
            (libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL) as _,
        )
    })? as usize;
    Ok((if sent == 0 { 0 } else { ends[sent - 1] }, at))
}

/// Receives up to BATCH_SIZE entries from a borrowed UDP socket without blocking.
///
/// Supports IPv4 and IPv6, including connected sockets. The caller enables
/// UDP_GRO and SO_RXQ_OVFL when wanted; source addresses are not requested.
/// Each entry reports truncation independently, preserving valid siblings.
pub fn recv_batch(fd: BorrowedFd<'_>, datagrams: &mut [ReceiveDatagram<'_>]) -> io::Result<usize> {
    if datagrams.is_empty() || datagrams.len() > BATCH_SIZE {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let mut vectors = [libc::iovec {
        iov_base: std::ptr::null_mut(),
        iov_len: 0,
    }; BATCH_SIZE];
    // SAFETY: zeroed headers request no source addresses.
    let mut messages: [libc::mmsghdr; BATCH_SIZE] = unsafe { zeroed() };
    // SAFETY: all-zero cmsghdr and bytes are valid writable storage.
    let mut controls: [[Ancillary; 2]; BATCH_SIZE] = unsafe { zeroed() };
    for (i, datagram) in datagrams.iter_mut().enumerate() {
        vectors[i] = libc::iovec {
            iov_base: datagram.buffer.as_mut_ptr().cast(),
            iov_len: datagram.buffer.len(),
        };
        messages[i].msg_hdr.msg_iov = &mut vectors[i];
        messages[i].msg_hdr.msg_iovlen = 1;
        messages[i].msg_hdr.msg_control = controls[i].as_mut_ptr().cast();
        messages[i].msg_hdr.msg_controllen = size_of::<[Ancillary; 2]>() as _;
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
    for ((message, control), datagram) in messages[..count].iter().zip(&controls).zip(datagrams) {
        datagram.length = message.msg_len as usize;
        let metadata = receive_control(&message.msg_hdr, control);
        let (segment, overflow) = metadata.unwrap_or((0, None));
        datagram.segment_size = segment;
        datagram.overflow = overflow;
        datagram.truncated = message.msg_hdr.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0
            || datagram.length > datagram.buffer.len()
            || metadata.is_none();
    }
    Ok(count)
}

fn receive_control(message: &libc::msghdr, controls: &[Ancillary]) -> Option<(usize, Option<u32>)> {
    let mut remaining = message.msg_controllen as usize;
    if remaining > std::mem::size_of_val(controls) {
        return None;
    }
    // SAFETY: only computes the platform control-message length.
    let expected = unsafe { libc::CMSG_LEN(4) } as usize;
    let mut segment = 0;
    let mut overflow = None;
    for control in controls {
        if remaining == 0 {
            break;
        }
        if remaining < expected || control.header.cmsg_len as usize != expected {
            return None;
        }
        let value = u32::from_ne_bytes(*control.data.first_chunk::<4>()?);
        match (control.header.cmsg_level, control.header.cmsg_type) {
            (libc::IPPROTO_UDP, libc::UDP_GRO) if segment == 0 && (1..=65535).contains(&value) => {
                segment = value as usize;
            }
            (libc::SOL_SOCKET, libc::SO_RXQ_OVFL) if overflow.is_none() => overflow = Some(value),
            _ => return None,
        }
        remaining = remaining.saturating_sub(size_of::<Ancillary>());
    }
    Some((segment, overflow))
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
    fn gso_and_plain_sends_preserve_every_logical_boundary_with_gro() {
        for gso in [false, true] {
            let mut rx = receiver();
            rx.enable_gro().unwrap();
            let tx = TransmitSocket::connect(&rx, rx.local_addr()).unwrap();
            tx.gso.store(gso, Ordering::Relaxed);
            let lengths = [1232, 1232, 700, 1452, 1452, 8952, 8952, 0, 20, 20, 10];
            let data: Vec<_> = lengths
                .iter()
                .enumerate()
                .map(|(i, &n)| vec![i as u8; n])
                .collect();
            let parts: Vec<_> = data
                .iter()
                .map(|bytes| {
                    let split = bytes.len().min(7);
                    [&bytes[..split], &bytes[split..]]
                })
                .collect();
            let outgoing: Vec<_> = parts.iter().map(|parts| SendDatagram { parts }).collect();
            assert_eq!(
                tx.send_batch(&outgoing, MAX_UDP_PAYLOAD).unwrap(),
                lengths.len()
            );
            let mut buffers = vec![[0; 65_536]; lengths.len()];
            let mut incoming: Vec<_> = buffers
                .iter_mut()
                .map(|b| ReceiveDatagram::new(b))
                .collect();
            let count = rx.recv_batch(&mut incoming).unwrap();
            let mut logical = 0;
            for received in &incoming[..count] {
                for packet in received.segments().unwrap() {
                    assert_eq!(packet, data[logical]);
                    logical += 1;
                }
            }
            assert_eq!(logical, lengths.len());
            if gso && tx.gso_enabled() && rx.gro_enabled() {
                assert_eq!(incoming[0].segment_size, 1232);
                assert_eq!(incoming[0].length, 1232 * 2 + 700);
                assert!(count < logical);
                assert_eq!(
                    rx.recv(&mut [0; 8]).unwrap_err().kind(),
                    io::ErrorKind::InvalidInput
                );
            }
        }
    }

    #[test]
    fn gso_limits_split_large_batches_and_truncation_does_not_hide_siblings() {
        let mut rx = receiver();
        rx.enable_gro().unwrap();
        let tx = TransmitSocket::connect(&rx, rx.local_addr()).unwrap();
        let payload = [9; MAX_UDP_PAYLOAD];
        let parts = [&payload[..]];
        let outgoing = [SendDatagram { parts: &parts }; BATCH_SIZE];
        // Cross the GSO aggregate limit without overflowing a capped receive queue.
        let expected = GSO_MAX_BYTES / MAX_UDP_PAYLOAD + 1;
        assert_eq!(
            tx.send_batch(&outgoing[..expected], MAX_UDP_PAYLOAD)
                .unwrap(),
            expected
        );
        let mut buffers = vec![vec![0; 65_536]; BATCH_SIZE];
        let mut incoming: Vec<_> = buffers
            .iter_mut()
            .map(|b| ReceiveDatagram::new(b))
            .collect();
        let count = rx.recv_batch(&mut incoming).unwrap();
        let mut logical = 0;
        for received in &incoming[..count] {
            for packet in received.segments().unwrap() {
                assert_eq!(packet, payload);
                logical += 1;
            }
        }
        assert_eq!(logical, expected);
        if rx.gro_enabled() && tx.gso_enabled() {
            assert!(count >= 2);
        }

        tx.send_batch(&outgoing[..2], MAX_UDP_PAYLOAD).unwrap();
        tx.send(b"last", BASE_UDP_PAYLOAD).unwrap();
        let mut small = [[0; 8]; 4];
        let mut incoming: Vec<_> = small.iter_mut().map(|b| ReceiveDatagram::new(b)).collect();
        let count = rx.recv_batch(&mut incoming).unwrap();
        assert!(incoming[0].truncated);
        assert!(incoming[0].segments().is_err());
        assert_eq!(
            incoming[count - 1].segments().unwrap().next().unwrap(),
            b"last"
        );
    }

    #[test]
    fn checksum_offload_failure_retries_without_gso_and_mtu_errors_survive() {
        let rx = receiver();
        let tx = TransmitSocket::connect(&rx, rx.local_addr()).unwrap();
        // Force a real GSO-incompatible socket setting; only these test sockets omit checksums.
        set_int(&tx.socket, libc::IPPROTO_UDP, libc::UDP_NO_CHECK6_TX, 1).unwrap();
        set_int(&rx.socket, libc::IPPROTO_UDP, libc::UDP_NO_CHECK6_RX, 1).unwrap();
        let outgoing = [SendDatagram {
            parts: &[b"prefix", b"payload"],
        }; 2];
        assert_eq!(tx.send_batch(&outgoing, BASE_UDP_PAYLOAD).unwrap(), 2);
        assert!(!tx.gso_enabled());
        for _ in 0..2 {
            let mut bytes = [0; 32];
            let (n, _) = rx.recv(&mut bytes).unwrap();
            assert_eq!(&bytes[..n], b"prefixpayload");
        }
        set_int(&tx.socket, libc::IPPROTO_UDP, libc::UDP_NO_CHECK6_TX, 0).unwrap();
        set_int(&tx.socket, libc::IPPROTO_IPV6, libc::IPV6_MTU, 1280).unwrap();
        tx.gso.store(true, Ordering::Relaxed);
        let large = [SendDatagram {
            parts: &[&[0; MAX_UDP_PAYLOAD], &[]],
        }; 2];
        assert_eq!(
            tx.send_batch(&large, MAX_UDP_PAYLOAD)
                .unwrap_err()
                .raw_os_error(),
            Some(libc::EMSGSIZE)
        );
        assert!(
            tx.gso_enabled(),
            "an MTU error is not an offload capability failure"
        );
        assert_eq!(
            rx.recv(&mut [0; 32]).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        for limit in [0, BASE_UDP_PAYLOAD - 1, MAX_UDP_PAYLOAD + 1, usize::MAX] {
            assert_eq!(
                tx.send_batch(&outgoing, limit).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
            assert_eq!(
                tx.send(b"small", limit).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
    }

    #[test]
    fn checked_batch_returns_successful_attempt_time_including_gso_fallback() {
        for fallback in [false, true] {
            let rx = receiver();
            let tx = TransmitSocket::connect(&rx, rx.local_addr()).unwrap();
            if fallback {
                set_int(&tx.socket, libc::IPPROTO_UDP, libc::UDP_NO_CHECK6_TX, 1).unwrap();
                set_int(&rx.socket, libc::IPPROTO_UDP, libc::UDP_NO_CHECK6_RX, 1).unwrap();
            }
            let outgoing = [SendDatagram {
                parts: &[&[7; BASE_UDP_PAYLOAD]],
            }; 2];
            let mut calls = 0;
            let (sent, at) = tx
                .send_batch_checked(&outgoing, BASE_UDP_PAYLOAD, || {
                    calls += 1;
                    Ok(Duration::from_micros(calls))
                })
                .unwrap();
            assert_eq!(sent, 2);
            assert_eq!(calls, if fallback { 2 } else { 1 });
            assert_eq!(at, Duration::from_micros(calls));
            assert_eq!(tx.gso_enabled(), !fallback);
            for _ in 0..2 {
                let mut bytes = [0; BASE_UDP_PAYLOAD];
                assert_eq!(rx.recv(&mut bytes).unwrap().0, BASE_UDP_PAYLOAD);
                assert!(bytes.iter().all(|byte| *byte == 7));
            }
        }
    }

    #[test]
    fn checked_batch_does_not_submit_or_disable_gso_on_callback_failure() {
        let rx = receiver();
        let tx = TransmitSocket::connect(&rx, rx.local_addr()).unwrap();
        let outgoing = [SendDatagram {
            parts: &[b"payload"],
        }; 2];
        for errno in [libc::EINVAL, libc::EIO, libc::EAGAIN] {
            let mut calls = 0;
            let error = tx
                .send_batch_checked(&outgoing, BASE_UDP_PAYLOAD, || {
                    calls += 1;
                    Err(io::Error::from_raw_os_error(errno))
                })
                .unwrap_err();
            assert_eq!(error.raw_os_error(), Some(errno));
            assert_eq!(calls, 1);
            assert!(tx.gso_enabled());
        }
        assert!(
            tx.send_batch_checked(&[], BASE_UDP_PAYLOAD, || panic!(
                "invalid input must not reach the callback"
            ))
            .is_err()
        );
        assert_eq!(
            rx.recv(&mut [0; 32]).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn deadline_passing_between_gso_failure_and_plain_retry_prevents_submission() {
        let rx = receiver();
        let tx = TransmitSocket::connect(&rx, rx.local_addr()).unwrap();
        set_int(&tx.socket, libc::IPPROTO_UDP, libc::UDP_NO_CHECK6_TX, 1).unwrap();
        set_int(&rx.socket, libc::IPPROTO_UDP, libc::UDP_NO_CHECK6_RX, 1).unwrap();
        let deadline = Duration::from_micros(100);
        let outgoing = [SendDatagram {
            parts: &[&[7; BASE_UDP_PAYLOAD]],
        }; 2];
        let mut calls = 0;
        let error = tx
            .send_batch_checked(&outgoing, BASE_UDP_PAYLOAD, || {
                let at = if calls == 0 { Duration::ZERO } else { deadline };
                calls += 1;
                if at < deadline {
                    Ok(at)
                } else {
                    Err(io::ErrorKind::WouldBlock.into())
                }
            })
            .unwrap_err();
        assert_eq!(calls, 2);
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert!(!tx.gso_enabled()); // A real capability failure preceded the rejected retry.
        assert_eq!(
            rx.recv(&mut [0; BASE_UDP_PAYLOAD]).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn discovery_preserves_dontfrag_and_reads_local_mtu_errors_without_payload() {
        let rx = receiver();
        let mut tx = TransmitSocket::connect(&rx, rx.local_addr()).unwrap();
        tx.enable_mtu_discovery().unwrap();
        assert_eq!(
            option(tx.as_fd(), libc::IPPROTO_IPV6, libc::IPV6_DONTFRAG),
            1
        );
        assert_eq!(
            option(tx.as_fd(), libc::IPPROTO_IPV6, libc::IPV6_MTU_DISCOVER),
            libc::IPV6_PMTUDISC_PROBE
        );
        assert_eq!(
            option(tx.as_fd(), libc::IPPROTO_IPV6, libc::IPV6_RECVERR),
            1
        );
        set_int(&tx.socket, libc::IPPROTO_IPV6, libc::IPV6_MTU, 1280).unwrap();
        assert_eq!(
            tx.send(&[0; 1452], 1452).unwrap_err().raw_os_error(),
            Some(libc::EMSGSIZE)
        );
        let error = tx
            .recv_mtu_error()
            .unwrap()
            .expect("missing local size error");
        assert!(error.local);
        assert_eq!(error.mtu, 1280);
        assert_eq!(error.destination, rx.local_addr());
        assert!(error.quoted_payload().is_empty());
        assert!(matches!(tx.recv_mtu_error(), Err(e) if e.kind() == io::ErrorKind::WouldBlock));
        tx.send(&[1; BASE_UDP_PAYLOAD], BASE_UDP_PAYLOAD).unwrap();
        let mut bytes = [0; MAX_UDP_PAYLOAD];
        assert_eq!(rx.recv(&mut bytes).unwrap().0, BASE_UDP_PAYLOAD);
        assert_eq!(&bytes[..BASE_UDP_PAYLOAD], &[1; BASE_UDP_PAYLOAD]);
    }

    #[test]
    fn mtu_error_metadata_rejects_truncation_wrong_kinds_and_short_records() {
        // SAFETY: zeroed ancillary records and msghdr are valid initialized storage.
        let (mut control, mut message): (ErrorControl, libc::msghdr) = unsafe { zeroed() };
        let minimum = unsafe { libc::CMSG_LEN(size_of::<libc::sock_extended_err>() as _) } as usize;
        assert_eq!(
            std::mem::offset_of!(ErrorControl, error),
            unsafe { libc::CMSG_LEN(0) } as usize
        );
        message.msg_controllen = size_of::<ErrorControl>() as _;
        control.header.cmsg_len = minimum as _;
        control.header.cmsg_level = libc::IPPROTO_IPV6;
        control.header.cmsg_type = libc::IPV6_RECVERR;
        control.error.ee_errno = libc::EMSGSIZE as u32;
        control.error.ee_origin = libc::SO_EE_ORIGIN_ICMP6;
        control.error.ee_type = 2;
        control.error.ee_info = 1500;
        assert_eq!(mtu_error(&message, &control), Some((1500, false)));
        for length in 0..minimum {
            control.header.cmsg_len = length as _;
            assert_eq!(mtu_error(&message, &control), None);
        }
        control.header.cmsg_len = minimum as _;
        for length in [0, minimum - 1, size_of::<ErrorControl>() + 1, usize::MAX] {
            message.msg_controllen = length as _;
            assert_eq!(mtu_error(&message, &control), None);
        }
        message.msg_controllen = size_of::<ErrorControl>() as _;
        message.msg_flags = libc::MSG_CTRUNC;
        assert_eq!(mtu_error(&message, &control), None);
        message.msg_flags = libc::MSG_TRUNC;
        assert_eq!(mtu_error(&message, &control), Some((1500, false)));
        for (origin, kind, code) in [
            (libc::SO_EE_ORIGIN_ICMP, 3, 4),
            (libc::SO_EE_ORIGIN_ICMP6, 1, 0),
            (libc::SO_EE_ORIGIN_ICMP6, 2, 1),
            (libc::SO_EE_ORIGIN_LOCAL, 2, 0),
        ] {
            control.error.ee_origin = origin;
            control.error.ee_type = kind;
            control.error.ee_code = code;
            assert_eq!(mtu_error(&message, &control), None);
        }
        control.error.ee_origin = libc::SO_EE_ORIGIN_LOCAL;
        control.error.ee_type = 0;
        control.error.ee_code = 0;
        assert_eq!(mtu_error(&message, &control), Some((1500, true)));
        control.error.ee_errno = libc::ECONNREFUSED as u32;
        assert_eq!(mtu_error(&message, &control), None);
    }

    #[test]
    fn ancillary_layout_and_malformed_gro_metadata_are_checked() {
        // SAFETY: these calls only calculate ABI sizes, and zeroed headers are valid storage.
        let (length, space, mut message, mut control): (_, _, libc::msghdr, Ancillary) =
            unsafe { (libc::CMSG_LEN(4), libc::CMSG_SPACE(4), zeroed(), zeroed()) };
        assert_eq!(size_of::<Ancillary>(), space as usize);
        // SAFETY: only computes the ancillary-data offset.
        assert_eq!(
            std::mem::offset_of!(Ancillary, data),
            unsafe { libc::CMSG_LEN(0) } as usize
        );
        assert_eq!(
            receive_control(&message, std::slice::from_ref(&control)).map(|v| v.0),
            Some(0)
        );
        message.msg_controllen = space as _;
        control.header.cmsg_len = length as _;
        control.header.cmsg_level = libc::IPPROTO_UDP;
        control.header.cmsg_type = libc::UDP_GRO;
        for size in [-1_i32, 0, 1, 1232, 65535, 65536] {
            control.data[..4].copy_from_slice(&size.to_ne_bytes());
            assert_eq!(
                receive_control(&message, std::slice::from_ref(&control)).map(|v| v.0),
                (1..=65535).contains(&size).then_some(size as usize)
            );
        }
        message.msg_controllen = (length - 1) as _;
        assert_eq!(
            receive_control(&message, std::slice::from_ref(&control)).map(|v| v.0),
            None
        );
        message.msg_controllen = space as _;
        control.header.cmsg_len = (length - 1) as _;
        assert_eq!(
            receive_control(&message, std::slice::from_ref(&control)).map(|v| v.0),
            None
        );
        let mut empty = [];
        let received = ReceiveDatagram::new(&mut empty);
        assert_eq!(
            received.segments().unwrap().collect::<Vec<_>>(),
            vec![&[] as &[u8]]
        );
    }

    #[test]
    fn gro_and_socket_overflow_metadata_coexist_in_either_order() {
        // SAFETY: zeroed ancillary headers are valid storage.
        let mut controls: [Ancillary; 2] = unsafe { zeroed() };
        let mut message: libc::msghdr = unsafe { zeroed() };
        message.msg_controllen = size_of::<[Ancillary; 2]>() as _;
        for control in &mut controls {
            // SAFETY: only calculates the ABI length.
            control.header.cmsg_len = unsafe { libc::CMSG_LEN(4) } as _;
        }
        controls[0].header.cmsg_level = libc::IPPROTO_UDP;
        controls[0].header.cmsg_type = libc::UDP_GRO;
        controls[0].data[..4].copy_from_slice(&1232_u32.to_ne_bytes());
        controls[1].header.cmsg_level = libc::SOL_SOCKET;
        controls[1].header.cmsg_type = libc::SO_RXQ_OVFL;
        controls[1].data[..4].copy_from_slice(&u32::MAX.to_ne_bytes());
        assert_eq!(
            receive_control(&message, &controls),
            Some((1232, Some(u32::MAX)))
        );
        message.msg_controllen += 1;
        assert_eq!(receive_control(&message, &controls), None);
        message.msg_controllen -= 1;
        controls.swap(0, 1);
        assert_eq!(
            receive_control(&message, &controls),
            Some((1232, Some(u32::MAX)))
        );
        controls[1].header.cmsg_level = libc::SOL_SOCKET;
        controls[1].header.cmsg_type = libc::SO_RXQ_OVFL;
        assert_eq!(receive_control(&message, &controls), None);
        message.msg_controllen -= 1;
        assert_eq!(receive_control(&message, &controls), None);
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
            lane.send(b"hello", BASE_UDP_PAYLOAD).unwrap();
            let mut bytes = [0; 32];
            let (n, from) = b.recv(&mut bytes).unwrap();
            assert_eq!(&bytes[..n], b"hello");
            assert_eq!(from, source);
            lane.send(b"again", BASE_UDP_PAYLOAD).unwrap();
            assert_eq!(b.recv(&mut bytes).unwrap().1, source);
        }
        reply.send(b"reply", BASE_UDP_PAYLOAD).unwrap();
        let mut bytes = [0; 32];
        assert_eq!(a.recv(&mut bytes).unwrap().1, reply.local_addr().unwrap());
        assert_eq!(
            a.recv(&mut bytes).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        lanes[0]
            .send(&[0; BASE_UDP_PAYLOAD], BASE_UDP_PAYLOAD)
            .unwrap();
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
                .send(&[0; BASE_UDP_PAYLOAD + 1], BASE_UDP_PAYLOAD)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn reuseport_steering_matches_payload_fields_and_wrapping_owner_arithmetic() {
        for members in [3, 4] {
            let first = receiver();
            let address = first.local_addr();
            let mut group = vec![first];
            for _ in 1..members {
                group.push(ReceiveSocket::bind(address, None).unwrap());
            }
            group[0].steer_group(members).unwrap();
            assert!(group[0].steer_group(0).is_err());
            assert!(group[0].steer_group(65).is_err());
            let sender = UdpSocket::bind("[::1]:0").unwrap();
            for (lane, cookie) in [
                (1_u16, 0_u32),
                (2, 0),
                (3, 0),
                (4, 0),
                (1, 1),
                (2, u32::MAX),
                (0, 0),
            ] {
                let mut bytes = [0_u8; 64];
                bytes[36..38].copy_from_slice(&lane.to_be_bytes());
                bytes[16..20].copy_from_slice(&cookie.to_be_bytes());
                let owner = cookie.wrapping_add(u32::from(lane).wrapping_sub(1)) as usize % members;
                sender.send_to(&bytes, address).unwrap();
                for (index, socket) in group.iter().enumerate() {
                    let mut received = [0; 64];
                    match socket.recv(&mut received) {
                        Ok((length, _)) => {
                            assert_eq!(index, owner);
                            assert_eq!(length, bytes.len());
                            assert_eq!(received, bytes);
                        }
                        Err(error) => {
                            assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
                            assert_ne!(index, owner);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn socket_options_enforce_wire_and_descriptor_policy() {
        let rx = receiver();
        let tx = TransmitSocket::connect(&rx, rx.local_addr()).unwrap();
        assert_eq!(option(rx.as_fd(), libc::SOL_SOCKET, libc::SO_REUSEPORT), 1);
        let queue_bytes = rx.receive_buffer_limit().unwrap();
        assert_eq!(
            queue_bytes,
            option(rx.as_fd(), libc::SOL_SOCKET, libc::SO_RCVBUF) as usize
        );
        assert!(queue_bytes > 0);
        if !rx.buffer_fallback {
            assert_eq!(queue_bytes, 2 * RECEIVE_QUEUE_BYTES as usize);
        }
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
                parts: &[b"head", b"tail"],
            },
            SendDatagram {
                parts: &[&[7; BASE_UDP_PAYLOAD], &[]],
            },
            SendDatagram {
                parts: &[b"last", &[]],
            },
        ];
        assert_eq!(tx.send_batch(&outgoing, BASE_UDP_PAYLOAD).unwrap(), 3);
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
                parts: &[&[0; BASE_UDP_PAYLOAD], &[1]],
            },
        ];
        assert_eq!(
            tx.send_batch(&invalid, BASE_UDP_PAYLOAD)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            rx.recv_batch(&mut incoming).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert!(tx.send_batch(&[], BASE_UDP_PAYLOAD).is_err());
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
        let parts: Vec<_> = prefixes
            .iter()
            .map(|prefix| [&prefix[..], &payload[..]])
            .collect();
        let outgoing: Vec<_> = parts.iter().map(|parts| SendDatagram { parts }).collect();
        let mut sent = 0;
        let mut calls = 0;
        while sent < BATCH_SIZE {
            let expected_time = Duration::from_micros(calls + 1);
            let (count, at) = send_batch(
                tx.as_fd(),
                &outgoing[sent..],
                BASE_UDP_PAYLOAD,
                false,
                || Ok(expected_time),
            )
            .unwrap();
            assert_eq!(at, expected_time);
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
    fn vectored_budget_returns_a_complete_prefix_and_validates_the_whole_batch() {
        for gso in [false, true] {
            let rx = receiver();
            let tx = TransmitSocket::connect(&rx, rx.local_addr()).unwrap();
            tx.gso.store(gso, Ordering::Relaxed);
            let parts = [&b"x"[..]; 500];
            let outgoing = [SendDatagram { parts: &parts }; 3];
            assert_eq!(tx.send_batch(&outgoing, BASE_UDP_PAYLOAD).unwrap(), 2);
            for _ in 0..2 {
                let mut bytes = [0; 500];
                assert_eq!(rx.recv(&mut bytes).unwrap().0, 500);
                assert_eq!(bytes, [b'x'; 500]);
            }
            assert_eq!(tx.send_batch(&outgoing[2..], BASE_UDP_PAYLOAD).unwrap(), 1);
            assert_eq!(rx.recv(&mut [0; 500]).unwrap().0, 500);
            let exact = [&b"x"[..]; MAX_VECTORS];
            let boundary = [
                SendDatagram { parts: &exact },
                SendDatagram { parts: &[] },
                SendDatagram { parts: &[b"tail"] },
            ];
            assert_eq!(tx.send_batch(&boundary, BASE_UDP_PAYLOAD).unwrap(), 2);
            assert_eq!(rx.recv(&mut [0; MAX_VECTORS]).unwrap().0, MAX_VECTORS);
            assert_eq!(rx.recv(&mut [0; 1]).unwrap().0, 0);
            assert_eq!(tx.send_batch(&boundary[2..], BASE_UDP_PAYLOAD).unwrap(), 1);
            assert_eq!(rx.recv(&mut [0; 4]).unwrap().0, 4);
            let too_many = [&b""[..]; MAX_VECTORS + 1];
            let invalid = [outgoing[0], outgoing[1], SendDatagram { parts: &too_many }];
            assert_eq!(
                tx.send_batch(&invalid, BASE_UDP_PAYLOAD)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidInput
            );
            assert_eq!(
                rx.recv(&mut [0; 500]).unwrap_err().kind(),
                io::ErrorKind::WouldBlock
            );
        }
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
