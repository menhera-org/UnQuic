#![cfg(feature = "bin")]
mod common;

use std::{
    ffi::OsString,
    fs,
    io::{self, Read},
    net::{Ipv6Addr, UdpSocket},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering},
    thread,
    time::{Duration, Instant},
};
use unquic::{
    UnquicUuid,
    wire::{Datagram, Record},
};

fn binary() -> OsString {
    std::env::var_os("UNQUIC_TEST_BINARY").unwrap_or_else(|| env!("CARGO_BIN_EXE_unquic").into())
}

struct ConfigFile(PathBuf);
impl ConfigFile {
    fn new(text: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "unquic-cli-{}-{}.toml",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        std::io::Write::write_all(&mut file, text.as_bytes()).unwrap();
        Self(path)
    }
}
impl Drop for ConfigFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

struct Running {
    child: Child,
    _file: ConfigFile,
}
impl Running {
    fn start(text: &str) -> Self {
        let file = ConfigFile::new(text);
        let child = Command::new(binary())
            .arg("run")
            .arg(&file.0)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        Self { child, _file: file }
    }
    fn stop(&mut self) {
        // SAFETY: request a snapshot from this fixture's live child.
        assert_eq!(
            unsafe { libc::kill(self.child.id() as _, libc::SIGUSR1) },
            0
        );
        let mut diagnostic = String::new();
        {
            use std::io::BufRead;
            let mut stderr = std::io::BufReader::new(self.child.stderr.as_mut().unwrap());
            while !diagnostic.contains("socket_overflow:") {
                assert_ne!(stderr.read_line(&mut diagnostic).unwrap(), 0);
            }
        }
        assert!(diagnostic.contains("admission_pressure:"));
        // SAFETY: this PID belongs to the child held and reaped by this fixture.
        assert_eq!(
            unsafe { libc::kill(self.child.id() as _, libc::SIGTERM) },
            0
        );
        until(|| self.child.try_wait().unwrap().is_some());
        assert!(self.child.wait().unwrap().success());
        self.child
            .stderr
            .as_mut()
            .unwrap()
            .read_to_string(&mut diagnostic)
            .unwrap();
        assert!(diagnostic.contains("TAPs ready"), "{diagnostic}");
        assert!(!diagnostic.contains("00112233-4455-4677-8899-aabbccddeeff"));
    }
}
impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn until(mut predicate: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(10);
    while !predicate() {
        assert!(Instant::now() < end, "daemon test timed out");
        thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn provisioning_and_configuration_errors_have_clean_cli_output() {
    let output = Command::new(binary()).arg("gen-uuid").output().unwrap();
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let text = std::str::from_utf8(&output.stdout).unwrap().trim();
    let uuid: UnquicUuid = text.parse().unwrap();
    assert_eq!(uuid.as_bytes()[6] >> 4, 4);
    assert_eq!(uuid.as_bytes()[8] >> 6, 2);
    for args in [vec![], vec!["bad"], vec!["gen-uuid", "extra"], vec!["run"]] {
        let output = Command::new(binary()).args(args).output().unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("usage:"));
    }
    let file = ConfigFile::new(
        "[[sessions]]\nname = 'test'\npeer = 'none'\ncookie = 'secret-invalid-cookie'\ninner_mtu = 1500",
    );
    let output = Command::new(binary())
        .arg("run")
        .arg(&file.0)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let diagnostic = String::from_utf8_lossy(&output.stderr);
    assert!(diagnostic.contains("invalid TOML"));
    assert!(!diagnostic.contains("secret-invalid-cookie"));
}

fn config(name: &str, listen: u16, peer: u16, host: &str, resolver: &str) -> String {
    format!(
        "
[[endpoints]]
name = 'rx'
listen_address = '::'
listen_port = {listen}
[[peers]]
name = 'peer'
endpoint = 'rx'
host = '{host}'
peer_port = {peer}
refresh_seconds = 1
{resolver}
[[sessions]]
name = '{name}'
peer = 'peer'
cookie = '00112233-4455-4677-8899-aabbccddeeff'
inner_mtu = 1500
"
    )
}

#[test]
#[ignore = "requires disposable container with tun and network namespace capabilities"]
fn foreground_readiness_dns_refresh_forwarding_and_signal_shutdown() {
    use common::*;
    // SAFETY: this thread and every later fixture child use a disposable namespace.
    check(unsafe { libc::unshare(libc::CLONE_NEWNET) });
    let net = UdpSocket::bind("[::]:0").unwrap();
    up(&net, c"lo");
    let changed: Ipv6Addr = "fd00::2".parse().unwrap();
    add_ipv6(&net, c"lo", changed);
    let server = UdpSocket::bind("[::1]:0").unwrap();
    server
        .set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    let mode = AtomicU8::new(0); // Hold replies until both TAPs exist.
    let stop = AtomicBool::new(false);
    thread::scope(|scope| {
        let dns = scope.spawn(|| {
            let mut bytes = [0; 512];
            while !stop.load(Ordering::Relaxed) {
                let (length, source) = match server.recv_from(&mut bytes) {
                    Ok(value) => value,
                    Err(e)
                        if e.kind() == io::ErrorKind::WouldBlock
                            || e.kind() == io::ErrorKind::TimedOut =>
                    {
                        continue;
                    }
                    Err(e) => panic!("{e}"),
                };
                while mode.load(Ordering::Relaxed) == 0 && !stop.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(1));
                }
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let mut end = 12;
                while bytes[end] != 0 {
                    end += usize::from(bytes[end]) + 1;
                }
                end += 1;
                let ipv6 = bytes[end..end + 2] == [0, 28];
                assert_eq!(end + 4, length);
                bytes[2..12].copy_from_slice(&[0x81, 0x80, 0, 1, 0, u8::from(ipv6), 0, 0, 0, 0]);
                let mut size = length;
                if ipv6 {
                    bytes[size..size + 12]
                        .copy_from_slice(&[0xc0, 12, 0, 28, 0, 1, 0, 0, 0, 1, 0, 16]);
                    size += 12;
                    let ip = if mode.load(Ordering::Relaxed) == 1 {
                        Ipv6Addr::LOCALHOST
                    } else {
                        changed
                    };
                    bytes[size..size + 16].copy_from_slice(&ip.octets());
                    size += 16;
                }
                server.send_to(&bytes[..size], source).unwrap();
            }
        });
        // Ensure a failed assertion also lets the scoped DNS fixture exit.
        struct Stop<'a>(&'a AtomicBool);
        impl Drop for Stop<'_> {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Relaxed);
            }
        }
        let _stop = Stop(&stop);
        let resolver = format!(
            "[peers.resolver]\nmode = 'static'\nnameservers = ['{}']",
            server.local_addr().unwrap()
        );
        let mut left = Running::start(&config(
            "uq-cli-a",
            40200,
            40201,
            "peer.example.invalid",
            &resolver,
        ));
        let mut right = Running::start(&config("uq-cli-b", 40201, 40200, "::1", ""));
        until(|| {
            // SAFETY: both names are NUL-terminated and the call only queries this namespace.
            unsafe {
                libc::if_nametoindex(c"uq-cli-a".as_ptr()) != 0
                    && libc::if_nametoindex(c"uq-cli-b".as_ptr()) != 0
            }
        });
        assert_eq!(flags(&net, c"uq-cli-a") & libc::IFF_UP as i16, 0);
        assert_eq!(flags(&net, c"uq-cli-b") & libc::IFF_UP as i16, 0);
        up(&net, c"uq-cli-a");
        up(&net, c"uq-cli-b");
        mode.store(1, Ordering::Relaxed);
        let a = raw_socket(c"uq-cli-a", 0x88b5);
        let b = raw_socket(c"uq-cli-b", 0x88b5);
        let mut bytes = [0; 2000];
        let mut frame = [0x5a; 60];
        frame[12..14].copy_from_slice(&0x88b5_u16.to_be_bytes());
        until(|| {
            raw_send(&a, &frame);
            raw_read(&b, &mut bytes).is_some_and(|n| bytes[..n] == frame)
        });
        let outer = raw_socket(c"lo", libc::ETH_P_IPV6 as u16);
        mode.store(2, Ordering::Relaxed);
        until(|| {
            raw_send(&a, &frame);
            for _ in 0..64 {
                let Some(n) = raw_read(&outer, &mut bytes) else {
                    break;
                };
                if n < 62
                    || bytes[20] != 17
                    || bytes[38..54] != changed.octets()
                    || bytes[56..58] != 40201_u16.to_be_bytes()
                {
                    continue;
                }
                if Datagram::decode(&bytes[62..n])
                    .is_ok_and(|d| d.records().any(|r| matches!(r, Record::Data { .. })))
                {
                    return true;
                }
            }
            false
        });
        until(|| {
            raw_send(&b, &frame);
            raw_read(&a, &mut bytes).is_some_and(|n| bytes[..n] == frame)
        });
        left.stop();
        right.stop();
        // Shutdown is not removal: the TAPs stay until explicitly removed.
        for name in [c"uq-cli-a", c"uq-cli-b"] {
            // SAFETY: query only; both children have joined and closed their TAP descriptors.
            assert_ne!(unsafe { libc::if_nametoindex(name.as_ptr()) }, 0);
            let removed = Command::new(binary())
                .arg("remove-tap")
                .arg(name.to_str().unwrap())
                .status()
                .unwrap();
            assert!(removed.success());
            // SAFETY: query only.
            assert_eq!(unsafe { libc::if_nametoindex(name.as_ptr()) }, 0);
        }
        stop.store(true, Ordering::Relaxed);
        dns.join().unwrap();
    });
}

