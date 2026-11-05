use std::io;
use std::os::fd::{FromRawFd, OwnedFd};

pub(crate) fn cvt(value: libc::c_int) -> io::Result<libc::c_int> {
    if value < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(value)
    }
}

pub(crate) fn udp_socket() -> io::Result<OwnedFd> {
    // SAFETY: socket has no pointer arguments and returns a new descriptor.
    let fd = cvt(unsafe {
        libc::socket(
            libc::AF_INET6,
            libc::SOCK_DGRAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
    })?;
    // SAFETY: this is the sole owner of the newly opened descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

pub(crate) fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
