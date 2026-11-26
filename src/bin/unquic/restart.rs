//! Handover format version 1 (design section 11): re-exec with TAP queues.
use std::{
    ffi::{OsStr, OsString},
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

/// What a daemon inherits at startup, from the predecessor that re-executed
/// it or from a service manager's descriptor store.
pub struct Inherited {
    pub taps: Vec<Option<unquic::tap::Tap>>,
    pub pid_file: Option<PathBuf>,
    /// A predecessor re-executed this image, which is therefore already detached.
    pub reexec: bool,
    /// Announced names this process did not adopt. It closed its descriptors
    /// for them; a descriptor store still holds its own until told to forget.
    pub dropped: Vec<String>,
}

/// The name a session's queue goes by in an announcement and in a store.
pub fn queue_name(sessions: &[Session<'_>], session: usize, queue: usize) -> Option<String> {
    let configured = sessions.get(session)?;
    Some(format!(
        "tap.{queue}.{}.{}",
        configured.queues,
        hex(configured.name)
    ))
}

/// Reads and removes the descriptor announcement. None means a fresh start.
/// Call before any thread exists.
// DECISION(2026-11-27): descriptors a service manager stored are adopted like a predecessor's,
//   and anything wrong with them means "adopt nothing" where a handover fails. Why: after a
//   crash the manager passes the same descriptors to every restart, so refusing them would
//   fail every restart. Rejected: ignoring them (their queues stay attached, and attaching
//   by name is then refused as a device in use).
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
    let pid = std::process::id().to_string();
    let ours = listen_pid.as_deref().and_then(|p| p.to_str()) == Some(&pid);
    let reexec = match marker {
        Some(marker) if ours && marker.to_str() == Some(&format!("1:{pid}")) => true,
        Some(_) => return Err(invalid("handover announcement names another process")),
        // A service manager passes what it stored to the process it starts.
        None if ours => false,
        None => return Ok(None),
    };
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
    // Take only descriptors that exist; a bogus count must not close unrelated ones.
    let mut fds = Vec::with_capacity(count);
    for index in 0..count {
        let fd = 3 + index as RawFd;
        // SAFETY: F_GETFD only queries the descriptor table.
        if unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
            return Err(invalid("announced handover descriptor is not open"));
        }
        // SAFETY: whoever announced this open descriptor passed it to us alone.
        fds.push(Some(unsafe { OwnedFd::from_raw_fd(fd) }));
    }
    let plan = if names.len() == count {
        assign(&names, sessions)
    } else {
        Err(invalid("handover names and descriptors disagree"))
    };
    let plan = match plan {
        Ok(plan) => plan,
        Err(error) if reexec => return Err(error),
        Err(_) => sessions.iter().map(|_| None).collect(),
    };
    let mut adopted = vec![false; count];
    let mut taps = Vec::with_capacity(sessions.len());
    for (session, places) in sessions.iter().zip(plan) {
        let queues: Option<Vec<OwnedFd>> = places.as_ref().and_then(|places| {
            places
                .iter()
                .map(|&place| fds.get_mut(place).and_then(Option::take))
                .collect()
        });
        let tap = match queues.map(|queues| unquic::tap::Tap::adopt(queues, session.mtu)) {
            Some(Ok(tap)) => Some(tap),
            Some(Err(error)) if reexec => return Err(error),
            _ => None,
        };
        if tap.is_some() {
            for place in places.into_iter().flatten() {
                if let Some(slot) = adopted.get_mut(place) {
                    *slot = true;
                }
            }
        }
        taps.push(tap);
    }
    // Queues nobody adopted close here, before the runtime attaches by name.
    drop(fds);
    let dropped = names
        .iter()
        .zip(&adopted)
        .filter(|&(_, &adopted)| !adopted)
        .map(|(name, _)| (*name).to_owned())
        .collect();
    let pid_file = pid_file.filter(|path| !path.is_empty()).map(PathBuf::from);
    Ok(Some(Inherited {
        taps,
        pid_file,
        reexec,
        dropped,
    }))
}

