//! Foreground UnQuic daemon. Process integration stays outside the library.
#[path = "unquic/config.rs"]
mod config;

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
                        job.peer,
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
                        job.peer,
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

fn serve(loaded: config::Loaded) -> io::Result<()> {
    signals()?;
    let control = Control::new();
    let worker_control = control.clone();
    let transport = thread::Builder::new()
        .name("unquic-run".into())
        .spawn(move || runtime::run(loaded.config, worker_control))?;
    let mut lookups = Some(loaded.lookups);
    let mut helper_error = None;
    while !transport.is_finished() {
        if REPORT.swap(false, Ordering::Relaxed) {
            let mut worker = 0;
            while let Some(snapshot) = control.worker_counters(worker) {
                eprintln!("unquic: worker {worker} {snapshot:?}");
                worker += 1;
            }
        }
        if STOP.load(Ordering::Relaxed) {
            control.shutdown();
            break;
        }
        if control.ready()
            && let Some(jobs) = lookups.take()
        {
            eprintln!("unquic: TAPs ready");
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
    // System DNS has no cancellable timeout; process exit ends any blocked helpers.
    if let Some(error) = helper_error {
        Err(error)
    } else {
        result
    }
}

fn execute() -> io::Result<()> {
    let mut args = std::env::args_os().skip(1);
    match (args.next(), args.next(), args.next()) {
        (Some(command), None, None) if command == "gen-uuid" => {
            writeln!(io::stdout().lock(), "{}", UnquicUuid::new_v4()?)
        }
        (Some(command), Some(path), None) if command == "run" => {
            serve(config::Loaded::parse(&std::fs::read_to_string(path)?)?)
        }
        (Some(command), None, None) if command == "--help" || command == "-h" => writeln!(
            io::stdout().lock(),
            "Usage: unquic gen-uuid | unquic run <config.toml>"
        ),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: unquic gen-uuid | unquic run <config.toml>",
        )),
    }
}

fn main() -> ExitCode {
    match execute() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("unquic: {error}");
            ExitCode::FAILURE
        }
    }
}
