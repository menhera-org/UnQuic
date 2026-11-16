//! Moves completed striped frames to their stripe's writer and restores
//! their order there (design section 13.1, item 6).
//!
//! A lane owner packs completed striped frames, each still starting with its
//! stripe tag, into a transfer buffer as `[length: u16 big-endian][flags:
//! u8][lane: u16][frame]` entries (flag bit 1: partial checksum, as on the
//! wire) and hands the buffer to the stripe's writer. The [`Writer`] keeps
//! a [`reorder::Window`] per active stripe, holds each buffer until every
//! frame in it has been delivered or dropped, and reports both through
//! callbacks, so nothing allocates.
//!
//! Each lane delivers its share of a stripe in rising sequence, except for
//! repaired frames. So once every lane recently carrying the stripe has
//! delivered a sequence at or past some mark, no frame below the mark can
//! still arrive in order, and the writer skips those gaps at once instead
//! of holding the frames behind them for the hold limit.

use crate::reorder::{Placement, Window};
use crate::wire::{STRIPE_TAG_LEN, stripe_tag};
use std::{ops::Range, time::Duration};

/// Active stripes one writer tracks at once.
pub const STRIPES: usize = 16;
/// Transfer buffers one writer may hold at once.
pub const HOLDERS: usize = 64;
/// A stripe silent this long starts afresh, so a reused stripe number cannot
/// meet its predecessor's window.
pub const IDLE: Duration = Duration::from_secs(1);

/// Lanes whose progress one stripe tracks. Frames of further lanes are still
/// ordered, but gaps are skipped without waiting for those lanes to pass.
const SOURCES: usize = 16;

/// Entry header bytes: length, flags, then the lane the frame arrived on.
const ENTRY_LEN: usize = 5;

/// Appends one frame, with whether its checksum is partial and the receive
/// lane it completed on, to a transfer buffer at `*used`; false, changing
/// nothing, when it does not fit or carries no stripe tag.
pub fn pack(buffer: &mut [u8], used: &mut usize, frame: &[u8], partial: bool, lane: u16) -> bool {
    let Ok(length) = u16::try_from(frame.len()) else {
        return false;
    };
    let end = *used + ENTRY_LEN + frame.len();
    if frame.len() < STRIPE_TAG_LEN || end > buffer.len() {
        return false;
    }
    buffer[*used..*used + 2].copy_from_slice(&length.to_be_bytes());
    buffer[*used + 2] = u8::from(partial) << 1;
    buffer[*used + 3..*used + ENTRY_LEN].copy_from_slice(&lane.to_be_bytes());
    buffer[*used + ENTRY_LEN..end].copy_from_slice(frame);
    *used = end;
    true
}

/// The frames packed in a buffer, as ranges of it with their partial flag
/// and lane. Stops at the first malformed entry.
pub fn entries(buffer: &[u8]) -> impl Iterator<Item = (Range<usize>, bool, u16)> + '_ {
    let mut at = 0;
    std::iter::from_fn(move || {
        let header = buffer.get(at..)?.first_chunk::<ENTRY_LEN>()?;
        let length = usize::from(u16::from_be_bytes([header[0], header[1]]));
        let range = at + ENTRY_LEN..at + ENTRY_LEN + length;
        buffer.get(range.clone())?;
        at = range.end;
        Some((
            range,
            header[2] & 2 != 0,
            u16::from_be_bytes([header[3], header[4]]),
        ))
    })
}

#[derive(Clone, Copy)]
struct Item {
    holder: u8,
    partial: bool,
    start: u32,
    end: u32,
}

/// One lane's progress in a stripe.
#[derive(Clone, Copy)]
struct Source {
    lane: u16,
    highest: u64,
    last: Duration,
}

struct Stripe {
    number: u16,
    last: Duration,
    window: Window<Item>,
    sources: [Option<Source>; SOURCES],
}

impl Stripe {
    fn new(number: u16, now: Duration) -> Self {
        Self {
            number,
            last: now,
            window: Window::new(),
            sources: [None; SOURCES],
        }
    }

    /// Records that `lane` delivered `sequence` at `now`.
    fn saw(&mut self, lane: u16, sequence: u64, now: Duration) {
        let mut free = None;
        for (index, slot) in self.sources.iter_mut().enumerate() {
            match slot {
                Some(source) if source.lane == lane => {
                    source.highest = source.highest.max(sequence);
                    source.last = now;
                    return;
                }
                None => {
                    free.get_or_insert(index);
                }
                Some(_) => {}
            }
        }
        if let Some(index) = free {
            self.sources[index] = Some(Source {
                lane,
                highest: sequence,
                last: now,
            });
        }
    }

