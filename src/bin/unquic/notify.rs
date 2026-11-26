//! The service manager's notification socket, spoken directly: no systemd
//! library is linked (design section 12). Without `NOTIFY_SOCKET` every call
//! does nothing, so the daemon runs the same under any launcher.
use std::{
    io,
    os::{
        fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd},
        linux::net::SocketAddrExt,
        unix::net::{SocketAddr, UnixDatagram},
    },
    time::Duration,
};

/// A connected notification socket, or nothing.
pub struct Notifier(Option<UnixDatagram>);

impl Notifier {
    /// Connects to the socket `NOTIFY_SOCKET` names.
    pub fn from_env() -> Self {
        Self::connect(std::env::var_os("NOTIFY_SOCKET").as_deref())
    }

    /// Connects to `name`: a path, or an abstract name after `@`. Anything
    /// else, and any failure, leaves an inactive notifier.
    fn connect(name: Option<&std::ffi::OsStr>) -> Self {
        use std::os::unix::ffi::OsStrExt;
        let connect = || {
            let name = name?;
            let address = match name.as_bytes() {
                [b'@', name @ ..] => SocketAddr::from_abstract_name(name).ok()?,
                [b'/', ..] => SocketAddr::from_pathname(name).ok()?,
                _ => return None,
            };
            let socket = UnixDatagram::unbound().ok()?;
            socket.connect_addr(&address).ok()?;
            Some(socket)
        };
        Self(connect())
    }

    /// Whether a service manager listens.
    pub fn active(&self) -> bool {
        self.0.is_some()
    }

    /// Sends newline-separated `KEY=value` fields. A manager that has gone
    /// away is not an error the daemon acts on.
    pub fn send(&self, fields: &str) {
        if let Some(socket) = &self.0 {
            let _ = socket.send(fields.as_bytes());
        }
    }

