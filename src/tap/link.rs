//! One rtnetlink query: what kind of device, if any, holds a TAP name.
use crate::sys::cvt;
use std::{
    ffi::CStr,
    io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
};

const RTM_NEWLINK: u16 = 16;
const RTM_GETLINK: u16 = 18;
const NLMSG_ERROR: u16 = 2;
const NLM_F_REQUEST: u16 = 1;
const IFLA_IFNAME: u16 = 3;
const IFLA_LINKINFO: u16 = 18;
const IFLA_INFO_KIND: u16 = 1;
const IFLA_INFO_DATA: u16 = 2;
// linux/if_link.h: IFLA_TUN_*.
const IFLA_TUN_TYPE: u16 = 3;
const IFLA_TUN_PI: u16 = 4;
const IFLA_TUN_PERSIST: u16 = 6;
const IFLA_TUN_MULTI_QUEUE: u16 = 7;
const IFLA_TUN_NUM_QUEUES: u16 = 8;
const HEADER: usize = 16;
const IFINFO: usize = 16;

/// A tun/tap device's kernel state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Tun {
    pub tap: bool,
    pub pi: bool,
    pub persist: bool,
    pub multi_queue: bool,
    /// Queues currently attached by some process.
    pub queues: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Link {
    Missing,
    /// A device that is not tun/tap.
    Other,
    Tun(Tun),
}

fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    bytes
        .get(at..at + 2)?
        .try_into()
        .ok()
        .map(u16::from_ne_bytes)
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    bytes
        .get(at..at + 4)?
        .try_into()
        .ok()
        .map(u32::from_ne_bytes)
}

/// Netlink attributes as (type without flag bits, payload).
fn attributes(mut bytes: &[u8]) -> impl Iterator<Item = (u16, &[u8])> {
    std::iter::from_fn(move || {
        let length = usize::from(u16_at(bytes, 0)?);
        let kind = u16_at(bytes, 2)? & 0x3fff;
        let payload = bytes.get(4..length)?;
        bytes = bytes.get(length.next_multiple_of(4)..).unwrap_or_default();
        Some((kind, payload))
    })
}

/// Parses one RTM_GETLINK reply.
fn parse(reply: &[u8]) -> io::Result<Link> {
    let malformed = || io::Error::new(io::ErrorKind::InvalidData, "malformed rtnetlink reply");
    let length = u32_at(reply, 0).ok_or_else(malformed)? as usize;
    let message = reply.get(..length).ok_or_else(malformed)?;
    match u16_at(message, 4).ok_or_else(malformed)? {
        NLMSG_ERROR => {
            let error = u32_at(message, HEADER).ok_or_else(malformed)? as i32;
            if error == -libc::ENODEV {
                Ok(Link::Missing)
            } else {
                Err(io::Error::from_raw_os_error(error.saturating_neg()))
            }
        }
        RTM_NEWLINK => {
            let info = attributes(message.get(HEADER + IFINFO..).ok_or_else(malformed)?)
                .find(|&(kind, _)| kind == IFLA_LINKINFO);
            let Some((_, info)) = info else {
                return Ok(Link::Other);
            };
            if !attributes(info).any(|(kind, value)| kind == IFLA_INFO_KIND && value == b"tun\0") {
                return Ok(Link::Other);
            }
            let mut tun = Tun::default();
            for (_, data) in attributes(info).filter(|&(kind, _)| kind == IFLA_INFO_DATA) {
                for (kind, value) in attributes(data) {
                    let flag = value.first().is_some_and(|&byte| byte != 0);
                    match kind {
                        IFLA_TUN_TYPE => tun.tap = value.first() == Some(&(libc::IFF_TAP as u8)),
                        IFLA_TUN_PI => tun.pi = flag,
                        IFLA_TUN_PERSIST => tun.persist = flag,
                        IFLA_TUN_MULTI_QUEUE => tun.multi_queue = flag,
                        IFLA_TUN_NUM_QUEUES => tun.queues = u32_at(value, 0).unwrap_or(u32::MAX),
                        _ => {}
                    }
                }
            }
            Ok(Link::Tun(tun))
        }
        _ => Err(malformed()),
    }
}

