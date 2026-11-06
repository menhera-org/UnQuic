# Performance audit: why the local 20 Gbit/s gate fails, and what to change

Date: 2026-11-05. Code audited at `3c42142`. Data from [BENCH.md](BENCH.md) and
[benches/tap_transport-overload.json](benches/tap_transport-overload.json).

This is the historical audit of `3c42142`. Subsequent measurements, rejected
experiments and current decisions are recorded in [BENCH.md](BENCH.md) and
[ROADMAP.md](ROADMAP.md); they supersede the recommendations below.

This file is an instruction list for whoever does the next performance work.
Each fix names the code it touches, what to change, what it should buy, and
how to check it. Gains are estimates derived from the recorded profile, not
measurements; measure after each tier before starting the next.

## 1. Verdict

The gate cannot pass with the current data path, no matter how much userspace
overhead is removed. The transport makes one `read()` and one `write()` on a
TAP descriptor per inner frame, plus a cross-worker handoff for about three
quarters of all frames in both directions. At inner MTU 1500, 20 Gbit/s in each
direction is 1.65 million frames per second per direction, so each daemon must
perform 3.3 million TAP system calls per second. The measured cost of those
calls alone exceeds the four cores each daemon was given.

Budget per daemon-second at the gate, using per-operation costs taken from the
16-flow unlimited run (left endpoint, `after` revision, 286% CPU):

| Operation | Rate needed at 20 Gbit/s each way | Measured cost | Core-seconds |
| --- | ---: | ---: | ---: |
| TAP read (1514-byte frame) | 1.65 M/s | 1.4 µs | 2.3 |
| TAP write (frame into the inner stack) | 1.65 M/s | 2.5 µs | 4.1 |
| Outer datagram sent (GSO, 1232-byte payload, 1.3 per frame) | 2.15 M/s | 0.6 µs | 1.3 |
| Outer datagram received and reassembled (GRO) | 2.15 M/s | 0.7 µs | 1.5 |
| Per-turn fixed work (epoll, timers, eventfd, counters) | | | about 0.5 |
| Total | | | about 9.7 |

Four cores give 4.0. The deficit is 2.4 times, and almost all of it is kernel
time inside TAP syscalls. The small-frame rows in BENCH.md say the same thing
from the other side: delivery plateaus at 160 to 180 thousand frames per
second per flow regardless of frame size, which is a per-frame cost ceiling,
not a byte ceiling.

How the costs were derived: the sampling profile attributes 24% of CPU to TAP
reads, 22% to TAP writes, 14% to transmit flush, 7% to epoll and about 15% to
receive processing without TAP writes. The left daemon read about 490 thousand
frames per second from TAP (1.09 million offered minus 600 thousand TAP TX
drops per second) and wrote 254 thousand per second. Dividing the profile
shares of 2.86 cores by those rates gives the per-operation costs above. The
outer path is already amortized by GSO and GRO, so its per-datagram cost is
mostly the data copy, which is cache-cold today because of the handoffs.

Two further conclusions follow:

1. Fixes that only trim userspace (tier A below) are necessary but reach
   perhaps 7 to 10 Gbit/s per direction on four cores. They must land first
   because they also remove the drops that make the current numbers noisy.
2. Reaching 20 Gbit/s needs fewer kernel entries per inner frame at the TAP
   boundary (tier B), and the measurement harness must be able to offer and
   absorb 20 Gbit/s per direction, which it cannot today (tier C).

## 2. Evidence in the code

Read these before changing anything. Line numbers are at `3c42142`.

