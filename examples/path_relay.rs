//! Linux-only impaired path for benchmarks. See benches/udp_load.md.
//!
//! Two endpoints each send to this relay instead of to each other. For each
//! direction one thread reads the receive socket and decides every datagram's
//! loss, and another forwards the survivors after a fixed delay. The four
//! threads spin on their own CPUs, so the emulated path costs the endpoints
//! nothing but their own system calls.
use std::{
    io,
    mem::zeroed,
    net::SocketAddrV6,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering::Relaxed},
        mpsc::{Receiver, Sender, TryRecvError, channel},
    },
    time::Instant,
};
use unquic::socket::{
    BATCH_SIZE, MAX_UDP_PAYLOAD, ReceiveDatagram, ReceiveSocket, SendDatagram, TransmitSocket,
};

/// One buffer holds one received message: a datagram or a GRO train.
const BUFFER: usize = 65_536;
/// Messages one direction can hold in its delay line.
const BUFFERS: usize = 2048;
/// Receive sockets per direction. A container caps each socket's buffer at a
/// few trains; the kernel spreads flows over the group, so each flow's bursts
/// get a buffer of their own.
const SOCKETS: usize = 8;
/// A forward later than this after its due time counts as late.
const LATE_NS: u64 = 100_000;

#[derive(Default)]
struct Stats {
    /// Datagrams read, in `messages` socket buffers.
    received: AtomicU64,
    messages: AtomicU64,
    lost: AtomicU64,
    forwarded: AtomicU64,
    /// Datagrams the relay could not forward: truncated, oversized or refused by the kernel.
    refused: AtomicU64,
    /// Kernel buffers the receive socket dropped; a GRO train counts once.
    overflow: AtomicU64,
    /// Turns on which every buffer was in the delay line.
    full: AtomicU64,
    /// Messages forwarded late, and the latest since the last report.
    late: AtomicU64,
    max_late_ns: AtomicU64,
}

/// One received message on its way through the delay line.
struct Message {
    buffer: Vec<u8>,
    length: usize,
    segment: usize,
    /// Bit `n` is set when segment `n` survives.
    keep: u64,
    due: u64,
}

/// xorshift64*: independent loss needs no better generator.
fn lose(seed: &mut u64, permille: u64) -> bool {
    *seed ^= *seed >> 12;
    *seed ^= *seed << 25;
    *seed ^= *seed >> 27;
    ((seed.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 32) * 1000) >> 32 < permille
}

fn retry(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
    ) || error.raw_os_error() == Some(libc::ENOBUFS)
}