#[test]
#[ignore = "requires disposable container with tun, iproute2 and network namespace capabilities"]
fn restart_re_executes_on_the_same_tap_without_a_gap() {
    use common::*;
    // SAFETY: this thread and the daemon it starts use a disposable namespace.
    check(unsafe { libc::unshare(libc::CLONE_NEWNET) });
    let net = UdpSocket::bind("[::]:0").unwrap();
    let pid_path = std::env::temp_dir().join(format!("unquic-restart-{}.pid", std::process::id()));
    let file = ConfigFile::new(&config("uq-rx", 40400, 40401, "::1", ""));
    let started = Command::new(binary())
        .arg("start")
        .arg(&file.0)
        .arg(&pid_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(started.success());
    let pid: i32 = fs::read_to_string(&pid_path)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // An upper device dies with its lower, so its survival shows no last-close gap.
    add_vlan(&net, c"uq-rx", 7);
    let index = |name: &std::ffi::CStr| {
        // SAFETY: the name is NUL-terminated; the call only queries this namespace.
        unsafe { libc::if_nametoindex(name.as_ptr()) }
    };
    let (tap, upper, mac) = (
        index(c"uq-rx"),
        index(c"uq-rx.7"),
        mac_address(&net, c"uq-rx"),
    );
    assert!(tap != 0 && upper != 0);
    use std::os::unix::fs::MetadataExt;
    let inode = |path: &PathBuf| fs::metadata(path).map(|m| m.ino()).ok();
    let published = inode(&pid_path);
    let signalled = Command::new(binary())
        .arg("restart")
        .arg(&pid_path)
        .status()
        .unwrap();
    assert!(signalled.success());
    // The re-executed image's initial environment carries the handover marker.
    let marker = format!("UNQUIC_REEXEC=1:{pid}");
    until(|| {
        fs::read(format!("/proc/{pid}/environ")).is_ok_and(|bytes| {
            bytes
                .split(|&b| b == 0)
                .any(|entry| entry == marker.as_bytes())
        })
    });
    // The successor republishes the PID file (a new inode) once its TAPs are ready;
    // a signal before then would meet default handlers.
    until(|| inode(&pid_path).is_some_and(|now| Some(now) != published));
    assert_eq!(fs::read_to_string(&pid_path).unwrap(), format!("{pid}\n"));
    assert_eq!((index(c"uq-rx"), index(c"uq-rx.7")), (tap, upper));
    assert_eq!(mac_address(&net, c"uq-rx"), mac);
    // SAFETY: the PID file names the daemon this test started.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    until(|| !pid_path.exists());
    assert_eq!(index(c"uq-rx"), tap, "shutdown removed the TAP");
    let removed = Command::new(binary())
        .args(["remove-tap", "uq-rx"])
        .status()
        .unwrap();
    assert!(removed.success());
    until(|| index(c"uq-rx") == 0);
}

#[test]
#[ignore = "requires disposable container with tun and network namespace capabilities"]
fn restart_with_an_incompatible_successor_keeps_the_daemon_running() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    // This thread and the daemon it starts use a disposable namespace.
    check_namespace();
    let directory = std::env::temp_dir().join(format!("unquic-refuse-{}", std::process::id()));
    fs::create_dir_all(&directory).unwrap();
    let copy = directory.join("unquic");
    fs::copy(binary(), &copy).unwrap();
    let pid_path = directory.join("unquic.pid");
    let file = ConfigFile::new(&config("uq-refuse", 40500, 40501, "::1", ""));
    let started = Command::new(&copy)
        .arg("start")
        .arg(&file.0)
        .arg(&pid_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(started.success());
    let pid: i32 = fs::read_to_string(&pid_path)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let inode = fs::metadata(&pid_path).unwrap().ino();
    // The installed binary now answers with another adoption protocol.
    let script = directory.join("next");
    fs::write(&script, "#!/bin/sh\necho unquic-adopt 2\n").unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    fs::rename(&script, &copy).unwrap();
    let signalled = Command::new(binary())
        .arg("restart")
        .arg(&pid_path)
        .status()
        .unwrap();
    assert!(signalled.success());
    thread::sleep(Duration::from_millis(500));
    let environ = fs::read(format!("/proc/{pid}/environ")).unwrap();
    assert!(
        !environ
            .split(|&b| b == 0)
            .any(|entry| entry.starts_with(b"UNQUIC_REEXEC="))
    );
    assert_eq!(fs::metadata(&pid_path).unwrap().ino(), inode);
    // SAFETY: the PID file names the daemon this test started, still running.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    until(|| !pid_path.exists());
    let removed = Command::new(binary())
        .args(["remove-tap", "uq-refuse"])
        .status()
        .unwrap();
    assert!(removed.success());
    let _ = fs::remove_dir_all(&directory);
}

#[test]
#[ignore = "requires disposable container with tun and network namespace capabilities"]
fn restart_whose_exec_fails_after_a_passing_probe_resumes_on_the_same_queues() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    // This thread and the daemon it starts use a disposable namespace.
    check_namespace();
    let net = UdpSocket::bind("[::]:0").unwrap();
    let directory = std::env::temp_dir().join(format!("unquic-noexec-{}", std::process::id()));
    fs::create_dir_all(&directory).unwrap();
    let copy = directory.join("unquic");
    let install = |text: Option<&str>| {
        let next = directory.join("next");
        match text {
            Some(text) => fs::write(&next, text).unwrap(),
            None => drop(fs::copy(binary(), &next).unwrap()),
        }
        fs::set_permissions(&next, fs::Permissions::from_mode(0o755)).unwrap();
        fs::rename(&next, &copy).unwrap();
    };
    install(None);
    let (pid_path, log) = (directory.join("unquic.pid"), directory.join("stderr"));
    let file = ConfigFile::new(&config("uq-noexec", 40600, 40601, "::1", ""));
    let started = Command::new(&copy)
        .arg("start")
        .arg(&file.0)
        .arg(&pid_path)
        .stdout(Stdio::null())
        .stderr(fs::File::create(&log).unwrap())
        .status()
        .unwrap();
    assert!(started.success());
    let pid: i32 = fs::read_to_string(&pid_path)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    common::add_vlan(&net, c"uq-noexec", 7);
    let index = |name: &std::ffi::CStr| {
        // SAFETY: the name is NUL-terminated; the call only queries this namespace.
        unsafe { libc::if_nametoindex(name.as_ptr()) }
    };
    let devices = (index(c"uq-noexec"), index(c"uq-noexec.7"));
    assert!(devices.0 != 0 && devices.1 != 0);
    let inode = || fs::metadata(&pid_path).map(|m| m.ino()).ok();
    let published = inode();
    let logged = |line: &str| fs::read_to_string(&log).map_or(0, |text| text.matches(line).count());
    let reexec = format!("UNQUIC_REEXEC=1:{pid}");
    let reexecuted = || {
        fs::read(format!("/proc/{pid}/environ")).is_ok_and(|bytes| {
            bytes
                .split(|&b| b == 0)
                .any(|entry| entry == reexec.as_bytes())
        })
    };
    let restart = || {
        let signalled = Command::new(binary())
            .arg("restart")
            .arg(&pid_path)
            .status()
            .unwrap();
        assert!(signalled.success());
    };
    // The successor passes the probe, then vanishes, so the exec itself fails.
    install(Some("#!/bin/sh\nrm -f \"$0\"\necho unquic-adopt 1\n"));
    restart();
    until(|| logged("re-exec failed") == 1);
    assert!(!copy.exists() && !reexecuted());
    // The daemon runs again on the queues it kept; the upper device never lost its lower.
    until(|| logged("TAPs ready") == 2);
    assert_eq!(inode(), published);
    assert_eq!((index(c"uq-noexec"), index(c"uq-noexec.7")), devices);
    // A later restart hands those same queues to a real successor.
    install(None);
    restart();
    until(reexecuted);
    until(|| inode().is_some_and(|now| Some(now) != published));
    assert_eq!((index(c"uq-noexec"), index(c"uq-noexec.7")), devices);
    // SAFETY: the PID file names the daemon this test started.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    until(|| !pid_path.exists());
    let removed = Command::new(binary())
        .args(["remove-tap", "uq-noexec"])
        .status()
        .unwrap();
    assert!(removed.success());
    let _ = fs::remove_dir_all(&directory);
}

