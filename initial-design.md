# NGN Ethernet Transport Initial Requirements and Design

**Draft 0.1 • 4 November 2026 • Human-life Information Platforms Institute Menhera®**

Build a Linux userspace Ethernet transport for closed-area NTT NGN networks. The transport must carry inner MTUs of at least 1500 over an IPv6-only UDP underlay, without emitting outer IP fragments. The target access line has IP MTU 1500; the final transport must also use larger path MTUs where discovery confirms them. It must use loss-tolerant congestion control and target sustained delivered throughput of at least 5 Gbit/s on modest 2–4-core hosts over routed IPv6 10G-EPON access lines with 4.5–7.5 ms RTT and nonzero packet loss.

Encryption is deliberately absent. A session UUID is the sole session admission cookie. The project must turn that deployment choice into materially lower processing cost and substantially higher throughput than WireGuard, SoftEther VPN, and comparable solutions under equivalent conditions.

This initial design distinguishes **confirmed requirements**, **proposed implementation choices**, and **open decisions**. It is a basis for prototyping and review; the byte-level protocol and performance acceptance profile are not yet frozen.

The project is named UnQuic and is written in pure Rust, built for `*-unknown-linux-musl` targets (see README.md). The library core depends only on `libc`; the transport, congestion controller, and UUID types are implemented in-tree, calling the kernel through `libc` wherever a syscall or socket option is needed. The in-tree UUID type is named `UnquicUuid` to avoid collisions with external UUID types. The crate is a library; the daemon binary is built only with the `bin` feature. Optional Cargo features pull in further crates: `dns` for FQDN resolution, `uuid-compat` for conversions to the `uuid` crate, and `bin` for the daemon's config parsing. Random bytes come from Linux's `getrandom(2)` through `libc::syscall(SYS_getrandom, ...)`, without an additional crate or filesystem access. `bin` enables `dns`, so the daemon always accepts FQDN hosts. Functions that can be `const fn` should be.

**Platform contract:** Linux means the kernel only. Host assumptions are limited to the interfaces, files, and permissions needed by the selected features: for example, `/dev/net/tun` for TAP creation and `/etc/resolv.conf` when using `iphost`'s system resolver. Document each additional assumption with the feature that needs it; do not infer a distribution or userspace environment from the presence of the Linux kernel. Standalone operation must not depend on XDG or other freedesktop.org conventions, GNU userspace, Android or ChromeOS facilities, systemd, or D-Bus. Future optional systemd integration and DEB/RPM packaging are allowed; configuration and startup must also work without them. Do not link against systemd or D-Bus libraries.

**Library I/O boundary:** the library must not access ordinary filesystem files, read environment variables, or use stdio, with one explicit filesystem exception: `iphost` may read `/etc/resolv.conf` when its system resolver is selected. Resolver modes that bypass the system resolver do not require that file. Access to kernel interfaces, including opening `/dev/net/tun`, sockets, and ioctls, is allowed. The caller or daemon handles application configuration files and command-line I/O and supplies explicit configuration to the library.

# 1 Deployment and motivation

The existing rs-l2tpd deployment uses the Linux L2TPv3 dataplane and native IPv6 fragmentation to carry frames larger than the underlay allows. NTT engineers have advised that packet loss makes naive fragmentation a poor fit for these NGN paths. Losing one fragment makes the whole encapsulated frame unavailable; moving the split into UDP alone does not remove this problem.

The proposed transport combines independently recoverable frame pieces, efficient packing, batching, and congestion control. Its opportunity is both lower CPU cost and better delivered goodput under loss.

The following are operator-provided deployment observations, not provider guarantees:

| Property | Current design input |
| - | - |
| Access | Shared 10G-EPON NTT NGN |
| Underlay | IPv6-only UDP |
| Underlay IP MTU | 1500 bytes on the target access line; larger confirmed MTUs used on other paths |
| RTT | Approximately 4.5–7.5 ms |
| Idle jitter | Less than approximately 1.5 ms |
| Loss | Nonzero; rate and burst distribution remain to be measured |
| Direct underlay throughput | Approximately 5 Gbit/s |
| Host budget | 2–4 cores with modest clock rates |

Available bandwidth may change with shared-access load. Idle jitter does not bound delay under congestion. The measured 5 Gbit/s is a comparison baseline, not a permanent pacing rate.

# 2 Confirmed requirements

The requirement identifiers below describe commitments established for this project. Proposed values and mechanisms appear separately.

## 2.1 Transport and addressing

- **R01 Ethernet service.** Expose Linux TAP interfaces and preserve Ethernet service across the underlay. Support a configurable inner MTU of at least 1500. Larger values must be carried by transport segmentation without outer IP fragmentation; the maximum is 9000 with up to two VLAN tags (9022 frame bytes).

- **R02 Outer transport.** Send UDP over IPv6 only. Every emitted outer IPv6 packet must fit the applicable path MTU and must not contain an IPv6 Fragment header.

- **R03 Endpoint configuration.** Identify each peer by an FQDN or IPv6 literal and a fixed UDP receive port.

- **R04 Receive port.** Each named endpoint has a fixed UDP port number used exclusively for receiving protocol traffic. The daemon never sends protocol packets from its configured receive ports.

- **R05 Transmit ports.** Maintain several dynamically allocated source ports per destination. Data, ACKs, probes, repair traffic, and session-control messages all leave through these dynamic ports and target the peer's fixed receive port.

- **R06 Interface and VRF binding.** Support binding the relevant local sockets to a Linux interface or VRF. Receive and transmit scope must be consistent.

## 2.2 Session and Ethernet behavior

- **R07 Session identity.** Identify a session by a 128-bit UUID cookie, independently of any one UDP five-tuple.

- **R08 Deliberately unencrypted operation.** Use no encryption. The session cookie is the sole session admission mechanism over the semi-trusted closed-area underlay.

- **R09 Device composition.** Support adding TAPs to Linux bridges or OVS and creating VLAN or macvlan interfaces over them, subject to normal Linux stacking rules.

- **R10 Ethernet transparency.** Support multiple MAC addresses, unicast, broadcast, multicast, tagged frames, and non-IP Ethernet traffic. The service must not depend on IPv4 or IPv6 being present inside a frame.

## 2.3 Performance and congestion

- **R11 Throughput.** Target sustained delivered inner goodput of at least 5 Gbit/s on the specified 2–4-core deployments over routed IPv6 10G-EPON with 4.5–7.5 ms RTT and nonzero packet loss, when contemporaneous underlay capacity supports the encapsulation and repair overhead. Local container tests must also sustain at least 20 Gbit/s in each direction simultaneously at inner MTU 1500, with CPU use reported.

- **R12 Competitive margin.** Substantially outperform WireGuard, SoftEther VPN, and comparable solutions under equivalent host, underlay, MTU, and service conditions. The numerical margin remains an acceptance decision.

- **R13 Congestion control.** Respond to congestion while tolerating some packet loss without disproportionate throughput collapse. Multiple dynamic ports must not bypass aggregate congestion control.

- **R14 Efficient Linux dataplane.** Design for the limited CPU budget, with batching, suitable offloads, bounded memory, and measured queue distribution.

- **R15 Supported-path performance.** Include bridged, OVS, VLAN, and multiple-MAC workloads in validation; host-originated TCP alone is insufficient.

# 3 Proposed scope and terminology

**Proposed initial scope:** one TAP represents one bidirectional point-to-point Ethernet session between two daemons. A daemon can host multiple sessions and peers. Bridging sessions together is the responsibility of Linux bridge or OVS. This keeps transport state separate from Ethernet switching.

A daemon is the process; a peer is a configured remote endpoint. A session is the UUID-identified Ethernet service. A transmit lane is a persistent dynamic-port UDP socket and its packet-accounting state. A path group is the set of lanes believed to share the relevant underlay bottleneck.

An inner frame is the Ethernet frame presented by TAP, excluding FCS and excluding TAP metadata. An outer datagram is one UDP datagram on the wire. A record is an independently parseable item inside its payload. A frame piece is a DATA record containing a byte range of one inner frame.

**Proposed exclusions for the first implementation:** encryption and per-packet cryptographic authentication, Internet NAT traversal, built-in Ethernet switching, compression, a reliable byte-stream interface, and kernel bypass. These exclusions focus the first implementation; packet numbers, duplicate suppression, and robust parsing are still necessary.

This is a custom UDP transport inspired by QUIC's packet acknowledgment, loss detection, and pacing mechanisms. It does not claim QUIC wire compatibility or reuse QUIC stream semantics.

The initial minimum kernel target is Linux 5.0, with offload fallbacks; qualify it with live-kernel tests before declaring release support. The maximum inner frame is fixed at 9022 bytes: MTU 9000 plus a 14-byte Ethernet header plus two VLAN tags, excluding FCS and TAP metadata. Buffer pools, reassembly slots, and TAP reads are sized from this constant; lowering it later is free.

**Library shape:** the library owns its pinned worker threads. It exposes a blocking run function that takes parsed configuration and returns on shutdown, and a control handle for counters, carrier state, and configuration reload. The daemon is the only known caller and is a thin wrapper over this API.

# 4 Socket and peer model

