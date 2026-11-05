//! Wire version 1, with explicit big-endian fields and allocation-free codecs.
//!
//! Decode a complete datagram before dispatching any records. This validates
//! wire invariants; connection identity, negotiated limits, sent-packet history,
//! and reassembly overlap checks belong to their state owners.

use crate::{MAX_FRAME_LEN, UnquicUuid};
use std::fmt;

/// Common header length in bytes.
pub const HEADER_LEN: usize = 48;
/// Common header plus one DATA record's fixed fields, excluding its piece bytes.
pub const DATA_PREFIX_LEN: usize = HEADER_LEN + 4 + 16;
/// Maximum number of records in one datagram.
pub const MAX_RECORDS: usize = 64;
/// Maximum number of ranges in one ACK record.
pub const MAX_ACK_RANGES: usize = 32;

/// The 48-byte common header. Magic, version and flags are fixed by the codec.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    /// Configured session admission cookie.
    pub cookie: UnquicUuid,
    /// Destination's receive tag, or zero for handshake packets.
    pub receive_tag: u128,
    /// Sender's lane, or zero for handshake packets.
    pub lane: u16,
    /// Exact bytes after the common header.
    pub record_area_len: u16,
    /// Sender's transmission number, or zero for handshake packets.
    pub packet_number: u64,
}

impl Header {
    /// Writes a common header; record validation is performed by encode_datagram.
    pub fn encode(&self, output: &mut [u8; HEADER_LEN]) {
        output[..4].copy_from_slice(&[0x55, 0x51, 1, 0]);
        output[4..20].copy_from_slice(self.cookie.as_bytes());
        output[20..36].copy_from_slice(&self.receive_tag.to_be_bytes());
        output[36..38].copy_from_slice(&self.lane.to_be_bytes());
        output[38..40].copy_from_slice(&self.record_area_len.to_be_bytes());
        output[40..48].copy_from_slice(&self.packet_number.to_be_bytes());
    }

    /// Decodes a header from a complete UDP payload, checking its exact length.
    pub fn decode(input: &[u8]) -> Result<Self, WireError> {
        if input.len() < HEADER_LEN {
            return Err(WireError::Length);
        }
        if input[..3] != [0x55, 0x51, 1] {
            return Err(WireError::Header);
        }
        if input[3] != 0 {
            return Err(WireError::Reserved);
        }
        let record_area_len = read16(input, 38);
        if input.len() - HEADER_LEN != usize::from(record_area_len) {
            return Err(WireError::Length);
        }
        Ok(Self {
            cookie: UnquicUuid::from_bytes(input[4..20].try_into().unwrap()),
            receive_tag: read128(input, 20),
            lane: read16(input, 36),
            record_area_len,
            packet_number: read64(input, 40),
        })
    }
}

/// Parameters advertised by HELLO and CHALLENGE.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Params {
    /// Configured fixed receive port; never zero.
    pub port: u16,
    /// Supported maximum Ethernet frame size, 1514 through 9022 bytes.
    pub max_frame_len: u16,
    /// Must be zero in version 1.
    pub mandatory_features: u32,
    /// Unknown bits are preserved by the codec and ignored by negotiation.
    /// Version 1 senders advertise zero.
    pub optional_features: u32,
}

impl Params {
    /// Validates fields independently of the locally configured peer.
    pub const fn validate(self) -> Result<(), WireError> {
        if self.port == 0
            || self.max_frame_len < 1514
            || self.max_frame_len as usize > MAX_FRAME_LEN
            || self.mandatory_features != 0
        {
            Err(WireError::Parameters)
        } else {
            Ok(())
        }
    }

    fn decode(input: &[u8]) -> Self {
        Self {
            port: read16(input, 0),
            max_frame_len: read16(input, 2),
            mandatory_features: read32(input, 4),
            optional_features: read32(input, 8),
        }
    }

    fn encode(self, output: &mut [u8]) {
        output[..2].copy_from_slice(&self.port.to_be_bytes());
        output[2..4].copy_from_slice(&self.max_frame_len.to_be_bytes());
        output[4..8].copy_from_slice(&self.mandatory_features.to_be_bytes());
        output[8..12].copy_from_slice(&self.optional_features.to_be_bytes());
    }
}

/// An inclusive packet-number range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AckRange {
    /// Lowest acknowledged number in this range.
    pub start: u64,
    /// Highest acknowledged number in this range.
    pub end: u64,
}

