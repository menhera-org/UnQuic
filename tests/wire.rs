//! Fixed version-1 interoperability vectors and malformed-input coverage.
use unquic::{
    UnquicUuid,
    wire::{
        AckRange, AckRanges, Datagram, HEADER_LEN, Header, Params, Record, WireError,
        encode_datagram,
    },
};

const I: u128 = 0x202122232425262728292a2b2c2d2e2f;
const R: u128 = 0x303132333435363738393a3b3c3d3e3f;

#[test]
fn probe_prefix_rejects_invalid_lengths_and_identities_before_writing() {
    use unquic::wire::{PROBE_PREFIX_LEN, encode_probe_prefix};
    let header = Header {
        cookie: UnquicUuid::from_bytes([1; 16]),
        receive_tag: R,
        lane: 1,
        record_area_len: 0,
        packet_number: 1,
    };
    let mut prefix = [0xa5; PROBE_PREFIX_LEN];
    for length in [
        0,
        PROBE_PREFIX_LEN - 1,
        HEADER_LEN + u16::MAX as usize + 1,
        usize::MAX,
    ] {
        assert_eq!(
            encode_probe_prefix(header, length, &mut prefix),
            Err(WireError::Length)
        );
        assert_eq!(prefix, [0xa5; PROBE_PREFIX_LEN]);
    }
    for invalid in [
        Header { lane: 0, ..header },
        Header {
            packet_number: 0,
            ..header
        },
        Header {
            receive_tag: 0,
            ..header
        },
    ] {
        assert_eq!(
            encode_probe_prefix(invalid, PROBE_PREFIX_LEN, &mut prefix),
            Err(WireError::Handshake)
        );
        assert_eq!(prefix, [0xa5; PROBE_PREFIX_LEN]);
    }
    encode_probe_prefix(header, PROBE_PREFIX_LEN, &mut prefix).unwrap();
    assert_eq!(Datagram::decode(&prefix).unwrap().records().count(), 2);
}
const FRAME: u64 = 0x1234_0000_0000_0009;
const PARAMS: Params = Params {
    port: 40000,
    max_frame_len: 9022,
    mandatory_features: 0,
    optional_features: 0,
};
const RANGES: [AckRange; 2] = [
    AckRange {
        start: 0x100,
        end: 0x102,
    },
    AckRange { start: 7, end: 9 },
];
const VECTORS: [&str; 11] = [
    include_str!("vectors/wire-v1/padding.hex"),
    include_str!("vectors/wire-v1/data.hex"),
    include_str!("vectors/wire-v1/ack.hex"),
    include_str!("vectors/wire-v1/hello.hex"),
    include_str!("vectors/wire-v1/challenge.hex"),
    include_str!("vectors/wire-v1/probe.hex"),
    include_str!("vectors/wire-v1/probe_reply.hex"),
    include_str!("vectors/wire-v1/frame_abandon.hex"),
    include_str!("vectors/wire-v1/confirm.hex"),
    include_str!("vectors/wire-v1/ready.hex"),
    include_str!("vectors/wire-v1/lane_retire.hex"),
];

fn vector(kind: usize) -> Vec<u8> {
    VECTORS[kind]
        .split_whitespace()
        .map(|part| u8::from_str_radix(part, 16).unwrap())
        .collect()
}

fn record(kind: usize) -> Record<'static> {
    match kind {
        0 => Record::Padding { length: 3 },
        1 => Record::Data {
            frame_id: FRAME,
            total_len: 14,
            offset: 10,
            bytes: &[0xde, 0xad, 0xbe, 0xef],
            offload: false,
        },
        2 => Record::Ack {
            lane: 0x5678,
            delay_us: 1000,
            ranges: AckRanges::new(&RANGES).unwrap(),
        },
        3 => Record::Hello {
            initiator: I,
            params: PARAMS,
        },
        4 => Record::Challenge {
            initiator: I,
            responder: R,
            params: PARAMS,
        },
        5 => Record::Probe,
        6 => Record::ProbeReply {
            lane: 0x5678,
            packet_number: 0x0102030405060708,
            payload_len: 1232,
        },
        7 => Record::FrameAbandon {
            lane: 0x1234,
            origin: 0,
            reason: 2,
            frame_id: FRAME,
        },
        8 => Record::Confirm {
            initiator: I,
            responder: R,
        },
        9 => Record::Ready {
            initiator: I,
            responder: R,
        },
        10 => Record::LaneRetire { lane: 0x5678 },
        _ => unreachable!(),
    }
}