Each endpoint receives on its configured fixed port. It creates a pool of dynamic-port transmit sockets for each destination and keeps their source ports stable while the sockets remain active. The source port is not randomly changed for every packet.

| Traffic | Source socket | Destination socket |
| - | - | - |
| A sends data | A dynamic port i | B fixed receive port |
| B acknowledges A | B dynamic port j | A fixed receive port |
| B sends data | B dynamic port j | A fixed receive port |
| A acknowledges B | A dynamic port i | B fixed receive port |

An ACK carries the identity of the original sender's lane and the packet ranges it acknowledges. Its own source port is unrelated to that identification. No protocol reply is sent to the observed dynamic source port.

The fixed receive port may be implemented by several sockets in a `SO_REUSEPORT` group. One port number does not require one file descriptor or one receive worker. Dynamic source ports provide distinct UDP flows for NIC RSS; RSS queue selection and reuseport socket selection must both be measured. Plain hashing does not guarantee an even distribution. [S3, S4]

Dynamic transmit sockets may use UDP `connect()` for destination selection and error handling. Normal protocol responses still arrive on the fixed receive port. The daemon must separately handle asynchronous socket errors and ICMPv6 feedback associated with transmit sockets. Deployments must permit inbound traffic to the fixed port explicitly; reverse protocol traffic is not necessarily the reverse five-tuple of an outbound datagram.

Configure each receive endpoint's address, port and binding scope explicitly. Endpoints in different VRFs may share a port number; endpoints may also use different port numbers. Apply `SO_BINDTODEVICE` before transmission to the selected interface or VRF; avoid depending on global cross-VRF socket acceptance. [S5]

Resolve FQDNs to IPv6 addresses outside the packet-processing loop. Select and validate a destination address, keep it stable during a path generation, and refresh DNS asynchronously. DNS failure should not immediately tear down a working path. Address changes require path validation and new path measurements. DNS lookup routing must be configured separately if resolution itself must use a particular VRF.

# 5 Session cookie and lifecycle

Carry the configured session UUID as 16 binary bytes in every datagram. Reject duplicate local cookies and unknown cookies before allocating packet state or replying. The `unquic gen-uuid` subcommand generates UUIDv4 cookies using `libc::syscall(SYS_getrandom, ...)` with flags zero; a short read or interruption is an error the operator retries, never a reason to substitute weaker randomness. Provision the cookie out of band and use a public session label in diagnostics. The daemon never generates or replaces the configured session cookie. [S6]

The cookie is a bearer value, without confidentiality or cryptographic payload integrity. An observer who learns it can forge data and feedback. The following handshake prevents accidental stale-state adoption; it does not change that accepted security boundary. [S7]

## 5.1 Connection identity and roles

Roles are elected per attempt; neither endpoint configures a role. Both endpoints may start with HELLO. When two unchallenged HELLO attempts cross, compare their nonzero tags as unsigned 128-bit integers: the lower tag remains I and its sender remains the initiator. The other endpoint abandons its outgoing attempt, generates a fresh R (not its abandoned HELLO tag), and replies with CHALLENGE. This switch retains the original pending deadline. Equal tags abandon the local attempt and require a fresh tag; the send limits still apply. If HELLO arrives before a local attempt exists, accept it as a responder directly.

Election applies only while waiting for the first challenge. Once an endpoint has sent CHALLENGE or accepted one, its pending pair and role are fixed until confirmation or expiry; unrelated HELLOs are ignored. An active connection retains its own role while a replacement is pending, and a replacement may elect the opposite role. Roles control establishment and directional tag selection only; Ethernet traffic is bidirectional.

Use independent, nonzero 128-bit random receive tags, I chosen by the initiator and R by the responder. Reject R equal to I. Established initiator-to-responder packets carry R; responder-to-initiator packets carry I. A connection is the confirmed pair (I, R), scoped by the configured cookie. Tag ordering is used only to break a simultaneous-HELLO tie; it never expresses freshness. Tags are never derived from a clock or persisted. Neither tag of an active pair may be reused by its replacement. A new connection resets packet/frame accounting only after confirmation. On a bound receive socket, admit packets by cookie, wire validity and protocol state, regardless of their source address or source port. Data and feedback must match the current local receive tag; they never establish or replace a connection. Different tags in each direction also reject reflected packets.

Generate tags with `libc::syscall(SYS_getrandom, ...)` using `GRND_NONBLOCK`, handling short reads and interruptions. If entropy is not ready, retry establishment later; report other errors and keep carrier down without a weak fallback. Randomness is needed only for establishment, not per packet. TAP creation/readiness proceeds independently. All durations below use monotonic time; no wall-clock value goes on the wire.

## 5.2 Four-message handshake (wire version 1)

Handshake packets contain exactly one handshake record, a zero receive tag, lane zero, and packet number zero. They are not packet-ACKed. Both endpoints send from dynamic ports to the configured fixed port. Validate the cookie, version, record and handshake state before handling it. Source addresses and source ports are not admission checks. The advertised fixed port must match configuration; a packet cannot redirect replies to an arbitrary endpoint.

1. Either endpoint creates fresh I and sends HELLO(I, initiator parameters); simultaneous attempts use the election rule above.
2. The responder records a pending candidate, creates fresh R, and sends CHALLENGE(I, R, responder parameters). It preserves any active connection.
3. The initiator accepts CHALLENGE only for its pending I, fixes R and the peer parameters, and sends CONFIRM(I, R). A different R or changed parameters for that attempt is ignored.
4. The responder accepts CONFIRM only for its exact pending pair, installs that connection, and sends READY(I, R). The initiator installs it only on READY matching its exact pending pair. DATA arriving before local installation is dropped and can be repaired normally after establishment.

Parameters are fixed receive port (2 bytes), maximum inner frame size (2), mandatory features (4), and optional features (4). All integers are big-endian. Mandatory feature bits are zero in v1; reject unsupported mandatory bits. Optional bits are zero on send and ignored if unknown. The negotiated frame ceiling is the smaller advertised size, which must be between 1514 and 9022 bytes. If the local configured MTU/tag depth requires frames exceeding that ceiling, reject the candidate, report the mismatch, and keep carrier down if no compatible connection is already active. This does not remove TAPs or retract daemon readiness.

There is at most one active connection and one pending handshake per session. A pending attempt lasts 10 seconds without extension by duplicates. The initiator retransmits HELLO every second until it has a challenge, then CONFIRM every second until READY; after expiry either endpoint starts again with fresh I, even if it was responding. An explicitly requested replacement continues until confirmation, including while the old connection remains healthy. For the same pending I and identical parameters, the responder resends its existing challenge, not a new R. While a candidate is pending, other HELLOs are ignored until it expires or commits, except for the unchallenged election above. A changed HELLO body with the same I is invalid. Send the first reply for each new handshake phase immediately. Limit repeated replies and retransmissions of a phase to once per second, and cap all handshake traffic at four datagrams per second per session with a burst of four; suppress excess duplicates.

Remember the active pair and its negotiated parameters. Duplicate CONFIRM for that pair resends READY without resetting packet, frame, or congestion state; duplicate READY is a no-op. At the active responder, HELLO for the active I can only repeat its existing challenge. The active initiator ignores such a reflected HELLO. Duplicate CONFIRM is answered only by the active responder; duplicate READY never changes state. A different HELLO can create a pending candidate but cannot clear active state. After a process restart, lost candidate state is never reconstructed from CONFIRM or READY: a new HELLO must obtain a fresh challenge. Thus an old HELLO plus old CONFIRM cannot install a connection after a restart. Delayed messages for an older pair cannot overwrite a later connection. Loss of READY is handled by repeated CONFIRM. Simultaneous startup and restart use the same bounded tag election. A restarted endpoint never adopts an abandoned HELLO tag as R, so delayed CONFIRM cannot bypass the fresh challenge.

A new connection commit discards the old connection's packet, frame, pending-write, and lane state through a worker barrier. Events and queued transmissions carry their connection tags and are rechecked before use; stale events cannot affect the replacement. Cancel outstanding grants, reconcile all successful sends, and report any remaining unacknowledged old transmissions as lost exactly once before releasing their congestion charges; replacing a connection must not manufacture free inflight credit. A commit must not reset counters under a retained tag. Re-exec without a versioned transport-state handover starts a fresh handshake even if TAP descriptors survive.

## 5.3 Liveness, path changes, and lanes

States are configured, establishing, active, degraded, and inactive. TAP existence determines daemon readiness independently of these states. A confirmed connection becomes active and raises carrier. On an idle connection, send an ACK-eliciting PROBE once per second in each direction. Only an ACK newly covering an outstanding ACK-eliciting packet, or the first matching PROBE_REPLY for an unanswered probe, demonstrates feedback for that direction; repeated old packets do not refresh it. After 3 seconds without such feedback, enter degraded, keep congestion-limited traffic and probes running, and let either endpoint start a replacement handshake. After 30 seconds, enter inactive and lower carrier; discard the active connection using the same worker barrier and congestion-accounting settlement as replacement. Both endpoints continue attempts with fresh election tags. Valid fresh feedback can restore a degraded connection before replacement or inactivity.

