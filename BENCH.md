# Release transport overload investigation

The original throughput collapse is reproduced and its receive-admission
failure is fixed. A later TAP readiness busy loop is now fixed too. The latest
20-second MTU-1500 and 64-byte repeats deliver at least as much under unlimited
load as at preceding paced saturation points. Broader plateau validation
remains open. Neither the local **20 Gbit/s in each direction** gate nor the
real **5 Gbit/s under loss** gate has passed.

This report separates that remaining decline from the earlier near-total
collapse. Recovery, congestion control and PMTU discovery remain unfinished;
their absence does not explain away a scheduling or admission failure.

## Current performance gaps

Status after the TAP readiness fix. The latest MTU-1500 and 64-byte results
use the 20-CPU single-socket profile and capped socket-buffer fallback below.
L→R / R→L pairs describe simultaneous traffic, not separate one-way peaks.

| Requirement | Current evidence | Remaining gap |
| --- | --- | --- |
| At least 20 Gbit/s each way at inner MTU 1500 | Repeated 16-flow unlimited points deliver 3.67–3.77 / 4.98–5.07 Gbit/s of inner UDP payload | Roughly 5.3× / 3.9× more delivered goodput is needed even relative to the better unlimited point. The gate is not close to passing. |
| Goodput must plateau under excess traffic | The latest MTU-1500 and 64-byte repeats have no paced-to-unlimited decline | The verified readiness busy loop is fixed. Longer runs, more frame mixes and broader flow/session coverage remain unverified. Removing the earlier admission cascade does not satisfy the plateau requirement. |
| Small-frame overload must remain efficient | With 64-byte frames and 16 flows, unlimited delivery is 483–486k / 523–537k frames/s, above preceding paced points | Repeated 20-second comparisons pass; the complete small-frame acceptance profile remains unfinished. |
| Establish the CPU-limited forwarding ceiling | Worker/process CPU and per-core ticks are recorded; a sampled profile identifies TAP I/O and polling costs | No theoretical or measured zero-loss CPU ceiling has been established. Socket placement, scheduling, cross-worker work and kernel costs still need controlled attribution. |
| Prevent split-count amplification of outer loss | Planned GSO batches end at frame boundaries, and pressure tests preserve useful tails | Runtime bounded selective repair is not implemented. Independent packet loss can still lose a split frame; aggregate-boundary alignment is not a substitute for repair. |
| Use larger confirmed path MTUs | The runtime still sends at the 1232-byte UDP baseline | Per-lane PMTU discovery and validation on larger and changing paths remain unimplemented. No performance gain from larger outer packets has been demonstrated. |
| At least 5 Gbit/s over routed IPv6 10G-EPON, RTT 4.5–7.5 ms, with nonzero loss | These results are unimpeded local loopback tests | No real-path result or independent/burst loss sweep verifies this target. Recovery, congestion control and pacing also remain unfinished. |

Loss under unlimited load is still high: the recorded MTU-1500 16-flow point
misses about 71% / 60% of offered frames despite zero admission-pressure
rejections and zero worker queue drops. TAP ingress drops and outer socket
overflows account for visible loss locations; their different units prevent
an exact frame-loss decomposition. Inner UDP receive-buffer errors are recorded
and are too small to explain the observed 64-byte goodput decline.

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
`EPERM`; `io_uring_setup` succeeded. TAP USO and io_uring read costs have
not yet been measured.

### Audit work still open

A3 is implemented with exact deadlines; A4's counter publication and A8's
queue setup are implemented. A9's contiguous copy is implemented; the two
record-cache prototypes were slower and were discarded. C4's gate is specified
above. A1 needs a decision on flow pinning or stable steering. A2 now has ordered group creation, kernel steering and lifecycle tests. The remaining A4 scheduling
changes, A5–A7, the B0 cost probes, B1–B3 and the C1–C3 generator/CPU work
remain unfinished. B4 remains deferred as requested.

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
