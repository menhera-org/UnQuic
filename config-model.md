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
are still to be implemented.

`tap_name` defaults to the session name. `mac` may be omitted for deterministic
derivation. The core accepts MTUs from 1500 through 9000 and queue/lane counts
from 1 through 64. Its default daemon memory ceiling is 256 MiB; fixed runtime
allocations must fit that ceiling before workers start.

Peer addresses select transmit destinations only. A bound receive socket admits
packets by cookie, wire validity and protocol state regardless of sender address
or port, and incoming packets never redirect the configured transmit destination.
