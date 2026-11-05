# NGN Ethernet Transport Initial Requirements and Design

**Draft 0.1 • 4 November 2026 • Human-life Information Platforms Institute Menhera®**

Build a Linux userspace Ethernet transport for closed-area NTT NGN networks. The transport must carry inner MTUs of at least 1500 over an IPv6-only UDP underlay whose IP MTU is fixed at 1500, without emitting outer IP fragments. It must use loss-tolerant congestion control and target sustained delivered throughput above 3 Gbit/s on modest 2–4-core hosts.

Encryption is deliberately absent. A session UUID is the sole session admission cookie. The project must turn that deployment choice into materially lower processing cost and substantially higher throughput than WireGuard, SoftEther VPN, and comparable solutions under equivalent conditions.

This initial design distinguishes **confirmed requirements**, **proposed implementation choices**, and **open decisions**. It is a basis for prototyping and review; the byte-level protocol and performance acceptance profile are not yet frozen.

The project is named UnQuic and is written in pure Rust with `libc` as the only dependency, built for `*-unknown-linux-musl` targets (see README.md). Every mechanism below, including the congestion controller, configuration parser, and UUID generation, must therefore be implemented in-tree over raw system calls.

# 1 Deployment and motivation

The existing rs-l2tpd deployment uses the Linux L2TPv3 dataplane and native IPv6 fragmentation to carry frames larger than the underlay allows. NTT engineers have advised that packet loss makes naive fragmentation a poor fit for these NGN paths. Losing one fragment makes the whole encapsulated frame unavailable; moving the split into UDP alone does not remove this problem.

The proposed transport combines independently recoverable frame pieces, efficient packing, batching, and congestion control. Its opportunity is both lower CPU cost and better delivered goodput under loss.

The following are operator-provided deployment observations, not provider guarantees:

| Property | Current design input |
| - | - |
| Access | Shared 10G-EPON NTT NGN |
| Underlay | IPv6-only UDP |
| Underlay IP MTU | Fixed at 1500 bytes |
| RTT | Approximately 4.5–7.5 ms |
| Idle jitter | Less than approximately 1.5 ms |
| Loss | Nonzero; rate and burst distribution remain to be measured |
| Direct underlay throughput | Approximately 5 Gbit/s |
| Host budget | 2–4 cores with modest clock rates |

Available bandwidth may change with shared-access load. Idle jitter does not bound delay under congestion. The measured 5 Gbit/s is a comparison baseline, not a permanent pacing rate.

# 2 Confirmed requirements

The requirement identifiers below describe commitments established for this project. Proposed values and mechanisms appear separately.

## 2.1 Transport and addressing

- **R01 Ethernet service.** Expose Linux TAP interfaces and preserve Ethernet service across the underlay. Support a configurable inner MTU of at least 1500. Larger values must be carried by transport segmentation without outer IP fragmentation; the maximum remains to be selected.

- **R02 Outer transport.** Send UDP over IPv6 only. Every emitted outer IPv6 packet must fit the applicable path MTU and must not contain an IPv6 Fragment header.

- **R03 Endpoint configuration.** Identify each peer by an FQDN or IPv6 literal and a fixed UDP receive port.

- **R04 Receive port.** Each daemon has one fixed UDP port number used exclusively for receiving protocol traffic. It never sends protocol packets from this port.

- **R05 Transmit ports.** Maintain several dynamically allocated source ports per destination. Data, ACKs, probes, repair traffic, and session-control messages all leave through these dynamic ports and target the peer's fixed receive port.

- **R06 Interface and VRF binding.** Support binding the relevant local sockets to a Linux interface or VRF. Receive and transmit scope must be consistent.

## 2.2 Session and Ethernet behavior

- **R07 Session identity.** Identify a session by a 128-bit UUID cookie, independently of any one UDP five-tuple.

- **R08 Deliberately unencrypted operation.** Use no encryption. The session cookie is the sole session admission mechanism over the semi-trusted closed-area underlay.

