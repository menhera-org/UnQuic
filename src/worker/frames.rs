//! Owner-local payload storage and compact frame handles.
use super::{BUFFERS, Buffer};
use crate::{loss::FrameRecovery, pack, tap::Segments};
use std::{io, mem::size_of, ops::Range, time::Duration};

pub(super) const CAPACITY: usize = 8192;
pub(super) const HEADER_BYTES: usize = 128;

const END: usize = CAPACITY;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Link {
    next: u16,
    previous: u16,
}

impl Link {
    const EMPTY: Self = Self {
        next: u16::MAX,
        previous: u16::MAX,
    };
}

pub(super) struct Queue {
    head: usize,
    tail: usize,
    len: usize,
}

impl Default for Queue {
    fn default() -> Self {
        Self {
            head: END,
            tail: END,
            len: 0,
        }
    }
}

impl Queue {
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn front(&self) -> Option<usize> {
        (self.head != END).then_some(self.head)
    }

    #[cfg(test)]
    pub fn iter<'a>(&self, store: &'a Store) -> impl Iterator<Item = usize> + 'a {
        std::iter::successors(self.front(), move |&index| {
            store
                .links
                .get(index)
                .map(|link| usize::from(link.next))
                .filter(|&next| next < END)
        })
        .take(self.len)
    }

    pub fn push_back(&mut self, store: &mut Store, index: usize) -> io::Result<()> {
        if store.frame(index).is_none() {
            return Err(io::Error::other("missing queued transmit frame"));
        }
        self.insert(&mut store.links, index)
    }

    pub fn push_front(&mut self, store: &mut Store, index: usize) -> io::Result<()> {
        if self.is_empty() {
            return self.push_back(store, index);
        }
        if store.frame(index).is_none()
            || store.links.get(index) != Some(&Link::EMPTY)
            || self.len >= CAPACITY
            || store
                .links
                .get(self.head)
                .is_none_or(|link| usize::from(link.previous) != END)
        {
            return Err(io::Error::other("invalid transmit queue prepend"));
        }
        store.links[index] = Link {
            next: self.head as u16,
            previous: END as u16,
        };
        store.links[self.head].previous = index as u16;
        self.head = index;
        self.len += 1;
        Ok(())
    }

    pub fn pop_front(&mut self, store: &mut Store) -> io::Result<Option<usize>> {
        let Some(index) = self.front() else {
            return Ok(None);
        };
        if store.frame(index).is_none() || !self.remove(&mut store.links, index)? {
            return Err(io::Error::other("missing queued transmit frame"));
        }
        Ok(Some(index))
    }

    fn insert(&mut self, links: &mut [Link], index: usize) -> io::Result<()> {
        if index >= CAPACITY
            || links.get(index) != Some(&Link::EMPTY)
            || self.len >= CAPACITY
            || (self.len != 0
                && links
                    .get(self.tail)
                    .is_none_or(|link| usize::from(link.next) != END))
        {
            return Err(io::Error::other("invalid transmit queue insertion"));
        }
        links[index] = Link {
            next: END as u16,
            previous: if self.len == 0 {
                END as u16
            } else {
                self.tail as u16
            },
        };
        if self.len == 0 {
            self.head = index;
        } else {
            links[self.tail].next = index as u16;
        }
        self.tail = index;
        self.len += 1;
        Ok(())
    }

    // The caller selects the queue from the frame's owner; no membership search.
    fn remove(&mut self, links: &mut [Link], index: usize) -> io::Result<bool> {
        let Some(&link) = links.get(index).filter(|&&link| link != Link::EMPTY) else {
            return Ok(false);
        };
        let (previous, next) = (usize::from(link.previous), usize::from(link.next));
        if self.len == 0
            || previous > END
            || next > END
            || (previous == END && self.head != index)
            || (next == END && self.tail != index)
            || (previous != END
                && links
                    .get(previous)
                    .is_none_or(|link| usize::from(link.next) != index))
            || (next != END
                && links
                    .get(next)
                    .is_none_or(|link| usize::from(link.previous) != index))
        {
            return Err(io::Error::other("invalid transmit queue removal"));
        }
        if previous == END {
            self.head = next;
        } else {
            links[previous].next = next as u16;
        }
        if next == END {
            self.tail = previous;
        } else {
            links[next].previous = previous as u16;
        }
        links[index] = Link::EMPTY;
        self.len -= 1;
        Ok(true)
    }
}

