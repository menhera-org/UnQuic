//! Host facts for the status report (design section 12): the CPU, the kernel's
//! socket-buffer ceilings, and each underlay device's driver, queues and
//! offloads. All of it is best effort: a missing file or a refused ioctl
//! prints `unknown`, never an error.
use std::{
    ffi::CStr,
    fs,
    net::{Ipv6Addr, SocketAddr, SocketAddrV6, UdpSocket},
    os::fd::AsRawFd,
};

const SIOCETHTOOL: libc::c_ulong = 0x8946;
const ETHTOOL_GDRVINFO: u32 = 0x03;
const ETHTOOL_GSTRINGS: u32 = 0x1b;
const ETHTOOL_GRXFH: u32 = 0x29;
const UDP_V6_FLOW: u32 = 6;
const ETHTOOL_GSSET_INFO: u32 = 0x37;
const ETHTOOL_GFEATURES: u32 = 0x3a;
const ETHTOOL_GCHANNELS: u32 = 0x3c;
const ETH_SS_FEATURES: u32 = 4;
const STRING_LEN: usize = 32;
/// The offloads README.md's operational checks name, as the kernel spells them.
const FEATURES: [&str; 7] = [
    "rx-checksum",
    "tx-checksum-ip-generic",
    "tx-checksum-ipv6",
    "tx-generic-segmentation",
    "tx-udp-segmentation",
    "rx-gro",
    "rx-udp-gro-forwarding",
];

fn word(bytes: &[u8], at: usize) -> u32 {
    bytes
        .get(at..at + 4)
        .and_then(|four| four.try_into().ok())
        .map_or(0, u32::from_ne_bytes)
}

fn put(bytes: &mut [u8], at: usize, value: u32) {
    if let Some(four) = bytes.get_mut(at..at + 4) {
        four.copy_from_slice(&value.to_ne_bytes());
    }
}

fn text(bytes: &[u8]) -> &str {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    std::str::from_utf8(&bytes[..end])
        .ok()
        .filter(|text| !text.is_empty())
        .unwrap_or("unknown")
}

/// Runs one ethtool command on `name`; `data` starts with the command word.
fn ethtool(socket: &UdpSocket, name: &str, data: &mut [u8]) -> bool {
    if name.len() >= libc::IFNAMSIZ {
        return false;
    }
    // SAFETY: a zeroed ifreq is valid before its fields are set.
    let mut request: libc::ifreq = unsafe { std::mem::zeroed() };
    for (dst, src) in request.ifr_name.iter_mut().zip(name.bytes()) {
        *dst = src as libc::c_char;
    }
    request.ifr_ifru.ifru_data = data.as_mut_ptr().cast();
    // SAFETY: the kernel copies each command's fixed structure in and out of
    // `data`, which every caller sizes for its command and keeps alive here.
    unsafe { libc::ioctl(socket.as_raw_fd(), SIOCETHTOOL as _, &mut request) == 0 }
}

/// `name=on` or `name=off` for each of [`FEATURES`] the device knows.
fn features(socket: &UdpSocket, name: &str) -> String {
    // struct ethtool_sset_info: cmd, reserved, sset_mask (u64), then one count.
    let mut info = [0_u8; 20];
    put(&mut info, 0, ETHTOOL_GSSET_INFO);
    info[8..16].copy_from_slice(&(1_u64 << ETH_SS_FEATURES).to_ne_bytes());
    // The kernel clears the mask bit of a string set it does not have.
    if !ethtool(socket, name, &mut info) || info[8..16] == [0; 8] {
        return " features=unknown".into();
    }
    let count = (word(&info, 16) as usize).min(1024);
    // struct ethtool_gstrings: cmd, string_set, len, then len 32-byte names.
    let mut names = vec![0_u8; 12 + count * STRING_LEN];
    put(&mut names, 0, ETHTOOL_GSTRINGS);
    put(&mut names, 4, ETH_SS_FEATURES);
    put(&mut names, 8, count as u32);
    // struct ethtool_gfeatures: cmd, size, then size blocks of four words,
    // the third of which holds the active bits.
    let blocks = count.div_ceil(32);
    let mut bits = vec![0_u8; 8 + blocks * 16];
    put(&mut bits, 0, ETHTOOL_GFEATURES);
    put(&mut bits, 4, blocks as u32);
    if !ethtool(socket, name, &mut names) || !ethtool(socket, name, &mut bits) {
        return " features=unknown".into();
    }
    let mut line = String::new();
    for wanted in FEATURES {
        let index = (0..count).find(|index| {
            names
                .get(12 + index * STRING_LEN..12 + (index + 1) * STRING_LEN)
                .is_some_and(|bytes| text(bytes) == wanted)
        });
        if let Some(index) = index {
            let active = word(&bits, 8 + (index / 32) * 16 + 8) >> (index % 32) & 1;
            line.push_str(&format!(
                " {wanted}={}",
                if active == 1 { "on" } else { "off" }
            ));
        }
    }
    line
}

