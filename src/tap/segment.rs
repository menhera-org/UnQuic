//! Checked UDP/TCP super-frame expansion; no wire-format change.
use super::uso::{fold, sum, word};

#[derive(Clone, Copy, Debug)]
pub(crate) struct Segments {
    network: usize,
    transport: usize,
    header: usize,
    tcp: bool,
    size: usize,
    next: usize,
    id: u16,
    v6: bool,
}

impl Segments {
    pub(super) fn parse(header: &[u8; 10], bytes: &[u8], limit: usize) -> Option<Self> {
        let kind = header[1];
        let tcp = matches!(kind, 1 | 4 | 0x81 | 0x84);
        if header[0] != 1 || (!tcp && kind != 5) || bytes.len() > 65535 {
            return None;
        }
        let size = u16::from_ne_bytes([header[4], header[5]]) as usize;
        let transport = u16::from_ne_bytes([header[6], header[7]]) as usize;
        let hint = u16::from_ne_bytes([header[2], header[3]]) as usize;
        if size == 0
            || hint > bytes.len()
            || header[8..10] != (if tcp { 16u16 } else { 6 }).to_ne_bytes()
            || transport + if tcp { 20 } else { 8 } >= bytes.len()
            || bytes.len() < 14
        {
            return None;
        }
        let header_len = transport
            + if tcp {
                let length = usize::from(bytes[transport + 12] >> 4) * 4;
                if length < 20 {
                    return None;
                }
                length
            } else {
                8
            };
        if header_len >= bytes.len() || header_len + size > limit {
            return None;
        }
        let mut network = 14;
        let mut protocol = word(bytes, 12);
        while matches!(protocol, 0x8100 | 0x88a8) {
            if network + 4 > transport {
                return None;
            }
            protocol = word(bytes, network + 2);
            network += 4;
        }
        let v6 = match protocol {
            0x0800 => false,
            0x86dd => true,
            _ => return None,
        };
        if tcp && (kind & 0x7f == 4) != v6 {
            return None;
        }
        let transport_protocol = if tcp { 6 } else { 17 };
        if v6 {
            if network + 40 > transport
                || bytes[network] >> 4 != 6
                || word(bytes, network + 4) as usize != bytes.len() - network - 40
            {
                return None;
            }
            let mut next = bytes[network + 6];
            let mut at = network + 40;
            while matches!(next, 0 | 43 | 60) {
                if at + 2 > transport {
                    return None;
                }
                let length = (usize::from(bytes[at + 1]) + 1) * 8;
                next = bytes[at];
                at += length;
            }
            if next != transport_protocol || at != transport {
                return None;
            }
        } else if network + 20 > transport
            || bytes[network] >> 4 != 4
            || bytes[network] & 15 < 5
            || network + usize::from(bytes[network] & 15) * 4 != transport
            || bytes[network + 9] != transport_protocol
            || word(bytes, network + 2) as usize != bytes.len() - network
            || word(bytes, network + 6) & !0x4000 != 0
        {
            return None;
        }
        if !tcp && word(bytes, transport + 4) as usize != bytes.len() - transport {
            return None;
        }
        Some(Self {
            network,
            transport,
            header: header_len,
            tcp,
            size,
            next: header_len,
            id: if v6 { 0 } else { word(bytes, network + 4) },
            v6,
        })
    }

    /// Writes one complete frame. The immutable input stays alive until exhausted.
    pub(crate) fn write(&mut self, input: &[u8], output: &mut [u8]) -> usize {
        if output.len() < self.header_len() + self.size.min(input.len().saturating_sub(self.next)) {
            return 0;
        }
        let Some(range) = self.write_header(input, output) else {
            return 0;
        };
        let header = self.header_len();
        let end = header + range.len();
        let (Some(payload), Some(source)) = (output.get_mut(header..end), input.get(range)) else {
            return 0;
        };
        payload.copy_from_slice(source);
        end
    }

    pub(crate) const fn header_len(&self) -> usize {
        self.header
    }

    pub(crate) fn next_len(&self, length: usize) -> usize {
        self.header_len() + self.size.min(length.saturating_sub(self.next))
    }