pub(super) struct Frame {
    pub id: u64,
    pub deadline: Duration,
    pub source: usize,
    pub range: Range<usize>,
    pub offset: usize,
    pub lane: usize,
    pub header_len: usize,
    pub header: [u8; HEADER_BYTES],
}

impl Frame {
    pub fn length(&self) -> usize {
        self.header_len + self.range.len()
    }
}

struct Source {
    buffer: Box<Buffer>,
    references: usize,
    pinned: bool,
}

pub(super) struct Store {
    sources: Vec<Option<Source>>,
    free_sources: Vec<usize>,
    frames: Vec<Option<Frame>>,
    recovery: Vec<FrameRecovery>,
    free_frames: Vec<usize>,
    links: Vec<Link>,
    // Min-heap of (deadline key, frame slot); keys live here so sifts stay in one array.
    expiry: Vec<(u64, u16)>,
    expiry_position: Vec<u16>,
}

/// Heap key: nanoseconds, saturating; only deadlines centuries away share a key.
fn expiry_key(deadline: Duration) -> u64 {
    u64::try_from(deadline.as_nanos()).unwrap_or(u64::MAX)
}

impl Store {
    pub const fn storage_bytes(capacity: usize) -> usize {
        BUFFERS * (size_of::<Option<Source>>() + size_of::<usize>())
            + capacity
                * (size_of::<Option<Frame>>()
                    + size_of::<FrameRecovery>()
                    + size_of::<usize>()
                    + size_of::<Link>()
                    + size_of::<(u64, u16)>()
                    + size_of::<u16>())
    }

    pub fn new(capacity: usize) -> io::Result<Self> {
        if capacity > CAPACITY {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let mut recovery = Vec::new();
        recovery
            .try_reserve_exact(capacity)
            .map_err(io::Error::other)?;
        for _ in 0..capacity {
            recovery.push(FrameRecovery::new(14).ok_or(io::ErrorKind::InvalidData)?);
        }
        let sources = BUFFERS;
        Ok(Self {
            sources: std::iter::repeat_with(|| None).take(sources).collect(),
            free_sources: (0..sources).rev().collect(),
            frames: std::iter::repeat_with(|| None).take(capacity).collect(),
            recovery,
            free_frames: (0..capacity).rev().collect(),
            links: vec![Link::EMPTY; capacity],
            expiry: Vec::with_capacity(capacity),
            expiry_position: vec![END as u16; capacity],
        })
    }

    pub fn available(&self) -> usize {
        self.free_frames.len()
    }

    /// Sources and slabs holding a buffer, for diagnostics only.
    #[cfg(feature = "trace")]
    pub fn trace_sources(&self) -> usize {
        self.sources.len() - self.free_sources.len()
    }

    pub fn source(&self, index: usize) -> Option<&Buffer> {
        Some(&self.sources.get(index)?.as_ref()?.buffer)
    }

    pub fn source_mut(&mut self, index: usize) -> Option<&mut Buffer> {
        Some(&mut self.sources.get_mut(index)?.as_mut()?.buffer)
    }

    pub fn insert_source(&mut self, buffer: Box<Buffer>) -> Result<usize, Box<Buffer>> {
        let Some(&index) = self.free_sources.last() else {
            return Err(buffer);
        };
        let Some(slot) = self.sources.get_mut(index) else {
            return Err(buffer);
        };
        self.free_sources.pop();
        *slot = Some(Source {
            buffer,
            references: 0,
            pinned: true,
        });
        Ok(index)
    }

    pub fn frame(&self, index: usize) -> Option<&Frame> {
        self.frames.get(index)?.as_ref()
    }

    pub fn frame_mut(&mut self, index: usize) -> Option<&mut Frame> {
        self.frames.get_mut(index)?.as_mut()
    }

    pub fn recovery(&self, index: usize) -> Option<&FrameRecovery> {
        self.frame(index)?;
        self.recovery.get(index)
    }

    pub fn recovery_mut(&mut self, index: usize) -> Option<&mut FrameRecovery> {
        self.frame(index)?;
        self.recovery.get_mut(index)
    }

    pub fn insert_frame(&mut self, frame: Frame) -> Option<usize> {
        let &index = self.free_frames.last()?;
        let slot = self.frames.get_mut(index)?;
        let source = self.sources.get_mut(frame.source)?.as_mut()?;
        if frame.header_len > frame.header.len()
            || frame.range.start > frame.range.end
            || frame.range.end > source.buffer.length
        {
            return None;
        }
        let length = frame.header_len.checked_add(frame.range.len())?;
        let references = source.references.checked_add(1)?;
        if frame.offset >= length || !self.recovery.get_mut(index)?.reset(length) {
            return None;
        }
        source.references = references;
        self.free_frames.pop();
        self.expiry_position[index] = self.expiry.len() as u16;
        self.expiry.push((expiry_key(frame.deadline), index as u16));
        *slot = Some(frame);
        self.sift_expiry(self.expiry.len() - 1);
        Some(index)
    }

    pub fn input(&self, index: usize) -> Option<pack::FrameInput<'_>> {
        let frame = self.frame(index)?;
        let source = self.source(frame.source)?;
        Some(pack::FrameInput {
            id: frame.id,
            header: frame.header.get(..frame.header_len)?,
            bytes: source.bytes.get(frame.range.clone())?,
            offset: frame.offset,
            end: frame.length(),
        })
    }