fn header(handshake: bool) -> Header {
    Header {
        cookie: UnquicUuid::from_bytes([0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]),
        receive_tag: if handshake {
            0
        } else {
            0x101112131415161718191a1b1c1d1e1f
        },
        lane: if handshake { 0 } else { 0x1234 },
        record_area_len: 0,
        packet_number: if handshake { 0 } else { 0x0102030405060708 },
    }
}

fn set_area_len(bytes: &mut [u8]) {
    let length = (bytes.len() - HEADER_LEN) as u16;
    bytes[38..40].copy_from_slice(&length.to_be_bytes());
}

fn roundtrip(bytes: &[u8]) {
    let packet = Datagram::decode(bytes).unwrap();
    let records: Vec<_> = packet.records().collect();
    let mut output = vec![0xcc; bytes.len()];
    assert_eq!(
        encode_datagram(packet.header(), &records, &mut output),
        Ok(bytes.len())
    );
    assert_eq!(output, bytes);
}

#[test]
fn handwritten_vectors_cover_every_record_and_common_header() {
    for kind in 0..VECTORS.len() {
        let expected = vector(kind);
        let record = record(kind);
        let header = header(record.is_handshake());
        let mut output = [0xcc; 1024];
        let n = encode_datagram(header, &[record], &mut output).unwrap();
        assert_eq!(&output[..n], expected, "record {kind}");
        let decoded = Header::decode(&expected).unwrap();
        assert_eq!(
            decoded,
            Header {
                record_area_len: (n - HEADER_LEN) as u16,
                ..header
            }
        );
        let mut encoded_header = [0; HEADER_LEN];
        decoded.encode(&mut encoded_header);
        assert_eq!(&encoded_header, &expected[..HEADER_LEN]);
        roundtrip(&expected);

        let n = record.encode(&mut output).unwrap();
        assert_eq!(&output[..n], &expected[HEADER_LEN..]);
        assert_eq!(record.encoded_len(), n);
        assert_eq!(record.ack_eliciting(), matches!(kind, 1 | 5 | 7 | 10));
    }
}

#[test]
fn data_and_ack_borrow_input_and_preserve_semantics() {
    let bytes = vector(1);
    let Record::Data {
        frame_id,
        total_len,
        offset,
        bytes: piece,
        ..
    } = Datagram::decode(&bytes).unwrap().records().next().unwrap()
    else {
        panic!("expected DATA")
    };
    assert_eq!((frame_id, total_len, offset), (FRAME, 14, 10));
    assert_eq!(piece.as_ptr(), bytes[68..].as_ptr());
    assert_eq!(piece, &[0xde, 0xad, 0xbe, 0xef]);

    let bytes = vector(2);
    let Record::Ack {
        lane,
        delay_us,
        ranges,
    } = Datagram::decode(&bytes).unwrap().records().next().unwrap()
    else {
        panic!("expected ACK")
    };
    assert_eq!((lane, delay_us), (0x5678, 1000));
    assert_eq!(ranges.iter().collect::<Vec<_>>(), RANGES);
}

#[test]
fn rejects_bad_header_length_magic_version_and_reserved_flags() {
    let good = vector(5);
    for cut in 0..good.len() {
        assert!(
            Datagram::decode(&good[..cut]).is_err(),
            "truncation at {cut}"
        );
    }
    for (at, value, error) in [
        (0, 0, WireError::Header),
        (1, 0, WireError::Header),
        (2, 0, WireError::Header),
        (2, 2, WireError::Header),
        (3, 1, WireError::Reserved),
        (39, 3, WireError::Length),
        (39, 5, WireError::Length),
    ] {
        let mut bad = good.clone();
        bad[at] = value;
        assert_eq!(Datagram::decode(&bad).unwrap_err(), error);
    }
    let mut trailing = good.clone();
    trailing.push(0);
    assert_eq!(Datagram::decode(&trailing).unwrap_err(), WireError::Length);
    set_area_len(&mut trailing);
    assert_eq!(Datagram::decode(&trailing).unwrap_err(), WireError::Length);
}

