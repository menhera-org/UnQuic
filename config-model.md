# Configuration model

Endpoints, peers and sessions form two one-to-many relationships: one endpoint
can serve many peers, and one peer can carry many sessions. Names are resolved
to immutable numeric indices at startup. Handshake roles are elected automatically.

The following daemon TOML example is illustrative:

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
work runs outside packet workers. The daemon's TOML parser and refresh scheduling
are still to be implemented. Library callers use
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
The TOML example above is a proposed daemon format, not an implemented parser.

| Struct | Fields and constraints |
| --- | --- |
| `Daemon` | `memory_limit`: nonzero bytes, default 256 MiB |
| `Endpoint` | Unique nonempty `name`; IPv6 `listen_address` (unspecified allowed); nonzero `listen_port`; numeric `scope_id`; optional interface/VRF `bind_device` |
| `Peer` | Unique nonempty `name`; existing `endpoint` name; optional unicast IPv6 `address`; nonzero `peer_port`; numeric `scope_id`; `tx_lanes` 1–64; optional nonempty `path_group` |
| `Session` | Unique nonempty `name`; existing `peer` name; unique `UnquicUuid` cookie; optional `tap_name`; `inner_mtu` 1500–9000; optional checked `MacAddress`; `queue_count` 1–64 |

Names cannot contain NUL. Effective TAP names must be unique, fit in 15 bytes,
and contain no NUL, whitespace, slash, colon or percent sign; "." and ".." are
invalid. Binding device names follow the same interface rules but may contain
percent signs. The library rejects overlapping receive endpoints with the same
port, device binding and scope. It rejects multicast and IPv4-mapped addresses;
link-local addresses require a nonzero numeric scope ID. A peer's unspecified
address is invalid: use None to wait for address supply.

`validate()` consumes the input and returns immutable numeric references or
the first `ConfigError`. That error identifies a field, optional zero-based
entry index and fixed reason; it does not include a cookie or user-provided
value. Validation performs no I/O and does not check device existence. Runtime
storage accounting is a separate startup check before TAP creation.

The worker count is the largest session `queue_count`, or one for no sessions.
TAP queue index selects its worker; TX lane index modulo worker count selects
its owner. Queue and lane counts need not match. `path_group` is validated and
retained, but congestion grouping has no runtime effect until stage 3.

The daemon's future `host` field uses iphost's `IpHost`/`FQDN` types. The
current `dns::resolve_ipv6` helper blocks and may allocate or lock, so call it
outside workers. It honors the selected resolver's cache-on-error policy and
returns None when it has no usable IPv6 selection. It does not schedule refresh,
choose a scope ID, submit an address or alter an active connection. A caller
retains the old destination on resolution failure and retries address submission
when the bounded control queue returns WouldBlock.