#[derive(Clone, Copy, Debug)]
enum RangeSource<'a> {
    Wire(&'a [u8]),
    Values(&'a [AckRange]),
}

/// Validated, borrowed ACK ranges, newest first.
///
/// Values supplied by a sender and bytes borrowed from a received datagram use
/// the same iterator; no native struct is cast onto wire storage.
#[derive(Clone, Copy, Debug)]
pub struct AckRanges<'a>(RangeSource<'a>);

impl<'a> AckRanges<'a> {
    /// Validates one through 32 descending, disjoint, nonadjacent ranges.
    pub fn new(ranges: &'a [AckRange]) -> Result<Self, WireError> {
        let ranges = Self(RangeSource::Values(ranges));
        ranges.validate()?;
        Ok(ranges)
    }

    /// Number of ranges, always between one and 32.
    pub const fn len(self) -> usize {
        match self.0 {
            RangeSource::Wire(bytes) => bytes.len() / 16,
            RangeSource::Values(ranges) => ranges.len(),
        }
    }

    /// Always false: an empty ACK range list is invalid.
    pub const fn is_empty(self) -> bool {
        self.len() == 0
    }

    /// Iterates over range pairs, never over the packet numbers they span.
    pub fn iter(self) -> impl ExactSizeIterator<Item = AckRange> + 'a {
        (0..self.len()).map(move |i| match self.0 {
            RangeSource::Wire(bytes) => AckRange {
                start: read64(bytes, i * 16),
                end: read64(bytes, i * 16 + 8),
            },
            RangeSource::Values(ranges) => ranges[i],
        })
    }

    fn validate(self) -> Result<(), WireError> {
        if self.is_empty() || self.len() > MAX_ACK_RANGES {
            return Err(WireError::Ack);
        }
        let mut previous_start = None;
        for range in self.iter() {
            if range.start == 0
                || range.start > range.end
                || previous_start.is_some_and(|start| range.end >= start - 1)
            {
                return Err(WireError::Ack);
            }
            previous_start = Some(range.start);
        }
        Ok(())
    }
}

/// One version-1 record. DATA and ACK ranges borrow caller-owned storage.
#[derive(Clone, Copy, Debug)]
pub enum Record<'a> {
    /// This many zero body bytes, excluding the four-byte record prefix.
    Padding {
        /// Number of zero body bytes.
        length: u16,
    },
    /// One nonempty piece of an Ethernet frame.
    Data {
        /// Lane in the high 16 bits, nonzero sequence in the low 48.
        frame_id: u64,
        /// Complete Ethernet frame length.
        total_len: u32,
        /// Byte offset of this piece.
        offset: u32,
        /// Borrowed frame bytes.
        bytes: &'a [u8],
    },
    /// Transport acceptance of packets on the explicitly named sender lane.
    Ack {
        /// Acknowledged sender lane.
        lane: u16,
        /// Delay for the highest acknowledged packet, at most 1000 microseconds.
        delay_us: u32,
        /// Newest ranges first.
        ranges: AckRanges<'a>,
    },
    /// Initial handshake message.
    Hello {
        /// Initiator's nonzero receive tag.
        initiator: u128,
        /// Initiator's advertised parameters.
        params: Params,
    },
    /// Responder's handshake challenge.
    Challenge {
        /// Initiator's receive tag.
        initiator: u128,
        /// Responder's nonzero receive tag, distinct from the initiator's.
        responder: u128,
        /// Responder's advertised parameters.
        params: Params,
    },
    /// ACK-eliciting probe; the common header identifies the transmission.
    Probe,
    /// Reply to one live probe, independently of packet ACK accounting.
    ProbeReply {
        /// Original probe lane.
        lane: u16,
        /// Original probe packet number.
        packet_number: u64,
        /// Actual received UDP payload length.
        payload_len: u16,
    },
    /// Advisory cancellation of a frame.
    FrameAbandon {
        /// Frame's sending lane.
        lane: u16,
        /// Zero for the frame sender, one for the frame receiver.
        origin: u8,
        /// Deadline (1), resource limit (2), retirement (3), or TAP failure (4).
        reason: u8,
        /// Frame ID whose high bits must name lane.
        frame_id: u64,
    },
    /// Initiator confirms the pending pair.
    Confirm {
        /// Initiator's receive tag.
        initiator: u128,
        /// Responder's receive tag.
        responder: u128,
    },
    /// Responder confirms installation of the pair.
    Ready {
        /// Initiator's receive tag.
        initiator: u128,
        /// Responder's receive tag.
        responder: u128,
    },
    /// Retires a sender lane distinct from the carrying lane.
    LaneRetire {
        /// Lane to retire.
        lane: u16,
    },
}

