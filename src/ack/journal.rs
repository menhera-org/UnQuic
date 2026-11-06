//! Bounded packet-to-frame mappings; packet charges and payload storage are separate.

use crate::{MAX_FRAME_LEN, wire::MAX_RECORDS};
use std::io;

const END: u32 = u32::MAX;
const HEAD: u32 = 1 << 31;

/// One DATA piece, independent of its payload's address.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FramePiece {
    /// Caller-owned frame slot, stable until all its mappings and plans are gone.
    pub frame_slot: u16,
    /// Full wire frame identity; validate its connection separately.
    pub frame_id: u64,
    /// Inclusive byte offset in the original frame.
    pub start: u16,
    /// Exclusive byte offset in the original frame.
    pub end: u16,
    /// Zero for fresh data, one or two for repair.
    pub attempt: u8,
}

#[derive(Clone, Copy)]
struct Node {
    piece: FramePiece,
    next_packet: u32,
    previous_packet: u32,
    next_frame: u32,
    previous_frame: u32,
}

impl Node {
    const EMPTY: Self = Self {
        piece: FramePiece {
            frame_slot: 0,
            frame_id: 0,
            start: 0,
            end: 0,
            attempt: 0,
        },
        next_packet: END,
        previous_packet: END,
        next_frame: END,
        previous_frame: END,
    };
}

#[derive(Clone, Copy)]
struct Frame {
    id: u64,
    head: u32,
    length: u16,
}

/// Fixed owner-local journal with constant-time unlinking by packet or frame.
///
/// Packet slots are caller-assigned indices, usually flattened sent-history slots.
/// Reserve mappings before I/O; cancel unsuccessful packets by popping their pieces.
/// Only successful submissions may update FrameRecovery or be exposed to feedback.
/// No operation allocates after construction.
///
/// Keep mappings after packet loss so late ACKs can settle frame coverage. ACKs
/// consume packet lists; expiry/completion consumes frame lists with a work budget.
/// Neither operation releases packet congestion charges. Before reusing a packet
/// slot, first secure its history reservation, then clear its old mappings. Failed
/// history reservations must not discard old mappings. Explicit resolved-prefix
/// retirement also permits clearing mappings, after earlier reports have drained.
///
/// Frame slots remain pinned by the caller while a prepared plan refers to them,
/// even if their mapping list is empty. Connection replacement must retire all old
/// plans and feedback before rebinding identities that can repeat on a new connection.
pub struct PieceJournal {
    nodes: Vec<Node>,
    packets: Vec<u32>,
    frames: Vec<Frame>,
    free: u32,
    available: usize,
}

impl PieceJournal {
    /// Storage for the requested fixed capacities, excluding allocator overhead.
    ///
    /// A frame slot fits u16; node and packet indices must fit below the head tag.
    pub fn storage_bytes(packets: usize, frames: usize, mappings: usize) -> Option<usize> {
        if packets >= HEAD as usize || mappings >= HEAD as usize || frames > u16::MAX as usize + 1 {
            return None;
        }
        size_of::<Self>()
            .checked_add(packets.checked_mul(size_of::<u32>())?)?
            .checked_add(frames.checked_mul(size_of::<Frame>())?)?
            .checked_add(mappings.checked_mul(size_of::<Node>())?)
    }

    /// Fallibly reserves fixed packet, frame and mapping stores.
    pub fn new(packets: usize, frames: usize, mappings: usize) -> io::Result<Self> {
        Self::storage_bytes(packets, frames, mappings).ok_or(io::ErrorKind::InvalidInput)?;
        let mut nodes = filled(mappings, Node::EMPTY)?;
        for (index, node) in nodes.iter_mut().enumerate() {
            node.next_packet = if index + 1 < mappings {
                index as u32 + 1
            } else {
                END
            };
        }
        Ok(Self {
            nodes,
            packets: filled(packets, END)?,
            frames: filled(
                frames,
                Frame {
                    id: 0,
                    head: END,
                    length: 0,
                },
            )?,
            free: if mappings == 0 { END } else { 0 },
            available: mappings,
        })
    }

    /// Free mappings; reserve a repair allowance before admitting fresh DATA.
    pub const fn available(&self) -> usize {
        self.available
    }

    /// Binds an empty frame slot to its current identity and original length.
    ///
    /// False leaves it unchanged. The caller must first retire old prepared plans.
    pub fn bind_frame(&mut self, slot: usize, id: u64, length: usize) -> bool {
        if id >> 48 == 0 || id & ((1 << 48) - 1) == 0 || !(14..=MAX_FRAME_LEN).contains(&length) {
            return false;
        }
        let Some(frame) = self.frames.get_mut(slot).filter(|frame| frame.head == END) else {
            return false;
        };
        frame.id = id;
        frame.length = length as u16;
        true
    }

    /// Whether a valid packet slot has no remaining mappings.
    pub fn packet_empty(&self, slot: usize) -> bool {
        self.packets.get(slot) == Some(&END)
    }

