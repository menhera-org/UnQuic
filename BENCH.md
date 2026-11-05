# Release transport overload investigation

The original throughput collapse is reproduced and its receive-admission
failure is fixed. The strict overload plateau requirement remains **open**:
some multi-flow directions still deliver less at unlimited load than at a
paced saturation point. Neither the local **20 Gbit/s in each direction**
gate nor the real **5 Gbit/s under loss** gate has passed.

This report separates that remaining decline from the earlier near-total
collapse. Recovery, congestion control and PMTU discovery remain unfinished;
their absence does not explain away a scheduling or admission failure.

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

These are unlimited-load points from the current revision. The data file also
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
throughout all 54 recorded points.

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
evidence for further profiling, not a cycle-accurate cost model.

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