    /// Rewrites only the header and returns a range in the unchanged source.
    /// Insufficient header storage leaves both cursor and output unchanged.
    pub(crate) fn write_header(
        &mut self,
        input: &[u8],
        output: &mut [u8],
    ) -> Option<std::ops::Range<usize>> {
        let header = self.header_len();
        if output.len() < header || input.len() <= self.next {
            return None;
        }
        let count = self.size.min(input.len() - self.next);
        let length = header + count;
        let range = self.next..self.next + count;
        // Parse put every offset below inside the header; checked access only
        // keeps a mismatched source panic-free, and the cursor moves on success.
        let output = output.get_mut(..header)?;
        output.copy_from_slice(input.get(..header)?);
        let at = self.transport;
        let transport_length = (header - at + count) as u16;
        let checksum_at = at + if self.tcp { 16 } else { 6 };
        // Adjust the supplied pseudo-header seed, preserving routing-header semantics.
        let seed = fold(
            u32::from(word(input, checksum_at))
                + u32::from(!((input.len() - at) as u16))
                + u32::from(transport_length),
        );
        if self.tcp {
            let seq = u32::from_be_bytes(*input.get(at + 4..)?.first_chunk()?);
            let seq = seq.wrapping_add((range.start - header) as u32);
            output
                .get_mut(at + 4..at + 8)?
                .copy_from_slice(&seq.to_be_bytes());
            let flags = output.get_mut(at + 13)?;
            if range.end != input.len() {
                *flags &= !0x09; // FIN and PSH belong to the final segment.
            }
            if range.start != header {
                *flags &= !0x80; // CWR belongs to the first segment.
            }
        } else {
            output
                .get_mut(at + 4..at + 6)?
                .copy_from_slice(&transport_length.to_be_bytes());
        }
        let checksum_field = checksum_at..checksum_at + 2;
        output
            .get_mut(checksum_field.clone())?
            .copy_from_slice(&seed.to_be_bytes());
        // Both transport headers preserve payload word alignment.
        let checksum = !fold(sum(output.get(at..)?) + sum(input.get(range.clone())?));
        let checksum = if !self.tcp && checksum == 0 {
            65535
        } else {
            checksum
        };
        output
            .get_mut(checksum_field)?
            .copy_from_slice(&checksum.to_be_bytes());
        let ip = self.network;
        if self.v6 {
            let payload_length = (length - ip - 40) as u16;
            output
                .get_mut(ip + 4..ip + 6)?
                .copy_from_slice(&payload_length.to_be_bytes());
        } else {
            let total_length = (length - ip) as u16;
            output
                .get_mut(ip + 2..ip + 4)?
                .copy_from_slice(&total_length.to_be_bytes());
            output
                .get_mut(ip + 4..ip + 6)?
                .copy_from_slice(&self.id.to_be_bytes());
            output.get_mut(ip + 10..ip + 12)?.fill(0);
            let checksum = !fold(sum(output.get(ip..at)?));
            output
                .get_mut(ip + 10..ip + 12)?
                .copy_from_slice(&checksum.to_be_bytes());
            self.id = self.id.wrapping_add(1);
        }
        self.next = range.end;
        Some(range)
    }

    pub(crate) fn remaining(&self, length: usize) -> usize {
        length.saturating_sub(self.next).div_ceil(self.size)
    }

    /// Logical Ethernet bytes still represented by this source, including repeated headers.
    pub(crate) fn remaining_bytes(&self, length: usize) -> Option<usize> {
        let payload = length.checked_sub(self.next)?;
        payload.checked_add(self.header_len().checked_mul(payload.div_ceil(self.size))?)
    }

