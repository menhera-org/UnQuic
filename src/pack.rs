//! Baseline DATA segmentation and deterministic inner-flow lane selection.
//!
//! A transmit owner keeps one Lane for the connection's whole lifetime. Packet
//! and frame counters never wrap or reuse identifiers. Replace it only through
//! the connection barrier. Batch packs queued frames without payload copies;
//! the single-frame encoder remains available for baseline segmentation.

use crate::{
    BASE_UDP_PAYLOAD, MAX_FRAME_LEN, UnquicUuid,
    handshake::Connection,
    socket::{GSO_MAX_BYTES, MAX_UDP_PAYLOAD},
    wire::{
        DATA_PREFIX_LEN, HEADER_LEN, Header, MAX_RECORDS, PROBE_PREFIX_LEN, Record, WireError,
        encode_data_prefix, encode_data_prefix_length, encode_datagram, encode_probe_prefix,
    },
};
use std::{collections::hash_map::DefaultHasher, fmt, hash::Hasher};

const DATA_OVERHEAD: usize = DATA_PREFIX_LEN;
const RECORD_PREFIX_LEN: usize = DATA_PREFIX_LEN - HEADER_LEN;
const MAX_SEQUENCE: u64 = 0x0000_ffff_ffff_ffff;
static PROBE_PADDING: [u8; MAX_UDP_PAYLOAD - PROBE_PREFIX_LEN] =
    [0; MAX_UDP_PAYLOAD - PROBE_PREFIX_LEN];

/// Encoding failure, with exhausted identities requiring a new connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackError {
    /// Lane zero is reserved for handshakes.
    Lane,
    /// Frame is outside Ethernet or negotiated size limits.
    Frame,
    /// Packet numbers or frame sequences have run out; never wrap them.
    Exhausted,
    /// UDP payload limit is outside the socket API's supported range.
    Limit,
    /// The record or supplied output buffer cannot be encoded.
    Wire(WireError),
}
impl fmt::Display for PackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lane => f.write_str("transmit lane must be nonzero"),
            Self::Frame => f.write_str("invalid frame length or negotiated ceiling"),
            Self::Exhausted => f.write_str("transmit identity space exhausted"),
            Self::Limit => f.write_str("unsupported UDP payload limit"),
            Self::Wire(error) => error.fmt(f),
        }
    }
}
impl std::error::Error for PackError {}

/// Metadata for a datagram encoded into the caller's buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Packet {
    /// Allocated packet number; each successful physical send uses this once.
    pub number: u64,
    /// Number of initialized UDP payload bytes.
    pub length: usize,
}

/// One local transmit lane's identities and negotiated frame ceiling.
pub struct Lane {
    header: Header,
    frame_limit: u16,
    next_frame: u64,
}
impl Lane {
    /// Starts packet/frame numbering at one under a freshly installed connection.
    ///
    /// The caller allocates nonzero lane IDs monotonically without reuse.
    pub fn new(cookie: UnquicUuid, connection: Connection, id: u16) -> Result<Self, PackError> {
        if id == 0 {
            return Err(PackError::Lane);
        }
        if !(1514..=MAX_FRAME_LEN as u16).contains(&connection.max_frame_len) {
            return Err(PackError::Frame);
        }
        Ok(Self {
            header: Header {
                cookie,
                receive_tag: connection.remote_tag().get(),
                lane: id,
                record_area_len: 0,
                packet_number: 1,
            },
            frame_limit: connection.max_frame_len,
            next_frame: 1,
        })
    }

    /// Allocates a frame identity and borrows its bytes until segmentation ends.
    ///
    /// Dropping the encoder abandons the unsent tail and never reuses its ID.
    pub fn frame<'a>(&'a mut self, bytes: &'a [u8]) -> Result<Frame<'a>, PackError> {
        let id = self.start_frame(bytes.len())?;
        Ok(Frame {
            lane: self,
            bytes,
            id,
            offset: 0,
        })
    }

    /// Allocates one frame identity for retained, independently owned payload storage.
    pub fn start_frame(&mut self, length: usize) -> Result<u64, PackError> {
        if !(14..=usize::from(self.frame_limit)).contains(&length) {
            return Err(PackError::Frame);
        }
        if self.next_frame == 0 || self.header.packet_number == 0 {
            return Err(PackError::Exhausted);
        }
        let id = (u64::from(self.header.lane) << 48) | self.next_frame;
        self.next_frame = if self.next_frame == MAX_SEQUENCE {
            0
        } else {
            self.next_frame + 1
        };
        Ok(id)
    }

