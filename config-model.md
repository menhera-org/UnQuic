# Configuration model

The following example is illustrative:

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
# role should be automatic, not needing a configuration on either end, instead
tap_name = "unquic0" # defaults to sessions.name
inner_mtu = 2400
mac = "02:00:00:00:00:01" # optional
```