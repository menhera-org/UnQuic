//! Moves completed striped frames to their stripe's writer and restores
//! their order there (design section 13.1, item 6).
//!
//! A lane owner packs completed striped frames, each still starting with its
//! stripe tag, into a transfer buffer as `[length: u16 big-endian][frame]`
//! entries and hands the buffer to the stripe's writer. The [`Writer`] keeps
//! a [`reorder::Window`] per active stripe, holds each buffer until every
//! frame in it has been delivered or dropped, and reports both through
//! callbacks, so nothing allocates.

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

/// Appends one frame to a transfer buffer at `*used`; false, changing
/// nothing, when it does not fit or carries no stripe tag.
pub fn pack(buffer: &mut [u8], used: &mut usize, frame: &[u8]) -> bool {
    let Ok(length) = u16::try_from(frame.len()) else {
        return false;
    };
    let end = *used + 2 + frame.len();
    if frame.len() < STRIPE_TAG_LEN || end > buffer.len() {
        return false;
    }
    buffer[*used..*used + 2].copy_from_slice(&length.to_be_bytes());
    buffer[*used + 2..end].copy_from_slice(frame);
    *used = end;
    true
}

/// The frames packed in the first `used` bytes, as ranges of the buffer.
/// Stops at the first malformed entry.
pub fn entries(buffer: &[u8]) -> impl Iterator<Item = Range<usize>> + '_ {
    let mut at = 0;
    std::iter::from_fn(move || {
        let length = usize::from(u16::from_be_bytes(*buffer.get(at..)?.first_chunk()?));
        let range = at + 2..at + 2 + length;
        buffer.get(range.clone())?;
        at = range.end;
        Some(range)
    })
}

#[derive(Clone, Copy)]
struct Item {
    holder: u8,
    start: u32,
    end: u32,
}

struct Stripe {
    number: u16,
    last: Duration,
    window: Window<Item>,
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
    stripes: [Option<Stripe>; STRIPES],
    holders: [Option<(H, u32)>; HOLDERS],
    totals: Totals,
}

impl<H: Copy> Default for Writer<H> {
    fn default() -> Self {
        Self::new()
    }
}

impl<H: Copy> Writer<H> {
    /// A writer tracking no stripe and holding no buffer.
    pub fn new() -> Self {
        Self {
            stripes: std::array::from_fn(|_| None),
            holders: [None; HOLDERS],
            totals: Totals::default(),
        }
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
                        stripe.window = Window::new();
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
        self.stripes[index] = Some(Stripe {
            number,
            last: now,
            window: Window::new(),
        });
        Some(index)
    }