    /// Encodes one established record at the 1232-byte baseline.
    ///
    /// Encoding errors leave the counter unchanged. After a failed send, retain
    /// the record's content and re-encode with a fresh number before retrying.
    /// Handshake records cannot use this established-lane encoder.
    pub fn record(&mut self, record: Record<'_>, output: &mut [u8]) -> Result<Packet, PackError> {
        if self.header.packet_number == 0 {
            return Err(PackError::Exhausted);
        }
        if matches!(record, Record::Data { total_len, .. } if total_len > u32::from(self.frame_limit))
        {
            return Err(PackError::Frame);
        }
        let limit = output.len().min(BASE_UDP_PAYLOAD);
        let length = encode_datagram(self.header, &[record], &mut output[..limit])
            .map_err(PackError::Wire)?;
        let number = self.header.packet_number;
        self.header.packet_number = number.checked_add(1).unwrap_or(0);
        Ok(Packet { number, length })
    }

    /// Allocates a probe identity and borrows shared zero padding without copying it.
    ///
    /// Send the prefix and returned bytes as one datagram. Success only encodes
    /// the probe; the owner must track actual submission and matching feedback.
    pub fn probe(
        &mut self,
        length: usize,
        prefix: &mut [u8; PROBE_PREFIX_LEN],
    ) -> Result<Piece<'static>, PackError> {
        if !(BASE_UDP_PAYLOAD..=MAX_UDP_PAYLOAD).contains(&length) {
            return Err(PackError::Limit);
        }
        if self.header.packet_number == 0 {
            return Err(PackError::Exhausted);
        }
        encode_probe_prefix(self.header, length, prefix).map_err(PackError::Wire)?;
        let number = self.header.packet_number;
        self.header.packet_number = number.checked_add(1).unwrap_or(0);
        Ok(Piece {
            packet: Packet { number, length },
            bytes: &PROBE_PADDING[..length - PROBE_PREFIX_LEN],
        })
    }
}

/// One scatter/gather DATA piece or probe, without copying its payload bytes.
pub struct Piece<'a> {
    /// Encoded packet identity and combined prefix/payload length.
    pub packet: Packet,
    /// Borrowed payload bytes to send immediately after the encoded prefix.
    pub bytes: &'a [u8],
}

/// A borrowed frame split into complete baseline datagrams.
pub struct Frame<'a> {
    lane: &'a mut Lane,
    bytes: &'a [u8],
    id: u64,
    offset: usize,
}
impl<'a> Frame<'a> {
    /// Encodes a prefix while borrowing piece bytes directly from the frame.
    ///
    /// Each prefix and returned byte slice form one scatter/gather datagram.
    /// The slice borrows the original frame, so several pieces can be prepared
    /// in separate prefix buffers and submitted together in one sendmmsg call.
    pub fn encode_next_split(
        &mut self,
        prefix: &mut [u8; DATA_PREFIX_LEN],
    ) -> Result<Option<Piece<'a>>, PackError> {
        if self.offset == self.bytes.len() {
            return Ok(None);
        }
        if self.lane.header.packet_number == 0 {
            return Err(PackError::Exhausted);
        }
        let end = self
            .bytes
            .len()
            .min(self.offset + BASE_UDP_PAYLOAD - DATA_PREFIX_LEN);
        let bytes = &self.bytes[self.offset..end];
        let record = Record::Data {
            frame_id: self.id,
            total_len: self.bytes.len() as u32,
            offset: self.offset as u32,
            bytes,
        };
        let length =
            encode_data_prefix(self.lane.header, record, prefix).map_err(PackError::Wire)?;
        let number = self.lane.header.packet_number;
        self.lane.header.packet_number = number.checked_add(1).unwrap_or(0);
        self.offset = end;
        Ok(Some(Piece {
            packet: Packet { number, length },
            bytes,
        }))
    }

    /// Allocated frame identity, unchanged across every piece.
    pub const fn id(&self) -> u64 {
        self.id
    }

    /// Encodes the next piece, or returns None after the complete frame.
    ///
    /// Output and state remain unchanged on encoding error. Each returned
    /// datagram must be submitted or discarded before encoding the next one.
    pub fn encode_next(&mut self, output: &mut [u8]) -> Result<Option<Packet>, PackError> {
        if self.offset == self.bytes.len() {
            return Ok(None);
        }
        let limit = output.len().min(BASE_UDP_PAYLOAD);
        if limit <= DATA_OVERHEAD {
            return Err(PackError::Wire(WireError::BufferTooSmall));
        }
        let end = self.bytes.len().min(self.offset + limit - DATA_OVERHEAD);
        let record = Record::Data {
            frame_id: self.id,
            total_len: self.bytes.len() as u32,
            offset: self.offset as u32,
            bytes: &self.bytes[self.offset..end],
        };
        let packet = self.lane.record(record, output)?;
        self.offset = end;
        Ok(Some(packet))
    }
}