Resolve addresses outside workers. Keep the active address stable while it works; a DNS failure keeps it. For a changed address, allow one candidate address alongside the active address and require a new handshake before using it for data. Either endpoint may attempt the candidate. Receive admission remains independent of the sender's address, including for HELLO; replies always use the configured transmit destination. Retire the old address on commit. Do not learn a new destination from an arbitrary packet. Reset path measurements for an address or binding change; a session-only reconnect on an unchanged path must not grant an independent congestion budget.

Lane IDs are 16-bit, nonzero, allocated monotonically per sending direction within a connection, with at most 64 active lanes. Zero is reserved for handshake packets. Never reuse an ID or reset its packet counter within that connection. Start a new handshake before exhausting IDs. Each local transmit lane uses one fixed source-address/source-port/destination/binding tuple. A local transmit tuple change retires that lane. A receive lane is identified by cookie, current receive tag and remote lane ID, independently of the packet's source tuple. Freeze receive reuseport membership while connections are active; changing worker/socket membership requires fresh connections, while keeping TAPs.

On lane retirement, stop sending on it and abandon all its outstanding frames. Retain compact transmit accounting for late ACKs or loss declaration until its congestion charges are settled; retirement does not release inflight credit by itself. Never repair an old frame on a replacement lane. The replacement gets a new lane ID and only new frames. Send LANE_RETIRE naming the old lane through a surviving or replacement lane, with bounded control retries. Reject a retirement record naming its own carrying lane. The receiver marks that lane retired, drops its reassembly and pending TAP writes, and rejects later packets for it. Retired lane bits remain until the connection ends (8192 bytes cover the entire ID space). A lost retirement message cannot cause cross-lane duplicates: old frames are never resent elsewhere, and their own windows and deadlines still apply. If retirement cannot free receiver lane capacity, reconnect rather than reuse an ID.

# 6 Packet and record format

The initial implementation uses wire version 1, replacing the unimplemented epoch-based version 0. Any incompatible future format change increments the version. Do not accept version 0. All integers are big-endian; encode explicitly rather than serializing native structures. Unknown versions, nonzero reserved flags, unknown record types, and malformed lengths are rejected without changing state.

| Field | Bytes | Purpose |
| - | -: | - |
| Magic | 2 | `0x5551` (ASCII `UQ`) |
| Version | 1 | `1` |
| Flags | 1 | Zero |
| Session cookie | 16 | Configured admission cookie |
| Receive tag | 16 | Destination's confirmed tag; zero for handshake |
| Transmit lane ID | 2 | Sender's lane; zero for handshake |
| Record area length | 2 | Exact bytes following the common header |
| Packet number | 8 | Per-lane transmission number; zero for handshake |
| Total | 48 | Common header |

Every record starts with type (1 byte), flags (1, zero), and record length including this prefix (2). The record area must exactly fill the UDP payload after the header, contain at least one record, and have at most 64 records. Validate the entire datagram before dispatch or state changes; a bad trailing record cannot leave a partially applied packet.

The fixed v1 record bodies are below. Sizes exclude the 4-byte record prefix.

| Type | Name | Body |
| - | - | - |
| 0 | PADDING | Zero bytes, any body length; no effect |
| 1 | DATA | Frame ID (8), total frame length (4), byte offset (4), nonempty piece bytes |
| 2 | ACK | Acknowledged lane (2), range count (1), reserved zero (1), ACK delay in microseconds (4), then inclusive packet-number pairs (8-byte start, 8-byte end) |
| 3 | HELLO | I (16), initiator parameters (12); 28 bytes |
| 4 | CHALLENGE | I (16), R (16), responder parameters (12); 44 bytes |
| 5 | PROBE | Empty body; packet number identifies the probe |
| 6 | PROBE_REPLY | Probed lane (2), reserved zero (2), probed packet number (8), received UDP payload length (2), reserved zero (2); 16 bytes |
| 7 | FRAME_ABANDON | Frame's lane (2), origin (1), reason (1), frame ID (8); 12 bytes |
| 8 | CONFIRM | I (16), R (16); 32 bytes |
| 9 | READY | I (16), R (16); 32 bytes |
| 10 | LANE_RETIRE | Retired sender lane (2), reserved zero (2); 4 bytes |

Established packets may mix records except handshake records, which are always isolated. DATA's frame ID is `(lane_id << 48) | frame_sequence`; the low 48 bits start at 1 and increase for each new frame on that lane. Its high bits must match the datagram's lane. Frame sequences never wrap or reset under that connection tag; retire the lane before exhaustion. A frame's total length is 14 through the negotiated frame ceiling. Offsets and lengths must fit that total without integer overflow. Reject inconsistent total lengths for an incomplete frame. Overlaps keep the first retained bytes, as section 8.1 specifies; fully covered pieces are duplicates. Every repair preserves the frame ID and offsets.

Every established physical transmission, including control and ACK-only packets, gets a new packet number starting at 1 on its lane. Retries use new numbers. Never reuse a number after a failed or partially successful batched send; track which allocated numbers actually reached a successful send syscall. Records refer only to the current connection; neither an ACK nor an abandonment record can reach a retired connection through an old tag.

ACK ranges have a count from 1 to 32 and are strictly descending, disjoint, nonadjacent, inclusive ranges with nonzero endpoints. The delay applies to the highest acknowledged packet, measured from reception to ACK construction and capped at 1000 microseconds. Reject future numbers and known unsent holes; ignore portions older than bounded sender history. Process ranges against retained sent entries, never by iterating an attacker-supplied integer span. Account each physical transmission at most once, even if several ACKs or repairs cover it.

A datagram containing DATA, PROBE, FRAME_ABANDON, or LANE_RETIRE elicits an ACK. ACK, PROBE_REPLY, and PADDING alone do not. Schedule feedback after 16 accepted ACK-eliciting packets on a lane or after 1 ms, whichever comes first; a newly detected gap schedules it immediately. Emit at most the newest 32 ranges from the receive window. Duplicate ACK-eliciting packets may trigger repeated feedback, limited to once per millisecond per lane. Missing older ranges can cause a bounded repair, never unbounded ACK history. ACKs may travel through any local transmit lane and name the original sender's lane explicitly.

A PROBE_REPLY echoes the received probe's lane, packet number and actual UDP payload length. Accept it only against a live matching sent-probe entry; do not treat its claims as an ACK for arbitrary data. It validates liveness/size but packet-ACK accounting still occurs through ACK. A PMTU probe contains PROBE plus PADDING and no DATA. Probe records and replies do not create ping-pong loops.

FRAME_ABANDON origin 0 means the frame sender stopped sending; origin 1 means the receiver discarded the frame. Reasons are deadline (1), resource limit (2), lane retirement (3), or TAP failure (4). Its frame ID must name its body lane. Origin 0 must travel on that same sender lane; if the lane has failed, use LANE_RETIRE instead. Origin 1 may travel through any local lane. Route origin 0 to the remote lane's reassembly owner and origin 1 to the local lane's transmit owner. A frame already written to TAP is never undone. Retire pending state and stop repairs. For origin 0, mark the frame sequence terminal even if its first piece has not arrived, advancing the receive frame window if necessary without allocating payload storage. This prevents reordered data from recreating a cancelled frame. For origin 1, unknown or already retired transmit frames are a no-op. Duplicates and sequences below the window floor never reopen state. Control retries use at most two retransmissions and a 100 ms lifetime. Abandonment is advisory: loss of this record does not prevent local expiry or release of buffers.

With a 1500-byte outer IP ceiling and no extension headers, the UDP payload ceiling is `1500 - 40 - 8 = 1452`. One DATA record holds `1452 - 48 - 20 = 1384` frame bytes. A 1514-byte Ethernet frame splits into 1384 and 130 bytes before packing. At the 1280-byte IPv6 baseline, the equivalent frame-piece ceiling is 1164 bytes.

# 7 Packing and segmentation

The transport must segment inner frames itself. Linux must never silently perform outer IPv6 fragmentation. Set `IPV6_DONTFRAG` on every transmit socket, handle `EMSGSIZE` and validated Packet Too Big feedback, and retain a per-path payload limit. Treat 1500 as the target access-line MTU, not a transport-wide ceiling. Start each lane at the IPv6 1280-byte baseline (1232-byte UDP payload) and use RFC 8899 DPLPMTUD to validate larger sizes, including above 1500 where available. Bound probes and data by the wire format and reserved buffer capacities; never allocate a larger packet buffer on the hot path. A local interface or route MTU can bound the search but cannot confirm end-to-end delivery. New paths restart at the baseline. The inner TAP MTU stays independently configured. Keep per-lane state because source-port paths can differ; ACK of ordinary traffic alone cannot validate an unsent larger size. Only a matching padded PROBE_REPLY confirms its size. A validated smaller limit reduces segmentation immediately; an unusable path below the baseline fails clearly. Probe loss never licenses fragmentation. [S8]

Allow one datagram to contain several records, including a frame tail followed by a piece of another frame. Each record contains enough information to recover its frame independently. No frame boundary may depend on successfully parsing a preceding datagram.

For a 1500-byte inner IP packet, the ordinary Ethernet frame is 1514 bytes. With the v1 record layout, sending it alone produces pieces of 1384 and 130 bytes. Packing the short tail with already queued data reduces packet count. The same approach applies to larger inner MTUs.

