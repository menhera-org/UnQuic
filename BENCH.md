# Release transport overload investigation

The original receive-admission collapse and a later TAP readiness busy loop
are fixed. Bounded UDP super-frame input and conditional worker-local lanes
deliver 16.57 / 15.60 Gbit/s paced and 16.50 / 16.45 unlimited in the latest
60-second eight-worker pair, using 500-entry test TAP queues. A preceding
20-second pair reached 17.95 / 17.07 unlimited. The longer run still declines
0.45% in one direction; earlier short repeats declined 2–3%, and 10,000-entry
queues produced larger declines. Overload stability remains unresolved.

These sweeps do not establish a plateau for every workload or the
theoretical CPU ceiling. Neither the local **20 Gbit/s in each direction**
gate nor the real **5 Gbit/s under loss** gate has passed.
Split-pool, deadline-cache and first-reader lane-association trials were rejected
and reverted after measurements.

This report separates that remaining decline from the earlier near-total
collapse. Recovery, congestion control and PMTU discovery remain unfinished;
their absence does not explain away a scheduling or admission failure.

## Current performance gaps

Status after the TAP readiness fix, per-peer payload limit, local TAP USO
bounded read rings, partial-checksum completion, UDP super-frame input and
conditional worker-local lanes for unfragmented UDP.
The split-pool, deadline-cache and lane-association experiments below are not
in the runtime.
The latest segmented-send results use 12 physical cores with four workers per
daemon, or 20 with eight workers, and the capped socket-buffer fallback below.
L→R / R→L pairs describe simultaneous traffic, not separate one-way peaks.

| Requirement | Current evidence | Remaining gap |
| --- | --- | --- |
| At least 20 Gbit/s each way at inner MTU 1500 | The UDP-only local-lane build delivers 16.50 / 16.45 Gbit/s unlimited over 60 seconds with eight workers and 500-entry test TAP queues, from 28.39 / 27.71 Gbit/s offered; a prior 20-second pair reached 17.95 / 17.07 | Still about 1.21× / 1.22× short in the longer pair. TCP super-frame input remains unfinished. |
| Goodput must plateau under excess traffic | The 60-second UDP-only pair declines 0.45% L→R under unlimited load | Shorter test queues reduce the long tail but do not establish a plateau: earlier left-side repeats lose 2–3%. With 10,000-entry queues, one pair loses 8.3%. Repeated longer runs, tagged/mixed traffic and more flows/sessions remain unverified. |
| Small-frame overload must remain efficient | The UDP-only local-lane build delivers 6.05M / 6.08M 64-byte frames/s with eight workers | The local-lane paced/unlimited sweep improves under overload, with high loss. Mixed and non-coalescible traffic still need coverage. |
| VLAN-heavy deployment | Current throughput sweeps use untagged IP frames. Input UDP segmentation and lane selection parse VLAN tags, but TAP output UDP coalescing currently accepts only untagged IPv4/IPv6 | Tagged frames take the ordinary output path. Benchmark 802.1Q and QinQ as primary workloads and extend checked coalescing before claiming representative throughput. |
| Establish the CPU-limited forwarding ceiling | Worker/process CPU and per-core ticks are recorded; a sampled profile identifies TAP I/O and polling costs | No theoretical or measured zero-loss CPU ceiling has been established. Socket placement, scheduling, cross-worker work and kernel costs still need controlled attribution. |
| Prevent split-count amplification of outer loss | Planned GSO batches end at frame boundaries, and pressure tests preserve useful tails | Runtime bounded selective repair is not implemented. Independent packet loss can still lose a split frame; aggregate-boundary alignment is not a substitute for repair. |
| Use larger confirmed path MTUs | The runtime accepts a per-peer 1232–1452-byte payload assertion; a controlled 1452-byte sweep shows only a modest change | Per-lane PMTU discovery and validation on larger and changing paths remain unimplemented. Configured capacity is not confirmed PMTU. |
| At least 5 Gbit/s over routed IPv6 10G-EPON, RTT 4.5–7.5 ms, with nonzero loss | These results are unimpeded local loopback tests | No real-path result or independent/burst loss sweep verifies this target. Recovery, congestion control and pacing also remain unfinished. |

Loss under unlimited load is still high: the latest UDP-only eight-worker
60-second MTU-1500 point misses 41.71% / 40.45% of offered frames despite zero
admission pressure and worker queue drops. TAP ingress drops, outer socket overflows
and inner UDP receive-buffer errors are recorded. Their different units prevent
an exact frame-loss decomposition.

Earlier single-socket runs still declined. Calibrated profiling then found
TAP wakeups with no available transmit buffer; the final follow-up below verifies
their removal. Missing congestion control, recovery and PMTU remain separate
production-performance gaps.

## Local acceptance gate

The local gate uses five 60-second bidirectional release runs at inner MTU
1500, with 16 flows per direction and 21 Gbit/s paced offered load per
direction. Every run must deliver at least 20 Gbit/s of UDP payload each way
with fewer than 1% missing frames. Unlimited-load delivery must be no lower
than paced delivery. Report actual offered load: a generator that cannot
reach the requested rate cannot establish the gate.

Record daemon and generator CPU, per-core IRQ/softirq time, and all transport,
socket, TAP and receiver drop counters. Keep the four-worker results as a
comparison; also test eight workers, queues and lanes per daemon with explicit
CPU placement. These criteria are not yet met.

## Profile and method

| Field | Value |
| --- | --- |
| Date | 2026-11-05 |
| Before | `a27d8e9` plus counter/snapshot observability; original receive/packing behavior |
| After | `a69f88d`, including `a4c6a7d` and `9db630c` |
| Build | Rust 1.99.1, all features, x86_64-unknown-linux-musl, release: opt-level 3, full LTO, strip, one codegen unit |
| Kernel / CPU | Linux 6.8.0-147-generic; Intel Xeon Gold 6138, nominal 2.00 GHz |
| Transport | Two daemons, one session each; four workers, four TAP queues and four TX lanes per daemon |
| Outer path | IPv6 UDP loopback inside a disposable container namespace; 1232-byte UDP ceiling; GSO/GRO |
| Inner path | IPv4 UDP through two TAPs in separate namespaces, MTU 1500, DF, fixed neighbors; no direct inner path |
| Traffic | iperf3 3.20; two simultaneous unidirectional client/server pairs; 1 or 16 flows each way |
| Sampling | Ten seconds per point, no warmup; both daemons stay running throughout each rate sweep |
| CPU placement | Daemons: left 0–3, right 4–7. Senders: left 8–11, right 16–19. Receivers: left 12–15, right 20–23 |
| Topology | CPUs 0–19 are on socket 0; receiver CPUs 20–23 are on socket 1. Shared local machine; no exclusive CPU reservation |
| Impairment | No injected delay or loss; overload still drops traffic |

Each sweep retains its source ports and daemons, so a later unlimited point
cannot hide poisoned reassembly state by restarting the transport. Source
ports start at 51000 and increment per stream. Independent sweeps create new
outer sockets: reuseport placement can differ. They are not paired trials
with identical kernel hash placement.

Goodput is received **UDP payload**, excluding all headers. Frame sizes exclude
FCS; payload length is frame length minus 42 bytes. Offered rate uses actual
sender packet counts and durations, rather than assuming iperf reached its
requested rate. Missing percentage is `100 * (1 - received / sent)`, including
missing tails. The receiver duration determines goodput. CPU snapshots also
include connection setup/teardown, which can extend the interval after overload.

The old `--bidir` generator shared sending and receiving CPU sets. Diagnostics
found hundreds of thousands of inner UDP receive-buffer errors under unlimited
load. The current four-process setup uses separate CPU sets and records those
errors. It does not assume that every remaining loss belongs to the transport.

## Verified causes and changes

- The 64 receive slots were shared by incomplete and completed frames. A large
  GRO batch could fill them before TAP delivery. Receive work now yields after
  64 expanded datagrams, 64 records or 64 KiB, with TAP delivery interleaved.
  The last datagram is indivisible and may exceed the record/byte target.
- Whole-datagram reservation rejected useful tails when a new head could not
  obtain a slot. Fully validated tails now progress in existing slots. The
  packet remains unacknowledged until all nonterminal DATA is retained; a
  drain that frees slots permits one retry.
- A GSO aggregate could end in the middle of a frame. Dropping that aggregate
  left incomplete frames occupying slots until expiry. Packing now ends a
  planned GSO batch at a frame boundary. The regression drops alternate
  aggregates and verifies that retained batches leave no orphan frames.
  Individual on-wire packet loss still requires bounded repair.
- Receive backlog could consume the shared buffer pool and starve transmit
  batching. Eight of the existing 128 buffers per worker are reserved for RX;
  RX cannot consume the TX share. No pool or reassembly limit was enlarged.
- A 128-record receive allowance competed with a 64-frame TAP-read allowance.
  Small-frame reverse delivery fell under load. The 64-record allowance removes
  that particular imbalance.
- Reuseport collisions could register several receive lanes on one worker.
  Lane ownership now distributes consecutive lane IDs independently of the
  socket that first receives them. The collision regression checks handoff,
  installation and complete buffer return.
- An idle worker could repeatedly poll a readable socket while all its RX
  buffers were held by other workers. Read interest is now suppressed in that
  state and restored after buffer return. Existing handoff notifications wake
  the worker; a regression checks that readiness resumes.

The 64-slot limit and 100 ms frame lifetime are unchanged. Tests cover
over-capacity bursts, expanded GRO budgets, packed tail/head pressure, TAP
backpressure, duplicate suppression and ACK exclusion. The release container
storm checks integrity and zero allocations/deallocations during forwarding.
The full release suite, container worker/runtime cases, Clippy and formatting
pass for the current fixes.

## MTU-1500 rate sweeps

All pairs are L→R / R→L except process CPU, which is left / right.
Requested rate is **per flow**, so 125M with 16 flows requests 2 Gbit/s each way.
CPU uses one core as 100%; each daemon can use four cores.

| Revision | Flows | Requested/flow | Actual offered Gbit/s | Delivered Gbit/s | Missing % | Daemon CPU % |
| --- | ---: | --- | ---: | ---: | ---: | ---: |
| before | 1 | 100M | 0.10 / 0.10 | 0.100 / 0.100 | 0.00 / 0.00 | 40.4 / 24.9 |
| after | 1 | 100M | 0.10 / 0.10 | 0.100 / 0.100 | 0.00 / 0.00 | 41.3 / 39.9 |
| before | 1 | 1G | 1.00 / 1.00 | 0.986 / 0.803 | 1.34 / 19.67 | 97.7 / 90.9 |
| after | 1 | 1G | 1.00 / 1.00 | 0.992 / 0.986 | 0.77 / 1.36 | 102.5 / 112.3 |
| before | 1 | 2G | 2.00 / 2.00 | 0.007 / 0.009 | 99.66 / 99.55 | 116.4 / 79.6 |
| after | 1 | 2G | 2.00 / 2.00 | 1.207 / 1.377 | 39.64 / 31.15 | 100.0 / 103.9 |
| before | 1 | 3G | 3.00 / 3.00 | 0.004 / 0.005 | 99.88 / 99.84 | 133.4 / 92.9 |
| after | 1 | 3G | 3.00 / 3.00 | 1.191 / 1.388 | 59.50 / 53.72 | 96.6 / 102.0 |
| before | 1 | unlimited | 4.12 / 3.74 | 0.005 / 0.012 | 99.88 / 99.67 | 134.6 / 96.5 |
| after | 1 | unlimited | 3.93 / 3.96 | 1.249 / 1.315 | 68.18 / 66.09 | 96.1 / 102.3 |
| before | 16 | 125M | 2.00 / 2.00 | 1.234 / 1.985 | 37.03 / 0.74 | 309.0 / 297.4 |
| after | 16 | 125M | 2.00 / 2.00 | 1.958 / 1.906 | 2.07 / 4.71 | 322.5 / 341.6 |
| before | 16 | 250M | 4.00 / 4.00 | 1.008 / 1.889 | 74.79 / 52.78 | 330.4 / 301.9 |
| after | 16 | 250M | 4.00 / 4.00 | 2.197 / 3.209 | 43.95 / 19.77 | 344.8 / 333.4 |
| before | 16 | 500M | 8.00 / 8.00 | 0.805 / 0.071 | 89.73 / 99.09 | 348.6 / 319.6 |
| after | 16 | 500M | 8.00 / 8.00 | 2.398 / 3.383 | 69.41 / 57.70 | 359.4 / 349.2 |
| before | 16 | unlimited | 12.68 / 12.72 | 0.143 / 0.117 | 98.85 / 99.06 | 319.7 / 294.2 |
| after | 16 | unlimited | 13.21 / 12.90 | 2.569 / 3.081 | 80.55 / 76.10 | 286.1 / 289.3 |

Repeated unlimited points after another paced overload point delivered
1.260 / 1.434 Gbit/s for one flow and 2.441 / 3.000 Gbit/s for 16 flows.
The 16-flow paced 500M points delivered 2.398 / 3.383 and 2.512 / 3.441 Gbit/s.
The reverse-direction decline is still material; these results do not establish
the theoretical CPU maximum or a load-independent plateau.

## Small frames

These are unlimited-load points from `a69f88d`. The data file also
contains the preceding paced points and repeated 60-byte overload points.

| Frame bytes | Flows | Delivered frames/s | Delivered Gbit/s | Missing % | Daemon CPU % |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 60 | 1 | 176101 / 176240 | 0.0254 / 0.0254 | 50.77 / 51.06 | 93.3 / 102.3 |
| 60 | 16 | 321847 / 511860 | 0.0463 / 0.0737 | 73.47 / 57.46 | 369.0 / 346.9 |
| 64 | 1 | 168814 / 169334 | 0.0297 / 0.0298 | 57.27 / 54.44 | 93.4 / 102.6 |
| 64 | 16 | 297062 / 390617 | 0.0523 / 0.0687 | 74.87 / 66.02 | 357.6 / 348.8 |
| 128 | 1 | 166793 / 167195 | 0.1148 / 0.1150 | 50.88 / 56.77 | 97.1 / 102.6 |
| 128 | 16 | 339887 / 301506 | 0.2338 / 0.2074 | 72.19 / 75.17 | 347.5 / 354.3 |

Single-flow runs plateau near 160–178 thousand frames/s. With 60-byte frames
and 16 flows, paced and unlimited saturation points stay around 313–327 thousand
frames/s forward and 508–515 thousand reverse. With 64-byte frames, reverse
delivery falls from 448,939 frames/s at 10M per flow to 390,617 unlimited.
Inner UDP receive-buffer drops are too few to explain that decline. Further
worker/kernel profiling is required.

## Single-socket follow-up

A separate 15-second-per-point sweep uses the same `a69f88d` binary and
transport settings, with all 20 test CPUs on socket 0. Daemons remain on 0–3
and 4–7; left sender/receiver use 8–11 / 12–13, right sender/receiver 14–17 /
18–19. Receiver sets shrink from four CPUs to two, and outer sockets are new.
This is a different measurement profile, not an isolated NUMA A/B comparison.