#[test]
fn rejects_bad_record_types_lengths_flags_and_reserved_bodies() {
    for kind in 0..VECTORS.len() {
        let good = vector(kind);
        // DATA defines flag 1 (offload); every other value and record reserves it.
        for flags in [1, 2, 3, 0x80] {
            if kind == 1 && flags == 1 {
                continue;
            }
            let mut bad = good.clone();
            bad[49] = flags;
            assert_eq!(Datagram::decode(&bad).unwrap_err(), WireError::Reserved);
        }
        for length in [0u16, 1, 2, 3, u16::MAX] {
            let mut bad = good.clone();
            bad[50..52].copy_from_slice(&length.to_be_bytes());
            assert!(Datagram::decode(&bad).is_err());
        }
        // Keep the common length consistent so record length checks run.
        let mut bad = good.clone();
        bad.pop();
        set_area_len(&mut bad);
        assert!(Datagram::decode(&bad).is_err());
    }
    for kind in [11, 127, 255] {
        let mut bad = vector(5);
        bad[48] = kind;
        assert_eq!(
            Datagram::decode(&bad).unwrap_err(),
            WireError::UnknownRecord
        );
    }
    for (kind, at) in [
        (0, 54),
        (2, 55),
        (6, 54),
        (6, 55),
        (6, 66),
        (6, 67),
        (10, 54),
        (10, 55),
    ] {
        let mut bad = vector(kind);
        bad[at] = 1;
        assert_eq!(Datagram::decode(&bad).unwrap_err(), WireError::Reserved);
    }
    // Fixed-size records cannot gain body bytes, even if both lengths agree.
    for kind in [3, 4, 5, 6, 7, 8, 9, 10] {
        let mut bad = vector(kind);
        bad.push(0);
        let n = (bad.len() - HEADER_LEN) as u16;
        bad[50..52].copy_from_slice(&n.to_be_bytes());
        set_area_len(&mut bad);
        assert_eq!(Datagram::decode(&bad).unwrap_err(), WireError::Length);
    }
}

#[test]
fn record_count_bounds_and_mixed_established_packets() {
    let mut bytes = vector(5);
    bytes.truncate(HEADER_LEN);
    set_area_len(&mut bytes);
    assert_eq!(
        Datagram::decode(&bytes).unwrap_err(),
        WireError::RecordCount
    );
    for count in 1..=65 {
        bytes.extend_from_slice(&[5, 0, 0, 4]);
        set_area_len(&mut bytes);
        if count <= 64 {
            assert_eq!(Datagram::decode(&bytes).unwrap().records().count(), count);
        } else {
            assert_eq!(
                Datagram::decode(&bytes).unwrap_err(),
                WireError::RecordCount
            );
        }
    }
    let records = [
        record(1),
        record(2),
        record(5),
        record(6),
        record(7),
        record(10),
        record(0),
    ];
    let mut buffer = [0; 1024];
    let n = encode_datagram(header(false), &records, &mut buffer).unwrap();
    roundtrip(&buffer[..n]);
    assert_eq!(
        Datagram::decode(&buffer[..n]).unwrap().records().count(),
        records.len()
    );
}

#[test]
fn rejects_data_identity_and_range_errors_without_overflow() {
    for (at, value) in [(60, 13u32), (60, 9023), (64, 11), (64, u32::MAX)] {
        let mut bad = vector(1);
        bad[at..at + 4].copy_from_slice(&value.to_be_bytes());
        assert_eq!(Datagram::decode(&bad).unwrap_err(), WireError::Data);
    }
    for id in [0u64, 9, 0x1234_0000_0000_0000] {
        let mut bad = vector(1);
        bad[52..60].copy_from_slice(&id.to_be_bytes());
        assert_eq!(Datagram::decode(&bad).unwrap_err(), WireError::Data);
    }
    let mut wrong_lane = vector(1);
    wrong_lane[52] ^= 1;
    assert_eq!(Datagram::decode(&wrong_lane).unwrap_err(), WireError::Lane);
    let mut empty_piece = vector(1);
    empty_piece.truncate(68);
    empty_piece[51] = 20;
    set_area_len(&mut empty_piece);
    assert!(Datagram::decode(&empty_piece).is_err());

    for length in [14usize, 1514, 9022] {
        let payload = vec![0x5a; length];
        let record = Record::Data {
            frame_id: FRAME,
            total_len: length as u32,
            offset: 0,
            bytes: &payload,
            offload: false,
        };
        let mut output = vec![0; HEADER_LEN + record.encoded_len()];
        encode_datagram(header(false), &[record], &mut output).unwrap();
        roundtrip(&output);
    }
}

