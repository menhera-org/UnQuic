//! Valid UDP frame fixtures and checksum repair for offload tests.
fn fold(mut sum: u32) -> u16 {
    while sum > 65535 {
        sum = (sum & 65535) + (sum >> 16);
    }
    sum as u16
}
fn sum(bytes: &[u8]) -> u32 {
    bytes
        .chunks(2)
        .map(|p| u16::from_be_bytes([p[0], *p.get(1).unwrap_or(&0)]) as u32)
        .sum()
}
pub fn frame(v6: bool, id: u16, payload: &[u8]) -> Vec<u8> {
    let transport = if v6 { 54 } else { 34 };
    let mut bytes = vec![0; transport + 8];
    bytes[..12].copy_from_slice(&[2, 0, 0, 0, 0, 2, 2, 0, 0, 0, 0, 5]);
    bytes[12..14].copy_from_slice(&(if v6 { 0x86dd_u16 } else { 0x0800 }).to_be_bytes());
    let udp_len = (8 + payload.len()) as u16;
    let pseudo = if v6 {
        bytes[14..18].copy_from_slice(&0x6a123456_u32.to_be_bytes());
        bytes[18..20].copy_from_slice(&udp_len.to_be_bytes());
        bytes[20] = 17;
        bytes[21] = 63;
        bytes[22..38].copy_from_slice(
            &"2001:db8::1"
                .parse::<std::net::Ipv6Addr>()
                .unwrap()
                .octets(),
        );
        bytes[38..54].copy_from_slice(
            &"2001:db8::2"
                .parse::<std::net::Ipv6Addr>()
                .unwrap()
                .octets(),
        );
        sum(&bytes[22..54]) + 17 + u32::from(udp_len)
    } else {
        bytes[14] = 0x45;
        bytes[15] = 0xab;
        bytes[16..18].copy_from_slice(&(udp_len + 20).to_be_bytes());
        bytes[18..20].copy_from_slice(&id.to_be_bytes());
        bytes[20..22].copy_from_slice(&0x4000_u16.to_be_bytes());
        bytes[22] = 63;
        bytes[23] = 17;
        bytes[26..34].copy_from_slice(&[198, 18, 0, 1, 198, 18, 0, 2]);
        let checksum = !fold(sum(&bytes[14..34]));
        bytes[24..26].copy_from_slice(&checksum.to_be_bytes());
        sum(&bytes[26..34]) + 17 + u32::from(udp_len)
    };
    bytes[transport..transport + 2].copy_from_slice(&9000_u16.to_be_bytes());
    bytes[transport + 2..transport + 4].copy_from_slice(&9999_u16.to_be_bytes());
    bytes[transport + 4..transport + 6].copy_from_slice(&udp_len.to_be_bytes());
    bytes.extend_from_slice(payload);
    let c = !fold(pseudo + sum(&bytes[transport..]));
    let checksum = if c == 0 { 65535 } else { c };
    bytes[transport + 6..transport + 8].copy_from_slice(&checksum.to_be_bytes());
    bytes
}

pub fn repair(bytes: &mut [u8], v6: bool) {
    let at = if v6 { 54 } else { 34 };
    if !v6 {
        bytes[24..26].fill(0);
        let check = !fold(sum(&bytes[14..34]));
        bytes[24..26].copy_from_slice(&check.to_be_bytes());
    }
    bytes[at + 6..at + 8].fill(0);
    let pseudo =
        sum(if v6 { &bytes[22..54] } else { &bytes[26..34] }) + 17 + (bytes.len() - at) as u32;
    let check = !fold(pseudo + sum(&bytes[at..]));
    bytes[at + 6..at + 8].copy_from_slice(&(if check == 0 { 65535 } else { check }).to_be_bytes());
}
