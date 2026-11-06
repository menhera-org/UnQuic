# Configuration model

Endpoints, peers and sessions form two one-to-many relationships: one endpoint
can serve many peers, and one peer can carry many sessions. Names are resolved
to immutable numeric indices at startup. Handshake roles are elected automatically.

The foreground daemon accepts this TOML shape. Replace the cookie placeholder
with an explicitly provisioned UUID:

```toml
[[endpoints]]
name = "default"
listen_address = "::"
listen_port = 40000
bind_device = "vrf-ngn"

[[peers]]
name = "peer-a"
endpoint = "default"
host = "test.example.invalid"
peer_port = 40000
tx_lanes = 4

[[sessions]]
name = "unquic0"
peer = "peer-a"
cookie = "<uuid-v4>"
tap_name = "unquic0" # defaults to sessions.name
inner_mtu = 2400
mac = "02:00:00:00:00:01" # optional
```

The core configuration contains only raw IPv6 addresses and numeric scope IDs.
The optional `dns` feature depends on exactly `iphost v0.4.4` and uses its
hostname types and resolvers to supply IPv6 addresses. A core peer may initially
have no address; TAP creation and readiness do not wait for resolution. Resolver
work runs outside packet workers. The daemon starts one helper per hostname
after TAP readiness; literals go directly into the core without a resolver.
Library callers use
`Control::supply_peer_address(peer_index, ipv6, scope_id)` after readiness.
The configured peer port is unchanged; candidate data waits for a fresh
handshake and worker commit. Submitting an update can return WouldBlock under
backpressure, so a resolver helper retains and retries its selected address.

`tap_name` defaults to the session name. `mac` may be omitted for deterministic
derivation. The core accepts MTUs from 1500 through 9000 and queue/lane counts
from 1 through 64. Its default daemon memory ceiling is 256 MiB; fixed runtime
allocations must fit that ceiling before workers start.

Peer addresses select transmit destinations only. A bound receive socket admits
packets by cookie, wire validity and protocol state regardless of sender address
or port, and incoming packets never redirect the configured transmit destination.

## Core fields and validation

Library callers construct `config::Config` directly. Only `Daemon` has field
defaults; `Config::default()` contains empty endpoint, peer and session lists.
The daemon defaults omitted `queue_count` and `tx_lanes` to one,
`scope_id` to zero, `outer_payload_limit` to 8952, and `refresh_seconds` to 60. Addresses, ports, session
cookies and `inner_mtu` remain explicit. Unknown TOML fields are rejected.

| Struct | Fields and constraints |
| --- | --- |
| `Daemon` | `memory_limit`: nonzero bytes, default 256 MiB |
| `Endpoint` | Unique nonempty `name`; IPv6 `listen_address` (unspecified allowed); nonzero `listen_port`; numeric `scope_id`; optional interface/VRF `bind_device` |
| `Peer` | Unique nonempty `name`; existing `endpoint` name; optional unicast IPv6 `address`; nonzero `peer_port`; numeric `scope_id`; `tx_lanes` 1–64; `outer_payload_limit` 1232–8952; optional nonempty `path_group` |
| `Session` | Unique nonempty `name`; existing `peer` name; unique `UnquicUuid` cookie; optional `tap_name`; `inner_mtu` 1500–9000; optional checked `MacAddress`; `queue_count` 1–64 |

Names cannot contain NUL. Effective TAP names must be unique, fit in 15 bytes,
and contain no NUL, whitespace, slash, colon or percent sign; "." and ".." are
invalid. Binding device names follow the same interface rules but may contain
percent signs. The library rejects overlapping receive endpoints with the same
port, device binding and scope. It rejects multicast and IPv4-mapped addresses;
link-local addresses require a nonzero numeric scope ID. A peer's unspecified
address is invalid: use None to wait for address supply.

`tap_name` (or the session name when omitted) is a creation name. Once created,
the TAP's device identity and queue descriptors bind it to the session. External
renames must be respected during operation and descriptor-preserving restart;
configuration must not rename it back or create a replacement at the old name.
Renaming does not recompute its MAC. Published TAP removal requires an explicit
instruction, independently of peer state or UDP socket lifetime. Descriptor
handover and full lifetime enforcement remain Stage 4 work.

`validate()` consumes the input and returns immutable numeric references or
the first `ConfigError`. That error identifies a field, optional zero-based
entry index and fixed reason; it does not include a cookie or user-provided
value. Validation performs no I/O and does not check device existence. Runtime
storage accounting is a separate startup check before TAP creation.

The worker count is the largest session `queue_count`, or one for no sessions.
TAP queue index selects its worker; TX lane index modulo worker count selects
its owner. Queue and lane counts need not match. Unfragmented UDP in a session with a TAP queue on
every worker hashes among the reading worker's own lanes. TAP queue migration
can change a flow's lane for new frames and cause reordering. A frame and its
repairs keep the originally selected lane.