/// Maximum queued frames or encoded packets in one retained batch.
pub const BATCH_CAPACITY: usize = 64;
/// Maximum scatter/gather slices for one header and 64 two-span DATA records.
pub const PACKED_SLICES: usize = 1 + MAX_RECORDS * 3;
/// Maximum borrowed slices across a complete batch.
pub const BATCH_SLICES: usize = BATCH_CAPACITY * 6;

/// A retained frame and selected byte range for fresh data or selective repair.
#[derive(Clone, Copy)]
pub struct FrameInput<'a> {
    /// Identity allocated by Lane::start_frame; unchanged across partial sends.
    pub id: u64,
    /// Optional rewritten Ethernet/IP/transport header preceding bytes.
    pub header: &'a [u8],
    /// Borrowed frame bytes following header; the complete frame when header is empty.
    pub bytes: &'a [u8],
    /// First selected byte in the concatenation of header and bytes.
    pub offset: usize,
    /// Exclusive selected end; use the complete frame length for fresh DATA.
    /// Repairs keep the original source spans and restrict this range.
    pub end: usize,
}

#[derive(Clone, Copy)]
struct PacketPlan {
    header: [u8; HEADER_LEN],
    packet: Packet,
    first: u16,
    count: u16,
}

#[derive(Clone, Copy)]
struct PieceRange {
    frame: u8,
    start: u16,
    end: u16,
}

/// Retained packet metadata addressing caller-owned frames by index and byte range.
///
/// No payload references or copies are retained. Keep frame slots and contents
/// unchanged until their last prepared packet has been submitted or discarded.
/// Already submitted slots may be released. Construction and reuse allocate nothing.
pub struct Batch {
    packets: [PacketPlan; BATCH_CAPACITY],
    prefixes: [[u8; RECORD_PREFIX_LEN]; BATCH_CAPACITY * 2],
    pieces: [PieceRange; BATCH_CAPACITY * 2],
    count: usize,
}

impl Default for Batch {
    fn default() -> Self {
        Self::new()
    }
}

impl Batch {
    /// Creates empty reusable packet metadata.
    pub const fn new() -> Self {
        Self {
            packets: [PacketPlan {
                header: [0; HEADER_LEN],
                packet: Packet {
                    number: 0,
                    length: 0,
                },
                first: 0,
                count: 0,
            }; BATCH_CAPACITY],
            prefixes: [[0; RECORD_PREFIX_LEN]; BATCH_CAPACITY * 2],
            pieces: [PieceRange {
                frame: 0,
                start: 0,
                end: 0,
            }; BATCH_CAPACITY * 2],
            count: 0,
        }
    }

    /// Forgets prepared packets without reusing their allocated packet numbers.
    pub const fn clear(&mut self) {
        self.count = 0;
    }

    /// Number of prepared logical UDP datagrams.
    pub const fn len(&self) -> usize {
        self.count
    }

    /// Whether there are no prepared datagrams.
    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Prepared identity and payload length, or None past the batch.
    pub const fn packet(&self, index: usize) -> Option<Packet> {
        if index < self.count {
            Some(self.packets[index].packet)
        } else {
            None
        }
    }

