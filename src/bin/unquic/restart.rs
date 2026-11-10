//! Handover format version 1 (design section 11): re-exec with TAP queues.
use std::{
    ffi::OsString,
    fs,
    io::{self, Read},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

/// What `adopt-version` prints; a successor must print exactly this.
pub const VERSION: &str = "unquic-adopt 1\n";

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn hex(name: &str) -> String {
    name.bytes().map(|byte| format!("{byte:02x}")).collect()
}

/// One configured session: its name, queue count and inner MTU.
pub struct Session<'a> {
    pub name: &'a str,
    pub queues: usize,
    pub mtu: u16,
}

/// What a re-executed daemon inherits from its predecessor.
pub struct Inherited {
    pub taps: Vec<Option<unquic::tap::Tap>>,
    pub pid_file: Option<PathBuf>,
}

/// Reads and removes the handover announcement. None means a fresh start.
/// Call before any thread exists.
pub fn entry(sessions: &[Session<'_>]) -> io::Result<Option<Inherited>> {
    let read = |name| std::env::var_os(name);
    let (marker, pid_file, listen_pid, listen_fds, names) = (
        read("UNQUIC_REEXEC"),
        read("UNQUIC_PIDFILE"),
        read("LISTEN_PID"),
        read("LISTEN_FDS"),
        read("LISTEN_FDNAMES"),
    );
    // SAFETY: no other thread exists yet, so nothing reads the environment concurrently.
    unsafe {
        for name in [
            "UNQUIC_REEXEC",
            "UNQUIC_PIDFILE",
            "LISTEN_PID",
            "LISTEN_FDS",
            "LISTEN_FDNAMES",
        ] {
            std::env::remove_var(name);
        }
    }
    let Some(marker) = marker else {
        return Ok(None);
    };
    let pid = std::process::id().to_string();
    if marker.to_str() != Some(&format!("1:{pid}"))
        || listen_pid.as_deref().and_then(|p| p.to_str()) != Some(&pid)
    {
        return Err(invalid("handover announcement names another process"));
    }
    let count: usize = listen_fds
        .as_deref()
        .and_then(|n| n.to_str())
        .and_then(|n| n.parse().ok())
        .filter(|&n| n <= 64 * 64)
        .ok_or_else(|| invalid("invalid handover descriptor count"))?;
    let names = names.unwrap_or_default();
    let names: Vec<&str> = match names.to_str() {
        Some("") if count == 0 => Vec::new(),
        Some(text) => text.split(':').collect(),
        None => return Err(invalid("invalid handover descriptor names")),
    };
    if names.len() != count {
        return Err(invalid("handover names and descriptors disagree"));
    }
    // Take only descriptors that exist; a bogus count must not close unrelated ones.
    let mut fds = Vec::with_capacity(count);
    for index in 0..count {
        let fd = 3 + index as RawFd;
        // SAFETY: F_GETFD only queries the descriptor table.
        if unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
            return Err(invalid("announced handover descriptor is not open"));
        }
        // SAFETY: the predecessor passed this open descriptor to us alone.
        fds.push(unsafe { OwnedFd::from_raw_fd(fd) });
    }
    let mut slots: Vec<Vec<Option<OwnedFd>>> = sessions
        .iter()
        .map(|session| (0..session.queues).map(|_| None).collect())
        .collect();
    let keys: Vec<String> = sessions.iter().map(|session| hex(session.name)).collect();
    for (name, fd) in names.into_iter().zip(fds) {
        let mut fields = name.split('.');
        let (Some("tap"), Some(queue), Some(total), Some(key), None) = (
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
        ) else {
            return Err(invalid("unknown handover descriptor name"));
        };
        let session = keys
            .iter()
            .position(|k| k == key)
            .ok_or_else(|| invalid("handover names an unconfigured session"))?;
        let (Ok(queue), Ok(total)) = (queue.parse::<usize>(), total.parse::<usize>()) else {
            return Err(invalid("invalid handover queue"));
        };
        let slot = slots[session]
            .get_mut(queue)
            .filter(|_| total == sessions[session].queues)
            .ok_or_else(|| invalid("handover queue count differs from the configuration"))?;
        if slot.replace(fd).is_some() {
            return Err(invalid("duplicate handover queue"));
        }
    }
    let mut taps = Vec::with_capacity(sessions.len());
    for (session, slots) in sessions.iter().zip(slots) {
        let queues: Option<Vec<OwnedFd>> = slots.into_iter().collect();
        let queues = queues.ok_or_else(|| invalid("handover lacks a configured queue"))?;
        taps.push(Some(unquic::tap::Tap::adopt(queues, session.mtu)?));
    }
    let pid_file = pid_file.filter(|path| !path.is_empty()).map(PathBuf::from);
    Ok(Some(Inherited { taps, pid_file }))
}