    pub fn unused(&self, index: usize) -> bool {
        self.sources
            .get(index)
            .and_then(Option::as_ref)
            .is_some_and(|s| s.references == 0)
    }

    pub fn producing(&self, index: usize) -> bool {
        self.sources
            .get(index)
            .and_then(Option::as_ref)
            .is_some_and(|s| s.pinned)
    }

    fn recycle(&mut self, index: usize) -> Option<Box<Buffer>> {
        let slot = self.sources.get_mut(index)?;
        let source = slot.as_ref()?;
        if source.pinned || source.references != 0 {
            return None;
        }
        let source = slot.take()?;
        self.free_sources.push(index);
        Some(source.buffer)
    }

    pub fn unpin(&mut self, index: usize) -> Option<Box<Buffer>> {
        self.sources.get_mut(index)?.as_mut()?.pinned = false;
        self.recycle(index)
    }

    pub fn release(&mut self, index: usize) -> Option<Box<Buffer>> {
        if self.links.get(index) != Some(&Link::EMPTY) {
            return None;
        }
        self.untrack_expiry(index).ok()?;
        let frame = self.frames.get_mut(index)?.take()?;
        self.free_frames.push(index);
        let source = self.sources.get_mut(frame.source)?.as_mut()?;
        source.references = source.references.checked_sub(1)?;
        self.recycle(frame.source)
    }

    pub fn deadline(&self) -> Option<Duration> {
        self.frame(usize::from(self.expiry.first()?.1))
            .map(|frame| frame.deadline)
    }

    /// Removes one due frame from the heap, leaving payload/ready ownership intact.
    pub fn pop_expired(&mut self, now: Duration) -> io::Result<Option<usize>> {
        if self.deadline().is_none_or(|deadline| deadline > now) {
            return Ok(None);
        }
        let index = usize::from(self.expiry[0].1);
        self.untrack_expiry(index)?;
        Ok(Some(index))
    }

    pub fn unqueue(&mut self, queue: &mut Queue, index: usize) -> io::Result<bool> {
        queue.remove(&mut self.links, index)
    }

    #[cfg(test)]
    fn expiry_before(&self, a: usize, b: usize) -> bool {
        self.expiry[a].0 < self.expiry[b].0
    }

