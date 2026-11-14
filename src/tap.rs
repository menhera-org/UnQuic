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

/// Transmit queue length raise_queue ensures: frames the kernel holds for the
/// daemon's reads. Current kernels give new tun devices this length already;
/// a device left shorter is raised.
pub const TAP_QUEUE_LEN: libc::c_int = 1000;

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

    /// Raises the device's transmit queue, the frames waiting for this daemon
    /// to read, to at least TAP_QUEUE_LEN, so a briefly slow worker drops
    /// fewer; an operator's longer queue stays. Without CAP_NET_ADMIN it
    /// changes nothing. Returns the queue length now in force.
    pub fn raise_queue(&self) -> io::Result<usize> {
        let control = udp_socket()?;
        let mut request = request_for(self.name());
        // SAFETY: SIOCGIFTXQLEN reads the name and writes the length into ifreq.
        cvt(unsafe { libc::ioctl(control.as_raw_fd(), libc::SIOCGIFTXQLEN as _, &mut request) })?;
        // SAFETY: the kernel filled the integer member (ifr_qlen) just read.
        let length = unsafe { request.ifr_ifru.ifru_metric };
        if length >= TAP_QUEUE_LEN {
            return Ok(length as usize);
        }
        match link::set_queue_length(self.name(), TAP_QUEUE_LEN as u32) {
            Ok(()) => Ok(TAP_QUEUE_LEN as usize),
            Err(error) if error.raw_os_error() == Some(libc::EPERM) => Ok(length as usize),
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
            complete_checksum(header, &mut buffer[..n])?;
            self.completed_checksums = self.completed_checksums.wrapping_add(1);
        }
        Ok(n)
    }

    /// Writes one offload frame (design section 6): a big-endian virtio-net
    /// header as its descriptor, then one Ethernet frame. The descriptor is
    /// checked against the frame first, and a mismatch returns InvalidData
    /// with nothing written. GSO the TAP refuses is segmented here with
    /// complete checksums; a failure after the first segment returns
    /// WriteZero, since the rest cannot be retried as one frame. Returns the
    /// Ethernet bytes delivered, counting each segment's headers.
    pub(crate) fn write_offload(&mut self, frame: &[u8]) -> io::Result<usize> {
        let invalid = || io::Error::from(io::ErrorKind::InvalidData);
        let (descriptor, ethernet) = frame
            .split_first_chunk::<{ crate::wire::DESCRIPTOR_LEN }>()
            .ok_or_else(invalid)?;
        let mut header = *descriptor;
        for at in [2, 4, 6, 8] {
            let value = u16::from_be_bytes([header[at], header[at + 1]]);
            header[at..at + 2].copy_from_slice(&value.to_ne_bytes());
        }
        let kind = header[1];
        let segments = if kind == 0 {
            if header[0] > 1
                || !(14..=self.frame_limit).contains(&ethernet.len())
                || (header[0] == 1 && checksum_field(&header, ethernet.len()).is_none())
            {
                return Err(invalid());
            }
            None
        } else {
            Some(Segments::parse(&header, ethernet, self.frame_limit).ok_or_else(invalid)?)
        };
        let length = segments.map_or(Some(ethernet.len()), |segments| {
            segments.remaining_bytes(ethernet.len())
        });
        let length = length.ok_or_else(invalid)?;
        if self.vnet
            && match kind {
                0 => true,
                5 => self.uso,
                _ => self.tso,
            }
        {
            let iov = [
                libc::iovec {
                    iov_base: header.as_mut_ptr().cast(),
                    iov_len: header.len(),
                },
                libc::iovec {
                    iov_base: ethernet.as_ptr().cast_mut().cast(),
                    iov_len: ethernet.len(),
                },
            ];
            // SAFETY: header and frame are readable for their declared lengths.
            let n = unsafe { libc::writev(self.fd.as_raw_fd(), iov.as_ptr(), 2) };
            match complete_write(n, header.len() + ethernet.len()) {
                Err(error)
                    if kind != 0
                        && matches!(
                            error.raw_os_error(),
                            Some(libc::EINVAL | libc::EOPNOTSUPP)
                        ) =>
                {
                    if kind == 5 {
                        self.uso = false;
                    } else {
                        self.tso = false;
                    }
                }
                result => return result.map(|()| length),
            }
        }
        let mut output = [0u8; MAX_FRAME_LEN];
        let Some(mut segments) = segments else {
            // Only a queue without virtio headers gets here.
            let copy = output.get_mut(..ethernet.len()).ok_or_else(invalid)?;
            copy.copy_from_slice(ethernet);
            if header[0] == 1 {
                complete_checksum(&header, copy)?;
            }
            return self.write_frame(copy).map(|()| length);
        };
        let mut written = 0;
        while !segments.done(ethernet.len()) {
            let n = segments.write(ethernet, &mut output);
            let result = match output.get(..n) {
                Some(segment) if n != 0 => self.write_frame(segment),
                _ => Err(invalid()),
            };
            match result {
                Ok(()) => written += n,
                Err(error) if written == 0 => return Err(error),
                Err(_) => return Err(io::ErrorKind::WriteZero.into()),
            }
        }
        Ok(written)
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

/// The checksum field a partial-checksum header names, if it fits the frame.
fn checksum_field(header: &[u8; 10], length: usize) -> Option<usize> {
    let start = u16::from_ne_bytes([header[6], header[7]]) as usize;
    let field = start + u16::from_ne_bytes([header[8], header[9]]) as usize;
    (start >= 14 && field + 2 <= length).then_some(field)
}

/// Completes a partial transport checksum as skb_checksum_help does,
/// including the mangled representation of zero.
fn complete_checksum(header: &[u8; 10], frame: &mut [u8]) -> io::Result<()> {
    let field = checksum_field(header, frame.len()).ok_or(io::ErrorKind::InvalidData)?;
    let start = u16::from_ne_bytes([header[6], header[7]]) as usize;
    let checksum = !uso::fold(uso::sum(&frame[start..]));
    frame[field..field + 2]
        .copy_from_slice(&if checksum == 0 { 65535u16 } else { checksum }.to_be_bytes());
    Ok(())
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
    fn offload_frames_write_natively_or_segment_with_complete_checksums() {
        use std::os::fd::FromRawFd;
        use test_common::{tcp, udp};
        // A seqpacket pair keeps each write's boundary for the reader.
        let pair = || {
            let mut fds = [0; 2];
            let kind = libc::SOCK_SEQPACKET | libc::SOCK_NONBLOCK;
            // SAFETY: socketpair writes two new descriptors into fds.
            assert_eq!(
                unsafe { libc::socketpair(libc::AF_UNIX, kind, 0, fds.as_mut_ptr()) },
                0
            );
            // SAFETY: both descriptors are new and owned only here.
            unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
        };
        let queue = |fd, vnet| TapQueue {
            fd,
            frame_limit: 1514,
            vnet,
            uso: true,
            tso: true,
            completed_checksums: 0,
        };
        let read = |fd: &OwnedFd| {
            let mut buffer = vec![0; 70_000];
            // SAFETY: buffer is writable for its length.
            let n = unsafe {
                libc::recv(
                    fd.as_raw_fd(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                    libc::MSG_DONTWAIT,
                )
            };
            buffer.truncate(n.max(0) as usize);
            buffer
        };
        let descriptor = |header: [u8; 10]| {
            let mut descriptor = header;
            for at in [2, 4, 6, 8] {
                let value = u16::from_ne_bytes([header[at], header[at + 1]]);
                descriptor[at..at + 2].copy_from_slice(&value.to_be_bytes());
            }
            descriptor
        };
        let payload: Vec<_> = (0..4000).map(|i| (i * 7) as u8).collect();
        for v6 in [false, true] {
            let mut input = tcp::frame(v6, 9, 1000, 0x18, &[], &payload);
            let header = tcp::partial(&mut input, v6, 1400);
            let frame = [&descriptor(header)[..], &input].concat();
            let mut segments = Segments::parse(&header, &input, 1514).unwrap();
            let mut expected = Vec::new();
            while !segments.done(input.len()) {
                let mut output = [0; MAX_FRAME_LEN];
                let n = segments.write(&input, &mut output);
                expected.push(output[..n].to_vec());
            }
            assert_eq!(expected.len(), 3);
            let total: usize = expected.iter().map(Vec::len).sum();
            // Natively: the virtio-net header, then the unchanged super-frame.
            let (a, b) = pair();
            let mut tap = queue(a, true);
            assert_eq!(tap.write_offload(&frame).unwrap(), total);
            assert_eq!(read(&b), [&header[..], &input].concat());
            // Without TSO, and without virtio headers: one frame per segment.
            tap.tso = false;
            assert_eq!(tap.write_offload(&frame).unwrap(), total);
            for segment in &expected {
                assert_eq!(read(&b), [&[0; 10][..], segment].concat());
            }
            let (a, c) = pair();
            let mut plain = queue(a, false);
            assert_eq!(plain.write_offload(&frame).unwrap(), total);
            for segment in &expected {
                assert_eq!(&read(&c), segment);
            }
            assert!(read(&c).is_empty());
            // A full queue refuses the first segment as retryable, but a
            // failure after it cannot be retried as one frame.
            while plain.write_frame(&expected[0]).is_ok() {}
            assert_eq!(
                plain.write_offload(&frame).unwrap_err().kind(),
                io::ErrorKind::WouldBlock
            );
            read(&c);
            assert_eq!(
                plain.write_offload(&frame).unwrap_err().kind(),
                io::ErrorKind::WriteZero
            );
            // A TAP refusing the GSO type loses that offload and segments instead;
            // an epoll descriptor has no write operation, so every write gets EINVAL.
            // SAFETY: epoll_create1 returns a new descriptor owned only here.
            let epoll = unsafe { OwnedFd::from_raw_fd(libc::epoll_create1(libc::EPOLL_CLOEXEC)) };
            let mut refusing = queue(epoll, true);
            let error = refusing.write_offload(&frame).unwrap_err();
            assert_eq!(error.raw_os_error(), Some(libc::EINVAL));
            assert_eq!(refusing.write_offloads(), (true, false));
        }
        // UDP segmentation uses its own switch.
        let mut input = udp::frame(false, 9, &payload[..3000]);
        let mut header = udp::partial(&mut input, false);
        header[1] = 5;
        header[2..4].copy_from_slice(&42u16.to_ne_bytes());
        header[4..6].copy_from_slice(&1472u16.to_ne_bytes());
        let frame = [&descriptor(header)[..], &input].concat();
        let (a, b) = pair();
        let mut tap = queue(a, true);
        tap.uso = false;
        tap.write_offload(&frame).unwrap();
        assert_eq!(read(&b).len(), 10 + 1514);
        assert_eq!(read(&b).len(), 10 + 1514);
        assert_eq!(read(&b).len(), 10 + 42 + 56);
        // A partial checksum without GSO passes through, or completes in place.
        let expected = udp::frame(true, 7, &payload[..1000]);
        let mut input = expected.clone();
        let header = udp::partial(&mut input, true);
        let frame = [&descriptor(header)[..], &input].concat();
        assert_eq!(tap.write_offload(&frame).unwrap(), input.len());
        assert_eq!(read(&b), [&header[..], &input].concat());
        let (a, c) = pair();
        let mut plain = queue(a, false);
        assert_eq!(plain.write_offload(&frame).unwrap(), input.len());
        assert_eq!(read(&c), expected);
        // Descriptors that do not match their frame write nothing.
        let mut bad = Vec::new();
        bad.push(frame[..23].to_vec());
        for (at, value) in [(0, 2), (1, 1), (1, 3), (6, 0x10), (7, 13), (8, 0xff)] {
            let mut frame = frame.clone();
            frame[at] = value;
            bad.push(frame);
        }
        let mut long = frame.clone();
        long.resize(10 + 1515, 0);
        bad.push(long);
        for frame in bad {
            assert_eq!(
                tap.write_offload(&frame).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
            assert_eq!(
                plain.write_offload(&frame).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
        assert!(read(&b).is_empty());
        assert!(read(&c).is_empty());
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