Packing should use data already available to the worker. Flush promptly when the queue empties; do not wait indefinitely for enough traffic to fill a datagram. Balance batch size against pacing burst size and latency.

Construct UDP GSO buffers as sequences of complete UDP payloads, each with its own common header. All but the final segment in a GSO submission have the configured segment size. GSO does not create custom protocol headers. A partially filled tail ends that submission or is sent separately; avoid wasteful padding merely to force batching.

For fresh DATA, end planned batches on frame boundaries within the GSO aggregate byte limit. Dropping a local aggregate then drops whole groups of frames rather than leaving orphan pieces in neighboring aggregates. This does not change per-datagram recovery on the routed path or permit outer IP fragmentation.

A missing datagram may contain pieces of several frames. Recover its still-needed content selectively and keep completion independent across frames. Packing must not become a connection-wide ordered stream.

# 8 Loss recovery and ordering

Use selective repair of missing byte ranges with bounded persistence. Moving fragmentation into UDP does not by itself prevent split-count loss amplification: this repair mechanism must recover missing pieces, rather than routinely losing whole frames when one piece is lost. Keep original payloads until every range is acknowledged, abandoned, or expired; an ACK means transport acceptance, not successful TAP or application delivery. Repairs retain frame identity but get new packet numbers. At most two repair transmissions may cover any byte range, and do not repair while another known outstanding copy already covers it. Whole-frame delivery is best effort; unrelated frames never wait for it. [S1]

## 8.1 Acceptance, delivery, and pressure

Validate the entire datagram, including every frame length, before retaining any DATA. Packet ACK eligibility starts only after all nonterminal DATA bytes are retained by the reassembly owner. If new frames cannot obtain slots, retain validated pieces for existing frames without allocating new slots, advancing packet/frame windows or applying control records. Drain completed frames to TAP and retry the same datagram once when slots were freed. If it still cannot fit, discard its unretained pieces without ACKing or marking the packet received. Frame coverage and terminal history suppress duplicate delivery on retry. A full handoff queue drops the datagram without ACK. Never evict an unrelated accepted frame to admit a new packet.

DATA for a frame already delivered, abandoned, expired, or below the frame window is terminal: discard it without allocating or writing to TAP again. Such duplicates can be packet-ACKed; when the frame was discarded rather than delivered, schedule a bounded FRAME_ABANDON if still useful. Packet ACKs never promise reliable frame delivery. A receiver may later abandon previously ACKed pieces at the frame deadline or on a TAP error and reports this when possible; it never retracts packet ACKs.

Overlapping DATA keeps the first retained byte at each offset. A changed total frame length rejects the whole datagram before applying records; later pieces cannot resize accepted storage.

A complete frame enters a bounded TAP-write queue once. While queued it remains ineligible for a second enqueue. A successful full-frame write marks it delivered; `EAGAIN` retains that same entry until writable or expired. Other write failures, including an unexpected short write, discard it and mark it terminal; never retry a partial frame as another Ethernet frame. Expiry or abandonment of an incomplete/queued frame also marks it terminal before releasing buffers.

## 8.2 Bounded duplicate history

Each active receive lane has a 65536-packet sliding window and a separate 65536-frame-sequence window. Each window has a monotonically increasing lower acceptance bound. Advance it when a validated, admitted higher number would exceed the window; numbers below the lower bound are permanently old until this connection ends. Discard incomplete or queued frames displaced by the frame bound and report abandonment when possible. Keep terminal bits for delivered/expired/abandoned frames inside the window. Old packets are never made new by a timer expiring or a ring slot being reused. A delayed repair with a new packet number still fails its old frame's terminal/window check.

Do not deduplicate using only a time-limited completion cache. Window advancement may discard unusually reordered traffic; this is bounded loss, never duplicate Ethernet delivery. Only the owning worker changes these windows. Retired-lane bits reject the whole lane before frame lookup, so its windows can be freed safely. For still-active lanes, preserve the lower bounds even through long idle periods.

Initial resource limits are 8192 retained transmit frames and 8192 receive frames (incomplete plus queued complete) per session, with 64 MiB of payload storage per direction and at most 64 active lanes per direction. The daemon's default total transport-memory ceiling is 256 MiB, including metadata, histories, and queues; caller-provided ceilings may raise or lower it. Refuse reservations beyond either limit. The sender also keeps at most 65536 sent-packet entries per lane; if advancing would overwrite an unresolved entry, apply backpressure rather than forget its inflight charge. On startup validate fixed allocations against the ceiling. Unknown cookies allocate nothing; pending handshakes, lane tables and worker queues are bounded separately. These are initial implementation limits, not performance claims.

## 8.3 Timers and initial ordering policy

Use RFC 9002 sections 5 and 6 for RTT estimation, per-lane packet/time loss detection, and probe timeouts, with these transport parameters: initial RTT 333 ms, maximum ACK delay 1 ms, timer granularity 1 ms for loss detection, packet threshold 3, and time threshold 9/8. Pacing has its own finer schedule. ACK delay cannot reduce a sample below observed minimum RTT. Apply packet thresholds only within one lane, never across unrelated packet-number spaces. [S9]

At frame admission, set its sender deadline to `now + clamp(4 * smoothed_rtt, 20 ms, 100 ms)`. The receiver deadline is 100 ms after the first retained piece and is not extended by repairs or duplicates. The two-repair limit and the deadline both apply. Deliver completed frames immediately in v1; do not add a per-flow reorder queue before measurements justify one. Pin each inner flow to a lane, with a fallback key of MAC addresses, VLAN stack, and EtherType for non-IP or unparseable traffic. Fragmented IP traffic uses a consistent address/protocol key without transport ports for all fragments. A lane change sends only new frames.

An individual PTO sends at most two probe datagrams per path group, with exponential backoff; one PTO does not declare all outstanding traffic lost. Retiring a frame does not erase the congestion charge for its outstanding packets. Free payloads at their deadlines but retain packet accounting until ACK or loss. If feedback is absent for three consecutive PTO periods, treat remaining outstanding transmissions as lost once and tell BBR; continue only backed-off probes until feedback resumes. Reset the PTO backoff on newly acknowledged ACK-eliciting traffic. The aggregate controller, not each lane independently, grants probe allowance. These at-most-two probes may exceed the congestion window when it is full, as in RFC 9002, but are still charged to inflight bytes and paced; no other packet bypasses the window this way.

LANE_RETIRE and FRAME_ABANDON retries consume ordinary transmission credit and stop at their own limits. Neither controls the correctness of local duplicate suppression. FEC and intentional per-flow reordering remain later measured experiments.

# 9 Congestion control and pacing

Implement BBRv3 directly in-tree for the first production controller, pinned to draft-ietf-ccwg-bbr-07. Use that revision's state machine, estimator updates, gains and loss responses; do not substitute a simpler loss controller or silently follow a newer draft. The reference remains experimental, so performance and coexistence still require measurement. [S11]

Use one controller owner per sending direction and path group, shared by every session and lane in that group. A peer names its configured path group; default to one group per binding scope so adding peers or ports does not multiply the access-link budget. Separate groups require explicit configuration for separate bottlenecks. Lane/session replacement within an unchanged group does not reset or duplicate its controller. Address/binding changes discard stale path measurements through that owner.

Charge outer IP bytes, including IPv6 and UDP headers, for each successful physical transmission of data, repair, probes, abandonment, and lane retirement. Set the controller's maximum segment size from the effective outer IP ceiling and its initial window to ten such datagrams. Use BBRv3's UDP/QUIC-style offload budget and its delivery-rate sampling rules, with shared delivered-byte counters and per-transmission send metadata. ACK duplicate suppression must precede sampling. A repair and its late original are distinct delivered transmissions for bandwidth accounting, while their frame bytes are delivered only once. Keep application-limited and host-queue-limited observations distinguishable. [S11]

The controller owner serializes send/ACK/loss reports from lane owners and issues byte grants covering both pacing and inflight limits. Reserve inflight credit before granting it. Grants expire for future sends after 100 microseconds, but reserved credit is not reusable until the worker reports sent and unused bytes; expiry alone must not double-spend it. Failed/partially successful `sendmmsg` submissions return only unsent credit, and each successful GSO segment is a separate transmission. Bounded report queues must retain accounting events: a full queue stops further grants/sends until reports drain, rather than dropping inflight updates.

Initially pace in userspace using monotonic deadlines and `timerfd`, without requiring an `fq` qdisc or host networking changes. Workers send only with current grants. Cap the combined outstanding burst allowance at the smaller of BBR's send quantum and 16 maximum-size datagrams, spread across workers; GSO aggregates obey that limit too. Timestamp at successful nonblocking socket submission and bound queued socket work. This is a testable baseline; kernel pacing remains a measured optimization. [S11]

ACK-only and PROBE_REPLY traffic can proceed without data-window credit to avoid feedback deadlock, but share a separate 4 MiB/s ceiling per path group with a 16-datagram burst. Coalesce pending ACKs and discard stale feedback when bounded queues fill; do not invent delivery progress. Handshake messages use the per-session one-per-second retry/reply limits and this same control ceiling. This is a ceiling, not a fixed send rate. All these bytes remain visible in counters. Worker/controller scheduling must give bounded service to feedback, fresh frames, and repairs; sender queue expiry drops whole unsent frames where possible.

