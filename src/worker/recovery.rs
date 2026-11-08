//! Payload coverage stays local; sampler handles name packet mappings until settlement.
use super::*;
use crate::ack::{FramePiece, PieceJournal};

const MAPPINGS_PER_FRAME: usize = 32;
const PACKETS: usize = u16::MAX as usize;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum Place {
    #[default]
    Fresh,
    Batch,
    Retained,
    Repair,
    Cleanup,
}

#[derive(Clone, Copy, Default)]
struct Slot {
    id: u64,
    place: Place,
}

pub(super) struct Recovery {
    pub journal: PieceJournal,
    slots: Vec<Slot>,
    cleanup: VecDeque<usize>,
    reserve: usize,
    pub staged: Option<Box<sender::Report>>,
    pub cursor: usize,
    pub quiesce: Option<usize>,
}

impl Recovery {
    pub fn storage_bytes(frames: usize) -> Option<usize> {
        PieceJournal::storage_bytes(
            if frames == 0 { 0 } else { PACKETS },
            frames,
            frames.checked_mul(MAPPINGS_PER_FRAME)?,
        )?
        .checked_add(frames.checked_mul(size_of::<Slot>() + size_of::<usize>())?)
    }

    pub fn new(frames: usize) -> io::Result<Self> {
        let mut slots = Vec::new();
        slots.try_reserve_exact(frames).map_err(io::Error::other)?;
        slots.resize(frames, Slot::default());
        let mut cleanup = VecDeque::new();
        cleanup
            .try_reserve_exact(frames)
            .map_err(io::Error::other)?;
        Ok(Self {
            journal: PieceJournal::new(
                if frames == 0 { 0 } else { PACKETS },
                frames,
                frames * MAPPINGS_PER_FRAME,
            )?,
            slots,
            cleanup,
            reserve: frames * MAPPINGS_PER_FRAME / 2,
            staged: None,
            cursor: 0,
            quiesce: None,
        })
    }

    pub fn bind(&mut self, index: usize, frame: &frames::Frame) -> io::Result<()> {
        let slot = self
            .slots
            .get_mut(index)
            .ok_or(io::ErrorKind::InvalidData)?;
        if slot.id != frame.id {
            if !self.journal.bind_frame(index, frame.id, frame.length()) {
                return Err(io::Error::other("live frame mapping reused"));
            }
            slot.id = frame.id;
            slot.place = Place::Fresh;
        }
        Ok(())
    }

    pub fn place(&self, index: usize) -> io::Result<Place> {
        self.slots
            .get(index)
            .map(|slot| slot.place)
            .ok_or(io::ErrorKind::InvalidData.into())
    }
    pub fn set(&mut self, index: usize, place: Place) -> io::Result<()> {
        self.slots
            .get_mut(index)
            .ok_or(io::ErrorKind::InvalidData)?
            .place = place;
        Ok(())
    }
    pub fn begin_quiesce(&mut self, frames: &frames::Store) {
        self.quiesce = (frames.available() != self.slots.len()).then_some(0);
    }

    pub fn has_cleanup(&self) -> bool {
        !self.cleanup.is_empty() || self.quiesce.is_some()
    }
    pub fn fresh_capacity(&self) -> bool {
        self.journal.available() > self.reserve
    }
}

impl Worker {
    pub(super) fn ready_lane(&mut self, session: usize, lane: usize) {
        if self.sessions[session].tx_ready & (1 << lane) == 0 {
            self.sessions[session].tx_ready |= 1 << lane;
            self.ready.push_back((session, lane));
        }
    }

    // A view is detached from every plan/ready queue before entering cleanup.
    pub(super) fn release_frame(&mut self, index: usize) -> io::Result<()> {
        if !self.sender_enabled() {
            self.release_frame_storage(index);
        } else if self.recovery.place(index)? != Place::Cleanup {
            let (session, lane) = self.frame_owner(index)?;
            let frame = self.frames.frame(index).ok_or(io::ErrorKind::InvalidData)?;
            let sent = self
                .frames
                .recovery(index)
                .ok_or(io::ErrorKind::InvalidData)?
                .sent();
            let unsent = frame
                .length()
                .checked_sub(sent)
                .ok_or(io::ErrorKind::InvalidData)?;
            self.release_unsent(session, lane, unsent)?;
            self.recovery.set(index, Place::Cleanup)?;
            if self.recovery.cleanup.len() == self.recovery.cleanup.capacity() {
                return Err(io::ErrorKind::InvalidData.into());
            }
            self.recovery.cleanup.push_back(index);
            self.counters.recovery_cleanup_peak = self
                .counters
                .recovery_cleanup_peak
                .max(self.recovery.cleanup.len() as u64);
        }
        Ok(())
    }

