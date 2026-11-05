# UnQuic Encapsulation

- Naming: `UnQuic`, because the transport borrows ideas from QUIC without being QUIC.
- 'Encapsulation' instead of 'VPN': people blindly expect a VPN to encrypt, and 'VPN' is a commercially contaminated marketing word.
- Development language: pure Rust.
- Dependencies: none except `libc`. Everything else (UUID types, congestion control) is written in-tree. The UUID type is named `UnquicUuid` to avoid collisions with external UUID types. Write C-like Rust, optimized for performance.
- Dependencies exceptions: feature-gate all the other deps: `dns` pulls `iphost v0.4.4` - otherwise the endpoints are configured manually. `uuid-compat` pulls in `uuid` crate, and the conversions from/into the in-tree UUID types. `bin` pulls in `serde` and `toml`, and enables `dns`. Obtain random bytes through Linux's `getrandom(2)` via `libc::getrandom()`, without an additional crate. The `unquic gen-uuid` subcommand generates session cookies; the daemon never generates a session cookie at runtime.
- Protocol baseline: automatic handshake role election, random connection receive tags from `libc::getrandom()`, bounded selective repair, and one in-tree BBRv3 controller per direction and path group. No wall-clock epochs or persistent restart counters.
- Crate shape: library only. The daemon binary exists only when the `bin` feature is enabled.
- Use `libc` for syscalls and socket options where appropriate; use `const fn` wherever a function can be one.
- Receive admission: bound receive sockets accept packets by cookie, wire validity and protocol state, regardless of source address or port. Peer addresses select transmit destinations only.
- Performance contract: no locks or allocations in hot paths. State transitions make no syscalls; workers batch I/O, notifications and timer updates. Performance claims require measurements.
- No async runtime: plain pinned threads with batched syscalls. `tokio` gives one syscall per packet, no GSO/GRO, cross-core task migration, and 1 ms timers, all wrong for a few always-busy sockets. See section 10 of the design.
- Platform contract: assume the Linux kernel and only the interfaces, files, and permissions needed by the selected features, such as `/dev/net/tun` for TAP creation and `/etc/resolv.conf` when using `iphost`'s system resolver. Its configurable `ipv6`, `ra`, and `dhcpv6` modes bypass system resolvers. Document each additional host assumption with the feature that needs it. Make no assumptions about the surrounding userspace: no required XDG or other freedesktop.org conventions, GNU userspace, Android or ChromeOS facilities, systemd, or D-Bus dependencies. Binaries are built for `*-unknown-linux-musl` targets.
- Library I/O: no ordinary filesystem access except `/etc/resolv.conf` through `iphost` when its system resolver is selected; no environment-variable reads or stdio. Kernel interfaces such as `/dev/net/tun`, sockets, and ioctls are allowed. Application configuration-file handling and command-line I/O belong to the caller or daemon.
- Future optional integration: systemd support without linking to systemd or D-Bus libraries, DEB/RPM packaging, and restarts that preserve TAPs by retaining and handing over their file descriptors, following rs-l2tpd's descriptor-preservation approach. These additions must leave standalone operation and the library I/O boundary intact.
- Startup readiness: every configured TAP interface exists, whether newly created or adopted. The daemon forks and its original parent exits successfully only at readiness, so any init system or launcher can wait for that exit status. This works without systemd and does not wait for network connectivity or peer handshakes. When used, the optional embedded systemd unit must make both `network.target` and `network-online.target` require and wait for daemon readiness; the daemon must not depend on either target or any wait-online service.
- TAP configuration: creation accepts `Option<MacAddress>`. The in-tree `MacAddress` type accepts nonzero unicast addresses, whether globally or locally administered. `None` derives a stable address in Menhera's `60:a4:34:4x:xx:xx` MAC-M range from the current UTS hostname and interface name using SHA-256. UnQuic sets the configured MTU and optional MAC but leaves administrative state, addressing and bridge membership to the caller or network manager (for example, systemd-networkd or ifupdown). Transport state controls carrier, independently of administrative state.
- Design: see [initial-design.md](initial-design.md). Implementation order and per-step acceptance: see [ROADMAP.md](ROADMAP.md).

## Implementation status

The first foundation provides `UnquicUuid` parsing and UUIDv4 provisioning,
duplicate-checked cookie lookup, checked `MacAddress` values, IPv6 receive and
transmit sockets, and nonpersistent multiqueue TAP ownership. Frame I/O and
cookie lookup allocate no memory. The default build depends only on `libc`;
`dns` enables exactly `iphost v0.4.4`, without its default Tokio, tracing or serde
features. Its direct, RA and DHCPv6 resolver modes are available.

Receive sockets use fixed nonzero ports and expose no send method. Transmit
sockets inherit their receiver's binding scope, use stable dynamic ports, and
disable IPv6 fragmentation. Sends are limited to the unprobed 1232-byte UDP
baseline. Truncated receives are rejected. TAP creation leaves the device
administratively down with carrier down, uses ordinary Ethernet frames without
virtio metadata, and closes all newly opened queues if setup fails. Queues can
be moved out of the `Tap` for per-worker ownership, and any queue controls
carrier.

