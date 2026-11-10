//! Persistent Ethernet TAP devices, independent of underlay readiness.
//!
//! Requires /dev/net/tun and CAP_NET_ADMIN in the device's network namespace.
//! Devices start administratively down with carrier down. The caller configures
//! bridge membership, addresses and administrative state. Queues can be moved
//! out of the Tap for per-worker ownership; any queue can control carrier.
//! Closing every queue keeps the device; only [`remove`] deletes it.

#[cfg(test)]
#[path = "../tests/common/mod.rs"]
pub(crate) mod test_common;

mod link;
mod read;
pub(crate) use read::Reader;
mod segment;
mod uso;
pub(crate) use segment::Segments;
#[cfg(test)]
#[path = "tap/uso_tests.rs"]
mod uso_tests;

use crate::{
    MAX_FRAME_LEN, MacAddress,
    sys::{cvt, invalid, udp_socket},
};
use std::{
    ffi::CStr,
    io,
    mem::zeroed,
    os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd},
};

/// Owns every queue of a created or reattached TAP.
///
/// The device is persistent: closing every queue keeps it, with its MAC,
/// attachments and upper devices, for the next start to reattach. Only
/// [`remove`] deletes it.
pub struct Tap {
    name: [libc::c_char; libc::IFNAMSIZ],
    queues: Vec<TapQueue>,
}

fn validate(name: &CStr) -> io::Result<()> {
    if name.is_empty() || name.to_bytes().len() >= libc::IFNAMSIZ || name.to_bytes().contains(&b'%')
    {
        return Err(invalid(
            "TAP name must contain 1..15 bytes and no percent sign",
        ));
    }
    Ok(())
}

fn request_for(name: &CStr) -> libc::ifreq {
    // SAFETY: all-zero ifreq is valid before its fields are populated.
    let mut request: libc::ifreq = unsafe { zeroed() };
    for (dst, &src) in request.ifr_name.iter_mut().zip(name.to_bytes()) {
        *dst = src as libc::c_char;
    }
    request
}

/// Opens one queue of the named TAP; `exclusive` refuses an existing device.
fn attach(request: &mut libc::ifreq, exclusive: bool) -> io::Result<OwnedFd> {
    // SAFETY: this is a constant NUL-terminated path; open returns a new fd.
    let fd = cvt(unsafe {
        libc::open(
            c"/dev/net/tun".as_ptr(),
            libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    })?;
    // SAFETY: this is the sole owner of the descriptor returned by open.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    request.ifr_ifru.ifru_flags = (libc::IFF_TAP
        | libc::IFF_NO_PI
        | libc::IFF_MULTI_QUEUE
        | libc::IFF_VNET_HDR
        | if exclusive { libc::IFF_TUN_EXCL } else { 0 })
        as libc::c_short;
    // SAFETY: TUNSETIFF reads/writes the initialized ifreq.
    cvt(unsafe { libc::ioctl(fd.as_raw_fd(), libc::TUNSETIFF, request as *mut libc::ifreq) })?;
    Ok(fd)
}

fn set_persist(fd: &OwnedFd, persist: bool) -> io::Result<()> {
    // SAFETY: TUNSETPERSIST takes an integer flag, not a pointer.
    cvt(unsafe {
        libc::ioctl(
            fd.as_raw_fd(),
            libc::TUNSETPERSIST,
            libc::c_ulong::from(persist),
        )
    })?;
    Ok(())
}

/// A persistent multiqueue TAP without packet information.
fn ours(tun: &link::Tun) -> bool {
    tun.tap && !tun.pi && tun.multi_queue && tun.persist
}

/// Deletes the named persistent TAP: it disappears once its last queue closes,
/// immediately if no process holds one. Shutdown and restart never call this;
/// only an explicit removal instruction does.
pub fn remove(name: &CStr) -> io::Result<()> {
    validate(name)?;
    match link::query(name)? {
        link::Link::Tun(tun) if ours(&tun) => {}
        link::Link::Missing => {
            return Err(io::Error::new(io::ErrorKind::NotFound, "no such TAP"));
        }
        _ => return Err(invalid("not a persistent multiqueue TAP")),
    }
    // The same flags leave a running owner's device features unchanged.
    set_persist(&attach(&mut request_for(name), false)?, false)
}