- **R09 Device composition.** Support adding TAPs to Linux bridges or OVS and creating VLAN or macvlan interfaces over them, subject to normal Linux stacking rules.

- **R10 Ethernet transparency.** Support multiple MAC addresses, unicast, broadcast, multicast, tagged frames, and non-IP Ethernet traffic. The service must not depend on IPv4 or IPv6 being present inside a frame.

## 2.3 Performance and congestion

- **R11 Throughput.** Target sustained delivered inner goodput above 3 Gbit/s on the specified 2–4-core deployments when the contemporaneous underlay capacity can support it.

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

Minimum supported kernel and maximum supported inner MTU remain open.

# 4 Socket and peer model

Each daemon receives on its configured fixed port. It creates a pool of dynamic-port transmit sockets for each destination and keeps their source ports stable while the sockets remain active. The source port is not randomly changed for every packet.

| Traffic | Source socket | Destination socket |
| - | - | - |
| A sends data | A dynamic port i | B fixed receive port |
| B acknowledges A | B dynamic port j | A fixed receive port |
| B sends data | B dynamic port j | A fixed receive port |
| A acknowledges B | A dynamic port i | B fixed receive port |

An ACK carries the identity of the original sender's lane and the packet ranges it acknowledges. Its own source port is unrelated to that identification. No protocol reply is sent to the observed dynamic source port.

The fixed receive port may be implemented by several sockets in a `SO_REUSEPORT` group. One port number does not require one file descriptor or one receive worker. Dynamic source ports provide distinct UDP flows for NIC RSS; RSS queue selection and reuseport socket selection must both be measured. Plain hashing does not guarantee an even distribution. [S3, S4]

Dynamic transmit sockets may use UDP `connect()` for destination selection and error handling. Normal protocol responses still arrive on the fixed receive port. The daemon must separately handle asynchronous socket errors and ICMPv6 feedback associated with transmit sockets. Deployments must permit inbound traffic to the fixed port explicitly; reverse protocol traffic is not necessarily the reverse five-tuple of an outbound datagram.

Use one fixed receive port number across configured binding scopes. If a daemon serves several VRFs, create the necessary receive sockets in those scopes using the same number. Apply `SO_BINDTODEVICE` before transmission to the selected interface or VRF; avoid depending on global cross-VRF socket acceptance. [S5]

Resolve FQDNs to IPv6 addresses outside the packet-processing loop. Select and validate a destination address, keep it stable during a path generation, and refresh DNS asynchronously. DNS failure should not immediately tear down a working path. Address changes require path validation and new path measurements. DNS lookup routing must be configured separately if resolution itself must use a particular VRF.

# 5 Session cookie and lifecycle

Carry the session UUID as 16 binary bytes in every protocol datagram, including feedback and control. All lanes for the session use the same configured cookie. Reject duplicate local session-cookie configurations and reject unknown cookies before allocating per-packet state or sending a response.

**Proposed generation rule:** require UUIDv4 generated from `getrandom(2)`. UUIDv4 has 122 random bits in a 128-bit field; arbitrary UUID versions do not provide the same unpredictability. Provision the cookie out of band and use a public session label for logs and metrics. The full cookie must not appear in routine diagnostics. [S6]

The cookie checks possession of a shared bearer value. It provides neither confidentiality nor cryptographic payload integrity. An observer who learns it can forge traffic or feedback. This is the deliberately accepted deployment boundary, analogous to the blind-injection protection of an L2TPv3 cookie. [S7]

**Proposed restart handling:** preserve the configured session UUID while establishing a fresh sender epoch on restart. Packet numbers and frame IDs are 64-bit and cannot be exhausted in practice, so an epoch changes only when the process restarts. Take the epoch from the wall clock at startup so it is monotonic across restarts without persistent state; a peer accepts a new epoch only through a HELLO exchange and only if it is greater than the one currently active. Scope packet numbers by session, sender epoch, and lane. Scope frame IDs by session and sender epoch. A restart exchange must establish the active generation before accepting new DATA; an arbitrary delayed packet with a different epoch must not reset an established session.