    pub(crate) fn done(&self, length: usize) -> bool {
        self.next == length
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::tap::test_common::{fuzz, udp};

    fn packet(v6: bool, count: usize, size: usize, short: bool) -> (Vec<u8>, [u8; 10]) {
        let length = size * count - usize::from(short);
        let bytes: Vec<_> = (0..length).map(|i| (i * 13) as u8).collect();
        let mut bytes = udp::frame(v6, 65534, &bytes);
        let mut header = udp::partial(&mut bytes, v6);
        header[1] = 5;
        header[4..6].copy_from_slice(&(size as u16).to_ne_bytes());
        (bytes, header)
    }

    /// Drives a parsed source through bounded writes into undersized and full outputs.
    pub(in crate::tap) fn drain(segments: &mut Segments, input: &[u8], rng: &mut fuzz::Rng) {
        let mut output = [0; crate::MAX_FRAME_LEN];
        for _ in 0..16 {
            if segments.done(input.len()) {
                break;
            }
            segments.next_len(input.len());
            segments.remaining(input.len());
            segments.remaining_bytes(input.len());
            let size = rng.below(output.len() + 1);
            let mut borrowed = *segments;
            borrowed.write_header(input, &mut output[..size]);
            segments.write(input, &mut output[..size]);
        }
    }

    #[test]
    fn random_metadata_and_frames_never_panic_parsing_or_segmentation() {
        use crate::tap::test_common::{fuzz::fix_lengths, tcp};
        let mut seeds = Vec::new();
        for v6 in [false, true] {
            let (input, header) = packet(v6, 3, 100, true);
            seeds.push((header, input));
            let mut input = tcp::frame(v6, 65534, u32::MAX - 10, 0x19, &[1; 12], &[7; 300]);
            let header = tcp::partial(&mut input, v6, 100);
            seeds.push((header, input));
            let mut input = tcp::frame(v6, 1, 0, 0x10, &[], &[7; 300]);
            let mut header = tcp::partial(&mut input, v6, 1);
            let extension = tcp::extend(&mut input, v6) as u16;
            for at in [2, 6] {
                let offset = u16::from_ne_bytes([header[at], header[at + 1]]) + extension;
                header[at..at + 2].copy_from_slice(&offset.to_ne_bytes());
            }
            seeds.push((header, input));
        }
        let mut rng = fuzz::Rng::new(0x5e60_0001);
        let mut input = Vec::new();
        for _ in 0..fuzz::INPUTS {
            let (mut header, ref seed) = seeds[rng.below(seeds.len())];
            if rng.below(2) == 0 {
                header[rng.below(10)] = rng.bits() as u8;
            }
            rng.input(std::slice::from_ref(seed), &mut input);
            if rng.below(2) == 0 {
                fix_lengths(&mut input);
            }
            let limit = [1514, crate::MAX_FRAME_LEN, rng.below(70_000)][rng.below(3)];
            if let Some(mut segments) = Segments::parse(&header, &input, limit) {
                drain(&mut segments, &input, &mut rng);
            }
        }
    }

    #[test]
    fn tcp_segments_match_independent_frames_with_flags_options_and_sequence_wrap() {
        use crate::tap::test_common::tcp;
        for v6 in [false, true] {
            for tags in [
                &[][..],
                &[0x81, 0, 0, 7][..],
                &[0x88, 0xa8, 0, 7, 0x81, 0, 0, 9][..],
            ] {
                for options in [&[][..], &[1; 12][..], &[1; 40][..]] {
                    for (size, count) in [(1, 3), (101, 3), (1448, 44)] {
                        for flags in [0x10, 0x19, 0xd9] {
                            let payload: Vec<_> = (0..size * count - usize::from(size > 1))
                                .map(|i| (i * 13) as u8)
                                .collect();
                            let mut input =
                                tcp::frame(v6, 65534, u32::MAX - 10, flags, options, &payload);
                            let mut header = tcp::partial(&mut input, v6, size as u16);
                            input.splice(12..12, tags.iter().copied());
                            for at in [2, 6] {
                                let offset = u16::from_ne_bytes([header[at], header[at + 1]])
                                    + tags.len() as u16;
                                header[at..at + 2].copy_from_slice(&offset.to_ne_bytes());
                            }
                            let mut segments = Segments::parse(&header, &input, 1600).unwrap();
                            let mut output = [0; 1600];
                            for (i, data) in payload.chunks(size).enumerate() {
                                let flags = flags
                                    & if i + 1 == count { 0xff } else { !0x09 }
                                    & if i == 0 { 0xff } else { !0x80 };
                                let mut expected = tcp::frame(
                                    v6,
                                    65534u16.wrapping_add(i as u16),
                                    (u32::MAX - 10).wrapping_add((i * size) as u32),
                                    flags,
                                    options,
                                    data,
                                );
                                expected.splice(12..12, tags.iter().copied());
                                let mut borrowed = segments;
                                let mut prefix = [0xa5; 128];
                                assert!(
                                    borrowed
                                        .write_header(
                                            &input,
                                            &mut prefix[..segments.header_len() - 1]
                                        )
                                        .is_none()
                                );
                                assert_eq!(prefix, [0xa5; 128]);
                                let range = borrowed.write_header(&input, &mut prefix).unwrap();
                                assert_eq!(
                                    &prefix[..segments.header_len()],
                                    &expected[..segments.header_len()]
                                );
                                assert_eq!(&input[range], &expected[segments.header_len()..]);
                                let remaining = segments.remaining_bytes(input.len()).unwrap();
                                let n = segments.write(&input, &mut output);
                                assert_eq!(&output[..n], expected);
                                assert_eq!(
                                    segments.remaining_bytes(input.len()),
                                    Some(remaining - n)
                                );
                                assert_eq!(
                                    crate::pack::flow_hash(&input),
                                    crate::pack::flow_hash(&output[..n])
                                );
                            }
                            assert!(segments.done(input.len()));
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn tcp_extension_headers_and_routing_checksum_seed_survive_segmentation() {
        use crate::tap::test_common::tcp;
        for v6 in [false, true] {
            let mut input = tcp::frame(v6, 65535, 0, 0x19, &[1; 40], &[7; 201]);
            let mut header = tcp::partial(&mut input, v6, 100);
            let transport = if v6 { 54 } else { 34 };
            let final_address = [9; 16];
            let pseudo_addresses = if v6 {
                let seed = fold(sum(&input[22..38]) + sum(&final_address) + 6 + 261);
                input[70..72].copy_from_slice(&seed.to_be_bytes());
                let mut extension = [0; 104];
                extension[0] = 43;
                extension[1] = 9;
                extension[80] = 6;
                extension[81] = 2;
                extension[82] = 4;
                extension[88..].copy_from_slice(&final_address);
                input.splice(54..54, extension);
                input[20] = 60;
                let length = (input.len() - 54) as u16;
                input[18..20].copy_from_slice(&length.to_be_bytes());
                sum(&input[22..38]) + sum(&final_address)
            } else {
                input.splice(34..34, [1; 40]);
                input[14] = 0x4f;
                let length = (input.len() - 14) as u16;
                input[16..18].copy_from_slice(&length.to_be_bytes());
                sum(&input[26..34])
            };
            let at = transport + if v6 { 104 } else { 40 };
            header[2..4].copy_from_slice(&((at + 60) as u16).to_ne_bytes());
            header[6..8].copy_from_slice(&(at as u16).to_ne_bytes());
            let mut segments = Segments::parse(&header, &input, 1514).unwrap();
            let mut output = [0; 1514];
            for i in 0..3 {
                let n = segments.write(&input, &mut output);
                assert_eq!(&output[transport..at], &input[transport..at]);
                assert_eq!(&output[at + 20..at + 60], &[1; 40]);
                assert_eq!(&output[at + 4..at + 8], &(i * 100u32).to_be_bytes());
                assert_eq!(output[at + 13], if i == 2 { 0x19 } else { 0x10 });
                assert_eq!(
                    fold(pseudo_addresses + 6 + (n - at) as u32 + sum(&output[at..n])),
                    65535
                );
                if !v6 {
                    assert_eq!(fold(sum(&output[14..at])), 65535);
                }
            }
            assert!(segments.done(input.len()));
            if v6 {
                input[20] = 44;
            } else {
                input[20] |= 0x20;
            }
            assert!(Segments::parse(&header, &input, 1514).is_none());
        }
    }

    #[test]
    fn tcp_metadata_rejects_truncation_protocol_mismatch_and_invalid_headers() {
        use crate::tap::test_common::tcp;
        for v6 in [false, true] {
            let mut input = tcp::frame(v6, 0, 0, 0x10, &[], &[7; 201]);
            let header = tcp::partial(&mut input, v6, 100);
            let at = if v6 { 54 } else { 34 };
            for length in 0..input.len() {
                assert!(Segments::parse(&header, &input[..length], 1514).is_none());
            }
            for (index, value) in [
                (0, 0),
                (0, 3),
                (1, if v6 { 1 } else { 4 }),
                (1, 0x41),
                (4, 0),
                (6, 0),
                (8, 6),
                (9, 1),
            ] {
                let mut bad = header;
                bad[index] = value;
                assert!(Segments::parse(&bad, &input, 1514).is_none());
            }
            for (index, value) in [(at + 12, 0x40), (if v6 { 20 } else { 23 }, 17)] {
                let mut bad = input.clone();
                bad[index] = value;
                assert!(Segments::parse(&header, &bad, 1514).is_none());
            }
            assert!(Segments::parse(&header, &input, at + 119).is_none());
        }
    }

    #[test]
    fn udp_segments_match_independent_frames_and_keep_flow_pinning() {
        for v6 in [false, true] {
            for count in [1, 2, 43, 44] {
                for size in [2, 22, 1472] {
                    for short in [false, true] {
                        let (input, header) = packet(v6, count, size, short);
                        let payload = if v6 { 62 } else { 42 };
                        let mut segments = Segments::parse(&header, &input, 1514 + 20).unwrap();
                        let mut output = [0u8; crate::MAX_FRAME_LEN];
                        let mut remaining = input[payload..]
                            .chunks(size)
                            .map(|data| payload + data.len())
                            .sum::<usize>();
                        assert_eq!(segments.remaining_bytes(payload - 1), None);
                        for (i, data) in input[payload..].chunks(size).enumerate() {
                            assert_eq!(segments.remaining_bytes(input.len()), Some(remaining));
                            let expected = udp::frame(v6, 65534u16.wrapping_add(i as u16), data);
                            let mut borrowed = segments;
                            let mut prefix = [0xa5; 128];
                            assert!(
                                borrowed
                                    .write_header(&input, &mut prefix[..payload - 1])
                                    .is_none()
                            );
                            assert_eq!(prefix, [0xa5; 128]);
                            let range = borrowed.write_header(&input, &mut prefix).unwrap();
                            assert_eq!(&prefix[..payload], &expected[..payload]);
                            assert_eq!(&input[range], &expected[payload..]);
                            let n = segments.write(&input, &mut output);
                            assert_eq!(&output[..n], expected);
                            remaining -= n;
                            assert_eq!(segments.remaining_bytes(input.len()), Some(remaining));
                            assert_eq!(
                                crate::pack::flow_hash(&input),
                                crate::pack::flow_hash(&output[..n])
                            );
                        }
                        assert!(segments.done(input.len()));
                        assert_eq!(remaining, 0);
                    }
                }
            }
        }
    }

    #[test]
    fn routing_seed_and_long_headers_survive_borrowed_segmentation() {
        for extension in [0, 80] {
            let (mut input, mut header) = packet(true, 3, 101, true);
            let final_address = [9; 16];
            let seed =
                fold(sum(&input[22..38]) + sum(&final_address) + 17 + (input.len() - 54) as u32);
            input[60..62].copy_from_slice(&seed.to_be_bytes());
            let mut routing = [0; 24];
            routing[0] = 17;
            routing[1] = 2;
            routing[2] = 4;
            routing[8..].copy_from_slice(&final_address);
            input.splice(54..54, routing);
            input[20] = 43;
            if extension != 0 {
                let mut options = [0; 80];
                options[0] = 43;
                options[1] = 9;
                input.splice(54..54, options);
                input[20] = 60;
            }
            let at = 54 + 24 + extension;
            let n = (input.len() - 54) as u16;
            input[18..20].copy_from_slice(&n.to_be_bytes());
            header[6..8].copy_from_slice(&(at as u16).to_ne_bytes());
            let mut segments = Segments::parse(&header, &input, 1514).unwrap();
            for i in 0..3 {
                let mut borrowed = segments;
                let mut prefix = [0xa5; 128];
                let range = borrowed.write_header(&input, &mut prefix);
                let mut output = [0; 1514];
                let n = segments.write(&input, &mut output);
                assert_eq!(n, at + 8 + if i == 2 { 100 } else { 101 });
                assert_eq!(&output[54..at], &input[54..at]);
                let pseudo = sum(&output[22..38]) + sum(&final_address) + 17 + (n - at) as u32;
                assert_eq!(fold(pseudo + sum(&output[at..n])), 65535);
                if let Some(range) = range {
                    assert_eq!(&prefix[..at + 8], &output[..at + 8]);
                    assert_eq!(&input[range], &output[at + 8..n]);
                } else {
                    assert!(at + 8 > prefix.len());
                    assert_eq!(prefix, [0xa5; 128]);
                    let mut fallback = [0; 1514];
                    assert_eq!(borrowed.write(&input, &mut fallback), n);
                    assert_eq!(&fallback[..n], &output[..n]);
                }
            }
        }
    }

    #[test]
    fn checked_offsets_options_tags_and_extension_headers() {
        for v6 in [false, true] {
            let (mut input, mut header) = packet(v6, 3, 100, true);
            let transport = if v6 { 54 } else { 34 };
            // An options/extension header changes the transport offset, not the seed.
            if v6 {
                input[20] = 60;
                input.splice(54..54, [17, 0, 0, 0, 0, 0, 0, 0]);
                let n = (input.len() - 54) as u16;
                input[18..20].copy_from_slice(&n.to_be_bytes());
            } else {
                input[14] = 0x47;
                input.splice(34..34, [1; 8]);
                let n = (input.len() - 14) as u16;
                input[16..18].copy_from_slice(&n.to_be_bytes());
            }
            for _ in 0..2 {
                input.splice(12..12, [0x81, 0, 0, 7]);
            }
            let at = transport + 16;
            header[6..8].copy_from_slice(&(at as u16).to_ne_bytes());
            let mut segments = Segments::parse(&header, &input, 1500).unwrap();
            let mut output = [0u8; 1500];
            for i in 0..3 {
                let n = segments.write(&input, &mut output);
                assert_eq!(&output[..22], &input[..22]);
                assert_eq!(&output[transport + 8..at], &input[transport + 8..at]);
                assert_eq!(n, at + 8 + if i == 2 { 99 } else { 100 });
                let pseudo =
                    sum(if v6 { &input[30..62] } else { &input[34..42] }) + 17 + (n - at) as u32;
                assert_eq!(fold(pseudo + sum(&output[at..n])), 65535);
                if !v6 {
                    assert_eq!(fold(sum(&output[22..at])), 65535);
                }
            }
            assert!(segments.done(input.len()));
        }
        let (input, header) = packet(false, 2, 100, false);
        for length in 0..input.len() {
            assert!(Segments::parse(&header, &input[..length], 1514).is_none());
        }
        for (at, value) in [
            (0, 0),
            (0, 3),
            (1, 1),
            (1, 0x85),
            (4, 0),
            (6, 0),
            (8, 5),
            (9, 1),
        ] {
            let mut bad = header;
            bad[at] = value;
            assert!(Segments::parse(&bad, &input, 1514).is_none());
        }
        for (at, value) in [(12, 0), (14, 0x44), (16, 1), (20, 0x20), (23, 6), (38, 1)] {
            let mut bad = input.clone();
            bad[at] = value;
            assert!(Segments::parse(&header, &bad, 1514).is_none());
        }
        assert!(Segments::parse(&header, &input, 141).is_none());
    }
}

#[cfg(test)]
#[test]
#[ignore = "requires disposable container, tun, iproute2 and CAP_NET_ADMIN"]
fn maximum_udp_sends_fit_the_tap_read_boundary() {
    use crate::tap::test_common::*;
    use std::{net::UdpSocket, os::fd::AsRawFd, process::Command};
    check(unsafe { libc::unshare(libc::CLONE_NEWNET) });
    let tap = super::Tap::create(c"uq-max", 1500, 1, None).unwrap();
    tap.enable_input_offloads().unwrap();
    let mut queue = tap.into_queues().pop().unwrap();
    queue.set_carrier(true).unwrap();
    for args in [
        vec!["link", "set", "uq-max", "addrgenmode", "none"],
        vec!["link", "set", "uq-max", "up"],
        vec!["addr", "add", "198.18.0.1/24", "dev", "uq-max"],
        vec!["-6", "addr", "add", "fd00::1/64", "dev", "uq-max", "nodad"],
        vec![
            "-6",
            "neigh",
            "add",
            "fd00::2",
            "lladdr",
            "02:00:00:00:00:02",
            "nud",
            "permanent",
            "dev",
            "uq-max",
        ],
        vec![
            "neigh",
            "add",
            "198.18.0.2",
            "lladdr",
            "02:00:00:00:00:02",
            "nud",
            "permanent",
            "dev",
            "uq-max",
        ],
    ] {
        assert!(Command::new("ip").args(args).status().unwrap().success());
    }
    for v6 in [false, true] {
        let socket = UdpSocket::bind(if v6 {
            "[fd00::1]:51000"
        } else {
            "198.18.0.1:51000"
        })
        .unwrap();
        socket
            .connect(if v6 {
                "[fd00::2]:52000"
            } else {
                "198.18.0.2:52000"
            })
            .unwrap();
        let payload_at = if v6 { 62 } else { 42 };
        let size = if v6 { 1452i32 } else { 1472 };
        // SAFETY: UDP_SEGMENT reads one initialized integer.
        check(unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                libc::IPPROTO_UDP,
                libc::UDP_SEGMENT,
                (&size as *const i32).cast(),
                size_of_val(&size) as _,
            )
        });
        let mut bytes = vec![0; 70_000];
        for length in if v6 { [65487, 65527] } else { [65487, 65507] } {
            if let Err(error) = socket.send(&vec![0x5a; length]) {
                // Some kernels cap the complete IPv6 packet at 65,535 bytes.
                assert!(v6 && length > 65487);
                assert_eq!(error.raw_os_error(), Some(libc::EMSGSIZE));
                continue;
            }
            let mut total = 0;
            loop {
                let mut header = [0u8; 10];
                let iov = [
                    libc::iovec {
                        iov_base: header.as_mut_ptr().cast(),
                        iov_len: 10,
                    },
                    libc::iovec {
                        iov_base: bytes.as_mut_ptr().cast(),
                        iov_len: bytes.len(),
                    },
                ];
                // SAFETY: iovecs cover writable metadata and payload storage.
                let n = unsafe { libc::readv(queue.fd.as_raw_fd(), iov.as_ptr(), 2) };
                if n < 0 {
                    assert_eq!(
                        std::io::Error::last_os_error().kind(),
                        std::io::ErrorKind::WouldBlock
                    );
                    break;
                }
                if bytes[12..14] != (if v6 { 0x86ddu16 } else { 0x0800 }).to_be_bytes()
                    || bytes[payload_at - 8..payload_at - 6] != 51000u16.to_be_bytes()
                {
                    continue;
                }
                let (n, mut segments) = queue
                    .finish_input(&header, n as usize, &mut bytes)
                    .unwrap_or_else(|e| panic!("read bytes={n} header={header:?}: {e}"));
                if let Some(ref mut segments) = segments {
                    let mut output = [0u8; crate::MAX_FRAME_LEN];
                    while !segments.done(n) {
                        let produced = segments.write(&bytes[..n], &mut output);
                        assert!(output[payload_at..produced].iter().all(|&b| b == 0x5a));
                        total += produced - payload_at;
                    }
                } else {
                    total += n - payload_at;
                }
            }
            assert_eq!(total, length);
        }
    }
}