/// The header fields a card hashes UDP over IPv6 by, from its `RXH_*` bits.
/// Lanes differ in their ports, so a card that names only the addresses
/// puts every lane of a peer on one receive queue.
fn hash_fields(bits: u64) -> String {
    let names = [
        (4, "src"),
        (5, "dst"),
        (6, "sport"),
        (7, "dport"),
        (9, "flowlabel"),
    ];
    let fields: Vec<&str> = names
        .into_iter()
        .filter(|(bit, _)| bits >> bit & 1 == 1)
        .map(|(_, name)| name)
        .collect();
    if fields.is_empty() {
        "none".into()
    } else {
        fields.join(",")
    }
}

// DECISION(2026-11-27): the report gives the card's UDP hash and queue counts, not a flow's queue.
//   Why: the kernel records a socket's receiving CPU and NAPI instance only for connected UDP
//   sockets, and receive sockets here take any source.
fn udp_hash(socket: &UdpSocket, name: &str) -> String {
    // struct ethtool_rxnfc: cmd, flow_type, data (u64), then a flow
    // specification and a rule count that this command leaves unused.
    let mut request = [0_u8; 192];
    put(&mut request, 0, ETHTOOL_GRXFH);
    put(&mut request, 4, UDP_V6_FLOW);
    if !ethtool(socket, name, &mut request) {
        return "unknown".into();
    }
    let mut bits = [0; 8];
    bits.copy_from_slice(&request[8..16]);
    hash_fields(u64::from_ne_bytes(bits))
}

/// The receive queue a driver's statistic counts packets for, by the names
/// drivers give such counters: `rx0_packets`, `rx_queue_0_packets`,
/// `rx-0.packets`. None for any other statistic.
fn queue_of(name: &str) -> Option<usize> {
    // One separator, where the driver writes one.
    fn skip(part: &str) -> &str {
        part.strip_prefix(['_', '-', '.']).unwrap_or(part)
    }
    let rest = skip(name.strip_prefix("rx")?);
    let rest = skip(rest.strip_prefix("queue").unwrap_or(rest));
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    let (queue, rest) = rest.split_at_checked(digits)?;
    let queue = queue.parse().ok()?;
    (skip(rest) == "packets").then_some(queue)
}

/// Packets each receive queue has taken since the device came up, in queue
/// order: whether lanes spread over the queues shows in two reports taken
/// under load. `unknown` where the driver does not count per queue under one
/// of the names above, and on a virtual device such as a macvlan, which has
/// no such counters; read it there on the machine that owns the card.
fn queue_packets(socket: &UdpSocket, name: &str) -> String {
    const ETHTOOL_GSTATS: u32 = 0x1d;
    const ETH_SS_STATS: u32 = 1;
    let mut info = [0_u8; 20];
    put(&mut info, 0, ETHTOOL_GSSET_INFO);
    info[8..16].copy_from_slice(&(1_u64 << ETH_SS_STATS).to_ne_bytes());
    if !ethtool(socket, name, &mut info) || info[8..16] == [0; 8] {
        return "unknown".into();
    }
    let count = (word(&info, 16) as usize).min(4096);
    let mut names = vec![0_u8; 12 + count * STRING_LEN];
    put(&mut names, 0, ETHTOOL_GSTRINGS);
    put(&mut names, 4, ETH_SS_STATS);
    put(&mut names, 8, count as u32);
    // struct ethtool_stats: cmd, n_stats, then n_stats 64-bit values.
    let mut values = vec![0_u8; 8 + count * 8];
    put(&mut values, 0, ETHTOOL_GSTATS);
    put(&mut values, 4, count as u32);
    if !ethtool(socket, name, &mut names) || !ethtool(socket, name, &mut values) {
        return "unknown".into();
    }
    let mut queues = [None; 64];
    for index in 0..count {
        let queue = names
            .get(12 + index * STRING_LEN..12 + (index + 1) * STRING_LEN)
            .and_then(|bytes| queue_of(text(bytes)));
        let value = values
            .get(8 + index * 8..16 + index * 8)
            .and_then(|eight| eight.try_into().ok())
            .map(u64::from_ne_bytes);
        if let (Some(slot), Some(value)) = (queue.and_then(|q| queues.get_mut(q)), value) {
            *slot = Some(value);
        }
    }
    let known: Vec<String> = queues
        .iter()
        .map_while(|queue| queue.map(|packets| packets.to_string()))
        .collect();
    if known.is_empty() {
        "unknown".into()
    } else {
        known.join("/")
    }
}