/// Sorts the announced queue names into the configured sessions: for each
/// session the positions of its queues in queue order, or None when the
/// session must attach by name instead.
// DECISION(2026-11-27): a successor adopts the sessions whose name and queue count it still
//   configures and closes every other announced queue. Why: a reload re-executes on a changed
//   file, and a session that is new, gone or resized must not stop the daemon. Rejected:
//   refusing any mismatch (the daemon dies on the first changed session).
fn assign(names: &[&str], sessions: &[Session<'_>]) -> io::Result<Vec<Option<Vec<usize>>>> {
    let mut slots: Vec<Vec<Option<usize>>> = sessions
        .iter()
        .map(|session| vec![None; session.queues])
        .collect();
    let keys: Vec<String> = sessions.iter().map(|session| hex(session.name)).collect();
    for (place, name) in names.iter().enumerate() {
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
        let (Ok(queue), Ok(total)) = (queue.parse::<usize>(), total.parse::<usize>()) else {
            return Err(invalid("invalid handover queue"));
        };
        // A session that is gone, or whose queue count changed, adopts nothing.
        let slot = keys
            .iter()
            .position(|k| k == key)
            .filter(|&session| sessions.get(session).is_some_and(|s| s.queues == total))
            .and_then(|session| slots.get_mut(session)?.get_mut(queue));
        if let Some(slot) = slot
            && slot.replace(place).is_some()
        {
            return Err(invalid("duplicate handover queue"));
        }
    }
    Ok(slots
        .into_iter()
        .map(|queues| queues.into_iter().collect())
        .collect())
}

/// Runs `binary` with `args` for at most three seconds. Returns whether it
/// succeeded and the first 64 bytes it printed; its diagnostics go to ours.
fn ask(binary: &Path, args: &[&OsStr]) -> io::Result<(bool, Vec<u8>)> {
    let mut child = Command::new(binary)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(3);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(io::ErrorKind::TimedOut, "successor probe"));
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let mut output = Vec::new();
    if let Some(stdout) = child.stdout.as_mut() {
        stdout.take(64).read_to_end(&mut output)?;
    }
    Ok((status.success(), output))
}

/// Requires `binary adopt-version` to print exactly [`VERSION`] and
/// `binary check-config <config>` to accept the file as it is now.
// DECISION(2026-11-27): the successor binary checks the configuration before the handover.
//   Why: a file the successor rejects would end the daemon after its workers stopped.
//   Rejected: parsing it in the running daemon (an upgraded binary may read other keys).
pub fn probe(binary: &Path, config: &OsStr) -> io::Result<()> {
    if ask(binary, &["adopt-version".as_ref()])? != (true, VERSION.as_bytes().to_vec()) {
        return Err(io::Error::other(
            "successor speaks another adoption protocol",
        ));
    }
    if !ask(binary, &["check-config".as_ref(), config])?.0 {
        return Err(io::Error::other("successor rejects the configuration file"));
    }
    Ok(())
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
        let Some(name) = queue_name(sessions, session, queue) else {
            return invalid("handed-over queue of an unconfigured session");
        };
        names.push(name);
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

/// Asks the daemon named by `pid_file` for its report and returns it once the
/// daemon has replaced its status file, within three seconds.
pub fn status(pid_file: &Path) -> io::Result<String> {
    use std::os::unix::fs::MetadataExt;
    let path = super::daemon::status_path(pid_file);
    let inode = || fs::metadata(&path).map(|file| file.ino()).ok();
    let before = inode();
    signal(pid_file, libc::SIGUSR1)?;
    let deadline = Instant::now() + Duration::from_secs(3);
    while inode().is_none_or(|now| Some(now) == before) {
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the daemon published no status",
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    fs::read_to_string(&path)
}

/// Sends `signal` to the daemon named by `pid_file`, which is checked as
/// `rs-l2tpd --restart` checks its own.
pub fn signal(pid_file: &Path, signal: libc::c_int) -> io::Result<()> {
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
    if unsafe { libc::kill(pid, signal) } < 0 {
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
    fn a_successor_adopts_only_sessions_it_still_configures_unchanged() {
        let session = |name, queues| Session {
            name,
            queues,
            mtu: 1500,
        };
        let (a, b, gone) = (hex("a"), hex("b"), hex("gone"));
        let names = [
            format!("tap.0.2.{a}"),
            format!("tap.0.1.{gone}"),
            format!("tap.1.2.{a}"),
            format!("tap.0.1.{b}"),
        ];
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        // Unchanged, resized, new: only the first adopts, in queue order.
        let sessions = [session("a", 2), session("b", 2), session("new", 1)];
        assert_eq!(
            assign(&names, &sessions).unwrap(),
            [Some(vec![0, 2]), None, None]
        );
        // A session missing one of its queues attaches by name.
        assert_eq!(assign(&names[..1], &[session("a", 2)]).unwrap(), [None]);
        // A queue index past the count is nobody's.
        assert_eq!(
            assign(&[&format!("tap.2.2.{a}")], &[session("a", 2)]).unwrap(),
            [None]
        );
        for bad in [
            vec!["sock.0.1.61"],
            vec!["tap.0.1"],
            vec!["tap.0.1.61.x"],
            vec!["tap.x.1.61"],
            vec!["tap.0.x.61"],
            vec!["tap.0.1.61", "tap.0.1.61"],
        ] {
            assert!(assign(&bad, &[session("a", 1)]).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn pid_files_must_hold_one_plain_pid() {
        let path = std::env::temp_dir().join(format!("unquic-signal-{}", std::process::id()));
        for text in ["", "1\n", "12a\n", "123", "99999999999\n"] {
            fs::write(&path, text).unwrap();
            assert_eq!(
                signal(&path, libc::SIGUSR2).unwrap_err().kind(),
                io::ErrorKind::InvalidInput,
                "{text:?}"
            );
        }
        fs::remove_file(&path).unwrap();
    }
}