TCP, fragmented IP and other traffic retain strict hashing. Sessions with
fewer TAP queues than global workers also retain strict hashing across
all lanes to use the other transmit workers. Readers with no owned lane also
retain hashing and handoffs. Multiples of the worker count give each reader the
same number of lane choices, but are not required. eBPF is excluded.
`path_group` is validated and retained, but congestion grouping has no runtime
effect until stage 3.

The daemon's `host` field uses iphost's `IpHost`/`FQDN` types. The
current `dns::resolve_ipv6` helper blocks and may allocate or lock, so call it
outside workers. It honors the selected resolver's cache-on-error policy and
returns None when it has no usable IPv6 selection. It does not schedule refresh,
choose a scope ID, submit an address or alter an active connection. A caller
retains the old destination on resolution failure and retries address submission
when the bounded control queue returns WouldBlock.

A peer's `outer_payload_limit` caps discovery on all its transmit lanes and
sessions. The daemon defaults to 8952 UDP payload bytes, allowing up to 9000
outer IPv6 bytes. DATA starts at 1232 bytes and grows only after an exact reply
to a successfully submitted padded probe. A configured value of 1452 caps
discovery at 1500 outer bytes; it does not assert that the path supports them.
Sessions sharing a transmit socket share its MTU state; each lane probes its
own path. New destination sockets start at the baseline.

Sockets forbid fragmentation. EMSGSIZE and validated probe Packet Too Big
reports lower the limit and replan unsent frame ranges without changing frame
identity. Probe timeouts also detect smaller paths when ICMPv6 is filtered.
Queued frames retain their bounded lifetimes; a path that fails baseline
confirmation suspends DATA until a baseline probe succeeds. TAP MTU remains
independent. See README.md for probe timing and current feedback limitations.

The production NTT NGN acceptance profile has a 1500-byte outer IPv6 ceiling.
Other deployments can have smaller or larger paths, so discovery must probe
within the configured ceiling, up to 9000 outer bytes. MTUs above 9000 are
outside the product's scope, even when a local interface supports them.

## Daemon resolver selection

Each peer has an optional `resolver` table. Omitting it selects `system`.
`refresh_seconds` belongs to the peer and accepts 1–86400 seconds. Refresh
runs that interval after each completed lookup; lookup latency adds to it.
Each hostname has its own helper, so a slow lookup does not block another peer.

| Mode | Additional fields | Resolution |
| --- | --- | --- |
| `system` | None | iphost's operating-system resolver |
| `static` | Nonempty `nameservers` array of IP socket addresses; optional `bind_device` | Direct DNS queries to those servers |
| `ra` | `interface` | DNS servers advertised through IPv6 Router Advertisements |
| `dhcpv6` | `interface`; optional `client_mac` | Stateless DHCPv6 DNS discovery |
| `ipv6` | `interface`; optional `client_mac` | iphost's combined discovery, preferring available DHCPv6 servers over RA |

For example, place this table after a peer's fields and before the next
`[[peers]]` or `[[sessions]]` table:

```toml
[peers.resolver]
mode = "static"
nameservers = ["[2001:db8::53]:53"]
bind_device = "vrf-ngn"
```

IPv6 discovery can use a DUID-LL derived from an explicitly supplied interface MAC:

```toml
[peers.resolver]
mode = "ipv6"
interface = "eth0"
client_mac = "02:00:00:00:00:01"
```

Omitting `client_mac` sends no DHCPv6 Client Identifier. Supply the actual
interface MAC where the network requires a matching DUID-LL. It is independent
of each session's TAP MAC. The daemon does not read a device MAC or invent a
client identity. Other iphost ClientId forms remain available to library callers.

Static and discovery modes never fall back to the system resolver. They do not
require resolv.conf. System mode follows the operating system's resolver policy.
RA uses iphost's raw ICMPv6 listener when permitted, otherwise its Linux netlink
fallback observes advertisements processed by the kernel. DHCPv6 needs access
to UDP port 546 and may conflict with another client; namespace identification
needs SO_NETNS_COOKIE or, on older kernels, readable /proc/thread-self/ns/net.
Interface binding and listener creation require the corresponding kernel
permissions. These requirements do not delay TAP creation: failed resolver
construction retries at the configured interval.

A failed or empty lookup preserves the transport's selected address. A changed
usable IPv6 result queues a candidate; WouldBlock retries every 100 ms while
refreshes can replace the pending selection. Unchanged results do not restart
connections. Link-local answers need the peer's explicit numeric `scope_id`.
The daemon never derives a transmit address from received traffic.

Resolver work may allocate, lock and create discovery threads; it is outside the
packet path and the transport storage reservation. On shutdown the daemon joins
transport workers and exits. It does not wait for blocked DNS helper threads:
iphost's system lookup has no cancellable time bound. The process exit closes
their descriptors. Real RA/DHCPv6 discovery has not yet been tested by this
project's container suite.
