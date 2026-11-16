//! Restores the order of one striped flow's frames before its writer
//! delivers them (design section 13.1, item 6).
//!
//! Frames carry 48-bit stripe sequences from 1. The window holds frames that
//! arrive ahead of the next sequence and releases them in order. A gap is
//! skipped when its sequence is known to be abandoned, when the oldest held
//! frame has waited the caller's hold limit, or when a frame arrives too far
//! ahead for the window. A frame arriving after its gap was skipped is
//! delivered at once, out of sequence. Storage is fixed; nothing allocates.

use std::time::Duration;

/// Frames a window can hold ahead of its next sequence: the skew between a
/// stripe's lanes it can absorb, about 10 ms of one flow at 1 Gbit/s.
pub const WINDOW: usize = 1024;
const WORDS: usize = WINDOW / 64;

/// What [`Window::insert`] did with a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
    /// Stored; [`Window::drain`] releases it in order.
    Held,
    /// Behind the window after its gap was skipped: deliver it now.
    Late,
    /// Already delivered or held, or sequence zero: drop it.
    Duplicate,
    /// Too far ahead: drain the window, then insert it again.
    Blocked,
}

/// Why sequences were skipped, for counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    /// Frames delivered out of sequence because their gap had been skipped.
    pub late: u64,
    /// Frames dropped as duplicates.
    pub duplicates: u64,
    /// Sequences skipped without delivery, for any reason.
    pub skipped: u64,
    /// Skips that the hold limit started.
    pub timeout_skips: u64,
    /// Skips that a frame too far ahead started.
    pub full_skips: u64,
    /// Sequences known to be abandoned.
    pub abandoned: u64,
    /// Skips the caller started because every source had passed the gap.
    pub passed_skips: u64,
}

#[derive(Clone, Copy)]
enum Slot<T> {
    Empty,
    Frame {
        sequence: u64,
        item: T,
        at: Duration,
    },
    Gone {
        sequence: u64,
    },
}

/// One stripe's reorder window.
pub struct Window<T: Copy> {
    // Lowest sequence neither delivered nor skipped.
    next: u64,
    // Sequences below this are skipped when drain reaches them.
    floor: u64,
    slots: [Slot<T>; WINDOW],
    held: usize,
    // Arrival of the oldest held frame; None when unknown after a release.
    oldest: Option<Duration>,
    // Delivered sequences among the WINDOW below next, so a late frame can
    // be told from a duplicate.
    delivered: [u64; WORDS],
    counters: Counters,
}

impl<T: Copy> Default for Window<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Copy> Window<T> {
    /// An empty window expecting sequence 1.
    pub const fn new() -> Self {
        Self {
            next: 1,
            floor: 1,
            slots: [Slot::Empty; WINDOW],
            held: 0,
            oldest: None,
            delivered: [0; WORDS],
            counters: Counters {
                late: 0,
                duplicates: 0,
                skipped: 0,
                timeout_skips: 0,
                full_skips: 0,
                abandoned: 0,
                passed_skips: 0,
            },
        }
    }

    /// The next sequence the window waits for.
    pub const fn next(&self) -> u64 {
        self.next
    }

    /// Frames currently held.
    pub const fn held(&self) -> usize {
        self.held
    }

    /// Totals since the window was created.
    pub const fn counters(&self) -> Counters {
        self.counters
    }

    fn bit(sequence: u64) -> (usize, u64) {
        let index = (sequence % WINDOW as u64) as usize;
        (index / 64, 1 << (index % 64))
    }

    fn was_delivered(&self, sequence: u64) -> bool {
        let (word, mask) = Self::bit(sequence);
        self.delivered[word] & mask != 0
    }

    fn set_delivered(&mut self, sequence: u64, delivered: bool) {
        let (word, mask) = Self::bit(sequence);
        if delivered {
            self.delivered[word] |= mask;
        } else {
            self.delivered[word] &= !mask;
        }
    }