/// Asks the kernel about the device named `name` in this network namespace.
pub(crate) fn query(name: &CStr) -> io::Result<Link> {
    // SAFETY: plain socket creation; the result is owned below.
    let fd = cvt(unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_RAW | libc::SOCK_CLOEXEC,
            libc::NETLINK_ROUTE,
        )
    })?;
    // SAFETY: socket just returned this descriptor.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let name = name.to_bytes_with_nul();
    let attribute = 4 + name.len();
    let total = HEADER + IFINFO + attribute.next_multiple_of(4);
    let mut request = vec![0_u8; total];
    request[0..4].copy_from_slice(&(total as u32).to_ne_bytes());
    request[4..6].copy_from_slice(&RTM_GETLINK.to_ne_bytes());
    request[6..8].copy_from_slice(&NLM_F_REQUEST.to_ne_bytes());
    request[HEADER] = libc::AF_UNSPEC as u8;
    let at = HEADER + IFINFO;
    request[at..at + 2].copy_from_slice(&(attribute as u16).to_ne_bytes());
    request[at + 2..at + 4].copy_from_slice(&IFLA_IFNAME.to_ne_bytes());
    request[at + 4..at + 4 + name.len()].copy_from_slice(name);
    // SAFETY: the zeroed sockaddr_nl addresses the kernel; request is readable.
    let sent = unsafe {
        let mut kernel: libc::sockaddr_nl = std::mem::zeroed();
        kernel.nl_family = libc::AF_NETLINK as _;
        libc::sendto(
            fd.as_raw_fd(),
            request.as_ptr().cast(),
            request.len(),
            0,
            (&kernel as *const libc::sockaddr_nl).cast(),
            size_of::<libc::sockaddr_nl>() as _,
        )
    };
    if sent < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut reply = vec![0_u8; 65536];
    loop {
        // SAFETY: reply is writable for its length.
        let n = unsafe { libc::recv(fd.as_raw_fd(), reply.as_mut_ptr().cast(), reply.len(), 0) };
        if n >= 0 {
            return parse(reply.get(..n as usize).unwrap_or_default());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attribute(kind: u16, payload: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&(4 + payload.len() as u16).to_ne_bytes());
        bytes.extend_from_slice(&kind.to_ne_bytes());
        bytes.extend_from_slice(payload);
        bytes.resize(bytes.len().next_multiple_of(4), 0);
        bytes
    }

    fn message(kind: u16, body: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0; HEADER];
        bytes[0..4].copy_from_slice(&((HEADER + body.len()) as u32).to_ne_bytes());
        bytes[4..6].copy_from_slice(&kind.to_ne_bytes());
        bytes.extend_from_slice(body);
        bytes
    }

    fn tun_reply() -> Vec<u8> {
        let mut data = Vec::new();
        data.extend(attribute(IFLA_TUN_TYPE, &[libc::IFF_TAP as u8]));
        data.extend(attribute(IFLA_TUN_PI, &[0]));
        data.extend(attribute(IFLA_TUN_PERSIST, &[1]));
        data.extend(attribute(IFLA_TUN_MULTI_QUEUE, &[1]));
        data.extend(attribute(IFLA_TUN_NUM_QUEUES, &2_u32.to_ne_bytes()));
        let mut info = attribute(IFLA_INFO_KIND, b"tun\0");
        // The kernel marks nested attributes with NLA_F_NESTED.
        info.extend(attribute(IFLA_INFO_DATA | 0x8000, &data));
        let mut body = vec![0; IFINFO];
        body.extend(attribute(IFLA_IFNAME, b"uq-test\0"));
        body.extend(attribute(IFLA_LINKINFO | 0x8000, &info));
        message(RTM_NEWLINK, &body)
    }

    fn bridge_reply() -> Vec<u8> {
        let mut bridge = vec![0; IFINFO];
        bridge.extend(attribute(
            IFLA_LINKINFO,
            &attribute(IFLA_INFO_KIND, b"bridge\0"),
        ));
        message(RTM_NEWLINK, &bridge)
    }

    #[test]
    fn random_and_mutated_replies_never_panic_parsing() {
        use crate::tap::test_common::fuzz::{INPUTS, Rng};
        let seeds = [
            message(NLMSG_ERROR, &(-libc::ENODEV).to_ne_bytes()),
            tun_reply(),
            bridge_reply(),
        ];
        let mut rng = Rng::new(0x11ca_0001);
        let mut reply = Vec::new();
        for _ in 0..INPUTS {
            rng.input(&seeds, &mut reply);
            if rng.below(2) == 0 && reply.len() >= 4 {
                let length = reply.len() as u32;
                reply[..4].copy_from_slice(&length.to_ne_bytes());
            }
            let _ = parse(&reply);
        }
    }

    #[test]
    fn replies_name_a_missing_other_or_tun_device() {
        let error = message(NLMSG_ERROR, &(-libc::ENODEV).to_ne_bytes());
        assert_eq!(parse(&error).unwrap(), Link::Missing);
        let denied = message(NLMSG_ERROR, &(-libc::EPERM).to_ne_bytes());
        assert_eq!(
            parse(&denied).unwrap_err().raw_os_error(),
            Some(libc::EPERM)
        );
        let expected = Tun {
            tap: true,
            pi: false,
            persist: true,
            multi_queue: true,
            queues: 2,
        };
        assert_eq!(parse(&tun_reply()).unwrap(), Link::Tun(expected));
        assert_eq!(parse(&bridge_reply()).unwrap(), Link::Other);
        assert!(parse(&[1, 2, 3]).is_err());
        assert!(parse(&message(99, &[])).is_err());
    }
}
