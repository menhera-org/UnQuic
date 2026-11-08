# What blocks UnQuic's performance targets, and how to fix them

Date: 2026-11-08. Code read at `926c664`. Numbers come from `BENCH.md` and the
JSON files it links. Nothing was built or measured for this report; section 11
lists what that leaves unverified. Line numbers refer to `926c664`.

The design has two throughput targets (`initial-design.md`, R11 and section
14.2):

- Deployment: at least 5 Gbit/s of delivered inner goodput over routed IPv6
  10G-EPON (NTT NGN) with a 4.5 to 7.5 ms idle RTT and nonzero loss, on two to
  four cores.
- Local gate: at least 20 Gbit/s in each direction at once at inner MTU 1500,
  which exists to prove CPU headroom for the deployment target.

The deployment traffic is mostly VLAN-tagged (often QinQ), mostly TCP with some
QUIC-style UDP, may carry SRv6 encapsulating IPv4 or IPv6, arrives through a
bridge from a NIC, and comes in a few large flows. This report ranks blockers
by their effect on that traffic first and the local gate second.

Follow-up, November 8: [BENCH.md](BENCH.md#successor-requests-and-ack-retention-do-not-fix-both-input-paths-november-8)
records the implementation work and matched release experiments after this
report. Early successor requests still collapse with GSO; retaining ACK
records improves GSO but regresses bridged input. Neither candidate is
applied. A bridged control worker reaches 95.5% CPU, so section 2's
serialization-only conclusion does not cover the newer workload. The
original report below remains a record of its stated revision.

## 1. Verdict

The current build delivers 1.5 to 1.8 Gbit/s per direction on the eight-lane
QinQ benchmark and uses about two of its eight cores per daemon. The last
build before the shared sender was connected delivered 7.76 / 16.70 Gbit/s on
the same harness. Single-stream TCP reaches 2.01 / 2.26 Gbit/s. None of these
runs had any delay or loss, so nothing yet measures the deployment path.

The first four problems below are in the code, and each alone keeps a single
large flow below 5 Gbit/s on the NGN path. The fifth is in how the code is
measured, and it hides three of the other four.

| # | Blocker | Effect on the deployment target | Contract status of the fix |
| --- | --- | --- | --- |
| 1 | The shared sender issues at most one send quantum (64 KiB) per owner round trip for the whole path group | About 3.2 Gbit/s of outer traffic at best, 1.7 to 1.9 measured, on any path | The design labels this batching "the first shared-owner implementation" and asks for its cost to be measured first. It has been. |
| 2 | ACKs carry up to 32 ranges because holes are never pruned, and all ACKs leave through worker 0 under a fixed 4 MiB/s ceiling | At 5 Gbit/s even one-range ACKs use 85% of the ceiling; with real loss, ACKs need 1.6 to 4.2 times the ceiling | Pruning and packing need no change. Scaling the ceiling is a real contract change, required by arithmetic. |
| 3 | Per-worker transmit retention (8192 / workers frames until ACK) and 64 receive slots per lane | One flow is capped near 1.4 to 2.9 Gbit/s at 7.5 ms on four to eight workers, and tolerates only about 0.5% loss at 5 Gbit/s | The design calls these initial limits that may grow with measurement |
| 4 | At light load, CPU per bit is roughly 1.5 to 2 times what the pre-integration build used | 5 Gbit/s each way may need four to six cores per daemon; the budget is two to four | No contract involved; needs a profile |
| 5 | The benchmark has zero RTT, zero loss, non-reactive UDP, host super-frames, and a QinQ setup that the kernel hashes onto one TAP queue | Recent decisions were measured in a regime unlike the deployment, and some point the wrong way | No contract involved |

A sixth problem affects inner QUIC and other sequenced UDP: single-tag UDP is
reordered 32 to 36% under load (section 9).

Fix the measurement first, because it is cheap and every later decision
depends on it. Then remove the sender epoch, shrink and decentralize ACKs, and
size capacities for the path's bandwidth-delay product. Section 10 gives the
order and the check for each step.

## 2. Where the numbers stand

| Measurement (no delay, no loss) | Result | Source in `BENCH.md` |
| --- | --- | --- |
| Eight-lane QinQ UDP, 21 Gbit/s offered each way | 1.63 to 1.81 / 1.44 to 1.62 Gbit/s, 91 to 93% missing | November 8 A/B/A sweeps |
| Same, CPU per daemon | about 205 to 240% of 800% available | same |
| Busiest worker thread at that point | about 51 to 65% of one core | per-thread CPU in `benches/tap-rx-scan.json` |
| Single-lane QinQ UDP, unlimited | 1.87 / 1.87 Gbit/s | "Bounding fresh work preserves single-lane overload delivery" |
| Single-stream TCP, QinQ | 2.01 / 2.26 Gbit/s at 276 / 282% CPU | "TCP output coalescing" |
| SRv6, sixteen inner flows, unlimited (pre-recovery build) | 5.56 / 5.82 Gbit/s | "Encapsulated-flow hashing and tagged SRv6" |
| SRv6, one inner flow (pre-recovery build) | 1.21 / 1.17 Gbit/s | same |
| Last pre-integration QinQ sweep, 60 s | 7.76 / 16.70 Gbit/s | top of `BENCH.md` |

No thread is saturated, so today's limit is latency and serialization, not CPU.
That will change once the sender is fixed (section 7).

## 3. Blocker 5 first: make the benchmark resemble the deployment

Every decision since November 6 was measured with the same profile: IPv6
loopback with about 32 µs minimum RTT, no loss, sixteen non-reactive UDP flows
from a generator that sends 44-segment `UDP_SEGMENT` aggregates, and QinQ built
from VLAN upper devices stacked on the TAP. It differs from the deployment in
three ways that change conclusions.

### 3.1 The harness QinQ setup concentrates all input on one TAP queue

`BENCH.md` ("TAP gating mostly reflects exhausted retention credits") records
that worker 3 reads essentially all GSO input on each endpoint, and its input
gate is closed for 17 of 21.8 seconds. Earlier QinQ sections report the same
single-reader pattern. Single-tag traffic does not show it.

Kernel source explains why, for this harness only:

1. `benches/tap_transport.sh:123-128` stacks an 802.1Q device on an 802.1ad
   device on the TAP. VLAN devices advertise no VLAN transmit offload
   ([vlan_dev.c](https://github.com/torvalds/linux/blob/v6.8/net/8021q/vlan_dev.c)),
   so when the inner tag reaches the 802.1ad device,
   [validate_xmit_vlan](https://github.com/torvalds/linux/blob/v6.8/net/core/dev.c)
   pushes it into the payload. That sets `skb->protocol` to 0x8100 and leaves
   the network header at the IP header
   ([if_vlan.h](https://github.com/torvalds/linux/blob/v6.8/include/linux/if_vlan.h)).
   The 802.1ad device then puts the outer tag in skb metadata.
2. The TAP picks a queue with `__skb_get_hash_symmetric`
   ([tun.c](https://github.com/torvalds/linux/blob/v6.8/drivers/net/tun.c)).
   For a metadata tag, `__skb_flow_dissect` replaces the protocol with
   `skb->protocol` but does not advance its offset; it then reads the second
   tag at the network offset, which is the IP header
   ([flow_dissector.c](https://github.com/torvalds/linux/blob/v6.8/net/core/flow_dissector.c)).
   The dissection fails before addresses and ports, every flow gets the same
   hash, and all QinQ flows land on one queue.

Frames that arrive from a NIC through a bridge take the receive path, which
untags the outer tag and resets the network header to the inner tag before the
bridge forwards them. The dissector should then see consistent headers and
spread QinQ flows across queues. This is predicted from source and not yet
measured.

Consequences: the eight-lane QinQ results measure one starved reader as well
as the sender. The design's choice of strict hashing for stacked VLANs
(section 8.3, "QinQ measurements show that hashing across all lanes avoids
stranding transmit workers when TAP input concentrates on one queue") was
justified by this artifact, and should be re-measured once input spreads.

### 3.2 Zero RTT hides the capacity limits and reverses some results

At 32 µs RTT, frames are acknowledged almost at once, so retention limits,
receive-slot limits and window sizes never bind the way they will at 4.5 to
7.5 ms. The 12,288-credit retention experiment ("Larger retention quota
worsens overload") was rejected because extra capacity only admitted work that
expired. On a 7.5 ms path the same quota is what lets one flow keep a
bandwidth-delay product in flight (section 6). `BENCH.md` itself says
"Loaded-RTT and lossy-path measurements must guide any later increase"; those
measurements do not exist yet.

### 3.3 Host super-frames and non-reactive UDP flatter the data path

A NIC delivers forwarded UDP as individual frames, while the generator's
44-segment sends reach the TAP as one super-frame per read. TCP from a NIC
does arrive GRO-coalesced, so TCP super-frame input is realistic. Non-reactive
UDP also cannot show what reordering or late repair does to inner QUIC or TCP.

### 3.4 What to change in the harness

None of this touches the transport or its contract.

- Add outer delay. The outer path is IPv6 loopback inside the bench namespace,
  and each packet crosses `lo` once, so `tc qdisc add dev lo root netem delay
  2250us limit 100000` gives a 4.5 ms RTT and `3750us` gives 7.5 ms. The
  default netem limit of 1000 packets is below one path's worth of datagrams
  at 5 Gbit/s.
- Add outer loss on single datagrams, not aggregates. netem drops whole GSO
  aggregates, which design section 14.2 forbids for loss injection. Route the
  outer path through a router namespace (the PMTU test already builds one),
  force segmentation before the router (disable UDP segmentation offload on
  the sender-side veth), and apply netem loss on the router's egress. Verify
  with a capture that drops are single datagrams. Run independent loss at
  0.1% and 1% and one burst profile, as ROADMAP stage 3 already asks.
- Feed the TAP through a bridge. Put the generator in its own namespace, tag
  frames there, and connect it by veth to a bridge whose port is the TAP. Use
  `SEGMENTS=1` (or forced segmentation) for UDP rows so UDP arrives as
  individual frames; leave TCP as is.
- Add reactive traffic: iperf3 TCP with one and four streams per direction,
  plus the existing UDP rows reporting reordering.
- Run two and four workers, the deployment's core budget, alongside eight.
- Record per-thread voluntary context switches from `/proc/<pid>/task/*/status`
  next to the CPU ticks the harness already snapshots. They count wake-ups,
  which matter in section 7.
- Run final acceptance where `SO_RCVBUFFORCE` succeeds. Unprivileged
  containers fall back to 425,984-byte outer receive buffers instead of the
  requested 8 MiB, and production hosts will not.
- Freeze a lab profile for the real-access target from these rows. Design
  section 14.2 lists the flow count, loss profile and latency bound as still
  open.

## 4. Blocker 1: the shared sender serializes on one quantum

### Mechanism

- `Grants::reserve` refuses a grant when the sum of all outstanding
  reservations would exceed `min(send quantum, 64 datagrams)`
  (`src/pacing/grants.rs:20-27`, `:202-217`). BBR's quantum is
  `min(pacing rate × 1 ms, 64 KiB)` (`src/bbr/controller.rs:778-782`), so
  above about 0.5 Gbit/s the whole path group shares 65,536 bytes.
- The first completion closes issuance (`src/pacing/sender.rs:597`), and so
  does any arriving ACK or loss report (`src/worker/sender/owner.rs:172`).
  Nothing commits until every issued grant has reported
  (`src/pacing/sender.rs:621`), and issuance reopens only after the feedback
  pass (`:700-705`).
- Each step between a worker and worker 0 is an SPSC push plus an eventfd
  wake.

Measured result ("Current sender service measurements"): epochs average 1.02
grants, grants average 35 to 39 KB, and consecutive grants are 163 to 171 µs
apart. That is 1.7 to 1.9 Gbit/s of outer traffic, the delivered rate. With
the controller's rate and window overridden, grants grew to about 49 KB and
the interval fell to 109 to 179 µs, which is still only 2.2 to 3.6 Gbit/s.

This ceiling does not depend on the path. On the NGN path it caps a session
below 5 Gbit/s regardless of RTT, loss or flow count.

### Fix, in two steps

Design section 9 states the batching as provisional ("For the first
shared-owner implementation ... Measure this batching cost before accepting
the performance gates") and separately caps "the combined outstanding burst
allowance at the smaller of BBR's send quantum and 64 maximum-size datagrams".
The steps below change the first paragraph, then the second sentence, and keep
everything else in section 9: one controller per path group, reservation
before grant, credit held until reported, the 100 µs grant lifetime, pacing,
and accounting for every successful transmission.

Step A1 removes the epoch and keeps the combined cap.

- Release a grant's reservation when its sent/unused report arrives, and
  allow new grants while other grants are outstanding.
- Take each grant's delivery-sampling snapshot (delivered count, delivered
  time, first-sent time, application-limited state and inflight) when the
  grant is reserved, and store it in the reserved sampling slots. Commit each
  report when it arrives, with its own pre-syscall timestamp. Commit order
  then stops mattering.
- If an ACK names a slot whose send report has not been committed yet, hold
  that ACK until the commit. Each packet's send still precedes its ACK.

`BENCH.md` warns that overlapping epochs "without preserving send/ACK ordering
would invalidate delivery sampling". The snapshot and the ACK hold keep that
ordering. The only change is that the snapshot is taken at reservation, about
23 µs (measured reservation-to-send) before the send. That error is under 1%
of a 4.5 ms path. On 32 µs loopback it is large, which is one more reason to
judge sender changes on the delayed rows.

Credit then returns 75 to 107 µs after issue (the measured
reservation-to-report time) instead of after the whole epoch and feedback
pass. One quantum per 75 to 107 µs is 4.9 to 7.0 Gbit/s of outer traffic.
That is enough to start measuring the deployment target, but leaves little
margin on a busy two-to-four-core host where each hop takes longer.

Step A2 makes the quantum a per-grant limit and lets the congestion window
bound concurrency. In `Grants::reserve`, check `bytes <= burst` instead of
`reserved + bytes <= burst`; keep the window check as is. The local gate needs
this by arithmetic: 20 Gbit/s needs 2.8 GB/s of outer traffic, one 64 KiB
grant every 23 µs, and one hop alone takes 10 to 18 µs. Wire bursts stay
bounded: grants are still spaced at `bytes / rate` and expire after 100 µs, so
overlapping grants can add at most 100 µs of pacing-rate bytes to one quantum,
about 128 KB in total at 5 Gbit/s.

On loopback the window also binds after A1 and A2. BBR sizes it from a 32 µs
minimum RTT, about 110 KB, while requests wait 530 to 600 µs for permission
and the window blocks 36 to 45% of open issuance time ("Grant-wait
attribution", "ACK timing after suffix retention"). That matters only for the
local gate. Draft-07's offload budget already allows a larger floor; the
three-quanta experiment ("ACK packing and offload-window experiments") gained
12 to 28%. On the NGN path the bandwidth-delay product, 2.8 to 4.7 MB at
5 Gbit/s, dwarfs any of this.

Tests to extend: the reversed-report and all-64-worker tests in
`src/pacing/sender.rs`, the commit/feedback ordering tests in
`src/worker/sender/owner.rs`, and the draft-07 vectors. Add one test that
reserves on eight workers, reports in reverse order, and checks that inflight,
delivered count and rate samples match the in-order case.

## 5. Blocker 2: ACK size and ACK routing

### Mechanism

- Range construction walks back from the highest packet to the window floor,
  up to 65,536 numbers, and reports the newest 32 ranges
  (`src/reassembly.rs:110-126`, `:330-347`).
- Holes never fill. Repairs take new packet numbers, and canceled sender
  reservations stay known-unsent holes (design sections 6 and 8.2). A hole
  stays in every ACK until 32 newer holes push it out.
- Every ACK goes through worker 0. Receive owners push feedback to worker 0
  (`src/worker/feedback.rs:111-170`), which sends one ACK record per datagram
  on the session's lane 1 socket (`src/worker.rs:3657-3702`) under the path
  group's 4 MiB/s, sixteen-datagram ceiling (`src/pacing.rs:12-13`). At the
  peer, all those ACKs arrive on lane 1 and are routed again to the transmit
  owners of the acknowledged lanes.

Measured on loopback without any network loss ("ACK timing after suffix
retention"): 22 to 25 ranges per ACK on average, 40 to 58% of ACKs at the full
32 ranges, about 500 outer bytes each. Before suffix retention it was 99.8% at
32 ranges. Raising the ceiling from 4 to 16 MiB/s alone doubled goodput
("ACK range overhead and control-budget pressure").

### Arithmetic at the deployment rate

An ACK-only datagram is 108 outer bytes plus 16 per range. At 5 Gbit/s of
1500-byte inner packets in QinQ frames, the sender emits about 458,000
datagrams per second, so the one-ACK-per-16-packets rule (design section 6)
yields about 28,600 ACKs per second, before gap-triggered and timer ACKs.

| Ranges per ACK | Outer bytes | ACK traffic | Share of 4 MiB/s |
| ---: | ---: | ---: | ---: |
| 1 | 124 | 3.5 MB/s | 85% |
| 2 | 140 | 4.0 MB/s | 95% |
| 8 | 236 | 6.8 MB/s | 161% |
| 32 | 620 | 17.7 MB/s | 423% |

Real loss makes this worse. On eight lanes, each lane's 65,536-number window
spans about 1.1 s at that rate. At 0.01% loss a window holds about seven
permanent holes; at 0.1% it holds about 66, so every ACK carries 32 ranges.
Each loss also triggers an immediate ACK, adding about 4,600 ACKs per second at
1% loss.

When the ceiling binds, ACKs wait. The measured wait was 150 to 170 µs per ACK
with 32-range ACKs. Late ACKs stall the window, the stall causes expiry, and
expiry creates more holes.

### Fixes

5a. Prune ranges after repetition. No contract change. Keep a per-lane floor:
once a range has been carried by three transmitted ACKs, later ACKs stop
before it. Design section 6 says "Emit at most the newest 32 ranges" and
"Missing older ranges can cause a bounded repair, never unbounded ACK
history". Cutting from the old end satisfies both. The cost is a spurious
repair when all three ACKs carrying a range were lost; the receiver discards
the duplicate as terminal. At 1% ACK loss that happens about once per million
ranges. `Lane::ack_transmitted` (`src/reassembly.rs:350`) is already the hook
for counting transmissions. Expected size: one or two ranges per ACK, 124 to
140 bytes, with or without network loss. `BENCH.md` notes that dropping older
ranges without delivery evidence would change recovery behavior; it changes it
only after three lost ACKs, which the design already permits.

Assigning packet numbers as late as possible, so expired or trimmed plans
leave no holes, would also help, but pruning removes the size problem
whatever creates the holes.

5b. Pack ACK records. No contract change: design section 6 lets established
packets mix records. The November 7 packing experiment found no gain, but it
combined at most four same-session records "within the base packet ceiling"
when records were about 400 to 520 bytes, so only two or three fit in a
datagram. After 5a,
records are 28 to 44 bytes, and packing every due record for the same peer
path cuts the cost per ACK from 124 bytes to roughly 40 to 60 when several
lanes are due together. A single busy lane gains nothing.

5c. Send ACKs from the receive owner. No contract change by itself: "ACKs may
travel through any local transmit lane" (design section 6). Today each ACK
costs two hops where it is sent (to worker 0 and back) and a routing hop where
it arrives (from lane 1's receive owner to the acknowledged lane's transmit
owner). With the lane numbering in section 9, an ACK sent on the receive
owner's own lane arrives at the peer worker that owns the acknowledged lane,
so all three hops disappear; only the report to worker 0 for BBR remains. The
ceiling is the obstacle: it is a per-group
budget owned by worker 0, and splitting a fixed 4 MiB/s across eight workers
would hurt a single busy lane. 5d removes the obstacle.

5d. Scale the control ceiling with received traffic. This is a contract
change to design section 9 ("share a separate 4 MiB/s ceiling per path group
with a 16-datagram burst"), and it is required. Even perfect one-range ACKs
from one 5 Gbit/s flow use 85% of the ceiling. With 1% loss, gap ACKs push it
to about 110% before any handshake or PROBE_REPLY traffic, and the local gate
needs three to four times the ceiling. Suggested text: ACK-only and
PROBE_REPLY traffic may use up to 4 MiB/s plus 1/32 of the outer bytes the
same worker received on the sessions whose ACKs it sends, over the same
interval. One thirty-second
covers even unpruned 32-range ACKs every 16 datagrams (about 2.6%). The limit
stays a ceiling, idle behavior is unchanged, handshake limits are unchanged,
and every byte stays in the counters. Per-worker accounting needs no shared
state, which makes 5c straightforward.

5e. Later: piggyback ACK records on DATA going to the same peer. The design
allows it, and inner TCP always produces reverse DATA, so most ACK-only
datagrams would disappear for TCP traffic. It interacts with retained DATA
plans, whose packet bytes are fixed, so do it after 5a to 5d are measured.

## 6. Blocker 3: capacities sized at loopback RTT

### Transmit retention

Each worker may retain `8192 / tx_workers` frames per session
(`src/worker.rs:304-306`, checked at `:2297-2302` and `:2385`). A frame stays
charged until all its bytes are acknowledged
(`src/worker/recovery.rs:369-375`), and one flow uses one lane on one worker.
So one flow can keep at most one worker's share in flight.

Upper bound for one flow, assuming 1500-byte inner packets and a retention time
of RTT plus 1 ms for ACK delay and local queueing:

| Workers | Frames per worker | 4.5 ms RTT | 7.5 ms RTT |
| ---: | ---: | ---: | ---: |
| 2 | 4096 | 8.9 Gbit/s | 5.8 Gbit/s |
| 4 | 2048 | 4.5 Gbit/s | 2.9 Gbit/s |
| 8 | 1024 | 2.2 Gbit/s | 1.4 Gbit/s |

These are upper bounds. Unsent fresh frames (up to 2 MiB per lane) share the
same quota, and BBR's probing phase needs about 1.25 times the
bandwidth-delay product in flight.

Fix: let a busy worker use session credit that other workers are not using.
Replace the static split with a per-session budget held in an atomic counter
and charged per source or per grant rather than per frame; keep a per-worker
cap well above `8192 / workers`. Atomics are allowed on hot paths; the rule is
no locks and no allocation. Then size the session limit from `memory_limit`
and the path, as design section 8.2 ("initial implementation limits") and
section 13 ("Bounds should scale with the configured memory ceiling and
measured path") already allow. The 16,384-credit variant needed 274.7 MB with
eight workers, above the 256 MiB default, so record what each size costs and
let operators raise `memory_limit`.

### Receive slots

Each receive lane has 64 frame slots (`src/reassembly.rs:18`). A frame that
loses a piece waits for its repair, about 1.5 RTT after the original. Each lost
datagram usually leaves two frames incomplete, because DATA packs one frame's
tail with the next frame's head.

Average slots occupied by incomplete frames on one lane carrying 5 Gbit/s,
assuming the repair lands 1.5 RTT plus 1 ms after the loss:

| Datagram loss | 4.5 ms RTT | 7.5 ms RTT |
| ---: | ---: | ---: |
| 0.1% | 7 | 11 |
| 0.5% | 35 | 56 |
| 1% | 71 | 112 |

A burst of about 60 consecutive lost datagrams fills the table at once. Above
64 slots, admission pressure drops datagrams without ACK, the sender counts
them as loss, and BBR cuts inflight once loss passes 2%. So one flow at
5 Gbit/s tolerates roughly 0.5% loss at 7.5 ms. Many flows spread over many
lanes tolerate more, but the deployment's worst case is few large flows.

Fix: share receive slots across the lanes a worker owns, keeping the session
total of 4096 frames inside design section 8.2's limit of 8192. `BENCH.md`
lists the shared pool as "an experiment to evaluate". Evaluate it on the lossy
delayed rows, where one busy lane is the case that matters.

### What tunnel repair can do for inner TCP

A repair reaches the receiver about 1.5 RTT after the lost original, and v1
delivers completed frames at once without reordering them. Linux TCP marks a
segment lost once later segments have been delivered for longer than a small
reordering window, a fraction of the minimum RTT that widens only after DSACKs
reveal spurious retransmissions. Inner TCP will therefore often retransmit and
cut its window before the tunnel's repair arrives, then undo the cut after the
DSACK. The lossy rows
with inner TCP will show whether tunnel repair improves goodput. If it does
not, the stage 5 candidate in `ROADMAP.md` (a bounded reorder queue) is the
next measured step. The roadmap forbids it before stage 5, and this report does
not change that.

## 7. Blocker 4: CPU per bit on two to four cores

| Build | Load | CPU per daemon | Cores per Gbit/s each way |
| --- | --- | ---: | ---: |
| Current, eight lanes | 1 Gbit/s each way | 83 to 125% | 0.85 to 1.25 |
| Current, one lane | 1 Gbit/s each way | 69 to 70% | about 0.7 |
| Pre-integration, eight workers | 2 Gbit/s each way | 117 to 121% | about 0.6 |
| Pre-integration QinQ handoff | 8 Gbit/s each way | 249 to 262% | about 0.33 |

These builds differ in traffic and harness details, so the ratio is
approximate. If today's cost per bit at 1 Gbit/s held at higher rates,
5 Gbit/s each way would need four to six cores per daemon. Batching lowers the
cost per bit as load rises (the pre-integration build fell from 0.6 to 0.33
cores per Gbit/s between 2 and 8 Gbit/s), so this overstates the need
somewhat. The deployment has two to four cores, and on a real host the NIC's
softirq work shares them.

Nothing attributes the increase yet; no CPU profile exists after the sender,
ACKs and recovery were connected. Message traffic is a likely part. At
5 Gbit/s each way, each daemon both sends about 28,600 ACKs per second and
processes about as many. Each ACK crosses workers up to five times (to
worker 0 and back for sending; at the receiver, to the transmit owner, to
worker 0 as a report, and back). Grants add two hops each, about 9,500 grants
per second at 64 KiB. That is on the order of 160,000 cross-worker messages per
second, many of which wake a sleeping thread. This is an estimate to check.

What to do:

1. Profile at fixed paced rates below saturation (1 and 2 Gbit/s each way) on
   two and four workers. Reuse the sampled thread-CPU stage timing from the
   earlier profiles (one turn in 256, calibrated clock), extended with owner,
   ACK construction, ACK application, history, journal and coverage stages.
   Add context-switch counts (section 3.4).
2. Sections 5a to 5c remove most ACK bytes, ACK syscalls and ACK hops.
3. Recheck after A1, which removes the epoch's idle waits but not its hops.

## 8. SRv6 and single large flows

The encapsulation-aware flow key (`BENCH.md`, "Encapsulated-flow hashing and
tagged SRv6") spreads many inner flows inside one SRv6 tunnel across lanes,
for both IPv4 and IPv6 inner packets. One large inner flow still stays on one
worker, and SRv6 frames get no TAP offload in either direction: a virtio
header cannot describe IP-in-IPv6 segmentation, so the kernel segments them
before the TAP and each frame costs one read and one write. The pre-recovery
build carried one such flow at about 1.2 Gbit/s using more than one core.

No in-contract change moves one flow past one worker's per-frame cost. Either
the frozen real-access profile (section 3.4) assumes several flows per
tunnel, or single-flow striping (a stage 5 candidate) becomes a requirement.
That is a project decision.

## 9. Reordering of single-tag UDP

Unfragmented UDP with at most one VLAN tag uses reader-local lanes (design
section 16, "Reordering policy"). Under load it is reordered 32 to 36%
(`BENCH.md`, "Current performance gaps"). Linux's TAP driver steers a flow to
the queue that last wrote its reverse direction, and both directions share one
symmetric hash. Reader `w` sends on wire lane `w + 1`, which the peer
registers on worker `(cookie_low32 + w) mod n`, so each round trip moves the
flow pair to another queue unless `2 × cookie_low32` is a multiple of `n`. The
cookie experiment ("Cookie placement and transmit FIFO investigation")
confirmed it: with an aligned cookie and the FIFO fix, reordering fell from 39
to 48% to 2 to 3%. Throughput did not improve, so no change was adopted.

A non-reactive generator cannot show the cost. Inner QUIC treats a packet as
lost once three later packets are acknowledged (RFC 9002), so a quarter of
packets arriving out of order means spurious retransmissions and smaller
windows for every QUIC connection through the tunnel.

Fix, with no contract change: number each session's transmit lanes so that
worker `w` sends wire lane `1 + ((w - cookie_low32) mod n)`. The peer then
registers that lane on worker `w` for every cookie, provided both ends use the
same queue count and the lane count is a multiple of it; other configurations
keep the forwarding fallback. The receive-owner formula,
the classic BPF program and the wire format stay as they are. This also gives
5c its zero-hop ACK routing. Judge it on the reactive rows. The aligned run
loaded some workers to 98% while others stayed under 50%; on the deployment's
few large flows, order matters more than that balance.

## 10. Order of work

| Step | Change | Contract | Check |
| --- | --- | --- | --- |
| 1 | Harness: outer delay, single-datagram loss, bridged input, TCP rows, two and four workers, context switches (section 3.4) | none | QinQ input spreads across TAP queues with bridged input; delayed rows run at 5 Gbit/s offered |
| 2 | A1: remove the sender epoch, snapshot sampling at reservation (section 4) | replaces the provisional batching paragraph in section 9 | grant interval falls from about 165 µs toward the reservation-to-report time; draft-07 vectors still pass |
| 3 | 5a and 5b: prune ACK ranges after three transmissions, pack records | none | mean ranges per ACK near one on loopback and below two at 1% loss |
| 4 | Lane numbering aligned with the peer's owner (section 9) | none | reordering on single-tag UDP near the strict-hashing level |
| 5 | 5d and 5c: scale the control ceiling per worker with received bytes, send ACKs from the receive owner | changes the 4 MiB/s sentence in section 9 | no control-pacing waits at 5 Gbit/s with 1% loss; fewer cross-worker messages |
| 6 | A2: per-grant quantum, window-bounded concurrency | changes the combined-burst sentence in section 9 | the local gate becomes measurable; wire bursts stay within quantum plus 100 µs of pacing |
| 7 | Transmit credit borrowing and shared receive slots, sized from the delayed and lossy rows (section 6) | within section 8.2's initial limits and section 10's queue rule | one flow reaches 5 Gbit/s at 7.5 ms with 0.5% and 1% loss on four workers |
| 8 | CPU profile and reductions (section 7) | none | at most about 0.6 cores per Gbit/s each way, so 5 Gbit/s each way fits in three of four cores |
| 9 | Five-run local gate and the frozen real-access profile | none | the `BENCH.md` gate definition and the profile from step 1 |

Measure each step with a matched A/B/A run, as `BENCH.md` already does, and
report the delayed and lossy rows next to the loopback rows. Steps 2 and 3 are
independent and can proceed in parallel with step 1.

## 11. What this report did not do

- No code was built, run or changed. Every number comes from `BENCH.md`, its
  JSON files, and the code as read at `926c664`.
- The QinQ queue-concentration mechanism comes from Linux 6.8 source and
  matches the recorded single-reader behavior. That bridged QinQ spreads
  across queues is a prediction; step 1 tests it. The benchmarks also ran on
  Linux 7.0, which was not read.
- The capacity tables in section 6 assume 1500-byte inner packets, one flow
  per lane, a retention time of RTT plus 1 ms and repair after 1.5 RTT plus
  1 ms. Loaded RTT is longer than the idle RTT used here, which makes the
  limits tighter.
- The CPU comparison in section 7 spans builds with different traffic, and the
  message count is an estimate. A profile is the first step there.
- The netem and bridge commands in section 3.4 were not tried in the test
  container; verify each with a packet capture before relying on it.