    /// Whether this exact bound frame has no remaining mappings.
    pub fn frame_empty(&self, slot: usize, id: u64) -> bool {
        id != 0
            && self
                .frames
                .get(slot)
                .is_some_and(|frame| frame.id == id && frame.head == END)
    }

    /// Reserves a whole packet's mappings before I/O, preserving piece order.
    ///
    /// Rejects a busy/invalid packet slot, insufficient capacity, more than 64
    /// pieces, stale frame identities and invalid ranges/attempts without mutation.
    /// The caller validates repair generations and reserves the whole send plan's
    /// capacity first. Roll back an unsuccessful packet with pop_packet.
    pub fn reserve_packet(&mut self, slot: usize, pieces: &[FramePiece]) -> bool {
        if !self.packet_empty(slot)
            || pieces.len() > MAX_RECORDS
            || pieces.len() > self.available
            || pieces.iter().any(|piece| {
                self.frames
                    .get(usize::from(piece.frame_slot))
                    .is_none_or(|frame| {
                        frame.id == 0
                            || frame.id != piece.frame_id
                            || piece.start >= piece.end
                            || piece.end > frame.length
                            || piece.attempt > 2
                    })
            })
        {
            return false;
        }
        for &piece in pieces.iter().rev() {
            let index = self.free;
            let Some(node) = self.nodes.get_mut(index as usize) else {
                return false;
            };
            self.free = node.next_packet;
            let Some(packet) = self.packets.get_mut(slot) else {
                return false;
            };
            let Some(frame) = self.frames.get_mut(usize::from(piece.frame_slot)) else {
                return false;
            };
            *node = Node {
                piece,
                next_packet: *packet,
                previous_packet: HEAD | slot as u32,
                next_frame: frame.head,
                previous_frame: END,
            };
            if let Some(next) = self.nodes.get_mut(*packet as usize) {
                next.previous_packet = index;
            }
            if let Some(next) = self.nodes.get_mut(frame.head as usize) {
                next.previous_frame = index;
            }
            *packet = index;
            frame.head = index;
            self.available -= 1;
        }
        true
    }

    /// Visits at most 64 retained pieces in wire order without consuming mappings.
    ///
    /// Use after successful submission and on loss; loss must retain late-ACK mappings.
    pub fn packet(&self, slot: usize) -> impl Iterator<Item = FramePiece> + '_ {
        std::iter::successors(self.packets.get(slot).copied(), |&index| {
            self.nodes.get(index as usize).map(|node| node.next_packet)
        })
        .take(MAX_RECORDS)
        .filter_map(|index| self.nodes.get(index as usize).map(|node| node.piece))
    }

    /// Removes one packet mapping from both lists, returning its piece exactly once.
    ///
    /// Use for ACK/cancellation or successful history eviction; invalid/empty is None.
    pub fn pop_packet(&mut self, slot: usize) -> Option<FramePiece> {
        self.unlink(*self.packets.get(slot)?)
    }

    /// Removes one mapping for this exact frame from both lists in constant time.
    ///
    /// Call at most the cleanup budget per turn, and retain the slot until frame_empty.
    /// A stale identity cannot remove a replacement frame's mappings.
    pub fn pop_frame(&mut self, slot: usize, id: u64) -> Option<FramePiece> {
        let frame = self
            .frames
            .get(slot)
            .filter(|frame| id != 0 && frame.id == id)?;
        self.unlink(frame.head)
    }

    fn unlink(&mut self, index: u32) -> Option<FramePiece> {
        let node = *self.nodes.get(index as usize)?;
        if node.piece.frame_id == 0 {
            return None;
        }
        // The head tag lets expiry update a packet head without searching histories.
        if node.previous_packet & HEAD != 0 {
            *self
                .packets
                .get_mut((node.previous_packet & !HEAD) as usize)? = node.next_packet;
        } else {
            self.nodes
                .get_mut(node.previous_packet as usize)?
                .next_packet = node.next_packet;
        }
        if let Some(next) = self.nodes.get_mut(node.next_packet as usize) {
            next.previous_packet = node.previous_packet;
        }
        if node.previous_frame == END {
            self.frames
                .get_mut(usize::from(node.piece.frame_slot))?
                .head = node.next_frame;
        } else {
            self.nodes.get_mut(node.previous_frame as usize)?.next_frame = node.next_frame;
        }
        if let Some(next) = self.nodes.get_mut(node.next_frame as usize) {
            next.previous_frame = node.previous_frame;
        }
        *self.nodes.get_mut(index as usize)? = Node {
            next_packet: self.free,
            ..Node::EMPTY
        };
        self.free = index;
        self.available += 1;
        Some(node.piece)
    }
}

fn filled<T: Clone>(length: usize, value: T) -> io::Result<Vec<T>> {
    let mut values = Vec::new();
    values
        .try_reserve_exact(length)
        .map_err(|_| io::ErrorKind::OutOfMemory)?;
    values.resize(length, value);
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mapping_storage_stays_compact() {
        assert_eq!(size_of::<Node>(), 32);
        assert_eq!(size_of::<Frame>(), 16);
    }
}
