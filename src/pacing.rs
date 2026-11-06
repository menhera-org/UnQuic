//! Owner-local control traffic service; data grants and BBR pacing are separate.
use std::{io, time::Duration};

const RATE: u64 = 4 * 1024 * 1024;
const BURST: usize = 16;

/// A single path group's successful control transmissions, owned by worker zero.
pub(crate) struct Control {
    finishes: [Duration; BURST],
    head: usize,
    len: usize,
}

impl Control {
    pub const fn new() -> Self {
        Self {
            finishes: [Duration::ZERO; BURST],
            head: 0,
            len: 0,
        }
    }

    /// Earliest slot release when the literal sixteen-packet burst is full.
    pub fn deadline(&self) -> Option<Duration> {
        (self.len == BURST).then(|| self.finishes[self.head])
    }

    /// Runs one nonblocking send if budget permits; failures consume no service.
    pub fn send(
        &mut self,
        outer_bytes: usize,
        now: Duration,
        send: impl FnOnce() -> io::Result<()>,
    ) -> io::Result<()> {
        if !(100..=1280).contains(&outer_bytes) {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        while self.len != 0 && self.finishes[self.head] <= now {
            self.head = (self.head + 1) % BURST;
            self.len -= 1;
        }
        if self.len == BURST {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        let tail = (self.head + self.len) % BURST;
        let start = if self.len == 0 {
            now
        } else {
            self.finishes[(tail + BURST - 1) % BURST].max(now)
        };
        let service = Duration::from_nanos((outer_bytes as u64 * 1_000_000_000).div_ceil(RATE));
        let finish = start
            .checked_add(service)
            .ok_or(io::ErrorKind::WouldBlock)?;
        send()?;
        self.finishes[tail] = finish;
        self.len += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_packet_burst_and_actual_byte_service_survive_wrap_and_idle() {
        let mut control = Control::new();
        let now = Duration::from_secs(1);
        let mut finish = now;
        let mut expected = std::collections::VecDeque::new();
        for bytes in (100..=115).chain([1280; 128]) {
            let at = if expected.len() == BURST {
                let next = expected.pop_front().unwrap();
                assert_eq!(control.deadline(), Some(next));
                assert_eq!(
                    control
                        .send(bytes, next - Duration::from_nanos(1), || panic!(
                            "early send"
                        ))
                        .unwrap_err()
                        .kind(),
                    io::ErrorKind::WouldBlock
                );
                next
            } else {
                now
            };
            control.send(bytes, at, || Ok(())).unwrap();
            finish += Duration::from_nanos((bytes as u64 * 1_000_000_000).div_ceil(RATE));
            expected.push_back(finish);
        }
        let idle = finish + Duration::from_secs(1);
        for _ in 0..BURST {
            control.send(100, idle, || Ok(())).unwrap();
        }
        assert_eq!(
            control.deadline(),
            Some(idle + Duration::from_nanos(23_842))
        );
    }

    #[test]
    fn failures_invalid_lengths_and_unrepresentable_time_consume_nothing() {
        let mut control = Control::new();
        for size in [0, 99, 1281, usize::MAX] {
            assert_eq!(
                control
                    .send(size, Duration::ZERO, || panic!("invalid send"))
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidInput
            );
        }
        for kind in [
            io::ErrorKind::WouldBlock,
            io::ErrorKind::Interrupted,
            io::ErrorKind::Other,
        ] {
            assert_eq!(
                control
                    .send(1280, Duration::ZERO, || Err::<(), _>(kind.into()))
                    .unwrap_err()
                    .kind(),
                kind
            );
            assert_eq!(control.len, 0);
        }
        assert_eq!(
            control
                .send(1280, Duration::MAX, || panic!("overflowing send"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(control.len, 0);
        for _ in 0..BURST {
            control.send(1280, Duration::ZERO, || Ok(())).unwrap();
        }
        assert_eq!(control.deadline(), Some(Duration::from_nanos(305_176)));
    }
}