Use a small lifecycle: configured, establishing, active, degraded, and inactive. HELLO messages advertise compatible protocol features, MTU limits, and the configured fixed receive endpoint. They do not exchange cryptographic keys. Reject incompatible mandatory features explicitly. Exact restart, stale-generation rejection, and recovery transitions must be specified before interoperable implementations are attempted.

After a lane failure, retire its packet state and establish a replacement lane without changing the session identity. Keep the TAP device through peer reconnection; use carrier state to reflect session availability with suitable hysteresis.

# 6 Proposed packet and record format

Use a versioned fixed common header followed by bounded records. Every on-wire UDP datagram is independently parseable and contains the cookie. Encoding uses explicit byte order and fixed-width integers; it must not serialize native Rust, C, or virtio structures directly.

The following is a candidate header for prototyping, not a frozen wire specification:

| Field | Bytes | Purpose |
| - | -: | - |
| Magic | 2 | Basic protocol recognition |
| Version | 1 | Wire-format version |
| Flags | 1 | Packet properties |
| Session cookie | 16 | Session lookup and admission |
| Sender epoch | 8 | Restart generation |
| Transmit lane ID | 2 | Packet-accounting lane |
| Record area length | 2 | Bounds checking |
| Packet number | 8 | Unique transmission identity |
| Total | 40 | Common header |

A candidate DATA record has a 4-byte type, flags, and length prefix; an 8-byte frame ID; a 4-byte total frame length; a 4-byte byte offset; and the piece's bytes. This costs 20 bytes before frame data. Frame IDs must be unique within a sender epoch; workers can allocate disjoint ID ranges to avoid a shared counter on every frame.

With no IPv6 extension headers, the UDP payload budget is `1500 - 40 - 8 = 1452` bytes. Under the candidate format, a datagram carrying one DATA record can hold `1452 - 40 - 20 = 1392` frame bytes. Additional records consume additional record headers. Wire-efficiency calculations must use the final layout rather than earlier illustrative header sizes.

Proposed record types are DATA, ACK, HELLO, HELLO_ACK, PROBE, PROBE_REPLY, FRAME_ABANDON, and optional padding for probes. ACKs name the acknowledged sender epoch and lane and carry bounded packet ranges and feedback delay. Exact record layouts, extension rules, and ACK limits remain open.

Every physical transmission gets a new packet number. A repair retains the original frame ID and byte offsets but uses a new transmission number. The receiver deduplicates both transmissions and frame content; a late original packet and a successful repair must never inject the frame twice.

Validate complete record boundaries, lengths, offsets, supported flags, and size limits before changing protocol state. Reject inconsistent total lengths and conflicting overlaps. Bound ACK processing to ranges that could correspond to packets actually sent. Pure ACK packets must not trigger ACK-of-ACK loops.

An ACK acknowledges transport reception, not successful consumption by the bridge, OVS, or an application. Define the exact receiver acceptance point and FRAME_ABANDON behavior before freezing the protocol so memory-pressure drops cannot be mistaken for reliable frame delivery.

# 7 Packing and segmentation

The transport must segment inner frames itself. Linux must never silently perform outer IPv6 fragmentation. Set `IPV6_DONTFRAG` on every transmit socket, handle `EMSGSIZE` and validated Packet Too Big feedback, and retain a per-path payload limit. Treat 1500 as the configured ceiling; a smaller effective path must cause smaller datagrams or a clear path failure. DPLPMTUD is the proposed robustness mechanism. [S8]

Allow one datagram to contain several records, including a frame tail followed by a piece of another frame. Each record contains enough information to recover its frame independently. No frame boundary may depend on successfully parsing a preceding datagram.