impl Record<'_> {
    /// Encoded size including the four-byte record prefix.
    pub const fn encoded_len(self) -> usize {
        4 + match self {
            Self::Padding { length } => length as usize,
            Self::Data { bytes, .. } => 16 + bytes.len(),
            Self::Ack { ranges, .. } => 8 + 16 * ranges.len(),
            Self::Hello { .. } => 28,
            Self::Challenge { .. } => 44,
            Self::Probe => 0,
            Self::ProbeReply { .. } => 16,
            Self::FrameAbandon { .. } => 12,
            Self::Confirm { .. } | Self::Ready { .. } => 32,
            Self::LaneRetire { .. } => 4,
        }
    }

    /// Whether this record causes transport ACK scheduling.
    pub const fn ack_eliciting(self) -> bool {
        matches!(
            self,
            Self::Data { .. } | Self::Probe | Self::FrameAbandon { .. } | Self::LaneRetire { .. }
        )
    }

    /// Whether this record must be isolated in a handshake datagram.
    pub const fn is_handshake(self) -> bool {
        matches!(
            self,
            Self::Hello { .. } | Self::Challenge { .. } | Self::Confirm { .. } | Self::Ready { .. }
        )
    }

    /// Encodes one record without allocation. Failure leaves output unchanged.
    ///
    /// Lane relationships to the common header are checked by encode_datagram.
    pub fn encode(self, output: &mut [u8]) -> Result<usize, WireError> {
        self.validate()?;
        let length = self.encoded_len();
        if output.len() < length {
            return Err(WireError::BufferTooSmall);
        }
        let kind = match self {
            Self::Padding { .. } => 0,
            Self::Data { .. } => 1,
            Self::Ack { .. } => 2,
            Self::Hello { .. } => 3,
            Self::Challenge { .. } => 4,
            Self::Probe => 5,
            Self::ProbeReply { .. } => 6,
            Self::FrameAbandon { .. } => 7,
            Self::Confirm { .. } => 8,
            Self::Ready { .. } => 9,
            Self::LaneRetire { .. } => 10,
        };
        output[..4].copy_from_slice(&[kind, 0, (length >> 8) as u8, length as u8]);
        let body = &mut output[4..length];
        match self {
            Self::Padding { .. } => body.fill(0),
            Self::Data {
                frame_id,
                total_len,
                offset,
                bytes,
            } => {
                encode_data_fields(frame_id, total_len, offset, body);
                body[16..].copy_from_slice(bytes);
            }
            Self::Ack {
                lane,
                delay_us,
                ranges,
            } => {
                body[..2].copy_from_slice(&lane.to_be_bytes());
                body[2] = ranges.len() as u8;
                body[3] = 0;
                body[4..8].copy_from_slice(&delay_us.to_be_bytes());
                for (bytes, range) in body[8..]
                    .as_chunks_mut::<16>()
                    .0
                    .iter_mut()
                    .zip(ranges.iter())
                {
                    bytes[..8].copy_from_slice(&range.start.to_be_bytes());
                    bytes[8..].copy_from_slice(&range.end.to_be_bytes());
                }
            }
            Self::Hello { initiator, params } => {
                body[..16].copy_from_slice(&initiator.to_be_bytes());
                params.encode(&mut body[16..]);
            }
            Self::Challenge {
                initiator,
                responder,
                params,
            } => {
                body[..16].copy_from_slice(&initiator.to_be_bytes());
                body[16..32].copy_from_slice(&responder.to_be_bytes());
                params.encode(&mut body[32..]);
            }
            Self::Probe => {}
            Self::ProbeReply {
                lane,
                packet_number,
                payload_len,
            } => {
                body[..2].copy_from_slice(&lane.to_be_bytes());
                body[2..4].fill(0);
                body[4..12].copy_from_slice(&packet_number.to_be_bytes());
                body[12..14].copy_from_slice(&payload_len.to_be_bytes());
                body[14..16].fill(0);
            }
            Self::FrameAbandon {
                lane,
                origin,
                reason,
                frame_id,
            } => {
                body[..2].copy_from_slice(&lane.to_be_bytes());
                body[2] = origin;
                body[3] = reason;
                body[4..12].copy_from_slice(&frame_id.to_be_bytes());
            }
            Self::Confirm {
                initiator,
                responder,
            }
            | Self::Ready {
                initiator,
                responder,
            } => {
                body[..16].copy_from_slice(&initiator.to_be_bytes());
                body[16..32].copy_from_slice(&responder.to_be_bytes());
            }
            Self::LaneRetire { lane } => {
                body[..2].copy_from_slice(&lane.to_be_bytes());
                body[2..4].fill(0);
            }
        }
        Ok(length)
    }

    pub(crate) fn validate(self) -> Result<(), WireError> {
        if self.encoded_len() > u16::MAX as usize {
            return Err(WireError::Length);
        }
        match self {
            Self::Padding { .. } | Self::Probe => {}
            Self::Data {
                frame_id,
                total_len,
                offset,
                bytes,
            } => {
                if frame_id >> 48 == 0
                    || frame_id & 0x0000_ffff_ffff_ffff == 0
                    || !(14..=MAX_FRAME_LEN as u32).contains(&total_len)
                    || bytes.is_empty()
                    || offset > total_len
                    || bytes.len() > (total_len - offset) as usize
                {
                    return Err(WireError::Data);
                }
            }
            Self::Ack {
                lane,
                delay_us,
                ranges,
            } => {
                if lane == 0 || delay_us > 1000 {
                    return Err(WireError::Ack);
                }
                ranges.validate()?;
            }
            Self::Hello { initiator, params } => {
                if initiator == 0 {
                    return Err(WireError::Handshake);
                }
                params.validate()?;
            }
            Self::Challenge {
                initiator,
                responder,
                params,
            } => {
                validate_pair(initiator, responder)?;
                params.validate()?;
            }
            Self::Confirm {
                initiator,
                responder,
            }
            | Self::Ready {
                initiator,
                responder,
            } => {
                validate_pair(initiator, responder)?;
            }
            Self::ProbeReply {
                lane,
                packet_number,
                payload_len,
            } => {
                if lane == 0 || packet_number == 0 || usize::from(payload_len) < HEADER_LEN + 4 {
                    return Err(WireError::Probe);
                }
            }
            Self::FrameAbandon {
                lane,
                origin,
                reason,
                frame_id,
            } => {
                if lane == 0
                    || frame_id >> 48 != u64::from(lane)
                    || frame_id & 0x0000_ffff_ffff_ffff == 0
                    || origin > 1
                    || !(1..=4).contains(&reason)
                {
                    return Err(WireError::Abandon);
                }
            }
            Self::LaneRetire { lane } => {
                if lane == 0 {
                    return Err(WireError::Lane);
                }
            }
        }
        Ok(())
    }

    fn validate_context(self, header: Header, count: usize) -> Result<(), WireError> {
        if self.is_handshake() {
            if count != 1
                || header.receive_tag != 0
                || header.lane != 0
                || header.packet_number != 0
            {
                return Err(WireError::Handshake);
            }
        } else if header.receive_tag == 0 || header.lane == 0 || header.packet_number == 0 {
            return Err(WireError::Handshake);
        }
        match self {
            Self::Data { frame_id, .. } if frame_id >> 48 != u64::from(header.lane) => {
                Err(WireError::Lane)
            }
            Self::FrameAbandon {
                lane, origin: 0, ..
            } if lane != header.lane => Err(WireError::Lane),
            Self::LaneRetire { lane } if lane == header.lane => Err(WireError::Lane),
            _ => Ok(()),
        }
    }
}