Both cases have 16 flows each way. Rows retain sweep order; goodput and
frames/s pairs are L→R / R→L, process CPU is left / right.

| Frame bytes | Requested/flow | Delivered Gbit/s | Delivered frames/s | Daemon CPU % |
| ---: | --- | ---: | ---: | ---: |
| 64 | 3M | 0.047 / 0.047 | 269840 / 269875 | 329.1 / 323.1 |
| 64 | 10M | 0.072 / 0.086 | 407914 / 488792 | 360.8 / 327.3 |
| 64 | unlimited | 0.073 / 0.078 | 414808 / 443111 | 354.2 / 337.7 |
| 64 | 10M | 0.070 / 0.085 | 396069 / 485555 | 359.3 / 314.1 |
| 64 | unlimited | 0.078 / 0.083 | 443767 / 469521 | 359.8 / 335.2 |
| 1514 | 125M | 1.972 / 1.912 | 167432 / 162329 | 328.2 / 351.1 |
| 1514 | 250M | 2.738 / 2.279 | 232547 / 193570 | 315.8 / 327.9 |
| 1514 | 500M | 2.613 / 2.099 | 221920 / 178208 | 344.5 / 344.7 |
| 1514 | unlimited | 2.878 / 2.706 | 244354 / 229766 | 291.9 / 279.6 |
| 1514 | 500M | 2.727 / 2.215 | 231554 / 188060 | 351.6 / 348.4 |
| 1514 | unlimited | 2.850 / 2.602 | 241988 / 221000 | 361.7 / 352.2 |

The 64-byte reverse direction falls from 488,792 to 443,111 frames/s on the
first paced/unlimited pair (9.3%), and from 485,555 to 469,521 on the second
(3.3%). Moving all work onto one socket therefore does not eliminate the
observed decline. This does not identify its cause.

MTU-1500 unlimited points deliver 2.878 / 2.706 and 2.850 / 2.602 Gbit/s,
above their preceding 500M-per-flow points. There is no unlimited-load decline
in this particular sweep, but the 24-CPU profile's decline remains unexplained.
Neither profile establishes a universal CPU ceiling or meets the throughput
gate. All follow-up points have zero admission-pressure and worker queue-drop
counts; isolated expiry events and socket overflow remain in the data.

## Drop attribution

The following unlimited MTU-1500 points use **left / right endpoint** order.
Admission pressure, queue drops, expiry and socket overflow are separate
counters. TAP TX drops occur before the daemon reads the inner frame; inner UDP
receive errors occur after TAP delivery.

| Revision / flows | Admission pressure | Worker queue drops | Expired frames | Socket overflow SKBs | TAP TX drops | Inner UDP receive-buffer errors |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| before / 1 | 3763223 / 4342629 | 0 / 0 | 6359 / 6389 | 25258 / 17635 | 60911 / 1282 | 0 / 0 |
| after / 1 | 0 / 0 | 0 / 0 | 0 / 0 | 10641 / 13982 | 1708952 / 1796171 | 82 / 2434 |
| before / 16 | 5010166 / 5246750 | 0 / 0 | 24328 / 23546 | 177214 / 250215 | 2348042 / 4458183 | 0 / 29 |
| after / 16 | 0 / 0 | 0 / 0 | 0 / 1 | 146074 / 139986 | 6266662 / 6044300 | 671 / 1640 |

Socket overflow counts kernel SKBs, which may contain several GSO/GRO datagrams.
It cannot be added to frame expiry or queue counts to obtain missing inner
frames. Expiry combines sender and receiver events. TAP/link counters include
control traffic as well as the measured UDP streams. Counter snapshots can lag
the final overflow until the next successful receive.

The admission-pressure cascade disappears in these after runs. Remaining losses
are visible at TAP ingress and the outer receive sockets, with smaller losses
at the inner receiver. That locates drops; it does not yet explain the remaining
goodput decline. Outer IPv6 fragment creation and reassembly deltas are zero
throughout all 95 recorded points.

## CPU and profiling

The data preserves each core's eight tick deltas. Below, CPU-set entries are
mean **busy%/softirq%**; busy includes user, nice, system, IRQ and softirq.
Per-core data includes unrelated activity and must not be added to process
CPU percentages. Generator CPU is the sum of the local sender and receiver.

| Frame / flows | Generator left/right % | CPUs 0–3 | 4–7 | 8–11 | 12–15 | 16–19 | 20–23 |
| --- | ---: | --- | --- | --- | --- | --- | --- |
| 1514 / 1 | 126.7 / 140.0 | 27.0/0.5 | 26.0/0.4 | 25.0/0.0 | 7.3/0.0 | 25.5/0.0 | 9.8/0.0 |
| 1514 / 16 | 482.0 / 485.5 | 74.3/1.1 | 72.8/1.3 | 77.4/0.2 | 16.8/0.1 | 80.8/0.0 | 16.2/0.0 |
| 60 / 16 | 545.7 / 506.1 | 94.9/1.2 | 87.4/1.6 | 98.1/0.2 | 39.6/0.1 | 98.1/0.0 | 27.0/0.0 |
| 64 / 16 | 502.7 / 498.2 | 92.0/1.3 | 88.2/1.5 | 97.0/0.3 | 29.4/0.1 | 97.3/0.0 | 26.2/0.0 |
| 128 / 16 | 484.0 / 515.5 | 90.7/1.6 | 89.3/1.4 | 97.8/0.3 | 23.5/0.1 | 97.7/0.0 | 30.2/0.0 |

A separate 20-second, 1514-byte, 16-flow profile sampled thread CPU time every
256 worker turns on `9db630c`. TAP reads consumed about 24% of sampled CPU;
TAP writes about 22%; transmit flush about 14%; epoll about 7%. Receive processing
and final TAP flush together accounted for 37%, including those TAP writes.
The profile exposed the idle receive polling fixed in `a69f88d`. Its timings
include startup and sampling overhead and are excluded from throughput tables.
The data retains per-worker samples and turn counts; they are directional
evidence for further profiling, not a cycle-accurate cost model. These old
shares include uncalibrated clock-read costs; use the calibrated follow-up
below for CPU attribution.

The next comparisons must control CPU/socket placement and repeat the remaining
64-byte and MTU-1500 declines. Neither NUMA placement nor absent recovery is a
verified explanation for those declines. Bounded repair remains necessary to
prevent independent outer losses from multiplying inner-frame loss by the split
count. Frame-aligned local GSO batches do not satisfy that requirement.

## Reproduce and inspect

Build the release daemon:

```sh
cargo build --release --all-features --target x86_64-unknown-linux-musl
```

Copy it and [tap_transport.sh](benches/tap_transport.sh) into a disposable
container with no external network, tun, CAP_SYS_ADMIN, CAP_NET_ADMIN,
CAP_NET_RAW, iproute2, iperf3 3.20, taskset, unshare, nsenter and POSIX utilities.
The default CPU sets require CPUs 0–23. Generator/receiver CPU sets can be
overridden with the four environment variables used by the script.

```sh
BITRATE='100M 1G 2G 3G 0 3G 0' \
    sh /root/tap_transport.sh /root/unquic 1514 1 10 /root/one-flow
BITRATE='125M 250M 500M 0 500M 0' \
    sh /root/tap_transport.sh /root/unquic 1514 16 10 /root/many-flows
BITRATE='3M 10M 0' \
    sh /root/tap_transport.sh /root/unquic 64 16 10 /root/small-frames
```

Each output directory must be new. Numbered subdirectories are consecutive
points in one sweep. The script records both client/server JSON pairs, process
and thread ticks, per-core ticks, inner/outer network counters, TAP counters
and SIGUSR1 worker snapshots. It removes its namespaces and processes on exit.
Collect results and delete the disposable container when testing is finished,
including on failure.

[tap_transport-overload.json](benches/tap_transport-overload.json) contains
exact counts, durations, CPU ticks, counter deltas, revision/binary identities
and profile samples for this investigation.
[tap_transport-baseline.json](benches/tap_transport-baseline.json) preserves
the earlier `74adc30` unlimited-load baseline, which used the older
`--bidir` generator profile and is not directly comparable to these sweeps.

## Audit follow-up: contiguous reassembly copy and timer rearming

The common in-order reassembly path now copies each piece as one slice and
clears only the coverage words used by the frame. The overlap path retains
the first bytes received, including an out-of-order tail followed by an
overlapping prefix. Slot-reuse and coverage-boundary regressions cover both
paths.

A pinned release CPU benchmark, `cargo bench --bench reassembly --target
x86_64-unknown-linux-musl`, decodes and admits one million frames per size.
Packets are prepared before timing; completions drain every 16 frames.
Two runs before/after give the following ranges. These exclude socket and
TAP work and do not establish end-to-end throughput.

| Frame bytes | Before ns/frame | After ns/frame |
| ---: | ---: | ---: |
| 60 | 253–256 | 233–245 |
| 64 | 257–279 | 240–252 |
| 128 | 286–293 | 244–253 |
| 1514 | 1056–1194 | 437–446 |
| 9022 | 5517–6093 | 1587–1757 |

The 15-second, 16-flow TAP sweep uses the single-socket CPU placement above.
The binary contains only the reassembly change on top of `ec2939b`; timer
rearming changes are excluded. New sockets mean this is not an isolated
comparison with the previous sweep's reuseport placement.

| Requested/flow | Delivered Gbit/s | Missing % | Daemon CPU % | Socket overflow SKBs |
| --- | ---: | ---: | ---: | ---: |
| 250M | 2.675 / 3.618 | 33.13 / 9.54 | 356.5 / 337.1 | 75,614 / 240,952 |
| 500M | 3.081 / 3.767 | 61.46 / 52.90 | 366.6 / 364.1 | 256,820 / 191,784 |
| unlimited | 3.045 / 3.466 | 76.95 / 73.42 | 360.4 / 352.5 | 240,720 / 198,908 |

Admission-pressure and worker-queue drops are zero in all three points.
Expiry counts are 0 / 0, 2 / 0 and 0 / 1 respectively. Outer fragmentation
and reassembly counters remain zero. Unlimited reverse goodput falls 8.0%
from the paced saturation point: the plateau requirement remains open.
The JSON dataset includes all three points, CPU and drop counters.

Audit item A3 now retains an already armed timer when it will wake at or
before the requested deadline. Cancellation, earlier deadlines, firing and
rearming have kernel regression coverage. Deadlines remain exact: rounding
the shared timer to milliseconds would conflict with the planned
100-microsecond pacing grants. Its transport gain has not yet been measured.

