//! Lossless coalescing of validated IPv4/IPv6 UDP frames with up to two VLAN tags.
use crate::pack::flow_hash;
use std::{io, os::fd::RawFd};

pub(super) fn sum(bytes: &[u8]) -> u32 {
    let (pairs, tail) = bytes.as_chunks::<2>();
    let native = pairs
        .iter()
        .map(|&p| u32::from(u16::from_ne_bytes(p)))
        .sum::<u32>()
        + tail
            .first()
            .map_or(0, |&b| u32::from(u16::from_ne_bytes([b, 0])));
    // Fold carries before the byte swap; this avoids swapping every input word.
    u32::from(fold(native).to_be())
}

pub(super) fn fold(mut sum: u32) -> u16 {
    while sum > 65535 {
        sum = (sum & 65535) + (sum >> 16);
    }
    sum as u16
}
fn word(bytes: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([bytes[at], bytes[at + 1]])
}
fn pseudo(frame: &[u8], ip: usize, at: usize, length: usize) -> u32 {
    sum(if at - ip == 20 {
        &frame[ip + 12..at]
    } else {
        &frame[ip + 8..at]
    }) + 17
        + length as u32
}
fn transport(frame: &[u8]) -> Option<(usize, usize)> {
    let mut ip = 14;
    let mut protocol = u16::from_be_bytes(frame.get(12..14)?.try_into().ok()?);
    for _ in 0..2 {
        if !matches!(protocol, 0x8100 | 0x88a8) {
            break;
        }
        let tag = frame.get(ip..ip + 4)?;
        protocol = u16::from_be_bytes([tag[2], tag[3]]);
        ip += 4;
    }
    let at = match protocol {
        0x0800
            if frame.len() >= ip + 29
                && frame[ip] == 0x45
                && word(frame, ip + 2) as usize == frame.len() - ip
                && word(frame, ip + 6) == 0x4000
                && frame[ip + 9] == 17
                && fold(sum(&frame[ip..ip + 20])) == 65535 =>
        {
            ip + 20
        }
        0x86dd
            if frame.len() >= ip + 49
                && frame[ip] >> 4 == 6
                && word(frame, ip + 4) as usize == frame.len() - ip - 40
                && frame[ip + 6] == 17 =>
        {
            ip + 40
        }
        _ => return None,
    };
    let length = frame.len() - at;
    (word(frame, at + 4) as usize == length
        && word(frame, at + 6) != 0
        && fold(pseudo(frame, ip, at, length) + sum(&frame[at..])) == 65535)
        .then_some((ip, at))
}
fn matches(first: &[u8], next: &[u8], ip: usize, at: usize, count: usize) -> bool {
    first[..ip] == next[..ip]
        && first[at..at + 4] == next[at..at + 4]
        && if at - ip == 20 {
            first[ip..ip + 2] == next[ip..ip + 2]
                && first[ip + 6..ip + 10] == next[ip + 6..ip + 10]
                && first[ip + 12..at] == next[ip + 12..at]
                && word(next, ip + 4) == word(first, ip + 4).wrapping_add(count as u16)
        } else {
            first[ip..ip + 4] == next[ip..ip + 4] && first[ip + 6..at] == next[ip + 6..at]
        }
}