    pub(super) fn cleanup_frames(&mut self) -> io::Result<bool> {
        if let Some(first) = self.recovery.quiesce {
            let end = (first + BATCH_SIZE).min(self.recovery.slots.len());
            for index in first..end {
                let stale = self
                    .frames
                    .frame(index)
                    .and_then(|frame| self.frames.source(frame.source))
                    .is_some_and(|source| !self.matches(source.session, source.tag));
                if stale && self.recovery.place(index)? != Place::Cleanup {
                    self.detach_frame(index)?;
                    self.release_frame(index)?;
                }
            }
            self.recovery.quiesce = (end < self.recovery.slots.len()).then_some(end);
        }
        let mut work = 128;

        while work != 0 {
            let Some(&index) = self.recovery.cleanup.front() else {
                break;
            };
            let id = self
                .frames
                .frame(index)
                .ok_or(io::ErrorKind::InvalidData)?
                .id;
            if self.recovery.journal.pop_frame(index, id).is_some() {
                work -= 1;
                continue;
            }
            self.recovery.cleanup.pop_front();
            *self
                .recovery
                .slots
                .get_mut(index)
                .ok_or(io::ErrorKind::InvalidData)? = Slot::default();
            self.release_frame_storage(index);
            work -= 1;
        }
        if work == 0 && !self.recovery.cleanup.is_empty() {
            self.counters.recovery_cleanup_yields =
                self.counters.recovery_cleanup_yields.wrapping_add(1);
        }
        Ok(self.recovery.has_cleanup())
    }

    pub(super) fn frame_owner(&self, index: usize) -> io::Result<(usize, usize)> {
        let frame = self.frames.frame(index).ok_or(io::ErrorKind::InvalidData)?;
        let source = self
            .frames
            .source(frame.source)
            .ok_or(io::ErrorKind::InvalidData)?;
        Ok((source.session, frame.lane))
    }

    pub(super) fn detach_frame(&mut self, index: usize) -> io::Result<()> {
        let (session, lane) = self.frame_owner(index)?;
        let mut batch = self.sessions[session].batches[lane]
            .take()
            .ok_or(io::ErrorKind::InvalidData)?;
        match self.recovery.place(index)? {
            Place::Fresh => {
                self.frames.unqueue(&mut batch.pending, index)?;
            }
            Place::Repair => {
                self.frames.unqueue(&mut batch.repairs, index)?;
            }
            Place::Batch => {
                for slot in &mut batch.frames[..batch.len] {
                    if *slot == Some(index) {
                        *slot = None;
                    }
                }
                self.discard_batch(session, lane, &mut batch);
            }
            Place::Retained | Place::Cleanup => {}
        }
        self.sessions[session].batches[lane] = Some(batch);
        Ok(())
    }

    pub(super) fn queue_recovery_in(
        &mut self,
        batch: &mut TxBatch,
        index: usize,
    ) -> io::Result<bool> {
        let state = self
            .frames
            .recovery(index)
            .ok_or(io::ErrorKind::InvalidData)?;
        let repair = state.next_repair(crate::MAX_FRAME_LEN).is_some();
        let fresh = state.sent()
            < self
                .frames
                .frame(index)
                .ok_or(io::ErrorKind::InvalidData)?
                .length();
        if repair || fresh {
            let queue = if repair {
                &mut batch.repairs
            } else {
                &mut batch.pending
            };
            queue.push_back(&mut self.frames, index)?;
            self.recovery
                .set(index, if repair { Place::Repair } else { Place::Fresh })?;
        }
        Ok(repair || fresh)
    }

    pub(super) fn queue_recovery(&mut self, index: usize) -> io::Result<()> {
        if self.recovery.place(index)? != Place::Retained {
            return Ok(());
        }
        let (session, lane) = self.frame_owner(index)?;
        let mut batch = self.sessions[session].batches[lane]
            .take()
            .ok_or(io::ErrorKind::InvalidData)?;
        let ready = self.queue_recovery_in(&mut batch, index)?;
        self.sessions[session].batches[lane] = Some(batch);
        if ready {
            self.ready_lane(session, lane);
        }
        Ok(())
    }

    pub(super) fn yield_batch(
        &mut self,
        session: usize,
        lane: usize,
        batch: &mut TxBatch,
    ) -> io::Result<()> {
        self.discard_batch(session, lane, batch);
        for slot in (0..batch.len).rev() {
            if let Some(index) = batch.frames[slot].take() {
                let state = self
                    .frames
                    .recovery(index)
                    .ok_or(io::ErrorKind::InvalidData)?;
                if state.next_repair(crate::MAX_FRAME_LEN).is_some() {
                    batch.repairs.push_front(&mut self.frames, index)?;
                    self.recovery.set(index, Place::Repair)?;
                } else if state.sent()
                    < self
                        .frames
                        .frame(index)
                        .ok_or(io::ErrorKind::InvalidData)?
                        .length()
                {
                    batch.pending.push_front(&mut self.frames, index)?;
                    self.recovery.set(index, Place::Fresh)?;
                } else {
                    self.recovery.set(index, Place::Retained)?;
                }
            }
        }

        batch.len = 0;
        Ok(())
    }