For a 1500-byte inner IP packet, the ordinary Ethernet frame is 1514 bytes. With the candidate record layout, sending it alone produces pieces of 1392 and 122 bytes. Packing the short tail with already queued data reduces packet count. The same approach applies to larger inner MTUs.

Packing should use data already available to the worker. Flush promptly when the queue empties; do not wait indefinitely for enough traffic to fill a datagram. Balance batch size against pacing burst size and latency.

Construct UDP GSO buffers as sequences of complete UDP payloads, each with its own common header. All but the final segment in a GSO submission have the configured segment size. GSO does not create custom protocol headers. A partially filled tail ends that submission or is sent separately; avoid wasteful padding merely to force batching.

A missing datagram may contain pieces of several frames. Recover its still-needed content selectively and keep completion independent across frames. Packing must not become a connection-wide ordered stream.

# 8 Loss recovery and ordering

**Proposed baseline:** selective retransmission with bounded persistence. Keep transmitted payload buffers briefly; ACK ranges identify received transmissions, and packet/time thresholds identify likely loss while allowing reordering. Repair missing frame ranges rather than retransmitting an entire oversized frame. This follows the independently receivable and repairable message-piece model described in RFC 8085. [S1]

Use RTT and variance estimators, a bounded feedback delay, and a tail-loss probe mechanism. QUIC loss detection is a reference for these mechanisms, not a requirement to implement QUIC or its cryptography. A probe timeout must not automatically declare every outstanding packet lost. [S9]

Frames expire by deadline and resource limits. Stop repairing expired or already delivered frames. Carry abandonment information when needed to release receiver state. Keep recent completion and expiry information long enough to prevent delayed pieces from recreating retired frames.

No frame may wait behind an unrelated incomplete frame merely because its global sequence number is later. For an inner flow undergoing repair, a short bounded reorder window can prevent later packets from provoking premature inner TCP loss recovery. This trades latency for loss concealment and must be evaluated. [S10]

Proposed first mapping: keep an inner flow on one transmit lane and keep all pieces of a frame under one reassembly owner. Define an Ethernet-based fallback for traffic whose IP or transport headers cannot be parsed. Flow keys must include relevant VLAN context and must handle inner fragmentation consistently.

This mapping may limit a single inner flow to one worker. If single-flow throughput requires striping, evaluate distributing complete frames across lanes with a separate bounded per-flow reorder mechanism. Do not introduce cross-worker assembly of every frame by default.

Retry count, frame deadline, reorder limits, and receiver abandonment semantics remain open. Compare bounded one-retry and two-retry profiles using measured loss traces and loaded RTT. FEC is a later experiment: it may reduce repair delay, but all redundant bytes consume bandwidth and congestion budget.

# 9 Congestion control and pacing

Congestion control is a required production behavior. Support arbitrary Ethernet traffic and account for the extra traffic created by repair. Do not rely solely on inner TCP to control the tunnel.

**Proposed controller candidate:** BBRv3, as an in-tree implementation of its delivery-rate and inflight model; the no-dependency rule rules out reusing an external crate. Because that is a large piece of work, stage 3 may start with a simpler in-tree loss-and-delay controller and replace it once BBRv3 is validated. BBRv3 remains an experimental specification. Selection depends on throughput, coexistence, queueing delay, and CPU results; the algorithm is not yet a confirmed requirement. [S11]

Maintain one congestion budget per sending direction and path group. Several source ports to the same peer are not independent entitlements to bandwidth. Share pacing and inflight limits across those lanes while retaining per-lane measurements. RFC 8085 explicitly discusses aggregate control across multiple sockets. [S1]

Count new data, retransmissions, FEC, and relevant probes in the offered-load budget. Keep ACK-only feedback promptly deliverable and separately bounded so waiting for data-window credit cannot deadlock feedback. Piggyback ACKs where useful without requiring reverse application traffic.

