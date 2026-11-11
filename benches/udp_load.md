# Concurrent UDP load tool

Build with `cargo build --release --target x86_64-unknown-linux-musl --example udp_load`.
The executable is `target/x86_64-unknown-linux-musl/release/examples/udp_load`.
It uses Linux UDP_SEGMENT and UDP_GRO, with no additional dependency.
It does not configure networking. Run it only on authorized test paths.

```text
udp_load send|recv LOCAL_IP PEER_IP LOCAL_PORT PEER_PORT FLOWS PAYLOAD_BYTES SECONDS BITS_PER_FLOW RUN_ID SEGMENTS
```

Both addresses are literals of the same family. Each flow increments both base
ports by its index. Use separate port ranges for senders and receivers when
running both directions. Up to 64 connected sockets share one event loop;
pin each process with `taskset`. All buffers and duplicate windows are allocated
before timing. The data loop uses no allocation or locks.

For each flow, a sender submits up to SEGMENTS datagrams, capped at 64 and
65,487 payload bytes per call. SEGMENTS=1 uses ordinary UDP. The kernel must
support the requested offloads; errors terminate the run. The tool never
silently switches a segmented test to plain sends. Both IP families prohibit
IP fragmentation through PMTU-discovery socket options.

A rate of zero is unlimited. Other rates are integer UDP payload bits/s per
flow, paced with a token bucket capped at two aggregates. Successful sends
consume credits and sequence numbers; failed sends consume neither. The
actual offered rate can be below the request and must be reported.
Senders set a 1 ns timer slack. With Linux's default 50 µs slack, each sleep
overshoots enough that a two-aggregate bucket loses credit above roughly
40,000 sends per second per flow: four individual-frame flows at 1.25 Gbit/s
offered only about 1.9 Gbit/s in total before this change.

Receivers use the library's checked `recvmmsg` path to read up to 16 messages
per ready socket per turn, with about 1 MiB of reusable payload storage per
process. A readiness poll avoids repeatedly reading empty sockets.
They expand GRO messages before validating each datagram's run ID,
sequence number, length and entire payload. Each flow tracks 262,144 blocks of
64 sequence numbers (16,777,216 positions), using 4 MiB of startup-allocated
tag and bitmap storage. The maximum 64-flow configuration reserves 256 MiB for
these histories per process. This covers the test TAP queues' delayed
super-frames without allocating while traffic runs. Earlier measurements used
an 8,192-position window. Reordering within the window does not count as loss.
`too_old` reports packets outside the window whose duplicate status cannot
be established. Such packets are excluded from unique delivery; a nonzero
count invalidates an exact loss claim. `invalid` includes corrupt payloads,
wrong run IDs, and truncated or invalid ancillary data. `wrong_run` is the
subset with valid length, magic and payload but another run ID;
`first_wrong_run` records the first such ID. Both `invalid` and `too_old`
must be zero for an acceptance run.

The JSON output includes process CPU time, syscall counts, blocked sends,
GRO messages, observed socket overflow and the actual receive-buffer limit.
Per-flow `calls` counts send/receive syscalls; `poll_calls` counts readiness polls.
Overflow is in kernel SKB units, not datagrams, and its final cumulative value
is available only when the kernel attaches it to a received message.

Match sender and receiver flow entries to compute full-run missing packets:
`sent - unique`. Receiver `prefix_missing` covers only through its highest
observed sequence and cannot see a lost tail. Two receiver SIGUSR1 signals
record unique counts and elapsed times into fixed storage. Compute delivery
goodput from the difference of these samples, divided by their time interval.
Final `unique` includes the receiver drain; report this tail separately.
Historical results without samples normalize final delivery by sender duration
and can overstate forwarding rate when queues retain a long tail. Report duplicates,
reordering, invalid and too-old counts separately. The socket layer also
checks UDP checksums.

## TAP harness