#[test]
fn offload_flag_round_trips_and_moves_the_frame_length_bounds() {
    let payload = [0x5a; 100];
    let data = |total_len: u32, length: usize, offload: bool| Record::Data {
        frame_id: FRAME,
        total_len,
        offset: 0,
        bytes: &payload[..length],
        offload,
    };
    for (total_len, length) in [(24, 24), (9023, 100), (65_535, 100)] {
        let mut output = vec![0; HEADER_LEN + data(total_len, length, true).encoded_len()];
        encode_datagram(header(false), &[data(total_len, length, true)], &mut output).unwrap();
        assert_eq!(output[49], 1);
        roundtrip(&output);
        let Some(Record::Data { offload, .. }) =
            Datagram::decode(&output).unwrap().records().next()
        else {
            panic!("expected DATA")
        };
        assert!(offload);
        // The same bytes without the flag are an ordinary frame, whose ceiling is lower.
        output[49] = 0;
        assert_eq!(Datagram::decode(&output).is_ok(), total_len <= 9022);
    }
    // An offload frame must hold an Ethernet header after its descriptor.
    let mut output = [0; 1024];
    assert_eq!(
        encode_datagram(header(false), &[data(23, 23, true)], &mut output),
        Err(WireError::Data)
    );
    let mut prefix = [0; unquic::wire::DATA_PREFIX_LEN];
    assert_eq!(
        unquic::wire::encode_data_prefix(header(false), data(23, 23, true), &mut prefix),
        Err(WireError::Data)
    );
    unquic::wire::encode_data_prefix(header(false), data(65_535, 100, true), &mut prefix).unwrap();
    assert_eq!(prefix[HEADER_LEN + 1], 1);
}

#[test]
fn ack_range_rules_and_huge_numeric_spans() {
    for ranges in [
        vec![],
        vec![AckRange { start: 0, end: 1 }],
        vec![AckRange { start: 2, end: 1 }],
        vec![AckRange { start: 2, end: 3 }, AckRange { start: 4, end: 5 }],
        vec![AckRange { start: 3, end: 5 }, AckRange { start: 1, end: 3 }],
        vec![AckRange { start: 3, end: 5 }, AckRange { start: 1, end: 2 }],
        vec![AckRange { start: 1, end: 1 }, AckRange { start: 1, end: 1 }],
    ] {
        assert!(AckRanges::new(&ranges).is_err());
    }
    let thirty_two: Vec<_> = (1..=32)
        .rev()
        .map(|n| AckRange {
            start: n * 2,
            end: n * 2,
        })
        .collect();
    assert_eq!(AckRanges::new(&thirty_two).unwrap().len(), 32);
    let mut thirty_three = thirty_two.clone();
    thirty_three.insert(0, AckRange { start: 66, end: 66 });
    assert!(AckRanges::new(&thirty_three).is_err());
    let huge = [AckRange {
        start: 1,
        end: u64::MAX,
    }];
    let ranges = AckRanges::new(&huge).unwrap();
    assert_eq!(ranges.iter().count(), 1);
    let mut output = [0; 128];
    let n = encode_datagram(
        header(false),
        &[Record::Ack {
            lane: 1,
            delay_us: 0,
            ranges,
        }],
        &mut output,
    )
    .unwrap();
    roundtrip(&output[..n]);

    for (at, value) in [(52, 0u16), (54, 0), (54, 33 << 8)] {
        let mut bad = vector(2);
        bad[at..at + 2].copy_from_slice(&value.to_be_bytes());
        assert!(Datagram::decode(&bad).is_err());
    }
    let mut bad = vector(2);
    bad[56..60].copy_from_slice(&1001u32.to_be_bytes());
    assert_eq!(Datagram::decode(&bad).unwrap_err(), WireError::Ack);
    for (at, value) in [(60, 0u64), (60, 0x103), (84, 0x100), (84, 0xff)] {
        let mut bad = vector(2);
        bad[at..at + 8].copy_from_slice(&value.to_be_bytes());
        assert_eq!(Datagram::decode(&bad).unwrap_err(), WireError::Ack);
    }
    let mut wrong_count = vector(2);
    wrong_count[54] = 1;
    assert_eq!(Datagram::decode(&wrong_count).unwrap_err(), WireError::Ack);
}