Workers should receive short-lived byte budgets or scheduled send opportunities; avoid a shared controller lock for every packet. Record send timing close enough to actual transmission that application queues and GSO bursts do not corrupt delivery-rate estimates. Handle application-limited samples and lost feedback.

Use bounded transmit queues with age limits. Drop whole unsent frames before segmentation where possible. Reserve sufficient service for control traffic and avoid letting bulk repair starve fresh traffic.

Do not assume that every loss is random or that every RTT increase is congestion. Measure receiver socket drops, queue delays, and CPU pressure alongside network feedback. An overloaded receiver also needs the sender to reduce its load.

ECN behavior is an open design item. Do not mark outer packets ECN-capable until a defined feedback and congestion response is implemented. Preserve inner packet fields unless an explicit tunnel ECN policy requires changing them. Fixed-rate and controller-disabled modes may exist for isolated experiments, but are not substitutes for the required production controller.

# 10 Linux dataplane

Create Ethernet TAP devices using `IFF_TAP`, `IFF_NO_PI`, and multiqueue support. Use `IFF_VNET_HDR` only with correctly implemented and negotiated checksum/GSO handling. Configure offloads explicitly and record their effective state. [S2]

The baseline fast path should use preallocated buffer pools, bounded descriptor rings, nonblocking I/O, `sendmmsg()`, `recvmmsg()`, UDP GSO, and UDP GRO where supported. Use fewer system calls and fewer full stack traversals per byte. GSO and GRO fallbacks must preserve correctness when an interface, driver, or binding scope cannot support them. [S12]

On UDP GRO receive, split the buffer into its logical datagrams using the returned segment metadata and validate each datagram, including its cookie. A coalesced receive buffer is not one large protocol packet.

Keep frame buffers and reassembly slots owned by one worker. Retain references for repair and use scatter/gather I/O where it avoids extra payload copies. Measure whether packing into contiguous buffers or gathering slices is faster on target machines. Ordinary TAP and socket I/O still involves kernel copies.

Start with a small worker count and several stable transmit lanes. Measure NIC queue counters, reuseport distribution, softirq CPU, and cross-worker traffic. Optional reuseport steering can align session/lane ownership with receive workers, but must not route all traffic for a session to one core merely because the cookie is shared. [S3, S4]

There is no encryption or per-packet cryptographic authentication pass. The implementation should not introduce transformation buffers or worker handoffs that exist only to support such a stage. Preserve ordinary UDP checksums and use hardware/kernel checksum support.

Avoid assuming every frame is host-generated TCP. Forwarded Ethernet frames, inner IPv6 extension headers, non-IP payloads, and unavailable offloads must remain supported.

**Experimental extension:** transport a negotiated, normalized representation of supported inner GSO and partial-checksum metadata, reconstructing it at the destination TAP. This could avoid segmenting and later coalescing inner packets in userspace. It is not a prerequisite for correctness or the first interoperable format. Validate metadata, cap aggregate size, and test bridge/OVS forwarding and loss behavior. Never interpret an ordinary oversized Ethernet frame as a GSO object. [S13]

# 11 Ethernet device behavior

Preserve source/destination MAC addresses, EtherType or length fields, payload, VLAN tags, and frame boundaries. Do not rewrite MAC addresses to the TAP's own address. Support IEEE 802.3/LLC traffic such as IS-IS and normal multicast and broadcast behavior.

Support 802.1Q and 802.1ad tags, including stacked tags and PCP/DEI bits. The Linux TAP path can materialize a VLAN tag from skb metadata into the bytes returned to userspace; tests must exercise this path rather than assuming all tags were already in-line. [S14]

Distinguish inner interface MTU, ordinary Ethernet-frame size, and an offloaded GSO object. MTU 2400 corresponds to 2414 bytes without VLAN tags and 2422 bytes with two tags, excluding FCS. Account separately for TAP metadata. Negotiate and enforce frame and aggregate limits; the maximum supported tag depth and inner MTU remain explicit design decisions.