`benches/tap_transport.sh` retains iperf3 by default. Set `UDP_LOAD` to the
absolute tool path and use integer per-flow rates in `BITRATE` to select this
tool. For example, 16 flows at `1312500000` bits/s request 21 Gbit/s per
direction. `SEGMENTS` defaults to 64. The two receivers run two seconds longer
than the senders to include the tail. `sent.*` snapshots follow sender exit;
`after.*` snapshots follow receiver exit. Use the former for load-interval
CPU and the latter for final drop attribution. Before/sent snapshots also
signal the receivers. A multi-rate invocation keeps the daemons, sockets,
TAPs and namespaces alive across all points, so overload and recovery exercise
the same transport state. Load processes restart with distinct run IDs;
receivers reject and count late packets from earlier points. Report those
`wrong_run` counters rather than crediting them as delivery. A failed child
fails the run.

For TCP comparisons, leave `UDP_LOAD` unset and set `IPERF_PROTOCOL=tcp`.
The same harness runs concurrent forward/reverse iperf3 connections with
64 KiB application writes; the flow argument selects streams per direction.
`BITRATE=0` lets TCP send without an application rate cap. TCP's congestion
control still limits offered traffic, so report delivered goodput and
retransmissions, not a UDP-style missing fraction. The frame-size argument
does not fix TCP segment size; the TAP MTU and TCP negotiation do.
`IPERF_PROTOCOL=udp` is the default.

`VLAN_DEPTH=0|1|2` selects untagged, 802.1Q or QinQ traffic. Frame sizes
include those tags. `SRV6=1` adds IPv6-in-IPv6 encapsulation with a one-segment
routing header and End.DT6 decapsulation inside the private endpoint
namespaces; it requires `UDP_LOAD` and kernel SRv6 support. A 1518-byte
single-tag frame then carries 1388 bytes of UDP payload. The harness records
IPv6 counters and SRv6 route counters alongside the existing snapshots.
Check actual offered rates: kernel encapsulation can saturate a generator
before it reaches the requested rate. Compare `SEGMENTS=1` with aggregates
to distinguish burst losses from sustained capacity.

Use `WORKERS=2`, `4` or `8` to test the deployment core budget as well as
the local gate. Snapshots include per-thread voluntary and involuntary context
switches beside CPU ticks.

`INNER_BRIDGE=1` moves the load generator into another namespace and forwards
its frames over a veth and bridge to the TAP. VLAN devices sit on the generator's
veth instead of the TAP. With `UDP_LOAD` and `SEGMENTS=1`, the harness disables
source checksum/segmentation offloads and bridge-port GRO. This completes UDP
checksums before ingress, as an ordinary NIC would, instead of carrying veth's
partial-checksum metadata through the bridge. Offload flags are recorded and
checksum completion is checked. Other rows retain their existing offloads;
TCP segmentation remains available for reactive tests. The default keeps the original direct-TAP
topology. Record per-worker input counts: a bridge alone does not guarantee
that QinQ reaches several TAP readers.

`OUTER_ROUTED=1` puts the daemons in separate network namespaces, joined
through a private IPv6 router and 1500-byte veth links. The harness disables
UDP segmentation on the endpoint links and GRO on the router links, so netem
sees individual outer datagrams. It verifies the endpoint offload flags and
records them. All links, routes, neighbors and forwarding settings belong to
the harness's private namespaces. This mode requires `ethtool`.

`OUTER_DELAY_US` adds a fixed one-way delay: 2250 models 4.5 ms added RTT,
and 3750 models 7.5 ms. `OUTER_LOSS_PERMILLE` sets random loss: 1, 10 and
50 mean 0.1%, 1% and 5%. Both default to zero. The delay range is 0–1000000
microseconds and the loss range is 0–1000; use decimal integers without leading
zeroes. `OUTER_LOSS_BURST`, 1–100 and default 1, makes the same mean loss
come in bursts: above 1, netem's Gilbert model loses every datagram in its bad
state, which lasts that many datagrams on average, and the loss rate must stay
below 100%. `OUTER_REORDER_PERMILLE`, 0–1000, sends that share of datagrams
without the delay, so they overtake earlier ones; it needs a nonzero delay.
`OUTER_RATE_MBIT` puts a token-bucket bottleneck of that rate on each outer
namespace's egress, before the router's delay line, and `OUTER_QUEUE_PACKETS`
(default 8192) sets its queue in 1500-byte packets; `bottleneck-left` and
`bottleneck-right` snapshots record its drops. (Netem's own `limit` also
counts packets waiting out the delay, so it cannot model a queue.)
With `OUTER_ROUTED=1`, `COMPETING_TCP=1` runs one netcat TCP stream from the
left outer namespace to the right one for each load point, sharing that
bottleneck with the tunnel; `competing-bytes` holds the bytes received in the
point's seconds.
Flow j runs from port 51000 + j to 52000 + j in both directions by default.
tun's symmetric flow hash then gives both directions of flow j the same hash,
so tun reads a flow on the queue the daemon last wrote its other direction on.
`REVERSE_PORT_OFFSET`, for example 2000, moves the right-to-left flows to
other ports so each direction hashes on its own.
Each snapshot's `memory` file records both daemons' VmHWM and VmRSS.
`DAEMON_TOML`, when set, starts both daemon configurations, for
example `[daemon]` options. Netem has an 8192-entry queue on each router egress. Qdisc snapshots
record drops and backlog; drops can include queue overflow as well as the
configured random loss.