    /// Frame indices and byte ranges submitted by a particular packet.
    ///
    /// Advance frame offsets only after successful socket submission. A partial
    /// socket batch consumes exactly the returned packet prefix. After each
    /// attempted send, re-prepare the remaining ranges with fresh packet numbers.
    pub fn pieces(
        &self,
        packet: usize,
    ) -> Option<impl Iterator<Item = (usize, std::ops::Range<usize>)> + '_> {
        if packet >= self.count {
            return None;
        }
        let plan = &self.packets[packet];
        let first = usize::from(plan.first);
        Some(
            self.pieces[first..first + usize::from(plan.count)]
                .iter()
                .map(|piece| {
                    (
                        usize::from(piece.frame),
                        usize::from(piece.start)..usize::from(piece.end),
                    )
                }),
        )
    }

    /// Gathers one packet from unchanged source slots and retained wire metadata.
    ///
    /// Empty slots are allowed only when no piece of this packet names them.
    /// Invalid indices or undersized source frames return Frame before any I/O.
    pub fn slices<'a, 'out>(
        &'a self,
        packet: usize,
        frames: &[FrameInput<'a>],
        output: &'out mut [&'a [u8]],
    ) -> Result<&'out [&'a [u8]], PackError> {
        let plan = self
            .packets
            .get(packet)
            .filter(|_| packet < self.count)
            .ok_or(PackError::Frame)?;
        if output.len() < 1 + usize::from(plan.count) * 2 {
            return Err(PackError::Wire(WireError::BufferTooSmall));
        }
        output[0] = &plan.header;
        let mut used = 1;
        for (index, (frame, range)) in self.pieces(packet).unwrap().enumerate() {
            let frame = frames.get(frame).ok_or(PackError::Frame)?;
            let split = frame.header.len();
            let header = frame
                .header
                .get(range.start.min(split)..range.end.min(split))
                .ok_or(PackError::Frame)?;
            let bytes = frame
                .bytes
                .get(range.start.saturating_sub(split)..range.end.saturating_sub(split))
                .ok_or(PackError::Frame)?;
            let count = 1 + usize::from(!header.is_empty()) + usize::from(!bytes.is_empty());
            let slots = output
                .get_mut(used..used + count)
                .ok_or(PackError::Wire(WireError::BufferTooSmall))?;
            slots[0] = &self.prefixes[usize::from(plan.first) + index];
            let mut part = 1;
            for span in [header, bytes] {
                if !span.is_empty() {
                    slots[part] = span;
                    part += 1;
                }
            }
            used += count;
        }
        Ok(&output[..used])
    }

    /// Prepares up to 64 packets from at most 64 frame ranges, without copying payloads.
    ///
    /// All inputs are validated before changing the lane or this batch. Packet
    /// exhaustion can produce a shorter batch; it errors when no packet can be
    /// produced. Batches end on selected range boundaries within one GSO byte
    /// budget, except at packet-number exhaustion. Fresh DATA selects the full
    /// remaining frame; repairs retain its total length and identity on the wire.
    /// Keep unplanned frames' IDs and advance
    /// offsets using pieces() only after submission.
    /// Only prepare again after submitting or discarding the previous batch.
    pub fn prepare(
        &mut self,
        lane: &mut Lane,
        frames: &[FrameInput<'_>],
        limit: usize,
    ) -> Result<(), PackError> {
        if !(BASE_UDP_PAYLOAD..=MAX_UDP_PAYLOAD).contains(&limit) {
            return Err(PackError::Limit);
        }
        if frames.len() > BATCH_CAPACITY
            || frames.iter().any(|frame| {
                let length = frame.header.len().saturating_add(frame.bytes.len());
                !(14..=usize::from(lane.frame_limit)).contains(&length)
                    || frame.offset >= frame.end
                    || frame.end > length
                    || frame.id >> 48 != u64::from(lane.header.lane)
                    || frame.id & MAX_SEQUENCE == 0
                    || (lane.next_frame != 0 && frame.id & MAX_SEQUENCE >= lane.next_frame)
            })
        {
            return Err(PackError::Frame);
        }
        if !frames.is_empty() && lane.header.packet_number == 0 {
            return Err(PackError::Exhausted);
        }
        self.clear();
        let mut frame = 0;
        let mut offset = frames.first().map_or(0, |frame| frame.offset);
        let mut pieces = 0;
        let packet_limit = BATCH_CAPACITY.min(GSO_MAX_BYTES / limit);
        while frame < frames.len() && self.count < packet_limit && lane.header.packet_number != 0 {
            let first = pieces;
            let mut length = HEADER_LEN;
            while frame < frames.len()
                && pieces - first < MAX_RECORDS
                && length + RECORD_PREFIX_LEN < limit
            {
                let input = &frames[frame];
                let frame_len = input.header.len() + input.bytes.len();
                // Keep selected ranges within one GSO aggregate under socket overload.
                let available = limit - length - RECORD_PREFIX_LEN
                    + (packet_limit - self.count - 1) * (limit - DATA_PREFIX_LEN);
                if input.end - offset > available {
                    break;
                }
                let end = input.end.min(offset + limit - length - RECORD_PREFIX_LEN);
                let mut prefix = [0; DATA_PREFIX_LEN];
                // Each piece ends an input or a packet: 64 of each need at most 127 pieces.
                debug_assert!(pieces < self.pieces.len());
                encode_data_prefix_length(
                    lane.header,
                    input.id,
                    frame_len as u32,
                    offset as u32,
                    end - offset,
                    &mut prefix,
                )
                .map_err(PackError::Wire)?;
                self.prefixes[pieces].copy_from_slice(&prefix[HEADER_LEN..]);
                self.pieces[pieces] = PieceRange {
                    frame: frame as u8,
                    start: offset as u16,
                    end: end as u16,
                };
                pieces += 1;
                length += RECORD_PREFIX_LEN + end - offset;
                offset = end;
                if offset == input.end {
                    frame += 1;
                    offset = frames.get(frame).map_or(0, |frame| frame.offset);
                }
            }
            if pieces == first {
                break;
            }
            let plan = &mut self.packets[self.count];
            let mut header = lane.header;
            header.record_area_len = (length - HEADER_LEN) as u16;
            header.encode(&mut plan.header);
            plan.packet = Packet {
                number: header.packet_number,
                length,
            };
            plan.first = first as u16;
            plan.count = (pieces - first) as u16;
            lane.header.packet_number = header.packet_number.checked_add(1).unwrap_or(0);
            self.count += 1;
        }
        Ok(())
    }
}