#[test]
fn handshake_envelopes_tags_and_parameter_constraints() {
    for kind in [3, 4, 8, 9] {
        for at in [35, 37, 47] {
            let mut bad = vector(kind);
            bad[at] = 1;
            assert_eq!(Datagram::decode(&bad).unwrap_err(), WireError::Handshake);
        }
        let mut mixed = vector(kind);
        mixed.extend_from_slice(&[0, 0, 0, 4]);
        set_area_len(&mut mixed);
        assert_eq!(Datagram::decode(&mixed).unwrap_err(), WireError::Handshake);
        let mut zero_i = vector(kind);
        zero_i[52..68].fill(0);
        assert_eq!(Datagram::decode(&zero_i).unwrap_err(), WireError::Handshake);
        if kind != 3 {
            for tag in [0u128, I] {
                let mut bad = vector(kind);
                bad[68..84].copy_from_slice(&tag.to_be_bytes());
                assert_eq!(Datagram::decode(&bad).unwrap_err(), WireError::Handshake);
            }
        }
    }
    for kind in [0, 1, 2, 5, 6, 7, 10] {
        for range in [20..36, 36..38, 40..48] {
            let mut bad = vector(kind);
            bad[range].fill(0);
            assert_eq!(Datagram::decode(&bad).unwrap_err(), WireError::Handshake);
        }
    }
    for (kind, params_at) in [(3, 68), (4, 84)] {
        for (at, value) in [
            (params_at, 0u16),
            (params_at + 2, 1513),
            (params_at + 2, 9023),
            (params_at + 4, 1),
        ] {
            let mut bad = vector(kind);
            bad[at..at + 2].copy_from_slice(&value.to_be_bytes());
            assert_eq!(Datagram::decode(&bad).unwrap_err(), WireError::Parameters);
        }
        for length in [1514u16, 9022] {
            let mut good = vector(kind);
            good[params_at + 2..params_at + 4].copy_from_slice(&length.to_be_bytes());
            // Unknown optional bits are preserved, not grounds for rejection.
            good[params_at + 8..params_at + 12].fill(0xff);
            roundtrip(&good);
        }
    }
}

#[test]
fn probe_abandonment_and_retirement_constraints() {
    for range in [52..54, 56..64, 64..66] {
        let mut bad = vector(6);
        bad[range].fill(0);
        assert_eq!(Datagram::decode(&bad).unwrap_err(), WireError::Probe);
    }
    for (at, value) in [(54, 2), (55, 0), (55, 5), (56, 0)] {
        let mut bad = vector(7);
        bad[at] = value;
        assert_eq!(Datagram::decode(&bad).unwrap_err(), WireError::Abandon);
    }
    let mut zero_sequence = vector(7);
    zero_sequence[58..64].fill(0);
    assert_eq!(
        Datagram::decode(&zero_sequence).unwrap_err(),
        WireError::Abandon
    );
    let mut wrong_carrying_lane = vector(7);
    wrong_carrying_lane[36..38].copy_from_slice(&0x5678u16.to_be_bytes());
    assert_eq!(
        Datagram::decode(&wrong_carrying_lane).unwrap_err(),
        WireError::Lane
    );
    wrong_carrying_lane[54] = 1;
    roundtrip(&wrong_carrying_lane);
    for lane in [0u16, 0x1234] {
        let mut bad = vector(10);
        bad[52..54].copy_from_slice(&lane.to_be_bytes());
        assert_eq!(Datagram::decode(&bad).unwrap_err(), WireError::Lane);
    }
}

#[test]
fn encoder_errors_never_modify_output() {
    let mut bytes = [0xcc; 64];
    for records in [
        vec![],
        vec![Record::Probe; 65],
        vec![Record::Data {
            frame_id: FRAME,
            total_len: 14,
            offset: 0,
            bytes: &[],
            offload: false,
        }],
        vec![Record::Padding { length: u16::MAX }],
        vec![Record::Padding { length: 40000 }; 2],
        vec![record(3), record(0)],
        vec![Record::LaneRetire { lane: 0x1234 }],
        vec![Record::ProbeReply {
            lane: 0,
            packet_number: 1,
            payload_len: 1232,
        }],
    ] {
        assert!(encode_datagram(header(false), &records, &mut bytes).is_err());
        assert_eq!(bytes, [0xcc; 64]);
    }
    for kind in 0..VECTORS.len() {
        let record = record(kind);
        let mut short = vec![0xcc; record.encoded_len() - 1];
        assert_eq!(record.encode(&mut short), Err(WireError::BufferTooSmall));
        assert!(short.iter().all(|&b| b == 0xcc));
        let mut short = vec![0xcc; HEADER_LEN + record.encoded_len() - 1];
        assert_eq!(
            encode_datagram(header(record.is_handshake()), &[record], &mut short),
            Err(WireError::BufferTooSmall)
        );
        assert!(short.iter().all(|&b| b == 0xcc));
    }
}

#[test]
fn every_vector_truncation_and_single_bit_mutation_is_safe_and_lossless_if_valid() {
    for kind in 0..VECTORS.len() {
        let good = vector(kind);
        for cut in 0..good.len() {
            assert!(Datagram::decode(&good[..cut]).is_err());
        }
        for at in 0..good.len() {
            for bit in 0..8 {
                let mut mutated = good.clone();
                mutated[at] ^= 1 << bit;
                if Datagram::decode(&mutated).is_ok() {
                    roundtrip(&mutated);
                }
            }
        }
    }
}
