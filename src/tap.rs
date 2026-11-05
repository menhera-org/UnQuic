//! Nonpersistent Ethernet TAP devices, independent of underlay readiness.
//!
//! Requires /dev/net/tun and CAP_NET_ADMIN in the device's network namespace.
//! Devices start administratively down with carrier down. The caller configures
//! bridge membership, addresses and administrative state. Queues can be moved
//! out of the Tap for per-worker ownership; any queue can control carrier.

#[cfg(test)]
#[path = "../tests/common/mod.rs"]
mod test_common;

mod read;
pub(crate) use read::Reader;
mod uso;
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

/// Owns every queue of a newly created TAP.
///
/// Closing the last queue deletes the nonpersistent device. Creation never
/// attaches to an existing device; descriptor adoption needs a versioned
/// handover contract and is not implemented.
pub struct Tap {
    name: [libc::c_char; libc::IFNAMSIZ],
    queues: Vec<TapQueue>,
}

impl Tap {
    /// Creates a named TAP with MTU 1500..9000 and 1..64 queues.
    ///
    /// Uses IFF_TAP, IFF_NO_PI, IFF_MULTI_QUEUE and internal virtio metadata.
    /// Read checksum/GSO offloads stay disabled; eligible writes may use USO.
    /// Any failure closes all queues created by this call.
    /// Sets the supplied MAC; None derives a Menhera MAC-M address from the
    /// current UTS hostname and interface name using SHA-256.
    /// Administrative state is left to the caller or network manager.
    pub fn create(
        name: &CStr,
        mtu: u16,
        queue_count: usize,
        mac: Option<MacAddress>,
    ) -> io::Result<Self> {
        if name.is_empty()
            || name.to_bytes().len() >= libc::IFNAMSIZ
            || name.to_bytes().contains(&b'%')
        {
            return Err(invalid(
                "TAP name must contain 1..15 bytes and no percent sign",
            ));
        }
        if !(1500..=9000).contains(&mtu) || !(1..=64).contains(&queue_count) {
            return Err(invalid("TAP requires MTU 1500..9000 and 1..64 queues"));
        }
        let mac = match mac {
            Some(mac) => mac,
            None => MacAddress::for_interface(name)?,
        };
        // SAFETY: all-zero ifreq is valid before its fields are populated.
        let mut request: libc::ifreq = unsafe { zeroed() };
        for (dst, &src) in request.ifr_name.iter_mut().zip(name.to_bytes()) {
            *dst = src as libc::c_char;
        }
        let mut tap = Self {
            name: request.ifr_name,
            queues: Vec::with_capacity(queue_count),
        };
        for i in 0..queue_count {
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
                | if i == 0 { libc::IFF_TUN_EXCL } else { 0 })
                as libc::c_short;
            // SAFETY: TUNSETIFF reads/writes the initialized ifreq.
            cvt(unsafe { libc::ioctl(fd.as_raw_fd(), libc::TUNSETIFF, &mut request) })?;
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
            });
        }
        request.ifr_ifru.ifru_mtu = mtu as libc::c_int;
        let control = udp_socket()?;
        // SAFETY: SIOCSIFMTU reads the interface name and integer MTU from ifreq.
        cvt(unsafe { libc::ioctl(control.as_raw_fd(), libc::SIOCSIFMTU as _, &request) })?;
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
        tap.set_carrier(false)?;
        Ok(tap)
    }

    /// Returns the exact requested device name.
    pub fn name(&self) -> &CStr {
        // SAFETY: name was zero-filled and holds fewer than IFNAMSIZ input bytes.
        unsafe { CStr::from_ptr(self.name.as_ptr()) }
    }

    /// Returns the nonblocking queues for worker assignment and epoll registration.
    pub fn queues(&mut self) -> &mut [TapQueue] {
        &mut self.queues
    }

    /// Gives each queue to its worker. Closing the last queue deletes the
    /// nonpersistent device, so the caller must keep at least one alive.
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
}

impl TapQueue {
    #[cfg(test)]
    pub(crate) fn test_fd(fd: OwnedFd) -> Self {
        Self {
            fd,
            frame_limit: MAX_FRAME_LEN,
            vnet: false,
            uso: false,
        }
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
        self.check_read(&header, n as usize)
    }

    fn check_read(&self, header: &[u8; 10], mut n: usize) -> io::Result<usize> {
        if self.vnet {
            if n < header.len() || header[0] & !2 != 0 || header[1] != 0 {
                return Err(io::ErrorKind::InvalidData.into());
            }
            n -= header.len();
        }
        if !(14..=self.frame_limit).contains(&n) {
            return Err(io::ErrorKind::InvalidData.into());
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
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        if n as usize != frame.len() + if self.vnet { header.len() } else { 0 } {
            return Err(io::ErrorKind::WriteZero.into());
        }
        Ok(())
    }

    pub(crate) fn uso_enabled(&self) -> bool {
        self.uso
    }

    /// The mask names every attempted frame, even when the write fails.
    pub(crate) fn write_batch<'a>(
        &mut self,
        mut frames: impl Iterator<Item = &'a [u8]>,
    ) -> (u64, io::Result<usize>) {
        let Some(first) = frames.next() else {
            return (0, Ok(0));
        };
        if self.uso
            && (14..=self.frame_limit).contains(&first.len())
            && let Some((mask, result)) =
                uso::write(self.fd.as_raw_fd(), first, frames, self.frame_limit)
        {
            if result
                .as_ref()
                .is_err_and(|e| matches!(e.raw_os_error(), Some(libc::EINVAL | libc::EOPNOTSUPP)))
            {
                self.uso = false;
            } else {
                return (mask, result);
            }
        }
        (1, self.write_frame(first).map(|()| first.len()))
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
}