/// Hashes an inner flow without allocation or parsing beyond the frame boundary.
///
/// The key includes MAC addresses, the VLAN stack and EtherType. IPv4/IPv6 add
/// addresses, protocol and TCP/UDP-style ports when available. All fragments
/// omit ports; IPv6 stops at the fragment header so later fragments agree even
/// when fragmentable extension headers precede the transport header. Malformed
/// IP uses only the Ethernet key. One unfragmented IPv4/IPv6/Ethernet
/// encapsulation adds its inner flow while retaining the outer key.
/// The hash is local, never a wire value.
pub fn flow_hash(frame: &[u8]) -> u64 {
    let (link, ip) = flow_key(frame);
    let mut hash = DefaultHasher::new();
    hash.write(link);
    if let Some(key) = ip {
        hash_ip(&mut hash, key);
        let inner = match key.1 {
            4 => ipv4(key.2),
            41 => ipv6(key.2),
            143 if key.2.len() >= 14 => {
                let (link, inner) = flow_key(key.2);
                let protocol = u16::from_be_bytes([link[link.len() - 2], link[link.len() - 1]]);
                if inner.is_some()
                    || !matches!(protocol, 0x0800 | 0x86dd | 0x8100 | 0x88a8 | 0x9100)
                {
                    hash.write(link);
                    inner
                } else {
                    None
                }
            }
            _ => None,
        };
        if let Some(key) = inner {
            hash_ip(&mut hash, key);
        }
    }
    hash.finish()
}

pub(crate) fn prefer_local_lane(frame: &[u8]) -> bool {
    // Stacked VLAN traffic can enter one TAP queue; spread it across all lanes.
    let (link, ip) = flow_key(frame);
    link.len() <= 18 && matches!(ip, Some((_, 17, ports)) if ports.len() == 4)
}

type IpKey<'a> = (&'a [u8], u8, &'a [u8]);

fn hash_ip(hash: &mut DefaultHasher, (addresses, protocol, tail): IpKey<'_>) {
    hash.write(addresses);
    hash.write_u8(protocol);
    if !matches!(protocol, 4 | 41 | 143) {
        hash.write(tail);
    }
}

