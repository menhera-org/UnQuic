//! ACK scheduling and bounded sender validation; storage admission belongs to reassembly.

mod journal;
mod sent;
pub use journal::{FramePiece, PieceJournal};
pub use sent::{
    AckProgress, Acknowledged, History, HistoryError, OutstandingScan, SENT_CAPACITY, SentPacket,
};

use std::time::Duration;

const DELAY: Duration = Duration::from_millis(1);

pub(crate) struct Schedule {
    pub highest: u64,
    highest_at: Duration,
    eliciting: u8,
    pub deadline: Option<Duration>,
    last_feedback: Option<Duration>,
}

impl Schedule {
    pub const fn new() -> Self {
        Self {
            highest: 0,
            highest_at: Duration::ZERO,
            eliciting: 0,
            deadline: None,
            last_feedback: None,
        }
    }

    pub fn accepted(&mut self, number: u64, eliciting: bool, now: Duration) {
        let gap = number > self.highest.saturating_add(1);
        if number > self.highest {
            self.highest = number;
            self.highest_at = now;
        }
        if eliciting {
            self.eliciting = self.eliciting.saturating_add(1);
            self.schedule(if gap || self.eliciting >= 16 {
                now
            } else {
                now.saturating_add(DELAY)
            });
        }
    }

    pub fn duplicate(&mut self, now: Duration) {
        if self.highest != 0 {
            let due = if let Some(last) = self.last_feedback {
                let Some(next) = last.checked_add(DELAY) else {
                    return;
                };
                next.max(now)
            } else {
                now
            };
            self.schedule(due);
        }
    }

    fn schedule(&mut self, due: Duration) {
        self.deadline = Some(self.deadline.map_or(due, |old| old.min(due)));
    }

    pub fn transmitted(&mut self, now: Duration) {
        let sent = self.last_feedback.map_or(now, |old| old.max(now));
        self.last_feedback = Some(sent);
        if self.eliciting == 0 {
            self.deadline = self
                .deadline
                .and_then(|due| sent.checked_add(DELAY).map(|next| due.max(next)));
        }
    }

    pub fn take(&mut self, now: Duration) -> Option<u32> {
        if self.deadline.is_none_or(|due| due > now) {
            return None;
        }
        self.deadline = None;
        self.eliciting = 0;
        self.last_feedback = Some(now);
        Some(now.saturating_sub(self.highest_at).as_micros().min(1000) as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn us(n: u64) -> Duration {
        Duration::from_micros(n)
    }

    #[test]
    fn count_deadline_and_gap_use_accepted_packets_only() {
        let mut ack = Schedule::new();
        for number in 1..16 {
            ack.accepted(number, true, us(number));
        }
        assert_eq!(ack.deadline, Some(us(1001)));
        assert_eq!(ack.take(us(999)), None);
        ack.accepted(16, true, us(999));
        assert_eq!(ack.take(us(999)), Some(0));
        assert_eq!(ack.take(us(1001)), None);
        ack.accepted(17, true, us(2000));
        assert_eq!(ack.take(us(2999)), None);
        assert_eq!(ack.take(us(3000)), Some(1000));
        ack.accepted(19, true, us(4000));
        assert_eq!(ack.take(us(4000)), Some(0));
        ack.accepted(18, true, us(4500));
        assert_eq!(ack.take(us(5500)), Some(1000));
    }

    #[test]
    fn feedback_alone_never_schedules_and_delay_tracks_the_highest_packet() {
        let mut ack = Schedule::new();
        ack.accepted(100, false, us(0));
        assert_eq!(ack.take(us(10000)), None);
        ack.accepted(101, true, us(100));
        ack.accepted(103, false, us(500));
        ack.accepted(102, false, us(700));
        assert_eq!(ack.deadline, Some(us(1100)));
        assert_eq!(ack.take(us(1100)), Some(600));
        ack.accepted(104, false, us(2000));
        assert_eq!(ack.take(us(3000)), None);
    }

    #[test]
    fn duplicate_rate_limit_coalesces_without_delaying_fresh_feedback() {
        let mut ack = Schedule::new();
        ack.duplicate(us(0));
        assert_eq!(ack.deadline, None);
        ack.accepted(1, true, us(0));
        ack.duplicate(us(10));
        assert_eq!(ack.take(us(10)), Some(10));
        for now in [10, 11, 100, 1000] {
            ack.duplicate(us(now));
            assert_eq!(ack.deadline, Some(us(1010)));
            assert_eq!(ack.take(us(now)), None);
        }
        assert_eq!(ack.take(us(1010)), Some(1000));
        ack.duplicate(us(1100));
        ack.accepted(3, true, us(1200));
        assert_eq!(ack.take(us(1200)), Some(0));
        ack.duplicate(us(1200));
        assert_eq!(ack.deadline, Some(us(2200)));
    }

    #[test]
    fn deferred_submission_spaces_duplicates_but_preserves_fresh_feedback() {
        for fresh in [false, true] {
            let mut ack = Schedule::new();
            ack.accepted(1, true, us(0));
            assert_eq!(ack.take(us(1000)), Some(1000));
            ack.duplicate(us(2000));
            if fresh {
                ack.accepted(3, true, us(2500));
            }
            ack.transmitted(us(3000));
            assert_eq!(ack.deadline, Some(us(if fresh { 2000 } else { 4000 })));
            if !fresh {
                assert_eq!(ack.take(us(3999)), None);
            }
            assert!(ack.take(us(4000)).is_some());
            ack.duplicate(Duration::MAX);
            ack.transmitted(Duration::MAX);
            assert_eq!(ack.deadline, None);
        }
    }

    #[test]
    fn maximum_time_and_packet_number_never_wrap() {
        let mut ack = Schedule::new();
        ack.accepted(u64::MAX, true, Duration::MAX);
        assert_eq!(ack.take(Duration::MAX), Some(0));
        ack.duplicate(Duration::MAX);
        assert_eq!(ack.take(Duration::MAX), None);
    }
}