    /// Offers a frame with its stripe sequence, received at `now`.
    pub fn insert(&mut self, sequence: u64, item: T, now: Duration) -> Placement {
        if sequence < self.next {
            // The delivered marks cover the WINDOW sequences below the next.
            // An older frame cannot be checked against them; its lane hands
            // over each frame once, so it is a late repair, not a repeat.
            let marked = self.next - sequence <= WINDOW as u64;
            if sequence == 0 || (marked && self.was_delivered(sequence)) {
                self.counters.duplicates += 1;
                return Placement::Duplicate;
            }
            if marked {
                self.set_delivered(sequence, true);
            }
            self.counters.late += 1;
            return Placement::Late;
        }
        if sequence - self.next >= WINDOW as u64 {
            let floor = sequence - WINDOW as u64 + 1;
            if floor > self.floor {
                self.floor = floor;
                self.counters.full_skips += 1;
            }
            return Placement::Blocked;
        }
        let slot = &mut self.slots[(sequence % WINDOW as u64) as usize];
        if matches!(*slot, Slot::Frame { sequence: held, .. } if held == sequence) {
            self.counters.duplicates += 1;
            return Placement::Duplicate;
        }
        *slot = Slot::Frame {
            sequence,
            item,
            at: now,
        };
        self.held += 1;
        if self.held == 1 {
            self.oldest = Some(now);
        } else if let Some(oldest) = self.oldest {
            self.oldest = Some(oldest.min(now));
        }
        Placement::Held
    }

    /// Marks a sequence the sender abandoned before it arrived.
    pub fn abandon(&mut self, sequence: u64) {
        if sequence < self.next || sequence - self.next >= WINDOW as u64 {
            return;
        }
        let slot = &mut self.slots[(sequence % WINDOW as u64) as usize];
        if matches!(*slot, Slot::Empty) {
            *slot = Slot::Gone { sequence };
            self.counters.abandoned += 1;
        }
    }

    /// Skips every gap below `sequence`, which the caller knows no frame can
    /// still fill in order; true when drain may now release more. Sequences
    /// beyond the window's reach are left to the full-window rule.
    pub fn skip_to(&mut self, sequence: u64) -> bool {
        let sequence = sequence.min(self.next + WINDOW as u64);
        if sequence <= self.floor || sequence <= self.next {
            return false;
        }
        self.floor = sequence;
        self.counters.passed_skips += 1;
        true
    }

    /// Releases the next frame in order, skipping gaps below the floor and
    /// abandoned sequences; None when the window waits for a gap.
    pub fn drain(&mut self) -> Option<T> {
        loop {
            let index = (self.next % WINDOW as u64) as usize;
            match self.slots[index] {
                Slot::Frame { sequence, item, .. } if sequence == self.next => {
                    self.slots[index] = Slot::Empty;
                    self.held -= 1;
                    self.set_delivered(sequence, true);
                    self.next += 1;
                    // Recomputed on demand, since the released frame may have been the oldest.
                    self.oldest = None;
                    return Some(item);
                }
                Slot::Gone { sequence } if sequence == self.next => {
                    self.slots[index] = Slot::Empty;
                }
                _ if self.next < self.floor => {
                    self.counters.skipped += 1;
                }
                _ => return None,
            }
            self.set_delivered(self.next, false);
            self.next += 1;
        }
    }

    fn oldest(&mut self) -> Option<Duration> {
        if self.held == 0 {
            return None;
        }
        if self.oldest.is_none() {
            self.oldest = self
                .slots
                .iter()
                .filter_map(|slot| match *slot {
                    Slot::Frame { at, .. } => Some(at),
                    _ => None,
                })
                .min();
        }
        self.oldest
    }

    /// When the oldest held frame reaches the hold limit, if any is held.
    pub fn deadline(&mut self, limit: Duration) -> Option<Duration> {
        self.oldest().map(|oldest| oldest.saturating_add(limit))
    }

