//! Samples where a running process spends its CPU time, for hosts where
//! `perf` is not allowed. Diagnostics only.
//!
//! Usage: `stack_sample PID SECONDS [INTERVAL_US]` as the process's own user
//! with ptrace permission (root in its container, or its parent). Every
//! interval it looks at each thread: one that is not running is counted as
//! asleep and left alone, and a running one is stopped for an instant and its
//! program counter read. A line per sample goes to stdout:
//!
//! `THREAD u OFFSET [TOP]` for user code, OFFSET being the program counter
//! less the executable's load address, ready for `addr2line -f -i -e BINARY`,
//! and TOP the word on top of the stack when it points into the executable:
//! the caller, if the sampled function keeps no frame (musl's `memcpy` and
//! `memset`), and noise otherwise. `THREAD k SYSCALL` is a thread inside the
//! kernel on that system call. The last lines give each thread's CPU time
//! over the run (`# thread THREAD ran NANOSECONDS ns`) and count the samples
//! and the sleeping looks.
//!
//! A thread that sleeps often is sampled less than its CPU time warrants,
//! because a look that meets it falling asleep is thrown away. Compare shares
//! within one thread, and weigh each thread's samples by its CPU time.
//!
//! Build the sampled binary with line tables and unstripped
//! (`CARGO_PROFILE_RELEASE_DEBUG=line-tables-only CARGO_PROFILE_RELEASE_STRIP=none`).
//! Each sample stops a thread for some tens of microseconds, so keep the
//! interval at a millisecond or more.
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
fn main() {
    eprintln!("stack_sample needs Linux on x86_64");
    std::process::exit(2);
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn main() -> std::io::Result<()> {
    use std::{
        fs,
        io::{Error, Write},
        time::{Duration, Instant},
    };
    let mut args = std::env::args().skip(1);
    let mut number = || args.next().and_then(|text| text.parse::<u64>().ok());
    let (Some(pid), Some(seconds)) = (number(), number()) else {
        eprintln!("usage: stack_sample PID SECONDS [INTERVAL_US]");
        std::process::exit(2);
    };
    let interval = Duration::from_micros(number().unwrap_or(2000).max(100));
    // The executable's first mapping starts at its load address.
    let exe = fs::read_link(format!("/proc/{pid}/exe"))?;
    let maps = fs::read_to_string(format!("/proc/{pid}/maps"))?;
    let base = maps
        .lines()
        .find(|line| line.ends_with(&*exe.to_string_lossy()))
        .and_then(|line| u64::from_str_radix(line.split('-').next()?, 16).ok())
        .ok_or_else(|| Error::other("no mapping of the executable"))?;
    let end = maps
        .lines()
        .filter(|line| line.ends_with(&*exe.to_string_lossy()))
        .filter_map(|line| u64::from_str_radix(line.split(['-', ' ']).nth(1)?, 16).ok())
        .max()
        .unwrap_or(u64::MAX);
    let mut threads = Vec::new();
    for entry in fs::read_dir(format!("/proc/{pid}/task"))? {
        let Some(tid) = entry?.file_name().to_str().and_then(|n| n.parse().ok()) else {
            continue;
        };
        let tid: libc::pid_t = tid;
        // SAFETY: PTRACE_SEIZE takes no memory from this process.
        if unsafe { libc::ptrace(libc::PTRACE_SEIZE, tid, 0, 0) } < 0 {
            return Err(Error::last_os_error());
        }
        threads.push(tid);
    }
    let running = |tid: libc::pid_t| {
        // The state follows the command name, which may hold spaces.
        fs::read_to_string(format!("/proc/{pid}/task/{tid}/stat"))
            .ok()
            .and_then(|stat| stat.rsplit(") ").next()?.chars().next())
            == Some('R')
    };
    // How often the thread has been given a CPU. A thread that went to sleep
    // after it was seen running is woken by the interrupt, and this count
    // then differs: its sample would say it ran inside the call it slept in.
    let schedstat = |tid: libc::pid_t, field: usize| {
        fs::read_to_string(format!("/proc/{pid}/task/{tid}/schedstat"))
            .ok()
            .and_then(|text| text.split_whitespace().nth(field)?.parse::<u64>().ok())
    };
    let turns = |tid: libc::pid_t| schedstat(tid, 2);
    // Nanoseconds each thread has run, to weigh its samples by.
    let ran: Vec<u64> = threads
        .iter()
        .map(|&tid| schedstat(tid, 0).unwrap_or(0))
        .collect();
    // Waits for `tid` to stop. A stop that is a signal's delivery, not the
    // interrupt's, hands the signal on and waits again.
    let stopped = |tid: libc::pid_t| -> std::io::Result<bool> {
        loop {
            let mut status = 0;
            // SAFETY: status is a valid place for the result.
            if unsafe { libc::waitpid(tid, &mut status, libc::__WALL) } < 0 {
                return Err(Error::last_os_error());
            }
            if !libc::WIFSTOPPED(status) {
                return Ok(false);
            }
            if status >> 16 == libc::PTRACE_EVENT_STOP {
                return Ok(true);
            }
            // SAFETY: PTRACE_CONT takes the signal to deliver as a number.
            unsafe { libc::ptrace(libc::PTRACE_CONT, tid, 0, libc::WSTOPSIG(status)) };
        }
    };
    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    let (mut samples, mut asleep) = (0_u64, 0_u64);
    let started = Instant::now();
    let mut next = started;
    while started.elapsed() < Duration::from_secs(seconds) {
        for &tid in &threads {
            let before = turns(tid);
            if !running(tid) {
                asleep += 1;
                continue;
            }
            // SAFETY: PTRACE_INTERRUPT takes no memory from this process.
            if unsafe { libc::ptrace(libc::PTRACE_INTERRUPT, tid, 0, 0) } < 0 || !stopped(tid)? {
                continue;
            }
            // Read while the thread is stopped: resuming it is another turn.
            let after = turns(tid);
            // SAFETY: a zeroed register set is valid, and the kernel fills it.
            let mut registers: libc::user_regs_struct = unsafe { std::mem::zeroed() };
            // SAFETY: registers is a valid user_regs_struct for the stopped thread.
            let read = unsafe { libc::ptrace(libc::PTRACE_GETREGS, tid, 0, &mut registers) };
            // The word on top of the stack: the return address while a
            // function that keeps no frame runs, as musl's memcpy and memset
            // do, and anything else otherwise. A failed read gives -1.
            // SAFETY: PTRACE_PEEKDATA reads a word of the stopped thread's memory.
            let top = unsafe { libc::ptrace(libc::PTRACE_PEEKDATA, tid, registers.rsp, 0) } as u64;
            // SAFETY: PTRACE_CONT with signal zero resumes the thread.
            unsafe { libc::ptrace(libc::PTRACE_CONT, tid, 0, 0) };
            if read < 0 {
                continue;
            }
            if before.is_none() || after != before {
                asleep += 1;
                continue;
            }
            samples += 1;
            // orig_rax holds the system call number while inside one.
            if registers.orig_rax as i64 >= 0 {
                writeln!(out, "{tid} k {}", registers.orig_rax)?;
            } else if (base..end).contains(&registers.rip) {
                // Zero-padded as addr2line -a prints addresses.
                write!(out, "{tid} u {:#018x}", registers.rip - base)?;
                if (base..end).contains(&top) {
                    write!(out, " {:#018x}", top - base)?;
                }
                writeln!(out)?;
            } else {
                // The vDSO, mostly: clock reads.
                writeln!(out, "{tid} u other")?;
            }
        }
        next += interval;
        std::thread::sleep(next.saturating_duration_since(Instant::now()));
    }
    for (&tid, &before) in threads.iter().zip(&ran) {
        let ran = schedstat(tid, 0).unwrap_or(before).saturating_sub(before);
        writeln!(out, "# thread {tid} ran {ran} ns")?;
    }
    writeln!(out, "# samples {samples} asleep {asleep}")?;
    out.flush()
}