impl Tap {
    /// Creates a named TAP with MTU 1500..9000 and 1..64 queues, or reattaches
    /// the persistent TAP an earlier run left with that name.
    ///
    /// Uses IFF_TAP, IFF_NO_PI, IFF_MULTI_QUEUE and internal virtio metadata.
    /// Read checksum/GSO offloads stay disabled; eligible writes may use USO/TSO.
    /// A new device gets the supplied MAC, or with None a Menhera MAC-M address
    /// derived from the current UTS hostname and interface name using SHA-256;
    /// a reattached device keeps its MAC. Reattaching requires a persistent
    /// multiqueue TAP with no queue attached; a device in use or of another kind
    /// fails with EBUSY. Any failure leaves no new device behind.
    /// Administrative state is left to the caller or network manager.
    pub fn create(
        name: &CStr,
        mtu: u16,
        queue_count: usize,
        mac: Option<MacAddress>,
    ) -> io::Result<Self> {
        validate(name)?;
        if !(1500..=9000).contains(&mtu) || !(1..=64).contains(&queue_count) {
            return Err(invalid("TAP requires MTU 1500..9000 and 1..64 queues"));
        }
        let fresh = match link::query(name)? {
            link::Link::Missing => true,
            link::Link::Tun(tun) if ours(&tun) && tun.queues == 0 => false,
            _ => return Err(io::Error::from_raw_os_error(libc::EBUSY)),
        };
        let mac = match (fresh, mac) {
            (false, _) => None,
            (true, Some(mac)) => Some(mac),
            (true, None) => Some(MacAddress::for_interface(name)?),
        };
        let mut request = request_for(name);
        let mut tap = Self {
            name: request.ifr_name,
            queues: Vec::with_capacity(queue_count),
        };
        for i in 0..queue_count {
            // A fresh device must be ours alone; a race with another creator fails.
            let fd = attach(&mut request, fresh && i == 0)?;
            // SAFETY: TUNSETOFFLOAD takes an integer bitmask, not a pointer.
            cvt(unsafe { libc::ioctl(fd.as_raw_fd(), libc::TUNSETOFFLOAD, 0 as libc::c_ulong) })?;
            let header_len = 10i32;
            // SAFETY: TUNSETVNETHDRSZ reads one initialized integer.
            cvt(unsafe { libc::ioctl(fd.as_raw_fd(), libc::TUNSETVNETHDRSZ, &header_len) })?;
            tap.queues.push(TapQueue {
                fd,
                frame_limit: mtu as usize + 22,
                vnet: true,
                uso: true,
                tso: true,
                completed_checksums: 0,
            });
        }
        request.ifr_ifru.ifru_mtu = mtu as libc::c_int;
        let control = udp_socket()?;
        // SAFETY: SIOCSIFMTU reads the interface name and integer MTU from ifreq.
        cvt(unsafe { libc::ioctl(control.as_raw_fd(), libc::SIOCSIFMTU as _, &request) })?;
        if let Some(mac) = mac {
            let mut address = libc::sockaddr {
                sa_family: libc::ARPHRD_ETHER,
                sa_data: [0; 14],
            };
            for (dst, &src) in address.sa_data.iter_mut().zip(mac.as_bytes()) {
                *dst = src as libc::c_char;
            }
            request.ifr_ifru.ifru_hwaddr = address;
            // SAFETY: SIOCSIFHWADDR reads an initialized Ethernet sockaddr.
            cvt(unsafe {
                libc::ioctl(
                    tap.queues[0].fd.as_raw_fd(),
                    libc::SIOCSIFHWADDR as _,
                    &request,
                )
            })?;
        }
        tap.set_carrier(false)?;
        if fresh {
            // Last, so a failed creation closes its queues and leaves no device.
            set_persist(&tap.queues[0].fd, true)?;
        }
        Ok(tap)
    }

