//! Byte-exact kernel USO checks in a disposable network namespace.
use super::TapQueue;
use super::test_common as common;
use common::*;
use std::{
    net::UdpSocket,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    process::Command,
};

fn ip(args: &[&str]) {
    assert!(Command::new("ip").args(args).status().unwrap().success());
}
use common::udp::{frame, repair};

fn disable_offloads(socket: &UdpSocket, name: &std::ffi::CStr) {
    for (set, get) in [(0x17_u32, 0x16), (0x24, 0x23), (0x2c, 0x2b)] {
        let mut value = [set, 0];
        let mut req = request(name);
        req.ifr_ifru.ifru_data = value.as_mut_ptr().cast();
        // SAFETY: ifreq points to the exact two-u32 ethtool_value ABI structure.
        check(unsafe { libc::ioctl(socket.as_raw_fd(), 0x8946 as _, &mut req) });
        value[0] = get;
        // SAFETY: initialized output argument remains alive and writable.
        check(unsafe { libc::ioctl(socket.as_raw_fd(), 0x8946 as _, &mut req) });
        assert_eq!(value[1], 0, "offload {get:#x} remains enabled");
    }
}
fn fixture(bytes: &[u8]) -> bool {
    bytes.get(6..11) == Some(&[2, 0, 0, 0, 0])
}
fn receive(capture: &OwnedFd) -> Vec<u8> {
    let mut bytes = vec![0; 65536];
    let mut length = 0;
    wait_for(|| {
        let Some(n) = raw_read(capture, &mut bytes) else {
            return false;
        };
        // Ignore bridge multicast reports, which use a different source MAC.
        length = n;
        fixture(&bytes[..n])
    });
    bytes.truncate(length);
    bytes
}