fn pin(cpu: usize) -> io::Result<()> {
    // SAFETY: an all-zero CPU set is valid, and CPU_SET checks its index.
    let mut set: libc::cpu_set_t = unsafe { zeroed() };
    unsafe { libc::CPU_SET(cpu, &mut set) };
    // SAFETY: the set is initialized and its size is passed with it.
    if unsafe { libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &set) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Reads one direction's sockets in turn into the delay line until `stop`.
///
/// It does nothing else: the time between two reads of a socket decides how
/// many trains the kernel drops there.
#[allow(clippy::too_many_arguments)]
fn receive(
    rx: &[ReceiveSocket],
    delay_ns: u64,
    permille: u64,
    mut seed: u64,
    start: Instant,
    line: Sender<Message>,
    free: Receiver<Vec<u8>>,
    stop: &AtomicBool,
    stats: &Stats,
) -> io::Result<()> {
    // Untouched pages cost nothing; a buffer's pages are touched as far as its messages reach.
    let mut buffers: Vec<Vec<u8>> = (0..BUFFERS).map(|_| vec![0; BUFFER]).collect();
    let mut overflow_last = vec![None; rx.len()];
    for turn in (0..rx.len()).cycle() {
        if stop.load(Relaxed) {
            break;
        }
        buffers.extend(free.try_iter());
        let count = buffers.len().min(BATCH_SIZE);
        if count == 0 {
            stats.full.fetch_add(1, Relaxed);
            std::hint::spin_loop();
            continue;
        }
        let first = buffers.len() - count;
        let mut storage = buffers[first..].iter_mut();
        let mut batch: [ReceiveDatagram<'_>; BATCH_SIZE] = std::array::from_fn(|_| {
            ReceiveDatagram::new(storage.next().map_or(&mut [][..], |buffer| &mut buffer[..]))
        });
        let received = match rx[turn].recv_batch(&mut batch[..count]) {
            Ok(n) => n,
            Err(error) if retry(&error) => 0,
            Err(error) => return Err(error),
        };
        if received == 0 {
            std::hint::spin_loop();
            continue;
        }
        let due = start.elapsed().as_nanos() as u64 + delay_ns;
        let mut filled = [(0, 0, 0); BATCH_SIZE];
        for (message, filled) in batch[..received].iter().zip(&mut filled) {
            if let Some(value) = message.overflow {
                let last = overflow_last[turn].replace(value).unwrap_or(value);
                stats
                    .overflow
                    .fetch_add(u64::from(value.wrapping_sub(last)), Relaxed);
            }
            let segment = if message.segment_size == 0 {
                message.length.max(1)
            } else {
                message.segment_size
            };
            let segments = message.length.div_ceil(segment).max(1);
            let mut keep = 0_u64;
            if message.truncated || segments > 64 || segment > MAX_UDP_PAYLOAD {
                stats.refused.fetch_add(segments as u64, Relaxed);
            } else {
                for bit in 0..segments {
                    keep |= u64::from(!lose(&mut seed, permille)) << bit;
                }
                stats.messages.fetch_add(1, Relaxed);
                stats.received.fetch_add(segments as u64, Relaxed);
                stats
                    .lost
                    .fetch_add(segments as u64 - u64::from(keep.count_ones()), Relaxed);
            }
            *filled = (message.length, segment, keep);
        }
        // The batch filled the first `received` of the buffers it borrowed, in order.
        // One with nothing to forward takes the same way back to this thread.
        for (buffer, (length, segment, keep)) in buffers.drain(first..first + received).zip(filled)
        {
            let message = Message {
                buffer,
                length,
                segment,
                keep,
                due,
            };
            if line.send(message).is_err() {
                return Ok(());
            }
        }
    }
    Ok(())
}