The daemon must allow Linux bridge or OVS to own forwarding policy. It need not implement an FDB, STP, or VLAN access/trunk policy. Reserved link-local traffic is transported if delivered to the daemon; bridge/OVS policy may consume or filter it before that point. [S15, S16]

The following independent configurations should work with existing bridge devices:

| Attachment | Example |
| - | - |
| Linux bridge | `ip link set tap0 master br0` |
| OVS system port | `ovs-vsctl add-port ovs0 tap0 -- set Interface tap0 type=system` |
| VLAN upper | `ip link add link tap0 name tap0.100 type vlan id 100` |
| Macvlan upper | `ip link add link tap0 name mv0 type macvlan mode bridge` |

These examples do not imply that all attachments can coexist on the same lower device. Preserve normal Linux stacking constraints. The daemon retains the TAP file descriptors when OVS uses it as a system port. [S16, S17]

Keep TAP names, MAC configuration, and device identity stable across ordinary peer reconnects. Persistent TAP ownership and reattachment across process restart are proposed operational features. Session failure changes carrier according to a defined policy; it does not delete the interface or its VLAN/macvlan children.

# 12 Configuration and operations

Separate daemon-wide receive configuration, peer transport configuration, and per-session Ethernet configuration. One peer may carry multiple sessions; shared lanes or separate lane pools must still observe the appropriate aggregate congestion budget.

An illustrative configuration shape follows. Names and default values are proposed, and the cookie is a placeholder. The file is a flat TOML-like subset (tables, array tables, and scalar `key = value` lines) parsed by a small in-tree parser; it is not full TOML, because no TOML crate is allowed.

```toml
[daemon]
listen_address = "::"
listen_port = 40000

[[peers]]
name = "peer-a"
host = "peer-a.example.net"
port = 40000
bind_device = "vrf-ngn"
tx_lanes = 4

[[sessions]]
name = "ngn-a"
peer = "peer-a"
cookie = "<uuid-v4>"
tap_name = "tap-ngn-a"
inner_mtu = 2400
```

Support both FQDN and IPv6-literal hosts, resolved with `getaddrinfo(3)` on a helper thread so the packet loop never blocks. `bind_device` is per peer because peers may live in different VRFs; the daemon creates one receive socket per distinct device on the single configured port, as section 4 requires. Define scope identifiers for link-local literals if those addresses are supported. Cookie changes are explicit administrative operations; do not silently replace a configured cookie during reconnect.

Expose per-session, per-peer, and per-lane counters for delivered frame bytes, outer bytes, repairs, duplicates, expired frames, queue drops, reassembly occupancy, RTT, feedback delay, pacing rate, bytes in flight, and path-MTU state. Distinguish malformed/unknown-cookie packets from operational loss without logging secrets.

Record kernel version, CPU model, active cores and frequency, NIC/driver, queue mapping, offload state, binding scope, and effective socket-buffer limits in diagnostic output. Include socket overflow and softnet/NIC drops in performance investigations.

Bound every queue, ACK history, incomplete-frame table, completion cache, and retained transmit-buffer set. Define behavior on allocation pressure, device removal, interface renaming, DNS failure, path change, and daemon restart. Configuration reload must preserve unrelated sessions.

Keep privileges limited to what is needed to create/configure TAPs and bind sockets; allow attaching to appropriately provisioned devices. The exact service-management and privilege model is an implementation decision.

# 13 Performance budget

Use delivered inner bytes per second as the headline result and report the precise measurement layer. Record outer IP bytes and repair/control bytes separately. Do not compare TCP payload throughput with Ethernet or outer-wire throughput without conversion.

For illustration, 3 Gbit/s of inner IP traffic requires the following rates when packets are full-sized. MTU 2400 is an example larger-MTU test point, not an operator-specified requirement:

| Inner MTU | Inner frames per second | Outer packets per second with two per frame |
| - | -: | -: |
| 1500 | 250000 | 500000 |
| 2400 | 156250 | 312500 |

