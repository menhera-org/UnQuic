//! Baseline DATA segmentation and deterministic inner-flow lane selection.
//!
//! A transmit owner keeps one Lane for the connection's whole lifetime. Packet
//! and frame counters never wrap or reuse identifiers. Replace it only through
//! the connection barrier. This initial encoder emits one record per datagram;
//! multi-record packing and negotiated larger payload ceilings follow later.

use crate::{
    BASE_UDP_PAYLOAD, MAX_FRAME_LEN, UnquicUuid,
    handshake::Connection,
    wire::{Header, Record, WireError, encode_datagram},
};
use std::{collections::hash_map::DefaultHasher, fmt, hash::Hasher};

const DATA_OVERHEAD: usize = 48 + 4 + 16;
const MAX_SEQUENCE: u64 = 0x0000_ffff_ffff_ffff;

/// Encoding failure, with exhausted identities requiring a new connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackError {
    /// Lane zero is reserved for handshakes.
    Lane,
    /// Frame is outside Ethernet or negotiated size limits.
    Frame,
    /// Packet numbers or frame sequences have run out; never wrap them.
    Exhausted,
    /// The record or supplied output buffer cannot be encoded.
    Wire(WireError),
}
impl fmt::Display for PackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lane => f.write_str("transmit lane must be nonzero"),
            Self::Frame => f.write_str("invalid frame length or negotiated ceiling"),
            Self::Exhausted => f.write_str("transmit identity space exhausted"),
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
        if !(14..=usize::from(self.frame_limit)).contains(&bytes.len()) {
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
        Ok(Frame {
            lane: self,
            bytes,
            id,
            offset: 0,
        })
    }

    /// Encodes one established record at the 1232-byte baseline.
    ///
    /// Errors leave the counter unchanged. After success retain the encoded
    /// bytes on WouldBlock, or discard them; re-encoding consumes a new number.
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
}

/// A borrowed frame split into complete baseline datagrams.
pub struct Frame<'a> {
    lane: &'a mut Lane,
    bytes: &'a [u8],
    id: u64,
    offset: usize,
}
impl Frame<'_> {
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

/// Hashes an inner flow without allocation or parsing beyond the frame boundary.
///
/// The key includes MAC addresses, the VLAN stack and EtherType. IPv4/IPv6 add
/// addresses, protocol and TCP/UDP-style ports when available. All fragments
/// omit ports; IPv6 stops at the fragment header so later fragments agree even
/// when fragmentable extension headers precede the transport header. Malformed
/// IP uses only the Ethernet key. The hash is local, never a wire value.
pub fn flow_hash(frame: &[u8]) -> u64 {
    let mut hash = DefaultHasher::new();
    if frame.len() < 14 {
        hash.write(frame);
        return hash.finish();
    }
    let mut offset = 14;
    let mut protocol = u16::from_be_bytes([frame[12], frame[13]]);
    while matches!(protocol, 0x8100 | 0x88a8 | 0x9100) {
        let Some(tag) = frame.get(offset..offset + 4) else {
            hash.write(&frame[..offset]);
            return hash.finish();
        };
        protocol = u16::from_be_bytes([tag[2], tag[3]]);
        offset += 4;
    }
    hash.write(&frame[..offset]);
    let ip = &frame[offset..];
    let key = match protocol {
        0x0800 => ipv4(ip),
        0x86dd => ipv6(ip),
        _ => None,
    };
    if let Some((addresses, protocol, ports)) = key {
        hash.write(addresses);
        hash.write_u8(protocol);
        hash.write(ports);
    }
    hash.finish()
}

type IpKey<'a> = (&'a [u8], u8, &'a [u8]);

fn ports(protocol: u8, payload: &[u8]) -> Option<&[u8]> {
    if matches!(protocol, 6 | 17 | 33 | 132 | 136) {
        payload.get(..4)
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
    let ports = if fragmented {
        &[][..]
    } else {
        ports(ip[9], &ip[header..length])?
    };
    Some((&ip[12..20], ip[9], ports))
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
            _ => return Some((&ip[8..40], next, ports(next, payload)?)),
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
        lane.header.packet_number = 1;
        lane.next_frame = MAX_SEQUENCE;
        assert_eq!(lane.frame(&[0; 60]).unwrap().id(), (1 << 48) | MAX_SEQUENCE);
        assert!(matches!(lane.frame(&[0; 60]), Err(PackError::Exhausted)));
    }
}
