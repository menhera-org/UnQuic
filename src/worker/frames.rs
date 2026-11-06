//! Owner-local payload storage and compact frame handles.
use super::{BUFFERS, Buffer};
use crate::{pack, tap::Segments};
use std::{io, mem::size_of, ops::Range, time::Duration};

pub(super) const CAPACITY: usize = 8192;
pub(super) const HEADER_BYTES: usize = 128;

const END: usize = CAPACITY;
const UNQUEUED: usize = usize::MAX;

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

    pub fn iter<'a>(&self, store: &'a Store) -> impl Iterator<Item = usize> + 'a {
        std::iter::successors(self.front(), move |&index| {
            store.links.get(index).copied().filter(|&next| next < END)
        })
        .take(self.len)
    }

    pub fn push_back(&mut self, store: &mut Store, index: usize) -> io::Result<()> {
        if store.frame(index).is_none()
            || store.links.get(index) != Some(&UNQUEUED)
            || self.len >= CAPACITY
            || (self.len != 0 && store.links.get(self.tail) != Some(&END))
        {
            return Err(io::Error::other("invalid transmit queue insertion"));
        }
        if self.len == 0 {
            self.head = index;
        } else if let Some(next) = store.links.get_mut(self.tail) {
            *next = index;
        }
        store.links[index] = END;
        self.tail = index;
        self.len += 1;
        Ok(())
    }

    pub fn pop_front(&mut self, store: &mut Store) -> io::Result<Option<usize>> {
        let Some(index) = self.front() else {
            return Ok(None);
        };
        let next = store.links.get(index).copied().unwrap_or(UNQUEUED);
        if store.frame(index).is_none()
            || next > END
            || self.len == 0
            || (next == END) != (self.len == 1)
        {
            return Err(io::Error::other("invalid transmit queue removal"));
        }
        self.head = next;
        self.len -= 1;
        if self.len == 0 {
            self.tail = END;
        }
        store.links[index] = UNQUEUED;
        Ok(Some(index))
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
    free_frames: Vec<usize>,
    links: Vec<usize>,
}

impl Store {
    pub const fn storage_bytes(capacity: usize) -> usize {
        BUFFERS * (size_of::<Option<Source>>() + size_of::<usize>())
            + capacity * (size_of::<Option<Frame>>() + 2 * size_of::<usize>())
    }

    pub fn new(capacity: usize) -> io::Result<Self> {
        if capacity > CAPACITY {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let sources = BUFFERS;
        Ok(Self {
            sources: std::iter::repeat_with(|| None).take(sources).collect(),
            free_sources: (0..sources).rev().collect(),
            frames: std::iter::repeat_with(|| None).take(capacity).collect(),
            free_frames: (0..capacity).rev().collect(),
            links: vec![UNQUEUED; capacity],
        })
    }

    pub fn available(&self) -> usize {
        self.free_frames.len()
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

    pub fn insert_frame(&mut self, frame: Frame) -> Option<usize> {
        let &index = self.free_frames.last()?;
        let slot = self.frames.get_mut(index)?;
        let source = self.sources.get_mut(frame.source)?.as_mut()?;
        if frame.header_len > frame.header.len()
            || frame.range.start > frame.range.end
            || frame.range.end > source.buffer.length
            || frame.offset >= frame.length()
        {
            return None;
        }
        source.references = source.references.checked_add(1)?;
        self.free_frames.pop();
        *slot = Some(frame);
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
        if self.links.get(index) != Some(&UNQUEUED) {
            return None;
        }
        let frame = self.frames.get_mut(index)?.take()?;
        self.free_frames.push(index);
        let source = self.sources.get_mut(frame.source)?.as_mut()?;
        source.references = source.references.checked_sub(1)?;
        self.recycle(frame.source)
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
        assert_eq!(changes, [0]);
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