Default MACs hash the exact bytes `<hostname>\0<interface name>`, with one NUL
separator and no trailing NUL or case normalization. For digest `h`, the address
is `[0x60, 0xa4, 0x34, 0x40 | (h[0] & 0x0f), h[1], h[2]]`. For example,
hostname `unquic-test-host` and interface `uq-test` produce
`60:a4:34:49:63:0a`. The library reads the hostname with `gethostname` and
hashes it in-tree without allocations or extra dependencies. A hostname lookup
failure aborts creation; an explicit MAC bypasses lookup. Name changes affect
future creation only.

The suffix has 20 bits, so collisions are possible; identical hostname/interface
pairs always produce identical MACs. Set an explicit MAC where needed to avoid
duplicates on the same Ethernet network.

```rust,no_run
use unquic::{MacAddress, tap::Tap};

fn create_tap() -> Result<Tap, Box<dyn std::error::Error>> {
    let mac: MacAddress = "02:00:00:00:00:01".parse()?;
    Ok(Tap::create(c"uq0", 1500, 2, Some(mac))?)
}
// The caller retains the TAP and delegates administrative setup to its network manager.
```

The v1 wire codec covers the 48-byte header and all eleven record types. It
validates each complete datagram before exposing records, borrows DATA payloads
and ACK ranges, and encodes into caller-owned buffers without allocation.
[Fixed byte vectors](tests/vectors/wire-v1/README.md) cover every record type;
tests exercise malformed lengths, reserved fields, lane identities, ACK ranges,
and handshake isolation.

The pure handshake state machine elects roles automatically, negotiates frame
limits, retries lost flights, and preserves active state through replacement.
Tags come from nonblocking kernel randomness; tests inject deterministic tags
and monotonic times. Commit, retirement and carrier changes are returned to the
caller. The worker barrier and actual network dispatch remain to be implemented.
Tests cover simultaneous startup, equal tags, each lost flight, stale replay,
role reversal, either/both endpoint restarts, reply limits and liveness deadlines.

`config::Config::validate` checks names, references, ports, addresses/scopes,
MTUs, queue/lane counts, TAP names and cookie uniqueness. It returns an immutable
configuration with numeric endpoint/peer/session indices. One endpoint serves
many peers; one peer serves many sessions. Core peers hold raw IPv6 addresses
or await a supplied address. The optional `dns` layer exposes iphost's types and a blocking resolver for
helper threads; it returns IPv6 candidates without changing the active
transport destination itself.

Cookie dispatch uses a short scan through 16 sessions and a randomized hash
table above that. Allocation-counting tests cover lookup and handshake paths.
Reproduce the lookup comparison with
`cargo bench --target x86_64-unknown-linux-musl --bench cookie_lookup`.

`counters::Counters` provides atomic totals and gauges for session, peer and
lane owners. Updates use Relaxed ordering; snapshots read with Acquire ordering.
Fields have explicit units, and a snapshot samples fields independently rather
than promising a coherent transport-state transaction. Workers will publish
batch totals; snapshots perform no allocation or locking.

The worker handoff foundation is a fixed 1024-entry SPSC ring with exclusive
producer/consumer handles and cached cursors. Push/pop use no locks, allocations
or syscalls. Wraparound, full-ring ownership, destruction and concurrent transfer
have unit tests and Miri coverage; worker notification is not yet integrated.

The crate does not yet forward frames between peers. Worker ownership,
batching/offloads, path-MTU probing, recovery, BBRv3, scheduled DNS
refresh, the daemon and descriptor adoption remain to be implemented.
There is no throughput claim for this foundation.

## Validation

Run the ordinary tests and lint checks with a Linux musl target:

```sh
cargo test --target x86_64-unknown-linux-musl
cargo clippy --target x86_64-unknown-linux-musl --all-targets -- -D warnings
cargo fmt --all -- --check
cargo test --target x86_64-unknown-linux-musl --features dns
```

The ignored `linux_foundation` test requires a disposable container with
`/dev/net/tun`, `CAP_SYS_ADMIN`, `CAP_NET_ADMIN` and `CAP_NET_RAW`. It creates a
fresh network and UTS namespaces before creating interfaces or changing the
hostname. Run the built
test executable there with `--ignored`, or run this command inside the container
if a Rust toolchain and the source are available:

```sh
cargo test --target x86_64-unknown-linux-musl --test linux_foundation -- --ignored
```

The kernel test checks actual IPv6 UDP headers and port use, explicit device
binding, TAP flags/MTU/MAC, deterministic MAC recreation, administrative state,
multiqueue frame I/O,
VLAN/QinQ bytes, carrier transitions and deletion after the last queue closes.
Maximum-size QinQ is currently checked in the TAP-write direction; testing its
TAP-read direction through VLAN upper devices belongs to the composition tests.
Bridge/OVS/VRF matrices, offloads, Linux 5.0 qualification and performance
acceptance remain open.

## License

Dual-licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- Mozilla Public License, Version 2.0 ([LICENSE-MPL](LICENSE-MPL))

at your option.