    /// The sequence every lane of the stripe has reached. A lane counts for
    /// as long as a stripe lives without frames, IDLE: a flow that pauses
    /// between bursts keeps its lanes, so the first to resume does not skip
    /// ahead of the others, and the hold limit covers a lane that stopped.
    fn passed(&mut self, now: Duration) -> Option<u64> {
        let mut mark: Option<u64> = None;
        for slot in &mut self.sources {
            match slot {
                Some(source) if now.saturating_sub(source.last) < IDLE => {
                    mark = Some(mark.map_or(source.highest, |mark| mark.min(source.highest)));
                }
                Some(_) => *slot = None,
                None => {}
            }
        }
        mark
    }
}

/// Counts of what a writer did, for counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Totals {
    /// Frames written in sequence.
    pub delivered: u64,
    /// Frames written out of sequence: late, or without a free stripe slot.
    pub unordered: u64,
    /// Frames dropped as duplicates or malformed.
    pub dropped: u64,
}

/// One writer's stripes and the transfer buffers their held frames live in.
/// `H` is the caller's handle for a transfer buffer.
pub struct Writer<H: Copy> {
    // STRIPES slots, on the heap: each holds a whole reorder window.
    stripes: Box<[Option<Stripe>]>,
    holders: [Option<(H, u32)>; HOLDERS],
    totals: Totals,
}

impl<H: Copy> Default for Writer<H> {
    fn default() -> Self {
        Self::new()
    }
}

impl<H: Copy> Writer<H> {
    /// A writer tracking no stripe and holding no buffer. Allocates its
    /// stripe slots here, once.
    pub fn new() -> Self {
        Self {
            stripes: (0..STRIPES).map(|_| None).collect(),
            holders: [None; HOLDERS],
            totals: Totals::default(),
        }
    }

    /// Heap bytes a writer owns.
    pub const fn storage_bytes() -> usize {
        STRIPES * size_of::<Option<Stripe>>()
    }

    /// Totals since the writer was created.
    pub const fn totals(&self) -> Totals {
        self.totals
    }

    /// Transfer buffers currently held.
    pub fn holding(&self) -> usize {
        self.holders.iter().flatten().count()
    }

    /// Whether another transfer buffer can be accepted.
    pub fn can_accept(&self) -> bool {
        self.holders.iter().any(Option::is_none)
    }

    fn stripe(&mut self, number: u16, now: Duration) -> Option<usize> {
        let mut free = None;
        for (index, slot) in self.stripes.iter_mut().enumerate() {
            match slot {
                Some(stripe) if stripe.number == number => {
                    if now.saturating_sub(stripe.last) >= IDLE && stripe.window.held() == 0 {
                        *stripe = Stripe::new(number, now);
                    }
                    stripe.last = now;
                    return Some(index);
                }
                Some(stripe)
                    if stripe.window.held() == 0 && now.saturating_sub(stripe.last) >= IDLE =>
                {
                    free.get_or_insert(index);
                }
                None => {
                    free.get_or_insert(index);
                }
                Some(_) => {}
            }
        }
        let index = free?;
        self.stripes[index] = Some(Stripe::new(number, now));
        Some(index)
    }

