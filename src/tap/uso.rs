//! Lossless coalescing of validated UDP/TCP frames with up to two VLAN tags.
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
/// Callers check lengths first; the zero fallback only keeps this panic-free.
pub(super) fn word(bytes: &[u8], at: usize) -> u16 {
    bytes
        .get(at..)
        .and_then(<[u8]>::first_chunk)
        .map_or(0, |&pair| u16::from_be_bytes(pair))
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Layout {
    ip: usize,
    at: usize,
    header: usize,
    destination: usize,
    tcp: bool,
    v6: bool,
}

impl Layout {
    fn pseudo(self, frame: &[u8], length: usize) -> u32 {
        // transport() checked these ranges; a short frame only sums as zero.
        let field = |at: usize, length: usize| sum(frame.get(at..at + length).unwrap_or_default());
        let addresses = if self.v6 {
            field(self.ip + 8, 16) + field(self.destination, 16)
        } else {
            field(self.ip + 12, 8)
        };
        addresses + if self.tcp { 6 } else { 17 } + length as u32
    }
}

fn transport(frame: &[u8]) -> Option<Layout> {
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
    let v6 = protocol == 0x86dd;
    let mut destination = ip + 24;
    let (at, protocol) = match protocol {
        0x0800 if frame.len() >= ip + 20 && frame[ip] >> 4 == 4 => {
            let at = ip + usize::from(frame[ip] & 15) * 4;
            if at < ip + 20
                || at > frame.len()
                || word(frame, ip + 2) as usize != frame.len() - ip
                || word(frame, ip + 6) != 0x4000
                || word(frame, ip + 10) == 65535
                || fold(sum(&frame[ip..at])) != 65535
            {
                return None;
            }
            (at, frame[ip + 9])
        }
        0x86dd if frame.len() >= ip + 40 && frame[ip] >> 4 == 6 => {
            if word(frame, ip + 4) as usize != frame.len() - ip - 40 {
                return None;
            }
            let mut at = ip + 40;
            let mut next = frame[ip + 6];
            while matches!(next, 0 | 43 | 60) {
                let prefix = frame.get(at..at + 8)?;
                let length = (usize::from(prefix[1]) + 1) * 8;
                if at + length > frame.len() || at + length > 128 {
                    return None;
                }
                if next == 43 {
                    // Active SRH checksums use the final destination in Segment List[0].
                    if prefix[2] != 4
                        || length < 24
                        || usize::from(prefix[4]) * 16 + 24 > length
                        || usize::from(prefix[3]) > usize::from(prefix[4]) + 1
                    {
                        return None;
                    }
                    if prefix[3] != 0 {
                        destination = at + 8;
                    }
                }
                next = prefix[0];
                at += length;
            }
            (at, next)
        }
        _ => return None,
    };
    let tcp = protocol == 6;
    let header = if tcp {
        let prefix = frame.get(at..at + 20)?;
        let length = usize::from(prefix[12] >> 4) * 4;
        // Segmentation emits positive zero; preserve a mangled zero through a single write.
        if word(frame, at + 16) == 65535
            || length < 20
            || prefix[12] & 15 != 0
            || prefix[13] & !0x48 != 0x10
        {
            return None;
        }
        at + length
    } else {
        if protocol != 17
            || at != ip + if v6 { 40 } else { 20 }
            || frame.len() < at + 9
            || word(frame, at + 4) as usize != frame.len() - at
            || word(frame, at + 6) == 0
        {
            return None;
        }
        at + 8
    };
    if header >= frame.len() || header > 128 {
        return None;
    }
    let layout = Layout {
        ip,
        at,
        header,
        destination,
        tcp,
        v6,
    };
    (fold(layout.pseudo(frame, frame.len() - at) + sum(&frame[at..])) == 65535).then_some(layout)
}
/// Callers check lengths first; the zero fallback only keeps this panic-free.
fn sequence(frame: &[u8], at: usize) -> u32 {
    frame
        .get(at + 4..)
        .and_then(<[u8]>::first_chunk)
        .map_or(0, |&bytes| u32::from_be_bytes(bytes))
}
fn matches(first: &[u8], next: &[u8], layout: Layout, count: usize, payload: usize) -> bool {
    let Layout {
        ip,
        at,
        header,
        tcp,
        v6,
        ..
    } = layout;
    // Both frames passed transport(); a range past either end never matches.
    let same = |range: std::ops::Range<usize>| matches!((first.get(range.clone()), next.get(range)), (Some(a), Some(b)) if a == b);
    let flags = |frame: &[u8]| frame.get(at + 13).map(|&flags| flags & !8);
    same(0..ip)
        && same(at..at + 4)
        && (!tcp
            || (sequence(next, at) == sequence(first, at).wrapping_add(payload as u32)
                && same(at + 8..at + 13)
                && flags(first) == flags(next)
                && same(at + 14..at + 16)
                && same(at + 18..header)))
        && if !v6 {
            same(ip..ip + 2)
                && same(ip + 6..ip + 10)
                && same(ip + 12..at)
                && word(next, ip + 4) == word(first, ip + 4).wrapping_add(count as u16)
        } else {
            same(ip..ip + 4) && same(ip + 6..at)
        }
}

/// Returns the attempted mask, TCP flag and original byte count on success.
/// The mask survives failure so the caller retains every selected frame.
pub(super) fn write<'a>(
    fd: RawFd,
    first: &'a [u8],
    mut frames: impl Iterator<Item = &'a [u8]>,
    frame_limit: usize,
    enabled: (bool, bool),
) -> Option<(u64, bool, io::Result<usize>)> {
    let second = frames.next()?;
    let layout = transport(first)?;
    let Layout {
        ip,
        at,
        header: header_len,
        tcp,
        v6,
        ..
    } = layout;
    // transport() bounds every offset below by the header; `?` only keeps this panic-free.
    let first_flags = if tcp { *first.get(at + 13)? } else { 0 };
    if !(if tcp { enabled.1 } else { enabled.0 }) || first_flags & 8 != 0 {
        return None;
    }
    let data = first.get(header_len..)?;
    let size = data.len();
    let hash = flow_hash(first);
    let mut vnet = [0u8; 10];
    let mut header = [0u8; 128];
    let mut iov = [libc::iovec {
        iov_base: std::ptr::null_mut(),
        iov_len: 0,
    }; 66];
    let mut count = 1;
    let mut mask = 1u64;
    let mut payload = size;
    let mut original_bytes = first.len();
    let mut last_flags = first_flags;
    iov[2] = libc::iovec {
        iov_base: data.as_ptr().cast_mut().cast(),
        iov_len: size,
    };
    for (index, next) in std::iter::once(second).chain(frames).take(63).enumerate() {
        if flow_hash(next) != hash {
            continue;
        }
        // A same-flow barrier must prevent moving a later frame ahead of it.
        if next.len() > frame_limit
            || transport(next) != Some(layout)
            || !matches(first, next, layout, count, payload)
        {
            break;
        }
        let Some(data) = next.get(header_len..) else {
            break;
        };
        let length = data.len();
        if length > size || header_len + payload + length > 65535 {
            break;
        }
        iov[count + 2] = libc::iovec {
            iov_base: data.as_ptr().cast_mut().cast(),
            iov_len: length,
        };
        count += 1;
        mask |= 1 << (index + 1);
        payload += length;
        original_bytes += next.len();
        if tcp {
            last_flags = *next.get(at + 13)?;
        }
        if length < size || (tcp && last_flags & 8 != 0) {
            break;
        }
    }
    if count == 1 {
        return None;
    }
    header
        .get_mut(..header_len)?
        .copy_from_slice(first.get(..header_len)?);
    let transport_len = payload + header_len - at;
    let checksum_at = at + if tcp { 16 } else { 6 };
    if tcp {
        *header.get_mut(at + 13)? = last_flags;
    } else {
        header
            .get_mut(at + 4..at + 6)?
            .copy_from_slice(&(transport_len as u16).to_be_bytes());
    }
    if !v6 {
        let total_len = (header_len + payload - ip) as u16;
        header
            .get_mut(ip + 2..ip + 4)?
            .copy_from_slice(&total_len.to_be_bytes());
        header.get_mut(ip + 10..ip + 12)?.fill(0);
        let checksum = !fold(sum(header.get(ip..at)?));
        header
            .get_mut(ip + 10..ip + 12)?
            .copy_from_slice(&checksum.to_be_bytes());
    } else {
        let payload_len = (header_len + payload - ip - 40) as u16;
        header
            .get_mut(ip + 4..ip + 6)?
            .copy_from_slice(&payload_len.to_be_bytes());
    }
    let checksum = fold(layout.pseudo(&header, transport_len));
    header
        .get_mut(checksum_at..checksum_at + 2)?
        .copy_from_slice(&checksum.to_be_bytes());
    vnet[0] = 1; // VIRTIO_NET_HDR_F_NEEDS_CSUM
    vnet[1] = if tcp { if v6 { 4 } else { 1 } } else { 5 };
    for (offset, value) in [
        (2, header_len as u16),
        (4, size as u16),
        (6, at as u16),
        (8, (checksum_at - at) as u16),
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
    let result =
        super::complete_write(n, vnet.len() + header_len + payload).map(|()| original_bytes);
    Some((mask, tcp, result))
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
            assert_eq!(transport(&first).map(|l| (l.ip, l.at)), Some((ip, at)));
            assert_eq!(transport(&next).map(|l| (l.ip, l.at)), Some((ip, at)));
            assert!(matches(&first, &next, transport(&first).unwrap(), 1, 120));
            for offset in 12..ip {
                next[offset] ^= 1;
                assert!(
                    !matches(&first, &next, transport(&first).unwrap(), 1, 120),
                    "tag byte {offset}"
                );
                next[offset] ^= 1;
            }
            next[at + 6] ^= 1;
            assert_eq!(transport(&next), None, "bad UDP checksum");
        }
    }
}