/// Forwards one direction's delay line until `stop` or until its reader ends.
fn forward(
    tx: &TransmitSocket,
    start: Instant,
    line: Receiver<Message>,
    free: Sender<Vec<u8>>,
    stop: &AtomicBool,
    stats: &Stats,
) {
    let mut next: Option<Message> = None;
    while !stop.load(Relaxed) {
        if next.is_none() {
            next = match line.try_recv() {
                Ok(message) => Some(message),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => return,
            };
        }
        let now = start.elapsed().as_nanos() as u64;
        let Some(message) = next.as_mut().filter(|message| message.due <= now) else {
            std::hint::spin_loop();
            continue;
        };
        if message.keep != 0 {
            let mut parts = [[&[][..]; 1]; 64];
            let mut count = 0;
            let payloads = message.buffer[..message.length].chunks(message.segment);
            for (bit, payload) in payloads.enumerate() {
                if (message.keep >> bit) & 1 != 0 {
                    parts[count] = [payload];
                    count += 1;
                }
            }
            let datagrams: [SendDatagram<'_>; 64] =
                std::array::from_fn(|i| SendDatagram { parts: &parts[i] });
            match tx.send_batch(&datagrams[..count], MAX_UDP_PAYLOAD) {
                Ok(sent) => {
                    stats.forwarded.fetch_add(sent as u64, Relaxed);
                    if sent < count {
                        // Keep the unsent suffix for the next turn.
                        for _ in 0..sent {
                            message.keep &= message.keep - 1;
                        }
                        continue;
                    }
                }
                Err(error) if retry(&error) => continue,
                // For example a peer that has not bound its port yet.
                Err(_) => {
                    stats.refused.fetch_add(count as u64, Relaxed);
                }
            }
            let late = now - message.due;
            stats.late.fetch_add(u64::from(late > LATE_NS), Relaxed);
            stats.max_late_ns.fetch_max(late, Relaxed);
        }
        if let Some(message) = next.take() {
            // The reader may already have stopped.
            let _ = free.send(message.buffer);
        }
    }
}

fn report(name: &str, stats: &Stats) {
    println!(
        "path_relay {name} received={} messages={} lost={} forwarded={} refused={} overflow={} full={} late={} max_late_us={}",
        stats.received.load(Relaxed),
        stats.messages.load(Relaxed),
        stats.lost.load(Relaxed),
        stats.forwarded.load(Relaxed),
        stats.refused.load(Relaxed),
        stats.overflow.load(Relaxed),
        stats.full.load(Relaxed),
        stats.late.load(Relaxed),
        stats.max_late_ns.swap(0, Relaxed) / 1000,
    );
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() != 7 {
        return Err(
            "usage: path_relay DELAY_US LOSS_PERMILLE A_SIDE B_SIDE A_PEER B_PEER CPU,CPU,CPU,CPU"
                .into(),
        );
    }
    let delay_ns = args[0].parse::<u64>()?.saturating_mul(1000);
    let permille: u64 = args[1].parse()?;
    if permille > 1000 {
        return Err("LOSS_PERMILLE is at most 1000".into());
    }
    let group = |address: &String| {
        let address = address.parse::<SocketAddrV6>()?;
        let mut sockets = Vec::new();
        for _ in 0..SOCKETS {
            let mut socket = ReceiveSocket::bind(address, None)?;
            socket.enable_gro()?;
            sockets.push(socket);
        }
        Ok::<_, Box<dyn std::error::Error>>(sockets)
    };
    let (a, b) = (group(&args[2])?, group(&args[3])?);
    // Each direction leaves from the relay's address on the side it goes to.
    let to_a = TransmitSocket::connect(&a[0], args[4].parse()?)?;
    let to_b = TransmitSocket::connect(&b[0], args[5].parse()?)?;
    let cpus = args[6]
        .split(',')
        .map(str::parse)
        .collect::<Result<Vec<usize>, _>>()?;
    if cpus.len() != 4 {
        return Err("four CPUs: A to B reader and forwarder, then B to A".into());
    }
    // Block the signals before the threads start, so only sigwait below sees them.
    // SAFETY: an all-zero signal set is valid storage for sigemptyset to initialize.
    let mut signals: libc::sigset_t = unsafe { zeroed() };
    // SAFETY: the set is initialized before use and the signal numbers are valid.
    unsafe {
        libc::sigemptyset(&mut signals);
        for signal in [libc::SIGUSR1, libc::SIGTERM, libc::SIGINT] {
            libc::sigaddset(&mut signals, signal);
        }
        if libc::pthread_sigmask(libc::SIG_BLOCK, &signals, std::ptr::null_mut()) != 0 {
            return Err(io::Error::last_os_error().into());
        }
    }
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos() as u64;
    let start = Instant::now();
    let stop = AtomicBool::new(false);
    let stats = [Stats::default(), Stats::default()];
    let directions = [("a-b", &a[..], &to_b), ("b-a", &b[..], &to_a)];
    std::thread::scope(|scope| {
        for (index, (name, rx, tx)) in directions.into_iter().enumerate() {
            let (stop, stats, cpus) = (&stop, &stats[index], &cpus[2 * index..]);
            let (line, due) = channel();
            let (free, freed) = channel();
            // Distinct nonzero seeds: the generator stays at zero forever.
            let seed = (seed ^ ((index as u64 + 1) << 32)) | 1;
            scope.spawn(move || {
                if let Err(error) = pin(cpus[0]).and_then(|()| {
                    receive(
                        rx, delay_ns, permille, seed, start, line, freed, stop, stats,
                    )
                }) {
                    eprintln!("path_relay {name}: {error}");
                    std::process::exit(1);
                }
            });
            scope.spawn(move || {
                if let Err(error) = pin(cpus[1]) {
                    eprintln!("path_relay {name}: {error}");
                    std::process::exit(1);
                }
                forward(tx, start, due, free, stop, stats);
            });
        }
        loop {
            let mut signal = 0;
            // SAFETY: the set is initialized and signal is writable.
            if unsafe { libc::sigwait(&signals, &mut signal) } != 0 {
                continue;
            }
            // SIGUSR1 reports and continues; the others report and stop.
            report("a-b", &stats[0]);
            report("b-a", &stats[1]);
            if signal != libc::SIGUSR1 {
                stop.store(true, Relaxed);
                break;
            }
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{net::UdpSocket, time::Duration};

    fn free_port() -> u16 {
        UdpSocket::bind("[::1]:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    fn local(port: u16) -> SocketAddrV6 {
        format!("[::1]:{port}").parse().unwrap()
    }

    #[test]
    fn loss_follows_permille() {
        let mut seed = 1;
        assert!((0..10_000).all(|_| !lose(&mut seed, 0)));
        assert!((0..10_000).all(|_| lose(&mut seed, 1000)));
        let lost = (0..100_000).filter(|_| lose(&mut seed, 10)).count();
        assert!((800..1200).contains(&lost), "{lost}");
    }

    /// Runs one direction on loopback and returns what arrived.
    fn through(delay: Duration, permille: u64, payloads: &[&[u8]]) -> (Vec<Vec<u8>>, Stats) {
        let side = ReceiveSocket::bind(local(free_port()), None).unwrap();
        let far = ReceiveSocket::bind(local(free_port()), None).unwrap();
        let peer = UdpSocket::bind("[::1]:0").unwrap();
        peer.set_read_timeout(Some(Duration::from_millis(300)))
            .unwrap();
        let peer_port = peer.local_addr().unwrap().port();
        let tx = TransmitSocket::connect(&far, local(peer_port)).unwrap();
        let sender = UdpSocket::bind("[::1]:0").unwrap();
        let (stop, stats) = (AtomicBool::new(false), Stats::default());
        let (line, due) = channel();
        let (free, freed) = channel();
        let start = Instant::now();
        let delay_ns = delay.as_nanos() as u64;
        let mut arrived = Vec::new();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let sockets = std::slice::from_ref(&side);
                receive(
                    sockets, delay_ns, permille, 1, start, line, freed, &stop, &stats,
                )
                .unwrap()
            });
            scope.spawn(|| forward(&tx, start, due, free, &stop, &stats));
            let sent = Instant::now();
            for payload in payloads {
                sender.send_to(payload, side.local_addr()).unwrap();
            }
            let mut buffer = [0; 2048];
            while let Ok(n) = peer.recv(&mut buffer) {
                assert!(sent.elapsed() >= delay);
                arrived.push(buffer[..n].to_vec());
            }
            stop.store(true, Relaxed);
        });
        (arrived, stats)
    }

    #[test]
    fn forwards_every_datagram_in_order_after_the_delay() {
        let payloads: [&[u8]; 3] = [&[1; 1200], &[2; 1200], &[3; 300]];
        let (arrived, stats) = through(Duration::from_millis(20), 0, &payloads);
        assert_eq!(arrived, payloads.map(<[u8]>::to_vec));
        assert_eq!(stats.received.load(Relaxed), 3);
        assert_eq!(stats.forwarded.load(Relaxed), 3);
        assert_eq!(stats.lost.load(Relaxed), 0);
        assert_eq!(stats.refused.load(Relaxed), 0);
    }

    #[test]
    fn full_loss_forwards_nothing_and_counts_it() {
        let (arrived, stats) = through(Duration::ZERO, 1000, &[&[7; 100], &[8; 100]]);
        assert!(arrived.is_empty());
        assert_eq!(stats.received.load(Relaxed), 2);
        assert_eq!(stats.lost.load(Relaxed), 2);
        assert_eq!(stats.forwarded.load(Relaxed), 0);
    }

    #[test]
    fn a_datagram_larger_than_the_relay_forwards_is_refused() {
        let (arrived, stats) = through(Duration::ZERO, 0, &[&[9; 9000]]);
        assert!(arrived.is_empty());
        assert_eq!(stats.refused.load(Relaxed), 1);
        assert_eq!(stats.received.load(Relaxed), 0);
    }
}