    /// Adopts queue descriptors handed over by a previous process (design
    /// section 11, handover format version 1). Each must be a multiqueue TAP
    /// queue with virtio headers and no packet information, all of one device.
    /// The device keeps its current name, MTU, MAC, carrier and administrative
    /// state; `mtu` is the configured inner MTU that bounds frames. Rejected
    /// descriptors are closed, which leaves a persistent device in place.
    pub fn adopt(queues: Vec<OwnedFd>, mtu: u16) -> io::Result<Self> {
        if !(1500..=9000).contains(&mtu) || !(1..=64).contains(&queues.len()) {
            return Err(invalid("TAP requires MTU 1500..9000 and 1..64 queues"));
        }
        let wanted = libc::IFF_TAP | libc::IFF_NO_PI | libc::IFF_MULTI_QUEUE | libc::IFF_VNET_HDR;
        let mut name = None;
        let mut adopted = Vec::with_capacity(queues.len());
        for fd in queues {
            // SAFETY: a zeroed ifreq is valid output storage for TUNGETIFF.
            let mut request: libc::ifreq = unsafe { zeroed() };
            // SAFETY: TUNGETIFF writes the device name and flags into request.
            cvt(unsafe { libc::ioctl(fd.as_raw_fd(), libc::TUNGETIFF, &mut request) })
                .map_err(|_| invalid("handed-over descriptor is not a TAP queue"))?;
            // SAFETY: TUNGETIFF filled the flags member.
            let flags = i32::from(unsafe { request.ifr_ifru.ifru_flags });
            if flags & (wanted | libc::IFF_TUN) != wanted {
                return Err(invalid(
                    "handed-over queue is not a multiqueue TAP with virtio headers",
                ));
            }
            match name {
                None => name = Some(request.ifr_name),
                Some(seen) if seen == request.ifr_name => {}
                Some(_) => return Err(invalid("handed-over queues belong to different devices")),
            }
            // SAFETY: plain descriptor flag updates on a descriptor we own.
            unsafe {
                let status = cvt(libc::fcntl(fd.as_raw_fd(), libc::F_GETFL))?;
                cvt(libc::fcntl(
                    fd.as_raw_fd(),
                    libc::F_SETFL,
                    status | libc::O_NONBLOCK,
                ))?;
                cvt(libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC))?;
            }
            let header_len = 10i32;
            // SAFETY: TUNSETVNETHDRSZ reads one initialized integer.
            cvt(unsafe { libc::ioctl(fd.as_raw_fd(), libc::TUNSETVNETHDRSZ, &header_len) })?;
            adopted.push(TapQueue {
                fd,
                frame_limit: mtu as usize + 22,
                vnet: true,
                uso: true,
                tso: true,
                completed_checksums: 0,
            });
        }
        let name = name.ok_or_else(|| invalid("no queues to adopt"))?;
        Ok(Self {
            name,
            queues: adopted,
        })
    }

    pub(crate) fn enable_input_offloads(&self) -> io::Result<()> {
        let fd = self.queues[0].fd.as_raw_fd();
        // Linux requires both TUN_F_USO4 (0x20) and TUN_F_USO6 (0x40).
        let tcp = (libc::TUN_F_CSUM | libc::TUN_F_TSO4 | libc::TUN_F_TSO6 | libc::TUN_F_TSO_ECN)
            as libc::c_ulong;
        let flags = tcp | 0x60;
        // SAFETY: TUNSETOFFLOAD takes an integer bitmask for this TAP.
        let result = cvt(unsafe { libc::ioctl(fd, libc::TUNSETOFFLOAD, flags) });
        match result {
            Ok(_) => Ok(()),
            Err(error) if matches!(error.raw_os_error(), Some(libc::EINVAL | libc::EOPNOTSUPP)) => {
                // Kernels without USO still support TCP segmentation offload.
                cvt(unsafe { libc::ioctl(fd, libc::TUNSETOFFLOAD, tcp) })?;
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    /// Returns the exact requested device name.
    pub fn name(&self) -> &CStr {
        // SAFETY: name was zero-filled and holds fewer than IFNAMSIZ input bytes.
        unsafe { CStr::from_ptr(self.name.as_ptr()) }
    }

    /// Number of queues this handle owns.
    pub fn queue_count(&self) -> usize {
        self.queues.len()
    }

    /// Returns the nonblocking queues for worker assignment and epoll registration.
    pub fn queues(&mut self) -> &mut [TapQueue] {
        &mut self.queues
    }

    /// Gives each queue to its worker. The device outlives its queues.
    pub fn into_queues(self) -> Vec<TapQueue> {
        self.queues
    }

    /// Changes carrier without removing the TAP or its upper devices.
    pub fn set_carrier(&self, up: bool) -> io::Result<()> {
        self.queues[0].set_carrier(up)
    }
}

/// One TAP queue. Frame operations allocate no memory and preserve boundaries.
pub struct TapQueue {
    fd: OwnedFd,
    frame_limit: usize,
    vnet: bool,
    uso: bool,
    tso: bool,
    pub(crate) completed_checksums: u64,
}

impl TapQueue {
    #[cfg(test)]
    pub(crate) fn test_fd(fd: OwnedFd, vnet: bool) -> Self {
        Self {
            fd,
            frame_limit: MAX_FRAME_LEN,
            vnet,
            uso: false,
            tso: false,
            completed_checksums: 0,
        }
    }

    /// Releases the queue descriptor, for handing it to a successor process.
    pub fn into_fd(self) -> OwnedFd {
        self.fd
    }

    /// Changes carrier for the whole device without removing the TAP or
    /// its upper devices. Any queue of the device may be used.
    pub fn set_carrier(&self, up: bool) -> io::Result<()> {
        let value = libc::c_int::from(up);
        // SAFETY: TUNSETCARRIER reads an integer through a valid pointer.
        cvt(unsafe { libc::ioctl(self.fd.as_raw_fd(), libc::TUNSETCARRIER, &value) })?;
        Ok(())
    }

    /// Reads one complete Ethernet frame without metadata or FCS.
    ///
    /// The buffer must hold MAX_FRAME_LEN + 1 bytes so oversized frames cannot
    /// be mistaken for complete frames. Invalid frames are consumed and return
    /// InvalidData. WouldBlock and Interrupted leave retry policy to the worker.
    pub fn read_frame(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let (length, segments) = self.read_input(buffer)?;
        if segments.is_some() {
            return Err(io::ErrorKind::InvalidData.into());
        }
        Ok(length)
    }

    pub(crate) fn read_input(
        &mut self,
        buffer: &mut [u8],
    ) -> io::Result<(usize, Option<Segments>)> {
        if buffer.len() <= MAX_FRAME_LEN {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let mut header = [0u8; 10];
        let iov = [
            libc::iovec {
                iov_base: header.as_mut_ptr().cast(),
                iov_len: header.len(),
            },
            libc::iovec {
                iov_base: buffer.as_mut_ptr().cast(),
                iov_len: buffer.len(),
            },
        ];
        // SAFETY: both buffers are writable for their declared lengths.
        let n = unsafe {
            if self.vnet {
                libc::readv(self.fd.as_raw_fd(), iov.as_ptr(), 2)
            } else {
                libc::read(
                    self.fd.as_raw_fd(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                )
            }
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        self.finish_input(&header, n as usize, buffer)
    }

    fn finish_input(
        &mut self,
        header: &[u8; 10],
        n: usize,
        buffer: &mut [u8],
    ) -> io::Result<(usize, Option<Segments>)> {
        if self.vnet && header[1] != 0 {
            let length = n
                .checked_sub(header.len())
                .ok_or(io::ErrorKind::InvalidData)?;
            if length >= buffer.len() {
                return Err(io::ErrorKind::InvalidData.into());
            }
            let segments = Segments::parse(header, &buffer[..length], self.frame_limit)
                .ok_or(io::ErrorKind::InvalidData)?;
            return Ok((length, Some(segments)));
        }
        self.finish_read(header, n, buffer)
            .map(|length| (length, None))
    }

    fn finish_read(
        &mut self,
        header: &[u8; 10],
        mut n: usize,
        buffer: &mut [u8],
    ) -> io::Result<usize> {
        if self.vnet {
            if n < header.len() || header[0] > 2 || header[1] != 0 {
                return Err(io::ErrorKind::InvalidData.into());
            }
            n -= header.len();
        }
        if !(14..=self.frame_limit).contains(&n) || n > buffer.len() {
            return Err(io::ErrorKind::InvalidData.into());
        }
        if self.vnet && header[0] == 1 {
            let start = u16::from_ne_bytes([header[6], header[7]]) as usize;
            let field = start + u16::from_ne_bytes([header[8], header[9]]) as usize;
            if start < 14 || field + 2 > n {
                return Err(io::ErrorKind::InvalidData.into());
            }
            // Match skb_checksum_help, including the mangled representation of zero.
            let checksum = !uso::fold(uso::sum(&buffer[start..n]));
            buffer[field..field + 2]
                .copy_from_slice(&if checksum == 0 { 65535u16 } else { checksum }.to_be_bytes());
            self.completed_checksums = self.completed_checksums.wrapping_add(1);
        }
        Ok(n)
    }

    /// Writes one complete frame. Never retries a short write as another frame.
    ///
    /// On WouldBlock the caller retains the whole frame until writable or
    /// expired. Other errors discard the frame in the transport.
    pub fn write_frame(&mut self, frame: &[u8]) -> io::Result<()> {
        if !(14..=self.frame_limit).contains(&frame.len()) {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let header = [0u8; 10];
        let iov = [
            libc::iovec {
                iov_base: header.as_ptr().cast_mut().cast(),
                iov_len: header.len(),
            },
            libc::iovec {
                iov_base: frame.as_ptr().cast_mut().cast(),
                iov_len: frame.len(),
            },
        ];
        // SAFETY: header and frame are readable for their declared lengths.
        let n = unsafe {
            if self.vnet {
                libc::writev(self.fd.as_raw_fd(), iov.as_ptr(), 2)
            } else {
                libc::write(self.fd.as_raw_fd(), frame.as_ptr().cast(), frame.len())
            }
        };
        complete_write(n, frame.len() + if self.vnet { header.len() } else { 0 })
    }

    /// Enabled UDP and TCP write offloads, respectively.
    pub(crate) fn write_offloads(&self) -> (bool, bool) {
        (self.uso, self.tso)
    }

    /// Returns the attempted mask, whether it used TSO, and the write result.
    /// The mask names every attempted frame, even when the write fails.
    pub(crate) fn write_batch<'a>(
        &mut self,
        mut frames: impl Iterator<Item = &'a [u8]>,
    ) -> (u64, bool, io::Result<usize>) {
        let Some(first) = frames.next() else {
            return (0, false, Ok(0));
        };
        if (self.uso || self.tso)
            && (14..=self.frame_limit).contains(&first.len())
            && let Some((mask, tcp, result)) = uso::write(
                self.fd.as_raw_fd(),
                first,
                frames,
                self.frame_limit,
                (self.uso, self.tso),
            )
        {
            if result
                .as_ref()
                .is_err_and(|e| matches!(e.raw_os_error(), Some(libc::EINVAL | libc::EOPNOTSUPP)))
            {
                if tcp {
                    self.tso = false;
                } else {
                    self.uso = false;
                }
            } else {
                return (mask, tcp, result);
            }
        }
        (1, false, self.write_frame(first).map(|()| first.len()))
    }
}

fn complete_write(written: isize, expected: usize) -> io::Result<()> {
    if written < 0 {
        Err(io::Error::last_os_error())
    } else if written as usize != expected {
        Err(io::ErrorKind::WriteZero.into())
    } else {
        Ok(())
    }
}

impl AsFd for TapQueue {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_configuration_fails_before_opening_tun() {
        for name in [c"", c"0123456789abcdef", c"tap%d"] {
            assert!(Tap::create(name, 1500, 1, None).is_err());
        }
        for (mtu, queues) in [(1499, 1), (9001, 1), (1500, 0), (1500, 65)] {
            assert!(Tap::create(c"unquic-test", mtu, queues, None).is_err());
        }
    }
    #[test]
    fn partial_checksums_complete_in_place_with_checked_metadata() {
        let mut tap = TapQueue {
            fd: std::fs::File::open("/dev/null").unwrap().into(),
            frame_limit: MAX_FRAME_LEN,
            vnet: true,
            uso: false,
            tso: false,
            completed_checksums: 0,
        };
        for v6 in [false, true] {
            for size in [0, 1, 2, 1471, 1472, 8900] {
                let payload: Vec<_> = (0..size).map(|i| (i * 13) as u8).collect();
                let expected = test_common::udp::frame(v6, 42, &payload);
                let mut bytes = expected.clone();
                let mut header = test_common::udp::partial(&mut bytes, v6);
                for tags in [0, 1, 2] {
                    let mut packet = bytes.clone();
                    let mut want = expected.clone();
                    for _ in 0..tags {
                        packet.splice(12..12, [0x81, 0, 0, 1]);
                        want.splice(12..12, [0x81, 0, 0, 1]);
                    }
                    let at = (if v6 { 54u16 } else { 34 }) + 4 * tags;
                    header[6..8].copy_from_slice(&at.to_ne_bytes());
                    let len = packet.len();
                    assert_eq!(
                        tap.finish_read(&header, len + 10, &mut packet).unwrap(),
                        len
                    );
                    assert_eq!(packet, want);
                }
            }
        }

        for v6 in [false, true] {
            for size in [1, 2, 1472] {
                let at = if v6 { 54 } else { 34 };
                let mut expected = test_common::udp::frame(v6, 42, &vec![0x5a; size]);
                expected.splice(at..at + 8, [0u8; 20]);
                expected[at + 12] = 0x50;
                expected[at + 13] = 0x18;
                let len = expected.len() - at;
                if v6 {
                    expected[20] = 6;
                    expected[18..20].copy_from_slice(&(len as u16).to_be_bytes());
                } else {
                    expected[23] = 6;
                    expected[16..18].copy_from_slice(&((len + 20) as u16).to_be_bytes());
                    expected[24..26].fill(0);
                    let checksum = !uso::fold(uso::sum(&expected[14..34]));
                    expected[24..26].copy_from_slice(&checksum.to_be_bytes());
                }
                let pseudo = uso::sum(if v6 {
                    &expected[22..54]
                } else {
                    &expected[26..34]
                }) + 6
                    + len as u32;
                let checksum = !uso::fold(pseudo + uso::sum(&expected[at..]));
                expected[at + 16..at + 18].copy_from_slice(
                    &if checksum == 0 { 65535u16 } else { checksum }.to_be_bytes(),
                );
                let mut packet = expected.clone();
                packet[at + 16..at + 18].copy_from_slice(&uso::fold(pseudo).to_be_bytes());
                let mut header = [0u8; 10];
                header[0] = 1;
                header[6..8].copy_from_slice(&(at as u16).to_ne_bytes());
                header[8..10].copy_from_slice(&16u16.to_ne_bytes());
                tap.finish_read(&header, packet.len() + 10, &mut packet)
                    .unwrap();
                assert_eq!(packet, expected);
            }
        }
        let mut zero = [0u8; 60];
        zero[20..22].fill(255);
        let mut header = [0u8; 10];
        header[0] = 1;
        header[6..8].copy_from_slice(&14u16.to_ne_bytes());
        header[8..10].copy_from_slice(&6u16.to_ne_bytes());
        tap.finish_read(&header, 70, &mut zero).unwrap();
        assert_eq!(&zero[20..22], &[255, 255]);
        assert_eq!(tap.completed_checksums, 43);

        for (flags, gso, start, offset, length) in [
            (3, 0, 14, 6, 70),
            (4, 0, 14, 6, 70),
            (1, 5, 14, 6, 70),
            (1, 0, 0, 6, 70),
            (1, 0, 59, 0, 70),
            (1, 0, 14, 65535, 70),
            (1, 0, 65535, 65535, 70),
            (1, 0, 14, 6, 9),
            (1, 0, 14, 6, 71),
        ] {
            let mut bytes = [7u8; 60];
            header[0] = flags;
            header[1] = gso;
            header[6..8].copy_from_slice(&(start as u16).to_ne_bytes());
            header[8..10].copy_from_slice(&(offset as u16).to_ne_bytes());
            assert_eq!(
                tap.finish_read(&header, length, &mut bytes)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidData
            );
            assert_eq!(bytes, [7; 60]);
            assert_eq!(tap.completed_checksums, 43);
        }
    }
}