    // A packet is handled atomically: no cursor can be invalidated by frame expiry.
    pub(super) fn stage_recovery(&mut self) -> io::Result<bool> {
        let Some(report) = self.recovery.staged.take() else {
            return Ok(false);
        };
        let mut pieces = 128;
        let mut bytes = 64 * 1024;
        while self.recovery.cursor < report.count && pieces >= 64 && bytes >= 9000 {
            let entry = report.entries[self.recovery.cursor];
            self.recovery.cursor += 1;
            let Some(sample) = entry.sample else { continue };
            let packet = usize::from(sample.get()) - 1;
            let mut mappings = [FramePiece::default(); 64];
            let mut count = 0;
            let mut length = 0;
            for piece in self.recovery.journal.packet(packet) {
                if count == mappings.len() {
                    return Err(io::ErrorKind::InvalidData.into());
                }
                length += usize::from(piece.end - piece.start);
                mappings[count] = piece;
                count += 1;
            }
            if length > 8952 {
                return Err(io::ErrorKind::InvalidData.into());
            }
            for piece in &mappings[..count] {
                let index = usize::from(piece.frame_slot);
                if self
                    .frames
                    .frame(index)
                    .is_none_or(|f| f.id != piece.frame_id)
                {
                    return Err(io::Error::other("stale frame mapping"));
                }
                let state = self
                    .frames
                    .recovery_mut(index)
                    .ok_or(io::ErrorKind::InvalidData)?;
                let range = usize::from(piece.start)..usize::from(piece.end);
                match report.kind {
                    sender::ReportKind::Ack { .. } => {
                        state
                            .acknowledge(range, piece.attempt)
                            .ok_or(io::ErrorKind::InvalidData)?;
                    }
                    sender::ReportKind::Loss => {
                        if !state.lost(range, piece.attempt) {
                            return Err(io::ErrorKind::InvalidData.into());
                        }
                    }
                    sender::ReportKind::Retire => {}
                }
                if !matches!(report.kind, sender::ReportKind::Loss) {
                    self.recovery
                        .journal
                        .pop_packet(packet)
                        .ok_or(io::ErrorKind::InvalidData)?;
                }
                if self.recovery.place(index)? == Place::Cleanup {
                    continue;
                }
                if self
                    .frames
                    .recovery(index)
                    .is_some_and(|s| s.remaining() == 0)
                {
                    self.detach_frame(index)?;
                    self.release_frame(index)?;
                } else {
                    self.queue_recovery(index)?;
                }
            }
            pieces -= count.max(1);
            bytes -= length;
        }
        if self.recovery.cursor == report.count {
            self.recovery.cursor = 0;
            self.sender.queue(0, sender::Message::Report(report))?;
            self.counters.recovery_staged_reports =
                self.counters.recovery_staged_reports.wrapping_add(1);
            Ok(false)
        } else {
            self.recovery.staged = Some(report);
            self.counters.recovery_staging_yields =
                self.counters.recovery_staging_yields.wrapping_add(1);
            Ok(true)
        }
    }