/// Returns the attempted frame mask and original byte count, including on failure.
/// A failed atomic TAP write retains every frame until the caller decides its fate.
pub(super) fn write<'a>(
    fd: RawFd,
    first: &'a [u8],
    mut frames: impl Iterator<Item = &'a [u8]>,
    frame_limit: usize,
) -> Option<(u64, io::Result<usize>)> {
    let second = frames.next()?;
    let (ip, at) = transport(first)?;
    let header_len = at + 8;
    let size = first.len() - header_len;
    let hash = flow_hash(first);
    let mut vnet = [0u8; 10];
    let mut header = [0u8; 70];
    let mut iov = [libc::iovec {
        iov_base: std::ptr::null_mut(),
        iov_len: 0,
    }; 66];
    let mut count = 1;
    let mut mask = 1u64;
    let mut payload = size;
    let mut original_bytes = first.len();
    iov[2] = libc::iovec {
        iov_base: first[header_len..].as_ptr().cast_mut().cast(),
        iov_len: size,
    };
    for (index, next) in std::iter::once(second).chain(frames).take(63).enumerate() {
        if flow_hash(next) != hash {
            continue;
        }
        // A same-flow barrier must prevent moving a later frame ahead of it.
        if next.len() > frame_limit
            || transport(next) != Some((ip, at))
            || !matches(first, next, ip, at, count)
        {
            break;
        }
        let length = next.len() - header_len;
        if length > size || header_len + payload + length > 65535 {
            break;
        }
        iov[count + 2] = libc::iovec {
            iov_base: next[header_len..].as_ptr().cast_mut().cast(),
            iov_len: length,
        };
        count += 1;
        mask |= 1 << (index + 1);
        payload += length;
        original_bytes += next.len();
        if length < size {
            break;
        }
    }
    if count == 1 {
        return None;
    }
    header[..header_len].copy_from_slice(&first[..header_len]);
    let udp_len = payload + 8;
    header[at + 4..at + 6].copy_from_slice(&(udp_len as u16).to_be_bytes());
    if at - ip == 20 {
        header[ip + 2..ip + 4].copy_from_slice(&((udp_len + 20) as u16).to_be_bytes());
        header[ip + 10..ip + 12].fill(0);
        let checksum = !fold(sum(&header[ip..at]));
        header[ip + 10..ip + 12].copy_from_slice(&checksum.to_be_bytes());
    } else {
        header[ip + 4..ip + 6].copy_from_slice(&(udp_len as u16).to_be_bytes());
    }
    let checksum = fold(pseudo(&header, ip, at, udp_len));
    header[at + 6..at + 8].copy_from_slice(&checksum.to_be_bytes());
    vnet[0] = 1; // VIRTIO_NET_HDR_F_NEEDS_CSUM
    vnet[1] = 5; // VIRTIO_NET_HDR_GSO_UDP_L4
    for (offset, value) in [
        (2, header_len as u16),
        (4, size as u16),
        (6, at as u16),
        (8, 6),
    ] {
        vnet[offset..offset + 2].copy_from_slice(&value.to_ne_bytes());
    }
    iov[0] = libc::iovec {
        iov_base: vnet.as_mut_ptr().cast(),
        iov_len: vnet.len(),
    };
    iov[1] = libc::iovec {
        iov_base: header.as_mut_ptr().cast(),
        iov_len: header_len,
    };
    // SAFETY: header and every borrowed payload remain readable until writev returns.
    let n = unsafe { libc::writev(fd, iov.as_ptr(), (count + 2) as _) };
    let result = if n < 0 {
        Err(io::Error::last_os_error())
    } else if n as usize != vnet.len() + header_len + payload {
        Err(io::ErrorKind::WriteZero.into())
    } else {
        Ok(original_bytes)
    };
    Some((mask, result))
}

#[cfg(test)]
#[test]
fn checksum_matches_reference_for_odd_even_and_unaligned_inputs() {
    let bytes: Vec<_> = (0..crate::MAX_FRAME_LEN + 2)
        .map(|n| (n * 13 + n / 256) as u8)
        .collect();
    for offset in 0..2 {
        for length in 0..=crate::MAX_FRAME_LEN {
            let input = &bytes[offset..offset + length];
            let expected: u32 = input
                .chunks(2)
                .map(|p| u16::from_be_bytes([p[0], *p.get(1).unwrap_or(&0)]) as u32)
                .sum();
            assert_eq!(fold(sum(input)), fold(expected));
        }
    }
}

#[cfg(test)]
#[test]
fn tagged_headers_preserve_tags_and_reject_truncation_and_excess_depth() {
    use crate::tap::test_common::udp::frame;
    for v6 in [false, true] {
        for depth in 0..=3 {
            let mut first = frame(v6, 65535, &[1; 120]);
            let mut next = frame(v6, 0, &[2; 120]);
            for bytes in [&mut first, &mut next] {
                for _ in 0..depth {
                    bytes.splice(12..12, [0x81, 0, 0xb0, 7]);
                }
            }
            for end in 0..first.len() {
                assert_eq!(transport(&first[..end]), None);
            }
            if depth == 3 {
                assert_eq!(transport(&first), None);
                continue;
            }
            let ip = 14 + 4 * depth;
            let at = ip + if v6 { 40 } else { 20 };
            assert_eq!(transport(&first), Some((ip, at)));
            assert_eq!(transport(&next), Some((ip, at)));
            assert!(matches(&first, &next, ip, at, 1));
            for offset in 12..ip {
                next[offset] ^= 1;
                assert!(!matches(&first, &next, ip, at, 1), "tag byte {offset}");
                next[offset] ^= 1;
            }
            next[at + 6] ^= 1;
            assert_eq!(transport(&next), None, "bad UDP checksum");
        }
    }
}
