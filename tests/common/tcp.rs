//! TCP frame fixtures with independently computed full and partial checksums.
use super::udp::{fold, sum};

pub fn frame(v6: bool, id: u16, seq: u32, flags: u8, options: &[u8], payload: &[u8]) -> Vec<u8> {
    assert!(options.len() <= 40 && options.len().is_multiple_of(4));
    let at = if v6 { 54 } else { 34 };
    let length = 20 + options.len() + payload.len();
    let mut bytes = super::udp::frame(v6, id, &vec![0; length - 8]);
    bytes[at..].fill(0);
    bytes[at..at + 4].copy_from_slice(&[0x23, 0x28, 0x27, 0x0f]);
    bytes[at + 4..at + 8].copy_from_slice(&seq.to_be_bytes());
    bytes[at + 8..at + 12].copy_from_slice(&12345u32.to_be_bytes());
    bytes[at + 12] = (((20 + options.len()) / 4) as u8) << 4;
    bytes[at + 13] = flags;
    bytes[at + 14..at + 16].copy_from_slice(&8192u16.to_be_bytes());
    bytes[at + 20..at + 20 + options.len()].copy_from_slice(options);
    bytes[at + 20 + options.len()..].copy_from_slice(payload);
    bytes[if v6 { 20 } else { 23 }] = 6;
    if !v6 {
        bytes[24..26].fill(0);
        let checksum = !fold(sum(&bytes[14..34]));
        bytes[24..26].copy_from_slice(&checksum.to_be_bytes());
    }
    let pseudo = sum(if v6 { &bytes[22..54] } else { &bytes[26..34] }) + 6 + length as u32;
    let checksum = !fold(pseudo + sum(&bytes[at..]));
    bytes[at + 16..at + 18].copy_from_slice(&checksum.to_be_bytes());
    bytes
}

pub fn partial(bytes: &mut [u8], v6: bool, size: u16) -> [u8; 10] {
    let at = if v6 { 54 } else { 34 };
    let pseudo =
        sum(if v6 { &bytes[22..54] } else { &bytes[26..34] }) + 6 + (bytes.len() - at) as u32;
    bytes[at + 16..at + 18].copy_from_slice(&fold(pseudo).to_be_bytes());
    let mut header = [0; 10];
    header[0] = 1;
    header[1] = (if v6 { 4 } else { 1 }) | (bytes[at + 13] & 0x80);
    header[2..4]
        .copy_from_slice(&((at + usize::from(bytes[at + 12] >> 4) * 4) as u16).to_ne_bytes());
    header[4..6].copy_from_slice(&size.to_ne_bytes());
    header[6..8].copy_from_slice(&(at as u16).to_ne_bytes());
    header[8..10].copy_from_slice(&16u16.to_ne_bytes());
    header
}

/// IPv4 options or an active IPv6 SRH preceded by destination options.
pub fn extend(bytes: &mut Vec<u8>, v6: bool) -> usize {
    if v6 {
        let mut extension = [0; 32];
        extension[0] = 43;
        extension[8] = 6;
        extension[9] = 2;
        extension[10] = 4;
        extension[11] = 1;
        extension[16..].copy_from_slice(&bytes[38..54]);
        bytes[38..54].fill(9);
        bytes.splice(54..54, extension);
        bytes[20] = 60;
        let length = (bytes.len() - 54) as u16;
        bytes[18..20].copy_from_slice(&length.to_be_bytes());
        32
    } else {
        bytes.splice(34..34, [1; 8]);
        bytes[14] = 0x47;
        let length = (bytes.len() - 14) as u16;
        bytes[16..18].copy_from_slice(&length.to_be_bytes());
        bytes[24..26].fill(0);
        let checksum = !fold(sum(&bytes[14..42]));
        bytes[24..26].copy_from_slice(&checksum.to_be_bytes());
        8
    }
}

pub fn repair(bytes: &mut [u8], v6: bool) {
    let at = if v6 { 54 } else { 34 };
    partial(bytes, v6, 1);
    let checksum = !fold(sum(&bytes[at..]));
    bytes[at + 16..at + 18].copy_from_slice(&checksum.to_be_bytes());
}