    pub(super) fn reserve_pieces(&mut self, batch: &TxBatch, count: usize) -> io::Result<bool> {
        let mut needed = 0;
        for ordinal in 0..count {
            needed += batch
                .plan
                .pieces(ordinal)
                .ok_or(io::ErrorKind::InvalidData)?
                .count();
        }
        let available = self.recovery.journal.available();
        if needed > available
            || (!batch.repair && needed > available.saturating_sub(self.recovery.reserve))
        {
            self.counters.recovery_mapping_pressure =
                self.counters.recovery_mapping_pressure.wrapping_add(1);
            return Ok(false);
        }
        // Validate complete selected ranges before linking any packet. Later pieces of
        // one fresh frame are contiguous but its submitted cursor has not advanced yet.
        for (ordinal, index) in batch.frames[..batch.len].iter().enumerate() {
            let Some(index) = *index else { continue };
            let frame = self.frames.frame(index).ok_or(io::ErrorKind::InvalidData)?;
            self.recovery.bind(index, frame)?;
            self.recovery.set(index, Place::Batch)?;
            let range = if batch.repair {
                usize::from(batch.selection[ordinal].start)
                    ..usize::from(batch.selection[ordinal].end)
            } else {
                frame.offset..frame.length()
            };
            if !self
                .frames
                .recovery(index)
                .ok_or(io::ErrorKind::InvalidData)?
                .can_submit(range, batch.selection[ordinal].attempt)
            {
                self.counters.recovery_selection_changed =
                    self.counters.recovery_selection_changed.wrapping_add(1);
                return Ok(false);
            }
        }
        let flight = self
            .sender
            .flight
            .as_ref()
            .ok_or(io::ErrorKind::InvalidData)?;
        for ordinal in 0..count {
            let sample = flight.handle(ordinal).ok_or(io::ErrorKind::InvalidData)?;
            if !self
                .recovery
                .journal
                .packet_empty(usize::from(sample.get()) - 1)
            {
                return Err(io::Error::other("live packet mapping reused"));
            }
            let packet = batch
                .plan
                .packet(ordinal)
                .ok_or(io::ErrorKind::InvalidData)?;
            let mut bytes = 0;
            for (slot, range) in batch
                .plan
                .pieces(ordinal)
                .ok_or(io::ErrorKind::InvalidData)?
            {
                let index = batch
                    .frames
                    .get(slot)
                    .copied()
                    .flatten()
                    .ok_or(io::ErrorKind::InvalidData)?;
                let frame = self.frames.frame(index).ok_or(io::ErrorKind::InvalidData)?;
                let selected = batch
                    .selection
                    .get(slot)
                    .ok_or(io::ErrorKind::InvalidData)?;
                let allowed = if batch.repair {
                    usize::from(selected.start)..usize::from(selected.end)
                } else {
                    frame.offset..frame.length()
                };
                if range.is_empty() || range.start < allowed.start || range.end > allowed.end {
                    return Err(io::Error::other("DATA mapping exceeds selected range"));
                }
                bytes += range.len();
            }
            if bytes > packet.length || packet.length > 8952 {
                return Err(io::Error::other("DATA mapping exceeds datagram"));
            }
        }
        for ordinal in 0..count {
            let packet = batch
                .plan
                .packet(ordinal)
                .ok_or(io::ErrorKind::InvalidData)?;
            let sample = flight.handle(ordinal).ok_or(io::ErrorKind::InvalidData)?;
            let mut pieces = [FramePiece::default(); 64];
            let mut length = 0;
            let mut n = 0;
            for (slot, range) in batch
                .plan
                .pieces(ordinal)
                .ok_or(io::ErrorKind::InvalidData)?
            {
                if n == pieces.len() {
                    return Err(io::ErrorKind::InvalidData.into());
                }
                let index = batch.frames[slot].ok_or(io::ErrorKind::InvalidData)?;
                let frame = self.frames.frame(index).ok_or(io::ErrorKind::InvalidData)?;
                length += range.len();
                pieces[n] = FramePiece {
                    frame_slot: index as u16,
                    frame_id: frame.id,
                    start: range.start as u16,
                    end: range.end as u16,
                    attempt: batch.selection[slot].attempt,
                };
                n += 1;
            }
            if length > packet.length
                || packet.length > 8952
                || !self
                    .recovery
                    .journal
                    .reserve_packet(usize::from(sample.get()) - 1, &pieces[..n])
            {
                for prior in 0..ordinal {
                    if let Some(sample) = flight.handle(prior) {
                        while self
                            .recovery
                            .journal
                            .pop_packet(usize::from(sample.get()) - 1)
                            .is_some()
                        {}
                    }
                }
                return Err(io::Error::other("invalid reserved DATA mappings"));
            }
        }
        Ok(true)
    }

    pub(super) fn settle_sent_pieces(
        &mut self,
        flight: &pacing::Flight,
        sent: usize,
    ) -> io::Result<()> {
        for ordinal in 0..flight.packets().len() {
            let Some(sample) = flight.handle(ordinal) else {
                continue;
            };
            let packet = usize::from(sample.get()) - 1;
            if ordinal < sent {
                for piece in self.recovery.journal.packet(packet) {
                    if self
                        .frames
                        .frame(usize::from(piece.frame_slot))
                        .is_none_or(|f| f.id != piece.frame_id)
                    {
                        return Err(io::Error::other("stale submitted frame mapping"));
                    }
                    let state = self
                        .frames
                        .recovery_mut(usize::from(piece.frame_slot))
                        .ok_or(io::ErrorKind::InvalidData)?;
                    if !state.submitted(
                        usize::from(piece.start)..usize::from(piece.end),
                        piece.attempt,
                    ) {
                        return Err(io::Error::other("invalid submitted DATA coverage"));
                    }
                }
            } else {
                while self.recovery.journal.pop_packet(packet).is_some() {}
            }
        }
        Ok(())
    }
}