fn flow_key(frame: &[u8]) -> (&[u8], Option<IpKey<'_>>) {
    if frame.len() < 14 {
        return (frame, None);
    }
    let mut offset = 14;
    let mut protocol = u16::from_be_bytes([frame[12], frame[13]]);
    while matches!(protocol, 0x8100 | 0x88a8 | 0x9100) {
        let Some(tag) = frame.get(offset..offset + 4) else {
            return (&frame[..offset], None);
        };
        protocol = u16::from_be_bytes([tag[2], tag[3]]);
        offset += 4;
    }
    let ip = &frame[offset..];
    let key = match protocol {
        0x0800 => ipv4(ip),
        0x86dd => ipv6(ip),
        _ => None,
    };
    (&frame[..offset], key)
}

fn flow_tail(protocol: u8, payload: &[u8]) -> Option<&[u8]> {
    if matches!(protocol, 6 | 17 | 33 | 132 | 136) {
        payload.get(..4)
    } else if matches!(protocol, 4 | 41 | 143) {
        Some(payload)
    } else {
        Some(&[])
    }
}

fn ipv4(ip: &[u8]) -> Option<IpKey<'_>> {
    if ip.len() < 20 || ip[0] >> 4 != 4 {
        return None;
    }
    let header = usize::from(ip[0] & 15) * 4;
    let length = usize::from(u16::from_be_bytes([ip[2], ip[3]]));
    if header < 20 || length < header || length > ip.len() {
        return None;
    }
    let fragmented = u16::from_be_bytes([ip[6], ip[7]]) & 0x3fff != 0;
    let tail = if fragmented {
        &[][..]
    } else {
        flow_tail(ip[9], &ip[header..length])?
    };
    Some((&ip[12..20], ip[9], tail))
}