/// A fully validated datagram borrowing its input buffer.
///
/// Validation precedes record iteration, so an invalid trailing record cannot
/// cause partial dispatch. Iteration decodes fields from immutable storage.
#[derive(Clone, Copy, Debug)]
pub struct Datagram<'a> {
    header: Header,
    area: &'a [u8],
}

impl<'a> Datagram<'a> {
    /// Checks the complete record area and all header/record relationships.
    pub fn decode(input: &'a [u8]) -> Result<Self, WireError> {
        let header = Header::decode(input)?;
        let area = &input[HEADER_LEN..];
        let mut remaining = area;
        let mut count = 0;
        while !remaining.is_empty() {
            count += 1;
            if count > MAX_RECORDS {
                return Err(WireError::RecordCount);
            }
            let (record, length) = decode_record(remaining)?;
            // A handshake must consume the entire area on its first record.
            let context_count = if length == area.len() { 1 } else { 2 };
            record.validate_context(header, context_count)?;
            remaining = &remaining[length..];
        }
        if count == 0 {
            return Err(WireError::RecordCount);
        }
        Ok(Self { header, area })
    }

    /// Returns the validated header.
    pub const fn header(self) -> Header {
        self.header
    }

    /// Iterates over records only after the entire datagram passed validation.
    pub fn records(self) -> impl Iterator<Item = Record<'a>> {
        let mut remaining = self.area;
        std::iter::from_fn(move || {
            if remaining.is_empty() {
                return None;
            }
            let (record, length) =
                decode_record(remaining).expect("validated immutable record area");
            remaining = &remaining[length..];
            Some(record)
        })
    }
}