BBRv3 receives receiver-pressure losses as well as network losses; no static 5 Gbit/s pacing rate is inferred from deployment measurements. Keep outer ECN disabled and outer DSCP zero in v1, preserving inner fields. ECN needs an explicit wire feedback extension before enabling it. Controller-disabled modes are confined to isolated experiments and cannot pass production acceptance. The real-NGN throughput and latency profile, including random and burst loss and competing traffic, remains a release gate.

# 10 Linux dataplane

Create Ethernet TAP devices using `IFF_TAP`, `IFF_NO_PI`, and multiqueue support. Internal `IFF_VNET_HDR` metadata permits byte-exact local USO writes without changing wire frames or requiring peer negotiation. Keep read offloads disabled until partial checksums and segmentation are implemented; validate and remove metadata before transmitting any frame. A wire representation of inner offloads remains a negotiated extension. Configure offloads explicitly and record their effective state. [S2]

**No async runtime.** The dataplane is plain threads over `std` and `libc`, not `tokio` or another async runtime. The workload is a handful of always-busy sockets and one TAP fd, not many idle connections, so readiness notification and task scheduling are pure overhead. Tokio's UDP API issues one syscall per packet and has no `recvmmsg`, `sendmmsg`, UDP GSO, or UDP GRO support; using those means dropping to `AsyncFd` plus `libc`, leaving the runtime as nothing but an event loop. Its work-stealing scheduler moves tasks between cores and defeats the one-worker-owns-its-buffers model below, and its 1 ms timer granularity is too coarse for pacing at hundreds of thousands of packets per second. Tailscale's WireGuard work and quinn's UDP layer both ended up on raw batched sockets for the same reasons. [S18] Workers are pinned threads that wait in `epoll_wait()` on sockets, TAP queues, `eventfd`, and `timerfd`; receive calls are nonblocking and batched. Section 9 fixes userspace pacing as the baseline.

Hot paths must not allocate or take locks. Keep protocol state under one worker's exclusive ownership and exchange work through bounded queues. State transitions use supplied monotonic timestamps and make no syscalls. Obtain time and randomness outside those transitions; randomness is needed only for establishment. Test allocation counts and measure state-transition costs before claiming speedups.

Minimize hot-path syscalls with preallocated buffer pools, bounded descriptor rings, nonblocking I/O, `sendmmsg()`, `recvmmsg()`, UDP GSO, and UDP GRO where supported. Batch queue notifications and arm timers only when the next deadline changes. Use fewer system calls and fewer full stack traversals per byte. GSO and GRO fallbacks must preserve correctness when an interface, driver, or binding scope cannot support them. [S12]

On UDP GRO receive, split the buffer into its logical datagrams using the returned segment metadata and validate each datagram, including its cookie. A coalesced receive buffer is not one large protocol packet.

Keep frame buffers and reassembly slots owned by one worker. Retain references for repair and use scatter/gather I/O where it avoids extra payload copies. Measure whether packing into contiguous buffers or gathering slices is faster on target machines. Ordinary TAP and socket I/O still involves kernel copies.

Start with a small worker count and several stable transmit lanes. Measure NIC queue counters, reuseport distribution, softirq CPU, and cross-worker traffic. Optional reuseport steering can align session/lane ownership with receive workers, but must not route all traffic for a session to one core merely because the cookie is shared. [S3, S4]

**Worker ownership and routing (v1).** A worker owns each transmit lane's packet state and each receive lane's reassembly, frame windows, and TAP-write queue. Keep an explicit connection-tag/lane-to-owner table; reuseport hashing is a delivery optimization, not the source of protocol identity. First reception registers a remote lane with the session control owner before admitting its data. Assign its owner as `cookie_low32.wrapping_add(u32::from(lane_id) - 1) % session_queue_count`, using the cookie's low word in network byte order, so consecutive lane IDs and single-lane sessions spread across workers independently of reuseport collisions; bound pending registration buffers and drop excess without ACK. Bind each endpoint's entire reuseport group on the control worker in worker-index order, then transfer descriptors through bounded control queues. A failed bind closes the partial group before retrying. Classic reuseport BPF uses the same wrapping sum modulo the global worker count; failure retains ordinary hashing. Registration, handshake packets, mixed-cookie GRO and sessions with fewer queues retain the forwarding fallback. Do not bind receive lane identity to a source tuple or reject packets because that tuple changed. Check retirement before allocation. Established DATA normally remains on its registered receive worker. If a packet arrives elsewhere, forward the complete datagram to that owner rather than creating another reassembly table. ACK and origin-1 abandonment go to the named local transmit owner; origin-0 abandonment and retirement go to the remote receive owner. Validate all records before dispatch, and apply datagram acceptance on the receive owner.

Use one bounded SPSC queue per ordered producer/consumer worker pair, initially 1024 entries each. The local-owner case needs no queue. Batch handoffs and signal the destination's nonblocking `eventfd` after publishing each batch; `EAGAIN` means a wakeup is already pending. The destination drains its eventfd and all incoming queues before sleeping, so enqueueing work cannot rely on unrelated socket traffic to wake it. A full DATA queue drops the datagram without ACK; a full ACK queue may drop feedback and let bounded recovery handle it. Control commits/retirements and congestion-accounting reports must be retried with backpressure, never silently dropped. Workers keep draining inbound queues while retrying; they must not block waiting for space in each other's queues. Tag every queued event with its connection identity and discard stale events after a commit barrier. Queue ownership remains single-producer even with four workers; no queue accepts writes from two producers.

There is no encryption or per-packet cryptographic authentication pass. The implementation should not introduce transformation buffers or worker handoffs that exist only to support such a stage. Preserve ordinary UDP checksums and use hardware/kernel checksum support.

Avoid assuming every frame is host-generated TCP. Forwarded Ethernet frames, inner IPv6 extension headers, non-IP payloads, and unavailable offloads must remain supported.

**Local TAP coalescing.** Within a bounded receive flush, combine compatible complete UDP frames into a USO write by borrowing their payloads. Validate the original checksums and exact header/length relationships; IPv4 requires no options, DF and consecutive IDs, while IPv6 permits no extension headers. Preserve per-flow order, including barriers at incompatible frames in that flow. Keep every selected slot on transient write failure and release only the selected slots on success or permanent failure. Do not wait for more frames to make a larger group. Disable USO on a queue after EINVAL/EOPNOTSUPP and retain ordinary writes. This optimization changes neither packet admission nor the 100 ms receive deadline.

**Experimental extension:** transport a negotiated, normalized representation of supported inner GSO and partial-checksum metadata, reconstructing it at the destination TAP. This could avoid segmenting and later coalescing inner packets in userspace. It is not a prerequisite for correctness or the first interoperable format. Validate metadata, cap aggregate size, and test bridge/OVS forwarding and loss behavior. Never interpret an ordinary oversized Ethernet frame as a GSO object. [S13]

# 11 Ethernet device behavior

Preserve source/destination MAC addresses, EtherType or length fields, payload, VLAN tags, and frame boundaries. Do not rewrite MAC addresses to the TAP's own address. Support IEEE 802.3/LLC traffic such as IS-IS and normal multicast and broadcast behavior.

Support 802.1Q and 802.1ad tags, including stacked tags and PCP/DEI bits. The Linux TAP path can materialize a VLAN tag from skb metadata into the bytes returned to userspace; tests must exercise this path rather than assuming all tags were already in-line. [S14]

Distinguish inner interface MTU, ordinary Ethernet-frame size, and an offloaded GSO object. MTU 2400 corresponds to 2414 bytes without VLAN tags and 2422 bytes with two tags, excluding FCS. Account separately for TAP metadata. Negotiate and enforce frame and aggregate limits; v1 supports MTU up to 9000 and up to two VLAN tags within its 9022-byte frame ceiling.

The daemon must allow Linux bridge or OVS to own forwarding policy. It need not implement an FDB, STP, or VLAN access/trunk policy. Reserved link-local traffic is transported if delivered to the daemon; bridge/OVS policy may consume or filter it before that point. [S15, S16]

The following independent configurations should work with existing bridge devices:

| Attachment | Example |
| - | - |
| Linux bridge | `ip link set tap0 master br0` |
| OVS system port | `ovs-vsctl add-port ovs0 tap0 -- set Interface tap0 type=system` |
| VLAN upper | `ip link add link tap0 name tap0.100 type vlan id 100` |
| Macvlan upper | `ip link add link tap0 name mv0 type macvlan mode bridge` |

These examples do not imply that all attachments can coexist on the same lower device. Preserve normal Linux stacking constraints. The daemon retains the TAP file descriptors when OVS uses it as a system port. [S16, S17]

**TAP creation decisions:** accept `Option<MacAddress>` for the interface MAC.
`MacAddress` is an in-tree checked type that rejects the all-zero address and
multicast addresses, including broadcast. Both globally and locally administered
unicast addresses are valid. `Some(mac)` sets that address during setup.
For `None`, read the current UTS hostname with `gethostname` and hash the exact
bytes `<hostname>\0<interface name>` using in-tree SHA-256. Include one NUL
separator, no trailing NUL, and no case normalization. Map the first three digest
bytes `h` to `[0x60, 0xa4, 0x34, 0x40 | (h[0] & 0x0f), h[1], h[2]]`, within
Menhera's `60:a4:34:4x:xx:xx` MAC-M range. Hostname lookup failure aborts creation without a
random fallback; an explicit MAC bypasses lookup. The 20-bit suffix can collide,
and identical name pairs produce identical addresses. Configure explicit MACs
to resolve duplicates on the same Ethernet network. Name changes affect future
creation only; existing devices retain their MAC.