#[cfg(test)]
#[test]
fn tcp_eligibility_checks_lengths_checksums_flags_and_sequence() {
    use crate::tap::test_common::tcp;
    for v6 in [false, true] {
        for options in [&[][..], &[1; 40][..]] {
            let first = tcp::frame(v6, 65535, u32::MAX - 99, 0x10, options, &[7; 100]);
            let next = tcp::frame(v6, 0, 0, 0x18, options, &[8; 99]);
            let layout = transport(&first).unwrap();
            assert_eq!(transport(&next), Some(layout));
            assert!(matches(&first, &next, layout, 1, 100));
            for end in 0..first.len() {
                assert!(transport(&first[..end]).is_none());
            }
            for flags in [0, 1, 2, 4, 0x30, 0x90] {
                assert!(transport(&tcp::frame(v6, 0, 0, flags, options, &[1])).is_none());
            }
            assert!(transport(&tcp::frame(v6, 0, 0, 0x10, options, &[])).is_none());
            let gap = tcp::frame(v6, 0, 1, 0x10, options, &[8; 99]);
            assert!(!matches(&first, &gap, layout, 1, 100));
            let at = layout.at;
            for offset in [at, at + 4, at + 8, at + 14, at + 18, first.len() - 1] {
                let mut bad = first.clone();
                bad[offset] ^= 1;
                assert!(transport(&bad).is_none(), "checksum mutation at {offset}");
            }
        }
    }
}