/// Writes a whole datagram, computing record_area_len from the supplied records.
///
/// Other header fields are preserved. Validation and capacity checks precede
/// all writes; errors leave output unchanged.
pub fn encode_datagram(
    mut header: Header,
    records: &[Record<'_>],
    output: &mut [u8],
) -> Result<usize, WireError> {
    if records.is_empty() || records.len() > MAX_RECORDS {
        return Err(WireError::RecordCount);
    }
    let mut length = 0usize;
    for &record in records {
        record.validate()?;
        record.validate_context(header, records.len())?;
        length += record.encoded_len();
    }
    header.record_area_len = u16::try_from(length).map_err(|_| WireError::Length)?;
    let length = length + HEADER_LEN;
    if output.len() < length {
        return Err(WireError::BufferTooSmall);
    }
    header.encode((&mut output[..HEADER_LEN]).try_into().unwrap());
    let mut at = HEADER_LEN;
    for &record in records {
        at += record.encode(&mut output[at..])?;
    }
    Ok(length)
}

/// Encodes only the fixed prefix of one DATA datagram for scatter/gather I/O.
///
/// The supplied record must be DATA. Send this prefix and its borrowed bytes as
/// two buffers in the same datagram. Returns their combined length. Validation
/// matches encode_datagram and finishes before modifying output.
pub fn encode_data_prefix(
    mut header: Header,
    data: Record<'_>,
    output: &mut [u8; DATA_PREFIX_LEN],
) -> Result<usize, WireError> {
    let Record::Data {
        frame_id,
        total_len,
        offset,
        bytes,
    } = data
    else {
        return Err(WireError::Data);
    };
    data.validate()?;
    data.validate_context(header, 1)?;
    let length = data.encoded_len();
    header.record_area_len = length as u16;
    header.encode((&mut output[..HEADER_LEN]).try_into().unwrap());
    output[HEADER_LEN..HEADER_LEN + 4].copy_from_slice(&[1, 0, (length >> 8) as u8, length as u8]);
    encode_data_fields(frame_id, total_len, offset, &mut output[HEADER_LEN + 4..]);
    Ok(DATA_PREFIX_LEN + bytes.len())
}

fn encode_data_fields(frame_id: u64, total_len: u32, offset: u32, body: &mut [u8]) {
    body[..8].copy_from_slice(&frame_id.to_be_bytes());
    body[8..12].copy_from_slice(&total_len.to_be_bytes());
    body[12..16].copy_from_slice(&offset.to_be_bytes());
}

fn decode_record(input: &[u8]) -> Result<(Record<'_>, usize), WireError> {
    if input.len() < 4 {
        return Err(WireError::Length);
    }
    if input[1] != 0 {
        return Err(WireError::Reserved);
    }
    let length = usize::from(read16(input, 2));
    if length < 4 || length > input.len() {
        return Err(WireError::Length);
    }
    let body = &input[4..length];
    let record = match input[0] {
        0 => {
            if body.iter().any(|&byte| byte != 0) {
                return Err(WireError::Reserved);
            }
            Record::Padding {
                length: body.len() as u16,
            }
        }
        1 if body.len() >= 17 => Record::Data {
            frame_id: read64(body, 0),
            total_len: read32(body, 8),
            offset: read32(body, 12),
            bytes: &body[16..],
        },
        2 if body.len() >= 8 => {
            if body[3] != 0 {
                return Err(WireError::Reserved);
            }
            let count = usize::from(body[2]);
            if count == 0 || count > MAX_ACK_RANGES || body.len() != 8 + 16 * count {
                return Err(WireError::Ack);
            }
            Record::Ack {
                lane: read16(body, 0),
                delay_us: read32(body, 4),
                ranges: AckRanges(RangeSource::Wire(&body[8..])),
            }
        }
        3 if body.len() == 28 => Record::Hello {
            initiator: read128(body, 0),
            params: Params::decode(&body[16..]),
        },
        4 if body.len() == 44 => Record::Challenge {
            initiator: read128(body, 0),
            responder: read128(body, 16),
            params: Params::decode(&body[32..]),
        },
        5 if body.is_empty() => Record::Probe,
        6 if body.len() == 16 => {
            if body[2..4] != [0, 0] || body[14..16] != [0, 0] {
                return Err(WireError::Reserved);
            }
            Record::ProbeReply {
                lane: read16(body, 0),
                packet_number: read64(body, 4),
                payload_len: read16(body, 12),
            }
        }
        7 if body.len() == 12 => Record::FrameAbandon {
            lane: read16(body, 0),
            origin: body[2],
            reason: body[3],
            frame_id: read64(body, 4),
        },
        8 if body.len() == 32 => Record::Confirm {
            initiator: read128(body, 0),
            responder: read128(body, 16),
        },
        9 if body.len() == 32 => Record::Ready {
            initiator: read128(body, 0),
            responder: read128(body, 16),
        },
        10 if body.len() == 4 => {
            if body[2..4] != [0, 0] {
                return Err(WireError::Reserved);
            }
            Record::LaneRetire {
                lane: read16(body, 0),
            }
        }
        1..=10 => return Err(WireError::Length),
        _ => return Err(WireError::UnknownRecord),
    };
    record.validate()?;
    Ok((record, length))
}

const fn validate_pair(initiator: u128, responder: u128) -> Result<(), WireError> {
    if initiator == 0 || responder == 0 || initiator == responder {
        Err(WireError::Handshake)
    } else {
        Ok(())
    }
}
fn read16(input: &[u8], at: usize) -> u16 {
    u16::from_be_bytes(input[at..at + 2].try_into().unwrap())
}
fn read32(input: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(input[at..at + 4].try_into().unwrap())
}
fn read64(input: &[u8], at: usize) -> u64 {
    u64::from_be_bytes(input[at..at + 8].try_into().unwrap())
}
fn read128(input: &[u8], at: usize) -> u128 {
    u128::from_be_bytes(input[at..at + 16].try_into().unwrap())
}

/// A wire validation or encoding error. No cookie or payload is included.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WireError {
    /// Incorrect magic or unsupported version.
    Header,
    /// Truncated, oversized, or inconsistent length.
    Length,
    /// Nonzero reserved flag, reserved field, or padding byte.
    Reserved,
    /// Unsupported record type.
    UnknownRecord,
    /// Empty record area or more than 64 records.
    RecordCount,
    /// Invalid frame identity, total length, or byte range.
    Data,
    /// Invalid ACK lane, delay, count, or range ordering.
    Ack,
    /// Invalid handshake tags, isolation, or zero-header fields.
    Handshake,
    /// Invalid advertised port, frame limit, or mandatory features.
    Parameters,
    /// Invalid lane identity or relationship to the carrying lane.
    Lane,
    /// Invalid probe reply fields.
    Probe,
    /// Invalid abandonment origin, reason, or frame identity.
    Abandon,
    /// Caller-provided output buffer is too small.
    BufferTooSmall,
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Header => "invalid wire magic or version",
            Self::Length => "invalid wire length",
            Self::Reserved => "nonzero reserved field",
            Self::UnknownRecord => "unknown record type",
            Self::RecordCount => "invalid record count",
            Self::Data => "invalid DATA record",
            Self::Ack => "invalid ACK record",
            Self::Handshake => "invalid handshake record or envelope",
            Self::Parameters => "invalid handshake parameters",
            Self::Lane => "invalid lane identity",
            Self::Probe => "invalid probe reply",
            Self::Abandon => "invalid frame abandonment",
            Self::BufferTooSmall => "wire output buffer too small",
        })
    }
}
impl std::error::Error for WireError {}