Assuming packing delivers an average of 1350 inner IP bytes per outer packet, the outer rate would be about 278000 packets/s. This average is a planning assumption below the candidate's 1392-byte frame-data ceiling; actual efficiency depends on Ethernet overhead, frame sizes, record boundaries, and partial batches.

At 3 Gbit/s and RTT 4.5–7.5 ms, the bandwidth-delay product is approximately 1.69–2.81 MB per direction. Retaining 20 ms of payload for recovery would require about 7.5 MB per direction before metadata and copies. Bounds should scale with the configured memory ceiling and measured path, not with arbitrary tiny packet counts.

Two 2 GHz cores provide approximately 14400 aggregate CPU cycles per outer packet at 278000 packets/s. That budget includes their kernel and userspace processing and is not evenly available if a queue concentrates work on one core. Full-duplex 3 Gbit/s in each direction roughly doubles packet work.

At an assumed 90 percent effective encapsulation efficiency, delivering 3 Gbit/s needs about 3.33 Gbit/s of outer traffic. Adding repair traffic equal to 10 percent of original transmissions raises this illustrative demand to about 3.67 Gbit/s. These are arithmetic budgets, not measured results.

A proposed development milestone is at least 4 Gbit/s of clean-path inner goodput on the actual CPU budget, providing headroom for the above-3-Gbit/s deployment target. Neither that milestone nor a competitive margin is established by removing encryption alone. Existing WireGuard implementations already use substantial batching and offload optimization. [S18]

# 14 Validation and acceptance

Freeze a repeatable acceptance profile before declaring the performance requirements met. It must name the endpoint hardware and core budget, inner MTU, frame mix, flow count, traffic direction, attachment topology, loss profile, and contemporaneous direct-underlay capacity.

## 14.1 Functional gates

- **V01 Transport invariants:** capture outside the sending host's offload boundary and verify IPv6 UDP only, packet sizes within the limit, no Fragment headers, all destinations at fixed receive ports, and no protocol transmission from a fixed receive port.

- **V02 Session correctness:** test valid/unknown cookies, duplicate configuration, malformed records, bad ACK ranges, duplicate and reordered datagrams, late originals after repairs, and no duplicate Ethernet delivery.

- **V03 Lifecycle:** test restart epochs, delayed old packets, lane replacement, loss of all feedback, peer reboot, DNS address changes, and persistent TAP reattachment.

- **V04 Ethernet composition:** test Linux bridge, OVS system ports, VLAN/QinQ, macvlan, multiple MACs, multicast, broadcast, and non-IP/LLC traffic. Verify frame contents and MTU handling.

- **V05 Binding and offloads:** test ordinary interface and VRF binding, lower effective PMTU, software fallbacks, GSO/GRO boundaries, VLAN metadata, partial checksums, and receive truncation.

- **V06 Resource behavior:** stress reassembly limits, loss bursts, ACK-history limits, queue backpressure, and sustained receiver overload. Verify bounded memory and recovery without corrupting unrelated sessions.

## 14.2 Performance comparisons

Compare the direct underlay, existing kernel L2TPv3 with fragmentation, the new transport, kernel WireGuard carrying an equivalent L2 service, and an appropriately configured SoftEther deployment. Record exact versions, acceleration settings, and encapsulation stacks. Plain WireGuard L3 may be an additional diagnostic baseline but is not the equivalent-service comparison.

Use the same endpoints, CPU/core budget, inner MTU, offered traffic, and underlay conditions. Do not disable a competitor's normal acceleration. Preserve supported inner MTUs rather than obtaining the result by silently reducing them.

Test MTU 1500 and proposed larger test points such as 2400 and 9000 if supported. Include small-frame and mixed-size traffic, single and multiple inner flows, both one-way directions, simultaneous bidirectional traffic, and bridged/OVS traffic from separate traffic generators.