The library sets the configured MTU and chosen MAC but never brings the
interface administratively up. The caller or a network manager such as
systemd-networkd or ifupdown owns administrative
state, addressing and bridge membership. Carrier remains under transport
control and is independent of administrative state. Queues can move out of the
`Tap` into their owning workers; any queue can set carrier for the device. The
interface MAC does not replace source or destination MACs in transported frames.

Keep TAP names, MAC configuration, and device identity stable across ordinary peer reconnects. Session failure changes carrier according to a defined policy; it does not delete the interface or its VLAN/macvlan children.

**Future restart support:** retain and hand over TAP queue file descriptors across daemon re-exec so the TAPs and their existing attachments survive. Follow rs-l2tpd's descriptor-preservation approach: it preserves L2TP tunnel sockets, while UnQuic would preserve TAP descriptors. Optional systemd descriptor storage may also retain descriptors across a crash for the next process to adopt. The daemon handles descriptor announcements, environment variables, and re-exec; the library receives explicitly supplied, validated descriptors. Define and version the handover format before implementing adoption. Preserving TAPs does not itself preserve userspace transport state or guarantee uninterrupted forwarding.

# 12 Configuration and operations

Separate daemon-wide resource limits, named receive endpoints, peer transport configuration, and per-session Ethernet configuration. One peer may carry multiple sessions; shared lanes or separate lane pools must still observe the appropriate aggregate congestion budget.

An illustrative configuration shape follows. Names and default values are proposed, and the cookie is a placeholder. The file is TOML, parsed with `serde` and `toml` under the `bin` feature; the library core takes already-parsed values and never touches the file.

```toml
[[endpoints]]
name = "default"
listen_address = "::"
listen_port = 40000
bind_device = "vrf-ngn"

[[peers]]
name = "peer-a"
endpoint = "default"
host = "peer-a.example.net"
peer_port = 40000
tx_lanes = 4

[[sessions]]
name = "ngn-a"
peer = "peer-a"
cookie = "<uuid-v4>"
tap_name = "tap-ngn-a" # optional; defaults to the session name
inner_mtu = 2400
# mac = "02:00:00:00:00:01" # optional; omit to derive from hostname and TAP name
```

The core configuration and transport use raw IPv6 addresses. The optional `dns` layer depends on exactly `iphost v0.4.4` and uses its `IpHost`/`FQDN` types and resolver; do not duplicate hostname parsing. It supplies resolved IPv6 candidates to the core from a helper thread. The daemon enables this layer for its TOML host fields. A core peer may initially have no address, so TAP creation and readiness never wait for DNS. Hostname values and resolver locks/allocations stay outside packet-processing workers. Each peer references a named endpoint containing its local listen address, fixed port and optional binding device. The daemon creates a receive socket set per endpoint; different endpoints may use different ports or VRFs. Peer `host` and `peer_port` specify the remote destination. See [config-model.md](config-model.md) for the configuration shape. Define scope identifiers for link-local literals if those addresses are supported. Cookie changes are explicit administrative operations; do not silently replace a configured cookie during reconnect.

Resolver selection must be configurable. `iphost`'s `ipv6`, `ra`, and `dhcpv6` modes bypass system resolvers and must work without `/etc/resolv.conf`. Using FQDNs or enabling the `dns` feature does not imply use of the system resolver. Do not silently fall back to the system resolver when a mode that bypasses it was selected. IPv6-literal endpoints require no resolver.

Expose per-session, per-peer, and per-lane counters for delivered frame bytes, outer bytes, repairs, duplicates, expired frames, queue drops, reassembly occupancy, RTT, feedback delay, pacing rate, bytes in flight, and path-MTU state. Distinguish malformed/unknown-cookie packets from operational loss without logging secrets. Keep counter sets with their worker owners and publish batch totals where possible. Updates use Relaxed atomics and control reads use Acquire; snapshots sample fields independently and never serve as synchronization for transport state. RTT and feedback delay use nanoseconds, pacing uses outer IP bytes per second, and byte/occupancy gauges state their units.

Record kernel version, CPU model, active cores and frequency, NIC/driver, queue mapping, offload state, binding scope, and effective socket-buffer limits in diagnostic output. Include socket overflow and softnet/NIC drops in performance investigations.

Bound every queue, ACK history, incomplete-frame table, completion cache, and retained transmit-buffer set. Define behavior on allocation pressure, device removal, interface renaming, DNS failure, path change, and daemon restart. Configuration reload must preserve unrelated sessions.

Keep privileges limited to what is needed to create/configure TAPs and bind sockets; allow attaching to appropriately provisioned devices. The daemon must run directly without a service manager. Future optional systemd integration may implement notification and descriptor-store protocols directly over Unix sockets with descriptor passing, without linking against systemd or D-Bus libraries. Keep this integration in the daemon and preserve the library I/O boundary. DEB/RPM packaging may supply service units and distribution-specific installation hooks; these must not become prerequisites for standalone operation. The exact privilege model is an implementation decision.

## 12.1 Startup readiness and optional systemd unit

Readiness means that all configured TAP interfaces exist, whether newly created or adopted through descriptor handover. Report readiness exactly once after this condition holds. Do not report success while any configured TAP is missing. TAP carrier may remain down; readiness does not require DNS resolution, an underlay address or route, a binding interface or VRF to appear, a peer handshake, or working forwarding. TAP creation and adoption must therefore proceed independently of underlay socket setup and peer establishment. Later session degradation changes carrier and diagnostics, not the completed startup readiness signal.

Follow rs-l2tpd's daemon startup model: double-fork, let the original parent wait for the daemon's readiness report, and exit that parent successfully only after all configured TAPs exist. If startup fails or the daemon dies before readiness, the parent reports failure. Any init system, supervisor, or launcher can wait for this parent exit status; daemon operation and this readiness signal do not require systemd or a notification service. Publish the daemon PID file before the parent reports success. Keep process management, PID-file handling, and readiness reporting in the daemon; the library exposes the TAP state needed to determine readiness without using files, environment variables, or stdio for that report. Foreground operation remains available for launchers that supervise the process directly.

The future optional systemd integration consumes this same parent-exit readiness signal. When used, the embedded unit and packaged copies must share one template: `Before=network.target network-online.target` in `[Unit]`, `RequiredBy=network.target network-online.target` in `[Install]`, and `Type=forking`, `PIDFile=`, `TimeoutStartSec=infinity`, and `Restart=on-failure` in `[Service]`. Enabling the unit must install both targets' requirements on the service, so startup through `network.target` alone also pulls in the daemon. The service must not require, want, or be ordered after either target or any wait-online service. Both targets must wait for the configured TAPs to exist; the daemon must not wait for connectivity that may itself depend on those TAPs.

# 13 Performance budget

Use delivered inner bytes per second as the headline result and report the precise measurement layer. Record outer IP bytes and repair/control bytes separately. Do not compare TCP payload throughput with Ethernet or outer-wire throughput without conversion.

For illustration, 5 Gbit/s of inner IP traffic requires the following rates when packets are full-sized. MTU 2400 is an example larger-MTU test point, not an operator-specified requirement:

| Inner MTU | Inner frames per second | Outer packets per second with two per frame |
| - | -: | -: |
| 1500 | 416667 | 833333 |
| 2400 | 260417 | 520833 |

Assuming packing delivers an average of 1350 inner IP bytes per outer packet, the outer rate would be about 463000 packets/s. This average is a planning assumption below the 1384-byte frame-data ceiling for a 1500-byte outer path; actual efficiency depends on Ethernet overhead, frame sizes, record boundaries, and partial batches.

At 5 Gbit/s and RTT 4.5–7.5 ms, the bandwidth-delay product is approximately 2.81–4.69 MB per direction. Retaining 20 ms of payload for recovery would require about 12.5 MB per direction before metadata and copies. Bounds should scale with the configured memory ceiling and measured path, not with arbitrary tiny packet counts.

Two 2 GHz cores provide approximately 8640 aggregate CPU cycles per outer packet at 463000 packets/s. That budget includes their kernel and userspace processing and is not evenly available if a queue concentrates work on one core. Full-duplex 5 Gbit/s in each direction roughly doubles packet work.

At an assumed 90 percent effective encapsulation efficiency, delivering 5 Gbit/s needs about 5.56 Gbit/s of outer traffic. Adding repair traffic equal to 10 percent of original transmissions raises this illustrative demand to about 6.11 Gbit/s. These are arithmetic budgets, not measured results.

The local container acceptance requirement is at least 20 Gbit/s of delivered inner goodput in each direction simultaneously at inner MTU 1500, with CPU use reported. This requirement provides processing headroom for the 5 Gbit/s real-access target. Both require measurements through the complete transport; codec or socket microbenchmarks cannot establish either result. Existing WireGuard implementations already use substantial batching and offload optimization. [S18]