    /// Sends `fields` with one descriptor attached.
    fn send_fd(&self, fields: &str, fd: BorrowedFd<'_>) -> io::Result<()> {
        let Some(socket) = &self.0 else {
            return Ok(());
        };
        let mut iov = libc::iovec {
            iov_base: fields.as_ptr().cast_mut().cast(),
            iov_len: fields.len(),
        };
        // One descriptor needs CMSG_SPACE(4) bytes, 24 on 64-bit Linux; the
        // u64 words keep the header aligned.
        let mut control = [0_u64; 4];
        // SAFETY: a zeroed msghdr is valid; iov and control outlive the call,
        // and the control buffer has room for one header and one descriptor.
        let sent = unsafe {
            let space = libc::CMSG_SPACE(size_of::<libc::c_int>() as u32) as usize;
            if space > size_of_val(&control) {
                return Err(io::ErrorKind::InvalidInput.into());
            }
            let mut message: libc::msghdr = std::mem::zeroed();
            message.msg_iov = &mut iov;
            message.msg_iovlen = 1;
            message.msg_control = control.as_mut_ptr().cast();
            message.msg_controllen = space as _;
            let header = libc::CMSG_FIRSTHDR(&message);
            (*header).cmsg_level = libc::SOL_SOCKET;
            (*header).cmsg_type = libc::SCM_RIGHTS;
            (*header).cmsg_len = libc::CMSG_LEN(size_of::<libc::c_int>() as u32) as _;
            std::ptr::write_unaligned(libc::CMSG_DATA(header).cast(), fd.as_raw_fd());
            libc::sendmsg(socket.as_raw_fd(), &message, libc::MSG_NOSIGNAL)
        };
        if sent < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    /// Asks the manager to keep `fd` under `name` and hand it to the next
    /// start (`FDSTORE=1`). The manager keeps its own duplicate.
    pub fn store(&self, name: &str, fd: BorrowedFd<'_>) {
        let _ = self.send_fd(&format!("FDSTORE=1\nFDNAME={name}"), fd);
    }

    /// Asks the manager to drop what it keeps under `name`.
    pub fn forget(&self, name: &str) {
        self.send(&format!("FDSTOREREMOVE=1\nFDNAME={name}"));
    }

    /// Returns once the manager has handled everything sent before, or after
    /// five seconds: it closes the pipe end sent with `BARRIER=1` when it gets
    /// to it.
    pub fn barrier(&self) {
        if self.0.is_none() {
            return;
        }
        let mut fds = [-1; 2];
        // SAFETY: fds has room for the two descriptors pipe2 returns.
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
            return;
        }
        // SAFETY: pipe2 just returned these descriptors; each has one owner.
        let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        if self.send_fd("BARRIER=1", write.as_fd()).is_err() {
            return;
        }
        drop(write);
        let mut poll = libc::pollfd {
            fd: read.as_raw_fd(),
            events: 0,
            revents: 0,
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            // SAFETY: poll reads and writes the one pollfd it is given.
            let ready = unsafe { libc::poll(&mut poll, 1, left.as_millis() as libc::c_int) };
            if ready >= 0 || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsFd;

    /// A stand-in manager: an abstract socket that reads one message and the
    /// descriptors that came with it.
    fn receive(socket: &UnixDatagram) -> (String, Vec<OwnedFd>) {
        let mut bytes = [0_u8; 256];
        let mut control = [0_u64; 8];
        let mut iov = libc::iovec {
            iov_base: bytes.as_mut_ptr().cast(),
            iov_len: bytes.len(),
        };
        // SAFETY: the buffers outlive the call; received descriptors are owned below.
        unsafe {
            let mut message: libc::msghdr = std::mem::zeroed();
            message.msg_iov = &mut iov;
            message.msg_iovlen = 1;
            message.msg_control = control.as_mut_ptr().cast();
            message.msg_controllen = size_of_val(&control) as _;
            let length = libc::recvmsg(socket.as_raw_fd(), &mut message, libc::MSG_CMSG_CLOEXEC);
            assert!(length >= 0);
            let mut fds = Vec::new();
            let mut header = libc::CMSG_FIRSTHDR(&message);
            while !header.is_null() {
                if (*header).cmsg_level == libc::SOL_SOCKET
                    && (*header).cmsg_type == libc::SCM_RIGHTS
                {
                    let fd: libc::c_int = std::ptr::read_unaligned(libc::CMSG_DATA(header).cast());
                    fds.push(OwnedFd::from_raw_fd(fd));
                }
                header = libc::CMSG_NXTHDR(&message, header);
            }
            (
                String::from_utf8_lossy(&bytes[..length as usize]).into_owned(),
                fds,
            )
        }
    }

    #[test]
    fn fields_descriptors_and_the_barrier_reach_a_manager() {
        use std::io::{Read, Write};
        let name = format!("unquic-notify-test-{}", std::process::id());
        let manager =
            UnixDatagram::bind_addr(&SocketAddr::from_abstract_name(name.as_bytes()).unwrap())
                .unwrap();
        let notifier = Notifier::connect(Some(format!("@{name}").as_ref()));
        assert!(notifier.active());
        for inactive in [
            None,
            Some("relative"),
            Some("@unquic-notify-nobody-listens"),
        ] {
            let notifier = Notifier::connect(inactive.map(std::ffi::OsStr::new));
            assert!(!notifier.active(), "{inactive:?}");
            // Every call is then a no-op that returns at once.
            notifier.send("READY=1");
            notifier.forget("x");
            notifier.barrier();
        }

        notifier.send("READY=1\nSTATUS=TAPs ready");
        let (text, fds) = receive(&manager);
        assert_eq!(text, "READY=1\nSTATUS=TAPs ready");
        assert!(fds.is_empty());

        // A stored descriptor arrives as a duplicate of the same open file.
        let mut ends = [-1; 2];
        // SAFETY: ends has room for the two descriptors pipe2 returns.
        assert_eq!(
            unsafe { libc::pipe2(ends.as_mut_ptr(), libc::O_CLOEXEC) },
            0
        );
        // SAFETY: pipe2 just returned these descriptors; each has one owner.
        let (reader, writer) =
            unsafe { (OwnedFd::from_raw_fd(ends[0]), OwnedFd::from_raw_fd(ends[1])) };
        notifier.store("tap.0.1.61", writer.as_fd());
        let (text, mut fds) = receive(&manager);
        assert_eq!(text, "FDSTORE=1\nFDNAME=tap.0.1.61");
        assert_eq!(fds.len(), 1);
        std::fs::File::from(fds.remove(0)).write_all(b"x").unwrap();
        let mut byte = [0];
        std::fs::File::from(reader).read_exact(&mut byte).unwrap();
        assert_eq!(byte, *b"x");

        notifier.forget("tap.0.1.61");
        assert_eq!(receive(&manager).0, "FDSTOREREMOVE=1\nFDNAME=tap.0.1.61");

        // The barrier returns when the manager closes the pipe end it was sent.
        let closer = std::thread::spawn(move || {
            let (text, fds) = receive(&manager);
            assert_eq!((text.as_str(), fds.len()), ("BARRIER=1", 1));
        });
        let started = std::time::Instant::now();
        notifier.barrier();
        closer.join().unwrap();
        assert!(started.elapsed() < Duration::from_secs(4));
    }
}