fn sysfs(device: &str, file: &str) -> String {
    fs::read_to_string(format!("/sys/class/net/{device}/{file}"))
        .map(|text| text.trim().to_owned())
        .ok()
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

/// One line about the device `name`: driver, MTU, queues, poll settings, offloads.
pub fn device(name: &str) -> String {
    // The name goes into a sysfs path; a device name is short and has no slash.
    if name.is_empty()
        || name.len() >= libc::IFNAMSIZ
        || name.contains('/')
        || name == "."
        || name == ".."
    {
        return "driver=unknown".into();
    }
    let Ok(socket) = UdpSocket::bind("[::]:0") else {
        return "driver=unknown".into();
    };
    // struct ethtool_drvinfo: cmd, then driver, version, fw_version, bus_info
    // of 32 bytes each, and fields this line does not use.
    let mut driver = [0_u8; 196];
    put(&mut driver, 0, ETHTOOL_GDRVINFO);
    let known = ethtool(&socket, name, &mut driver);
    let field = |at: usize| {
        if known {
            text(&driver[at..at + 32])
        } else {
            "unknown"
        }
    };
    // struct ethtool_channels: cmd, four maxima, then rx, tx, other, combined.
    let mut channels = [0_u8; 36];
    put(&mut channels, 0, ETHTOOL_GCHANNELS);
    let queues = if ethtool(&socket, name, &mut channels) {
        format!(
            "rx={} tx={} combined={}",
            word(&channels, 20),
            word(&channels, 24),
            word(&channels, 32)
        )
    } else {
        "unknown".into()
    };
    format!(
        "driver={} version={} bus={} mtu={} tx_queue_len={} channels {queues} \
udp6_hash={} rx_queue_packets={} gro_flush_timeout={} napi_defer_hard_irqs={}{}",
        field(4),
        field(36),
        field(100),
        sysfs(name, "mtu"),
        sysfs(name, "tx_queue_len"),
        udp_hash(&socket, name),
        queue_packets(&socket, name),
        sysfs(name, "gro_flush_timeout"),
        sysfs(name, "napi_defer_hard_irqs"),
        features(&socket, name),
    )
}

/// The device a datagram to `address` would leave by, found without sending
/// one: the kernel's source address for it names the device.
pub fn route_device(address: Ipv6Addr, scope_id: u32) -> Option<String> {
    let socket = UdpSocket::bind("[::]:0").ok()?;
    socket
        .connect(SocketAddrV6::new(address, 9, 0, scope_id))
        .ok()?;
    let SocketAddr::V6(local) = socket.local_addr().ok()? else {
        return None;
    };
    let wanted = local.ip().octets();
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills list with a chain this function frees below.
    if unsafe { libc::getifaddrs(&mut list) } != 0 {
        return None;
    }
    let mut found = None;
    let mut entry = list;
    while !entry.is_null() {
        // SAFETY: entry is a live node of the chain; its address, when present
        // and of family AF_INET6, is a sockaddr_in6, and its name is a C string.
        unsafe {
            let address = (*entry).ifa_addr;
            if found.is_none()
                && !address.is_null()
                && i32::from((*address).sa_family) == libc::AF_INET6
                && (*address.cast::<libc::sockaddr_in6>()).sin6_addr.s6_addr == wanted
            {
                found = CStr::from_ptr((*entry).ifa_name)
                    .to_str()
                    .ok()
                    .map(str::to_owned);
            }
            entry = (*entry).ifa_next;
        }
    }
    // SAFETY: list came from getifaddrs and is freed once.
    unsafe { libc::freeifaddrs(list) };
    found
}

fn first_line<'a>(file: &'a str, key: &str) -> Option<&'a str> {
    file.lines()
        .find_map(|line| line.strip_prefix(key))
        .and_then(|rest| rest.trim_start().strip_prefix(':'))
        .map(str::trim)
}