    fn place_expiry(&mut self, position: usize, entry: (u64, u16)) {
        self.expiry[position] = entry;
        self.expiry_position[usize::from(entry.1)] = position as u16;
    }

    // At most 13 levels, including out-of-order cross-worker admissions. The moved
    // entry waits in a hole; the smaller child is chosen without a branch, since
    // FIFO releases sink the newest deadline from the root past near-equal pairs.
    fn sift_expiry(&mut self, mut position: usize) {
        let Some(&entry) = self.expiry.get(position) else {
            return;
        };
        let start = position;
        while position != 0 {
            let parent = (position - 1) / 2;
            let above = self.expiry[parent];
            if entry.0 >= above.0 {
                break;
            }
            self.place_expiry(position, above);
            position = parent;
        }
        if position == start {
            let len = self.expiry.len();
            loop {
                let mut child = position * 2 + 1;
                if child >= len {
                    break;
                }
                if child + 1 < len {
                    child += usize::from(self.expiry[child + 1].0 < self.expiry[child].0);
                }
                let below = self.expiry[child];
                if below.0 >= entry.0 {
                    break;
                }
                self.place_expiry(position, below);
                position = child;
            }
        }
        self.place_expiry(position, entry);
    }

    fn untrack_expiry(&mut self, index: usize) -> io::Result<()> {
        let Some(&position) = self.expiry_position.get(index) else {
            return Ok(());
        };
        let position = usize::from(position);
        if position == END {
            return Ok(());
        }
        if self.expiry.get(position).map(|&(_, slot)| slot) != Some(index as u16) {
            return Err(io::Error::other("invalid transmit expiry position"));
        }
        let last = self.expiry.pop().ok_or(io::ErrorKind::InvalidData)?;
        self.expiry_position[index] = END as u16;
        if position < self.expiry.len() {
            self.place_expiry(position, last);
            self.sift_expiry(position);
        }
        Ok(())
    }

    #[cfg(test)]
    pub fn set_deadline(&mut self, index: usize, deadline: Duration) {
        self.frame_mut(index).unwrap().deadline = deadline;
        let position = usize::from(self.expiry_position[index]);
        assert!(position < self.expiry.len());
        self.expiry[position].0 = expiry_key(deadline);
        self.sift_expiry(position);
    }