fn check_namespace() {
    // SAFETY: unshare changes only this test thread's network namespace.
    common::check(unsafe { libc::unshare(libc::CLONE_NEWNET) });
}

#[test]
#[ignore = "requires disposable container with tun and network namespace capabilities"]
fn start_returns_after_taps_exist_and_reports_failure_before_them() {
    use common::*;
    // SAFETY: this thread and the daemons it starts use a disposable namespace.
    check(unsafe { libc::unshare(libc::CLONE_NEWNET) });
    let pid_path = std::env::temp_dir().join(format!("unquic-start-{}.pid", std::process::id()));
    let start = |text: &str| {
        let file = ConfigFile::new(text);
        Command::new(binary())
            .arg("start")
            .arg(&file.0)
            .arg(&pid_path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
    };
    let present = || {
        // SAFETY: the name is NUL-terminated; the call only queries this namespace.
        unsafe { libc::if_nametoindex(c"uq-start".as_ptr()) != 0 }
    };
    // No peer listens: readiness needs the TAP, not the path.
    let text = config("uq-start", 40300, 40301, "::1", "");
    for _ in 0..2 {
        // The second start reattaches the TAP the first left behind.
        assert!(start(&text).success());
        assert!(present(), "the parent returned before the TAP existed");
        let pid: i32 = fs::read_to_string(&pid_path)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // SAFETY: the PID file names the detached daemon this test started.
        assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
        until(|| !pid_path.exists());
        assert!(present(), "shutdown removed the TAP");
    }
    let removed = Command::new(binary())
        .args(["remove-tap", "uq-start"])
        .status()
        .unwrap();
    assert!(removed.success());
    until(|| !present());
    // Storage over memory_limit fails after the fork and before any TAP exists.
    let failing = format!("[daemon]\nmemory_limit = 4096\n{text}");
    assert_eq!(start(&failing).code(), Some(1));
    assert!(!present() && !pid_path.exists());
}
