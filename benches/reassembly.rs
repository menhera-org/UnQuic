//! Decode and reassemble preencoded frames without socket or TAP costs.
use std::{
    hint::black_box,
    time::{Duration, Instant},
};
use unquic::{
    UnquicUuid,
    reassembly::{Admission, Lane},
    wire::{self, Datagram, Header, Record},
};
fn main() {
    for length in [60, 64, 128, 1514, 9022] {
        let frame = [0x5a; 9022];
        let mut packets = Vec::new();
        for offset in (0..length).step_by(1164) {
            let mut bytes = vec![0; 1232];
            let end = (offset + 1164).min(length);
            let n = wire::encode_datagram(
                Header {
                    cookie: UnquicUuid::from_bytes([1; 16]),
                    receive_tag: 1,
                    lane: 1,
                    record_area_len: 0,
                    packet_number: 1,
                },
                &[Record::Data {
                    frame_id: (1 << 48) | 1,
                    total_len: length as u32,
                    offset: offset as u32,
                    bytes: &frame[offset..end],
                }],
                &mut bytes,
            )
            .unwrap();
            bytes.truncate(n);
            packets.push(bytes);
        }
        let mut lane = Lane::new(1, 9022).unwrap();
        let mut pn = 0_u64;
        let mut sum = 0;
        let start = Instant::now();
        const COUNT: u64 = 1_000_000;
        for seq in 1..=COUNT {
            for bytes in &mut packets {
                pn += 1;
                bytes[40..48].copy_from_slice(&pn.to_be_bytes());
                bytes[52..60].copy_from_slice(&((1_u64 << 48) | seq).to_be_bytes());
                let packet = Datagram::decode(black_box(bytes)).unwrap();
                assert_eq!(lane.admit(&packet, Duration::ZERO), Admission::Fresh);
            }
            if seq % 16 == 0 {
                while let Some(frame) = lane.front(Duration::ZERO) {
                    assert_eq!(frame.bytes.len(), length);
                    sum += black_box(frame.bytes[0]) as u64;
                    lane.finish_front();
                }
            }
        }
        assert_eq!(sum, COUNT * 0x5a);
        let ns = start.elapsed().as_nanos() as f64 / COUNT as f64;
        println!("{length} bytes: {ns:.1} ns/frame, {:.0} frames/s", 1e9 / ns);
    }
}
