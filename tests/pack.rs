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
fn packed_sequences_round_trip_with_borrowed_payloads_and_reordering() {
    for limit in [BASE_UDP_PAYLOAD, unquic::socket::MAX_UDP_PAYLOAD] {
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
            let mut sender = sender();
            let mut encoder = sender.frames(&borrowed, limit).unwrap();
            let mut metadata = pack::Packed::new();
            let mut packets = Vec::new();
            let mut mixed = false;
            while let Some(packet) = encoder.encode_next_split(&mut metadata).unwrap() {
                assert_eq!(packet.number, packets.len() as u64 + 1);
                let mut slices = [&[][..]; pack::PACKED_SLICES];
                let parts = metadata.slices(&mut slices);
                for payload in parts.iter().skip(2).step_by(2) {
                    assert!(frames.iter().any(|frame| {
                        let range = frame.as_ptr_range();
                        payload.as_ptr() >= range.start
                            && payload.as_ptr().wrapping_add(payload.len()) <= range.end
                    }));
                }
                let bytes: Vec<u8> = parts.iter().flat_map(|part| part.iter().copied()).collect();
                assert_eq!(bytes.len(), packet.length);
                assert!(bytes.len() <= limit);
                let datagram = Datagram::decode(&bytes).unwrap();
                mixed |= datagram.records().count() > 1;
                packets.push(bytes);
            }
            assert!(mixed);
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
    let frames = [&[1; 1514][..], &[2; 1514][..]];
    let mut sender = sender();
    let mut encoder = sender.frames(&frames, BASE_UDP_PAYLOAD).unwrap();
    let mut metadata = pack::Packed::new();
    let mut lengths = Vec::new();
    while let Some(packet) = encoder.encode_next_split(&mut metadata).unwrap() {
        let mut slices = [&[][..]; pack::PACKED_SLICES];
        let bytes: Vec<u8> = metadata
            .slices(&mut slices)
            .iter()
            .flat_map(|part| part.iter().copied())
            .collect();
        let records: Vec<_> = Datagram::decode(&bytes).unwrap().records().collect();
        lengths.push(
            records
                .iter()
                .map(|record| match record {
                    Record::Data { bytes, .. } => bytes.len(),
                    _ => unreachable!(),
                })
                .collect::<Vec<_>>(),
        );
        assert_eq!(packet.length, bytes.len());
    }
    assert_eq!(lengths, [vec![1164], vec![350, 794], vec![720]]);
}

#[test]
fn packed_invalid_input_preserves_ids_and_empty_queue_emits_nothing() {
    let mut sender = sender();
    assert!(matches!(
        sender.frames(&[&[0; 13]], BASE_UDP_PAYLOAD),
        Err(PackError::Frame)
    ));
    assert!(matches!(
        sender.frames(&[&[0; 14]], 0),
        Err(PackError::Limit)
    ));
    let mut metadata = pack::Packed::new();
    let mut slices = [&[][..]; pack::PACKED_SLICES];
    assert!(metadata.slices(&mut slices).is_empty());
    assert_eq!(
        sender
            .frames(&[], BASE_UDP_PAYLOAD)
            .unwrap()
            .encode_next_split(&mut metadata)
            .unwrap(),
        None
    );
    let frames = [&[0; 14][..], &[0; 14][..]];
    let _abandoned = sender.frames(&frames, BASE_UDP_PAYLOAD).unwrap();
    assert_eq!(sender.frame(&[0; 14]).unwrap().id(), (1 << 48) | 3);
}

#[test]
fn small_frame_bursts_pack_tightly_and_flush_the_last_packet() {
    for size in [14, 60, 64, 128] {
        let frames: Vec<Vec<u8>> = (0..256).map(|index| vec![index as u8; size]).collect();
        let borrowed: Vec<_> = frames.iter().map(Vec::as_slice).collect();
        let mut sender = sender();
        let mut receiver = ReceiveLane::new(1, 9022).unwrap();
        let mut encoder = sender.frames(&borrowed, BASE_UDP_PAYLOAD).unwrap();
        let mut metadata = pack::Packed::new();
        let mut packets = 0;
        let mut delivered = 0;
        while let Some(packet) = encoder.encode_next_split(&mut metadata).unwrap() {
            let mut slices = [&[][..]; pack::PACKED_SLICES];
            let bytes: Vec<u8> = metadata
                .slices(&mut slices)
                .iter()
                .flat_map(|part| part.iter().copied())
                .collect();
            let datagram = Datagram::decode(&bytes).unwrap();
            assert!(datagram.records().count() <= unquic::wire::MAX_RECORDS);
            assert_eq!(receiver.admit(&datagram, Duration::ZERO), Admission::Fresh);
            while let Some(frame) = receiver.front(Duration::ZERO) {
                assert_eq!(frame.bytes, frames[delivered]);
                delivered += 1;
                receiver.finish_front();
            }
            assert_eq!(bytes.len(), packet.length);
            packets += 1;
        }
        assert_eq!(delivered, frames.len());
        assert!(packets < 40, "{size}-byte frames used {packets} packets");
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
    let mut encoder = sender.frames(&frames, BASE_UDP_PAYLOAD).unwrap();
    let mut metadata: [pack::Packed<'_>; 3] = std::array::from_fn(|_| pack::Packed::new());
    for packet in &mut metadata {
        assert!(encoder.encode_next_split(packet).unwrap().is_some());
    }
    let mut storage = [[&[][..]; pack::PACKED_SLICES]; 3];
    let outgoing: Vec<_> = metadata
        .iter()
        .zip(&mut storage)
        .map(|(packet, slices)| SendDatagram {
            parts: packet.slices(slices),
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