/// The CPU model, its clock and the CPUs this process may use, then the
/// kernel's ceilings for socket buffers.
pub fn lines() -> String {
    let cpuinfo = fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let status = fs::read_to_string("/proc/self/status").unwrap_or_default();
    let limit = |name: &str| {
        fs::read_to_string(format!("/proc/sys/net/core/{name}"))
            .map(|text| text.trim().to_owned())
            .unwrap_or_else(|_| "unknown".into())
    };
    format!(
        "unquic: cpu {}; {} MHz; allowed {}\nunquic: limits rmem_max={} wmem_max={}\n",
        first_line(&cpuinfo, "model name").unwrap_or("unknown"),
        first_line(&cpuinfo, "cpu MHz").unwrap_or("unknown"),
        first_line(&status, "Cpus_allowed_list").unwrap_or("unknown"),
        limit("rmem_max"),
        limit("wmem_max"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_reports_its_mtu_offloads_and_route() {
        let line = device("lo");
        assert!(line.starts_with("driver="), "{line}");
        assert!(line.contains(" mtu=65536 "), "{line}");
        // Loopback knows every generic offload, on or off.
        for feature in ["tx-udp-segmentation=o", "rx-gro=o", "rx-checksum=o"] {
            assert!(line.contains(feature), "{feature}: {line}");
        }
        assert_eq!(route_device(Ipv6Addr::LOCALHOST, 0).as_deref(), Some("lo"));
        // Loopback has no receive hash; a card that has one names its fields.
        assert!(line.contains(" udp6_hash=unknown "), "{line}");
        assert_eq!(hash_fields(0x30), "src,dst");
        assert_eq!(hash_fields(0xf0), "src,dst,sport,dport");
        assert_eq!(hash_fields(1 << 31), "none");
        // Loopback counts no packets per queue. Drivers that do name the
        // counters in a few ways, and nothing else is taken for one.
        assert!(line.contains(" rx_queue_packets=unknown "), "{line}");
        for (name, queue) in [
            ("rx0_packets", Some(0)),
            ("rx17_packets", Some(17)),
            ("rx_queue_3_packets", Some(3)),
            ("rx-2.packets", Some(2)),
            ("rx_packets", None),
            ("rx0_bytes", None),
            ("rx_queue_0_xdp_packets", None),
            ("rx0_csum_none", None),
            ("tx0_packets", None),
            ("rx_queue_packets", None),
        ] {
            assert_eq!(queue_of(name), queue, "{name}");
        }
        // Names that cannot be devices cost no ioctl and no path lookup.
        for bad in ["", "..", "a/b", "sixteen-bytes-long"] {
            assert!(device(bad).starts_with("driver=unknown"), "{bad}");
        }
    }

    #[test]
    #[ignore = "needs two virtio cards joined back to back, as tests/qemu-kernel.sh gives its guest; passes without them"]
    fn a_virtio_cards_receive_queues_are_counted() {
        let driver = |name: &str| {
            fs::read_link(format!("/sys/class/net/{name}/device/driver"))
                .ok()
                .and_then(|path| Some(path.file_name()?.to_str()?.to_owned()))
        };
        let mut cards: Vec<String> = fs::read_dir("/sys/class/net")
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| driver(name).as_deref() == Some("virtio_net"))
            .collect();
        cards.sort();
        let [near, far] = cards.as_slice() else {
            // tests/qemu-kernel.sh says that its guest has the cards, so a
            // guest without them fails instead of passing unseen.
            assert!(
                std::env::var_os("UNQUIC_VIRTIO_CARDS").is_none(),
                "{} virtio cards here",
                cards.len()
            );
            eprintln!("skipped: {} virtio cards here", cards.len());
            return;
        };
        // Each queue's count, or None where the line says unknown.
        let counted = |name: &str| -> Option<Vec<u64>> {
            let line = device(name);
            let field = line
                .split(' ')
                .find_map(|part| part.strip_prefix("rx_queue_packets="))?;
            field.split('/').map(|count| count.parse().ok()).collect()
        };
        let before = counted(far).expect("virtio_net counts packets per receive queue");
        assert!(!before.is_empty());
        // The far card takes whatever the near one says as it comes up:
        // group reports and neighbour discovery.
        for (card, flags) in [(far, &["promisc", "on", "up"][..]), (near, &["up"][..])] {
            let status = std::process::Command::new("ip")
                .args(["link", "set", "dev", card])
                .args(flags)
                .status()
                .unwrap();
            assert!(status.success());
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            let now = counted(far).unwrap();
            if now.iter().sum::<u64>() > before.iter().sum::<u64>() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "no packet counted: {before:?} then {now:?}"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    #[test]
    fn host_lines_name_the_cpu_and_the_buffer_ceilings() {
        let text = lines();
        let mut lines = text.lines();
        assert!(lines.next().unwrap().starts_with("unquic: cpu "));
        assert!(
            lines
                .next()
                .unwrap()
                .starts_with("unquic: limits rmem_max=")
        );
        assert_eq!(lines.next(), None);
        assert_eq!(
            first_line("a: 1\nmodel name\t: Some CPU\n", "model name"),
            Some("Some CPU")
        );
        assert_eq!(first_line("model: x", "model name"), None);
    }
}