#[cfg(test)]
#[test]
fn random_and_mutated_frames_never_panic_validation_or_coalescing() {
    use crate::tap::test_common::{
        fuzz::{INPUTS, Rng, fix_lengths},
        tcp, udp,
    };
    let mut flows = Vec::new();
    for v6 in [false, true] {
        let mut first = udp::frame(v6, 65535, &[1; 120]);
        let mut next = udp::frame(v6, 0, &[2; 120]);
        flows.push([first.clone(), next.clone()]);
        for bytes in [&mut first, &mut next] {
            bytes.splice(12..12, [0x88, 0xa8, 0, 7, 0x81, 0, 0, 9]);
        }
        flows.push([first, next]);
        for options in [&[][..], &[1; 40][..]] {
            flows.push([
                tcp::frame(v6, 65535, u32::MAX - 99, 0x10, options, &[7; 100]),
                tcp::frame(v6, 0, 0, 0x18, options, &[8; 99]),
            ]);
        }
        let mut pair = [
            tcp::frame(v6, 1, 0, 0x10, &[], &[7; 100]),
            tcp::frame(v6, 2, 100, 0x10, &[], &[8; 100]),
        ];
        for bytes in &mut pair {
            tcp::extend(bytes, v6);
        }
        flows.push(pair);
    }
    // Restores checksums after a mutation so validation can reach coalescing.
    let repair = |bytes: &mut Vec<u8>| {
        let v6 = match bytes.get(12..14) {
            Some([0x86, 0xdd]) => true,
            Some([0x08, 0x00]) => false,
            _ => return,
        };
        if bytes.len() >= if v6 { 74 } else { 54 } {
            fix_lengths(bytes);
            if bytes[if v6 { 20 } else { 23 }] == 6 {
                tcp::repair(bytes, v6);
            } else {
                udp::repair(bytes, v6);
            }
        }
    };
    let mut rng = Rng::new(0x0505_0001);
    let (mut first, mut next) = (Vec::new(), Vec::new());
    for _ in 0..INPUTS {
        let [seed, follower] = &flows[rng.below(flows.len())];
        rng.input(std::slice::from_ref(seed), &mut first);
        if rng.below(2) == 0 {
            repair(&mut first);
        }
        // An unchanged follower keeps coalescing reachable; a mutated one tests barriers.
        next.clone_from(follower);
        if rng.below(2) == 0 {
            rng.input(std::slice::from_ref(follower), &mut next);
            if rng.below(2) == 0 {
                repair(&mut next);
            }
        }
        let limit = [1514, crate::MAX_FRAME_LEN, rng.below(10_000)][rng.below(3)];
        let enabled = (rng.below(4) != 0, rng.below(4) != 0);
        let frames = [&next[..], follower].into_iter().take(1 + rng.below(2));
        write(-1, &first, frames, limit, enabled);
    }
}

