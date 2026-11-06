use std::time::Duration;
use unquic::{
    BASE_UDP_PAYLOAD, MAX_FRAME_LEN, UnquicUuid,
    handshake::{Connection, Params, Role, Tag},
    pack::{self, PackError, flow_hash},
    reassembly::{Admission, Lane as ReceiveLane},
    wire::{Datagram, Record},
};

fn sender() -> pack::Lane {
    let params = Params {
        port: 4444,
        max_frame_len: 9022,
        mandatory_features: 0,
        optional_features: 0,
    };
    pack::Lane::new(
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
fn every_supported_frame_length_segments_and_reassembles() {
    let mut sender = sender();
    let mut receiver = ReceiveLane::new(1, 9022).unwrap();
    let mut output = [0; BASE_UDP_PAYLOAD];
    let mut packet_number = 0;
    let frame: Vec<_> = (0..MAX_FRAME_LEN).map(|i| i as u8).collect();
    for length in 14..=MAX_FRAME_LEN {
        let mut pieces = sender.frame(&frame[..length]).unwrap();
        let mut count = 0;
        while let Some(packet) = pieces.encode_next(&mut output).unwrap() {
            packet_number += 1;
            assert_eq!(packet.number, packet_number);
            assert!(packet.length <= BASE_UDP_PAYLOAD);
            let datagram = Datagram::decode(&output[..packet.length]).unwrap();
            assert_eq!(datagram.header().receive_tag, 2);
            assert_eq!(receiver.admit(&datagram, Duration::ZERO), Admission::Fresh);
            count += 1;
        }
        assert_eq!(count, length.div_ceil(1164));
        assert!(count <= 8);
        assert_eq!(
            receiver.front(Duration::ZERO).unwrap().bytes,
            &frame[..length]
        );
        receiver.finish_front();
    }
}

#[test]
fn frame_tail_abandonment_and_output_failure_never_reuse_identifiers() {
    let mut sender = sender();
    assert!(matches!(sender.frame(&[0; 13]), Err(PackError::Frame)));
    assert!(matches!(
        sender.frame(&[0; MAX_FRAME_LEN + 1]),
        Err(PackError::Frame)
    ));
    let mut output = [7; BASE_UDP_PAYLOAD];
    {
        let mut first = sender.frame(&[1; 1500]).unwrap();
        assert_eq!(first.id(), (1 << 48) | 1);
        assert!(first.encode_next(&mut output[..68]).is_err());
        assert_eq!(output, [7; BASE_UDP_PAYLOAD]);
        assert_eq!(first.encode_next(&mut output).unwrap().unwrap().number, 1);
        // The unsent tail is dropped when first leaves scope.
    }
    {
        let mut second = sender.frame(&[2; 60]).unwrap();
        assert_eq!(second.id(), (1 << 48) | 2);
        assert_eq!(second.encode_next(&mut output).unwrap().unwrap().number, 2);
        assert_eq!(second.encode_next(&mut output).unwrap(), None);
    }
    assert_eq!(sender.record(Record::Probe, &mut output).unwrap().number, 3);
}

fn ethernet(protocol: u16, payload: &[u8]) -> Vec<u8> {
    let mut frame = vec![0; 14];
    frame[..12].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
    frame[12..14].copy_from_slice(&protocol.to_be_bytes());
    frame.extend_from_slice(payload);
    frame
}
fn v4() -> Vec<u8> {
    let mut ip = vec![0; 40];
    ip[0] = 0x45;
    ip[2..4].copy_from_slice(&40_u16.to_be_bytes());
    ip[9] = 6;
    ip[12..20].copy_from_slice(&[192, 0, 2, 1, 192, 0, 2, 2]);
    ip[20..24].copy_from_slice(&[1, 2, 3, 4]);
    ip
}
fn v6(next: u8, payload: &[u8]) -> Vec<u8> {
    let mut ip = vec![0; 40];
    ip[0] = 0x60;
    ip[4..6].copy_from_slice(&(payload.len() as u16).to_be_bytes());
    ip[6] = next;
    ip[8..40].copy_from_slice(&[3; 32]);
    ip.extend_from_slice(payload);
    ip
}

#[test]
fn ipv4_flow_ignores_payload_but_distinguishes_addresses_ports_and_vlans() {
    let ip = v4();
    let base = flow_hash(&ethernet(0x0800, &ip));
    for byte in [4, 5, 8, 24, 39] {
        let mut changed = ip.clone();
        changed[byte] ^= 0xff;
        assert_eq!(flow_hash(&ethernet(0x0800, &changed)), base);
    }
    for byte in [12, 19, 20, 23] {
        let mut changed = ip.clone();
        changed[byte] ^= 0xff;
        assert_ne!(flow_hash(&ethernet(0x0800, &changed)), base);
    }
    let mut vlan = vec![0, 10, 0x08, 0];
    vlan.extend_from_slice(&ip);
    let tagged = flow_hash(&ethernet(0x8100, &vlan));
    assert_ne!(tagged, base);
    vlan[1] = 11;
    assert_ne!(flow_hash(&ethernet(0x8100, &vlan)), tagged);
    let mut qinq = vec![0, 20, 0x81, 0];
    qinq.extend_from_slice(&vlan);
    assert_ne!(flow_hash(&ethernet(0x88a8, &qinq)), tagged);
    let mut changed = ethernet(0x0800, &ip);
    changed[0] ^= 0xff;
    assert_ne!(flow_hash(&changed), base);
}

#[test]
fn every_ipv4_fragment_uses_address_protocol_without_ports() {
    let mut first = v4();
    first[6] = 0x20; // More fragments, offset zero and transport ports present.
    let expected = flow_hash(&ethernet(0x0800, &first));
    for field in [1_u16, 0x2001, 0x1fff] {
        let mut later = first.clone();
        later[6..8].copy_from_slice(&field.to_be_bytes());
        later[20..].fill(0xff);
        assert_eq!(flow_hash(&ethernet(0x0800, &later)), expected);
    }
    first[6..8].fill(0); // Unfragmented traffic includes the transport ports.
    assert_ne!(flow_hash(&ethernet(0x0800, &first)), expected);
}

#[test]
fn ipv6_extension_chain_reaches_ports_and_fragments_stop_before_fragmentable_headers() {
    let transport = [1, 2, 3, 4, 0, 8, 0, 0];
    let base = flow_hash(&ethernet(0x86dd, &v6(17, &transport)));
    // Hop-by-hop, routing, destination options, AH, UDP.
    let mut chain = vec![
        43, 0, 0, 0, 0, 0, 0, 0, 60, 0, 0, 0, 0, 0, 0, 0, 51, 0, 0, 0, 0, 0, 0, 0, 17, 1, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0,
    ];
    chain.extend_from_slice(&transport);
    assert_eq!(flow_hash(&ethernet(0x86dd, &v6(0, &chain))), base);
    *chain.last_mut().unwrap() = 255;
    assert_eq!(flow_hash(&ethernet(0x86dd, &v6(0, &chain))), base);
    chain[36] ^= 1;
    assert_ne!(flow_hash(&ethernet(0x86dd, &v6(0, &chain))), base);

    let mut fragment = vec![
        60, 0, 0, 1, 1, 2, 3, 4, // Fragment header points at destination options.
        17, 0, 0, 0, 0, 0, 0, 0,
    ];
    fragment.extend_from_slice(&transport);
    let first = flow_hash(&ethernet(0x86dd, &v6(44, &fragment)));
    fragment[2..4].copy_from_slice(&8_u16.to_be_bytes()); // Final non-first fragment.
    fragment[8..].fill(255);
    assert_eq!(flow_hash(&ethernet(0x86dd, &v6(44, &fragment))), first);
}

#[test]
fn malformed_ip_and_non_ip_use_ethernet_fallback_without_payload_hashing() {
    let malformed_v4 = flow_hash(&ethernet(0x0800, &[0; 20]));
    let mut ip = v4();
    for length in 0..24 {
        assert_eq!(flow_hash(&ethernet(0x0800, &ip[..length])), malformed_v4);
    }
    ip[0] = 0x4f;
    assert_eq!(flow_hash(&ethernet(0x0800, &ip)), malformed_v4);
    let malformed_v6 = flow_hash(&ethernet(0x86dd, &[0; 40]));
    for next in [0, 43, 44, 51, 60] {
        for length in 0..8 {
            assert_eq!(
                flow_hash(&ethernet(0x86dd, &v6(next, &vec![0xff; length]))),
                malformed_v6
            );
        }
    }
    assert_eq!(
        flow_hash(&ethernet(0x0806, &[1; 200])),
        flow_hash(&ethernet(0x0806, &[2; 10]))
    );
    // Every truncation of an Ethernet/VLAN/IP packet is safe.
    let mut tagged = vec![0, 1, 0x86, 0xdd];
    tagged.extend_from_slice(&v6(0, &[17, 0, 0, 0, 0, 0, 0, 0, 1, 2, 3, 4]));
    let frame = ethernet(0x8100, &tagged);
    for end in 0..=frame.len() {
        std::hint::black_box(flow_hash(&frame[..end]));
    }
}

fn encapsulate(v6_outer: bool, protocol: u8, payload: &[u8]) -> Vec<u8> {
    let ip = if v6_outer {
        v6(protocol, payload)
    } else {
        let mut ip = v4();
        ip.truncate(20);
        ip[9] = protocol;
        ip.extend_from_slice(payload);
        let length = ip.len() as u16;
        ip[2..4].copy_from_slice(&length.to_be_bytes());
        ip
    };
    ethernet(if v6_outer { 0x86dd } else { 0x0800 }, &ip)
}

#[test]
fn encapsulated_flows_include_inner_ports_and_keep_outer_identity() {
    for outer in [false, true] {
        for (protocol, mut inner, port) in [
            (4, v4(), 20),
            (41, v6(17, &[1, 2, 3, 4, 0, 8, 0, 0]), 40),
            (143, ethernet(0x0800, &v4()), 34),
        ] {
            if protocol == 143 {
                inner.splice(12..12, [0x88, 0xa8, 0, 7, 0x81, 0, 0, 42]);
            }
            let port = port + if protocol == 143 { 8 } else { 0 };
            let base = flow_hash(&encapsulate(outer, protocol, &inner));
            *inner.last_mut().unwrap() ^= 1;
            assert_eq!(flow_hash(&encapsulate(outer, protocol, &inner)), base);
            inner[port] ^= 1;
            assert_ne!(flow_hash(&encapsulate(outer, protocol, &inner)), base);
            inner[port] ^= 1;
            let mut frame = encapsulate(outer, protocol, &inner);
            frame[14 + if outer { 8 } else { 12 }] ^= 1;
            assert_ne!(flow_hash(&frame), base);
            if protocol == 143 {
                inner[15] ^= 1;
                assert_ne!(flow_hash(&encapsulate(outer, protocol, &inner)), base);
            }
            if outer {
                let mut srh = vec![protocol, 2, 4, 0, 0, 0, 0, 0];
                srh.extend_from_slice(&[3; 16]);
                srh.extend_from_slice(&inner);
                assert_eq!(
                    flow_hash(&ethernet(0x86dd, &v6(43, &srh))),
                    flow_hash(&encapsulate(true, protocol, &inner))
                );
            }
        }
    }
}

#[test]
fn encapsulation_stops_at_fragments_and_one_inner_level() {
    for outer in [false, true] {
        let mut inner = v4();
        inner[6] = 0x20;
        let first = flow_hash(&encapsulate(outer, 4, &inner));
        inner[6..8].copy_from_slice(&1u16.to_be_bytes());
        inner[20..].fill(255);
        assert_eq!(flow_hash(&encapsulate(outer, 4, &inner)), first);
        let mut fragment = vec![17, 0, 0, 1, 1, 2, 3, 4, 1, 2, 3, 4];
        let first = flow_hash(&encapsulate(outer, 41, &v6(44, &fragment)));
        fragment[2..4].copy_from_slice(&8u16.to_be_bytes());
        fragment[8..].fill(255);
        assert_eq!(
            flow_hash(&encapsulate(outer, 41, &v6(44, &fragment))),
            first
        );

        let mut nested = v6(4, &v4());
        let first = flow_hash(&encapsulate(outer, 41, &nested));
        nested[60] ^= 1;
        assert_eq!(flow_hash(&encapsulate(outer, 41, &nested)), first);
        nested[8] ^= 1;
        assert_ne!(flow_hash(&encapsulate(outer, 41, &nested)), first);

        let mut frame = encapsulate(outer, 4, &v4());
        if outer {
            frame[20] = 44;
            frame.splice(54..54, [4, 0, 0, 1, 1, 2, 3, 4]);
            frame[18..20].copy_from_slice(&48u16.to_be_bytes());
        } else {
            frame[20] = 0x20;
        }
        let first = flow_hash(&frame);
        *frame.last_mut().unwrap() ^= 1;
        let port = if outer { 82 } else { 54 };
        frame[port] ^= 1;
        assert_eq!(flow_hash(&frame), first);
    }
}

#[test]
fn truncated_inner_headers_fall_back_without_reading_payload() {
    for outer in [false, true] {
        for (protocol, inner, required) in [
            (4, v4(), 40),
            (41, v6(17, &[1, 2, 3, 4]), 44),
            (143, ethernet(0x0800, &v4()), 54),
        ] {
            let fallback = flow_hash(&encapsulate(outer, protocol, &[]));
            for end in 0..required {
                assert_eq!(
                    flow_hash(&encapsulate(outer, protocol, &inner[..end])),
                    fallback
                );
            }
        }
        let mut incomplete_tag = ethernet(0x8100, &[0, 42, 0x81, 0]);
        incomplete_tag.extend_from_slice(&[0, 7]);
        assert_eq!(
            flow_hash(&encapsulate(outer, 143, &incomplete_tag)),
            flow_hash(&encapsulate(outer, 143, &[]))
        );
    }
}

#[test]
fn encapsulated_non_ip_ethernet_uses_only_its_link_key() {
    let mut inner = ethernet(0x0806, &[0; 28]);
    let base = flow_hash(&encapsulate(true, 143, &inner));
    inner[14..].fill(255);
    assert_eq!(flow_hash(&encapsulate(true, 143, &inner)), base);
    inner[0] ^= 1;
    assert_ne!(flow_hash(&encapsulate(true, 143, &inner)), base);
}

#[test]
fn scatter_gather_pieces_borrow_original_frame_and_match_contiguous_encoding() {
    use unquic::wire::{DATA_PREFIX_LEN, encode_data_prefix, encode_datagram};
    let mut sender = sender();
    let original = [42; MAX_FRAME_LEN];
    let mut frame = sender.frame(&original).unwrap();
    let mut prefixes = [[0; DATA_PREFIX_LEN]; 8];
    let mut pieces = Vec::new();
    for prefix in &mut prefixes {
        let piece = frame.encode_next_split(prefix).unwrap().unwrap();
        let start = piece.bytes.as_ptr() as usize - original.as_ptr() as usize;
        assert_eq!(piece.bytes, &original[start..start + piece.bytes.len()]);
        assert!(start + piece.bytes.len() <= original.len());
        pieces.push(piece);
    }
    assert!(
        frame
            .encode_next_split(&mut [0; DATA_PREFIX_LEN])
            .unwrap()
            .is_none()
    );
    for (prefix, piece) in prefixes.iter().zip(pieces) {
        let mut packet = prefix.to_vec();
        packet.extend_from_slice(piece.bytes);
        assert_eq!(packet.len(), piece.packet.length);
        let datagram = Datagram::decode(&packet).unwrap();
        let record = datagram.records().next().unwrap();
        let mut contiguous = [0; BASE_UDP_PAYLOAD];
        let length = encode_datagram(datagram.header(), &[record], &mut contiguous).unwrap();
        assert_eq!(&contiguous[..length], packet);
        let mut separate = [0; DATA_PREFIX_LEN];
        assert_eq!(
            encode_data_prefix(datagram.header(), record, &mut separate).unwrap(),
            length
        );
        assert_eq!(separate, *prefix);
        let saved = separate;
        assert!(encode_data_prefix(datagram.header(), Record::Probe, &mut separate).is_err());
        assert_eq!(separate, saved);
    }
}

#[test]
fn padded_probes_borrow_shared_padding_and_match_the_wire_codec() {
    use unquic::wire::{PROBE_PREFIX_LEN, encode_datagram};
    let mut lane = sender();
    let mut prefix = [0xa5; PROBE_PREFIX_LEN];
    for length in [
        0,
        BASE_UDP_PAYLOAD - 1,
        unquic::socket::MAX_UDP_PAYLOAD + 1,
        usize::MAX,
    ] {
        assert!(matches!(
            lane.probe(length, &mut prefix),
            Err(PackError::Limit)
        ));
        assert_eq!(prefix, [0xa5; PROBE_PREFIX_LEN]);
    }
    let first = lane.probe(BASE_UDP_PAYLOAD, &mut prefix).unwrap();
    assert_eq!(first.packet.number, 1);
    let mut previous = first.packet.number;
    for length in BASE_UDP_PAYLOAD..=unquic::socket::MAX_UDP_PAYLOAD {
        let probe = lane.probe(length, &mut prefix).unwrap();
        assert_eq!(probe.packet.number, previous + 1);
        previous = probe.packet.number;
        assert_eq!(probe.bytes.as_ptr(), first.bytes.as_ptr());
        assert_eq!(probe.bytes.len() + prefix.len(), length);
        assert_eq!(probe.packet.length, length);
        let mut bytes = prefix.to_vec();
        bytes.extend_from_slice(probe.bytes);
        let datagram = Datagram::decode(&bytes).unwrap();
        assert_eq!(datagram.header().packet_number, previous);
        let records: Vec<_> = datagram.records().collect();
        assert_eq!(records.len(), 2);
        assert!(matches!(records[0], Record::Probe));
        assert!(
            matches!(records[1], Record::Padding { length: n } if usize::from(n) == length - PROBE_PREFIX_LEN)
        );
        let mut contiguous = vec![0xa5; length];
        assert_eq!(
            encode_datagram(datagram.header(), &records, &mut contiguous).unwrap(),
            length
        );
        assert_eq!(bytes, contiguous);
    }
}

fn pack_all(sender: &mut pack::Lane, frames: &[&[u8]], limit: usize) -> Vec<Vec<u8>> {
    let inputs: Vec<_> = frames
        .iter()
        .map(|bytes| pack::FrameInput {
            id: sender.start_frame(bytes.len()).unwrap(),
            header: &[],
            bytes,
            offset: 0,
            end: bytes.len(),
        })
        .collect();
    let mut output = Vec::new();
    let mut batch = pack::Batch::new();
    for chunk in inputs.chunks(pack::BATCH_CAPACITY) {
        let mut pending = chunk.to_vec();
        while !pending.is_empty() {
            batch.prepare(sender, &pending, limit).unwrap();
            assert!(!batch.is_empty());
            assert!(batch.len() <= pack::BATCH_CAPACITY);
            let frames: Vec<_> = pending.iter().map(|frame| frame.bytes).collect();
            for packet in 0..batch.len() {
                let meta = batch.packet(packet).unwrap();
                assert_eq!(meta.number, output.len() as u64 + 1);
                let mut slices = [&[][..]; pack::PACKED_SLICES];
                let parts = batch.slices(packet, &pending, &mut slices).unwrap();
                for payload in parts.iter().skip(2).step_by(2) {
                    assert!(frames.iter().any(|frame| {
                        let range = frame.as_ptr_range();
                        payload.as_ptr() >= range.start
                            && payload.as_ptr().wrapping_add(payload.len()) <= range.end
                    }));
                }
                let bytes: Vec<u8> = parts.iter().flat_map(|part| part.iter().copied()).collect();
                assert_eq!(bytes.len(), meta.length);
                assert!(bytes.len() <= limit);
                Datagram::decode(&bytes).unwrap();
                output.push(bytes);
                for (index, range) in batch.pieces(packet).unwrap() {
                    assert_eq!(pending[index].offset, range.start);
                    pending[index].offset = range.end;
                }
            }
            pending.retain(|frame| frame.offset < frame.bytes.len());
        }
    }
    output
}

#[test]
fn packed_sequences_round_trip_with_borrowed_payloads_and_reordering() {
    for limit in [
        BASE_UDP_PAYLOAD,
        1452,
        4096,
        unquic::socket::MAX_UDP_PAYLOAD,
    ] {
        for seed in 0..64_u64 {
            let mut random = seed + 1;
            let frames: Vec<Vec<u8>> = (0..32)
                .map(|index| {
                    random ^= random << 13;
                    random ^= random >> 7;
                    random ^= random << 17;
                    let length = if seed == 0 {
                        14
                    } else {
                        [
                            14,
                            15,
                            1163,
                            1164,
                            1165,
                            1514,
                            9022,
                            14 + random as usize % (MAX_FRAME_LEN - 13),
                        ][index % 8]
                    };
                    (0..length).map(|byte| (byte ^ index) as u8).collect()
                })
                .collect();
            let borrowed: Vec<_> = frames.iter().map(Vec::as_slice).collect();
            let packets = pack_all(&mut sender(), &borrowed, limit);
            assert!(
                packets
                    .iter()
                    .any(|packet| Datagram::decode(packet).unwrap().records().count() > 1)
            );
            for reverse in [false, true] {
                let mut receiver = ReceiveLane::new(1, 9022).unwrap();
                let mut seen = [false; 32];
                for step in 0..packets.len() {
                    let index = if reverse {
                        packets.len() - 1 - step
                    } else {
                        step
                    };
                    let datagram = Datagram::decode(&packets[index]).unwrap();
                    assert_eq!(receiver.admit(&datagram, Duration::ZERO), Admission::Fresh);
                    assert_eq!(
                        receiver.admit(&datagram, Duration::ZERO),
                        Admission::Duplicate
                    );
                    while let Some(frame) = receiver.front(Duration::ZERO) {
                        let index = (frame.id & 0x0000_ffff_ffff_ffff) as usize - 1;
                        assert!(!seen[index]);
                        assert_eq!(frame.bytes, frames[index]);
                        seen[index] = true;
                        receiver.finish_front();
                    }
                }
                assert!(seen.into_iter().all(|delivered| delivered));
            }
        }
    }
}

#[test]
fn packed_frame_tail_and_next_head_share_one_packet() {
    let packets = pack_all(&mut sender(), &[&[1; 1514], &[2; 1514]], BASE_UDP_PAYLOAD);
    let lengths: Vec<Vec<usize>> = packets
        .iter()
        .map(|bytes| {
            Datagram::decode(bytes)
                .unwrap()
                .records()
                .map(|record| match record {
                    Record::Data { bytes, .. } => bytes.len(),
                    _ => unreachable!(),
                })
                .collect()
        })
        .collect();
    assert_eq!(lengths, [vec![1164], vec![350, 794], vec![720]]);
}

#[test]
fn batch_input_validation_and_empty_queue_preserve_packet_numbers() {
    let mut sender = sender();
    let id = sender.start_frame(60).unwrap();
    let input = pack::FrameInput {
        id,
        header: &[],
        bytes: &[0; 60],
        offset: 0,
        end: 60,
    };
    let mut batch = pack::Batch::new();
    for bad in [
        pack::FrameInput { id: 0, ..input },
        pack::FrameInput {
            id: (2 << 48) | 1,
            ..input
        },
        pack::FrameInput {
            id: id + 1,
            ..input
        },
        pack::FrameInput {
            bytes: &[0; 13],
            ..input
        },
        pack::FrameInput {
            bytes: &[0; MAX_FRAME_LEN + 1],
            ..input
        },
        pack::FrameInput {
            offset: 60,
            ..input
        },
        pack::FrameInput { end: 0, ..input },
        pack::FrameInput { end: 61, ..input },
        pack::FrameInput {
            offset: 32,
            end: 31,
            ..input
        },
    ] {
        assert_eq!(
            batch.prepare(&mut sender, &[bad], BASE_UDP_PAYLOAD),
            Err(PackError::Frame)
        );
    }
    assert_eq!(
        batch.prepare(
            &mut sender,
            &[input; pack::BATCH_CAPACITY + 1],
            BASE_UDP_PAYLOAD
        ),
        Err(PackError::Frame)
    );
    assert_eq!(
        batch.prepare(&mut sender, &[input], 0),
        Err(PackError::Limit)
    );
    batch.prepare(&mut sender, &[], BASE_UDP_PAYLOAD).unwrap();
    assert!(batch.is_empty());
    assert!(batch.packet(0).is_none());
    assert!(batch.pieces(0).is_none());
    batch
        .prepare(&mut sender, &[input], BASE_UDP_PAYLOAD)
        .unwrap();
    assert_eq!(batch.packet(0).unwrap().number, 1);
    let mut slices = [&[][..]; pack::PACKED_SLICES];
    assert!(batch.slices(0, &[], &mut slices).is_err());
    assert!(batch.slices(1, &[input], &mut slices).is_err());
    assert!(batch.slices(0, &[input], &mut []).is_err());
    assert!(
        batch
            .slices(
                0,
                &[pack::FrameInput {
                    bytes: &[0; 59],
                    ..input
                }],
                &mut slices
            )
            .is_err()
    );
}

#[test]
fn frame_bursts_continue_across_packet_capacity_and_flush_the_tail() {
    for size in [14, 60, 64, 128, 9022] {
        let frames: Vec<Vec<u8>> = (0..256).map(|index| vec![index as u8; size]).collect();
        let borrowed: Vec<_> = frames.iter().map(Vec::as_slice).collect();
        let packets = pack_all(&mut sender(), &borrowed, BASE_UDP_PAYLOAD);
        let mut receiver = ReceiveLane::new(1, 9022).unwrap();
        let mut delivered = 0;
        for bytes in &packets {
            let datagram = Datagram::decode(bytes).unwrap();
            assert!(datagram.records().count() <= unquic::wire::MAX_RECORDS);
            assert_eq!(receiver.admit(&datagram, Duration::ZERO), Admission::Fresh);
            while let Some(frame) = receiver.front(Duration::ZERO) {
                assert_eq!(frame.bytes, frames[delivered]);
                delivered += 1;
                receiver.finish_front();
            }
        }
        assert_eq!(delivered, frames.len());
        if size < 9022 {
            assert!(
                packets.len() < 40,
                "{size}-byte frames used {} packets",
                packets.len()
            );
        }
    }
}

#[test]
fn packed_datagrams_send_original_slices_through_the_socket_batch() {
    use unquic::socket::{ReceiveSocket, SendDatagram, TransmitSocket};
    let rx = (0..16)
        .find_map(|_| {
            let probe = std::net::UdpSocket::bind("[::1]:0").unwrap();
            let std::net::SocketAddr::V6(address) = probe.local_addr().unwrap() else {
                unreachable!()
            };
            drop(probe);
            ReceiveSocket::bind(address, None).ok()
        })
        .unwrap();
    let tx = TransmitSocket::connect(&rx, rx.local_addr()).unwrap();
    let frames = [&[1; 1514][..], &[2; 1514][..]];
    let mut sender = sender();
    let inputs: Vec<_> = frames
        .iter()
        .map(|bytes| pack::FrameInput {
            id: sender.start_frame(bytes.len()).unwrap(),
            header: &[],
            bytes,
            offset: 0,
            end: bytes.len(),
        })
        .collect();
    let mut batch = pack::Batch::new();
    batch
        .prepare(&mut sender, &inputs, BASE_UDP_PAYLOAD)
        .unwrap();
    assert_eq!(batch.len(), 3);
    let mut storage = [[&[][..]; pack::PACKED_SLICES]; 3];
    let outgoing: Vec<_> = storage
        .iter_mut()
        .enumerate()
        .map(|(packet, slices)| SendDatagram {
            parts: batch.slices(packet, &inputs, slices).unwrap(),
        })
        .collect();
    let mut first = 0;
    let mut receiver = ReceiveLane::new(1, 9022).unwrap();
    let mut delivered = 0;
    while first < outgoing.len() {
        let count = tx.send_batch(&outgoing[first..], BASE_UDP_PAYLOAD).unwrap();
        assert!(count > 0);
        for _ in 0..count {
            let mut bytes = [0; BASE_UDP_PAYLOAD];
            let length = rx.recv(&mut bytes).unwrap().0;
            let datagram = Datagram::decode(&bytes[..length]).unwrap();
            assert_eq!(datagram.header().packet_number, first as u64 + 1);
            assert_eq!(receiver.admit(&datagram, Duration::ZERO), Admission::Fresh);
            while let Some(frame) = receiver.front(Duration::ZERO) {
                assert_eq!(frame.bytes, frames[delivered]);
                delivered += 1;
                receiver.finish_front();
            }
            first += 1;
        }
    }
    assert_eq!(delivered, 2);
}

#[test]
fn dropping_alternate_gso_batches_leaves_no_orphan_frames() {
    for limit in [
        BASE_UDP_PAYLOAD,
        1452,
        4096,
        unquic::socket::MAX_UDP_PAYLOAD,
    ] {
        for size in [60, 1514, MAX_FRAME_LEN] {
            let mut sender = sender();
            let mut receiver = ReceiveLane::new(1, MAX_FRAME_LEN as u16).unwrap();
            let frames: Vec<_> = (0..256).map(|i| vec![i as u8; size]).collect();
            let mut delivered = 0;
            let mut expected = 0;
            let mut batches = 0;
            for frames in frames.chunks(pack::BATCH_CAPACITY) {
                let mut inputs: Vec<_> = frames
                    .iter()
                    .map(|bytes| pack::FrameInput {
                        id: sender.start_frame(bytes.len()).unwrap(),
                        header: &[],
                        bytes,
                        offset: 0,
                        end: bytes.len(),
                    })
                    .collect();
                while !inputs.is_empty() {
                    let mut batch = pack::Batch::new();
                    batch.prepare(&mut sender, &inputs, limit).unwrap();
                    assert!(!batch.is_empty());
                    assert!(
                        (0..batch.len())
                            .map(|i| batch.packet(i).unwrap().length)
                            .sum::<usize>()
                            <= 65_535 - 40 - 8
                    );
                    let mut offsets = vec![0; inputs.len()];
                    for packet in 0..batch.len() {
                        let mut slices = [&[][..]; pack::PACKED_SLICES];
                        let bytes: Vec<_> = batch
                            .slices(packet, &inputs, &mut slices)
                            .unwrap()
                            .iter()
                            .flat_map(|part| part.iter().copied())
                            .collect();
                        for (frame, range) in batch.pieces(packet).unwrap() {
                            offsets[frame] = range.end;
                        }
                        if batches % 2 == 0 {
                            assert_eq!(
                                receiver.admit(&Datagram::decode(&bytes).unwrap(), Duration::ZERO),
                                Admission::Fresh
                            );
                            while let Some(frame) = receiver.front(Duration::ZERO) {
                                let index = (frame.id & 0x0000_ffff_ffff_ffff) as usize - 1;
                                assert!(frame.bytes.iter().all(|&byte| byte == index as u8));
                                delivered += 1;
                                receiver.finish_front();
                            }
                        }
                    }
                    let completed = offsets.iter().filter(|&&offset| offset == size).count();
                    assert!(completed > 0);
                    assert!(offsets.iter().all(|&offset| offset == 0 || offset == size));
                    assert_eq!(receiver.retained_frames(), 0);
                    if batches % 2 == 0 {
                        expected += completed;
                    }
                    inputs.drain(..completed);
                    batches += 1;
                }
            }
            assert_eq!(delivered, expected);
            assert!(delivered > 0 && delivered < frames.len());
        }
    }
}

#[test]
fn scattered_headers_match_contiguous_frames_across_boundaries_and_retries() {
    for limit in [BASE_UDP_PAYLOAD, 1452, unquic::socket::MAX_UDP_PAYLOAD] {
        for size in [14, 60, 1514, MAX_FRAME_LEN] {
            let frames: Vec<Vec<u8>> = (0..pack::BATCH_CAPACITY)
                .map(|i| (0..size).map(|j| (i * 17 + j) as u8).collect())
                .collect();
            for split in [0, 1, 13, 42, 128, 1164, 1384, size] {
                let split = split.min(size);
                let mut sender = sender();
                let mut inputs: Vec<_> = frames
                    .iter()
                    .enumerate()
                    .map(|(i, bytes)| {
                        let split = if i % 3 == 0 { 0 } else { split };
                        pack::FrameInput {
                            id: sender.start_frame(size).unwrap(),
                            header: &bytes[..split],
                            bytes: &bytes[split..],
                            offset: i % size,
                            end: size,
                        }
                    })
                    .collect();
                let mut assembled: Vec<_> = inputs
                    .iter()
                    .enumerate()
                    .map(|(i, f)| frames[i][..f.offset].to_vec())
                    .collect();
                let mut first = 0;
                while first < inputs.len() {
                    let mut batch = pack::Batch::new();
                    batch.prepare(&mut sender, &inputs[first..], limit).unwrap();
                    assert!(!batch.is_empty());
                    let mut slices = [&[][..]; pack::BATCH_SLICES];
                    let mut used = 0;
                    for packet in 0..batch.len() {
                        let parts = batch
                            .slices(packet, &inputs[first..], &mut slices[used..])
                            .unwrap();
                        used += parts.len();
                        let encoded: Vec<_> =
                            parts.iter().flat_map(|b| b.iter().copied()).collect();
                        assert_eq!(encoded.len(), batch.packet(packet).unwrap().length);
                        let datagram = Datagram::decode(&encoded).unwrap();
                        for record in datagram.records() {
                            let Record::Data {
                                frame_id,
                                total_len,
                                offset,
                                bytes,
                            } = record
                            else {
                                panic!("unexpected record")
                            };
                            let index = (frame_id & 0x0000_ffff_ffff_ffff) as usize - 1;
                            assert_eq!(total_len as usize, size);
                            assert_eq!(offset as usize, assembled[index].len());
                            assembled[index].extend_from_slice(bytes);
                        }
                        for (i, range) in batch.pieces(packet).unwrap() {
                            inputs[first + i].offset = range.end;
                        }
                        // Reprepare after a partial physical batch, including inside headers.
                        if packet == batch.len() / 2 {
                            break;
                        }
                    }
                    assert!(used <= pack::BATCH_SLICES);
                    while first < inputs.len() && inputs[first].offset == size {
                        first += 1;
                    }
                }
                assert_eq!(assembled, frames);
            }
        }
    }
}

fn selected_packet(batch: &pack::Batch, packet: usize, inputs: &[pack::FrameInput<'_>]) -> Vec<u8> {
    let mut slices = [&[][..]; pack::PACKED_SLICES];
    batch
        .slices(packet, inputs, &mut slices)
        .unwrap()
        .iter()
        .flat_map(|bytes| bytes.iter().copied())
        .collect()
}

#[test]
fn selected_ranges_preserve_total_length_across_spans_and_partial_sends() {
    for limit in [BASE_UDP_PAYLOAD, 1452, unquic::socket::MAX_UDP_PAYLOAD] {
        let bytes: Vec<_> = (0..MAX_FRAME_LEN).map(|i| i as u8).collect();
        for split in [0, 1, 13, 42, 128, 1164, MAX_FRAME_LEN] {
            let mut sender = sender();
            let ranges = [
                split.saturating_sub(1)..(split + 1).min(MAX_FRAME_LEN),
                123..1514,
                8000..MAX_FRAME_LEN,
            ];
            let mut inputs: Vec<_> = ranges
                .iter()
                .map(|range| pack::FrameInput {
                    id: sender.start_frame(bytes.len()).unwrap(),
                    header: &bytes[..split],
                    bytes: &bytes[split..],
                    offset: range.start,
                    end: range.end,
                })
                .collect();
            let mut assembled = [Vec::new(), Vec::new(), Vec::new()];
            let mut first = 0;
            let mut last = 0;
            while first < inputs.len() {
                let mut batch = pack::Batch::new();
                batch.prepare(&mut sender, &inputs[first..], limit).unwrap();
                assert!(!batch.is_empty());
                let packet = batch.packet(0).unwrap();
                assert!(packet.number > last);
                last = packet.number;
                let encoded = selected_packet(&batch, 0, &inputs[first..]);
                assert!(encoded.len() <= limit);
                let datagram = Datagram::decode(&encoded).unwrap();
                for ((index, range), record) in batch.pieces(0).unwrap().zip(datagram.records()) {
                    let index = first + index;
                    let Record::Data {
                        frame_id,
                        total_len,
                        offset,
                        bytes: payload,
                    } = record
                    else {
                        panic!("unexpected record");
                    };
                    assert_eq!(frame_id, inputs[index].id);
                    assert_eq!(total_len as usize, bytes.len());
                    assert_eq!(offset as usize, range.start);
                    assert_eq!(inputs[index].offset, range.start);
                    assert!(range.end <= inputs[index].end);
                    assert_eq!(payload, &bytes[range.clone()]);
                    assembled[index].extend_from_slice(payload);
                    inputs[index].offset = range.end;
                }
                // Only the successful prefix advances; every retry gets fresh numbers.
                while first < inputs.len() && inputs[first].offset == inputs[first].end {
                    first += 1;
                }
            }
            for (actual, range) in assembled.iter().zip(ranges) {
                assert_eq!(actual, &bytes[range]);
            }
        }
    }
}

#[test]
fn losing_each_original_piece_and_its_first_repair_still_delivers_once() {
    use unquic::loss::FrameRecovery;
    let bytes: Vec<_> = (0..MAX_FRAME_LEN).map(|i| i as u8).collect();
    for limit in [BASE_UDP_PAYLOAD, 1452, unquic::socket::MAX_UDP_PAYLOAD] {
        let pieces = MAX_FRAME_LEN.div_ceil(limit - unquic::wire::DATA_PREFIX_LEN);
        for lost in 0..pieces {
            let mut sender = sender();
            let input = pack::FrameInput {
                id: sender.start_frame(bytes.len()).unwrap(),
                header: &bytes[..128],
                bytes: &bytes[128..],
                offset: 0,
                end: bytes.len(),
            };
            let mut batch = pack::Batch::new();
            batch.prepare(&mut sender, &[input], limit).unwrap();
            assert_eq!(batch.len(), pieces);
            let originals: Vec<_> = (0..batch.len())
                .map(|p| selected_packet(&batch, p, &[input]))
                .collect();
            let mut receiver = ReceiveLane::new(1, MAX_FRAME_LEN as u16).unwrap();
            let mut state = FrameRecovery::new(bytes.len()).unwrap();
            let mut missing = None;
            for (packet, encoded) in originals.iter().enumerate() {
                let range = batch.pieces(packet).unwrap().next().unwrap().1;
                assert!(state.submitted(range.clone(), 0));
                if packet == lost {
                    missing = Some(range);
                } else {
                    assert_eq!(
                        receiver.admit(&Datagram::decode(encoded).unwrap(), Duration::ZERO),
                        Admission::Fresh
                    );
                    assert_eq!(state.acknowledge(range.clone(), 0), Some(range.len()));
                }
            }
            assert!(receiver.front(Duration::ZERO).is_none());
            let missing = missing.unwrap();
            assert!(state.lost(missing.clone(), 0));
            for attempt in 1..=2 {
                assert_eq!(
                    state.next_repair(usize::MAX),
                    Some((missing.clone(), attempt))
                );
                let repair = pack::FrameInput {
                    offset: missing.start,
                    end: missing.end,
                    ..input
                };
                batch.prepare(&mut sender, &[repair], limit).unwrap();
                assert_eq!(batch.len(), 1);
                assert!(batch.packet(0).unwrap().number > originals.len() as u64);
                let encoded = selected_packet(&batch, 0, &[repair]);
                assert!(state.submitted(missing.clone(), attempt));
                if attempt == 1 {
                    assert!(state.lost(missing.clone(), attempt));
                } else {
                    assert_eq!(
                        receiver.admit(&Datagram::decode(&encoded).unwrap(), Duration::ZERO),
                        Admission::Fresh
                    );
                    assert_eq!(
                        state.acknowledge(missing.clone(), attempt),
                        Some(missing.len())
                    );
                }
            }
            assert_eq!(state.remaining(), 0);
            assert_eq!(receiver.front(Duration::ZERO).unwrap().bytes, bytes);
            assert!(receiver.finish_front());
            let late = Datagram::decode(&originals[lost]).unwrap();
            assert_eq!(receiver.admit(&late, Duration::ZERO), Admission::Fresh);
            assert_eq!(receiver.admit(&late, Duration::ZERO), Admission::Duplicate);
            assert_eq!(state.acknowledge(missing, 0), Some(0));
            assert!(receiver.front(Duration::ZERO).is_none());
        }
    }
}

fn submit_tracked(
    batch: &pack::Batch,
    inputs: &[pack::FrameInput<'_>],
    attempt: u8,
    history: &mut unquic::ack::History,
    journal: &mut unquic::ack::PieceJournal,
    recovery: &mut [unquic::loss::FrameRecovery],
    now: Duration,
) -> Vec<Vec<u8>> {
    let mut encoded = Vec::new();
    for index in 0..batch.len() {
        let packet = batch.packet(index).unwrap();
        let pieces: Vec<_> = batch
            .pieces(index)
            .unwrap()
            .map(|(frame, range)| unquic::ack::FramePiece {
                frame_slot: ((inputs[frame].id & 0x0000_ffff_ffff_ffff) - 1) as u16,
                frame_id: inputs[frame].id,
                start: range.start as u16,
                end: range.end as u16,
                attempt,
            })
            .collect();
        history
            .reserve(packet.number, (packet.length + 48) as u16, true, true)
            .unwrap();
        assert!(journal.reserve_packet(packet.number as usize, &pieces));
        encoded.push(selected_packet(batch, index, inputs));
    }
    for index in 0..batch.len() {
        let number = batch.packet(index).unwrap().number;
        assert!(history.submit(number, now));
        for piece in journal.packet(number as usize) {
            assert!(recovery[usize::from(piece.frame_slot)].submitted(
                usize::from(piece.start)..usize::from(piece.end),
                piece.attempt
            ));
        }
    }
    encoded
}

fn apply_tracked_ack(
    receiver: &mut ReceiveLane,
    history: &mut unquic::ack::History,
    journal: &mut unquic::ack::PieceJournal,
    recovery: &mut [unquic::loss::FrameRecovery],
    now: Duration,
) -> unquic::ack::AckProgress {
    use unquic::wire::{AckRange, MAX_ACK_RANGES};
    let mut storage = [AckRange { start: 0, end: 0 }; MAX_ACK_RANGES];
    let Record::Ack { ranges, .. } = receiver.take_ack(now, &mut storage).unwrap() else {
        panic!("expected ACK");
    };
    history.begin_ack(ranges).unwrap();
    receiver.ack_transmitted(now);
    for _ in 0..256 {
        if let Some(progress) = history.advance_ack(1, |acked| {
            while let Some(piece) = journal.pop_packet(acked.packet.number as usize) {
                assert!(
                    recovery[usize::from(piece.frame_slot)]
                        .acknowledge(
                            usize::from(piece.start)..usize::from(piece.end),
                            piece.attempt
                        )
                        .is_some()
                );
            }
        }) {
            return progress;
        }
    }
    panic!("bounded ACK did not finish");
}

#[test]
fn packed_tail_head_loss_recovers_through_actual_receiver_acks_without_duplicate_delivery() {
    use unquic::{
        ack::{History, PieceJournal},
        loss::{FrameRecovery, Rtt},
    };
    let time = Duration::from_millis;
    for (limit, length) in [
        (BASE_UDP_PAYLOAD, 1514),
        (1452, 1514),
        (BASE_UDP_PAYLOAD, MAX_FRAME_LEN),
        (1452, MAX_FRAME_LEN),
        (unquic::socket::MAX_UDP_PAYLOAD, MAX_FRAME_LEN),
    ] {
        let bytes: Vec<Vec<u8>> = (0..2)
            .map(|frame| {
                (0..length)
                    .map(|byte| (byte ^ (frame * 37)) as u8)
                    .collect()
            })
            .collect();
        let mut sender = sender();
        let inputs: Vec<_> = bytes
            .iter()
            .map(|bytes| pack::FrameInput {
                id: sender.start_frame(length).unwrap(),
                header: &bytes[..128],
                bytes: &bytes[128..],
                offset: 0,
                end: length,
            })
            .collect();
        let mut history = History::new().unwrap();
        let mut journal = PieceJournal::new(64, 2, 64).unwrap();
        let mut recovery = [
            FrameRecovery::new(length).unwrap(),
            FrameRecovery::new(length).unwrap(),
        ];
        for (slot, input) in inputs.iter().enumerate() {
            assert!(journal.bind_frame(slot, input.id, length));
        }
        let mut receiver = ReceiveLane::new(1, MAX_FRAME_LEN as u16).unwrap();
        let mut batch = pack::Batch::new();
        batch.prepare(&mut sender, &inputs, limit).unwrap();
        let lost = (0..batch.len())
            .find(|&p| batch.pieces(p).unwrap().count() == 2)
            .unwrap();
        let mixed: Vec<_> = batch.pieces(lost).unwrap().collect();
        assert_eq!(mixed[0].0, 0);
        assert_eq!(mixed[0].1.end, length);
        assert_eq!(mixed[1].0, 1);
        assert_eq!(mixed[1].1.start, 0);
        assert!(mixed[1].1.end < length);
        let originals = submit_tracked(
            &batch,
            &inputs,
            0,
            &mut history,
            &mut journal,
            &mut recovery,
            time(0),
        );
        let lost_number = batch.packet(lost).unwrap().number;
        for (index, encoded) in originals.iter().enumerate().rev() {
            if index != lost {
                assert_eq!(
                    receiver.admit(&Datagram::decode(encoded).unwrap(), time(1)),
                    Admission::Fresh
                );
            }
        }
        assert!(receiver.front(time(1)).is_none());
        let ack = apply_tracked_ack(
            &mut receiver,
            &mut history,
            &mut journal,
            &mut recovery,
            time(3),
        );
        assert_eq!(ack.packets as usize, originals.len() - 1);
        let mut rtt = Rtt::new();
        assert!(rtt.sample(ack.rtt_sent_at.unwrap(), time(3), 0));
        let mut lost_number = lost_number;
        let mut largest_acked = batch.packet(batch.len() - 1).unwrap().number;
        let mut first_repair = Vec::new();
        for attempt in 1..=2 {
            let now = time(if attempt == 1 { 9 } else { 15 });
            let mut scan = history.outstanding_scan();
            let mut losses = 0;
            while !scan.is_complete() {
                let mut budget = 1;
                if let Some(packet) = scan.advance(&history, &mut budget) {
                    assert_eq!(packet.number, lost_number);
                    assert!(
                        rtt.loss_deadline(packet.number, largest_acked, packet.sent_at)
                            .unwrap()
                            <= now
                    );
                    assert_eq!(history.lose(packet.number), Some(packet));
                    for piece in journal.packet(packet.number as usize) {
                        assert!(recovery[usize::from(piece.frame_slot)].lost(
                            usize::from(piece.start)..usize::from(piece.end),
                            piece.attempt
                        ));
                    }
                    losses += 1;
                }
            }
            assert_eq!(losses, 1);
            let repairs: Vec<_> = recovery
                .iter()
                .zip(&inputs)
                .map(|(state, input)| {
                    let (range, generation) = state.next_repair(usize::MAX).unwrap();
                    assert_eq!(generation, attempt);
                    pack::FrameInput {
                        offset: range.start,
                        end: range.end,
                        ..*input
                    }
                })
                .collect();
            batch.prepare(&mut sender, &repairs, limit).unwrap();
            assert_eq!(batch.len(), 1);
            let encoded = submit_tracked(
                &batch,
                &repairs,
                attempt,
                &mut history,
                &mut journal,
                &mut recovery,
                now + time(1),
            );
            if attempt == 1 {
                first_repair = encoded[0].clone();
                lost_number = batch.packet(0).unwrap().number;
                // Later accepted traffic supplies the ACK needed for time-threshold loss.
                let mut bytes = [0; BASE_UDP_PAYLOAD];
                let probe = sender.record(Record::Probe, &mut bytes).unwrap();
                history
                    .reserve(probe.number, (probe.length + 48) as u16, true, true)
                    .unwrap();
                assert!(history.submit(probe.number, time(11)));
                assert_eq!(
                    receiver.admit(&Datagram::decode(&bytes[..probe.length]).unwrap(), time(12)),
                    Admission::Fresh
                );
                let ack = apply_tracked_ack(
                    &mut receiver,
                    &mut history,
                    &mut journal,
                    &mut recovery,
                    time(13),
                );
                assert_eq!(ack.packets, 1);
                assert!(rtt.sample(ack.rtt_sent_at.unwrap(), time(13), 0));
                largest_acked = probe.number;
            } else {
                assert_eq!(
                    receiver.admit(&Datagram::decode(&encoded[0]).unwrap(), time(17)),
                    Admission::Fresh
                );
                let mut delivered = 0;
                while let Some(frame) = receiver.front(time(17)) {
                    let index = (frame.id & 0x0000_ffff_ffff_ffff) as usize - 1;
                    assert_eq!(frame.bytes, bytes[index]);
                    assert_eq!(delivered & (1 << index), 0, "duplicate frame");
                    delivered |= 1 << index;
                    assert!(receiver.finish_front());
                }
                assert_eq!(delivered, 3);
                let ack = apply_tracked_ack(
                    &mut receiver,
                    &mut history,
                    &mut journal,
                    &mut recovery,
                    time(18),
                );
                assert_eq!(ack.packets, 1);
                assert_eq!(history.inflight_bytes(), 0);
                assert!(recovery.iter().all(|frame| frame.remaining() == 0));
            }
        }
        for (slot, input) in inputs.iter().enumerate() {
            while journal.pop_frame(slot, input.id).is_some() {}
        }
        assert_eq!(journal.available(), 64);
        // Both previously lost copies can arrive late and be ACKed, but never redeliver.
        for encoded in [&originals[lost], &first_repair] {
            let datagram = Datagram::decode(encoded).unwrap();
            assert_eq!(receiver.admit(&datagram, time(19)), Admission::Fresh);
            assert_eq!(receiver.admit(&datagram, time(19)), Admission::Duplicate);
        }
        assert!(receiver.front(time(19)).is_none());
        let ack = apply_tracked_ack(
            &mut receiver,
            &mut history,
            &mut journal,
            &mut recovery,
            time(20),
        );
        assert_eq!(ack.packets, 2);
        assert_eq!(ack.released_bytes, 0);
        assert_eq!(history.inflight_bytes(), 0);
        assert_eq!(journal.available(), 64);
    }
}
