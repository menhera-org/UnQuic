//! UnQuic daemon, foreground or double-forked. Process integration stays
//! outside the library.
#[path = "unquic/config.rs"]
mod config;
#[path = "unquic/daemon.rs"]
mod daemon;
#[path = "unquic/host.rs"]
mod host;
#[path = "unquic/notify.rs"]
mod notify;
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

/// Each peer's current underlay address and scope, for the report's device lines.
type Routes = std::sync::Arc<std::sync::Mutex<Vec<Option<(std::net::Ipv6Addr, u32)>>>>;

fn resolve(job: config::Lookup, control: Control, routes: Routes) {
    let mut resolver = None;
    let mut submitted = None;
    let mut pending = None;
    let mut silent = false;
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
            if let Some(resolver) = &resolver {
                match resolve_ipv6(resolver) {
                    Some(address) => {
                        pending = (submitted != Some(address)).then_some(address);
                        silent = false;
                    }
                    // Said once per run of failures: a mode that finds no
                    // server on its link otherwise fails without a word.
                    None if !silent => {
                        eprintln!("unquic: peer {} lookup found no address", job.name);
                        silent = true;
                    }
                    None => {}
                }
            }
            due = Instant::now() + job.refresh;
        }
        if let Some(address) = pending {
            match control.supply_peer_address(job.peer, address, job.scope_id) {
                Ok(()) => {
                    submitted = Some(address);
                    pending = None;
                    if let Ok(mut routes) = routes.lock()
                        && let Some(route) = routes.get_mut(job.peer)
                    {
                        *route = Some((address, job.scope_id));
                    }
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

/// One line for each underlay device the configuration names or a peer's
/// address routes through, with what uses it.
fn device_lines(
    config: &unquic::config::Config,
    routes: &[Option<(std::net::Ipv6Addr, u32)>],
) -> String {
    use std::fmt::Write;
    let mut devices: Vec<(String, Vec<String>)> = Vec::new();
    let mut note = |device: String, label: String| match devices
        .iter_mut()
        .find(|(name, _)| *name == device)
    {
        Some((_, labels)) => labels.push(label),
        None => devices.push((device, vec![label])),
    };
    for endpoint in &config.endpoints {
        if let Some(device) = endpoint.bind_device.as_ref().and_then(|d| d.to_str().ok()) {
            note(device.to_owned(), format!("endpoint {}", endpoint.name));
        }
    }
    for (peer, route) in config.peers.iter().zip(routes) {
        // A bound endpoint's device is already listed, and a lookup made
        // without that binding could name another.
        let bound = config
            .endpoints
            .iter()
            .any(|endpoint| endpoint.name == peer.endpoint && endpoint.bind_device.is_some());
        if !bound
            && let Some((address, scope)) = route
            && let Some(device) = host::route_device(*address, *scope)
        {
            note(device, format!("peer {}", peer.name));
        }
    }
    let mut text = String::new();
    for (device, labels) in devices {
        let _ = writeln!(
            text,
            "unquic: device {device} ({}) {}",
            labels.join(", "),
            host::device(&device)
        );
    }
    text
}

/// The SIGUSR1 report: the host's lines and devices, each worker's totals,
/// each session's state and totals with those of its lanes, then each
/// peer's totals. Sessions and peers go by their configured names, the
/// public labels; no cookie or address appears.
fn report_text(config: &unquic::config::Config, control: &Control, routes: &Routes) -> String {
    use std::fmt::Write;
    let mut text = host_line();
    text.push_str(&host::lines());
    if let Ok(routes) = routes.lock() {
        text.push_str(&device_lines(config, &routes));
    }
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

/// Has a service manager's descriptor store forget the queues this process
/// did not adopt, and waits until it has: a device with a stored queue still
/// attached refuses a new attachment by name.
fn forget(dropped: &[String]) {
    if dropped.is_empty() {
        return;
    }
    let notifier = notify::Notifier::from_env();
    for name in dropped {
        notifier.forget(name);
    }
    notifier.barrier();
}

fn serve(loaded: config::Loaded, mut launch: Launch) -> io::Result<()> {
    signals()?;
    let binary = std::env::current_exe()?;
    let notifier = notify::Notifier::from_env();
    let mut _pid_file = None;
    #[cfg(feature = "trace")]
    let mut trace = loaded
        .config
        .sessions
        .first()
        .and_then(|session| Trace::open(&session.name));
    let mut adopted = std::mem::take(&mut launch.adopted);
    let routes: Routes = std::sync::Arc::new(std::sync::Mutex::new(
        loaded
            .config
            .peers
            .iter()
            .map(|peer| peer.address.map(|address| (address, peer.scope_id)))
            .collect(),
    ));
    loop {
        let control = Control::new();
        if notifier.active() {
            control.keep_queue_copies();
        }
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
                let text = report_text(&loaded.config, &control, &routes);
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
                notifier.send("STOPPING=1");
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
                // DECISION(2026-11-27): under a service manager every TAP queue goes into its
                //   descriptor store. Why: the queues then stay attached across a crash and a
                //   manager's restart, and the next start adopts them, so the links above the
                //   TAPs see no queue close. Rejected: storing nothing (each such restart
                //   detaches every queue until the daemon is back).
                if notifier.active() {
                    use std::os::fd::AsFd;
                    let sessions = sessions(&loaded.config);
                    for (session, queue, fd) in control.take_queue_copies() {
                        if let Some(name) = restart::queue_name(&sessions, session, queue) {
                            notifier.store(&name, fd.as_fd());
                        }
                    }
                    notifier.send("READY=1\nSTATUS=TAPs ready");
                }
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
                // The host's facts go to the log once per start, after readiness
                // and never before it; the report repeats them on request.
                eprint!("{}{}", host_line(), host::lines());
                if let Ok(routes) = routes.lock() {
                    eprint!("{}", device_lines(&loaded.config, &routes));
                }
                for job in jobs {
                    let control = control.clone();
                    let routes = routes.clone();
                    if let Err(error) = thread::Builder::new()
                        .name("unquic-dns".into())
                        .spawn(move || resolve(job, control, routes))
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
unquic systemd-unit <config.toml> <pid-file> [<installed binary>] | \
unquic remove-tap <name> | unquic adopt-version";

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
            let (adopted, pid_path) = inherited.map_or((Vec::new(), None), |i| {
                forget(&i.dropped);
                (i.taps, i.pid_file)
            });
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
            let inherited = restart::entry(&sessions(&loaded.config))?;
            if let Some(inherited) = &inherited {
                forget(&inherited.dropped);
            }
            let launch = match inherited {
                // A re-executed daemon is already detached and adopts its queues.
                Some(inherited) if inherited.reexec => Launch {
                    readiness: None,
                    pid_path: inherited.pid_file.or(Some(pid_file)),
                    adopted: inherited.taps,
                    args: original,
                },
                stored => {
                    // Queues a service manager stored cross the forks with the daemon;
                    // the parent's own descriptors for them close when it returns.
                    let adopted = stored.map_or_else(Vec::new, |inherited| inherited.taps);
                    match daemon::start(pid_file.clone())? {
                        daemon::Started::Parent(status) => return Ok(status),
                        daemon::Started::Daemon(readiness) => Launch {
                            readiness: Some(readiness),
                            pid_path: Some(pid_file),
                            adopted,
                            args: original,
                        },
                    }
                }
            };
            serve(loaded, launch)?;
        }
        (Some(command), Some(name), None, None) if command == "remove-tap" => {
            use std::os::unix::ffi::OsStringExt;
            let name = std::ffi::CString::new(name.into_vec())
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in TAP name"))?;
            unquic::tap::remove(&name)?;
        }
        (Some(command), Some(path), Some(pid_file), installed)
            if command == "systemd-unit" && args.next().is_none() =>
        {
            // A package names where the binary will be installed; otherwise it is this one.
            let binary = match installed {
                Some(path) => absolute(path)?,
                None => std::env::current_exe()?,
            };
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