fn ipv6(ip: &[u8]) -> Option<IpKey<'_>> {
    if ip.len() < 40 || ip[0] >> 4 != 6 {
        return None;
    }
    let length = 40 + usize::from(u16::from_be_bytes([ip[4], ip[5]]));
    if length > ip.len() {
        return None;
    }
    let mut next = ip[6];
    let mut offset = 40;
    loop {
        let payload = &ip[offset..length];
        let size = match next {
            0 | 43 | 60 => (usize::from(*payload.get(1)?) + 1) * 8,
            51 => {
                let size = (usize::from(*payload.get(1)?) + 2) * 4;
                if size < 12 {
                    return None;
                }
                size
            }
            44 => {
                let fragment = payload.get(..8)?;
                return Some((&ip[8..40], fragment[0], &[]));
            }
            _ => return Some((&ip[8..40], next, flow_tail(next, payload)?)),
        };
        if size > payload.len() {
            return None;
        }
        next = payload[0];
        offset += size;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handshake::{Params, Role, Tag};

    #[test]
    fn local_lanes_require_unfragmented_udp_and_at_most_one_tag() {
        for v6 in [false, true] {
            let frame = crate::tap::test_common::udp::frame(v6, 1, &[7; 22]);
            assert!(prefer_local_lane(&frame));
            for end in 0..frame.len() {
                assert!(!prefer_local_lane(&frame[..end]));
            }
            let mut tagged = frame.clone();
            tagged.splice(12..12, [0x81, 0, 0, 7]);
            assert!(prefer_local_lane(&tagged));
            for outer in [0x8100u16, 0x88a8, 0x9100] {
                let mut stacked = tagged.clone();
                stacked.splice(
                    12..12,
                    [outer.to_be_bytes()[0], outer.to_be_bytes()[1], 0, 42],
                );
                assert!(!prefer_local_lane(&stacked));
            }
            let mut tcp = frame.clone();
            tcp[if v6 { 20 } else { 23 }] = 6;
            assert!(!prefer_local_lane(&tcp));
            for (protocol, inner) in [
                (
                    4,
                    crate::tap::test_common::udp::frame(false, 1, &[7; 22])[14..].to_vec(),
                ),
                (
                    41,
                    crate::tap::test_common::udp::frame(true, 1, &[7; 22])[14..].to_vec(),
                ),
                (143, frame.clone()),
            ] {
                let mut outer = frame[..if v6 { 54 } else { 34 }].to_vec();
                outer[if v6 { 20 } else { 23 }] = protocol;
                outer.extend_from_slice(&inner);
                let length = (outer.len() - if v6 { 54 } else { 14 }) as u16;
                let at = if v6 { 18 } else { 16 };
                outer[at..at + 2].copy_from_slice(&length.to_be_bytes());
                assert!(!prefer_local_lane(&outer));
            }
            let mut fragment = frame;
            if v6 {
                fragment[20] = 44;
                let length = u16::from_be_bytes([fragment[18], fragment[19]]) + 8;
                fragment[18..20].copy_from_slice(&length.to_be_bytes());
                fragment.splice(54..54, [17, 0, 0, 1, 0, 0, 0, 1]);
            } else {
                fragment[20] = 0x20;
            }
            assert!(!prefer_local_lane(&fragment));
        }
    }

    pub(super) fn lane() -> Lane {
        let params = Params {
            port: 4444,
            max_frame_len: 9022,
            mandatory_features: 0,
            optional_features: 0,
        };
        Lane::new(
            UnquicUuid::from_bytes([1; 16]),
            Connection {
                role: Role::Initiator,
                initiator: Tag::new(1).unwrap(),
                responder: Tag::new(2).unwrap(),
                local: params,
                peer: params,
                max_frame_len: 9022,
            },
            1,
        )
        .unwrap()
    }

    #[test]
    fn batch_exhaustion_and_invalid_inputs_preserve_previous_packets() {
        let mut lane = lane();
        lane.next_frame = MAX_SEQUENCE;
        let id = lane.start_frame(1514).unwrap();
        assert!(matches!(lane.start_frame(14), Err(PackError::Exhausted)));
        let frames = [FrameInput {
            id,
            header: &[],
            bytes: &[1; 1514],
            offset: 0,
            end: 1514,
        }];
        lane.header.packet_number = u64::MAX;
        let mut batch = Batch::new();
        batch.prepare(&mut lane, &frames, BASE_UDP_PAYLOAD).unwrap();
        assert_eq!(batch.len(), 1);
        assert_eq!(batch.packet(0).unwrap().number, u64::MAX);
        assert_eq!(batch.pieces(0).unwrap().last().unwrap().1.end, 1164);
        let before = batch.packets[0].header;
        assert_eq!(
            batch.prepare(&mut lane, &frames, BASE_UDP_PAYLOAD),
            Err(PackError::Exhausted)
        );
        assert_eq!(batch.packets[0].header, before);
        assert_eq!(batch.len(), 1);
        let invalid = [FrameInput {
            offset: 1514,
            ..frames[0]
        }];
        assert_eq!(
            batch.prepare(&mut lane, &invalid, BASE_UDP_PAYLOAD),
            Err(PackError::Frame)
        );
        assert_eq!(batch.packets[0].header, before);
    }

    #[test]
    fn exhaustion_never_wraps_and_output_errors_do_not_consume_numbers() {
        let mut lane = lane();
        let mut output = [0; 1232];
        let before = output;
        assert!(lane.record(Record::Probe, &mut output[..1]).is_err());
        assert_eq!(output, before);
        assert_eq!(lane.record(Record::Probe, &mut output).unwrap().number, 1);
        lane.header.packet_number = u64::MAX;
        assert_eq!(
            lane.record(Record::Probe, &mut output).unwrap().number,
            u64::MAX
        );
        assert_eq!(
            lane.record(Record::Probe, &mut output),
            Err(PackError::Exhausted)
        );
        let mut prefix = [0xa5; PROBE_PREFIX_LEN];
        assert!(matches!(
            lane.probe(MAX_UDP_PAYLOAD, &mut prefix),
            Err(PackError::Exhausted)
        ));
        assert_eq!(prefix, [0xa5; PROBE_PREFIX_LEN]);
        lane.header.packet_number = u64::MAX;
        assert_eq!(
            lane.probe(MAX_UDP_PAYLOAD, &mut prefix)
                .unwrap()
                .packet
                .number,
            u64::MAX
        );
        assert!(matches!(
            lane.probe(MAX_UDP_PAYLOAD, &mut prefix),
            Err(PackError::Exhausted)
        ));
        lane.header.packet_number = 1;
        lane.next_frame = MAX_SEQUENCE;
        assert_eq!(lane.frame(&[0; 60]).unwrap().id(), (1 << 48) | MAX_SEQUENCE);
        assert!(matches!(lane.frame(&[0; 60]), Err(PackError::Exhausted)));
    }
}