A1 cannot safely assume permanent TAP queue pinning. Linux 6.8's
[`tun_flow_update` and `tun_automq_select_queue`](https://github.com/torvalds/linux/blob/v6.8/drivers/net/tun.c#L445-L509)
can change a flow's queue after receive-side learning. Selecting a lane
solely from the reading worker would change that flow's lane. Stable
steering must be established before replacing the current flow hash.
The audit's per-operation costs combine different runs and are estimates,
not a measured CPU ceiling.

Two record-cache prototypes were rejected after measurement with the same
CPU benchmark. A 64-element decoded-record array took 732 ns per 1514-byte
frame; compact 16-byte DATA descriptors took 532–540 ns. Both were slower
than the retained implementation's 437–446 ns. These experiments do not prove
every possible cache slower; they rule out adopting these two layouts
without further work. Neither prototype is in the runtime.

Counter publication now occurs after one millisecond or 256 turns, rather
than every turn; shutdown always publishes. A quiet worker publishes remaining
totals on its next wake, so these snapshots are not an immediate completion
barrier. The release forwarding/reconnect test still passes its allocation
and counter checks.

The timer/counter follow-up uses the same 15-second, 16-flow, single-socket
profile, with fresh sockets and alternating paced/unlimited points:

| Requested/flow | Delivered Gbit/s | Missing % | Daemon CPU % | Socket overflow SKBs |
| --- | ---: | ---: | ---: | ---: |
| 500M | 2.757 / 3.776 | 65.52 / 52.79 | 356.2 / 358.6 | 323,085 / 205,708 |
| unlimited | 3.054 / 3.486 | 76.88 / 72.82 | 296.6 / 294.2 | 247,826 / 147,674 |
| 500M | 2.809 / 3.635 | 64.87 / 53.94 | 359.6 / 355.0 | 310,715 / 188,256 |
| unlimited | 2.486 / 3.008 | 80.64 / 75.17 | 328.3 / 339.0 | 332,164 / 174,194 |

Admission pressure and worker queue drops remain zero. Expiry counts are
0 / 0, 0 / 5, 2 / 0 and 100 / 165. Fragmentation counters remain zero.
The second unlimited point declines in both directions. These results do
not establish a transport gain from timer/counter changes.

## Audit follow-up: kernel queues

Receive sockets request 8 MiB with `SO_RCVBUFFORCE`, fall back to
`SO_RCVBUF` on `EPERM`, and report the fallback count and actual kernel
accounting capacity. Other option failures fail socket setup and contribute
to socket-error counters. The benchmark sets only its own TAPs'
`txqueuelen` to 10,000 before bringing them up.

Every receive socket in this run used the fallback and reported 425,984
bytes. This verifies the capped fallback, **not** an 8 MiB queue. The
privileged success branch remains unverified here. No host sysctl was changed.
The same 15-second single-socket profile gives:

| Requested/flow | Delivered Gbit/s | Missing % | Daemon CPU % | Socket overflow SKBs |
| --- | ---: | ---: | ---: | ---: |
| 250M | 2.729 / 3.593 | 30.84 / 10.16 | 351.3 / 344.0 | 56,508 / 186,339 |
| 500M | 2.985 / 4.297 | 62.50 / 46.09 | 360.6 / 353.2 | 175,386 / 185,752 |
| unlimited | 3.069 / 4.200 | 76.01 / 67.00 | 365.8 / 366.1 | 189,733 / 176,080 |
| 500M | 3.069 / 4.347 | 61.12 / 45.65 | 364.0 / 358.8 | 160,108 / 174,041 |
| unlimited | 3.058 / 4.093 | 76.33 / 67.05 | 369.3 / 361.3 | 171,086 / 159,876 |

Admission pressure and worker queue drops remain zero. Expiry counts are
0 / 0, 0 / 1, 156 / 0, 0 / 0 and 297 / 358. No outer fragmentation or
reassembly is recorded. Unlimited reverse delivery still declines 2.3% and
5.8% from the preceding paced points. Larger queues have not established
the plateau or throughput gate; differences from earlier sweeps also include
fresh reuseport placement.

A small Rust kernel probe verified classic reuseport BPF payload offsets
36 (lane) and 16 (cookie low word), socket indexes in bind order, and
32-bit wrapping addition before modulo. The production owner calculation
must use the same wrapping arithmetic. This probe does not establish group
lifecycle correctness. Loading an eBPF socket-filter program returned
`EPERM`; `io_uring_setup` succeeded. TAP USO and io_uring read costs were subsequently measured in the B0 follow-up below.

### Audit work still open

A3 is implemented with exact deadlines; A4's counter publication and A8's
queue setup are implemented. A9's contiguous copy is implemented; the two
record-cache prototypes were slower and were discarded. C4's gate is specified
above. A1's immutable lane-association trial below was rejected; strict pinning
still needs stable steering for dependable locality. A2 now has ordered group
creation, kernel steering and lifecycle tests. The remaining A4 scheduling
changes, A6–A7, B1–B3 and the C1–C3 generator/CPU work remain unfinished.
B0's isolated TAP cost probes are recorded below. B4 remains deferred as requested.

## Audit follow-up: receive steering

The control worker binds all reuseport sockets in worker order and transfers
them through bounded queues. Failed binds discard the partial group. A full
handoff queue retains its socket; no connection starts before all members are
installed. Classic BPF uses cookie/lane fields with the same 32-bit wrapping
arithmetic as registration. Ordinary forwarding remains available when steering
fails, during initial registration, and when a session uses fewer workers.

Three- and four-member socket tests check bind-order selection and wrapping
arithmetic. Worker tests cover bind retry, full handoff queues, stable installed
descriptors and registration on the designated owner. Release/default tests,
Clippy and all container worker, runtime, foundation and CLI cases pass,
including the forwarding storm's allocation checks.

The 15-second, 16-flow single-socket sweep retains the queue settings from the
previous run:

| Requested/flow | Delivered Gbit/s | Missing % | Daemon CPU % | Socket overflow SKBs |
| --- | ---: | ---: | ---: | ---: |
| 125M | 1.996 / 1.985 | 0.19 / 0.75 | 319.0 / 334.8 | 8,594 / 1,088 |
| 250M | 3.360 / 3.875 | 15.90 / 3.11 | 353.4 / 357.3 | 31,212 / 50,669 |
| 500M | 3.087 / 4.823 | 61.25 / 39.71 | 367.0 / 369.2 | 81,553 / 42,064 |
| unlimited | 3.486 / 4.861 | 72.13 / 61.88 | 364.8 / 379.7 | 98,226 / 37,173 |
| 500M | 3.150 / 4.741 | 60.08 / 40.73 | 363.6 / 362.5 | 83,767 / 57,489 |
| unlimited | 2.756 / 3.527 | 77.90 / 71.89 | 363.2 / 370.9 | 86,533 / 51,684 |

Every worker received traffic at every point. Socket errors, worker queue drops
and admission-pressure rejections are zero throughout; the JSON includes
per-worker distribution checks. Expiry counts are 0 / 0, 0 / 0, 0 / 5,
0 / 0, 624 / 53 and 1 / 1. Outer fragmentation counters remain zero.

The last unlimited point falls 12.5% / 25.6% below the preceding paced point.
Inner UDP receive-buffer errors are only 6,655 / 2,456 frames, too few to
explain that decline. TAP ingress accepts more frames than at the previous
unlimited point (7.79 / 8.94 million versus 5.79 / 7.54 million), while
transport delivery falls. That observation warrants profiling the balance
between transmit intake and receive delivery. It does not establish that
balance as the cause. Fewer socket-overflow SKBs alone do not imply fewer
lost logical datagrams because aggregate sizes can differ.

## TAP readiness under transmit-buffer pressure

A readable TAP stayed in epoll's read set after its transmit buffer share was
exhausted. Under unlimited traffic, 89.8–90.2% / 92.0–92.3% of TAP-read
dispatches could not obtain a buffer. Returning immediately from `read_tap`
did not clear level-triggered readiness, so epoll repeatedly woke the worker.

Workers now suppress TAP read interest until a transmit buffer is available.
TAP write readiness remains active for backpressure; writable-only events no
longer call `read_tap`. Existing buffer-return notifications wake the owner.
The change adds no buffer storage, allocations, locks or shorter deadlines.

Kernel regressions verify read suppression, resumption after buffer return,
and `EPOLLOUT` delivery while reads are disabled. Release/default suites,
Clippy, the worker regressions and the forwarding/reconnect allocation test
pass.

[Calibrated profile data](benches/worker-turn-profile.json) records CPU stages
every 256 turns and counts all TAP dispatches in complete one-second bins.
The clock baseline is measured with consecutive thread-CPU clock reads and
subtracted after aggregation. Tiny negative dispatch residuals are retained
as measurement noise. Nested TAP-write timing is disabled. Uncalibrated
prototypes overestimated CPU and are excluded from quantitative attribution.

| Unlimited-load diagnostic | Before, left / right | After, left / right |
| --- | ---: | ---: |
| No-buffer TAP dispatches | 89.8–90.2% / 92.0–92.3% | 0% / 0% |
| Worker turns/second, summed per daemon | 147–155k / 233–246k | 21k / 29k |
| Estimated epoll share of thread CPU | about 5% / 7.4–7.5% | about 1.1% / 1.6% |

These instrumented runs establish the polling waste and its removal; they
are separate from production throughput measurements. A discarded after-fix
diagnostic run overlapped compilation and is not used in the comparison.

The following normal release runs used 16 flows, 20 seconds per point and the
same single-socket CPU and queue settings. No build/test work from this
investigation ran concurrently. Directions stay L→R / R→L; CPU and socket
overflow stay left / right.

| Frame bytes | Requested/flow | Delivered | Missing % | Daemon CPU % | Socket overflow SKBs |
| ---: | --- | ---: | ---: | ---: | ---: |
| 1514 | 500M | 3.195 / 5.034 Gbit/s | 59.92 / 37.06 | 375.2 / 371.2 | 79,802 / 64,579 |
| 1514 | unlimited | 3.674 / 5.071 Gbit/s | 71.69 / 59.46 | 348.5 / 342.8 | 59,116 / 48,618 |
| 1514 | 500M | 3.123 / 4.944 Gbit/s | 60.56 / 38.03 | 370.7 / 368.5 | 88,037 / 74,714 |
| 1514 | unlimited | 3.766 / 4.977 Gbit/s | 70.72 / 60.28 | 352.0 / 337.3 | 60,824 / 44,396 |
| 64 | 10M | 459,018 / 515,432 frames/s | 49.36 / 43.10 | 366.5 / 361.6 | 45 / 67,460 |
| 64 | unlimited | 486,206 / 522,829 frames/s | 59.44 / 54.95 | 342.2 / 327.7 | 3,307 / 37,282 |
| 64 | 10M | 466,916 / 523,618 frames/s | 48.51 / 42.20 | 364.1 / 354.6 | 1,602 / 47,179 |
| 64 | unlimited | 482,957 / 537,309 frames/s | 59.57 / 54.02 | 343.7 / 330.3 | 2,080 / 32,250 |

Admission-pressure and worker-queue drops are zero in all eight points.
Expiry counts in row order are 3 / 2, 0 / 0, 0 / 0, 141 / 0,
0 / 0, 0 / 268, 0 / 0 and 0 / 0. Outer fragmentation counters stay zero.
Every unlimited point exceeds its preceding paced point in both directions.

The JSON also retains an earlier four-point, 15-second after-fix sweep whose
first point overlapped a Clippy check; its annotation excludes it from this
comparison. The clean repeats support the readiness fix. They do not establish
the theoretical CPU ceiling, the five-run acceptance gate, or loss recovery.

## Configured outer payload comparison (audit A5)

Each peer now accepts `outer_payload_limit = 1232..1452`, default 1232,
through configuration, packing and both send APIs. A larger value asserts
underlay capacity for every destination path; it is not discovered PMTU.
Counters distinguish configured state (5) from baseline (1) and confirmed
(3). Fragmentation stays disabled, and EMSGSIZE still drops the unsent batch
and counts a socket error.

Config tests reject 1231 and 1453 and cover omitted/default and explicit limits.
Existing pack round trips cover both endpoints of the range. The container
runtime test mixes baseline and 1452-byte peers, forwards split 1514-byte
frames and checks session/lane MTU states. The release suite, eight worker
kernel tests, 42-second runtime/reconnect test, Clippy and Rust 1.89 check pass.

The same normal release binary ran both sweeps below: `b5d233e` plus A5,
SHA-256
`3b12cdd5c92c71ffc7bc2e3e686afc7b98ae728f2f97d2635aeca7e47331bb02`.
Each point lasts 20 seconds with 16 flows per direction, inner MTU 1500,
the single-socket 20-CPU profile and the same queue settings. The only
configuration difference is `OUTER_PAYLOAD_LIMIT=1232` or `1452` in
`benches/tap_transport.sh`. No builds or tests ran concurrently. Both
sweeps retain their daemons between rates. The baseline sweep precedes
the 1452 sweep, so this is not a randomized estimate of a small speedup.

Goodput and missing fractions are L→R / R→L; CPU and socket overflow are
left / right. CPU 100% denotes one occupied core.

| UDP ceiling | Requested/flow | Delivered Gbit/s | Missing % | Daemon CPU % | Socket overflow SKBs |
| ---: | --- | ---: | ---: | ---: | ---: |
| 1232 | 125M | 1.995 / 1.987 | 0.25 / 0.67 | 317.2 / 327.0 | 10,973 / 2,183 |
| 1232 | 250M | 3.415 / 3.946 | 14.63 / 1.34 | 360.1 / 355.8 | 19,727 / 96,840 |
| 1232 | 500M | 3.132 / 4.921 | 60.71 / 37.84 | 369.8 / 363.6 | 89,748 / 71,731 |
| 1232 | unlimited | 3.611 / 4.954 | 71.74 / 59.74 | 350.9 / 339.1 | 66,691 / 46,122 |
| 1232 | 500M | 3.254 / 5.060 | 59.33 / 36.57 | 375.5 / 372.7 | 51,392 / 111,501 |
| 1232 | unlimited | 3.853 / 4.952 | 69.70 / 61.02 | 352.7 / 341.2 | 58,416 / 58,017 |
| 1452 | 125M | 1.996 / 1.994 | 0.19 / 0.32 | 322.1 / 332.0 | 5,255 / 1,815 |
| 1452 | 250M | 3.469 / 3.951 | 13.27 / 1.21 | 361.6 / 360.0 | 18,048 / 95,299 |
| 1452 | 500M | 3.321 / 5.055 | 58.35 / 36.81 | 372.9 / 365.1 | 64,692 / 144,685 |
| 1452 | unlimited | 3.867 / 4.955 | 69.26 / 59.84 | 348.0 / 339.9 | 84,764 / 114,525 |
| 1452 | 500M | 3.294 / 5.026 | 58.66 / 37.17 | 365.5 / 367.2 | 62,819 / 133,579 |
| 1452 | unlimited | 3.829 / 5.031 | 69.58 / 59.34 | 350.8 / 342.3 | 77,427 / 122,061 |

Admission-pressure rejections, worker queue drops and socket errors are zero
throughout. Baseline expiry counts are 0 / 0, 0 / 0, 22 / 4, 2 / 0,
0 / 6 and 55 / 2; at 1452, only the second 500M point expires frames
(0 / 2). All outer fragmentation and reassembly counters remain zero.
Socket overflow counts aggregates, so their increase at 1452 cannot be
converted directly into lost inner frames.

| Ceiling, unlimited repeat | Actual offered Gbit/s, L→R / R→L | Generator CPU %, left / right | TAP ingress drops, left / right | Inner UDP buffer errors, left / right |
| --- | ---: | ---: | ---: | ---: |
| 1232, run 1 | 12.82 / 12.35 | 519.1 / 493.1 | 13,891,994 / 10,804,041 | 14,608 / 3,298 |
| 1232, run 2 | 12.75 / 12.75 | 517.1 / 495.8 | 12,908,766 / 11,608,035 | 36,889 / 15,180 |
| 1452, run 1 | 12.71 / 12.38 | 517.5 / 497.1 | 13,205,861 / 11,289,819 | 14,554 / 11,949 |
| 1452, run 2 | 12.64 / 12.42 | 518.1 / 496.5 | 13,065,089 / 11,343,942 | 8,662 / 8,173 |

The 1452-byte unlimited repeats average 3.85 / 4.99 Gbit/s, versus
3.73 / 4.95 at 1232: about 3.1% / 0.8% higher. Individual comparisons vary;
this is not evidence that a larger ceiling solves the throughput gap.
TAP ingress drops and outer socket overflows still dominate visible loss
locations. Their different units and sampling boundaries prevent an exact
loss decomposition. Raw records include receiver loss, process/per-core CPU,
transport counters and kernel counters in the overload JSON.

The strict plateau condition also remains unmet. At 1232, increasing the
paced rate from 250M to 500M per flow reduces L→R delivery by 8.3%;
the corresponding 1452 decrease is 4.3%. One baseline paced-to-unlimited
repeat falls 2.2% in reverse, and one 1452 repeat falls 2.0%.
Neither missing recovery nor absent congestion control is sufficient
attribution. Continue profiling and the remaining pool/scheduling work.

## Split-pool trial rejected (audit A6)

A trial separated each worker's shared pool into 1024 transmit buffers of
9023 bytes and 32 GRO receive buffers of 64 KiB. Startup accounting included
both payload stores and their ownership queues. Cross-worker buffer returns
kept their original pools; transmit reservations still obeyed the per-session
frame and byte limits. Receive work and TAP reads retained their 64-record/
64-read budgets. No deadline was shortened.

The release suite, eight worker kernel regressions and the runtime allocation/
reconnect test passed. Added assertions covered both pool sizes, payload
accounting, independent exhaustion and return through full handoff queues.
Clippy flagged the retained `Vec<Box<Buffer>>` ownership pools after their
inline payload became a boxed slice; that lint was not resolved because
the performance trial was rejected.

The following six 20-second points used the same 1452-byte configuration,
16 flows and CPU/queue placement as A5, with no concurrent builds or tests.
The experimental binary's SHA-256 is
`b89b8d055e17a078867a6bdd1a652944ace8b3bd70847dcc015bbe0dc506de5f`.
Its worker/runtime sources accompany the raw results; the JSON marks this
variant experimental. **The pool expansion was reverted and is not in the
current runtime.**

| Requested/flow | Delivered Gbit/s, L→R / R→L | Missing % | Daemon CPU %, left / right | Worker queue drops, left / right | Expiries, left / right | Socket overflow SKBs, left / right |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 125M | 1.999 / 1.994 | 0.02 / 0.28 | 321.4 / 330.6 | 0 / 0 | 0 / 0 | 4,371 / 190 |
| 250M | 3.474 / 3.950 | 13.16 / 1.26 | 359.9 / 353.2 | 0 / 0 | 76 / 0 | 18,685 / 94,243 |
| 500M | 3.248 / 5.071 | 58.98 / 36.45 | 371.7 / 361.9 | 52 / 4,699 | 1,591 / 849 | 104,261 / 108,317 |
| unlimited | 3.492 / 5.168 | 72.69 / 58.89 | 361.6 / 343.6 | 4,414 / 12,042 | 622 / 201 | 94,937 / 130,170 |
| 500M | 3.309 / 5.012 | 58.58 / 37.34 | 359.1 / 354.1 | 114 / 2,013 | 1,889 / 1,683 | 103,382 / 141,942 |
| unlimited | 3.472 / 5.065 | 73.24 / 59.87 | 349.5 / 333.9 | 7,912 / 21,693 | 958 / 1,391 | 117,637 / 105,407 |

Unlimited goodput averages 3.48 / 5.12 Gbit/s, compared with A5's
3.85 / 4.99: about 9.5% lower forward and 2.5% higher reverse.
Admission-pressure rejections, socket errors and outer fragmentation remain
zero, but worker queue drops and expiries increase. The drop counters alone
do not establish why forward CPU efficiency worsens.

A6 therefore remains open. More storage has not fixed the remaining decline.
Next, profile the pending-frame scans and work admitted versus drained per
turn, then repeat the pool trial after any verified scheduling fix. Preserve
the current bounded admission, ACK and TAP-backpressure behavior throughout.

## Backlog profiling and rejected deadline-cache trial (audit A4)

[Backlog profile data](benches/worker-backlog-profile.json) compares the
unchanged runtime, the rejected split-pool trial and transmit-deadline caching.
It samples thread CPU every 256 turns with a measured clock baseline, and
counts queue work on every turn. Results below aggregate four workers per
endpoint over complete one-second bins. Instrumented throughput is kept
separate from the normal release results.

| Diagnostic | Current-sized pools, before cache | Rejected split-pool trial | Current-sized pools, cached |
| --- | ---: | ---: | ---: |
| TX deadline frame entries examined/second | 0.37–1.11 million | 5.82–13.09 million | 1,329–2,886 |
| Approximate deadline CPU share | −0.02% to 0.14% | 0.70% to 1.73% | −0.15% to −0.03% |
| Largest pending queue on one worker | 353 frames | 3,048 frames | 336 frames |
| Pending examinations rotated past a full/prepared batch | 13–30% | 15–33% | 14–30% |

Negative corrected CPU shares are measurement noise, not negative work.
Caching demonstrably removes repeated scans; this timer cost was too small
to explain the whole pool regression. All diagnostic queue drops in the
split-pool trial came from transmit handoffs, with no receive-queue or
transmit-quota drops. Pending depth and expiry also rose. Those observations
narrow the investigation without establishing a complete causal explanation.

The trial cached an early transmit expiry, lowered it when a frame arrived
with an earlier deadline, and rescanned pending frames and batches when the
cached deadline arrived. A removed frame could leave one harmless early wake.
Handoffs and queue rotation make a front-only deadline incorrect. Frame
lifetimes, receive budgets and ACK admission are unchanged. A kernel regression
covers out-of-order arrival, movement into batches, expiry and early release.
The release suite, all nine worker kernel tests, runtime allocation/reconnect
test, Clippy and Rust 1.89 check pass.

Uninstrumented release measurements below use 1452-byte outer payload, 16 flows,
20 seconds per point and the same single-socket CPU/queue settings.
Rows are chronological: cached sweep, unchanged baseline repeat, cached
repeat. No builds or tests ran concurrently. The cached binary SHA-256 is
`1f53efbfa05f53e3a9b0483ee850aef3866df68de3a1e8ecd1eaa99fd5930a1d`.

| Runtime | Requested/flow | Delivered Gbit/s, L→R / R→L | Missing % | Daemon CPU %, left / right | Socket overflow SKBs, left / right |
| --- | --- | ---: | ---: | ---: | ---: |
| cached | 125M | 1.987 / 1.985 | 0.64 / 0.74 | 315.3 / 326.6 | 11,895 / 6,516 |
| cached | 250M | 3.380 / 3.932 | 15.50 / 1.69 | 350.8 / 349.6 | 23,696 / 107,541 |
| cached | 500M | 3.319 / 4.902 | 58.34 / 38.72 | 360.5 / 353.2 | 81,578 / 132,288 |
| cached | unlimited | 2.925 / 4.672 | 77.22 / 62.30 | 302.7 / 326.1 | 199,424 / 90,988 |
| cached | 500M | 3.402 / 5.007 | 57.31 / 37.40 | 363.7 / 357.9 | 77,410 / 136,070 |
| cached | unlimited | 4.001 / 4.837 | 68.02 / 61.96 | 343.7 / 335.5 | 81,931 / 106,813 |
| unchanged | 500M | 3.405 / 5.171 | 57.43 / 35.22 | 375.7 / 366.4 | 70,208 / 122,601 |
| unchanged | unlimited | 3.841 / 4.875 | 70.17 / 60.73 | 347.1 / 343.0 | 86,869 / 109,611 |
| unchanged | 500M | 3.388 / 5.058 | 57.48 / 36.60 | 367.8 / 362.1 | 80,943 / 135,387 |
| unchanged | unlimited | 3.789 / 5.092 | 69.96 / 59.09 | 355.0 / 341.0 | 82,250 / 118,378 |
| cached | 500M | 3.377 / 4.883 | 57.78 / 38.95 | 355.0 / 350.2 | 103,095 / 131,737 |
| cached | unlimited | 3.414 / 4.611 | 74.07 / 65.82 | 318.1 / 320.3 | 132,562 / 89,094 |
| cached | 500M | 3.379 / 5.022 | 57.60 / 37.05 | 363.6 / 364.3 | 79,951 / 128,507 |
| cached | unlimited | 3.936 / 4.920 | 69.07 / 61.98 | 349.5 / 339.6 | 88,503 / 113,341 |

Admission-pressure rejections, worker queue drops and socket errors are zero
in every point. Expiry ranges from 0 to 88 / 0 to 3 frames per point.
Outer fragmentation remains zero. Full process/per-core CPU, generator CPU,
TAP drops and inner UDP errors remain in the overload JSON.

The slower cached unlimited points coincide with more user/system CPU ticks
on the assigned cores than in the daemon process totals: about 35 and 14
percentage points on the left. Other points have small negative differences.
These counters have different sampling boundaries and accounting scopes;
they identify neither a competing process nor the cause of the decline.
Both points remain in the report.

No throughput gain is established for caching. The unchanged baseline also
has a reverse-direction decline, and the cached runs still fail the strict
plateau requirement. Continue the scheduling/ownership investigation; do not
attribute the unresolved decline to absent congestion control or recovery.

The cached variant also ran four 20-second points with 64-byte Ethernet frames,
16 flows and the default 1232-byte outer payload:

| Requested/flow | Delivered frames/s, L→R / R→L | Missing % | Daemon CPU %, left / right | Socket overflow SKBs, left / right |
| --- | ---: | ---: | ---: | ---: |
| 10M | 457256 / 509137 | 49.54 / 43.99 | 362.1 / 358.3 | 917 / 60,059 |
| unlimited | 459418 / 511070 | 61.96 / 56.49 | 339.3 / 322.3 | 2,209 / 51,239 |
| 10M | 455936 / 510052 | 49.80 / 43.66 | 357.3 / 355.7 | 1,978 / 50,203 |
| unlimited | 484720 / 499875 | 60.95 / 58.92 | 337.9 / 326.4 | 1,597 / 49,459 |

These points have zero admission pressure, worker queue drops, expiry and
outer fragmentation. The final reverse unlimited point declines 2.0%
from its paced predecessor. The trial does not establish a small-frame
throughput gain either.

**The deadline cache was reverted.** Its scan reduction is verified, but
current-pool scan cost is small and the throughput repeats do not justify
adding it now. The tested source remains with the raw results for a future
larger-queue comparison. A4's scheduling work remains open; the next
priority is reducing transmit ownership handoffs while preserving flow pinning.

## Rejected first-reader lane associations (audit A1)

A bounded trial chose a new flow's lane on its first TAP reader and kept that
association when later frames arrived on another queue. Each session used
4096 atomic entries (32 KiB), four probes at most, no eviction and the original
hash rule on probe exhaustion. Immutable entries and fallback preserved
pinning under concurrent insertion, collisions and saturation. Runtime startup
accounted for the storage; the hot path allocated nothing and took no lock.

The release suite, nine worker kernel cases, allocation/reconnect test,
Clippy and Rust 1.89 check passed. New cases moved a flow between simulated
TAP queues and checked buffer return; concurrent insertions and full tables
kept their original lanes. The storm now also asserts zero worker queue
drops. Correctness checks did not establish a performance gain.

Normal release sweeps used 16 flows, 20 seconds per point, four workers per
daemon and the same single-socket placement. The MTU-1500 trials used a
1452-byte outer payload ceiling. No builds or tests ran during measurements.
The trial binary SHA-256 is
`a631745537fab5ac2f85fd6b0c8ff30a570cc58d6fd7aaf18f723012cc76eec0`;
its source accompanies the raw results. The first sweep improved forward
unlimited goodput, but the repeat lost reverse throughput and ended with a
6.6% reverse decline from its preceding paced point.

| Runtime | Requested/flow | Delivered Gbit/s, L→R / R→L | Missing % | Daemon CPU %, left / right | Socket overflow SKBs, left / right |
| --- | --- | ---: | ---: | ---: | ---: |
| trial | 125M | 1.998 / 1.998 | 0.10 / 0.10 | 276.4 / 317.7 | 602 / 1,017 |
| trial | 250M | 3.195 / 3.233 | 20.13 / 19.16 | 329.7 / 360.2 | 65,298 / 56,524 |
| trial | 500M | 4.450 / 4.530 | 44.21 / 43.37 | 367.5 / 376.9 | 32,326 / 131,544 |
| trial | unlimited | 4.896 / 4.807 | 60.27 / 61.50 | 372.9 / 368.8 | 61,508 / 204,295 |
| trial | 500M | 4.622 / 4.643 | 42.01 / 41.76 | 373.2 / 378.2 | 35,701 / 107,159 |
| trial | unlimited | 4.924 / 4.846 | 61.07 / 61.22 | 373.2 / 363.6 | 75,393 / 198,315 |
| trial repeat | 250M | 3.771 / 3.188 | 5.71 / 20.31 | 325.8 / 359.2 | 74,161 / 37,640 |
| trial repeat | 500M | 4.202 / 4.177 | 46.93 / 47.60 | 358.6 / 359.5 | 34,414 / 128,321 |
| trial repeat | unlimited | 4.422 / 4.267 | 65.32 / 66.91 | 342.7 / 337.2 | 76,378 / 170,770 |
| trial repeat | 500M | 4.274 / 4.266 | 46.40 / 46.67 | 360.5 / 363.6 | 36,126 / 112,688 |
| trial repeat | unlimited | 4.459 / 3.986 | 62.50 / 69.12 | 339.8 / 338.3 | 69,358 / 182,260 |

The 64-byte comparison used the default 1232-byte outer ceiling. Forward
unlimited delivery declined 6.7% and 9.9% in the trial. Its forward rates were
also below both subsequent unchanged baseline unlimited points. Reverse
delivery improved, so the change redistributed throughput rather than
providing a dependable gain in both directions.

| Runtime | Requested/flow | Delivered frames/s, L→R / R→L | Missing % | Daemon CPU %, left / right | Socket overflow SKBs, left / right |
| --- | --- | ---: | ---: | ---: | ---: |
| trial | 10M | 383022 / 609213 | 57.77 / 32.98 | 354.8 / 346.8 | 42,414 / 9,178 |
| trial | unlimited | 357475 / 713413 | 70.07 / 41.00 | 362.6 / 326.9 | 68,836 / 46 |
| trial | 10M | 386589 / 611601 | 57.34 / 32.02 | 353.6 / 346.1 | 49,470 / 2,856 |
| trial | unlimited | 348299 / 701002 | 69.97 / 40.85 | 359.6 / 322.6 | 80,118 / 212 |
| unchanged | 10M | 453031 / 503777 | 49.66 / 44.58 | 350.2 / 353.7 | 5,441 / 73,003 |
| unchanged | unlimited | 479686 / 517970 | 58.73 / 57.50 | 335.4 / 326.4 | 1,589 / 35,796 |
| unchanged | 10M | 460797 / 479321 | 49.28 / 47.11 | 344.9 / 339.2 | 1,370 / 72,607 |
| unchanged | unlimited | 418902 / 454667 | 64.24 / 62.50 | 325.0 / 326.0 | 33,123 / 37,701 |

The unchanged baseline's final unlimited point declined 9.1% / 5.1% from
its paced predecessor. The plateau failure therefore persists without the
trial. All 19 normal points have zero admission-pressure rejections, worker
queue drops, socket errors and outer fragmentation. MTU-1500 expiry is at
most 3 / 5 frames per point; all eight small-frame points have zero expiry.
Process/per-core CPU, generator CPU, TAP drops and inner UDP errors are
retained in the overload JSON. These counters identify loss locations but
do not establish the cause of each rate-dependent decline.

A separate [handoff counter profile](benches/worker-handoff-profile.json)
counts every TAP frame over complete one-second bins. It adds no per-frame
clock calls. Instrumented goodput remains separate from normal results.

| Runtime | Requested/flow | TX handoffs as % of TAP frames, left / right | Diagnostic delivered Gbit/s, L→R / R→L |
| --- | --- | ---: | ---: |
| unchanged | 500M | 62.65 / 88.41 | 3.304 / 5.016 |
| unchanged | unlimited | 62.53 / 90.82 | 3.821 / 4.802 |
| trial | 500M | 20.30 / 100.00 | 4.039 / 4.415 |
| trial | unlimited | 17.67 / 100.00 | 4.393 / 4.373 |

There were no failed handoffs in these diagnostic bins. First-reader
associations reduced left-side crossings but nearly every right-side frame
still crossed workers. They do not establish stable TAP placement; the
unchanged tuple can outlive the queue on which its first frame arrived.
The bounded table would also fall back permanently after saturation.
The trial is reverted; the runtime retains deterministic hashing and handoffs.
The user's latest November 5 decision permits relaxing flow pinning where
measurements establish that it harms performance, and retaining it otherwise.
That permits a separate worker-local trial; it does not make this rejected
association table useful. eBPF is excluded from both implementation and future
performance assumptions.

## Direct TAP cost probes (audit B0)

[The probe](benches/tap_cost.rs) measures the four requested TAP operations
with prebuilt 1514-byte Ethernet/IPv4/UDP frames (1472-byte UDP payload).
It creates a private network namespace and fresh nonpersistent TAPs.
Three sequential release runs used CPU 0 on socket 0, with the kernel,
CPU model and build settings above; no builds or other tests ran concurrently.
[All results](benches/tap-cost.json) include CPU timing and whole-loop wall time.
The binary SHA-256 is
`07559fef8982913fa9d20b44b8338f81cd9043a0aab1bcedbaab48b5067047b5`.

Each case runs 8192 batches. Plain reads and writes use 64 frames per batch.
USO writes use one `writev` per aggregate: a 10-byte virtio header, a
42-byte Ethernet/IP/UDP header and borrowed payload slices. TAP checksum and
segmentation offloads for reads stay disabled. Each write is followed by UDP
receives that verify every payload and its position in the batch; the listener
does not enable UDP GRO.

| Operation | Frames per syscall | Thread CPU ns/frame, three runs |
| --- | ---: | ---: |
| Plain prequeued TAP read | 1 | 1184.89 / 1166.16 / 1219.68 |
| io_uring prequeued TAP read | 64 | 599.86 / 604.14 / 721.88 |
| Plain TAP write | 1 | 2409.99 / 2420.98 / 2553.64 |
| USO writev | 1 | 2348.32 / 2335.67 / 2484.66 |
| USO writev | 2 | 1837.19 / 1921.03 / 1844.82 |
| USO writev | 16 | 612.32 / 648.70 / 663.25 |
| USO writev | 43 | 524.11 / 570.38 / 547.17 |
| USO writev | 44 | 505.44 / 520.62 / 534.15 |

Timing uses `CLOCK_THREAD_CPUTIME_ID` around each I/O batch and subtracts
the mean of 10,000 back-to-back clock pairs (938–988 ns in these runs).
Packet construction, generator sends, receiver drains and payload checks
are outside the CPU interval. The wall interval includes those operations.
These are isolated costs with hot buffers, not end-to-end throughput or a
measured production CPU ceiling.

The ring submits 64 `IORING_OP_READ` operations with `RWF_NOWAIT` and
harvests 64 completions per enter. Every populated completion returned one
1514-byte frame with the expected payload. An empty batch returned 64
`EAGAIN` completions; there were no dropped SQEs or CQ overflows, and
the process had one task after both read cases.
Linux 6.8 explicitly marks
[TAP as supporting nonblocking I/O](https://github.com/torvalds/linux/blob/v6.8/drivers/net/tun.c#L3305-L3306).
This verifies useful batching on this kernel; availability on other kernels
and sandbox policies still requires a plain-read fallback.

Plain TAP costs alone would consume roughly 6.1–6.4 core-seconds per daemon
at 20 Gbit/s of UDP payload each way if these per-frame costs persisted:
the payload target requires about 1.70 million frames/s in each direction.
That extrapolation excludes outer transport work and scheduling. The probes
justify evaluating B1 and B2, but do not establish the target as achievable.

USO payload delivery is verified for groups of 1, 2, 16, 43 and 44.
At this probe revision, byte-exact Ethernet/IP reconstruction, short final
segments, IPv6 and mixed traffic had not been verified. Header construction and checksum/coalescibility
checks are excluded from the write timing. The B1 checks below cover those cases; B3 super-frame reads remain unmeasured.
The probe itself did not change the daemon's TAP path.

To reproduce, build the probe, then copy the printed executable path into a
disposable container with `/dev/net/tun`, `CAP_NET_ADMIN`, iproute2 and
taskset. Inside that container run the second command:

```sh
cargo bench --bench tap_cost --all-features --target x86_64-unknown-linux-musl --no-run
taskset -c 0 ./tap_cost --run-in-container
```

Without the explicit argument the probe exits before creating a namespace.
Clippy, formatting and Rust 1.89 compilation pass. The three container runs
completed all payload, completion-count, empty-ring and queue-overflow checks.

## Lossless local TAP USO (audit B1)

Complete compatible UDP frames now share a TAP `writev`. Payloads stay in
their reassembly slots; only the first Ethernet/IP/UDP header is copied.
Grouping preserves per-flow order, caps each write at 64 frames and 65,535
bytes, and never waits for more traffic. Plain writes remain the fallback.
Receive admission, ACK eligibility, frame deadlines and storage limits are
unchanged. Transient writes retain all selected frames; kernel capability
rejection disables USO on that queue.

The initial checksum loop cost 1.051–1.090 µs per 1472 bytes in a standalone
release probe. Exact two-byte chunks reduced it to 120–123 ns, with the same
sum at every length from 0 through 65,534. These are wall-time measurements
over two million calls per case, repeated three times on CPU 0, with hot
buffers. They isolate the loop cost, not end-to-end gain. The final code uses
the equivalent `as_chunks::<2>()` array-slice API; the regression also checks
odd lengths and unaligned input.

The first six-point USO trial retained the slow loop. Its unlimited points
delivered 3.08–3.78 / 4.80–4.94 Gbit/s; the first forward point declined 4.6%
from paced overload. Those results remain in the data file as
`uso_scalar_trial`. The following sweeps use exact-pair accumulation.
They retain the existing 20-second, four-worker, 16-flow, single-socket
profile. No builds or other own tests ran during measurements; CPUs were
not exclusively reserved.

| Frame bytes | Sweep | Requested/flow | Delivered Gbit/s | Missing % | Daemon CPU % |
| ---: | --- | --- | ---: | ---: | ---: |
| 1514 | MTU | 125M | 1.995 / 1.988 | 0.23 / 0.58 | 326.1 / 333.6 |
| 1514 | MTU | 250M | 3.611 / 3.938 | 9.72 / 1.56 | 352.2 / 355.9 |
| 1514 | MTU | 500M | 3.509 / 5.413 | 55.69 / 32.33 | 355.9 / 365.1 |
| 1514 | MTU | unlimited | 4.387 / 5.798 | 66.88 / 55.27 | 342.5 / 335.2 |
| 1514 | MTU | 500M | 3.621 / 5.047 | 54.58 / 36.90 | 343.5 / 346.8 |
| 1514 | MTU | unlimited | 4.311 / 5.686 | 67.29 / 55.50 | 337.1 / 330.1 |
| 1514 | MTU repeat | 500M | 3.123 / 5.015 | 60.68 / 37.01 | 315.6 / 357.7 |
| 1514 | MTU repeat | unlimited | 4.119 / 5.484 | 67.76 / 57.35 | 319.8 / 329.5 |
| 1514 | MTU repeat | 500M | 3.557 / 5.199 | 55.08 / 35.02 | 346.2 / 351.4 |
| 1514 | MTU repeat | unlimited | 4.680 / 5.640 | 64.30 / 56.57 | 343.9 / 332.4 |
| 64 | small | 5M | 0.078 / 0.079 | 2.48 / 1.65 | 354.3 / 362.5 |
| 64 | small | unlimited | 0.130 / 0.126 | 37.76 / 39.48 | 329.0 / 319.4 |
| 64 | small | 5M | 0.077 / 0.078 | 3.68 / 2.53 | 352.6 / 361.2 |
| 64 | small | unlimited | 0.133 / 0.127 | 34.52 / 39.31 | 330.9 / 320.6 |
| 64 | small, high | 10M | 0.107 / 0.113 | 33.07 / 29.24 | 346.2 / 355.9 |
| 64 | small, high | unlimited | 0.114 / 0.128 | 47.59 / 37.16 | 309.6 / 312.8 |
| 64 | small, high | 10M | 0.107 / 0.108 | 33.22 / 32.30 | 341.4 / 344.4 |
| 64 | small, high | unlimited | 0.127 / 0.131 | 40.18 / 38.85 | 324.3 / 319.5 |

The unchanged MTU-1500 baseline preceding this work delivered
3.79–3.84 / 4.87–5.09 Gbit/s under unlimited load. These are independently
started sweeps, so the difference also includes scheduling and kernel flow
placement variation. All normal-build points, actual offered rates, per-core
ticks and drop counters are in [the data file](benches/tap_transport-overload.json).

Under MTU-1500 unlimited load, 97.2–98.0% of delivered frames used USO,
averaging 6.65–7.31 frames per coalesced write. For 64-byte frames, about
99% used USO, averaging 10.2–11.5 frames per coalesced write. No offload
fallbacks or TAP errors occurred. These counters measure actual use of the
optimization, independently of throughput.

All 32 new normal-build points have zero admission-pressure rejections,
worker queue drops and outer IPv6 fragmentation. Loss is still substantial:
in the last exact-pair MTU repeat, TAP TX drops were 12.32 / 11.03 million,
socket overflow was 82,629 / 121,590 SKBs, and inner UDP receive-buffer errors
were 114,791 / 67,647. Expiry was 0 / 256 frames. Those left/right counters
name different stages and units; they cannot be added into a frame-loss total.


The final `as_chunks::<2>()` build repeats the overload comparison at both
frame sizes. Its binary SHA-256 is
`a47b8149bc80dd7c54e59ec1e80c9d5495851845006e3542f879a58b3bf7e638`.

| Frame bytes | Requested/flow | Delivered Gbit/s | Missing % | Daemon CPU % |
| ---: | --- | ---: | ---: | ---: |
| 1514 | 500M | 3.587 / 5.443 | 55.00 / 31.77 | 356.8 / 362.7 |
| 1514 | unlimited | 4.380 / 5.869 | 65.83 / 54.00 | 328.9 / 332.0 |
| 1514 | 500M | 3.564 / 5.442 | 55.31 / 31.97 | 356.2 / 361.8 |
| 1514 | unlimited | 4.561 / 5.904 | 64.42 / 56.07 | 341.2 / 341.1 |
| 64 | 10M | 0.110 / 0.114 | 30.95 / 28.84 | 353.8 / 360.4 |
| 64 | unlimited | 0.128 / 0.126 | 36.43 / 38.40 | 328.1 / 318.1 |
| 64 | 10M | 0.103 / 0.109 | 35.48 / 31.96 | 335.9 / 342.3 |
| 64 | unlimited | 0.130 / 0.129 | 39.59 / 39.80 | 326.0 / 321.8 |

Each final unlimited point exceeds its paced predecessor in both directions.
Small-frame unlimited delivery is 729–737k / 714–734k frames/s. Neither
the five-run 60-second gate nor the real-path target has been tested here.

### Remaining CPU cost

[A diagnostic profile](benches/worker-uso-profile.json) samples one turn in
256, subtracts a clock-pair calibration and retains only complete one-second
bins. It records 19 seconds per point. Its instrumented binary is separate
from all normal-build throughput results.

| Stage | Paced 500M/flow CPU share, left/right % | Unlimited CPU share, left/right % |
| --- | ---: | ---: |
| TAP reads and frame dispatch | 30.0 / 38.8 | 35.4 / 40.4 |
| Receive processing, including interleaved TAP delivery | 26.6 / 15.9 | 25.6 / 20.6 |
| Final TAP delivery | 18.1 / 13.2 | 14.3 / 12.6 |
| Transmit flush | 13.5 / 18.0 | 14.2 / 16.0 |
| epoll wait | 2.1 / 3.2 | 1.5 / 1.7 |

No TAP read call began without a transmit buffer. Estimated sampled CPU was
346% / 350% under unlimited load, against process measurements of 339% / 341%.
The read category includes frame hashing and handoff, so it is not a syscall-only
measurement. B0 independently measured lower cost for batched reads; B2 is the
next experiment. This profile does not establish a theoretical CPU maximum.

### Correctness checks

Kernel tests compare every byte after veth segmentation with ordinary writes
for IPv4 and IPv6, groups of 1, 2, 43 and 44, short final segments, IPv4 ID
wrap, zero-result UDP checksums, the 64-frame limit and the aggregate-byte
limit. They cover header mismatches, invalid checksums, same-flow barriers,
cross-flow grouping, backpressure, short writes, capability rejection and
unexpected virtio metadata. Reassembly tests verify exact slot release,
retained ordering and duplicate suppression.

The full release suite, all 11 ignored kernel unit tests, TAP/socket foundation
test and reconnect/runtime storm pass. The runtime test verifies coalesced UDP
delivery with zero allocations and deallocations while counting; the existing
storm also checks corruption, duplicates and zero worker queue drops.
Clippy, formatting and Rust 1.89 checks pass. Real old-kernel capability
fallback remains untested; descriptor fault tests cover its control flow.

## Bounded TAP read rings (audit B2)

Workers now submit nonblocking `READV` batches through one io_uring each.
Headers and frame buffers remain separate iovecs. The existing pool and
64-frame per-event ceiling remain unchanged; every accepted completion is
drained before forwarding or returning buffers. Unsupported rings or operations
fall back to ordinary reads. Ring mappings count toward startup's memory
reservation. [README](README.md#tap-read-batching) describes the failure policy.

The first fixed-batch trial improved MTU-1500 unlimited delivery to
4.95–5.21 / 5.97–6.18 Gbit/s, but its second reverse point declined 1.6%
from paced overload. At 2 Gbit/s, batches averaged only 3.9 / 4.1 valid frames;
daemon CPU rose to 357% / 366%, against about 326% / 334% with USO alone.
The old decline is recorded; it is not attributed solely to empty reads.

Occupancy-based sizing reduced light-load CPU. Its first formula also shrank
the next batch when the pool supplied fewer buffers than requested, even if
every read succeeded. A regression now verifies that pool pressure preserves
the larger hint. Only a genuinely short batch reduces it; a full batch grows
it, up to 64. The two six-point exploratory sweeps remain in the data file
as `tap_ring_fixed_trial` and `tap_ring_adaptive_trial`.

The first corrected build's SHA-256 is
`51fcbeec140ab3ff4cfef6e90ab655de7f4174328620d4bc9f7c050a7b95dd95`.
These 14 normal-build points retain the four-worker, 16-flow, 20-second,
single-socket profile. No own builds or other tests ran during measurements.
Independent sweeps still differ in scheduling and kernel flow placement.

| Frame bytes | Sweep | Requested/flow | Delivered Gbit/s | Missing % | Daemon CPU % |
| ---: | --- | --- | ---: | ---: | ---: |
| 1514 | MTU | 125M | 1.999 / 1.989 | 0.06 / 0.54 | 335.1 / 336.6 |
| 1514 | MTU | 250M | 3.802 / 3.959 | 4.93 / 1.03 | 358.2 / 360.1 |
| 1514 | MTU | 500M | 4.090 / 6.015 | 48.74 / 24.81 | 353.5 / 356.7 |
| 1514 | MTU | unlimited | 5.077 / 6.313 | 60.54 / 50.35 | 330.6 / 325.5 |
| 1514 | MTU | 500M | 3.972 / 5.954 | 50.19 / 25.42 | 355.0 / 365.0 |
| 1514 | MTU | unlimited | 5.037 / 6.230 | 60.12 / 51.09 | 331.3 / 327.7 |
| 1514 | MTU repeat | 500M | 4.107 / 6.089 | 48.51 / 23.88 | 358.2 / 368.3 |
| 1514 | MTU repeat | unlimited | 4.936 / 6.244 | 61.83 / 50.70 | 323.4 / 322.1 |
| 1514 | MTU repeat | 500M | 4.084 / 5.980 | 48.80 / 25.12 | 354.8 / 360.3 |
| 1514 | MTU repeat | unlimited | 4.977 / 6.306 | 62.81 / 50.08 | 329.4 / 325.7 |
| 64 | small | 10M | 0.134 / 0.127 | 16.01 / 20.39 | 354.0 / 350.3 |
| 64 | small | unlimited | 0.153 / 0.149 | 28.36 / 31.02 | 314.8 / 316.0 |
| 64 | small | 10M | 0.129 / 0.127 | 19.11 / 20.89 | 348.0 / 360.2 |
| 64 | small | unlimited | 0.149 / 0.149 | 28.15 / 32.36 | 313.5 / 313.1 |

The final bounds-hardened build, SHA-256
`6d038d3c4705b90c4643e7c5a3f6118c26702def35ef3be28f86ed341e445a80`,
retains the same batching policy. Its four additional points are recorded as
`tap_ring_checked`:

| Frame bytes | Requested/flow | Delivered Gbit/s | Missing % | Daemon CPU % |
| ---: | --- | ---: | ---: | ---: |
| 1514 | 500M | 4.268 / 6.304 | 46.53 / 21.19 | 357.3 / 367.3 |
| 1514 | unlimited | 5.277 / 6.480 | 57.94 / 47.83 | 324.6 / 336.6 |
| 64 | 10M | 0.140 / 0.132 | 12.35 / 17.19 | 359.7 / 355.0 |
| 64 | unlimited | 0.158 / 0.153 | 23.10 / 27.11 | 320.9 / 318.2 |

The final MTU unlimited point reports 10.54 / 7.80 million TAP TX drops,
135,599 / 113,894 socket-overflow SKBs, 40,569 / 53,669 inner UDP
receive-buffer errors and 7 / 0 expired frames. The final small-frame point
delivers 899,068 / 868,229 frames/s.

All five corrected MTU-1500 unlimited points and three small-frame unlimited
points exceed their preceding paced points in both directions. USO alone
delivered 4.38–4.56 / 5.87–5.90 Gbit/s and 729–737k / 714–734k frames/s.
The read-ring small-frame points reach 847–899k / 844–868k frames/s.

Under MTU-1500 unlimited load, each read syscall returns an average of
24.8–26.9 valid frames. Empty requests account for 1.2–2.5% on the left and
6.0–6.9% on the right. At 2 Gbit/s, the sizing rule allows roughly one empty
request per valid frame and averages 2.1 / 2.5 valid frames per syscall.
Those requests share an enter syscall; they are not separate kernel entries.

All 30 new normal-build points have zero admission-pressure rejections,
worker queue drops, TAP errors and outer IPv6 fragmentation. No read ring
fell back at startup or during the sweeps. In the last point of the first corrected MTU-1500 repeat, TAP TX drops were 11.61 / 9.66 million, socket overflow was
66,622 / 164,297 SKBs, inner UDP receive-buffer errors were 49,175 / 23,265,
and frame expiry was zero. These counters name different stages and units.

Small-frame receiver pressure is more visible: unlimited points report
443–466k / 694–796k inner UDP receive-buffer errors. The iperf3 receiver
processes still have two CPUs each and do not use UDP GRO. This warrants the
planned GRO-aware generator/receiver, but does not account for every missing
frame or establish a CPU ceiling.

### Profile after read batching

[The sampled profile](benches/worker-read-ring-profile.json) uses the same
one-in-256-turn method and clock calibration as B1. It is a separate diagnostic
build and is excluded from normal throughput comparisons.

| Stage | Paced 500M/flow CPU share, left/right % | Unlimited CPU share, left/right % |
| --- | ---: | ---: |
| TAP reads and frame dispatch | 25.4 / 32.2 | 28.0 / 31.2 |
| Receive processing, including interleaved TAP delivery | 29.8 / 19.2 | 28.6 / 23.6 |
| Final TAP delivery | 17.7 / 14.1 | 15.6 / 14.0 |
| Transmit flush | 14.3 / 19.1 | 16.1 / 18.4 |
| epoll wait | 2.1 / 3.6 | 1.9 / 2.3 |

Estimated unlimited CPU was 332% / 325%, against process measurements of
330% / 326%. No TAP read handler began without an available transmit buffer.
The read category still includes frame dispatch. Receive processing and TAP
delivery together remain about 44% / 38% of sampled CPU.

The results remain below the audit's projections and the 20 Gbit/s gate.
The current generator offers only about 13 Gbit/s each way at MTU 1500.
C2's concurrent UDP_SEGMENT/UDP_GRO tool is the next measurement prerequisite;
production super-frame TAP reads, the eight-worker profile and the full
acceptance gate remain unfinished.

### Read-ring correctness checks

Tests cover batches of 1, 2, 63 and 64 real TAP frames at 60, 1514 and
9014 bytes, byte order, metadata validation and empty reads. Every full batch
uses one enter syscall; no extra kernel I/O worker threads appeared.
Fault injection submits a real prefix before reporting EINTR or EPERM,
verifying exact completion accounting, withdrawal of unsubmitted work and
ordinary-read delivery of the remainder. Another test exercises operation
rejection. The pool-pressure regression checks the next batch and the RX reserve.

The full release suite, all 16 ignored kernel unit tests, Clippy and Rust
1.89 checks pass. The reconnect/runtime storm read about 843,000 frames per
endpoint through rings with no fallback, corruption, duplicates, allocations
or deallocations during its counted interval. The USO capture test now ignores
unrelated bridge multicast reports by source MAC; fixture frames and duplicate
checks remain byte-exact. A real thread-local seccomp filter denies ring setup;
the worker then reads through the ordinary path and counts the fallback.
Older kernels remain untested.

## Concurrent UDP generator and GRO receiver (C2/C3)

The [Rust load tool](benches/udp_load.md) runs concurrent connected sockets
on one thread per process. It supports fixed ports, per-flow integer pacing,
UDP segmentation and GRO, with byte validation and bounded duplicate tracking.
No new dependency is required. Its measured send/receive loop allocates and
deallocates nothing in the kernel regression.

These capacity probes connect two disposable namespaces directly by veth,
with MTU 1500, 16 flows each way and 1472-byte UDP payloads. Four distinct
physical cores on one socket host the two senders and two receivers.
Each sender runs for 10 seconds; receivers include a two-second drain window.
No daemon participates. These figures do not count toward the tunnel gate.

[Raw per-flow results](benches/udp-load.json) retain all six points.
CPU below is process CPU time divided by the corresponding sender duration;
receiver values include drain work. No own builds or other tests ran alongside
the measurements.

| Build | Requested Gbit/s each way | Actual offered Gbit/s | Delivered Gbit/s | Missing % | Sender CPU % | Receiver CPU % |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Scalar validation trial | 21 | 20.969 / 20.969 | 12.679 / 12.621 | 39.54 / 39.81 | 51.8 / 51.2 | 100.4 / 100.7 |
| Scalar validation trial | unlimited | 51.255 / 51.554 | 12.798 / 12.704 | 75.03 / 75.36 | 99.7 / 98.9 | 100.7 / 100.3 |
| Vectorizable validation | 21 | 20.972 / 20.967 | 20.972 / 20.962 | 0.0010 / 0.0240 | 65.9 / 64.7 | 71.4 / 70.0 |
| Vectorizable validation | unlimited | 35.698 / 36.256 | 35.691 / 35.822 | 0.0194 / 1.1976 | 99.7 / 99.8 | 100.6 / 99.8 |
| Final example target | 21 | 20.926 / 20.940 | 20.924 / 20.939 | 0.0092 / 0.0022 | 64.9 / 64.2 | 69.8 / 70.3 |
| Final example target | unlimited | 37.820 / 36.900 | 37.820 / 36.897 | 0.0004 / 0.0080 | 99.7 / 99.3 | 100.5 / 100.5 |

The first receiver's early-exit byte comparison limited delivery to about
13 Gbit/s. Replacing that expression with a vectorizable bitwise reduction,
while still checking every byte, removed this measured bottleneck.
The final executable's SHA-256 is
`da57a37ab95c5db52287d577e95c81439d9c16c7ebf89097b7e6a2035f53ec3d`.

Every probe reports zero corruption, duplicates and packets outside the
duplicate window. In the final paced point, receive-socket overflow was
37 / 9 SKBs; unlimited overflow was 3 / 57 SKBs. Each held 44 datagrams,
and those counts account exactly for the final points' missing datagrams.
Actual paced offers were slightly below the requested 21 Gbit/s; report the
measured rates, not the request.

The first segmented TAP smoke test failed with EIO before producing a
throughput result. A focused kernel test reproduces the failure with
TUNSETOFFLOAD=0, then verifies successful sends with TUN_F_CSUM and a single
USO super-frame when both USO flags are enabled. Linux 6.8's
[UDP send path](https://github.com/torvalds/linux/blob/v6.8/net/ipv4/udp.c#L854-L873)
requires partial-checksum support for segmented sends, and its
[TUN setup](https://github.com/torvalds/linux/blob/v6.8/drivers/net/tun.c#L2752-L2755)
requires both USO flags together.

Production TAP read offloads remain disabled. B3 must validate, checksum
and segment their input before this generator can exercise segmented
TAP-to-TAP traffic. Plain-UDP smoke tests pass with both four and eight
workers, and the existing iperf3 mode remains available. Neither those
two-second checks nor the direct veth probes establish tunnel performance.
The full four-/eight-worker sweeps and five-run acceptance gate remain open.

Four ordinary unit tests and two ignored container tests cover pacing,
sequence windows, argument bounds, ancillary validation, IPv4/IPv6,
four concurrent sockets, 18/1472/8972-byte payloads and batches up to 64
datagrams. Corrupting every byte of the checked payload body is detected. The send/receive
test counts zero allocations and deallocations. Clippy and Rust 1.89 checks pass.

## Partial TAP checksum completion: segmented sender sweeps

The runtime now enables TUN_F_CSUM and completes bounded partial checksums
in place. GSO read offloads stay disabled; the kernel still segments before
TAP reads. This removes the generator's EIO prerequisite without changing
the wire format, frame quotas, receive deadlines or per-turn budgets.
`tap_checksums` counts completions before transmit admission.

[These 12 normal-build points](benches/tap-udp-load.json) use the Rust generator
with UDP_SEGMENT and a GRO-aware receiver. Each run has 16 flows per direction,
20 seconds of sending and a two-second receiver tail. Four-worker daemons use
eight cores plus four load-process cores; eight-worker daemons use sixteen plus
four. The CPU sets are disjoint physical cores on the same socket.
The daemon hash is
`4446515549d0217d6e95b48a6abf88830580fb8510cc4399d91504af3eabff47`.

These rows differ from iperf3 in burst shape, destination ports, receiver
offloads and load-process placement. Compare paced and unlimited points
within each sweep; they are not an isolated checksum-speed comparison.

| Workers each | Requested Gbit/s each way | Actual offered Gbit/s | Delivered Gbit/s | Missing % | Daemon CPU % |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 4 | 2 | 2.000 / 1.999 | 1.999 / 1.999 | 0.04 / 0.01 | 93.9 / 100.4 |
| 4 | 8 | 7.966 / 7.953 | 6.266 / 5.271 | 21.34 / 33.73 | 316.7 / 325.0 |
| 4 | 21 | 10.699 / 12.101 | 5.311 / 6.253 | 50.36 / 48.33 | 302.9 / 347.3 |
| 4 | unlimited | 10.421 / 12.500 | 6.816 / 5.706 | 34.60 / 54.35 | 336.7 / 355.4 |
| 4 | 21 | 10.589 / 12.648 | 6.698 / 5.554 | 36.75 / 56.09 | 336.4 / 357.2 |
| 4 | unlimited | 10.904 / 12.672 | 6.528 / 5.765 | 40.13 / 54.50 | 332.2 / 359.1 |
| 8 | 2 | 2.000 / 1.999 | 1.999 / 1.999 | 0.03 / 0.01 | 120.5 / 116.7 |
| 8 | 8 | 7.974 / 7.996 | 7.921 / 7.926 | 0.67 / 0.88 | 406.9 / 395.0 |
| 8 | 21 | 10.889 / 11.065 | 9.949 / 9.026 | 8.63 / 18.43 | 518.8 / 523.8 |
| 8 | unlimited | 10.575 / 10.690 | 9.421 / 8.421 | 10.91 / 21.23 | 504.5 / 513.4 |
| 8 | 21 | 11.156 / 11.012 | 9.589 / 8.497 | 14.05 / 22.84 | 522.5 / 527.1 |
| 8 | unlimited | 10.666 / 11.957 | 9.789 / 8.921 | 8.21 / 25.39 | 510.7 / 530.0 |

All points have zero corruption, duplicates, packets outside the duplicate
window, admission-pressure rejections, worker queue drops, TAP errors and
outer IPv6 fragmentation. Checksum completion counts track the accepted load frames; unrelated
control traffic can add ordinary reads. Socket overflow and TAP ingress drops remain.

The four-worker 8-to-21-requested transition is a verified decline:
L→R delivery falls 15.2% while actual offered load rises from 7.97 to
10.70 Gbit/s. The reverse direction gains throughput in the same transition.
Later paced/unlimited comparisons also vary. The eight-worker first
unlimited point is lower in both delivery and actual offered load than its
paced predecessor, so that comparison alone does not prove collapse from
greater input.

Per-thread measurements expose substantial imbalance. In the four-worker
declining point, left worker CPU is 96.8%, 94.2%, 44.8% and 67.2%.
The fixed 16-flow set maps to transmit-lane counts 4/2/7/3 on the left and
4/6/5/1 on the right. Receive ownership and learned TAP queue placement
create different work distributions. These observations establish unused
aggregate CPU alongside saturated workers; they do not yet explain every
change between points or justify changing flow pinning.

The unit tests cover TCP/UDP checksum offsets, IPv4/IPv6, VLANs, odd payload
lengths, zero-checksum encoding and invalid metadata without modifying rejected
frames. Ordinary and io_uring reads share the checked completion path.
All 16 kernel tests pass. The reconnect storm forwards about 1.07 million
frames per endpoint without corruption, duplicates or counted allocations.
Full super-frame input and a stable plateau remain unfinished.

### Profile and checksum arithmetic follow-up

[The five-point diagnostic repeat](benches/worker-checksum-profile.json)
uses the previous one-in-256-turn CPU-clock method. It does not reproduce
the full initial 15.2% decline: its requested-8-Gbit point delivers
6.43 / 5.76 Gbit/s, followed by 7.06 / 5.95 Gbit/s at requested 21 Gbit/s.
Smaller later declines remain. Independent sweeps differ in scheduling and
TAP queue placement; the initial failed point remains in the normal data.

The profile puts TAP reads, checksum completion and dispatch at 36–40%
of daemon CPU. Receive processing plus TAP delivery takes about 29–32%,
transmit flush 17–19%, and epoll 2–4%. No read handler starts without a
transmit buffer. The samples identify where CPU goes, but do not establish
a complete cause for the changing per-direction goodput.

A separate checksum probe found an avoidable byte swap per 16-bit input
word. Summing native-endian words, folding carries and then swapping the
result preserves the Internet checksum. At 1472 bytes, the hot calculation
falls from 116–120 ns to 68–72 ns; at 9022 bytes, from 711–732 ns to
359–380 ns. These are isolated calculation costs, not forwarding latency.
The probe checks both formulations across lengths through 65,534 bytes;
the production regression checks every aligned/unaligned length through
MAX_FRAME_LEN against an independent network-order reference.

The unchanged iperf3 workload retains its four-worker, 20-core profile.
Both before and after the arithmetic change, each unlimited point exceeds
its preceding paced point:

| Checksum arithmetic | Requested/flow | Delivered Gbit/s | Missing % | Daemon CPU % |
| --- | --- | ---: | ---: | ---: |
| Per-word swap | 500M | 3.920 / 5.636 | 50.86 / 29.40 | 350.1 / 359.5 |
| Per-word swap | unlimited | 5.049 / 6.013 | 58.50 / 50.89 | 327.9 / 327.1 |
| Per-word swap | 500M | 3.884 / 5.637 | 51.45 / 29.40 | 349.1 / 358.6 |
| Per-word swap | unlimited | 5.223 / 6.024 | 56.73 / 51.44 | 337.7 / 331.4 |
| Native-word sum | 500M | 4.171 / 6.177 | 47.86 / 22.79 | 364.0 / 366.7 |
| Native-word sum | unlimited | 5.264 / 6.407 | 57.44 / 48.02 | 337.2 / 329.9 |
| Native-word sum | 500M | 4.155 / 6.224 | 47.89 / 22.03 | 367.3 / 369.6 |
| Native-word sum | unlimited | 5.306 / 6.407 | 57.98 / 47.87 | 340.5 / 335.3 |

The native-word build restores the reverse direction to about 6.41 Gbit/s
in these two unlimited points, compared with about 6.02 Gbit/s before the
arithmetic change. The final hash is
`9645ae5a83b387ecc9437c2fce93c3c9f602a83b7871cb8997c92caf167bcb0d`.
A separate three-second bidirectional TCP smoke test also passes at about
1.98 / 1.95 Gbit/s. These checks do not resolve the segmented workload's
remaining overload variation.

### Native-checksum segmented-send repeat

The same 20-second, 16-flow profiles were repeated with the final native-word
checksum build above. Rates and loss use unique receiver sequences divided
by the sender's measured duration; daemon CPU uses the before/sent snapshots.
The JSON retains generator/receiver CPU, per-worker and per-core counters.
These runs and the diagnostic profile ran separately from builds and tests.

| Workers | Frame bytes | Requested Gbit/s each way | Actual offered Gbit/s | Delivered Gbit/s | Missing % | Daemon CPU % |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 4 | 1514 | 8 | 7.964 / 7.993 | 6.490 / 5.931 | 18.51 / 25.80 | 310.2 / 312.5 |
| 4 | 1514 | 21 | 10.574 / 12.394 | 6.860 / 5.645 | 35.12 / 54.45 | 334.2 / 353.1 |
| 4 | 1514 | unlimited | 10.552 / 12.455 | 6.914 / 5.658 | 34.48 / 54.57 | 333.7 / 352.9 |
| 4 | 1514 | 21 | 10.537 / 12.520 | 6.992 / 5.729 | 33.64 / 54.24 | 336.1 / 354.6 |
| 4 | 1514 | unlimited | 11.247 / 12.155 | 6.974 / 5.722 | 37.99 / 52.93 | 336.7 / 354.2 |
| 4 | 64 | 0.16 | 0.160 / 0.160 | 0.160 / 0.160 | 0.00 / 0.01 | 241.8 / 252.0 |
| 4 | 64 | unlimited | 0.261 / 0.266 | 0.215 / 0.186 | 17.43 / 29.98 | 318.0 / 321.0 |
| 8 | 1514 | 8 | 7.994 / 7.905 | 7.963 / 7.868 | 0.40 / 0.47 | 394.8 / 384.6 |
| 8 | 1514 | 21 | 10.711 / 11.682 | 9.908 / 9.194 | 7.50 / 21.30 | 503.6 / 519.6 |
| 8 | 1514 | unlimited | 10.515 / 11.371 | 9.752 / 9.454 | 7.26 / 16.86 | 500.0 / 520.8 |
| 8 | 1514 | 21 | 10.458 / 11.279 | 9.570 / 9.203 | 8.49 / 18.41 | 492.1 / 514.6 |
| 8 | 1514 | unlimited | 10.522 / 11.037 | 9.738 / 9.291 | 7.45 / 15.82 | 494.1 / 518.3 |
| 8 | 64 | 0.16 | 0.160 / 0.160 | 0.160 / 0.160 | 0.00 / 0.00 | 272.0 / 275.3 |
| 8 | 64 | unlimited | 0.273 / 0.280 | 0.273 / 0.277 | 0.13 / 0.87 | 454.5 / 472.4 |

The four-worker reverse direction falls from 5.931 to 5.645 Gbit/s (4.8%)
when actual offered load rises from 7.99 to 12.39 Gbit/s. Later points
remain below that first paced result. The decline is unresolved. Eight
workers reach about 9.74 / 9.29–9.45 Gbit/s unlimited, but actual offered
load varies and remains below the gate's 21 Gbit/s request.

For 64-byte frames, four workers deliver 907,752 / 908,202 frames/s paced
and 1,224,374 / 1,059,330 unlimited. Eight deliver 907,278 / 906,953 paced
and 1,549,763 / 1,576,089 unlimited. Both short sweeps improve under overload.
These packets carry only 22 payload bytes; frame rate is more useful than
payload goodput for this case.

Drop attribution below uses endpoint L / R order, including the receiver
drain. Socket overflow counts SKBs, which can contain multiple datagrams;
TAP TX drops count inner frames. Inner UDP errors count receive-buffer failures.
They cannot be added as if they had the same unit.

| Native run | TAP TX drops | Transport socket overflow | Inner UDP errors | Expired frames |
| --- | ---: | ---: | ---: | ---: |
| rx-csum-native-4-1452-1514-16/4 | 4897364 / 8138761 | 189518 / 141985 | 4052 / 15 | 0 / 0 |
| rx-csum-native-8-1452-1514-16/2 | 26576 / 2362663 | 63668 / 92435 | 3901 / 626 | 0 / 0 |
| rx-csum-native-8-1452-1514-16/4 | 0 / 2400244 | 41785 / 98299 | 1936 / 462 | 0 / 0 |
| rx-csum-native-4-1232-64-16/1 | 4315315 / 8499000 | 9687 / 20603 | 0 / 0 | 0 / 0 |
| rx-csum-native-8-1232-64-16/1 | 17281 / 238390 | 1013 / 519 | 0 / 0 | 0 / 0 |

All fourteen points have zero admission pressure, worker queue drops,
corruption, duplicate sequences, out-of-window sequences, TAP errors and
outer IPv6 fragmentation. The final reconnect test delivered about 1.10
million frames per endpoint without corruption, duplicates, hot allocations
or deallocations. All 16 kernel tests, the full release suite, Clippy and
Rust 1.89 checks pass. These checks establish the checksum prerequisite;
full TAP super-frame input, overload stability and both performance gates
remain unfinished.

## Bounded UDP super-frame input (audit B3, UDP portion)

The runtime now enables UDP L4 TAP input offloads. It retains a super-frame
in the existing pool and expands ordinary Ethernet frames before packing.
Expansion stops after 64 frames or 64 KiB in a worker turn, with one final
frame allowed to cross the byte target. Sessions rotate; input order within
each session stays intact. One spare output buffer and byte credit prevent
a queue of inputs from exhausting the resources needed for expansion.
Raw input and produced frames both count against transmit limits. Expiry
counts remaining logical frames and preserves the original 100 ms deadline.
TCP input remains segmented by the kernel.

The kernel comparison reproduces every output byte for IPv4/IPv6 USO inputs,
including short tails and IPv4 ID wrap. Additional tests cover VLANs, options,
IPv6 extensions, length/metadata rejection, flow pinning, maximum kernel UDP
sends, pool pressure, turn budgets, expiry and quiesce. The release runtime
test exercises actual super-frame reads and verifies zero steady-state
allocations/deallocations, corruption or duplicate delivery.

### Measurement correction: queued traffic and drain delivery

[All experiments are retained](benches/tap-uso-input.json), including the
initial shared-tunnel sweep. That sweep reached 36–42 Gbit/s actual offered
load but later points contained invalid records. A diagnostic repeat
classified all 75,550 invalid records as well-formed payloads carrying the
previous run's ID. There were no other invalid payloads in that repeat.
The original records lack this classification, so their exact causes cannot
be recovered retrospectively.

USO changes the unit held by the kernel TAP queue: one queued SKB can now
carry dozens of frames. The earlier two-second drain did not empty every
queue before the next run. Each rate point now starts fresh daemons and
private namespaces. The initial failed points remain excluded from acceptance.

Receiver SIGUSR1 snapshots at the before/sent boundaries now measure
unique delivery within a defined interval. Final counts still include the
drain and determine observed missing packets for that receiver lifetime.
Several overloaded four-worker points deliver another 1.1–1.3 million
frames during the drain. Dividing final delivery by sender duration, as
the historical tables do, overstates the forwarding rate in these cases.
New tables use the receiver sample interval and report drain-inclusive
delivery separately in JSON.

TAP TX drop counters now count super-frame SKBs where applicable; transport
socket overflow also counts SKBs. Neither can be added directly to logical
inner-frame loss. The 100 ms userspace deadline starts when the worker reads
a TAP input, and does not measure time already spent in the kernel TAP queue.

### Isolated, sampled release sweeps

The daemon hash is
`e228a51486c4eab44f3a5936dca84cd68721ebd996e2c2ccf5bcf8e80e7427fa`.
Four-worker runs use the same 12 physical cores as the preceding segmented
sweep; eight-worker runs use 20. Each point sends for 20 seconds, with
16 flows each way and 44 UDP segments per send. The receiver interval is
about 20.12 seconds. All points below have zero invalid records, duplicate
sequences, admission pressure, worker queue drops and outer IPv6 fragmentation.

| Workers | Requested Gbit/s each way | Actual offered Gbit/s | In-window delivered Gbit/s | Missing after drain % | Daemon CPU % |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 4 | 8 | 7.979 / 7.982 | 6.872 / 7.573 | 13.34 / 2.32 | 287.7 / 276.6 |
| 4 | 21 | 20.807 / 20.971 | 7.734 / 7.059 | 59.51 / 62.87 | 348.9 / 349.2 |
| 4 | unlimited | 36.092 / 43.012 | 7.824 / 7.377 | 76.39 / 81.22 | 359.8 / 347.0 |
| 4 | 21 | 20.696 / 20.893 | 7.735 / 7.212 | 59.25 / 61.64 | 344.0 / 344.0 |
| 4 | unlimited | 36.026 / 42.392 | 7.813 / 7.273 | 76.39 / 81.16 | 357.5 / 343.4 |
| 8 | 8 | 7.971 / 7.963 | 7.907 / 7.875 | 0.21 / 0.51 | 301.0 / 294.1 |
| 8 | 21 | 20.531 / 20.640 | 14.708 / 11.297 | 25.52 / 40.06 | 576.2 / 569.6 |
| 8 | unlimited | 24.293 / 30.118 | 14.443 / 11.002 | 37.27 / 59.92 | 592.8 / 598.8 |
| 8 | 21 | 20.647 / 20.702 | 14.762 / 11.264 | 25.62 / 40.54 | 579.6 / 571.6 |
| 8 | unlimited | 25.248 / 30.428 | 14.773 / 11.048 | 37.92 / 60.41 | 604.4 / 601.4 |

The last four-worker unlimited point has 34 packets outside the duplicate
window, so its missing percentage is not an exact loss claim. All other
isolated points above have zero out-of-window packets. Four-worker expiry
counts are 0 / 1 at the first unlimited point and 2 / 0 at the second;
the eight-worker points have no expiry.

The eight-worker first paced/unlimited pair falls from 14.708 / 11.297
to 14.443 / 11.002 Gbit/s as actual input increases. Its second pair holds
L→R delivery but R→L falls from 11.264 to 11.048 Gbit/s. Four workers also
lose reverse delivery between requested 8 and 21 Gbit/s. These are measured
declines; the 20 Gbit/s gate and a stable overload plateau remain unmet.

### Matched controls, small frames and CPU attribution

Checksum-only controls use the same fresh-namespace harness, receiver samples,
16 flows and 20-second intervals. Their actual offered rate remains lower
because the kernel segments each send before the TAP read. These are matched
requests and placement, not equal actual offered traffic.

| Workers | Requested Gbit/s each way | Actual offered Gbit/s | In-window delivered Gbit/s | Missing after drain % | Daemon CPU % |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 8 | 8 | 7.862 / 7.925 | 7.721 / 7.808 | 1.21 / 0.89 | 386.5 / 385.8 |
| 8 | 21 | 10.326 / 11.488 | 9.448 / 8.881 | 7.96 / 22.04 | 491.5 / 509.0 |
| 8 | unlimited | 10.339 / 11.244 | 9.450 / 8.810 | 8.06 / 21.03 | 496.5 / 514.6 |
| 4 | 21 | 10.456 / 11.928 | 5.319 / 6.098 | 48.67 / 48.49 | 292.7 / 329.6 |
| 4 | unlimited | 9.532 / 11.814 | 6.574 / 5.438 | 30.46 / 53.62 | 329.6 / 352.3 |

At the eight-worker 8 Gbit/s request, UDP input uses about 22–24% less daemon
CPU. At unlimited load it delivers about 53–56% more L→R and 25% more R→L
than the sampled checksum-only control, with about 20% more daemon CPU and
higher offered traffic. Four-worker control variability prevents an equally
clean speedup attribution.

The 64-byte runs use 22-byte UDP payloads and 64 segments per send. Goodput
below counts frames only within the receiver sample interval; missing rates
use final delivery after drain.

| Workers | Requested per-flow Mbit/s | Offered Mframes/s | Delivered Mframes/s | Missing after drain % | Daemon CPU % |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 4 | 10 | 0.909 / 0.909 | 0.903 / 0.903 | 0.00 / 0.02 | 148.9 / 146.1 |
| 4 | unlimited | 19.004 / 18.735 | 1.749 / 2.126 | 90.23 / 87.91 | 364.4 / 351.3 |
| 8 | 10 | 0.906 / 0.906 | 0.901 / 0.901 | 0.00 / 0.00 | 170.2 / 166.8 |
| 8 | unlimited | 16.877 / 17.999 | 3.621 / 3.083 | 77.46 / 81.76 | 686.9 / 681.6 |
| 8 | 40 | 3.601 / 3.609 | 3.228 / 2.567 | 8.06 / 24.92 | 556.8 / 544.1 |
| 8 | unlimited | 16.712 / 17.992 | 3.602 / 3.058 | 77.30 / 81.87 | 693.2 / 679.8 |

These small-frame points have no invalid, duplicate or out-of-window records,
worker queue drops, admission pressure, expiry or outer fragmentation.
Unlimited delivery exceeds the preceding paced point in both directions,
including the 40 Mbit/s-per-flow saturation comparison.

The unchanged iperf3 workload was also tested with fresh namespaces. With
the original wider generator placement, four workers deliver 3.958 / 5.527
Gbit/s at the 8 Gbit/s request and 4.778 / 5.914 Gbit/s unlimited, missing
50.37% / 30.79% and 61.11% / 53.35%, respectively. Daemon CPU is
338.5% / 346.8% and 320.4% / 319.6%. A separate single-core-per-generator
smoke test delivers 3.289 / 3.303 and 3.430 / 3.500 Gbit/s; its lower offered
rate cannot test the transport ceiling. Both placements have zero pressure
and worker queue drops. The wider unlimited point expires three left-side
frames. These ordinary-send runs do not establish an improvement over the
earlier checksum-only iperf3 results.

[The eight-worker CPU profile](benches/worker-uso-input-profile.json) samples
one turn in 256 with thread CPU time and subtracts measured clock overhead.
Complete one-second bins attribute roughly 24–29% of CPU to transmit flush,
22–26% to receive processing including interleaved TAP delivery, 10–12% to
final TAP delivery, 8–12% to TAP reads, and 8–11% to super-frame expansion.
UDP receive calls account for another 7–11%. No TAP handler ran without
available transmit capacity. Estimated total CPU is within about 3% of
the process snapshots; small negative dispatch residuals show sampling noise.

Worker load remains uneven: in the last uninstrumented eight-worker unlimited
point, some worker CPUs spend about half the interval idle while others
are almost fully busy. Both receiver process cores are busy for roughly
94–97% of the sampled interval. This supports further placement and receiver
profiling; it does not prove either is the remaining ceiling. The instrumented
pair itself improves under unlimited load, so it does not explain the
uninstrumented 1.8–2.6% declines.

| Sampled run | TAP TX drops (SKBs) | Transport socket overflow (SKBs) | Inner UDP errors | Expired logical frames |
| --- | ---: | ---: | ---: | ---: |
| rx-uso-isolated-4-1452-1514-16/4 | 908138 / 1260708 | 184517 / 383997 | 3159 / 96 | 2 / 0 |
| rx-uso-isolated-8-1452-1514-16/2 | 201073 / 565211 | 351179 / 406451 | 21142 / 11428 | 0 / 0 |
| rx-uso-isolated-8-1452-1514-16/4 | 232701 / 576463 | 358531 / 361391 | 17666 / 11633 | 0 / 0 |
| rx-uso-small-saturation-8-1452-64-16/1 | 3585681 / 4309183 | 589451 / 925899 | 13029 / 2002 | 0 / 0 |

The final release suite, all 18 kernel library tests, both load-tool kernel
tests, foundation and reconnect tests, Clippy and Rust 1.89 checks pass.
The reconnect test includes byte-checked UDP super-frame delivery and checks
for steady-state allocations and deallocations. TCP input, the overload
plateau and both throughput gates remain unfinished.

## Conditional UDP lane locality (audit A1 follow-up)

The user permitted relaxing pinning where measurements show a benefit, while
retaining it otherwise, and excluded eBPF. The runtime now hashes parseable,
unfragmented UDP among the TAP reader's owned lanes when every worker has a
queue for that session. A reader with no owned lane uses the original hash
and handoff. Sessions with fewer TAP queues than global workers also retain
strict hashing so their other workers can transmit. TCP, IP fragments and
other traffic keep strict flow pinning.

A UDP flow can move to a new lane when Linux changes its TAP queue. A frame's
pieces and future repairs keep its original lane. The change adds no flow
table, eviction policy, allocation, lock or wire-format field. Tests cover
all worker/lane counts through 64, fallback configurations, protocol gating,
VLANs, truncated headers and both IP fragment formats.

### Reordering and measurement validity

[All lane-selection experiments](benches/tap-local-lanes.json) are retained.
The first all-protocol trial exceeded the load tool's 8,192-sequence history:
millions of late records could not be classified as new or duplicate, so
those points are invalid. They are not evidence of data corruption or exact
loss. The tool now reserves a 16,777,216-position tagged bitmap per flow at
startup, with 4 MiB of storage per flow and no traffic-time allocations.
It still reports and rejects out-of-window observations.

Corrected runs have zero invalid, duplicate and out-of-window records.
Reordering is real: one 10,000-entry-queue local-lane unlimited point reordered
15.55M / 14.78M of 27.56M / 26.95M delivered frames; its strict-pinning control
reordered 2.09M / 1.75M of 26.85M / 20.21M. Extending the tracker measures this
traffic; it does not fix reordering or increase the transport's buffers.

The first TCP comparison also argues against applying locality universally.
With eight workers, 16 streams each way, 20 seconds, 500-entry TAP queues and
64-KiB writes, [strict pinning delivers](benches/tap-local-tcp.json)
5.565 / 5.845 Gbit/s at 646.9% / 660.6% daemon CPU, with
49,976 / 39,391 sender retransmissions. The all-protocol local-lane trial
delivers 7.178 / 5.691 Gbit/s at 664.8% / 682.8% CPU, with
89,689 / 97,356 retransmissions. Worker pressure, queue drops and expiry
are zero in both. This mixed result is why TCP keeps strict pinning.

### UDP controls and remaining declines

The following are simultaneous 16-flow, 20-second release runs with the wider
sequence tracker. Both daemons use eight workers. Delivery uses receiver
before/sent samples; missing rates include the final drain. The local-lane
prototype uses the same UDP selection as the final build, but also changed
TCP before the protocol guard was added.

| Selection / queue length | Requested Gbit/s each way | Actual offered Gbit/s | Delivered Gbit/s | Missing after drain % | Daemon CPU % |
| --- | ---: | ---: | ---: | ---: | ---: |
| local trial / 10000 | 21 | 20.885 / 20.854 | 17.564 / 15.657 | 14.82 / 24.28 | 630.1 / 621.1 |
| local trial / 10000 | unlimited | 20.708 / 21.863 | 16.099 / 15.578 | 20.41 / 27.28 | 595.5 / 599.4 |
| strict / 10000 | 21 | 20.664 / 20.832 | 14.790 / 11.054 | 24.98 / 41.72 | 580.8 / 564.9 |
| strict / 10000 | unlimited | 23.158 / 27.398 | 12.764 / 10.687 | 41.33 / 57.04 | 558.1 / 553.6 |
| strict / 500 | 21 | 20.838 / 20.817 | 15.254 / 11.539 | 26.11 / 43.93 | 571.3 / 564.5 |
| strict / 500 | unlimited | 31.902 / 37.569 | 15.164 / 11.738 | 52.00 / 68.39 | 628.2 / 636.8 |
| local trial / 500 | 21 | 20.906 / 20.927 | 17.544 / 15.953 | 15.40 / 23.18 | 588.0 / 584.2 |
| local trial / 500 | unlimited | 28.818 / 28.480 | 17.012 / 16.873 | 40.35 / 40.22 | 728.2 / 738.8 |
| local trial / 500 | 21 | 20.917 / 20.942 | 17.079 / 16.021 | 17.68 / 22.88 | 581.7 / 572.5 |
| local trial / 500 | unlimited | 26.937 / 28.275 | 16.753 / 16.907 | 37.15 / 39.61 | 719.5 / 710.1 |
| UDP-only local / 500 | 21 | 20.891 / 20.913 | 17.692 / 16.247 | 14.62 / 21.68 | 581.6 / 582.8 |
| UDP-only local / 500 | unlimited | 32.047 / 31.689 | 17.945 / 17.073 | 43.44 / 45.62 | 687.8 / 688.4 |

The initial 500-entry local-lane pair improved under overload, but two
subsequent pairs lost 3.0% and 1.9% L→R. The final UDP-only pair improves;
one pair cannot establish a plateau. Short queues reduce retained tails from
hundreds of thousands or millions of frames to tens of thousands in most
points. They also change offered load and loss. This is a useful kernel-queue
diagnostic, not a replacement for the transport investigation. The daemon
does not set administrative queue length; the harness exposes
`TAP_TXQUEUELEN` for reproducible comparisons.

Four-worker local-lane UDP trials deliver 7.859 / 7.722 Gbit/s at an 8 Gbit/s
request, 9.779 / 8.874 at 21 Gbit/s, and 9.843 / 8.803 unlimited.
Daemon CPU is 232.8% / 230.1%, 365.3% / 396.3%, and 367.7% / 396.1%.
The unlimited reverse direction still declines slightly. Ordinary iperf3
UDP reaches 5.173 / 4.975 paced and 6.063 / 6.152 unlimited with four workers,
versus 3.958 / 5.527 and 4.778 / 5.914 in the strict-pinning comparison.
The paced reverse regression remains visible.

For 64-byte frames, the eight-worker local-lane trial delivers 3.488 / 3.352
million frames/s at a 40 Mbit/s-per-flow request and 5.866 / 5.743 million
unlimited. Missing rates rise from 2.24% / 5.97% to 62.77% / 65.71%;
daemon CPU rises from 425.5% / 418.9% to 767.9% / 770.5%.
The final UDP-only build's unlimited check delivers
6.054 / 6.082 million frames/s,
missing 62.20 / 61.85% at
770.0 / 783.3% daemon CPU.

| Final UDP-only run | TAP TX drops (SKBs) | Socket overflow (SKBs) | Inner UDP errors | Expired frames |
| --- | ---: | ---: | ---: | ---: |
| rx-udp-local-final-8-1452-1514-16/0 | 45658 / 24920 | 420396 / 207342 | 3230 / 5380 | 0 / 0 |
| rx-udp-local-final-8-1452-1514-16/1 | 388601 / 345077 | 635117 / 463544 | 25435 / 27980 | 4 / 4 |
| rx-udp-local-final-8-1452-64-16 | 3108819 / 3023234 | 397226 / 289134 | 918 / 4688 | 0 / 0 |

All valid UDP points have zero worker admission pressure, queue drops and
outer fragmentation. SKB drop counts cannot be summed with logical frame
loss. The release suite, protocol-gating tests, all 18 kernel library tests,
load-tool kernel tests, reconnect/allocation test, Clippy and Rust 1.89
checks pass. The final UDP-only binary SHA-256 is
`83f262aa576b3f7ddf8ec5a324a9a490e4b58f8e238a7e84691ad27cbe932885`.
The local 20 Gbit/s gate, stable overload plateau and real-path gate remain open.

### Profile after UDP lane locality

A follow-up profile of `640adda` uses eight workers per daemon, 16 UDP flows
each way, 1514-byte frames, a 1452-byte outer payload ceiling and 500-entry
test TAP queues. These are two 20-second release runs with temporary timing
instrumentation, not acceptance runs. The
[stage measurements and counters](benches/worker-udp-local-profile.json)
retain the complete observations.

| Requested Gbit/s each way | Actual offered Gbit/s | Delivered Gbit/s | Missing after drain % | Daemon CPU % |
| --- | ---: | ---: | ---: | ---: |
| 21 | 20.915 / 20.588 | 17.771 / 16.262 | 14.37 / 20.41 | 582.4 / 589.3 |
| unlimited | 34.137 / 33.579 | 17.856 / 16.959 | 47.18 / 49.02 | 645.0 / 651.1 |

Timing samples one turn in 256 with thread CPU time, subtracts a calibrated
clock-read baseline and includes only complete one-second bins: 19 seconds
per point. The estimated total CPU is within 2.3% of the process snapshots.
Small negative setup/dispatch residuals are retained; those stages are below
the calibration resolution.

Across the four daemon observations, receive processing including interleaved
TAP delivery takes 29.6–30.5% of sampled CPU, transmit flushing 23.5–25.3%,
final TAP delivery 12.7–13.7%, TAP reads and frame dispatch 11.6–13.1%,
UDP receive calls 10.5–10.8%, and super-frame expansion 8.1–8.3%.
No TAP-read handler ran without an available transmit buffer. The earlier
readiness busy loop has not returned in these observations.

| Request | TAP TX drops (SKBs) | Transport socket overflow (SKBs) | Inner UDP errors | Expired frames |
| --- | ---: | ---: | ---: | ---: |
| 21 Gbit/s | 34790 / 6652 | 428237 / 224821 | 5384 / 12221 | 2 / 0 |
| unlimited | 494771 / 454061 | 559087 / 409723 | 7189 / 3715 | 0 / 4 |

Worker admission pressure and queue drops remain zero. Receivers report no
invalid, duplicate, out-of-window or wrong-run records; outer fragmentation
counters remain zero. These counters separate where drops occur, but cannot
convert dropped GRO/GSO SKBs into exact logical-frame losses.

This instrumented pair improves under excess load. It neither reproduces nor
explains the earlier small declines, and remains below the 20 Gbit/s gate.
Receive/TAP delivery and transmit flushing warrant finer profiling; their
combined stage times do not identify an individual function or establish a
CPU ceiling. No speculative optimization or timing instrumentation is retained
in production.

### Sixty-second production follow-up

The tested production binary, without profiling instrumentation, ran one
60-second paced/unlimited pair with the same eight-worker placement,
16 flows each way and 500-entry test TAP queues.
[Full counters and receiver samples](benches/tap-local-lanes-60s.json)
retain both points. These frames are untagged; VLAN performance is still unmeasured.

| Requested Gbit/s each way | Actual offered Gbit/s | Delivered Gbit/s | Missing after drain % | Daemon CPU % |
| --- | ---: | ---: | ---: | ---: | ---: |
| 21 | 20.858 / 20.760 | 16.573 / 15.597 | 20.34 / 24.66 | 553.4 / 551.9 |
| unlimited | 28.388 / 27.707 | 16.497 / 16.453 | 41.71 / 40.45 | 722.5 / 715.2 |

L→R declines 0.45% while R→L improves 5.48%. This single pair does not
establish whether the small decline is repeatable or measurement variation.
It also does not establish the required plateau or throughput gate.

| Request | TAP TX drops (SKBs) | Transport socket overflow (SKBs) | Inner UDP errors | Expired frames |
| --- | ---: | ---: | ---: | ---: |
| 21 Gbit/s | 234894 / 173263 | 1135346 / 726146 | 35946 / 20926 | 65 / 2 |
| unlimited | 717894 / 600380 | 1998071 / 1751411 | 175616 / 451379 | 11 / 36 |

Admission pressure and worker queue drops remain zero. Invalid, duplicate,
out-of-window and wrong-run receiver records and outer fragmentation counters
are zero. Higher CPU and socket/TAP drops accompany overload; the counters
do not prove which cost causes the remaining decline.