Measure on a controlled link with approximately the observed RTT and reproducible independent and burst loss, then on real NGN paths. Record real loss and loaded delay before selecting impairment parameters. Host-side loss injection must be placed so it operates on intended on-wire packets rather than accidentally dropping entire offload aggregates.

**Proposed measurement procedure:** at least five runs with at least 60 seconds of steady-state measurement per profile; retain cold-start results separately. Measure the direct underlay before and after real-network runs. Use longer runs where shared-access variation is material.

Report goodput, total endpoint CPU including softirq, per-core load, p50/p95/p99 latency under load, frame loss/expiry, repair overhead, memory, and queue drops. A throughput gain accompanied by excessive queueing must remain visible.

**Pass criteria still to freeze:** the numerical definition of a large competitive margin; which profiles must exceed 3 Gbit/s; whether that target is single-flow, aggregate, one-way, or simultaneous bidirectional; maximum acceptable loaded latency; and a representative loss profile. A tentative 1.5-fold throughput advantage can be evaluated as a proposal, not treated as an agreed requirement.

If a comparison already reaches the available underlay ceiling, report that fact and CPU cost at equal goodput. Do not claim a large throughput margin that the measured path cannot demonstrate.

# 15 Implementation stages

1. **Socket and device foundation.** Implement fixed receive-only sockets, dynamic transmit pools, IPv6/interface/VRF binding, UUID lookup, TAP ownership, and counters. Validate R01–R10 and the port invariants.

2. **Batched transport.** Implement frame records, packing, bounded reassembly, UDP GSO/GRO, and TAP offload handling. Establish CPU and packet-rate baselines against L2TPv3.

3. **Recovery and congestion.** Add bounded selective repair, ACK accounting, adaptive timing, aggregate pacing, and congestion control. Evaluate loss and latency together; this stage is required for a production candidate.

4. **Composition and robustness.** Complete the bridge/OVS/VLAN/macvlan matrix, resource stress, restart and path-change behavior, and sustained bidirectional tests.

5. **Measured optimization.** Profile the remaining bottleneck. Evaluate inner GSO representation, steering, single-flow striping, and FEC only where measurements justify their complexity.

6. **Protocol freeze.** Resolve the open decisions, publish exact encodings/state transitions and test vectors, and repeat the acceptance suite against the frozen version.

Maintain a clear distinction between a throughput experiment and an interoperable production protocol. Promote mechanisms only after correctness and the supported topologies have been exercised.

# 16 Open decisions

| Decision | Initial direction |
| - | - |
| Numeric competitive margin | Define before acceptance; evaluate 1.5-fold as a proposal |
| Scope of the above-3-Gbit/s target | Freeze flow count, direction, topology, CPU model, and loss profile |
| Maximum inner MTU and tag depth | Require at least 1500; evaluate 2400, 9000, and at least two VLAN tags |
| Controller implementation | Evaluate BBRv3 and its aggregate-lane integration |
| Retry and frame deadlines | Select from loaded-path measurements |
| Reordering policy | Bounded per-flow policy with a non-IP fallback |
| Restart and abandonment protocol | Specify exact transitions and stale-state handling |
| Final wire layout | Candidate 40-byte common header; ACK and control encodings open |
| Worker and lane counts | Start small; determine from queue and CPU measurements |
| DNS refresh and address migration | Explicit policy with stable active-path behavior |
| Inner offload representation | Experimental, negotiated extension |
| ECN and DSCP policy | Define before enabling outer ECN capability |
| FEC | Optional experiment after selective repair is measured |
| Minimum kernel and service packaging | At least 5.0, since UDP GRO requires it (UDP GSO needs 4.18); confirm after validating the other APIs |

These decisions do not weaken the confirmed constraints: encryption remains absent, the fixed port remains receive-only, all outgoing protocol traffic uses dynamic per-destination ports, native outer fragmentation remains prohibited, and `libc` remains the only dependency.

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