Without `OUTER_ROUTED=1`, these impairment settings apply to loopback and may
drop whole GSO aggregates. That mode measures correlated aggregate loss,
not independent datagram loss. Do not compare the two topologies without their
own zero-loss controls. Each output records the path settings; routed snapshots
also include both endpoint IPv6 counters and both router qdiscs.
`qdisc-left` affects R→L and `qdisc-right` affects L→R.

`GENERATOR_PROCESSES=1|2|4|8` (with `UDP_LOAD`) splits each direction's flows
across that many sender and receiver processes on consecutive port ranges.
Process `k` uses the `k`-th CPU of a single `A-B` CPU range, so separate
processes do not share a core; other CPU lists are shared. Process 0 keeps the
original `client.json` and `server.json` names; process `k` adds `-k`. One
individual-frame sender saturates near 2.5 Gbit/s on the bridged path, so use
four processes to offer 5 Gbit/s. `examples/sweep_report.rs` sums every
process and reports one record per point plus median, minimum and maximum
over repeats for a `ROOT/BUILD/COMBO/REPEAT/POINT` tree.

`SESSION_COOKIE` overrides the session UUID for queue-placement controls;
record it when comparing runs because it affects receive-worker assignment.

`WORKERS=4` is the default; `WORKERS=2` and `WORKERS=8` also change the lane and queue
counts. `DAEMON_LEFT_CPUS` and `DAEMON_RIGHT_CPUS` override daemon placement.
The existing `GENERATOR_*_CPUS` and `RECEIVER_*_CPUS` variables select the
four load-process CPU sets. Explicitly choose non-overlapping physical cores
on one socket. The four-worker defaults use daemon CPUs 0-7 and load CPUs 8-19.
The two-worker defaults use daemon CPUs 0–3 and load CPUs 4–7.
The eight-worker defaults use daemon CPUs 0-15 and one load process on each of
CPUs 16-19. Verify the host's topology before using any layout.

`TAP_TXQUEUELEN` sets the test TAP queue length (1–10,000; default 10,000).
With USO input, each entry can hold a whole super-frame, so record this value
and compare shorter queues before attributing a decline to the transport.

The runtime enables checksum offload and completes partial checksums after
TAP reads. With USO support, UDP_SEGMENT super-frames reach the runtime in
one read and undergo bounded userspace segmentation. Older kernels use the
checksum-only fallback and segment before TAP reads.
The kernel test also covers the older behavior: UDP_SEGMENT fails with EIO
when all TAP read offloads are disabled. The direct veth measurements in
BENCH.md establish harness capacity, not tunnel throughput.

The Linux interfaces are described in [udp(7)](https://man7.org/linux/man-pages/man7/udp.7.html).
The checksum prerequisite is visible in
[udp_send_skb](https://github.com/torvalds/linux/blob/v6.8/net/ipv4/udp.c#L854-L873).
Linux 6.8 requires both USO flags in
[TUNSETOFFLOAD](https://github.com/torvalds/linux/blob/v6.8/drivers/net/tun.c#L2752-L2755).

## Checks

`cargo test --release --target x86_64-unknown-linux-musl --example udp_load`
checks CLI bounds, integer pacing, duplicate-window behavior and ancillary
validation. Run its ignored tests inside a disposable container with
iproute2, tun and network capabilities. They check both IP families, four
concurrent sockets, ordinary and segmented sends, GRO, small/MTU/jumbo
payloads, byte corruption and the TAP offload prerequisite.
