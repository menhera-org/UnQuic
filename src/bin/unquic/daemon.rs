//! `unquic start`: double fork with a readiness pipe, as design section 12.1
//! describes. The original parent exits 0 only after the daemon has published
//! its PID file and every configured TAP exists.
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::OpenOptionsExt,
    },
    path::{Path, PathBuf},
    sync::atomic::{AtomicI32, Ordering},
};

/// Where this process continues after [`start`].
pub enum Started {
    /// The original parent, with its exit status: 0 once the daemon is ready.
    Parent(u8),
    /// The daemon itself.
    Daemon(Readiness),
}

/// The daemon's end of the readiness pipe and the PID file to publish.
pub struct Readiness {
    pipe: File,
    pid_file: PathBuf,
}

/// A published PID file, removed on drop while it still names this process.
pub struct PidFile(PathBuf);

static DAEMON: AtomicI32 = AtomicI32::new(0);

extern "C" fn forward(signal: libc::c_int) {
    let pid = DAEMON.load(Ordering::Relaxed);
    if pid > 0 {
        // SAFETY: kill is async-signal-safe; the PID came from our own daemon.
        unsafe { libc::kill(pid, signal) };
    }
}

fn last() -> io::Error {
    io::Error::last_os_error()
}

/// Forks twice. Call before any thread exists.
pub fn start(pid_file: PathBuf) -> io::Result<Started> {
    let mut fds = [-1; 2];
    // SAFETY: fds has room for the two descriptors pipe2 returns.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        return Err(last());
    }
    // SAFETY: pipe2 just returned these descriptors; each File owns one.
    let (read, write) = unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) };
    // SAFETY: no other thread exists, so the child may keep running Rust code.
    match unsafe { libc::fork() } {
        -1 => Err(last()),
        0 => {
            drop(read);
            detach(write, pid_file)
        }
        child => {
            drop(write);
            parent(read, child).map(Started::Parent)
        }
    }
}

fn detach(mut pipe: File, pid_file: PathBuf) -> io::Result<Started> {
    // SAFETY: plain syscalls in the single-threaded first child.
    if unsafe { libc::setsid() } < 0 {
        return Err(last());
    }
    // SAFETY: still single-threaded; the first child exits at once.
    match unsafe { libc::fork() } {
        -1 => return Err(last()),
        0 => {}
        // SAFETY: _exit skips destructors the daemon still owns.
        _ => unsafe { libc::_exit(0) },
    }
    pipe.write_all(&std::process::id().to_ne_bytes())?;
    let null = File::open("/dev/null")?;
    // SAFETY: both descriptors are open; dup2 replaces standard input.
    if unsafe { libc::dup2(null.as_raw_fd(), 0) } < 0 {
        return Err(last());
    }
    // Do not keep the launch directory's mount busy; every path is absolute.
    std::env::set_current_dir("/")?;
    Ok(Started::Daemon(Readiness { pipe, pid_file }))
}

