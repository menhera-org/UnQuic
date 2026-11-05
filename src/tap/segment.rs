//! Checked UDP L4 super-frame expansion; no wire-format change.
use super::uso::{fold, sum};

#[derive(Clone, Copy, Debug)]
pub(crate) struct Segments {
    network: usize,
    transport: usize,
    size: usize,
    next: usize,
    id: u16,
    v6: bool,
}

fn word(bytes: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([bytes[at], bytes[at + 1]])
}

impl Segments {
    pub(super) fn parse(header: &[u8; 10], bytes: &[u8], limit: usize) -> Option<Self> {
        if header[0] != 1 || header[1] != 5 || bytes.len() > 65535 {
            return None;
        }
        let size = u16::from_ne_bytes([header[4], header[5]]) as usize;
        let transport = u16::from_ne_bytes([header[6], header[7]]) as usize;
        let hint = u16::from_ne_bytes([header[2], header[3]]) as usize;
        if size == 0
            || hint > bytes.len()
            || header[8..10] != 6u16.to_ne_bytes()
            || transport + 8 >= bytes.len()
            || transport + 8 + size > limit
            || bytes.len() < 14
        {
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
            if next != 17 || at != transport {
                return None;
            }
        } else if network + 20 > transport
            || bytes[network] >> 4 != 4
            || bytes[network] & 15 < 5
            || network + usize::from(bytes[network] & 15) * 4 != transport
            || bytes[network + 9] != 17
            || word(bytes, network + 2) as usize != bytes.len() - network
            || word(bytes, network + 6) & !0x4000 != 0
        {
            return None;
        }
        if word(bytes, transport + 4) as usize != bytes.len() - transport {
            return None;
        }
        Some(Self {
            network,
            transport,
            size,
            next: transport + 8,
            id: if v6 { 0 } else { word(bytes, network + 4) },
            v6,
        })
    }

    /// Writes one complete frame. The immutable input stays alive until exhausted.
    pub(crate) fn write(&mut self, input: &[u8], output: &mut [u8]) -> usize {
        let header = self.transport + 8;
        let count = self.size.min(input.len() - self.next);
        let length = header + count;
        output[..header].copy_from_slice(&input[..header]);
        output[header..length].copy_from_slice(&input[self.next..self.next + count]);
        self.next += count;
        let at = self.transport;
        let udp_length = (8 + count) as u16;
        // Adjust the supplied pseudo-header seed, preserving routing-header semantics.
        let seed = fold(
            u32::from(word(input, at + 6))
                + u32::from(!word(input, at + 4))
                + u32::from(udp_length),
        );
        output[at + 4..at + 6].copy_from_slice(&udp_length.to_be_bytes());
        output[at + 6..at + 8].copy_from_slice(&seed.to_be_bytes());
        let checksum = !fold(sum(&output[at..length]));
        output[at + 6..at + 8]
            .copy_from_slice(&if checksum == 0 { 65535u16 } else { checksum }.to_be_bytes());
        let ip = self.network;
        if self.v6 {
            output[ip + 4..ip + 6].copy_from_slice(&((length - ip - 40) as u16).to_be_bytes());
        } else {
            output[ip + 2..ip + 4].copy_from_slice(&((length - ip) as u16).to_be_bytes());
            output[ip + 4..ip + 6].copy_from_slice(&self.id.to_be_bytes());
            self.id = self.id.wrapping_add(1);
            output[ip + 10..ip + 12].fill(0);
            let checksum = !fold(sum(&output[ip..at]));
            output[ip + 10..ip + 12].copy_from_slice(&checksum.to_be_bytes());
        }
        length
    }

    pub(crate) fn remaining(&self, length: usize) -> usize {
        (length - self.next).div_ceil(self.size)
    }

    pub(crate) fn done(&self, length: usize) -> bool {
        self.next == length
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tap::test_common::udp;

    fn packet(v6: bool, count: usize, size: usize, short: bool) -> (Vec<u8>, [u8; 10]) {
        let length = size * count - usize::from(short);
        let bytes: Vec<_> = (0..length).map(|i| (i * 13) as u8).collect();
        let mut bytes = udp::frame(v6, 65534, &bytes);
        let mut header = udp::partial(&mut bytes, v6);
        header[1] = 5;
        header[4..6].copy_from_slice(&(size as u16).to_ne_bytes());
        (bytes, header)
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
                        for (i, data) in input[payload..].chunks(size).enumerate() {
                            let expected = udp::frame(v6, 65534u16.wrapping_add(i as u16), data);
                            let n = segments.write(&input, &mut output);
                            assert_eq!(&output[..n], expected);
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