    /// Takes a transfer buffer's frames at `now`, writing at once those that
    /// are in sequence or cannot wait, and skipping gaps that every lane
    /// of their stripe has passed. `bytes` must stay readable through
    /// `handle` until `release` returns it. False, taking nothing, when no
    /// holder slot is free.
    pub fn accept(
        &mut self,
        handle: H,
        bytes: &[u8],
        now: Duration,
        deliver: &mut impl FnMut(H, Range<usize>, bool),
        release: &mut impl FnMut(H),
    ) -> bool {
        let Some(holder) = self.holders.iter().position(Option::is_none) else {
            return false;
        };
        // Held while this call runs, so deliveries cannot release it early.
        self.holders[holder] = Some((handle, 1));
        // One bit per stripe slot this buffer touched.
        let mut touched = 0_u16;
        for (range, partial, lane) in entries(bytes) {
            let frame = &bytes[range.clone()];
            let Some((number, sequence)) = stripe_tag(frame) else {
                self.totals.dropped += 1;
                continue;
            };
            let item = Item {
                holder: holder as u8,
                partial,
                start: range.start as u32,
                end: range.end as u32,
            };
            let Some(index) = self.stripe(number, now) else {
                self.totals.unordered += 1;
                deliver(handle, range, partial);
                continue;
            };
            touched |= 1 << index;
            if let Some(stripe) = self.stripes[index].as_mut() {
                stripe.saw(lane, sequence, now);
            }
            while let Some(stripe) = self.stripes[index].as_mut() {
                match stripe.window.insert(sequence, item, now) {
                    Placement::Held => {
                        if let Some((_, references)) = &mut self.holders[holder] {
                            *references += 1;
                        }
                    }
                    Placement::Late => {
                        self.totals.unordered += 1;
                        deliver(handle, range.clone(), partial);
                    }
                    Placement::Duplicate => self.totals.dropped += 1,
                    Placement::Blocked => {
                        self.drain(index, deliver, release);
                        continue;
                    }
                }
                break;
            }
            self.drain(index, deliver, release);
        }
        for index in 0..STRIPES {
            if touched & (1 << index) != 0
                && self.stripes[index].as_mut().is_some_and(|stripe| {
                    stripe
                        .passed(now)
                        .is_some_and(|mark| stripe.window.skip_to(mark))
                })
            {
                self.drain(index, deliver, release);
            }
        }
        self.unhold(holder as u8, release);
        true
    }

    fn unhold(&mut self, holder: u8, release: &mut impl FnMut(H)) {
        let slot = &mut self.holders[usize::from(holder)];
        if let Some((handle, references)) = slot {
            *references -= 1;
            if *references == 0 {
                let handle = *handle;
                *slot = None;
                release(handle);
            }
        }
    }

    fn drain(
        &mut self,
        index: usize,
        deliver: &mut impl FnMut(H, Range<usize>, bool),
        release: &mut impl FnMut(H),
    ) {
        while let Some(item) = self.stripes[index]
            .as_mut()
            .and_then(|stripe| stripe.window.drain())
        {
            if let Some((handle, _)) = self.holders[usize::from(item.holder)] {
                self.totals.delivered += 1;
                deliver(handle, item.start as usize..item.end as usize, item.partial);
            }
            self.unhold(item.holder, release);
        }
    }