- **One syscall per frame, both directions.** [src/tap.rs:167-187](src/tap.rs#L167-L187)
  reads one frame per `read`; [src/tap.rs:193-206](src/tap.rs#L193-L206)
  writes one frame per `write`. [src/worker.rs:1662-1724](src/worker.rs#L1662-L1724)
  loops up to 64 reads per ready TAP and ends every loop with a wasted
  `EAGAIN` read unless 64 was reached. [src/worker.rs:2000-2043](src/worker.rs#L2000-L2043)
  writes frames one by one. `TUNSETOFFLOAD` is set to zero and `IFF_VNET_HDR`
  is off at [src/tap.rs:77-85](src/tap.rs#L77-L85), so the kernel never hands
  the daemon a super-frame and the daemon can never hand one back.
- **Three quarters of transmit frames change worker.** [src/worker.rs:1694-1697](src/worker.rs#L1694-L1697)
  picks the lane as `flow_hash % tx_lanes` and the owner as `lane % workers`.
  With four lanes and four workers, a frame read on TAP queue `w` goes to
  worker `w` only one time in four. Each crossing costs an SPSC push, an
  `eventfd` write, a cache-cold read of the frame bytes by the owner's
  `sendmmsg`, and a `Free` event plus another `eventfd` write to return the
  buffer ([src/worker.rs:911-951](src/worker.rs#L911-L951)).
- **Three quarters of received datagrams change worker.** The receive owner
  is `(lane - 1 + session) % queue_count` at [src/worker.rs:1489-1490](src/worker.rs#L1489-L1490),
  while the kernel's reuseport hash picks the socket from the outer
  four-tuple. Nothing aligns them, so most GRO aggregates are forwarded as
  `Event::Packet` ([src/worker.rs:1514-1524](src/worker.rs#L1514-L1524)) and
  the reassembly copy at [src/reassembly.rs:184-209](src/reassembly.rs#L184-L209)
  runs on a core that did not receive the bytes.
- **A timer syscall nearly every turn.** [src/worker.rs:2362-2397](src/worker.rs#L2362-L2397)
  computes the deadline as the minimum over every pending and batched frame.
  Under load the oldest frame leaves every turn, the minimum moves, and
  [src/worker.rs:151-153](src/worker.rs#L151-L153) re-arms `timerfd` because
  it only skips an exactly equal deadline.
- **Other per-turn fixed costs.** `epoll_wait` with a zero timeout whenever
  there is backlog ([src/worker.rs:778-781](src/worker.rs#L778-L781)); up to
  one `eventfd` write per peer worker per turn ([src/worker.rs:943-951](src/worker.rs#L943-L951));
  all counters republished every turn ([src/worker.rs:766](src/worker.rs#L766),
  [src/worker.rs:819-858](src/worker.rs#L819-L858)); a scan of every
  pending frame and ready batch at [src/worker.rs:754-764](src/worker.rs#L754-L764).
  The profile recorded 4 thousand to 81 thousand turns per second per worker,
  with the control worker spinning fastest.
- **Shallow pools turn jitter into drops.** Every pool entry is a 64 KiB
  buffer ([src/worker.rs:271](src/worker.rs#L271), [src/worker.rs:316](src/worker.rs#L316))
  and there are 128 per worker ([src/runtime.rs:25](src/runtime.rs#L25)), of
  which 8 are the receive reserve ([src/worker.rs:273](src/worker.rs#L273)).
  So a worker can hold 120 frames of 1.5 KB in 7.5 MiB of storage, which is
  290 µs of traffic at the gate rate, and at most 8 GRO aggregates per
  `recvmmsg`. Each receive lane has 64 frame slots ([src/reassembly.rs:18](src/reassembly.rs#L18)),
  which is 155 µs at the gate rate. The receive socket keeps the kernel
  default buffer (no `SO_RCVBUF` at [src/socket.rs:106-119](src/socket.rs#L106-L119)),
  about 208 KiB, which is three GRO aggregates. The TAP ring is the kernel
  default of 500 frames per queue. These are the sources of the 146 thousand
  socket-overflow SKBs and 6.3 million TAP TX drops in the drop table.
- **Outer payload fixed at 1232 bytes.** [src/lib.rs:38](src/lib.rs#L38) and
  [src/worker.rs:446](src/worker.rs#L446). A 1514-byte frame becomes 1164 plus
  350 bytes and about 1.3 datagrams after tail packing. The socket API already
  accepts up to 1452 ([src/socket.rs:23](src/socket.rs#L23)), which gives
  1384 plus 130 bytes and about 1.1 datagrams per frame.
- **Three decode passes per datagram.** [src/wire.rs:543-567](src/wire.rs#L543-L567)
  validates every record, [src/wire.rs:575-586](src/wire.rs#L575-L586) decodes
  them again on iteration, and [src/reassembly.rs:386-479](src/reassembly.rs#L386-L479)
  iterates twice. The piece copy at [src/reassembly.rs:184-209](src/reassembly.rs#L184-L209)
  copies 64 bytes per loop iteration with a mask test each time, even when the
  whole range is fresh.

## 3. Fixes

Do tier A completely, run the 16-flow sweep, commit the numbers to BENCH.md,
then start tier B. Tier C can proceed in parallel because it touches only the
bench script and generators. Keep the existing tests green after every item;
the ones named below are the ones each item is most likely to break.

### Tier A: remove waste that is already measurable

**A1. Make transmit lanes worker-local.**
In `read_tap` ([src/worker.rs:1694-1697](src/worker.rs#L1694-L1697)) choose
among the lanes this worker owns: `own = lanes where lane % workers == id`,
then `buffer.lane = own[flow_hash % own.len()]`. Keep the current handoff as
the fallback when a worker owns no lane (`tx_lanes < queue_count`). The flow
pinning rule holds because the kernel's multiqueue TAP steers a flow to one
queue by its own hash, so a flow still sees one lane. Document in
config-model.md that `tx_lanes` should be a multiple of `queue_count`.
Buys: no SPSC push, no `eventfd` write and no buffer return for transmit
frames; the `sendmmsg` copy reads bytes the same core just wrote.
Check: the container runtime test, `transmit_batches_retain_partial_sends_expire_frames_and_quiesce`,
and a new assertion in the ten-second storm that `queue_drops` stays zero.

**A2. Steer receive lanes to their owner with a classic BPF reuseport program.**
The kernel runs `SO_ATTACH_REUSEPORT_CBPF` programs on the UDP payload, so the
program can read the lane at payload offset 36 and the cookie at 4..20. Emit
the program with `libc::sock_fprog` (no new crate, no CAP_BPF): load halfword
at 36, subtract 1, add the word at offset 16 (low cookie bits), modulo the
socket count, return. Change the owner rule at [src/worker.rs:1489-1490](src/worker.rs#L1489-L1490)
to the same expression, `(lane - 1 + cookie_low32) % queue_count`, and update
the sentence in initial-design.md section 10 and README.md that state the
session-index rule. The cookie term keeps single-lane sessions spread, which
was the reason for the session index.
The program returns a socket index in bind order, so bind every reuseport
member of an endpoint on worker 0 in worker order, attach the program after
the last bind, and hand the descriptors to the other workers through the
existing control events instead of letting each worker bind in `setup`
([src/worker.rs:1099-1122](src/worker.rs#L1099-L1122)). A failed bind retries
the whole endpoint group, as now. Keep the forwarding path: it still covers a
group with a missing member and lane 0 handshakes.
Buys: no `Event::Packet` handoffs for data; the reassembly copy is cache-hot;
GRO aggregates never mix lanes, so steering by lane is consistent with GRO.
Check: `colliding_receive_flows_register_on_distinct_owners` must be rewritten
for the new rule; add a container test that sends on four lanes and asserts
each worker's `rx_outer_bytes` is nonzero while its peers' `queue_drops` are
zero.

**A3. Stop re-arming the timer every turn.**
In `Reactor::arm` ([src/worker.rs:151-153](src/worker.rs#L151-L153)) keep the
armed deadline when it is at or before the requested one; frame deadlines are
100 ms out and only ever move later, so the timer fires once and is re-armed
once per expiry instead of once per turn. Also round requested deadlines up to
the next millisecond so equal-millisecond requests compare equal.
Check: `notifications_timers_and_fd_interests_remain_ready_until_consumed`.

**A4. Cut the other per-turn fixed costs.**
Publish counters at most once per millisecond or every 256 turns, not every
turn ([src/worker.rs:766](src/worker.rs#L766)). Track the pending-frame
deadline minimum incrementally instead of scanning `pending` and every batch
in `deadline` ([src/worker.rs:2387-2395](src/worker.rs#L2387-L2395)); a
monotonic queue only needs its front. Replace the per-turn scan at
[src/worker.rs:754-764](src/worker.rs#L754-L764) with a `blocked_lanes`
bitmask maintained where `tx.blocked` changes. The trailing `EAGAIN` read in
`read_tap` cannot be avoided (a TAP read has no short-read signal), so raise
the per-event read budget from 64 to 128 to halve its share; A6 supplies the
pool depth that allows it. After A1 and A2 the `eventfd` writes mostly
disappear by themselves.
Buys: roughly the 7% epoll share and most of the 18% "other" share.
Check: no new test; watch `turns` per second in the sampling profile fall.

**A5. Raise the outer payload to 1452 for the local gate.**
Add a per-peer `outer_payload_limit` (1232 to 1452, default 1232) to the
config and thread it to `TxBatch::prepare` ([src/worker.rs:446](src/worker.rs#L446)),
`send_record` and `send_batch` in place of `BASE_UDP_PAYLOAD`. Document it as
an operator assertion that the path carries 1500-byte outer packets, to be
replaced by DPLPMTUD in stage 3; an `EMSGSIZE` still disables the batch as
today. The receiver already accepts any segment size up to `MAX_UDP_PAYLOAD`.
Buys: about 15% fewer outer datagrams and 20-byte record prefixes per frame.
Check: `tests/pack.rs` round trips at 1452; `tests/config.rs` rejects 1231
and 1453.

**A6. Size the pools for the rate, and split them by purpose.**
Replace the single 64 KiB buffer pool with two: transmit frame buffers of
`MAX_FRAME_LEN + 1` bytes, 1024 per worker (9 MiB), and receive buffers of
64 KiB, 32 per worker (2 MiB). Update `Worker::storage_bytes`
([src/worker.rs:522-576](src/worker.rs#L522-L576)) and `frame_quota`
([src/worker.rs:293-300](src/worker.rs#L293-L300)); 1024 frames per worker
times four workers stays under the 8192-frame and 64 MiB per-direction
ceilings at 9022 bytes. Raise `RECEIVE_BUFFERS` to 32 and the receive budget
([src/worker.rs:284-288](src/worker.rs#L284-L288)) to 256 datagrams per turn
once A7 gives the slots to hold them.
Buys: a worker can absorb a 2.5 ms turn instead of 0.3 ms before the TAP ring
or the socket overflows.
Check: `origin_reservations_bound_every_session_direction`,
`receive_and_transmit_cannot_exhaust_each_others_buffers`, the memory-limit
test in [src/runtime.rs:538-552](src/runtime.rs#L538-L552).

**A7. Share receive frame slots across a worker's lanes.**
Today each of the 64 lane tables owns 64 slots of 9022 bytes, so a worker's
one busy lane gets 155 µs of buffering while 63 idle tables hold 36 MiB. Move
the slot storage to one pool per session per worker of
`64 MiB / queue_count / MAX_FRAME_LEN` slots (about 1750 for four workers) and
let `reassembly::Lane` borrow slot indices from it. `FRAME_CAPACITY` becomes
the per-lane cap on borrowed slots, not a storage size; keep it at 64 for the
pressure semantics or raise it to the pool size. The compile-time asserts at
[src/worker.rs:290-291](src/worker.rs#L290-L291) move to the pool constructor.
Buys: the admission-pressure path stops firing at the gate rate.
Check: `tests/reassembly.rs` pressure and expiry cases, `receive_bursts_budget_expanded_datagrams_records_and_bytes`,
`blocked_tap_keeps_tails_but_never_acks_an_unretained_head`.

**A8. Deeper kernel queues.**
Set `SO_RCVBUFFORCE` to 8 MiB on receive sockets in `bind_inner`
([src/socket.rs:106-119](src/socket.rs#L106-L119)); the daemon already holds
`CAP_NET_ADMIN` for the TAP, and the option bypasses `rmem_max` without a host
sysctl. Fall back to `SO_RCVBUF` on `EPERM` and count the failure. In the
bench script, run `ip link set uq-$side txqueuelen 10000` inside each
namespace before `up` ([benches/tap_transport.sh:79-82](benches/tap_transport.sh#L79-L82));
that is administrative state and stays outside the daemon.
Check: `socket_options_enforce_wire_and_descriptor_policy` reads the option back.

**A9. One decode pass and one copy per piece.**
Have `Datagram::decode` fill a fixed `[RecordDesc; 64]` (type, offset, length,
and the DATA fields) and make `records()` iterate that array; `admit` then
needs no second decode. In `Frame::piece`
([src/reassembly.rs:184-209](src/reassembly.rs#L184-L209)) test the covered
words first and, when the whole range is fresh, do one `copy_from_slice` and
one mask update per word.
Buys: a few hundred nanoseconds per datagram at two million per second.
Check: `tests/wire.rs`, `tests/reassembly.rs` overlap cases.

### Tier B: fewer kernel entries per inner frame

Run the probes in B0 before writing any of this. They decide between B2 and
B3, and whether B1 helps on this kernel.

**B0. Cost probes (one afternoon, throwaway).**
Write a small ignored container test or `benches/` binary that measures, on
this kernel, the per-frame cost of: plain TAP `write` of a 1514-byte frame
into a namespace with a listening UDP socket; the same frames written as one
`IFF_VNET_HDR` super-frame with `gso_type = VIRTIO_NET_HDR_GSO_UDP_L4` (USO)
via `writev`; plain TAP `read`; TAP reads submitted through `io_uring`
(`io_uring_setup` through `libc::syscall`, no crate) in batches of 64. Record
the numbers in BENCH.md. Also record whether `io_uring_setup` is permitted in
the test container; Docker's default seccomp profile denies it, Incus
unprivileged containers normally allow it.

**B1. Write coalesced USO super-frames to the TAP (lossless).**
Open TAP queues with `IFF_VNET_HDR` ([src/tap.rs:77-83](src/tap.rs#L77-L83));
reads then carry a 10-byte `virtio_net_hdr` prefix to skip (with
`TUNSETOFFLOAD` still zero it is always `gso_type = NONE`), and writes may
carry one. In `flush_rx` ([src/worker.rs:2000-2043](src/worker.rs#L2000-L2043)),
before writing, group the lane's ready frames by inner flow and emit each run
of coalescible frames as one `writev`: a 10-byte header with
`flags = NEEDS_CSUM`, `gso_type = UDP_L4`, `hdr_len = 42`, `gso_size =` the UDP
payload length, `csum_start = 34`, `csum_offset = 6`; a 42-byte copy of the
first frame's headers with IPv4 total length and UDP length set to the run's
totals and the UDP checksum field holding the pseudo-header sum; then one iov
per frame payload borrowed from its reassembly slot. The kernel segments the
run back into the original frames.
Coalesce only when the result is byte-identical to writing the frames one by
one: same 14-byte Ethernet header; IPv4 with no options, same TOS, TTL,
flags and addresses, DF set and offset zero, IP IDs exactly consecutive
modulo 65536 (connected UDP sockets with DF produce this; anything else falls
back to single writes); same UDP ports; every payload the same length except
the last, which may be shorter; each frame's UDP checksum present and
verified correct (one pass over 1472 bytes, about 0.1 µs). IPv6 needs the
same with flow label and hop limit equal and no ID rule. Cap a run at 64 KiB
and `UIO_MAXIOV`. Reorder only across flows inside one flush batch; per-flow
order is unchanged, which the README already allows.
Buys: TAP write cost per frame drops from about 2.5 µs to under 1 µs when runs
average 16 frames, roughly 2.5 to 3 core-seconds at the gate rate.
Check: a container test that writes 1, 2, 43 and 44 coalescible frames plus
frames differing in each field above, captures on the far side of a veth, and
compares bytes with the single-write path; the storm test's corruption and
duplicate checks; a probe for kernels that reject `GSO_UDP_L4` from a TAP
(fall back to single writes on `EINVAL`).

**B2. Batch TAP reads with io_uring** (if B0 shows syscall entry is a large
share of the 1.4 µs; on this Skylake with mitigations it is probably 0.3 to
0.5 µs).
A ring per worker, `IORING_OP_READ` with the TAP fd and 64 outstanding
transmit frame buffers, completions harvested in the turn, one
`io_uring_enter` per batch. Keep the plain `read` path as the fallback when
`io_uring_setup` fails. Pure `libc`, about 400 lines.
Buys: 0.3 to 0.5 µs per read, 0.5 to 0.8 core-seconds at the gate rate.
Risk: a seccomp profile that denies io_uring; a TAP without `FMODE_NOWAIT`
would make the kernel punt reads to its worker pool, which B0 must rule out.

**B3. Accept super-frames from the host TAP and segment them in userspace.**
Enable `TUN_F_CSUM | TUN_F_TSO4 | TUN_F_TSO6 | TUN_F_USO4 | TUN_F_USO6` in
`TUNSETOFFLOAD` ([src/tap.rs:85](src/tap.rs#L85)). The host then hands the
daemon one read per TCP send or per `UDP_SEGMENT` send, up to 64 KiB, with a
partial checksum. Before packing, split the super-frame into wire frames
(headers rewritten per segment, sequence and IP ID advanced, the transport
checksum computed from the partial sum) into transmit frame buffers. Plain
frames also arrive with `NEEDS_CSUM` once `TUN_F_CSUM` is on, so compute
their checksum too.
Buys: for TCP and USO senders, reads fall by up to 43 times. For iperf3 UDP
without `UDP_SEGMENT` it buys nothing; the Rust generator in C2 should use
`UDP_SEGMENT` so the gate run exercises this path, and the report must say so.
Check: a container test that sends a 64 KiB TCP write and a `UDP_SEGMENT`
burst through the TAP and compares every captured frame with a run of the
same traffic over a veth.

**B4. Later: carry the super-frame on the wire.**
The design's negotiated inner-GSO extension (section 10) would remove the
userspace segmentation of B3 and the coalescing of B1 by transporting the
10-byte header and a frame up to 65535 bytes. It needs a feature bit, a larger
`max_frame_len`, and seven times the reservation per frame slot. Do not start
it until B1 and B3 are measured; it is a stage 5 item in ROADMAP.md.

**Not recommended:** `vhost-net` would move the TAP copies to kernel threads,
but requires `/dev/vhost-net` access in every test container; `IFF_NAPI` adds GRO on TAP writes but UDP sockets
without `UDP_GRO` never coalesce, so it does nothing for the gate traffic;
an `AF_PACKET` ring on the TAP netdev reads outgoing frames without syscalls
but leaves every frame to be dropped in the TAP ring, which operators would
see as 100% TX drops.

### Tier C: a harness that can show 20 Gbit/s

The current bench cannot offer or absorb the gate rate, so even a passing
transport would not be observed.

**C1. Core placement.** The host has 80 logical CPUs on two sockets. Put both
daemons, both senders and both receivers on socket 0 physical cores without
hyperthread siblings (use `lscpu -p` to pick them). Give each daemon 8 cores
(`queue_count = 8`, `tx_lanes = 8`) for the gate run and report CPU; the gate
fixes throughput, not the core count. Keep a 4-core row too so the budget
table in section 1 can be checked against reality.

**C2. Generator.** iperf3 UDP with 16 flows offered 13 Gbit/s on 4 cores at
2.8 µs per packet; 20 Gbit/s needs 6 to 8 generator cores per side. Write the
small Rust generator ROADMAP.md stage 2 allows: connected UDP sockets, DF set,
`UDP_SEGMENT` enabled, fixed source ports, paced by a token bucket per flow,
with a receiver that enables `UDP_GRO` and counts bytes, sequence gaps and
duplicates. Report generator CPU separately, as now. Keep the iperf3 profile
as a second row so the two agree.

**C3. Receiver.** iperf3 servers cost 2.5 µs per packet, so 1.65 million
packets per second needs 4 to 5 cores per side; the C2 receiver with
`UDP_GRO` needs fewer. Never put receivers on socket 1 again.

**C4. Pass definition.** Add to BENCH.md before the run: five runs of 60 s at
inner MTU 1500, 16 flows per direction, both directions simultaneously; pass
when every run delivers at least 20 Gbit/s of inner UDP payload per direction
at a paced offered rate of 21 Gbit/s with under 1% missing, and the unlimited
point delivers no less than the paced point. Report daemon CPU, generator CPU,
per-core softirq, socket overflow, TAP TX drops and inner receive errors per
run.

## 4. Order of work and expected position after each step

| Step | Expected per-direction goodput on 4 cores | Why |
| --- | ---: | --- |
| Today | 2.5 to 3.4 Gbit/s | handoffs, drops, per-turn syscalls |
| After A1 to A4 | 5 to 7 Gbit/s | handoffs gone, cache-hot copies, turn cost down |
| After A5 to A9 | 7 to 10 Gbit/s | fewer datagrams, no jitter drops |
| After B1 | 11 to 14 Gbit/s | TAP write cost cut by 2 to 3 times |
| After B2 or B3 with a USO generator | 14 to 20 Gbit/s | TAP read cost cut |
| Same code on 8 cores (C1) | above 20 Gbit/s | per-worker pipelines scale linearly once nothing crosses workers |

These are projections from the section 1 budget. Replace each row with a
measured number as the step lands; if a step lands below its row, stop and
profile before continuing.

## 5. What this audit did not do

- No code was changed and nothing was re-measured; every number comes from
  BENCH.md, the overload JSON, and the code as read.
- The per-operation costs are derived from a 256-turn sampling profile that
  includes startup and shutdown; treat them as within a factor of 1.5.
- Kernel behavior claimed for `GSO_UDP_L4` writes from a TAP, reuseport
  classic BPF seeing the UDP payload, and io_uring on a TAP descriptor was
  recalled from kernel source knowledge, not verified on this 6.8 kernel.
  B0 exists to verify all three before anything depends on them.
- The right-side control worker's 81 thousand turns per second was noted but
  not explained; add a per-turn wake-reason counter when doing A4.