    /// Takes a transfer buffer's frames at `now`, writing at once those that
    /// are in sequence or cannot wait. `bytes` must stay readable through
    /// `handle` until `release` returns it. False, taking nothing, when no
    /// holder slot is free.
    pub fn accept(
        &mut self,
        handle: H,
        bytes: &[u8],
        now: Duration,
        deliver: &mut impl FnMut(H, Range<usize>),
        release: &mut impl FnMut(H),
    ) -> bool {
        let Some(holder) = self.holders.iter().position(Option::is_none) else {
            return false;
        };
        // Held while this call runs, so deliveries cannot release it early.
        self.holders[holder] = Some((handle, 1));
        for range in entries(bytes) {
            let frame = &bytes[range.clone()];
            let Some((number, sequence)) = stripe_tag(frame) else {
                self.totals.dropped += 1;
                continue;
            };
            let item = Item {
                holder: holder as u8,
                start: range.start as u32,
                end: range.end as u32,
            };
            let Some(index) = self.stripe(number, now) else {
                self.totals.unordered += 1;
                deliver(handle, range);
                continue;
            };
            while let Some(stripe) = self.stripes[index].as_mut() {
                match stripe.window.insert(sequence, item, now) {
                    Placement::Held => {
                        if let Some((_, references)) = &mut self.holders[holder] {
                            *references += 1;
                        }
                    }
                    Placement::Late => {
                        self.totals.unordered += 1;
                        deliver(handle, range.clone());
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
        deliver: &mut impl FnMut(H, Range<usize>),
        release: &mut impl FnMut(H),
    ) {
        while let Some(item) = self.stripes[index]
            .as_mut()
            .and_then(|stripe| stripe.window.drain())
        {
            if let Some((handle, _)) = self.holders[usize::from(item.holder)] {
                self.totals.delivered += 1;
                deliver(handle, item.start as usize..item.end as usize);
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
        deliver: &mut impl FnMut(H, Range<usize>),
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

    fn batch(frames: &[Vec<u8>]) -> Vec<u8> {
        let mut buffer = vec![0; 4096];
        let mut used = 0;
        for frame in frames {
            assert!(pack(&mut buffer, &mut used, frame));
        }
        buffer.truncate(used);
        buffer
    }

    #[test]
    fn packing_round_trips_and_refuses_what_does_not_fit() {
        let frames = [frame(1, 1, 1), frame(1, 2, 2)];
        let buffer = batch(&frames);
        let read: Vec<_> = entries(&buffer)
            .map(|range| buffer[range].to_vec())
            .collect();
        assert_eq!(read, frames);
        let mut small = [0; 60];
        let mut used = 0;
        assert!(pack(&mut small, &mut used, &frame(1, 1, 1)));
        assert!(!pack(&mut small, &mut used, &frame(1, 2, 2)));
        assert!(!pack(&mut small, &mut used, &[0; 7]), "no room for a tag");
        assert_eq!(used, 50);
        // A truncated entry ends the walk.
        assert_eq!(entries(&buffer[..buffer.len() - 1]).count(), 1);
    }

    #[test]
    fn frames_spread_over_buffers_leave_in_order_and_buffers_return_once_empty() {
        let mut writer = Writer::<u8>::new();
        let mut written = Vec::new();
        let mut released = Vec::new();
        let buffers = [
            batch(&[frame(3, 2, 2), frame(3, 4, 4)]),
            batch(&[frame(3, 1, 1), frame(3, 3, 3)]),
        ];
        let mut deliver = |handle: u8, range: Range<usize>| {
            written.push(buffers[usize::from(handle)][range][STRIPE_TAG_LEN]);
        };
        let mut release = |handle: u8| released.push(handle);
        assert!(writer.accept(0, &buffers[0], Duration::ZERO, &mut deliver, &mut release));
        assert_eq!(writer.holding(), 1);
        assert!(writer.accept(1, &buffers[1], Duration::ZERO, &mut deliver, &mut release));
        assert_eq!(written, [1, 2, 3, 4]);
        // Frame 4, buffer 0's last, leaves during the second call, before
        // that call lets go of buffer 1.
        assert_eq!(released, [0, 1]);
        assert_eq!(writer.holding(), 0);
        assert_eq!(writer.totals().delivered, 4);
    }

    #[test]
    fn gaps_expire_late_frames_still_leave_and_idle_stripes_restart() {
        let mut writer = Writer::<u8>::new();
        let mut written = Vec::new();
        let mut released = 0;
        let limit = Duration::from_millis(5);
        let buffers = [
            batch(&[frame(9, 2, 2), frame(9, 3, 3)]),
            batch(&[frame(9, 1, 1), frame(9, 3, 3)]),
            batch(&[frame(9, 1, 7)]),
        ];
        let mut deliver = |handle: u8, range: Range<usize>| {
            written.push(buffers[usize::from(handle)][range][STRIPE_TAG_LEN]);
        };
        let mut release = |_| released += 1;
        writer.accept(0, &buffers[0], Duration::ZERO, &mut deliver, &mut release);
        let due = writer.expire(Duration::from_millis(1), limit, &mut deliver, &mut release);
        assert_eq!(due, Some(limit));
        assert_eq!(
            writer.expire(limit, limit, &mut deliver, &mut release),
            None
        );
        // The missing first frame arrives late; a repeat of 3 is a duplicate.
        writer.accept(1, &buffers[1], limit, &mut deliver, &mut release);
        assert_eq!(writer.totals().unordered, 1);
        assert_eq!(writer.totals().dropped, 1);
        // After a second of silence the number may start over at 1.
        writer.accept(2, &buffers[2], limit + IDLE, &mut deliver, &mut release);
        assert_eq!(written, [2, 3, 1, 7]);
        assert_eq!(writer.holding(), 0);
        assert_eq!(released, 3);
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
        let limit = Duration::from_millis(5);
        // Three stripes of 400 frames each, shuffled locally, some lost or repeated.
        let mut arrivals = Vec::new();
        for number in 1..=3_u16 {
            for sequence in 1..=400_u64 {
                if random(40) != 0 {
                    arrivals.push((number, sequence));
                }
                if random(40) == 0 {
                    arrivals.push((number, sequence));
                }
            }
        }
        for index in 0..arrivals.len() {
            let other = (index + random(24) as usize).min(arrivals.len() - 1);
            arrivals.swap(index, other);
        }
        let buffers: Vec<Vec<u8>> = arrivals
            .chunks(1 + random(6) as usize)
            .map(|chunk| {
                let frames: Vec<_> = chunk
                    .iter()
                    .map(|&(number, sequence)| frame(number, sequence, sequence as u8))
                    .collect();
                batch(&frames)
            })
            .collect();
        let mut writer = Writer::<usize>::new();
        let mut seen = std::collections::HashSet::new();
        let mut last = [0_u64; 4];
        let mut ordered_breaks = 0;
        let mut released = vec![0_u8; buffers.len()];
        let mut now = Duration::ZERO;
        {
            let mut deliver = |handle: usize, range: Range<usize>| {
                let (number, sequence) = stripe_tag(&buffers[handle][range]).unwrap();
                assert!(seen.insert((number, sequence)), "{number}/{sequence} twice");
                if sequence < last[usize::from(number)] {
                    ordered_breaks += 1;
                }
                last[usize::from(number)] = last[usize::from(number)].max(sequence);
            };
            let mut release = |handle: usize| released[handle] += 1;
            for (handle, buffer) in buffers.iter().enumerate() {
                now += Duration::from_micros(random(400));
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
    }

    #[test]
    fn a_full_stripe_table_or_holder_table_degrades_without_loss() {
        let mut writer = Writer::<usize>::new();
        let mut written = 0;
        let mut released = 0;
        // Fill every stripe slot with a held frame (sequence 2, waiting for 1).
        let buffers: Vec<_> = (0..=STRIPES)
            .map(|number| batch(&[frame(number as u16, 2, 0)]))
            .collect();
        let mut deliver = |_: usize, _: Range<usize>| written += 1;
        let mut release = |_: usize| released += 1;
        for (handle, buffer) in buffers.iter().enumerate().take(STRIPES) {
            assert!(writer.accept(handle, buffer, Duration::ZERO, &mut deliver, &mut release));
        }
        // A seventeenth stripe finds no slot: its frame goes out unordered.
        assert!(writer.accept(
            STRIPES,
            &buffers[STRIPES],
            Duration::ZERO,
            &mut deliver,
            &mut release
        ));
        assert_eq!(writer.totals().unordered, 1);
        assert_eq!(writer.holding(), STRIPES);
        assert_eq!((written, released), (1, 1));
        // With every holder slot taken, accept refuses and takes nothing.
        let mut holder = Writer::<usize>::new();
        // Each buffer holds one frame waiting behind the missing sequence 1.
        let held: Vec<_> = (0..=HOLDERS)
            .map(|handle| batch(&[frame(1, handle as u64 + 2, 0)]))
            .collect();
        for (handle, buffer) in held.iter().enumerate().take(HOLDERS) {
            assert!(holder.accept(handle, buffer, Duration::ZERO, &mut |_, _| {}, &mut |_| {}));
        }
        assert!(!holder.can_accept());
        let last = &held[HOLDERS];
        assert!(!holder.accept(HOLDERS, last, Duration::ZERO, &mut |_, _| {}, &mut |_| {}));
    }
}