# 14 Validation and acceptance

Freeze a repeatable acceptance profile before declaring the performance requirements met. It must name the endpoint hardware and core budget, inner MTU, frame mix, flow count, traffic direction, attachment topology, loss profile, and contemporaneous direct-underlay capacity.

Functional tests need only what any Linux host or container can provide: a kernel at or above the supported minimum, `/dev/net/tun`, and `CAP_NET_ADMIN` in the test's network namespace. There is no reference environment; tests must create every interface, namespace, and route they use and remove them afterwards, so they run the same on any contributor's machine.

## 14.1 Functional gates

- **V01 Transport invariants:** capture outside the sending host's offload boundary and verify IPv6 UDP only, packet sizes within the limit, no Fragment headers, all destinations at fixed receive ports, and no protocol transmission from a fixed receive port.

- **V02 Session correctness:** test valid/unknown cookies, duplicate configuration, malformed records, bad ACK ranges, duplicate and reordered datagrams, late originals after repairs, and no duplicate Ethernet delivery.

- **V03 Lifecycle:** test random-tag establishment, delayed old packets, lane replacement, loss of all feedback, peer reboot, DNS address changes, and persistent TAP reattachment. Without systemd, verify that the original parent waits while any configured TAP is missing, exits successfully once all exist despite unavailable DNS, underlay bindings, routes, or peers, and reports failure if startup fails or the daemon dies before readiness. For optional systemd integration, verify that the enabled embedded/packaged unit makes both `network.target` and `network-online.target` require and wait for readiness, including startup through `network.target` alone, without reverse dependencies or wait-online dependencies.

- **V04 Ethernet composition:** test Linux bridge, OVS system ports, VLAN/QinQ, macvlan, multiple MACs, multicast, broadcast, and non-IP/LLC traffic. Verify frame contents and MTU handling.

- **V05 Binding and offloads:** test ordinary interface and VRF binding, lower effective PMTU, software fallbacks, GSO/GRO boundaries, VLAN metadata, partial checksums, and receive truncation.

- **V06 Resource behavior:** stress reassembly limits, loss bursts, ACK-history limits, queue backpressure, and sustained receiver overload. Verify bounded memory and recovery without corrupting unrelated sessions.

### Protocol and ownership regression cases

The implementation must exercise these cases with deterministic loss, reordering, clock-independent randomness fixtures, and bounded queues. The real handshake and reassembly implementations are exercised in `tests/handshake.rs` and `tests/reassembly.rs`; the earlier test-only model has been removed. These tests complement the socket/TAP container tests.

- Lose each handshake message, duplicate every message, start both endpoints together with either tag order and equal-tag collisions, restart either or both endpoints, replay old HELLO/CONFIRM/READY after a newer connection, and change the wall clock. Only the current pending exchange may commit; duplicate confirmation never resets active counters. A pending attempt must not evict working data state.
- Deliver valid-cookie, current-tag packets from changed source addresses and
  ports on the bound receiver; they must pass admission without changing the
  configured transmit destination. Unknown cookies and stale tags still fail.
- Confirm the responder, lose READY, and resend CONFIRM. The responder repeats READY without reinstalling state; the initiator eventually commits. If a responder restarts between challenge and confirmation, the old confirmation is rejected and a fresh attempt succeeds.
- Deliver FRAME_ABANDON before a frame's first piece and verify that reordered data cannot recreate it. Deliver a frame, lose its ACK, retire its lane, then deliver old pieces and attempted repairs on another lane. The frame reaches TAP once. After the frame window advances or a lane retires, even a repair carrying a new packet number cannot recreate it.
- Fill reassembly, TAP-write, and handoff queues. Unreserved datagrams are not ACKed; accepted frames either complete once or become terminal. Expired completion bits never reopen sequence numbers below a window floor. Exercise memory ceilings with many sessions and lanes.
- Route feedback from three workers to one owner, wake an otherwise idle owner, fill queues, and commit a new connection with old events still queued. Verify the per-pair producer contract, no lost wakeups, and no stale state mutation.
- Feed BBR duplicate ACKs, ACK loss, reordered ranges, late originals after repairs, sender stalls, receiver drops, and partial GSO/sendmmsg completion. Assert that reservations plus actual inflight bytes respect the shared budget and that no session/lane replacement multiplies it. Pin deterministic controller vectors to draft 07 before optimizing.

## 14.2 Performance comparisons

Compare the direct underlay, existing kernel L2TPv3 with fragmentation, the new transport, kernel WireGuard carrying an equivalent L2 service, and an appropriately configured SoftEther deployment. Record exact versions, acceleration settings, and encapsulation stacks. Plain WireGuard L3 may be an additional diagnostic baseline but is not the equivalent-service comparison.

Use the same endpoints, CPU/core budget, inner MTU, offered traffic, and underlay conditions. Do not disable a competitor's normal acceleration. Preserve supported inner MTUs rather than obtaining the result by silently reducing them.

Test MTU 1500 and proposed larger test points such as 2400 and 9000 if supported. Include small-frame and mixed-size traffic, single and multiple inner flows, both one-way directions, simultaneous bidirectional traffic, and bridged/OVS traffic from separate traffic generators.

Measure on a controlled link with approximately the observed RTT and reproducible independent and burst loss, then on real NGN paths. Record real loss and loaded delay before selecting impairment parameters. Host-side loss injection must be placed so it operates on intended on-wire packets rather than accidentally dropping entire offload aggregates.

**Proposed measurement procedure:** at least five runs with at least 60 seconds of steady-state measurement per profile; retain cold-start results separately. Measure the direct underlay before and after real-network runs. Use longer runs where shared-access variation is material.

Report goodput, total endpoint CPU including softirq, per-core load, p50/p95/p99 latency under load, frame loss/expiry, repair overhead, memory, and queue drops. A throughput gain accompanied by excessive queueing must remain visible.

**Required throughput:** local container tests must sustain at least 20 Gbit/s in each direction simultaneously at inner MTU 1500 and report CPU use. The real-access target is at least 5 Gbit/s over routed IPv6 10G-EPON with 4.5–7.5 ms RTT and nonzero packet loss. Record the achieved rate for each direction separately; an aggregate local result of 20 Gbit/s does not pass. Measure delivered inner goodput, excluding duplicates and repair/control bytes, and state the measurement layer. Local results do not establish real-access performance.

**Pass criteria still to freeze:** local test hardware, core budget and flow count; real-access flow count and direction; maximum acceptable loaded latency; and representative random and burst loss profiles. “Nonzero loss” must become a reproducible rate and distribution before acceptance. The numerical definition of a large competitive margin also remains open; a tentative 1.5-fold throughput advantage is a proposal.

If a comparison already reaches the available underlay ceiling, report that fact and CPU cost at equal goodput. Do not claim a large throughput margin that the measured path cannot demonstrate.

# 15 Implementation stages

**Current status:** stage 1 is complete; stage 2 is in progress. Implemented foundations are
`UnquicUuid` parsing/provisioning, duplicate-checked cookie indexing,
`MacAddress` validation, IPv6 sockets with separate receive/transmit APIs, and
nonpersistent multiqueue TAP creation and frame I/O. The version-1 wire codec
validates complete datagrams before dispatch and has fixed byte vectors for all
eleven record types in tests/vectors/wire-v1. TAP creation accepts an
optional MAC and leaves administrative state unchanged. Tests cover the public
types, loopback socket invariants and live TAP behavior in an isolated network
namespace; see README.md for commands and coverage limits. The pure handshake
state machine implements automatic role election, bounded retries, negotiated
parameters, replacement and liveness transitions using supplied monotonic times.
The runtime now applies commit and carrier events through the worker barrier. Configuration
validation now freezes named endpoint/peer/session relationships into numeric
indices. Cookie lookup uses short scans for small sets and randomized hashing
for larger sets, with an allocation check and repeatable benchmark. The optional
iphost resolution layer supplies IPv6 candidates above the raw-address core.
Atomic counters now cover the section 12 totals and gauges with explicit units
and allocation-free snapshots. The fixed SPSC worker handoff ring has concurrent,
wraparound, ownership and Miri tests. A pure two-phase commit barrier tracks
quiescence and installation, with full-queue and stale-acknowledgment tests.
The receive foundation now has fixed frame storage, whole-packet ACK admission with existing-frame progress under pressure,
permanent packet/frame windows, expiry and a retained TAP-write queue. Its tests
replace the frame model, including pressure, repair and allocation checks.
Baseline segmentation and inner-flow selection now cover the full frame-size
range, VLANs and IPv4/IPv6 fragments, with allocation and exhaustion tests.
Batched socket APIs use recvmmsg and scatter/gather sendmmsg, with original
frame slices, partial-completion tests and allocation checks. The owned worker
runtime now forwards frames between TAPs for configured or supplied IPv6 peers.
Its container test covers two workers/four transmit lanes per endpoint,
bidirectional 60/1514/1518/9022-byte frames, kernel VLAN tag materialization,
carrier loss and reconnection after restart. Forwarding checks observe no
allocations or deallocations. Readiness, buffer ownership, memory reservations
and remaining runtime gaps are documented in README.md. Candidate-address
updates preserve active paths through fresh handshakes and worker barriers.
Tests cover failed candidates, migration back, unchanged destinations, multiple
peers/sessions and independent endpoint readiness. Workers publish per-session,
per-peer and per-lane counters in batches; lane snapshots include the local
connection generation. README.md defines byte accounting, lifetime, aggregation
and currently unsupported gauges. Socket APIs now support caller-supplied payload
limits, scatter/gather UDP GSO with fallback, and validated UDP GRO splitting.
Worker sends and receives now use these offloads with ordinary-I/O fallback.
An unchanged GRO buffer can cross session and lane owners; each segment is
validated independently. Fixed receive pools and per-origin frame reservations
keep each session below the directional frame/byte limits without shared hot
reservations. Workers now pack multiple frames per datagram and retain batches
through partial sends. Metadata uses frame indices/ranges without payload copies
or self-references; expiry preserves live frames and quiescence drains batches.
The socket API gathers original slices within a fixed descriptor budget.
Generated sequences, small-frame bursts and partial batches have regression tests.
A ten-second bidirectional TAP storm checks observed packing, integrity, duplicate
suppression and zero allocations/deallocations, while reporting drops. BENCH.md
records the release throughput/CPU baseline; overload loss is severe and the
throughput gate has not passed. Receive-side ACK scheduling/range construction
is implemented over the existing bitmap; sender-history validation and runtime
ACK routing are not yet connected. Recovery and congestion control remain unfinished. Current liveness
probes do not discover MTU. The UDP limit defaults to 1232 bytes; an interim
per-peer `outer_payload_limit` accepts 1232–1452 as an operator assertion for
all destination paths. Larger configured limits report configured state, not
confirmed PMTU. Stage 3 must replace this assertion with per-lane discovery.
The foreground daemon now parses the documented TOML, provisions cookies through
gen-uuid, refreshes hostnames through per-peer iphost helpers after readiness,
and joins transport workers on SIGINT/SIGTERM. Tests cover delayed DNS, address
refresh and signal cleanup. Double-fork readiness and service integration remain
stage 4 work. No performance acceptance claim has been made.