    /// Skips the gaps before the lowest held frame once the oldest held frame
    /// has waited `limit`; true when drain now has frames to release.
    pub fn expire(&mut self, now: Duration, limit: Duration) -> bool {
        if self.deadline(limit).is_none_or(|due| now < due) {
            return false;
        }
        let lowest = (self.next..self.next + WINDOW as u64).find(|&sequence| {
            matches!(self.slots[(sequence % WINDOW as u64) as usize],
                Slot::Frame { sequence: held, .. } if held == sequence)
        });
        match lowest {
            Some(lowest) if lowest > self.floor => {
                self.floor = lowest;
                self.counters.timeout_skips += 1;
                true
            }
            Some(_) => true,
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMIT: Duration = Duration::from_millis(5);

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn drained(window: &mut Window<u64>) -> Vec<u64> {
        std::iter::from_fn(|| window.drain()).collect()
    }

    #[test]
    fn gaps_every_source_has_passed_are_skipped_at_once() {
        let mut window = Window::new();
        for sequence in [2, 3, 5, 6] {
            assert_eq!(window.insert(sequence, sequence, ms(0)), Placement::Held);
        }
        // Nothing below 1 or at the next sequence can be skipped.
        assert!(!window.skip_to(1));
        assert_eq!(drained(&mut window), []);
        // Every source is past 4: sequences 1 and 4 are skipped, 5 and 6 are not held back.
        assert!(window.skip_to(5));
        assert_eq!(drained(&mut window), [2, 3, 5, 6]);
        assert!(!window.skip_to(5));
        assert_eq!(window.counters().passed_skips, 1);
        assert_eq!(window.counters().skipped, 2);
        // The skipped frames still arrive once, out of sequence.
        assert_eq!(window.insert(4, 4, ms(1)), Placement::Late);
        assert_eq!(window.insert(4, 4, ms(1)), Placement::Duplicate);
        // A mark beyond the window stops at its reach.
        assert_eq!(window.insert(8, 8, ms(1)), Placement::Held);
        assert!(window.skip_to(u64::MAX));
        assert_eq!(drained(&mut window), [8]);
        assert_eq!(window.next(), 7 + WINDOW as u64);
    }

    #[test]
    fn in_order_and_held_frames_leave_in_sequence() {
        let mut window = Window::new();
        assert_eq!(window.insert(1, 1, ms(0)), Placement::Held);
        assert_eq!(drained(&mut window), [1]);
        for sequence in [4, 3, 5] {
            assert_eq!(window.insert(sequence, sequence, ms(1)), Placement::Held);
        }
        assert_eq!(drained(&mut window), []);
        assert_eq!(window.held(), 3);
        assert_eq!(window.insert(2, 2, ms(2)), Placement::Held);
        assert_eq!(drained(&mut window), [2, 3, 4, 5]);
        assert_eq!((window.next(), window.held()), (6, 0));
        // Duplicates of delivered and of held frames, and sequence zero.
        assert_eq!(window.insert(3, 3, ms(3)), Placement::Duplicate);
        assert_eq!(window.insert(7, 7, ms(3)), Placement::Held);
        assert_eq!(window.insert(7, 7, ms(3)), Placement::Duplicate);
        assert_eq!(window.insert(0, 0, ms(3)), Placement::Duplicate);
        assert_eq!(window.counters().duplicates, 3);
    }

    #[test]
    fn the_hold_limit_skips_a_gap_and_its_late_frame_still_goes_out_once() {
        let mut window = Window::new();
        window.insert(1, 1, ms(0));
        drained(&mut window);
        window.insert(3, 3, ms(10));
        window.insert(4, 4, ms(12));
        assert_eq!(window.deadline(LIMIT), Some(ms(15)));
        assert!(!window.expire(ms(14), LIMIT));
        assert!(window.expire(ms(15), LIMIT));
        assert_eq!(drained(&mut window), [3, 4]);
        let counters = window.counters();
        assert_eq!((counters.skipped, counters.timeout_skips), (1, 1));
        assert_eq!(window.insert(2, 2, ms(16)), Placement::Late);
        assert_eq!(window.insert(2, 2, ms(16)), Placement::Duplicate);
        assert_eq!(window.counters().late, 1);
        // The next gap gets its own full hold, from its oldest frame.
        window.insert(7, 7, ms(20));
        window.insert(6, 6, ms(21));
        assert_eq!(drained(&mut window), []);
        assert_eq!(window.deadline(LIMIT), Some(ms(25)));
        assert!(window.expire(ms(25), LIMIT));
        assert_eq!(drained(&mut window), [6, 7]);
    }

    #[test]
    fn a_frame_older_than_the_delivered_marks_is_late_not_a_repeat() {
        let mut window = Window::new();
        // Sequence 1 is skipped, then a window and more of frames goes by.
        let last = WINDOW as u64 + 2;
        for sequence in 2..=last {
            assert_eq!(window.insert(sequence, sequence, ms(0)), Placement::Held);
            window.skip_to(2);
            assert_eq!(drained(&mut window), [sequence]);
        }
        // A repair that late is still delivered, once at least; within the
        // marks a repeat is recognized.
        assert_eq!(window.insert(1, 1, ms(9)), Placement::Late);
        assert_eq!(window.insert(last, last, ms(9)), Placement::Duplicate);
        let counters = window.counters();
        assert_eq!((counters.late, counters.duplicates), (1, 1));
    }

    #[test]
    fn abandoned_sequences_and_a_full_window_skip_their_gaps() {
        let mut window = Window::new();
        window.abandon(1);
        window.insert(2, 2, ms(0));
        assert_eq!(drained(&mut window), [2]);
        assert_eq!(window.counters().abandoned, 1);
        // Behind or beyond the window, abandonment is ignored.
        window.abandon(1);
        window.abandon(3 + WINDOW as u64);
        assert_eq!(window.counters().abandoned, 1);
        // Sequence 3 is missing; a frame a full window ahead forces the skip.
        window.insert(4, 4, ms(1));
        let far = 3 + WINDOW as u64;
        assert_eq!(window.insert(far, far, ms(2)), Placement::Blocked);
        assert_eq!(drained(&mut window), [4]);
        assert_eq!(window.insert(far, far, ms(2)), Placement::Held);
        // Everything between 5 and far was never sent: skipped as the floor allows.
        assert_eq!(drained(&mut window), []);
        assert_eq!(window.counters().full_skips, 1);
        assert!(window.expire(ms(7), LIMIT));
        assert_eq!(drained(&mut window), [far]);
        assert_eq!(window.held(), 0);
        assert_eq!(window.deadline(LIMIT), None);
    }

    #[test]
    fn random_arrivals_leave_once_in_order_except_late_frames() {
        let mut seed = 0x5eed_u64;
        let mut random = |below: u64| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) % below
        };
        let mut totals = Counters::default();
        for round in 0..50 {
            let mut window = Window::new();
            let count = 2000;
            // Sequences arrive shuffled within a bounded distance, some lost or
            // repeated. A repeat follows closely: lanes hand each frame over
            // once, so none can come from beyond the delivered marks.
            let mut arrivals: Vec<u64> = (1..=count).collect();
            let spread = [4, 64, 1500][round % 3];
            for index in 0..arrivals.len() {
                let other = (index + random(spread) as usize).min(arrivals.len() - 1);
                arrivals.swap(index, other);
            }
            let mut delivered = vec![0_u8; count as usize + 1];
            let mut last_in_order = 0;
            let mut now = Duration::ZERO;
            let mut queue = std::collections::VecDeque::from(arrivals);
            while let Some(sequence) = queue.pop_front() {
                now += Duration::from_micros(random(200));
                if random(50) == 0 {
                    continue; // lost
                }
                // A frame later than the marks reach is not remembered, so only
                // rounds that stay within them repeat frames.
                if spread < WINDOW as u64 && random(50) == 0 {
                    queue.insert(queue.len().min(2), sequence); // repeated soon
                }
                match window.insert(sequence, sequence, now) {
                    Placement::Blocked => {
                        queue.push_front(sequence);
                        while let Some(item) = window.drain() {
                            assert!(item > last_in_order);
                            last_in_order = item;
                            delivered[item as usize] += 1;
                        }
                        // A blocked frame always fits after draining.
                        continue;
                    }
                    Placement::Late => delivered[sequence as usize] += 1,
                    Placement::Duplicate | Placement::Held => {}
                }
                // A writer expires and drains until nothing more is due.
                loop {
                    let expired = window.expire(now, LIMIT);
                    while let Some(item) = window.drain() {
                        assert!(
                            item > last_in_order,
                            "out of order {item} after {last_in_order}"
                        );
                        last_in_order = item;
                        delivered[item as usize] += 1;
                    }
                    if !expired {
                        break;
                    }
                }
                assert!(window.held() <= WINDOW);
                if let Some(due) = window.deadline(LIMIT) {
                    assert!(due > now, "a frame waited past the limit");
                }
            }
            let end = now + LIMIT;
            while window.expire(end, LIMIT) {
                while let Some(item) = window.drain() {
                    delivered[item as usize] += 1;
                }
            }
            assert!(delivered.iter().all(|&n| n <= 1), "a frame left twice");
            assert_eq!(window.held(), 0);
            let counters = window.counters();
            totals.late += counters.late;
            totals.duplicates += counters.duplicates;
            totals.timeout_skips += counters.timeout_skips;
            totals.full_skips += counters.full_skips;
        }
        // Every rule was exercised.
        assert!(totals.late > 0 && totals.duplicates > 0, "{totals:?}");
        assert!(
            totals.timeout_skips > 0 && totals.full_skips > 0,
            "{totals:?}"
        );
    }
}