#[test]
#[ignore = "requires disposable container, tun, iproute2 and CAP_NET_ADMIN/CAP_NET_RAW"]
fn uso_writes_reproduce_single_frame_bytes_across_a_veth() {
    // SAFETY: only this test thread and its children enter the private namespace.
    check(unsafe { libc::unshare(libc::CLONE_NEWNET) });
    let control = UdpSocket::bind("[::]:0").unwrap();
    // SAFETY: constant pathname; successful open transfers one new descriptor.
    let tap = unsafe {
        OwnedFd::from_raw_fd(check(libc::open(
            c"/dev/net/tun".as_ptr(),
            libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )))
    };
    let mut req = request(c"uq-uso");
    req.ifr_ifru.ifru_flags =
        (libc::IFF_TAP | libc::IFF_NO_PI | libc::IFF_VNET_HDR | libc::IFF_TUN_EXCL) as _;
    // SAFETY: initialized ifreq, integer offload mask and ten-byte header size.
    unsafe {
        check(libc::ioctl(tap.as_raw_fd(), libc::TUNSETIFF, &mut req));
        check(libc::ioctl(
            tap.as_raw_fd(),
            libc::TUNSETOFFLOAD,
            0 as libc::c_ulong,
        ));
        check(libc::ioctl(tap.as_raw_fd(), libc::TUNSETVNETHDRSZ, &10i32));
    }
    ip(&["link", "add", "uq-br", "type", "bridge"]);
    ip(&[
        "link", "add", "uq-v0", "type", "veth", "peer", "name", "uq-v1",
    ]);
    ip(&["link", "set", "uq-v1", "address", "02:00:00:00:00:02"]);
    for name in ["uq-uso", "uq-v0"] {
        ip(&["link", "set", name, "master", "uq-br"]);
    }
    for name in ["uq-uso", "uq-v0", "uq-v1", "uq-br"] {
        ip(&["link", "set", name, "addrgenmode", "none"]);
        ip(&["link", "set", name, "up"]);
    }
    disable_offloads(&control, c"uq-v0");
    disable_offloads(&control, c"uq-v1");
    let mut tap = TapQueue {
        fd: tap,
        frame_limit: crate::MAX_FRAME_LEN,
        vnet: true,
        uso: true,
    };
    let capture = raw_socket(c"uq-v1", libc::ETH_P_ALL as u16);
    for v6 in [false, true] {
        let size = if v6 { 1432 } else { 1472 };
        for count in [1, 2, 43, 44] {
            for short in [false, true] {
                let payloads: Vec<_> = (0..count)
                    .map(|i| {
                        let len = if short && i + 1 == count { 1 } else { size };
                        (0..len).map(|b| (i * 13 + b * 7) as u8).collect::<Vec<_>>()
                    })
                    .collect();
                let expected: Vec<_> = payloads
                    .iter()
                    .enumerate()
                    .map(|(i, p)| frame(v6, 65520u16.wrapping_add(i as u16), p))
                    .collect();
                for p in &expected {
                    tap.write_frame(p).unwrap();
                    assert_eq!(receive(&capture), *p, "single frame changed");
                }
                let frames: Vec<_> = expected.iter().map(Vec::as_slice).collect();
                let (mask, result) = tap.write_batch(frames.iter().copied());
                let expected_mask = (1_u64 << count) - 1;
                assert_eq!(mask, expected_mask);
                assert_eq!(
                    result.unwrap(),
                    expected.iter().map(Vec::len).sum::<usize>()
                );
                for (i, p) in expected.iter().enumerate() {
                    let got = receive(&capture);
                    assert_eq!(
                        got.len(),
                        p.len(),
                        "v6={v6} count={count} short={short} segment={i}"
                    );
                    if got != *p {
                        let changes: Vec<_> = got
                            .iter()
                            .zip(p)
                            .enumerate()
                            .filter(|(_, (a, b))| a != b)
                            .map(|(n, (a, b))| (n, *a, *b))
                            .collect();
                        panic!(
                            "v6={v6} count={count} short={short} segment={i} changed bytes {changes:?}"
                        );
                    }
                }
                println!("v6={v6} count={count} short={short} byte_exact=true");
            }
        }
    }

    // Reorder only across flows; a changed header within a flow is a barrier.
    for v6 in [false, true] {
        let at = if v6 { 54 } else { 34 };
        let a = frame(v6, 100, &[1; 120]);
        let b = frame(v6, 101, &[2; 120]);
        let mut other = frame(v6, 900, &[3; 120]);
        other[at..at + 2].copy_from_slice(&9001u16.to_be_bytes());
        repair(&mut other, v6);
        let (mask, result) =
            tap.write_batch([a.as_slice(), other.as_slice(), b.as_slice()].into_iter());
        assert_eq!(mask, 0b101);
        assert_eq!(result.unwrap(), a.len() + b.len());
        assert_eq!(receive(&capture), a);
        assert_eq!(receive(&capture), b);
        tap.write_frame(&other).unwrap();
        assert_eq!(receive(&capture), other);
        let mut barrier = b.clone();
        barrier[if v6 { 21 } else { 22 }] ^= 1;
        repair(&mut barrier, v6);
        let (mask, result) =
            tap.write_batch([a.as_slice(), barrier.as_slice(), b.as_slice()].into_iter());
        assert_eq!(mask, 1);
        assert_eq!(result.unwrap(), a.len());
        assert_eq!(receive(&capture), a);

        let mut zero_payload = [0u8; 120];
        let base = frame(v6, 1, &zero_payload);
        zero_payload[118..].copy_from_slice(&base[at + 6..at + 8]);
        let zero = [frame(v6, 1, &zero_payload), frame(v6, 2, &zero_payload)];
        assert_eq!(&zero[0][at + 6..at + 8], &[255, 255]);
        let (mask, result) = tap.write_batch(zero.iter().map(Vec::as_slice));
        assert_eq!(mask, 3);
        assert_eq!(result.unwrap(), zero.iter().map(Vec::len).sum::<usize>());
        for p in &zero {
            assert_eq!(receive(&capture), *p);
        }
        let empty = frame(v6, 1, &[]);
        let (mask, result) = tap.write_batch([empty.as_slice(), empty.as_slice()].into_iter());
        assert_eq!(mask, 1);
        assert_eq!(result.unwrap(), empty.len());
        assert_eq!(receive(&capture), empty);

        if v6 {
            let mut extension = b.clone();
            extension.splice(54..54, [17, 0, 0, 0, 0, 0, 0, 0]);
            extension[20] = 0;
            let length = (extension.len() - 54) as u16;
            extension[18..20].copy_from_slice(&length.to_be_bytes());
            assert_eq!(
                crate::pack::flow_hash(&a),
                crate::pack::flow_hash(&extension)
            );
            let (mask, result) =
                tap.write_batch([a.as_slice(), extension.as_slice(), b.as_slice()].into_iter());
            assert_eq!(mask, 1, "extension header must remain a same-flow barrier");
            assert_eq!(result.unwrap(), a.len());
            assert_eq!(receive(&capture), a);
        }
        let size = if v6 { 1432 } else { 1472 };
        let large: Vec<_> = (0..64u16).map(|id| frame(v6, id, &vec![1; size])).collect();
        let capacity = (65535 - at - 8) / size;
        let (mask, result) = tap.write_batch(large.iter().map(Vec::as_slice));
        assert_eq!(mask, (1u64 << capacity) - 1);
        assert_eq!(result.unwrap(), capacity * large[0].len());
        for p in &large[..capacity] {
            assert_eq!(receive(&capture), *p);
        }

        let small: Vec<_> = (0..64u16).map(|id| frame(v6, id, &[1; 100])).collect();
        let (mask, result) = tap.write_batch(small.iter().map(Vec::as_slice));
        assert_eq!(mask, u64::MAX);
        assert_eq!(result.unwrap(), small.iter().map(Vec::len).sum::<usize>());
        for p in &small {
            assert_eq!(receive(&capture), *p);
        }
        let offsets = if v6 {
            vec![
                5,
                11,
                12,
                14,
                15,
                17,
                18,
                20,
                21,
                22,
                38,
                at,
                at + 2,
                at + 4,
                at + 6,
            ]
        } else {
            vec![
                5,
                11,
                12,
                14,
                15,
                16,
                18,
                20,
                21,
                22,
                23,
                24,
                26,
                30,
                at,
                at + 2,
                at + 4,
                at + 6,
            ]
        };
        for offset in offsets {
            let mut changed = b.clone();
            changed[offset] ^= 1;
            repair(&mut changed, v6);
            if offset == at + 6 {
                changed[offset] ^= 1;
            }
            if !v6 && offset == 24 {
                changed[offset] ^= 1;
            }
            let (mask, result) = tap.write_batch([a.as_slice(), changed.as_slice()].into_iter());
            assert_eq!(mask, 1, "v6={v6} offset={offset} unexpectedly coalesced");
            assert_eq!(result.unwrap(), a.len());
            assert_eq!(receive(&capture), a);
            tap.write_frame(&changed).unwrap();
            assert_eq!(receive(&capture), changed);
        }
    }
    let mut extra = [0; 65536];
    while let Some(n) = raw_read(&capture, &mut extra) {
        assert!(!fixture(&extra[..n]), "unexpected duplicate fixture frame");
    }
}