/// Runs `binary adopt-version` and requires exactly [`VERSION`] within three seconds.
pub fn probe(binary: &Path) -> io::Result<()> {
    let mut child = Command::new(binary)
        .arg("adopt-version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(3);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(io::ErrorKind::TimedOut, "adoption probe"));
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let mut output = Vec::new();
    if let Some(stdout) = child.stdout.as_mut() {
        stdout.take(64).read_to_end(&mut output)?;
    }
    if status.success() && output == VERSION.as_bytes() {
        Ok(())
    } else {
        Err(io::Error::other(
            "successor speaks another adoption protocol",
        ))
    }
}

/// Execs `binary` with `queues` as descriptors 3 onward, announced as version 1
/// requires. Returns only on failure, with the caller's descriptors intact.
pub fn exec(
    binary: &Path,
    args: &[OsString],
    sessions: &[Session<'_>],
    queues: &[(usize, usize, OwnedFd)],
    pid_file: Option<&Path>,
) -> io::Error {
    let mut names = Vec::with_capacity(queues.len());
    for &(session, queue, _) in queues {
        let Some(configured) = sessions.get(session) else {
            return invalid("handed-over queue of an unconfigured session");
        };
        names.push(format!(
            "tap.{queue}.{}.{}",
            configured.queues,
            hex(configured.name)
        ));
    }
    let raw: Vec<RawFd> = queues.iter().map(|(_, _, fd)| fd.as_raw_fd()).collect();
    let pid = std::process::id().to_string();
    let mut command = Command::new(binary);
    command
        .args(args)
        .env("LISTEN_PID", &pid)
        .env("LISTEN_FDS", raw.len().to_string())
        .env("LISTEN_FDNAMES", names.join(":"))
        .env("UNQUIC_REEXEC", format!("1:{pid}"))
        .env(
            "UNQUIC_PIDFILE",
            pid_file.map(Path::as_os_str).unwrap_or_default(),
        );
    // A private descriptor table keeps the shuffle away from live descriptors;
    // if exec fails, only this short-lived thread's copy is lost.
    let spawned = std::thread::Builder::new()
        .name("unquic-exec".into())
        .spawn(move || -> io::Error {
            // SAFETY: unshare affects only this thread's descriptor table.
            if unsafe { libc::unshare(libc::CLONE_FILES) } < 0 {
                return io::Error::last_os_error();
            }
            let high = 3 + raw.len() as RawFd;
            let mut copies = Vec::with_capacity(raw.len());
            for &fd in &raw {
                // SAFETY: fd is open in this private table; the copy lands above the targets.
                let copy = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, high) };
                if copy < 0 {
                    return io::Error::last_os_error();
                }
                copies.push(copy);
            }
            for (index, &copy) in copies.iter().enumerate() {
                // SAFETY: dup2 into 3.. clears close-on-exec for the successor only.
                if unsafe { libc::dup2(copy, 3 + index as RawFd) } < 0 {
                    return io::Error::last_os_error();
                }
            }
            command.exec()
        });
    match spawned {
        Ok(thread) => thread
            .join()
            .unwrap_or_else(|_| io::Error::other("exec thread failed")),
        Err(error) => error,
    }
}

/// Asks the daemon named by `pid_file` to restart, as `rs-l2tpd --restart` does.
pub fn signal(pid_file: &Path) -> io::Result<()> {
    let mut text = String::new();
    fs::File::open(pid_file)?
        .take(33)
        .read_to_string(&mut text)?;
    let pid = text
        .strip_suffix('\n')
        .filter(|n| !n.is_empty() && n.len() <= 10 && n.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|n| n.parse::<libc::pid_t>().ok())
        .filter(|&pid| pid > 1)
        .ok_or_else(|| invalid("invalid PID file"))?;
    // SAFETY: kill only sends a signal to the named process.
    if unsafe { libc::kill(pid, libc::SIGUSR2) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_encode_sessions_without_colons() {
        assert_eq!(hex("uq-a:b"), "75712d613a62");
        assert!(!hex("uq-a:b").contains(':'));
    }

    #[test]
    fn pid_files_must_hold_one_plain_pid() {
        let path = std::env::temp_dir().join(format!("unquic-signal-{}", std::process::id()));
        for text in ["", "1\n", "12a\n", "123", "99999999999\n"] {
            fs::write(&path, text).unwrap();
            assert_eq!(
                signal(&path).unwrap_err().kind(),
                io::ErrorKind::InvalidInput,
                "{text:?}"
            );
        }
        fs::remove_file(&path).unwrap();
    }
}
