# Release transport overload investigation

The original throughput collapse is reproduced and its receive-admission
failure is fixed. A later TAP readiness busy loop is now fixed too. Subsequent
20-second MTU-1500 sweeps still show rate-dependent declines: the latest
unchanged MTU-1500 baseline repeat falls 5.7% in reverse under unlimited load.
The latest 64-byte baseline repeat falls 9.1% / 5.1%.
Split-pool, deadline-cache and first-reader lane-association trials were rejected
and reverted after measurements.
The strict plateau requirement remains unmet. Neither the local **20 Gbit/s in each direction** gate nor the
real **5 Gbit/s under loss** gate has passed.

This report separates that remaining decline from the earlier near-total
collapse. Recovery, congestion control and PMTU discovery remain unfinished;
their absence does not explain away a scheduling or admission failure.

## Current performance gaps

Status after the TAP readiness fix and per-peer payload-limit comparison.
The split-pool, deadline-cache and lane-association experiments below are not
in the runtime.
The latest results use the 20-CPU single-socket profile and capped
socket-buffer fallback below.
L→R / R→L pairs describe simultaneous traffic, not separate one-way peaks.

| Requirement | Current evidence | Remaining gap |
| --- | --- | --- |
| At least 20 Gbit/s each way at inner MTU 1500 | The latest unchanged 1452-byte baseline repeats deliver 3.79–3.84 / 4.87–5.09 Gbit/s of inner UDP payload under unlimited load | Roughly 5.2× / 3.9× more delivered goodput is needed even relative to the better unlimited point. The gate is not close to passing. |
| Goodput must plateau under excess traffic | An unchanged MTU-1500 baseline repeat declines 5.7% in reverse; the latest 64-byte repeat declines 9.1% / 5.1% | The verified readiness busy loop is fixed. Rate-dependent declines persist and need further profiling. Longer runs, more frame mixes and broader flow/session coverage remain unverified. Removing the earlier admission cascade does not satisfy the plateau requirement. |
| Small-frame overload must remain efficient | The latest unchanged 64-byte, 16-flow unlimited points deliver 419–480k / 455–518k frames/s | The second repeat falls below its paced predecessor in both directions. Earlier passing repeats do not establish a sustained plateau; the small-frame gate remains open. |
| Establish the CPU-limited forwarding ceiling | Worker/process CPU and per-core ticks are recorded; a sampled profile identifies TAP I/O and polling costs | No theoretical or measured zero-loss CPU ceiling has been established. Socket placement, scheduling, cross-worker work and kernel costs still need controlled attribution. |
| Prevent split-count amplification of outer loss | Planned GSO batches end at frame boundaries, and pressure tests preserve useful tails | Runtime bounded selective repair is not implemented. Independent packet loss can still lose a split frame; aggregate-boundary alignment is not a substitute for repair. |
| Use larger confirmed path MTUs | The runtime accepts a per-peer 1232–1452-byte payload assertion; a controlled 1452-byte sweep shows only a modest change | Per-lane PMTU discovery and validation on larger and changing paths remain unimplemented. Configured capacity is not confirmed PMTU. |
| At least 5 Gbit/s over routed IPv6 10G-EPON, RTT 4.5–7.5 ms, with nonzero loss | These results are unimpeded local loopback tests | No real-path result or independent/burst loss sweep verifies this target. Recovery, congestion control and pacing also remain unfinished. |

Loss under unlimited load is still high: the latest unchanged 1452-byte MTU-1500 points
miss about 70% / 59–61% of offered frames despite zero admission-pressure
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
above. A1's immutable lane-association trial below was rejected; strict pinning
still needs stable steering for dependable locality. A2 now has ordered group
creation, kernel steering and lifecycle tests. The remaining A4 scheduling
changes, A6–A7, the B0 cost probes, B1–B3 and the C1–C3 generator/CPU work
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
The trial is reverted. A1 remains open; changing lanes after migration would
weaken the agreed pinning rule, so the runtime keeps deterministic hashing.