1. **Socket and device foundation.** Implement fixed receive-only sockets, dynamic transmit pools, IPv6/interface/VRF binding, UUID lookup, TAP ownership, and counters. Validate R01–R10 and the port invariants.

2. **Batched transport.** Implement frame records, packing, bounded reassembly, UDP GSO/GRO, and TAP offload handling. Establish CPU and packet-rate baselines against L2TPv3.

3. **Recovery and congestion.** Implement the v1 selective-repair/ACK rules and BBRv3 aggregate pacing and congestion control. Evaluate loss and latency together; this stage is required for a production candidate.

4. **Composition and robustness.** Complete the bridge/OVS/VLAN/macvlan matrix, resource stress, restart and path-change behavior, and sustained bidirectional tests.

5. **Measured optimization.** Profile the remaining bottleneck. Evaluate inner GSO representation, steering, single-flow striping, and FEC only where measurements justify their complexity.

6. **Protocol freeze.** Resolve the open decisions, publish exact encodings/state transitions and test vectors, and repeat the acceptance suite against the frozen version.

Maintain a clear distinction between a throughput experiment and an interoperable production protocol. Promote mechanisms only after correctness and the supported topologies have been exercised.

# 16 Decisions and remaining acceptance work

| Decision | Initial direction |
| - | - |
| Numeric competitive margin | Define before acceptance; evaluate 1.5-fold as a proposal |
| Local throughput | Required: ≥20 Gbit/s each direction simultaneously at inner MTU 1500; report CPU; freeze hardware, core budget and flow count |
| Real-access throughput | Required target: ≥5 Gbit/s over routed IPv6 10G-EPON, RTT 4.5–7.5 ms and nonzero loss; freeze flow count, direction, CPU model and loss profile |
| Outer path MTU | Discover per lane from 1280; use confirmed larger sizes, including above 1500, within wire and reserved-buffer bounds |
| Maximum inner MTU and tag depth | Fixed: MTU 9000 with two VLAN tags, 9022-byte frame ceiling (section 3) |
| Controller implementation | Decided: in-tree BBRv3 draft 07, one owner/budget per direction and path group (section 9) |
| Retry and frame deadlines | Initial two-repair limit and bounded RTT-based sender / 100 ms receiver deadlines (section 8); tune by measurement |
| Reordering policy | Decided: flow-to-lane pinning, immediate completed-frame delivery; extra reorder buffering deferred |
| Restart protocol | Decided: automatic tag election, random directional receive tags, HELLO/CHALLENGE/CONFIRM/READY (section 5) |
| Abandonment protocol | Decided: transport-acceptance ACKs, bounded advisory abandonment, permanent window floors and lane tombstones (sections 6 and 8) |
| Wire layout | v1: 48-byte common header and exact record bodies (section 6); interoperability vectors required before release |
| Worker and lane counts | Start small; determine from queue and CPU measurements |
| Feedback routing | Decided: per-worker-pair bounded SPSC queues, eventfd wakeups, explicit lane owners (section 10) |
| Library API | Decided: library owns worker threads, blocking run plus control handle (section 3) |
| Public identity types | Decided: in-tree `UnquicUuid` and checked nonzero unicast `MacAddress` |
| TAP MAC and administrative state | Decided: `Option<MacAddress>`; None derives a Menhera MAC-M address using SHA-256 over hostname, NUL and interface name. The caller/network manager brings interfaces up; any owned queue controls carrier (section 11) |
| DNS refresh and address migration | Keep a working address; one candidate and a new handshake for change (section 5) |
| Inner offload representation | Experimental, negotiated extension |
| ECN and DSCP policy | Decided: outer Not-ECT and DSCP zero; preserve inner fields; ECN feedback deferred |
| FEC | Optional experiment after selective repair is measured |
| Minimum kernel and service packaging | Kernel target 5.0; qualify APIs/fallbacks before release. Optional service integration and packages do not raise core runtime requirements |

These decisions do not weaken the confirmed constraints: encryption remains absent, the fixed port remains receive-only, all outgoing protocol traffic uses dynamic per-destination ports, native outer fragmentation remains prohibited, and the library core depends on nothing beyond `libc`.

# 17 Technical references

The deployment figures and project requirements come from the operator's brief. The references below support Linux behavior and transport mechanisms; they do not establish this project's performance.

- **S1** [RFC 8085 UDP Usage Guidelines](https://www.rfc-editor.org/rfc/rfc8085.html) — message sizing, independently repairable pieces, and congestion control across sockets.

- **S2** [Linux Universal TUN TAP driver](https://docs.kernel.org/networking/tuntap.html) — Ethernet TAP devices and multiqueue operation.

- **S3** [Linux socket manual](https://man7.org/linux/man-pages/man7/socket.7.html) — reuseport, socket binding, and receive-overflow reporting.

- **S4** [Linux networking scaling](https://docs.kernel.org/networking/scaling.html) — RSS, receive queues, and CPU distribution.

- **S5** [Linux VRF documentation](https://docs.kernel.org/networking/vrf.html) — VRF-aware sockets and binding scope.

- **S6** [RFC 9562 UUIDs](https://www.rfc-editor.org/rfc/rfc9562.html) — UUIDv4 layout and random-bit count.

- **S7** [RFC 3931 L2TPv3 section 8](https://www.rfc-editor.org/rfc/rfc3931.html#section-8) — cookie protection and its boundary.

- **S8** [RFC 8899 Datagram PLPMTUD](https://www.rfc-editor.org/rfc/rfc8899.html) — robust path-MTU discovery for datagram transports.

- **S9** [RFC 9002 QUIC Loss Detection and Congestion Control](https://www.rfc-editor.org/rfc/rfc9002.html) — RTT estimation, loss detection, and probe timers.

- **S10** [RFC 3366 Advice on link ARQ](https://www.rfc-editor.org/rfc/rfc3366.html) — recovery persistence, reordering, and latency tradeoffs.

- **S11** [BBR congestion control draft 07](https://datatracker.ietf.org/doc/html/draft-ietf-ccwg-bbr-07) — experimental BBRv3 specification, dated 2 November 2026.

- **S12** [Linux UDP manual](https://man7.org/linux/man-pages/man7/udp.7.html) — UDP_SEGMENT and UDP_GRO.

- **S13** [Linux segmentation offloads](https://docs.kernel.org/networking/segmentation-offloads.html) — GSO, GRO, and checksum metadata relationships.

- **S14** [Linux TAP implementation](https://codebrowser.dev/linux/linux/drivers/net/tun.c.html) — VLAN materialization and carrier control; the cited view follows its published source snapshot.

- **S15** [Linux bridge documentation](https://docs.kernel.org/networking/bridge.html) — VLAN handling and reserved multicast forwarding policy.

- **S16** [Open vSwitch common configuration issues](https://docs.openvswitch.org/en/stable/faq/issues/) — external TAP ownership and bridge attachment.

- **S17** [Linux ip link manual](https://man7.org/linux/man-pages/man8/ip-link.8.html) — VLAN and macvlan configuration.

- **S18** [Tailscale throughput optimization report](https://tailscale.com/blog/more-throughput) — evidence that optimized encrypted tunnels already exploit batching and offloads; its benchmark numbers are not predictions for this project.