    /// Appends a complete frame; dead ranges remain charged until the slab recycles.
    pub fn append(
        &mut self,
        slab: usize,
        input: usize,
        segments: Option<&mut Segments>,
    ) -> Option<Range<usize>> {
        let [slab, input] = self.sources.get_disjoint_mut([slab, input]).ok()?;
        let slab = &mut slab.as_mut()?.buffer;
        let input = &input.as_ref()?.buffer;
        let start = slab.length;
        let output = slab.bytes.get_mut(start..)?;
        let length = if let Some(segments) = segments {
            let length = segments.write(input.bytes.get(..input.length)?, output);
            if length == 0 {
                return None;
            }
            length
        } else {
            output
                .get_mut(..input.length)?
                .copy_from_slice(input.bytes.get(..input.length)?);
            input.length
        };
        slab.length += length;
        Some(start..slab.length)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MAX_FRAME_LEN;

    fn buffer(length: usize) -> Box<Buffer> {
        Box::new(Buffer {
            origin: 0,
            rx_counted: false,
            tx_reserved: false,
            segments: None,
            rx_reserved: false,
            offset: 0,
            segment_size: 0,
            session: 0,
            tag: 1,
            lane: 0,
            length,
            deadline: Duration::from_millis(100),
            bytes: [7; 65536],
        })
    }

    fn frame(source: usize, range: Range<usize>, id: u64) -> Frame {
        Frame {
            id,
            source,
            range,
            offset: 0,
            lane: 0,
            deadline: Duration::from_millis(id),
            header_len: 0,
            header: [0; HEADER_BYTES],
        }
    }

    #[test]
    fn empty_store_and_oversized_reservation_do_not_create_frame_slots() {
        assert!(Store::new(CAPACITY + 1).is_err());
        let mut store = Store::new(0).unwrap();
        let source = store.insert_source(buffer(60)).ok().unwrap();
        assert_eq!(store.available(), 0);
        assert!(store.insert_frame(frame(source, 0..60, 1)).is_none());
        assert!(store.unpin(source).is_some());
        assert!(store.frames.is_empty());
        assert!(store.links.is_empty());
        assert_eq!(store.recovery.capacity(), 0);
        assert!(store.recovery(0).is_none());
        assert!(store.recovery_mut(0).is_none());
    }

    #[test]
    fn recovery_storage_is_stable_and_invalid_admission_preserves_ownership() {
        let mut store = Store::new(1).unwrap();
        let source = store.insert_source(buffer(MAX_FRAME_LEN + 1)).ok().unwrap();
        let index = store
            .insert_frame(frame(source, 0..MAX_FRAME_LEN, 1))
            .unwrap();
        let address = store.recovery(index).unwrap() as *const FrameRecovery;
        let recovery = store.recovery_mut(index).unwrap();
        assert!(recovery.submitted(0..MAX_FRAME_LEN, 0));
        assert_eq!(recovery.acknowledge(0..60, 0), Some(60));
        assert!(store.release(index).is_none());
        assert!(store.recovery(index).is_none());
        assert!(store.recovery_mut(index).is_none());

        for length in [0, 13, MAX_FRAME_LEN + 1] {
            assert!(store.insert_frame(frame(source, 0..length, 2)).is_none());
            assert_eq!(store.available(), 1);
            assert_eq!(store.sources[source].as_ref().unwrap().references, 0);
            assert_eq!(store.recovery[index].sent(), MAX_FRAME_LEN);
            assert_eq!(store.recovery[index].remaining(), MAX_FRAME_LEN - 60);
        }
        // Reference overflow must reject before resetting a recycled recovery slot.
        store.sources[source].as_mut().unwrap().references = usize::MAX;
        assert!(store.insert_frame(frame(source, 0..60, 2)).is_none());
        assert_eq!(store.recovery[index].remaining(), MAX_FRAME_LEN - 60);
        store.sources[source].as_mut().unwrap().references = 0;

        for length in [60, MAX_FRAME_LEN] {
            assert_eq!(store.insert_frame(frame(source, 0..length, 3)), Some(index));
            let recovery = store.recovery_mut(index).unwrap();
            assert_eq!(recovery as *const FrameRecovery, address);
            assert_eq!(recovery.sent(), 0);
            assert_eq!(recovery.remaining(), length);
            assert!(recovery.submitted(0..length, 0));
            assert!(recovery.lost(0..length, 0));
            assert_eq!(recovery.next_repair(length), Some((0..length, 1)));
            assert!(recovery.submitted(0..length, 1));
            assert_eq!(recovery.acknowledge(0..length, 0), Some(length));
            assert!(store.release(index).is_none());
        }
        assert!(store.unpin(source).is_some());
        assert_eq!(store.available(), 1);
    }

    #[test]
    fn recovery_storage_is_charged_for_exact_frame_capacity() {
        let per_frame = size_of::<Option<Frame>>()
            + size_of::<FrameRecovery>()
            + size_of::<usize>()
            + size_of::<Link>()
            + size_of::<(u64, u16)>()
            + size_of::<u16>();
        for capacity in [1, 17, CAPACITY] {
            assert_eq!(
                Store::storage_bytes(capacity) - Store::storage_bytes(0),
                capacity * per_frame
            );
        }
        assert_eq!(size_of::<FrameRecovery>(), 3392);
        assert!(size_of::<Frame>() < 256);
    }

    #[test]
    fn interleaved_queues_preserve_order_and_recycled_indices() {
        let mut store = Store::new(CAPACITY).unwrap();
        let source = store.insert_source(buffer(60)).ok().unwrap();
        let mut a = Queue::default();
        let mut b = Queue::default();
        let ids: Vec<_> = (1..=6)
            .map(|id| store.insert_frame(frame(source, 0..60, id)).unwrap())
            .collect();
        for &index in &ids[..4] {
            if index % 2 == 0 { &mut a } else { &mut b }
                .push_back(&mut store, index)
                .unwrap();
        }
        assert!(b.push_back(&mut store, ids[0]).is_err());
        assert!(a.push_back(&mut store, CAPACITY).is_err());
        assert_eq!(a.pop_front(&mut store).unwrap(), Some(ids[0]));
        a.push_back(&mut store, ids[4]).unwrap();
        b.push_back(&mut store, ids[5]).unwrap();
        assert_eq!(a.iter(&store).collect::<Vec<_>>(), [ids[2], ids[4]]);
        assert_eq!(b.iter(&store).collect::<Vec<_>>(), [ids[1], ids[3], ids[5]]);
        assert!(store.release(ids[0]).is_none());
        let recycled = store.insert_frame(frame(source, 0..60, 7)).unwrap();
        assert_eq!(recycled, ids[0]);
        a.push_back(&mut store, recycled).unwrap();
        assert_eq!(
            a.iter(&store).collect::<Vec<_>>(),
            [ids[2], ids[4], recycled]
        );
        for queue in [&mut a, &mut b] {
            let expected: Vec<_> = queue.iter(&store).collect();
            for index in expected {
                assert_eq!(queue.front(), Some(index));
                assert_eq!(queue.pop_front(&mut store).unwrap(), Some(index));
                assert!(store.release(index).is_none());
            }
            assert!(queue.is_empty());
            assert_eq!(queue.len(), 0);
            assert_eq!(queue.front(), None);
            assert_eq!(queue.pop_front(&mut store).unwrap(), None);
        }
        assert!(store.unpin(source).is_some());
        assert_eq!(store.available(), CAPACITY);
    }

    #[test]
    fn queue_capacity_operations_change_only_head_and_tail_links() {
        let mut store = Store::new(CAPACITY).unwrap();
        let source = store.insert_source(buffer(60)).ok().unwrap();
        let mut queue = Queue::default();
        for id in 1..CAPACITY {
            let index = store.insert_frame(frame(source, 0..60, id as u64)).unwrap();
            queue.push_back(&mut store, index).unwrap();
        }
        let index = store
            .insert_frame(frame(source, 0..60, CAPACITY as u64))
            .unwrap();
        let before = store.links.clone();
        queue.push_back(&mut store, index).unwrap();
        let changes: Vec<_> = before
            .iter()
            .zip(&store.links)
            .enumerate()
            .filter_map(|(i, (a, b))| (a != b).then_some(i))
            .collect();
        assert_eq!(changes, [CAPACITY - 2, CAPACITY - 1]);
        assert_eq!(queue.len(), CAPACITY);
        assert_eq!(store.available(), 0);
        assert!(queue.push_back(&mut store, index).is_err());
        assert!(store.release(0).is_none());
        assert!(store.frame(0).is_some());
        let before = store.links.clone();
        assert_eq!(queue.pop_front(&mut store).unwrap(), Some(0));
        let changes: Vec<_> = before
            .iter()
            .zip(&store.links)
            .enumerate()
            .filter_map(|(i, (a, b))| (a != b).then_some(i))
            .collect();
        assert_eq!(changes, [0, 1]);
        let before = store.links.clone();
        queue.push_front(&mut store, 0).unwrap();
        let changes: Vec<_> = before
            .iter()
            .zip(&store.links)
            .enumerate()
            .filter_map(|(i, (a, b))| (a != b).then_some(i))
            .collect();
        assert_eq!(changes, [0, 1]);
        assert_eq!(queue.front(), Some(0));
        assert_eq!(queue.len(), CAPACITY);
        assert!(queue.push_front(&mut store, 0).is_err());
        assert_eq!(queue.pop_front(&mut store).unwrap(), Some(0));
        assert!(store.release(0).is_none());

        for expected in 1..CAPACITY {
            let index = queue.pop_front(&mut store).unwrap().unwrap();
            assert_eq!(index, expected);
            assert!(store.release(index).is_none());
        }
        assert!(queue.is_empty());
        assert!(store.unpin(source).is_some());
        assert_eq!(store.available(), CAPACITY);
    }

    #[test]
    fn source_pins_and_frame_references_bound_capacity_and_recycle_once() {
        for (capacity, close_first) in [1, 17, CAPACITY]
            .into_iter()
            .flat_map(|capacity| [false, true].map(|close_first| (capacity, close_first)))
        {
            let mut store = Store::new(capacity).unwrap();
            let source = store.insert_source(buffer(60)).ok().unwrap();
            let pointer = store.source(source).unwrap().bytes.as_ptr();
            let mut ids = Vec::new();
            for id in 1..=capacity {
                ids.push(store.insert_frame(frame(source, 0..60, id as u64)).unwrap());
            }
            assert_eq!(store.available(), 0);
            assert!(store.insert_frame(frame(source, 0..60, 9000)).is_none());
            if close_first {
                assert!(store.unpin(source).is_none());
            }
            for index in ids[..capacity - 1].iter().rev() {
                assert!(store.release(*index).is_none());
                assert!(store.release(*index).is_none());
            }
            let last = ids[capacity - 1];
            assert_eq!(store.input(last).unwrap().bytes.as_ptr(), pointer);
            let released = store.release(last);
            let released = if close_first {
                released
            } else {
                assert!(released.is_none());
                store.unpin(source)
            }
            .unwrap();
            assert_eq!(released.bytes.as_ptr(), pointer);
            assert_eq!(store.available(), capacity);
            assert_eq!(store.free_sources.len(), BUFFERS);
            assert!(store.unpin(source).is_none());
        }
    }

    fn check_expiry(store: &Store) {
        let mut expected: Vec<_> = store
            .frames
            .iter()
            .enumerate()
            .filter_map(|(index, frame)| {
                frame
                    .as_ref()
                    .filter(|_| store.expiry_position[index] != END as u16)
                    .map(|frame| (frame.deadline, index))
            })
            .collect();
        expected.sort_unstable();
        assert_eq!(store.deadline(), expected.first().map(|entry| entry.0));
        assert_eq!(store.expiry.len(), expected.len());
        for (position, &(key, index)) in store.expiry.iter().enumerate() {
            assert_eq!(
                usize::from(store.expiry_position[usize::from(index)]),
                position
            );
            // The inline key must match the frame it orders.
            assert_eq!(
                expiry_key(store.frame(usize::from(index)).unwrap().deadline),
                key
            );
            if position != 0 {
                assert!(!store.expiry_before(position, (position - 1) / 2));
            }
        }
    }

    #[test]
    fn expiry_orders_arbitrary_deadlines_and_survives_removal_and_slot_reuse() {
        let mut store = Store::new(CAPACITY).unwrap();
        let source = store.insert_source(buffer(60)).ok().unwrap();
        // A permutation mixes old and new epochs, including equal deadlines.
        for id in 0..CAPACITY {
            let mut input = frame(source, 0..60, id as u64 + 1);
            input.deadline = Duration::from_nanos(((id * 4051) % CAPACITY / 4) as u64);
            store.insert_frame(input).unwrap();
        }
        check_expiry(&store);
        assert!(store.pop_expired(Duration::ZERO).unwrap().is_some());
        for index in (0..CAPACITY).step_by(3) {
            store.release(index);
            let mut input = frame(source, 0..60, 10000 + index as u64);
            input.deadline = if index % 2 == 0 {
                Duration::ZERO
            } else {
                Duration::MAX
            };
            assert_eq!(store.insert_frame(input), Some(index));
            if index % 127 == 0 {
                check_expiry(&store);
            }
        }
        // Include an already-untracked frame from pop_expired above.
        for index in 0..CAPACITY {
            if store.expiry_position[index] == END as u16 {
                store.release(index);
            }
        }
        let mut previous = Duration::ZERO;
        let count = store.expiry.len();
        assert!(store.unpin(source).is_none());
        for remaining in (0..count).rev() {
            let index = store.pop_expired(Duration::MAX).unwrap().unwrap();
            let deadline = store.frame(index).unwrap().deadline;
            assert!(previous <= deadline);
            previous = deadline;
            assert_eq!(store.release(index).is_some(), remaining == 0);
            if remaining % 127 == 0 {
                check_expiry(&store);
            }
        }
        assert_eq!(store.deadline(), None);
        assert_eq!(store.pop_expired(Duration::MAX).unwrap(), None);
        assert_eq!(store.available(), CAPACITY);
    }

    #[test]
    fn expiry_unlinks_pending_middle_frames_without_reordering_or_early_release() {
        let mut store = Store::new(4).unwrap();
        let source = store.insert_source(buffer(60)).ok().unwrap();
        let mut queue = Queue::default();
        for id in [30, 10, 20, 40] {
            let index = store.insert_frame(frame(source, 0..60, id)).unwrap();
            queue.push_back(&mut store, index).unwrap();
        }
        assert_eq!(
            store
                .pop_expired(Duration::from_millis(10) - Duration::from_nanos(1))
                .unwrap(),
            None
        );
        let index = store
            .pop_expired(Duration::from_millis(10))
            .unwrap()
            .unwrap();
        assert_eq!(store.frame(index).unwrap().id, 10);
        assert!(store.release(index).is_none());
        assert!(store.frame(index).is_some());
        assert!(store.unqueue(&mut queue, index).unwrap());
        assert!(!store.unqueue(&mut queue, index).unwrap());
        assert_eq!(
            queue
                .iter(&store)
                .map(|i| store.frame(i).unwrap().id)
                .collect::<Vec<_>>(),
            [30, 20, 40]
        );
        assert!(store.release(index).is_none());
        assert_eq!(store.deadline(), Some(Duration::from_millis(20)));
        store.set_deadline(3, Duration::ZERO);
        assert_eq!(store.pop_expired(Duration::ZERO).unwrap(), Some(3));
        assert!(store.unqueue(&mut queue, 3).unwrap());
        store.release(3);
        store.set_deadline(2, Duration::MAX);
        assert_eq!(store.deadline(), Some(Duration::from_millis(30)));
        while let Some(index) = queue.pop_front(&mut store).unwrap() {
            store.release(index);
        }
        assert!(store.unpin(source).is_some());
        assert_eq!(store.deadline(), None);
    }

    #[test]
    fn slabs_preserve_each_deadline_and_charge_dead_ranges_until_recycling() {
        let mut store = Store::new(CAPACITY).unwrap();
        let input = store.insert_source(buffer(MAX_FRAME_LEN)).ok().unwrap();
        let slab = store.insert_source(buffer(0)).ok().unwrap();
        let mut ids = Vec::new();
        for id in 1..=7 {
            let range = store.append(slab, input, None).unwrap();
            ids.push(store.insert_frame(frame(slab, range, id)).unwrap());
        }
        let used = 7 * MAX_FRAME_LEN;
        assert_eq!(store.source(slab).unwrap().length, used);
        assert!(store.append(slab, input, None).is_none());
        assert_eq!(store.source(slab).unwrap().length, used);
        for &index in &ids[..6] {
            assert!(store.release(index).is_none());
        }
        assert_eq!(store.source(slab).unwrap().length, used);
        let last = ids[6];
        assert_eq!(
            store.frame(last).unwrap().deadline,
            Duration::from_millis(7)
        );
        assert_eq!(store.input(last).unwrap().bytes, [7; MAX_FRAME_LEN]);
        assert!(store.unpin(slab).is_none());
        assert_eq!(store.release(last).unwrap().length, used);
        assert!(store.unpin(input).is_some());
        assert_eq!(store.available(), CAPACITY);
        assert_eq!(store.free_sources.len(), BUFFERS);
    }
}