#[test]
#[ignore = "kernel descriptor test; run in a disposable container"]
fn batching_preserves_backpressure_and_disables_rejected_offloads() {
    use std::io;
    use std::os::unix::net::UnixDatagram;
    let (sender, receiver) = UnixDatagram::pair().unwrap();
    sender.set_nonblocking(true).unwrap();
    receiver.set_nonblocking(true).unwrap();
    while sender.send(&[7]).is_ok() {}
    let mut tap = TapQueue {
        fd: sender.into(),
        frame_limit: crate::MAX_FRAME_LEN,
        vnet: true,
        uso: true,
    };
    let a = frame(false, 1, &[1; 120]);
    let b = frame(false, 2, &[2; 120]);
    let (mask, result) = tap.write_batch([a.as_slice(), b.as_slice()].into_iter());
    assert_eq!(mask, 3);
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::WouldBlock);
    assert!(tap.uso);
    let mut bytes = [0; 1024];
    while let Ok(n) = receiver.recv(&mut bytes) {
        assert_eq!(&bytes[..n], &[7]);
    }
    let (mask, result) = tap.write_batch([a.as_slice(), b.as_slice()].into_iter());
    assert_eq!(mask, 3);
    assert_eq!(result.unwrap(), a.len() + b.len());
    assert_eq!(receiver.recv(&mut bytes).unwrap(), 10 + 42 + 240);
    assert_eq!(bytes[1], 5);

    // eventfd consumes only eight bytes: a short write must name the entire attempted batch.
    let fd = check(unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) });
    // SAFETY: sole owner of the new descriptor.
    let mut short = TapQueue {
        fd: unsafe { OwnedFd::from_raw_fd(fd) },
        frame_limit: crate::MAX_FRAME_LEN,
        vnet: true,
        uso: true,
    };
    let (mask, result) = short.write_batch([a.as_slice(), b.as_slice()].into_iter());
    assert_eq!(mask, 3);
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::WriteZero);

    // An epoll descriptor rejects writes with EINVAL, exercising capability fallback.
    let fd = check(unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) });
    // SAFETY: sole owner of the new descriptor.
    let mut rejected = TapQueue {
        fd: unsafe { OwnedFd::from_raw_fd(fd) },
        frame_limit: crate::MAX_FRAME_LEN,
        vnet: true,
        uso: true,
    };
    let (mask, result) = rejected.write_batch([a.as_slice(), b.as_slice()].into_iter());
    assert_eq!(mask, 1);
    assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::EINVAL));
    assert!(!rejected.uso);
    let (mask, _) = rejected.write_batch([a.as_slice(), b.as_slice()].into_iter());
    assert_eq!(mask, 1);
}

#[test]
#[ignore = "kernel descriptor test; run in a disposable container"]
fn tap_metadata_is_removed_and_unsupported_partial_frames_are_rejected() {
    use std::{io, os::unix::net::UnixDatagram};
    let (queue, input) = UnixDatagram::pair().unwrap();
    queue.set_nonblocking(true).unwrap();
    let mut tap = TapQueue {
        fd: queue.into(),
        frame_limit: crate::MAX_FRAME_LEN,
        vnet: true,
        uso: true,
    };
    let mut bytes = [0u8; crate::MAX_FRAME_LEN + 1];
    for flags in [0, 2, 1, 4, 128] {
        let mut frame = [7u8; 70];
        frame[..10].fill(0);
        frame[0] = flags;
        input.send(&frame).unwrap();
        let read = tap.read_frame(&mut bytes);
        if flags & !2 != 0 {
            assert_eq!(read.unwrap_err().kind(), io::ErrorKind::InvalidData);
        } else {
            assert_eq!(read.unwrap(), 60);
            assert_eq!(&bytes[..60], &[7; 60]);
        }
    }
    let mut offloaded = [0u8; 70];
    offloaded[1] = 5;
    input.send(&offloaded).unwrap();
    assert_eq!(
        tap.read_frame(&mut bytes).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
    input.send(&[0; 9]).unwrap();
    assert_eq!(
        tap.read_frame(&mut bytes).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
    input.send(&vec![0; crate::MAX_FRAME_LEN + 11]).unwrap();
    assert_eq!(
        tap.read_frame(&mut bytes).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
}