#[cfg(test)]
#[test]
fn tcp_coalescing_stops_at_flow_barriers_and_counts_only_selected_frames() {
    use crate::tap::test_common::tcp;
    let a = tcp::frame(false, 100, 0, 0x10, &[1; 12], &[1; 100]);
    let b = tcp::frame(false, 101, 100, 0x18, &[1; 12], &[2; 100]);
    let mut other = a.clone();
    other[34] ^= 1;
    tcp::repair(&mut other, false);
    assert_ne!(flow_hash(&a), flow_hash(&other));
    let (mask, tcp, result) =
        write(-1, &a, [&other[..], &b[..]].into_iter(), 1514, (true, true)).unwrap();
    assert_eq!(mask, 0b101);
    assert!(tcp);
    assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::EBADF));
    for offset in [38, 42, 48, 52, 54] {
        let mut barrier = b.clone();
        barrier[offset] ^= 1;
        crate::tap::test_common::tcp::repair(&mut barrier, false);
        assert!(transport(&barrier).is_some());
        assert!(
            write(
                -1,
                &a,
                [&barrier[..], &b[..]].into_iter(),
                1514,
                (true, true)
            )
            .is_none(),
            "changed TCP field {offset} must not be crossed"
        );
    }
    let c = crate::tap::test_common::tcp::frame(false, 102, 200, 0x10, &[1; 12], &[3; 100]);
    assert!(
        write(-1, &b, [&c[..]].into_iter(), 1514, (true, true)).is_none(),
        "PSH starts a new group"
    );
    let (mask, _, _) = write(-1, &a, [&b[..], &c[..]].into_iter(), 1514, (true, true)).unwrap();
    assert_eq!(mask, 0b11, "PSH closes the current group");
    assert!(write(-1, &a, [&b[..]].into_iter(), 1514, (true, false)).is_none());
}