fn parent(mut pipe: File, child: libc::pid_t) -> io::Result<u8> {
    let mut status = 0;
    // SAFETY: child is our own first child; status is writable.
    while unsafe { libc::waitpid(child, &mut status, 0) } < 0 {
        let error = last();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
    let mut pid = [0; 4];
    if pipe.read_exact(&mut pid).is_err() {
        return Ok(1);
    }
    DAEMON.store(i32::from_ne_bytes(pid), Ordering::Relaxed);
    // A launcher stopping the parent before readiness stops the daemon too.
    // SAFETY: zeroed sigaction, then a valid handler and an empty mask.
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = forward as *const () as usize;
    // SAFETY: action stays alive for these calls.
    unsafe {
        if libc::sigemptyset(&mut action.sa_mask) < 0
            || libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut()) < 0
            || libc::sigaction(libc::SIGTERM, &action, std::ptr::null_mut()) < 0
        {
            return Err(last());
        }
    }
    let mut byte = [0];
    loop {
        match pipe.read(&mut byte) {
            Ok(0) => return Ok(1),
            Ok(_) => return Ok(u8::from(byte != [1])),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

impl Readiness {
    /// Publishes the PID file, then releases the parent. Failing to publish is
    /// a startup failure: the parent then sees the pipe close and exits 1.
    pub fn ready(mut self) -> io::Result<PidFile> {
        let pid = PidFile::publish(&self.pid_file)?;
        match self.pipe.write_all(&[1]) {
            // A launcher that already gave up does not stop the daemon.
            Err(error) if error.kind() != io::ErrorKind::BrokenPipe => Err(error),
            _ => Ok(pid),
        }
    }
}

impl PidFile {
    pub fn publish(path: &Path) -> io::Result<Self> {
        let pid = std::process::id();
        let mut temporary = path.as_os_str().to_owned();
        temporary.push(format!(".{pid}.tmp"));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .open(&temporary)?;
        // Readers see either no file or the whole PID.
        let written = file
            .write_all(format!("{pid}\n").as_bytes())
            .and_then(|()| fs::rename(&temporary, path));
        if written.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        written.map(|()| Self(path.to_owned()))
    }
}

impl Drop for PidFile {
    fn drop(&mut self) {
        if fs::read_to_string(&self.0).is_ok_and(|text| text == format!("{}\n", std::process::id()))
        {
            let _ = fs::remove_file(&self.0);
        }
    }
}

// Unit files split on spaces and expand % and $; keep paths that need no quoting.
fn unit_path(path: &Path) -> io::Result<&str> {
    path.to_str()
        .filter(|text| {
            text.starts_with('/')
                && !text.contains(|c: char| {
                    c.is_whitespace() || c.is_control() || "%$\\\"'".contains(c)
                })
        })
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "unit paths must be absolute, without spaces, quotes, backslashes, % or $",
            )
        })
}

/// The unit template of design section 12.1 for this binary and paths.
pub fn unit(binary: &Path, config: &Path, pid_file: &Path) -> io::Result<String> {
    let (binary, config, pid) = (unit_path(binary)?, unit_path(config)?, unit_path(pid_file)?);
    Ok(format!(
        "[Unit]
Description=UnQuic Ethernet transport
Before=network.target network-online.target

[Service]
Type=forking
PIDFile={pid}
ExecStart={binary} start {config} {pid}
TimeoutStartSec=infinity
Restart=on-failure

[Install]
RequiredBy=network.target network-online.target
"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_has_the_design_lines_and_rejects_unsafe_paths() {
        let text = unit(
            Path::new("/usr/bin/unquic"),
            Path::new("/etc/unquic.toml"),
            Path::new("/run/unquic.pid"),
        )
        .unwrap();
        for line in [
            "Before=network.target network-online.target",
            "Type=forking",
            "PIDFile=/run/unquic.pid",
            "ExecStart=/usr/bin/unquic start /etc/unquic.toml /run/unquic.pid",
            "TimeoutStartSec=infinity",
            "Restart=on-failure",
            "RequiredBy=network.target network-online.target",
        ] {
            assert!(text.lines().any(|l| l == line), "{line}");
        }
        // The service must not depend on, or wait for, the targets it precedes.
        for word in ["After=", "Requires=", "Wants=", "wait-online"] {
            assert!(!text.contains(word), "{word}");
        }
        for bad in [
            "relative.toml",
            "/etc/un quic.toml",
            "/etc/%n.toml",
            "/etc/$X.toml",
        ] {
            assert!(
                unit(
                    Path::new("/usr/bin/unquic"),
                    Path::new(bad),
                    Path::new("/run/u.pid")
                )
                .is_err()
            );
        }
    }

    #[test]
    fn pid_file_publishes_whole_and_removes_only_its_own() {
        let path = std::env::temp_dir().join(format!("unquic-pid-{}", std::process::id()));
        let pid = PidFile::publish(&path).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            format!("{}\n", std::process::id())
        );
        drop(pid);
        assert!(!path.exists());
        let pid = PidFile::publish(&path).unwrap();
        // Another daemon has replaced the file: leave it alone.
        fs::write(&path, "1\n").unwrap();
        drop(pid);
        assert_eq!(fs::read_to_string(&path).unwrap(), "1\n");
        fs::remove_file(&path).unwrap();
    }
}
