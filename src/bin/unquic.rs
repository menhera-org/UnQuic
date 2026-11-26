//! UnQuic daemon, foreground or double-forked. Process integration stays
//! outside the library.
#[path = "unquic/config.rs"]
mod config;
#[path = "unquic/daemon.rs"]
mod daemon;
#[path = "unquic/restart.rs"]
mod restart;

use std::{
    io::{self, Write},
    process::ExitCode,
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};
use unquic::{
    UnquicUuid,
    dns::{IpHostResolver, resolve_ipv6},
    runtime::{self, Control},
};

static REPORT: AtomicBool = AtomicBool::new(false);

extern "C" fn report(_: libc::c_int) {
    REPORT.store(true, Ordering::Relaxed);
}

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn stop(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

static RESTART: AtomicBool = AtomicBool::new(false);

extern "C" fn restart_signal(_: libc::c_int) {
    RESTART.store(true, Ordering::Relaxed);
}

fn signals() -> io::Result<()> {
    // SAFETY: zero initializes the platform sigaction, then its handler/mask are set.
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = stop as *const () as usize;
    // SAFETY: action has writable storage for its mask and remains alive for both calls.
    unsafe {
        if libc::sigemptyset(&mut action.sa_mask) < 0
            || libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut()) < 0
            || libc::sigaction(libc::SIGTERM, &action, std::ptr::null_mut()) < 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    action.sa_sigaction = report as *const () as usize;
    // SAFETY: initialized handler and mask remain alive for this call.
    if unsafe { libc::sigaction(libc::SIGUSR1, &action, std::ptr::null_mut()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    action.sa_sigaction = restart_signal as *const () as usize;
    // DECISION(2026-11-27): SIGHUP reloads by the same re-exec as SIGUSR2. Why: the successor
    //   reads the file anew and adopts the TAPs of unchanged sessions. Rejected: changing
    //   sessions inside running workers (their per-session state is fixed at start).
    for signal in [libc::SIGUSR2, libc::SIGHUP] {
        // SAFETY: as above.
        if unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn resolve(job: config::Lookup, control: Control) {
    let mut resolver = None;
    let mut submitted = None;
    let mut pending = None;
    let mut due = Instant::now();
    while !STOP.load(Ordering::Relaxed) && !control.stopped() {
        if Instant::now() >= due {
            if resolver.is_none() {
                match job.resolver.build() {
                    Ok(selected) => {
                        resolver =
                            Some(IpHostResolver::new(job.host.clone()).with_resolver(selected))
                    }
                    Err(error) => eprintln!(
                        "unquic: peer {} resolver unavailable: {}",
                        job.name,
                        error.kind()
                    ),
                }
            }
            if let Some(resolver) = &resolver
                && let Some(address) = resolve_ipv6(resolver)
            {
                pending = (submitted != Some(address)).then_some(address);
            }
            due = Instant::now() + job.refresh;
        }
        if let Some(address) = pending {
            match control.supply_peer_address(job.peer, address, job.scope_id) {
                Ok(()) => {
                    submitted = Some(address);
                    pending = None;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) if error.kind() == io::ErrorKind::BrokenPipe => break,
                Err(error) => {
                    eprintln!(
                        "unquic: peer {} address rejected: {}",
                        job.name,
                        error.kind()
                    );
                    pending = None;
                }
            }
        }
        let interval = if pending.is_some() {
            Duration::from_millis(100)
        } else {
            Duration::from_secs(1)
        };
        thread::park_timeout(due.saturating_duration_since(Instant::now()).min(interval));
    }
}

/// Diagnostics-only writer for `UNQUIC_TRACE_DIR/<first session>.trace`.
#[cfg(feature = "trace")]
struct Trace {
    file: io::BufWriter<std::fs::File>,
    cursors: Vec<u64>,
    records: Vec<unquic::trace::Record>,
    marks: u64,
    lost: u64,
}

#[cfg(feature = "trace")]
impl Trace {
    fn open(name: &str) -> Option<Self> {
        let path = std::path::Path::new(&std::env::var_os("UNQUIC_TRACE_DIR")?)
            .join(format!("{name}.trace"));
        let file = std::fs::File::create(&path)
            .map_err(|error| eprintln!("unquic: trace {}: {error}", path.display()))
            .ok()?;
        let mut trace = Self {
            file: io::BufWriter::new(file),
            cursors: Vec::new(),
            records: Vec::new(),
            marks: 0,
            lost: 0,
        };
        trace.mark(false);
        Some(trace)
    }

    /// Copies every ring; records are little-endian u64 words in drain order.
    fn drain(&mut self) {
        self.cursors.resize(unquic::trace::workers(), 0);
        for (worker, cursor) in self.cursors.iter_mut().enumerate() {
            self.lost += unquic::trace::drain(worker, cursor, &mut self.records);
        }
        for record in self.records.drain(..) {
            for word in record {
                if let Err(error) = self.file.write_all(&word.to_le_bytes()) {
                    eprintln!("unquic: trace write: {error}");
                    return;
                }
            }
        }
    }

    /// Markers 1.. follow each SIGUSR1 counter report; 0 opens and closes the file.
    fn mark(&mut self, report: bool) {
        self.drain();
        let sequence = if report {
            self.marks += 1;
            self.marks
        } else {
            0
        };
        let record = [
            unquic::trace::MARK | 0xff << 8 | sequence << 16,
            unquic::trace::now().as_nanos() as u64,
            unquic::trace::boottime().as_nanos() as u64,
            self.lost,
            0,
            0,
            0,
            0,
        ];
        for word in record {
            let _ = self.file.write_all(&word.to_le_bytes());
        }
        let _ = self.file.flush();
    }
}

/// How serve starts: readiness pipe, PID file and adopted TAPs, if any.
struct Launch {
    readiness: Option<daemon::Readiness>,
    pid_path: Option<std::path::PathBuf>,
    adopted: Vec<Option<unquic::tap::Tap>>,
    // Arguments for a re-exec, as this process received them.
    args: Vec<std::ffi::OsString>,
}

fn sessions(config: &unquic::config::Config) -> Vec<restart::Session<'_>> {
    config
        .sessions
        .iter()
        .map(|session| restart::Session {
            name: &session.name,
            queues: session.queue_count,
            mtu: session.inner_mtu,
        })
        .collect()
}

/// Turns queues kept for a failed re-exec back into each session's TAP.
fn adopt_back(
    sessions: &[restart::Session<'_>],
    queues: Vec<(usize, usize, std::os::fd::OwnedFd)>,
) -> io::Result<Vec<Option<unquic::tap::Tap>>> {
    let mut grouped: Vec<Vec<std::os::fd::OwnedFd>> = sessions.iter().map(|_| Vec::new()).collect();
    for (session, _, fd) in queues {
        if let Some(group) = grouped.get_mut(session) {
            group.push(fd);
        }
    }
    sessions
        .iter()
        .zip(grouped)
        .map(|(session, fds)| {
            if fds.is_empty() {
                Ok(None)
            } else {
                unquic::tap::Tap::adopt(fds, session.mtu).map(Some)
            }
        })
        .collect()
}

/// This build's version, the running kernel's release and how many CPUs the
/// daemon may use: what a reader of a report needs before its numbers.
fn host_line() -> String {
    // SAFETY: zeroed utsname and cpu_set_t are valid output storage; uname
    // NUL-terminates the release it writes.
    let (release, cpus) = unsafe {
        let mut name: libc::utsname = std::mem::zeroed();
        let release = if libc::uname(&mut name) == 0 {
            std::ffi::CStr::from_ptr(name.release.as_ptr())
                .to_string_lossy()
                .into_owned()
        } else {
            String::from("unknown")
        };
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        let cpus = if libc::sched_getaffinity(0, size_of::<libc::cpu_set_t>(), &mut set) == 0 {
            libc::CPU_COUNT(&set)
        } else {
            0
        };
        (release, cpus)
    };
    format!(
        "unquic: version {} kernel {release} allowed_cpus {cpus}\n",
        env!("CARGO_PKG_VERSION")
    )
}

/// The SIGUSR1 report: the host line, each worker's totals, each session's
/// state and totals with those of its lanes, then each peer's totals.
/// Sessions and peers go by their configured names, the public labels; no
/// cookie or address appears.
fn report_text(config: &unquic::config::Config, control: &Control) -> String {
    use std::fmt::Write;
    let mut text = host_line();
    let mut worker = 0;
    while let Some(totals) = control.worker_counters(worker) {
        let _ = writeln!(text, "unquic: worker {worker} {totals:?}");
        worker += 1;
    }
    for (index, session) in config.sessions.iter().enumerate() {
        let name = &session.name;
        if let (Some(status), Some(totals)) =
            (control.session(index), control.session_counters(index))
        {
            let _ = writeln!(text, "unquic: session {name} {status:?} {totals:?}");
        }
        for lane in 1..=64 {
            if let Some(lane) = control.transmit_lane_counters(index, lane) {
                let _ = writeln!(text, "unquic: session {name} transmit {lane:?}");
            }
            if let Some(lane) = control.receive_lane_counters(index, lane) {
                let _ = writeln!(text, "unquic: session {name} receive {lane:?}");
            }
        }
    }
    for (index, peer) in config.peers.iter().enumerate() {
        if let Some(totals) = control.peer_counters(index) {
            let _ = writeln!(text, "unquic: peer {} {totals:?}", peer.name);
        }
    }
    text
}

fn serve(loaded: config::Loaded, mut launch: Launch) -> io::Result<()> {
    signals()?;
    let binary = std::env::current_exe()?;
    let mut _pid_file = None;
    #[cfg(feature = "trace")]
    let mut trace = loaded
        .config
        .sessions
        .first()
        .and_then(|session| Trace::open(&session.name));
    let mut adopted = std::mem::take(&mut launch.adopted);
    loop {
        let control = Control::new();
        let transport = {
            let control = control.clone();
            let config = loaded.config.clone();
            let adopted = std::mem::take(&mut adopted);
            thread::Builder::new()
                .name("unquic-run".into())
                .spawn(move || runtime::run_adopting(config, control, adopted))?
        };
        let mut lookups = Some(loaded.lookups.clone());
        let mut helper_error = None;
        let mut restarting = false;
        while !transport.is_finished() {
            if REPORT.swap(false, Ordering::Relaxed) {
                let text = report_text(&loaded.config, &control);
                eprint!("{text}");
                // `unquic status` reads the same report from beside the PID file.
                if let Some(path) = &launch.pid_path
                    && let Err(error) = daemon::publish(&daemon::status_path(path), &text)
                {
                    eprintln!("unquic: status file: {error}");
                }
                #[cfg(feature = "trace")]
                if let Some(trace) = &mut trace {
                    trace.mark(true);
                }
            }
            #[cfg(feature = "trace")]
            if let Some(trace) = &mut trace {
                trace.drain();
            }
            if STOP.load(Ordering::Relaxed) {
                control.shutdown();
                break;
            }
            if RESTART.swap(false, Ordering::Relaxed) {
                // A successor that cannot adopt these queues, or rejects the
                // configuration file as it is now, never stops this daemon.
                let probed = match launch.args.get(1) {
                    Some(config) => restart::probe(&binary, config),
                    None => Err(io::ErrorKind::InvalidInput.into()),
                };
                match probed {
                    Ok(()) => {
                        restarting = true;
                        control.handover();
                        break;
                    }
                    Err(error) => eprintln!("unquic: restart refused: {error}"),
                }
            }
            if control.ready()
                && let Some(jobs) = lookups.take()
            {
                eprintln!("unquic: TAPs ready");
                if _pid_file.is_none() {
                    let published = match launch.readiness.take() {
                        Some(readiness) => readiness.ready().map(Some),
                        None => launch
                            .pid_path
                            .as_deref()
                            .map(daemon::PidFile::publish)
                            .transpose(),
                    };
                    match published {
                        Ok(pid_file) => _pid_file = pid_file,
                        Err(error) => {
                            helper_error = Some(error);
                            control.shutdown();
                            break;
                        }
                    }
                }
                for job in jobs {
                    let control = control.clone();
                    if let Err(error) = thread::Builder::new()
                        .name("unquic-dns".into())
                        .spawn(move || resolve(job, control))
                    {
                        helper_error = Some(error);
                        break;
                    }
                }
                if helper_error.is_some() {
                    control.shutdown();
                    break;
                }
            }
            thread::park_timeout(Duration::from_millis(50));
        }
        let result = transport
            .join()
            .unwrap_or_else(|_| Err(io::ErrorKind::Other.into()));
        if restarting && result.is_ok() && helper_error.is_none() {
            let sessions = sessions(&loaded.config);
            let queues = control.take_handed_queues();
            let error = restart::exec(
                &binary,
                &launch.args,
                &sessions,
                &queues,
                launch.pid_path.as_deref(),
            );
            eprintln!("unquic: re-exec failed; continuing on the same TAP queues: {error}");
            adopted = adopt_back(&sessions, queues)?;
            continue;
        }
        #[cfg(feature = "trace")]
        if let Some(trace) = &mut trace {
            trace.mark(false);
        }
        // System DNS has no cancellable timeout; process exit ends any blocked helpers.
        return if let Some(error) = helper_error {
            Err(error)
        } else {
            result
        };
    }
}

const USAGE: &str = "unquic gen-uuid | unquic check-config <config.toml> | \
unquic run <config.toml> | \
unquic start <config.toml> <pid-file> | unquic restart <pid-file> | \
unquic status <pid-file> | \
unquic systemd-unit <config.toml> <pid-file> | unquic remove-tap <name> | unquic adopt-version";

fn absolute(path: std::ffi::OsString) -> io::Result<std::path::PathBuf> {
    let path = std::path::PathBuf::from(path);
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "PID and daemon configuration files need absolute paths",
        ))
    }
}

/// Returns the process exit status.
fn execute() -> io::Result<u8> {
    let original: Vec<_> = std::env::args_os().skip(1).collect();
    let mut args = original.clone().into_iter();
    match (args.next(), args.next(), args.next(), args.next()) {
        (Some(command), None, None, None) if command == "gen-uuid" => {
            writeln!(io::stdout().lock(), "{}", UnquicUuid::new_v4()?)?;
        }
        (Some(command), None, None, None) if command == "adopt-version" => {
            io::stdout().lock().write_all(restart::VERSION.as_bytes())?;
        }
        (Some(command), Some(pid_file), None, None) if command == "restart" => {
            restart::signal(&absolute(pid_file)?, libc::SIGUSR2)?;
        }
        (Some(command), Some(pid_file), None, None) if command == "status" => {
            let text = restart::status(&absolute(pid_file)?)?;
            io::stdout().lock().write_all(text.as_bytes())?;
        }
        (Some(command), Some(path), None, None) if command == "check-config" => {
            runtime::check(config::Loaded::parse(&std::fs::read_to_string(path)?)?.config)?;
        }
        (Some(command), Some(path), None, None) if command == "run" => {
            let loaded = config::Loaded::parse(&std::fs::read_to_string(path)?)?;
            let inherited = restart::entry(&sessions(&loaded.config))?;
            let (adopted, pid_path) =
                inherited.map_or((Vec::new(), None), |i| (i.taps, i.pid_file));
            let launch = Launch {
                readiness: None,
                pid_path,
                adopted,
                args: original,
            };
            serve(loaded, launch)?;
        }
        (Some(command), Some(path), Some(pid_file), None) if command == "start" => {
            let path = absolute(path)?;
            let pid_file = absolute(pid_file)?;
            let loaded = config::Loaded::parse(&std::fs::read_to_string(&path)?)?;
            // Report configuration errors from the launching process, before forking.
            loaded.config.clone().validate().map_err(io::Error::other)?;
            // A re-executed daemon is already detached and adopts its queues.
            let launch = match restart::entry(&sessions(&loaded.config))? {
                Some(inherited) => Launch {
                    readiness: None,
                    pid_path: inherited.pid_file.or(Some(pid_file)),
                    adopted: inherited.taps,
                    args: original,
                },
                None => match daemon::start(pid_file.clone())? {
                    daemon::Started::Parent(status) => return Ok(status),
                    daemon::Started::Daemon(readiness) => Launch {
                        readiness: Some(readiness),
                        pid_path: Some(pid_file),
                        adopted: Vec::new(),
                        args: original,
                    },
                },
            };
            serve(loaded, launch)?;
        }
        (Some(command), Some(name), None, None) if command == "remove-tap" => {
            use std::os::unix::ffi::OsStringExt;
            let name = std::ffi::CString::new(name.into_vec())
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in TAP name"))?;
            unquic::tap::remove(&name)?;
        }
        (Some(command), Some(path), Some(pid_file), None) if command == "systemd-unit" => {
            let binary = std::env::current_exe()?;
            let text = daemon::unit(&binary, &absolute(path)?, &absolute(pid_file)?)?;
            io::stdout().lock().write_all(text.as_bytes())?;
        }
        (Some(command), None, None, None) if command == "--help" || command == "-h" => {
            writeln!(io::stdout().lock(), "Usage: {USAGE}")?;
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("usage: {USAGE}"),
            ));
        }
    }
    Ok(0)
}

fn main() -> ExitCode {
    match execute() {
        Ok(status) => ExitCode::from(status),
        Err(error) => {
            eprintln!("unquic: {error}");
            ExitCode::FAILURE
        }
    }
}
