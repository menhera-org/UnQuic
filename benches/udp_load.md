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

Receivers expand GRO messages before validating each datagram's run ID,
sequence number, length and entire payload. Each flow tracks 128 blocks of
64 sequence numbers. Reordering within the window does not count as loss.
`too_old` reports packets outside the window whose duplicate status cannot
be established. Such packets are excluded from unique delivery; a nonzero
count invalidates an exact loss claim. `invalid` includes corrupt payloads,
wrong run IDs, and truncated or invalid ancillary data. Both counts must
be zero for an acceptance run.

The JSON output includes process CPU time, syscall counts, blocked sends,
GRO messages, observed socket overflow and the actual receive-buffer limit.
Overflow is in kernel SKB units, not datagrams, and its final cumulative value
is available only when the kernel attaches it to a received message.

Match sender and receiver flow entries to compute full-run missing packets:
`sent - unique`. Receiver `prefix_missing` covers only through its highest
observed sequence and cannot see a lost tail. Compute delivered payload
goodput as `unique * payload_bytes * 8 / sender.seconds`. Report duplicates,
reordering, invalid and too-old counts separately. The socket layer also
checks UDP checksums.

## TAP harness

`benches/tap_transport.sh` retains iperf3 by default. Set `UDP_LOAD` to the
absolute tool path and use integer per-flow rates in `BITRATE` to select this
tool. For example, 16 flows at `1312500000` bits/s request 21 Gbit/s per
direction. `SEGMENTS` defaults to 64. The two receivers run two seconds longer
than the senders to include the tail. `sent.*` snapshots follow sender exit;
`after.*` snapshots follow receiver exit. Use the former for load-interval
CPU and the latter for final drop attribution. A failed child fails the run.

`WORKERS=4` is the default; `WORKERS=8` also changes the lane and queue
counts. `DAEMON_LEFT_CPUS` and `DAEMON_RIGHT_CPUS` override daemon placement.
The existing `GENERATOR_*_CPUS` and `RECEIVER_*_CPUS` variables select the
four load-process CPU sets. Explicitly choose non-overlapping physical cores
on one socket. The four-worker defaults use daemon CPUs 0-7 and load CPUs 8-19. The
eight-worker defaults use daemon CPUs 0-15 and one load process on each of
CPUs 16-19. Verify the host's topology before using either layout.

The runtime enables checksum offload and completes partial checksums after
TAP reads. UDP_SEGMENT senders work, but the kernel still segments their
super-frames before TAP reads. Production super-frame input remains unfinished.
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
