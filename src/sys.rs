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

pub(crate) fn getrandom(bytes: &mut [u8], flags: libc::c_uint) -> io::Result<usize> {
    // Full LTO can merge libc's getrandom with std's nullable weak symbol.
    // SAFETY: SYS_getrandom takes a writable buffer, its length and unsigned flags.
    let count =
        unsafe { libc::syscall(libc::SYS_getrandom, bytes.as_mut_ptr(), bytes.len(), flags) };
    if count < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(count as usize)
    }
}

pub(crate) fn random_tag() -> io::Result<Option<u128>> {
    random_tag_with(|bytes| getrandom(bytes, libc::GRND_NONBLOCK))
}

fn random_tag_with(
    mut read: impl FnMut(&mut [u8]) -> io::Result<usize>,
) -> io::Result<Option<u128>> {
    let mut bytes = [0; 16];
    let mut filled = 0;
    while filled < bytes.len() {
        match read(&mut bytes[filled..]) {
            Ok(0) => {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            Ok(count) => filled += count,
            Err(error) => match error.kind() {
                io::ErrorKind::Interrupted => continue,
                io::ErrorKind::WouldBlock => return Ok(None),
                _ => return Err(error),
            },
        }
    }
    Ok(Some(u128::from_ne_bytes(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_tag_handles_short_reads_interruptions_and_errors() {
        let mut calls = 0;
        let result = random_tag_with(|bytes| {
            calls += 1;
            if calls == 1 {
                return Err(io::ErrorKind::Interrupted.into());
            }
            let count = bytes.len().min(3);
            bytes[..count].fill(0x5a);
            Ok(count)
        })
        .unwrap();
        assert_eq!(result, Some(u128::from_ne_bytes([0x5a; 16])));
        assert_eq!(calls, 7);
        assert_eq!(
            random_tag_with(|_| Err(io::ErrorKind::WouldBlock.into())).unwrap(),
            None
        );
        assert_eq!(
            random_tag_with(|_| Ok(0)).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        assert_eq!(
            random_tag_with(|_| Err(io::ErrorKind::PermissionDenied.into()))
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        let mut first = true;
        assert_eq!(
            random_tag_with(|bytes| {
                if first {
                    first = false;
                    bytes[0] = 1;
                    Ok(1)
                } else {
                    Err(io::ErrorKind::WouldBlock.into())
                }
            })
            .unwrap(),
            None
        );
    }
}