    /// Skips gaps whose oldest held frame has waited `limit` and writes what
    /// that releases. Returns the next time anything will be due.
    pub fn expire(
        &mut self,
        now: Duration,
        limit: Duration,
        deliver: &mut impl FnMut(H, Range<usize>, bool),
        release: &mut impl FnMut(H),
    ) -> Option<Duration> {
        let mut due: Option<Duration> = None;
        for index in 0..STRIPES {
            while self.stripes[index]
                .as_mut()
                .is_some_and(|stripe| stripe.window.expire(now, limit))
            {
                self.drain(index, deliver, release);
            }
            if let Some(next) = self.stripes[index]
                .as_mut()
                .and_then(|stripe| stripe.window.deadline(limit))
            {
                due = Some(due.map_or(next, |due| due.min(next)));
            }
        }
        due
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(number: u16, sequence: u64, fill: u8) -> Vec<u8> {
        let mut frame = ((u64::from(number) << 48) | sequence)
            .to_be_bytes()
            .to_vec();
        frame.extend_from_slice(&[fill; 40]);
        frame
    }

    const LIMIT: Duration = Duration::from_millis(5);

    /// Packs frames with the lane each completed on.
    fn mixed(frames: &[(u16, Vec<u8>)]) -> Vec<u8> {
        let mut buffer = vec![0; 4096];
        let mut used = 0;
        for (lane, frame) in frames {
            assert!(pack(&mut buffer, &mut used, frame, false, *lane));
        }
        buffer.truncate(used);
        buffer
    }

    fn batch(lane: u16, frames: &[Vec<u8>]) -> Vec<u8> {
        let frames: Vec<_> = frames.iter().map(|frame| (lane, frame.clone())).collect();
        mixed(&frames)
    }

    #[test]
    fn packing_round_trips_and_refuses_what_does_not_fit() {
        let frames = [frame(1, 1, 1), frame(1, 2, 2)];
        let buffer = batch(1, &frames);
        let read: Vec<_> = entries(&buffer)
            .map(|(range, _, _)| buffer[range].to_vec())
            .collect();
        assert_eq!(read, frames);
        let mut small = [0; 60];
        let mut used = 0;
        assert!(pack(&mut small, &mut used, &frame(1, 1, 1), true, 0x1234));
        assert!(!pack(&mut small, &mut used, &frame(1, 2, 2), false, 1));
        assert!(
            !pack(&mut small, &mut used, &[0; 7], false, 1),
            "no room for a tag"
        );
        assert_eq!(used, 53);
        // The partial flag and the lane travel with their frame.
        assert_eq!(entries(&small[..used]).next(), Some((5..53, true, 0x1234)));
        // A truncated entry ends the walk.
        assert_eq!(entries(&buffer[..buffer.len() - 1]).count(), 1);
    }

    #[test]
    fn frames_spread_over_buffers_leave_in_order_and_buffers_return_once_empty() {
        let mut writer = Writer::<u8>::new();
        let mut written = Vec::new();
        let mut released = Vec::new();
        // Lane 2 carries the odd sequences and lane 1 the even ones.
        let buffers = [
            batch(2, &[frame(3, 1, 1)]),
            batch(1, &[frame(3, 2, 2), frame(3, 4, 4), frame(3, 6, 6)]),
            batch(2, &[frame(3, 3, 3), frame(3, 5, 5)]),
        ];
        let mut deliver = |handle: u8, range: Range<usize>, _| {
            written.push(buffers[usize::from(handle)][range][STRIPE_TAG_LEN]);
        };
        let mut release = |handle: u8| released.push(handle);
        let at = Duration::ZERO;
        assert!(writer.accept(0, &buffers[0], at, &mut deliver, &mut release));
        // Lane 2 has only reached 1, so 4 and 6 wait behind the missing 3.
        assert!(writer.accept(1, &buffers[1], at, &mut deliver, &mut release));
        assert_eq!(writer.holding(), 1);
        assert!(writer.accept(2, &buffers[2], at, &mut deliver, &mut release));
        assert_eq!(written, [1, 2, 3, 4, 5, 6]);
        // Frame 6, buffer 1's last, leaves during the third call, before
        // that call lets go of buffer 2.
        assert_eq!(released, [0, 1, 2]);
        assert_eq!(writer.holding(), 0);
        assert_eq!(writer.totals().delivered, 6);
        assert_eq!(writer.totals().unordered, 0);
    }

    #[test]
    fn gaps_every_lane_has_passed_are_skipped_without_waiting() {
        let mut writer = Writer::<u8>::new();
        let mut written = Vec::new();
        // Three lanes carry reads in turn; lane 2 loses sequence 4.
        let buffers = [
            batch(1, &[frame(5, 1, 1), frame(5, 2, 2)]),
            batch(2, &[frame(5, 3, 3), frame(5, 5, 5)]),
            batch(3, &[frame(5, 6, 6), frame(5, 7, 7)]),
            batch(1, &[frame(5, 8, 8)]),
            batch(2, &[frame(5, 4, 4)]),
            batch(1, &[frame(5, 11, 11)]),
            batch(1, &[frame(5, 12, 12)]),
            batch(1, &[frame(5, 14, 14)]),
        ];
        let mut deliver = |handle: u8, range: Range<usize>, _| {
            written.push(buffers[usize::from(handle)][range][STRIPE_TAG_LEN]);
        };
        let mut release = |_| {};
        let at = Duration::ZERO;
        for handle in 0..3 {
            writer.accept(
                handle,
                &buffers[usize::from(handle)],
                at,
                &mut deliver,
                &mut release,
            );
        }
        // Lane 1 has only reached 2, so 5, 6 and 7 wait for sequence 4.
        assert_eq!(writer.holding(), 2);
        // Once lane 1 delivers 8, every lane is past 4: it is skipped at once.
        writer.accept(3, &buffers[3], at, &mut deliver, &mut release);
        assert_eq!(writer.holding(), 0);
        assert_eq!(writer.expire(at, LIMIT, &mut deliver, &mut release), None);
        // Its repair still arrives, out of sequence.
        writer.accept(4, &buffers[4], at, &mut deliver, &mut release);
        assert_eq!(writer.totals().unordered, 1);
        // Lanes 2 and 3 pause and lane 1 resumes alone. They still count, so
        // sequence 11 waits out the hold limit instead of skipping 9 and 10
        // at once, which would put a slower lane's frames out of sequence.
        writer.accept(5, &buffers[5], LIMIT, &mut deliver, &mut release);
        assert_eq!(writer.holding(), 1);
        assert_eq!(
            writer.expire(LIMIT * 2, LIMIT, &mut deliver, &mut release),
            None
        );
        // A lane silent for as long as a stripe may idle no longer counts.
        writer.accept(6, &buffers[6], IDLE / 2, &mut deliver, &mut release);
        writer.accept(7, &buffers[7], IDLE, &mut deliver, &mut release);
        assert_eq!(writer.holding(), 0);
        assert_eq!(written, [1, 2, 3, 5, 6, 7, 8, 4, 11, 12, 14]);
    }

    #[test]
    fn gaps_expire_late_frames_still_leave_and_idle_stripes_restart() {
        let mut writer = Writer::<u8>::new();
        let mut written = Vec::new();
        let mut released = 0;
        let limit = LIMIT;
        let buffers = [
            batch(2, &[frame(9, 1, 1)]),
            batch(1, &[frame(9, 3, 3), frame(9, 4, 4)]),
            batch(2, &[frame(9, 2, 2), frame(9, 4, 4)]),
            batch(1, &[frame(9, 1, 7)]),
        ];
        let mut deliver = |handle: u8, range: Range<usize>, _| {
            written.push(buffers[usize::from(handle)][range][STRIPE_TAG_LEN]);
        };
        let mut release = |_| released += 1;
        let at = Duration::ZERO;
        writer.accept(0, &buffers[0], at, &mut deliver, &mut release);
        // Lane 2 has not passed sequence 2, so 3 and 4 wait for the hold limit.
        writer.accept(1, &buffers[1], at, &mut deliver, &mut release);
        let due = writer.expire(Duration::from_millis(1), limit, &mut deliver, &mut release);
        assert_eq!(due, Some(limit));
        assert_eq!(
            writer.expire(limit, limit, &mut deliver, &mut release),
            None
        );
        // The missing frame arrives late; a repeat of 4 is a duplicate.
        writer.accept(2, &buffers[2], limit, &mut deliver, &mut release);
        assert_eq!(writer.totals().unordered, 1);
        assert_eq!(writer.totals().dropped, 1);
        // After a second of silence the number may start over at 1.
        writer.accept(3, &buffers[3], limit + IDLE, &mut deliver, &mut release);
        assert_eq!(written, [1, 3, 4, 2, 7]);
        assert_eq!(writer.holding(), 0);
        assert_eq!(released, 4);
    }

    #[test]
    fn random_batches_leave_once_in_order_per_stripe_and_every_buffer_returns() {
        let mut seed = 0x51_u64;
        let mut random = |below: u64| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) % below
        };
        let limit = LIMIT;
        let lane_of = |sequence: u64| 1 + (sequence / 8 % 3) as u16;
        // Three stripes of 400 frames each. A stripe's three lanes take eight
        // sequences in turn and deliver them in order, except that some
        // frames are lost, repeated, or repaired and so arrive twelve later.
        let mut queues = Vec::new();
        let mut repaired = 0;
        for number in 1..=3_u16 {
            for lane in 1..=3_u16 {
                let mut queue = std::collections::VecDeque::new();
                let mut late: Vec<(usize, u64)> = Vec::new();
                for sequence in (1..=400_u64).filter(|&sequence| lane_of(sequence) == lane) {
                    match random(40) {
                        0 => {}
                        1 => queue.extend([(number, sequence); 2]),
                        2 => {
                            late.push((queue.len() + 12, sequence));
                            repaired += 1;
                        }
                        _ => queue.push_back((number, sequence)),
                    }
                    late.retain(|&(due, sequence)| {
                        let wait = queue.len() < due;
                        if !wait {
                            queue.push_back((number, sequence));
                        }
                        wait
                    });
                }
                queue.extend(late.iter().map(|&(_, sequence)| (number, sequence)));
                queues.push(queue);
            }
        }
        // Lanes run at most about forty sequences ahead of each other.
        let mut arrivals = Vec::new();
        while queues.iter().any(|queue| !queue.is_empty()) {
            let pick = random(9) as usize;
            let stripe = pick / 3 * 3;
            let lowest = (stripe..stripe + 3)
                .filter_map(|index| Some((queues[index].front()?.1, index)))
                .min();
            let Some((lowest, slowest)) = lowest else {
                continue;
            };
            let index = match queues[pick].front() {
                Some(&(_, sequence)) if sequence <= lowest + 40 => pick,
                _ => slowest,
            };
            arrivals.extend(queues[index].pop_front());
        }
        let buffers: Vec<Vec<u8>> = arrivals
            .chunks(1 + random(6) as usize)
            .map(|chunk| {
                let frames: Vec<_> = chunk
                    .iter()
                    .map(|&(number, sequence)| {
                        (lane_of(sequence), frame(number, sequence, sequence as u8))
                    })
                    .collect();
                mixed(&frames)
            })
            .collect();
        let mut writer = Writer::<usize>::new();
        let mut seen = std::collections::HashSet::new();
        let mut last = [0_u64; 4];
        let mut ordered_breaks = 0;
        let mut released = vec![0_u8; buffers.len()];
        let mut now = Duration::ZERO;
        {
            let mut deliver = |handle: usize, range: Range<usize>, _| {
                let (number, sequence) = stripe_tag(&buffers[handle][range]).unwrap();
                assert!(seen.insert((number, sequence)), "{number}/{sequence} twice");
                if sequence < last[usize::from(number)] {
                    ordered_breaks += 1;
                }
                last[usize::from(number)] = last[usize::from(number)].max(sequence);
            };
            let mut release = |handle: usize| released[handle] += 1;
            for (handle, buffer) in buffers.iter().enumerate() {
                now += Duration::from_micros(random(100));
                writer.expire(now, limit, &mut deliver, &mut release);
                assert!(writer.accept(handle, buffer, now, &mut deliver, &mut release));
            }
            writer.expire(now + limit, limit, &mut deliver, &mut release);
        }
        assert!(
            released.iter().all(|&n| n == 1),
            "a buffer returned other than once"
        );
        assert_eq!(writer.holding(), 0);
        // Out-of-sequence writes are exactly the late or unordered ones.
        assert_eq!(ordered_breaks, writer.totals().unordered);
        assert!(writer.totals().dropped > 0 && writer.totals().delivered > 1000);
        // Lost frames delay nothing past their lanes' progress, so only
        // repaired frames, and a stripe's first frames before every lane is
        // known, leave out of sequence.
        assert!(
            writer.totals().unordered <= repaired + 8,
            "{} out of sequence for {repaired} repairs",
            writer.totals().unordered
        );
    }

    #[test]
    fn a_full_stripe_table_or_holder_table_degrades_without_loss() {
        let mut writer = Writer::<usize>::new();
        let mut written = 0;
        let mut released = 0;
        // Fill every stripe slot with a held frame: lane 2 delivers sequence
        // 1, then lane 1 delivers 3, which waits for 2.
        let buffers: Vec<_> = (0..=STRIPES)
            .map(|number| {
                let number = number as u16;
                mixed(&[(2, frame(number, 1, 0)), (1, frame(number, 3, 0))])
            })
            .collect();
        let mut deliver = |_: usize, _: Range<usize>, _| written += 1;
        let mut release = |_: usize| released += 1;
        let at = Duration::ZERO;
        for (handle, buffer) in buffers.iter().enumerate().take(STRIPES) {
            assert!(writer.accept(handle, buffer, at, &mut deliver, &mut release));
        }
        // A seventeenth stripe finds no slot: its frames go out unordered.
        assert!(writer.accept(STRIPES, &buffers[STRIPES], at, &mut deliver, &mut release));
        assert_eq!(writer.totals().unordered, 2);
        assert_eq!(writer.holding(), STRIPES);
        assert_eq!((written, released), (STRIPES + 2, 1));
        // With every holder slot taken, accept refuses and takes nothing.
        let mut holder = Writer::<usize>::new();
        // Lane 2 stops at sequence 1, so each buffer's frame from lane 1
        // waits behind the missing sequence 2.
        let held: Vec<_> = (0..=HOLDERS)
            .map(|handle| {
                let waiting = (1, frame(1, handle as u64 + 3, 0));
                if handle == 0 {
                    mixed(&[(2, frame(1, 1, 0)), waiting])
                } else {
                    mixed(&[waiting])
                }
            })
            .collect();
        for (handle, buffer) in held.iter().enumerate().take(HOLDERS) {
            assert!(holder.accept(handle, buffer, at, &mut |_, _, _| {}, &mut |_| {}));
        }
        assert!(!holder.can_accept());
        let last = &held[HOLDERS];
        assert!(!holder.accept(HOLDERS, last, at, &mut |_, _, _| {}, &mut |_| {}));
    }
}
