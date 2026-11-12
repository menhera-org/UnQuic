# UnQuic implementation roadmap

This file tells a coding agent what to build next, in what order, and how to
prove each step works. [initial-design.md](initial-design.md) is the
specification; this file never overrides it. Where this file names a section,
read that section before writing code. Keep the "Current status" paragraph in
section 15 of the design and the "Implementation status" section of
[README.md](README.md) true after every step.

## November 12 checkpoint

Stage 4 is done. An OVS system-port test now runs on Linux 6.8 and 7.0, with
Open vSwitch userspace unpacked into a private tree rather than installed, and
the encoders and batch planner use checked access everywhere Clippy's
`indexing_slicing` lint looked. The first no-regression check of that change
failed; a micro-benchmark traced it to an iterator chain and a slice
comparison, both rewritten, after which the rerun passed on loopback. On the
decision workload one point failed by 0.03 points of share. An A/A run of
identical binaries failed the same criterion at one point by a full point, so
side-effect bars now allow one failing direction-point
([BENCH.md](BENCH.md#checked-indexing-in-the-encoders-and-batch-planner-november-12)).

A/A runs of identical binaries now calibrate the bars. On the loopback check a
copy run second wins six of eight pairs with a +1.4% median, and single
directions swing by up to 18%, so throughput at overload cannot show a gain
of a few percent. Efficiency changes are judged instead by CPU per delivered
Gbit/s at an offer-limited load, in pairs that alternate order. On that
metric two changes landed: arming the worker timer only before blocking
(−2.0% at the loopback check's 4G point) and sweeping only occupied receive
slots when collecting ACKs (−4.1% at 0.5G on the decision workload).
Masking the TAP flush and deadline scans missed its bar.

`daemon.loss_threshold_permille` (default 20, draft 07's 2%) landed: at 100
one flow at 5% loss rises from under 10 Mbit/s to about 0.45 Gbit/s with a
competing TCP flow's share unchanged, but it costs 3% at 1% loss, so the
default stays. A trace places the remaining 5% limit in BBR's long-term
bound, which makes that row a job for FEC. FEC only lifts it if packets it
recovers are acknowledged as received, hiding their loss from congestion
control, congestion losses included; whether that is acceptable is a
policy decision still open.

A 9000-byte outer path nearly halves the per-bit cost. On the loopback check
at an 8952-byte payload limit the 21G total rises 70–126% (24–29 Gbit/s
over both directions; one direction reaches 18.4 Gbit/s) at 34–45% less CPU
per bit, and eight workers do as well as sixteen
([BENCH.md](BENCH.md#a-9000-byte-outer-path-on-the-loopback-check-november-12)).
No configuration gives 20 each way, and no thread saturates; reordering and
socket overflows rise instead. The 1500-byte NGN path cannot use this.

The local 20 Gbit/s gate remains open: with sixteen workers per daemon the
loopback check reaches 7.11 / 7.03 Gbit/s with no thread above 75%, limited
by receive-socket overflow under the container's 416 KiB receive buffer, and
per-bit cost puts both daemons near the host's core count before the gate.
Keeping the single QinQ reader off its own lanes cost 45% more CPU and was
dropped.

The small-frame rows, rerun at the current build, fell behind the
pre-recovery build: untagged 64-byte frames over sixteen flows deliver about
3.3 M frames/s per direction unlimited with eight workers (5.87 / 5.74 M
before) and 4.2–4.5 M with sixteen. Two costs add up. Each frame takes about
45% more CPU than before recovery and BBR, and tun's queue hash leaves one
to one and a half of eight workers per daemon without transmit work, while
the busiest readers run at 96–98%. Balanced lanes do not change this:
unfragmented UDP keeps a lane owned by the worker that read it, and
`balance_flows` never sees those frames
([BENCH.md](BENCH.md#small-frame-rows-at-the-current-build-november-12)).
Dropping that locality spreads paced traffic (+15–19%) but loses 18–20%
under overload, so it was rejected.

The per-frame cost is on the transmit side. Profiling both builds puts HEAD
at about 3,250 busy cycles per frame against 2,540, with the receive side
level and the transmit side about 620 cycles higher: `sendmmsg`, releasing
ACKed frames, staging ACKs and reserving pieces. Four candidates cut 0–3%
each at overloaded points, where none cleared its bar. The cheapest, an
expiry heap with inline keys and branchless sifting, then landed on the
project's efficiency method: −4.2% CPU per frame at an offer-limited
small-frame point, eight of eight pairs, against an A/A median of +0.2%
([BENCH.md](BENCH.md#the-expiry-heap-at-an-offer-limited-point-landed)).
No single large target remains in the per-frame path.

At light load each lane sends one frame at a time, so a 1514-byte frame's
130-byte tail takes a datagram of its own. `daemon.tx_hold_us`, off by
default, lets an idle lane wait for a second datagram's worth of frames; at
200 µs it cuts CPU per delivered Gbit/s 5.5% at 0.5G on the decision
workload. Its first version hid idle moments from application-limited
marking and caused admission drops; the fixed version appeared to lose
0.19% of one direction in one run, which later turned out to be host stalls
and ProbeRTT (below). It stays off until decision 4
([BENCH.md](BENCH.md#holding-idle-lanes-for-a-second-datagram-november-12)).

`udp_load` now measures one-way delay. It found frames reaching receivers
seconds late under overload: a worker owning a busy lane stopped reading its
own TAP while other readers' frames filled its input queue. Gating reads only
on the worker's own inputs cut 5G p99.9 from 0.1–0.4 s to 30–66 ms
([BENCH.md](BENCH.md#frames-seconds-old-under-overload-november-12)). Most
remaining overload delay is the kernel's TAP ring; README recommends 64
entries.

Light-load loss on the decision workload has two sources outside the hold.
The test host stalls system-wide for 10–50 ms every few seconds, and at the
20 ms frame-lifetime floor each stall expires frames; that is most of the
control's loss. BBR's ProbeRTT window, half of max_bw × min_rtt, can also
fall below what a 0.5 Gbit/s offer keeps in flight, because those bytes stay
out for min_rtt plus up to 1 ms of ACK delay.
`daemon.probe_rtt_ack_delay` adds that delay to the ProbeRTT window. It kept
every ProbeRTT episode off its window but first missed its loss bar because
a host stall fell inside one episode; measured again with stalls counted
out, it halved the remaining light-load loss, cost nothing elsewhere and is
now on by default
([BENCH.md](BENCH.md#probertts-window-over-min_rtt-plus-the-ack-delay-now-on-by-default)).
With both sources counted out, `tx_hold_us = 200` loses no more than the
control ([BENCH.md](BENCH.md#the-holds-loss-without-host-stalls-and-probertt)),
so the hold's only cost is latency. `udp_load` now bins frames by send time
so `sweep_report` can leave stalls out without a trace build; a release
repeat confirms the hold at 6% less CPU with no extra loss
([BENCH.md](BENCH.md#leaving-out-stalls-without-a-trace-build)).

TCP through the tunnel turned out to be limited by the harness and then by
the host. Both iperf3 directions had used the same port pair, so tun's
symmetric hash moved mirrored flows between readers and reordered whole TSO
blocks; with distinct ports SACK reordering falls from 10,000–36,000 events
per side to under 300 at the same total of about 9.2 Gbit/s. About 1% of
segments are still retransmitted under unlimited load. What remains are
receive-socket overflows: with UDP GRO each one drops a coalesced batch,
and the container's 416 KiB buffer holds only a few
([BENCH.md](BENCH.md#tap-queue-length-and-overload-delay)). Turning GRO off
made it worse. A cookie that aligns each lane's receive owner with its
transmit owner lifts TCP 17–19% but costs the decision workload 4–15%, so
placement stays
([BENCH.md](BENCH.md#lane-owner-alignment-for-tcp-and-qinq-november-12-exploratory)).
A candidate for unprivileged hosts is a second receive socket per worker,
doubling the buffering a worker gets without `CAP_NET_ADMIN`.

A fresh decision-workload baseline
([BENCH.md](BENCH.md#decision-workload-baseline-november-12-evening)) shows
the plateau gate failing for few flows: four flows delivered 1.31 Gbit/s L→R
at 2G offered but 0.92 at 5G. A trace ruled out CPU and BBR: the two lane
owners also read TAP queues, and reading until the receive reserve left them
no exchange buffers to accept frames other readers handed them, so they
refused handoffs while idle
([BENCH.md](BENCH.md#why-four-flows-lose-lr-delivery-past-2g)). TAP reads
now stop 16 buffers earlier when other workers transmit for the session.
That restores the plateau: four flows at 5G deliver 1.21–1.23 / 2.02–2.05
Gbit/s instead of 0.92–0.93 / 1.36–1.46 (+40% in total), for 7% of 2G
L→R, with sixteen flows, the loopback check and light-load CPU unchanged
([BENCH.md](BENCH.md#owners-refusing-each-other-a-read-margin-landed)).
Across the whole decision matrix every 5G total now matches or exceeds its
2G total, at 6–19% less CPU per bit; lossy four-flow L→R still loses 10%
from 2G to 5G, held by the one owner hashing gives two flows, which runs at
95–96% of a core. Overloaded p99 delay rose with the extra delivery; a
64-entry TAP ring removes most of it (decision 2).
Balanced lanes rescue the same point differently (decision 5 below).

Decisions only the user can make, each blocking a remaining goal:

1. FEC for the 5% loss row only helps if packets it recovers are
   acknowledged as received, which hides their loss from congestion control,
   congestion losses included. Is that acceptable, and if so, under what
   limit?
2. Should the daemon set a short transmit queue (for example 64 entries) on
   the TAPs it creates, or leave it to the operator as now? On the decision
   workload at 5G, 64 entries instead of 500 cut p99 delay from 25–42 ms to
   11–14 ms with delivery unchanged
   ([BENCH.md](BENCH.md#why-four-flows-lose-lr-delivery-past-2g)).
3. The 20 Gbit/s gate is limited locally by the container's 416 KiB receive
   buffers, which this host cannot raise and a root deployment does not
   have. Is a measurement on a host with more CPUs and larger buffers
   available, or should the gate be judged on the 9000-byte path?
4. Should `daemon.tx_hold_us` default to 200? It saves 5–6% of CPU per
   delivered bit at 0.5 Gbit/s and about 3% at 2G, leaves 5G unchanged and
   no longer shows a loss of its own, but it
   adds about 33 µs to the median one-way delay and 0.2 ms at p99 for light
   traffic.
5. Should `daemon.balance_flows` be on by default? On the decision workload
   it adds about 20% for four flows even with the read margin (5G total
   3.91–3.95 instead of 3.25–3.29 Gbit/s) and lifts sixteen flows at 2G by
   10–15%, but it still costs the loopback overload check about 12% at 21G
   and more CPU per bit (rechecked on `3178088`)
   ([BENCH.md](BENCH.md#balanced-lanes-on-the-decision-workload-revisited)).
6. Carried over: the real-path test needs sudo through the endpoint account,
   which conflicts with this host's no-networking-changes rule; the 256 MiB
   `memory_limit` default (which refuses sixteen workers) remains
   undecided.

## November 11 checkpoint

The loopback overload check was window-limited, not CPU-limited. BBR sized
its window from a min_rtt of tens of microseconds while the feedback loop,
with each lane holding ACKs up to 1 ms and loaded workers batching, took
about a millisecond. Hash lanes hid this by leaving workers idle, which kept
the group application-limited and BBR in Startup with an unbounded window.
Windows now use at least the receiver's 1 ms ACK delay as RTT. The weaker
loopback direction rose from 2.55 to 5.49 Gbit/s and the busiest thread to
98% ([BENCH.md](BENCH.md#sizing-windows-from-at-least-one-ack-delay-november-11)).
That thread reads every QinQ flow because tun's flow dissector, by our
reading, misparses locally routed double-tagged frames and hashes them all
alike. That reader starves its own direction's senders and also drops the
other direction's packets at its receive socket. Letting tun spread the
input (one tag) made the check slower, at 23% more CPU per bit, and more
receive buffers only moved capacity between the directions.

A `profile` feature counts worker-loop cycles per phase because perf cannot
run on either test host. On the remote host one flow at 7.5 ms delivers about
1.6 Gbit/s; BBR paces it, and in ProbeBW_UP inflight grows without delivery,
so a limit downstream of the sender caps it there
([BENCH.md](BENCH.md#what-limits-one-flow-at-75-ms-november-11)). Striping one
flow across lanes raised its medians by 24–36% but doubled reordering and
missed its bar. Balanced lanes cleared their decision-workload bar and became
the default, then cut the loopback check by up to 77% (the same window
limit) and went back off; lane-placement changes must clear both workloads.
Two side-effect bars this day used a criterion that fails for any no-op 99.5%
of the time; BENCH.md records a valid replacement.

Stage 4 is done except OVS (absent from the test images) and converting
locally bounded indexing in encoders and the batch planner. Today added
fuzzing of every untrusted-input parser and of the workers' packet paths,
which found an ACK burst that could stop the runtime, and seccomp fault
injection of worker and thread-start failures.

## November 10 checkpoint

Three performance changes are adopted: coalescing waiting ACKs per lane with
32 transfer boxes per worker, draft 07's application-limited bubbles from a
per-group unsent-data mask
([BENCH.md](BENCH.md#application-limited-bubbles-from-unsent-data-november-10)),
and draft 07's three-quantum offload budget, which lifts the loopback check's
window-limited direction by up to 71%. The decision workload is CPU-bound: at
sixteen flows every worker runs at 79–99% of a core, so moving work between
workers no longer helps, and the ACK-lane and whole-plan candidates were
rejected. Fixed flow-hash keys also leave workers idle when only a few flows
run.

Stage 3's remaining work items are done: a netem loss-and-reordering container
check and the loss sweep, which shows draft 07 collapsing at 5% loss. Stage 4
gained double-fork readiness, the systemd unit template, VRF and Ethernet
composition tests, and the removal of every explicit panic call from library
code. TAP persistence, descriptor handover across re-exec (including a failed
exec) and the overload and memory-ceiling stress tests followed. A V03 test
that cuts all feedback found that recovery after an outage waited on
unbounded PTO backoff; backoff is now capped at one second and reset when a
connection commits. Next, in the order below: stage 4's implicit panics; then
stage 5's candidates, led by single-flow striping and lane
placement, which bound both the single-flow target and the local gate.

On the single-flow target, draft 07's short-term bounds were the first limit.
After any loss round outside probing, `bw_shortterm` becomes the larger of the
round's delivery rate and 70% of its old value, so under steady 1% random loss
Cruise ratchets it down to about half of `max_bw`. The `tolerate_random_loss`
switch, recorded in design section 9 and off by default, cuts only after a
round above 2% loss and raises every target direction by 8–81%
([BENCH.md](BENCH.md#tolerating-random-loss-below-bbrs-loss-threshold-november-10)).
Its coexistence with competing flows is not yet measured.

The single flow then stops near 1 Gbit/s per direction because one transmit
worker carries it: each 1522-byte frame takes two 1452-byte datagrams, and
that worker reaches 95–97% of a core. Five gigabits for one flow needs stage
5's single-flow striping across lanes, and a generator that can offer it; one
generator process offers only about 2.5 Gbit/s.

## November 9 checkpoint

Performance decisions now use a fixed decision workload: routed 4.5 ms RTT,
bridged individual QinQ frames, four workers, 4 and 16 flows, 0.5/2/5 Gbit/s
per direction, with no loss and 0.1% loss. The generator can now offer those
rates (four pinned processes and 1 ns timer slack); earlier bridged 5G points
offered only 2.3–2.8 Gbit/s. Five production repeats show light load is
bimodal: most runs miss under 1%, but one invocation in five collapses one
direction to 0.16–0.37 Gbit/s through never-submitted expiry. Matched
comparisons need three to five interleaved repeats per build;
[BENCH.md](BENCH.md#decision-workload-and-its-noise-november-9) has the
numbers.

A diagnostics-only `trace` feature attributes each never-submitted frame to
what its sender was waiting for. Light-load collapses and most 2G expiry come
from one lane that cannot reserve sent-history numbers: an unacknowledged tail
pins the history floor, retirement waits for pressure and idle ACK work, and
refused attempts consume packet numbers. Pacing, window, burst and closed
issuance each explain under 5% of expired lifetimes.
[BENCH.md](BENCH.md#light-load-expiry-traced-to-a-stalled-lane-november-9)
has the evidence.

The fix retires handle-less history entries as each ACK finishes, requests
reported retirement once loss handles pin half the history, checks numbers
before allocating them, and keeps the last history numbers for probes and
control. In matched repeats it raises most 2G and 5G directions by 22–116%
and removes the observed collapses; four-flow L→R at 5G falls 14% because the
left daemon's busiest worker reaches 96% CPU.
[BENCH.md](BENCH.md#sent-history-headroom-removes-the-lane-stall-november-9)
records the comparison.

New `tx_admission_drops` and `handoff_drops` subsets of `queue_drops` show
that high-load input loss is admission, not handoff overflow. A trace pins it
on one worker: the receiver of the peer's lane 1, which carries every ACK
packet, holds most of the transmit buffers as queued sources and unreturned
credits, so the other readers refuse input.

Audit step 3's diagnostic builds settle the larger question. Disabling the
shared sender recovers 6.75 / 15.66 Gbit/s on the loopback check, close to the
pre-integration 7.76 / 16.70, and roughly doubles delayed-path delivery until
four cores saturate. Fixing BBR's rate and window gains only 5–16% on the
delayed path. The shared-owner protocol is the limit; tuning inside it will
not reach the gates.
[BENCH.md](BENCH.md#the-shared-owner-sender-is-the-throughput-limit-november-9)
has the data.

Design section 9 now specifies the replacement: published controller state
read under a sequence check, one atomic inflight counter and one atomic
pacing clock per group, send records kept by each lane's owner, one report per
ACK to the controller, and ACKs sent from the receive owner under a per-worker
control ceiling.

The runtime now uses it. `src/pacing/budget.rs` holds the published state,
the atomic inflight and pacing claims, per-worker send records and the
controller-side sampler. A randomized oracle compares that sampler with the
chronological one across one to four lanes, partial batches, late ACKs after
loss, idle restarts and application-limited marks, and the pinned draft-07
delivery vectors replay identically through it. Grants, the shared sampling
slab and ordered commits are gone. On the decision workload fourteen of
sixteen 2G and 5G directions rise beyond production's range, by 16–129%
([BENCH.md](BENCH.md#replacing-grants-with-a-shared-send-budget-november-9)).
The busiest worker now saturates on ACK handling. The zero-delay loopback
overload check regresses by about a fifth without saturating any core. A
diagnostic traces most of that to partial claims fragmenting sends. Claiming
whole plans recovers the reverse loopback direction with less CPU, but loses
18% in one decision-workload direction and was rejected. A faster window
retry does not help. No performance gate has passed.

### Next, in order

Stages 2 through 5 close only when their gates pass. Work through this list;
each item is one commit with its tests, both kernel suites, and a BENCH.md
entry when it changes performance. Items 1 to 3 finish the
[November 9 throughput audit](#november-9-throughput-audit) below, whose
rules apply to all of them.

1. **Replace the grant protocol in the runtime.** Done (see above). The
   earlier split into "charge atomically" and "move records into lane
   histories" could not land as two commits: without per-batch grants the
   ordered sampler has no epoch to wait for.
2. **Send ACKs from receive owners.** Measured and rejected. Steering each
   ACK to the lane whose peer owner needs it, and sending it on one of the
   receive owner's own lanes, both cut ACK-buffer drops but moved delivery
   between directions. At sixteen flows every worker already runs at 79–99%
   of a core, so moving ACK work around cannot help, and a pool of four
   reports per worker changed nothing on the single-flow target
   ([BENCH.md](BENCH.md#whole-plan-claims-and-acks-routed-to-their-peer-owner-november-10)).
3. **Size capacities** (audit step 5). The ACK-box pool is done: 32 boxes per
   worker, with a newer ACK replacing one still waiting for the same lane,
   remove ACK-buffer drops and raise ten of sixteen decision directions by
   5–37% ([BENCH.md](BENCH.md#more-ack-boxes-and-coalescing)). Remaining:
   per-session retention credit that busy workers can borrow and shared receive
   slots per worker, each sized from the delayed and lossy rows, with memory
   cost reported. Target one flow at 5 Gbit/s at 7.5 ms with 0.5% and 1% loss
   on four workers, within two to four cores per daemon. That target is out of
   reach in the current design: with the `tolerate_random_loss` switch one flow
   reaches about 1 Gbit/s per direction, where its single transmit worker
   saturates, and one generator process offers only about 2.5 Gbit/s. It
   returns with stage 5's single-flow striping.
4. **Finish stage 3.** General application-limited detection is done: a
   per-group unsent-data mask starts draft 07's bubble, which removed the
   warm-up collapse on the lossy target
   ([BENCH.md](BENCH.md#application-limited-bubbles-from-unsent-data-november-10)).
   The container check with `tc netem` loss and reordering is done:
   `benches/netem_check.sh` passes with no duplicate delivery and a flat
   peak of about 137 MiB per daemon. The loss sweep is recorded
   ([BENCH.md](BENCH.md#stage-3-loss-sweep-november-10)): 0.1% costs nothing
   measurable and 1% halves one 2G direction under draft 07, but 5% loss,
   independent or in bursts, collapses delivery below 11% of even a 0.5G offer
   because draft 07 treats sustained loss above 2% as congestion. Stage 3's
   work items are done; its 5 Gbit/s lossy-path gate stays open and returns in
   stage 5, where the 5% rows need forward error correction or a
   loss-agnostic response.
5. **Close the stage 2 gate:** 20 Gbit/s each direction locally at inner MTU
   1500, plus the small-frame burst rows. On November 10 the loopback overload
   check delivers 5.77 / 2.81 Gbit/s at 21 Gbit/s requested, after draft 07's
   three-quantum offload budget lifted the window-limited R→L sender by 71%
   ([BENCH.md](BENCH.md#three-quantum-offload-budget-november-10)). On
   November 11 the window RTT floor (design section 9) lifted the weaker
   direction again, to 5.49 / 5.70 Gbit/s
   ([BENCH.md](BENCH.md#sizing-windows-from-at-least-one-ack-delay-november-11)):
   the busiest threads now run at 98% and 83%. The 98% thread is the one
   worker that reads every QinQ flow, and it also receives the R→L lanes it
   owns. A trace shows L→R senders idling for want of its input, while its
   receive socket overflows and BBR cuts R→L after the losses. Letting tun
   spread reads (one tag, distinct ports per direction) lowers delivery and
   costs 23% more CPU per bit
   ([BENCH.md](BENCH.md#whether-the-single-qinq-reader-limits-the-loopback-check-november-11)).
   Thirty-two receive buffers instead of eight cut its overflows by 81% and
   raise the total 11%, but move capacity from L→R (−14%) to R→L (+37%) and
   were rejected
   ([BENCH.md](BENCH.md#receive-buffers-for-a-busy-reader-november-11)).
   Next: keep receive work off the worker that reads the TAP, or split TAP
   reading inside the daemon, so the two directions stop sharing one core.
   Placement alone cannot close the gate: at about 0.74 of a core per Gbit/s
   (both ends together) sixteen daemon CPUs top out near 10.8 Gbit/s each
   way, so the per-bit cost must fall by almost half. Sending batches takes
   about 25% of busy cycles (`sendmmsg` alone 11%), TAP writes 15%, receive
   processing 11%, and applying ACK reports to frames (`stage_recovery`)
   7.6%. On the decision workload sending takes 30%, TAP reads and writes
   15% each and `stage_recovery` 3%, so the per-frame I/O path is the general
   target and the journal's per-packet bookkeeping a loopback-rate one
   ([BENCH.md](BENCH.md#receive-buffers-for-a-busy-reader-november-11)).
   A 9000-byte outer path does the halving where the path allows it: the
   loopback check reaches 24–29 Gbit/s in total at 46–61% of a core per
   Gbit/s, up to 18.4 Gbit/s in one direction, with no thread saturated
   ([BENCH.md](BENCH.md#a-9000-byte-outer-path-on-the-loopback-check-november-12)).
   Next on that path: find what holds the weaker direction back when CPU
   does not, starting from the reordering and socket overflows.
   Waking only sleeping workers removed most of the 5–6% spent writing
   eventfds and landed: +2.5% on the loopback total, +0.9% median on the
   decision workload
   ([BENCH.md](BENCH.md#waking-only-sleeping-workers-november-11)).
   Arming the worker timer only before blocking cuts `timerfd_settime`
   calls by about half. It first won six of eight paired loopback runs on
   throughput, which an A/A run shows is noise; retested on CPU per
   delivered Gbit/s at the offer-limited 4G point, it used less in seven of
   eight alternating pairs (median −2.0%) and landed on November 12
   ([BENCH.md](BENCH.md#arming-the-timer-only-before-blocking-november-11)). Sixteen workers per daemon raise the loopback total 36–38%
   with no thread saturated; there the limit is receive-socket overflow,
   which the container's 416 KiB receive buffer (no `CAP_NET_ADMIN` in the
   initial namespace) makes worse than a root deployment's 8 MiB
   ([BENCH.md](BENCH.md#sixteen-workers-per-daemon-november-11-exploratory)).
   The small-frame rows were rerun on November 12. They show the same two
   limits in another form: per-frame cost about 45% above the pre-recovery
   build, and transmit work tied to tun's uneven queue hash, which leaves
   some workers idle while the busiest readers saturate
   ([BENCH.md](BENCH.md#small-frame-rows-at-the-current-build-november-12)).
   UDP lane locality keeps transmit work on the reading worker. Dropping
   it in an exploratory patch, with balanced lanes, delivers 15–19% more
   at the paced point but 18–20% less under overload, at 21–26% more CPU
   per frame, so it was rejected
   ([BENCH.md](BENCH.md#without-udp-lane-locality-exploratory-rejected)).
   A first try spread the frames of reads that filled a whole batch, but
   under this load every read returns one UDP super-frame with the read
   limit at one, so it never fired
   ([BENCH.md](BENCH.md#spreading-only-after-a-full-tap-read-exploratory-never-fired)).
   A second try keyed on pending TAP inputs cannot fire either, because
   no read starts while inputs are pending. A real test needs a signal
   from outside the read path, such as how often reads find data at once
   or the reader's busy share, a counter showing how often it fires, and
   no reordering for a flow that changes lanes. Expect little from it:
   both measured points already overload the readers (20–28% missing at
   40M), so such a signal would fire at both, and the no-locality run
   already shows that regime losing 18–20% unlimited. The per-frame cost
   (about 45% above the pre-recovery build) was the better target, and it
   turned out to be spread thin. A cost history shows 8% arriving before
   the sender and the rest with sender and recovery, where intermediate
   builds are paced too low to compare. Phase profiles of both builds put
   the growth in transmit bookkeeping: releasing ACKed frames (about 280
   cycles per frame), staging ACKs (180) and reserving and settling pieces
   (215). Four candidates were tested against written bars: a frame
   layout, releasing ACKed frames without the cleanup queue (which only
   moved the cost), an expiry heap with inline keys and branchless sifting,
   and that heap with a counter-first coverage layout. The heap lowered
   cost per frame in all six of its pairs and raised unlimited delivery
   2–7%, but no candidate's median reached its 3–5% bar at those overloaded
   points ([BENCH.md](BENCH.md#where-the-small-frame-cost-comes-from-november-12)).
   Retested once with the project's efficiency method (offer-limited
   point, eight alternating pairs, an A/A run alongside), the heap cut CPU
   per frame 4.2% and landed
   ([BENCH.md](BENCH.md#the-expiry-heap-at-an-offer-limited-point-landed)).
   Giving one worker several TAP queues would not help the
   idle workers: under a random hash, a worker owning two of sixteen queues
   is as likely to get no flow as one owning one of eight (about 12% each
   with sixteen flows). Only flow-aware steering would even them out, and
   tun's steering programs are eBPF, which deployment excludes; with many
   flows the gap mostly closes on its own.
   One small-frame flow reaches 0.61–0.67 M frames/s, limited by one
   thread. 60-byte frames match 64-byte ones; 128-byte frames are lossless
   at 0.92 M frames/s paced, and unlimited R→L trails L→R by a quarter to a
   third. QinQ 128-byte frames reach the same total with the split
   reversed (2.18–2.31 / 3.32–3.53 M frames/s).
6. **Stage 4** is done as of November 12. OVS joined the composition
   matrix, and the panic audit converted the last locally bounded indexing in
   the encoders and the batch planner: explicit panics are gone, untrusted-input
   parsers and the worker's packet paths are fuzzed and checked, and seccomp
   fault injection covers worker and thread-start failures. TAP persistence
   and descriptor handover, including a failed exec after a passing probe,
   double-fork readiness, the systemd unit, the VRF and composition tests and
   the V03 lifecycle cases are done.
7. **Stage 5:** profile what remains and try its candidates behind switches.

If evidence shows a gate cannot be reached within the current design, write
that conclusion with its evidence in BENCH.md and here instead of trying more
small changes.

## November 9 throughput audit

The audit starts from these facts. The last build before the shared sender
delivered 7.76 / 16.70 Gbit/s on the loopback QinQ check. Connecting the
sender, ACKs and recovery cut that to about 2 Gbit/s, and the routed 4.5 ms
path delivered 0.5–0.9 Gbit/s per direction against gates of 20 Gbit/s
locally and 5 Gbit/s over a lossy 4.5–7.5 ms path. About two dozen
single-change candidates had each been judged by one sweep, which cannot
separate 10–30% effects from run-to-run variation.

Rules it adds to "Rules for every change":

- Do not start another one-constant candidate until steps 1 and 2 are done.
- The shared sender's contract is provisional and may change with evidence:
  one coordinator on worker 0, epoch batching with ordered commits, the
  aggregate burst cap of min(quantum, 64 datagrams), the 4 MiB/s control
  ceiling, the `8192 / workers` retention split and 32 ACK boxes per worker.
  Any replacement must keep one congestion budget per path group, account for
  every transmitted byte including partial sends, sample delivery rate
  exactly, and keep each packet's send before its ACK.
- For every step, write in BENCH.md the hypothesis, the result that would
  confirm or refute it (written before running), the median and range over
  the repeats, and the decision. Keep rejected patches in `benches/`.
- Update this checkpoint and README's status after each step.
- If evidence shows a gate is unreachable within the current design, stop and
  write that conclusion with its evidence instead of trying small changes.

Its steps, in order:

1. **Make measurements able to decide.** Done. The decision workload is the
   routed 4.5 ms path with bridged individual QinQ frames, four workers, 4 and
   16 flows, 0.5/2/5 Gbit/s and 0%/0.1% loss; 44-segment GSO stays an
   overload check only. Five production repeats measured the noise, and the
   generator's timer slack was found and removed.
   [BENCH.md](BENCH.md#decision-workload-and-its-noise-november-9)
2. **Explain light-load expiry with a trace.** Done. The `trace` feature
   attributes each never-submitted frame's lifetime. Verdicts: one stalled
   lane while others had credit, confirmed; dropped ACK records mattered by
   stranding sent history, not through a congestion response; ProbeRTT,
   Drain, burst refusal and closed issuance were rejected at under 5% each.
   The fix and its regression tests are in.
   [BENCH.md](BENCH.md#light-load-expiry-traced-to-a-stalled-lane-november-9)
3. **Measure the coordination ceiling.** Done. The no-coordinator diagnostic
   recovers most of the pre-integration rate and the controller bypass does
   not, so the shared-owner protocol is the limit.
   [BENCH.md](BENCH.md#the-shared-owner-sender-is-the-throughput-limit-november-9)
4. **Redesign the coordinator and amend the design.** The section 9
   amendment, its equivalence oracle and the runtime replacement are done and
   measured. Sending ACKs from receive owners was measured and rejected.
5. **Size capacities for the path.** In progress; item 3 of "Next, in
   order". The ACK-box pool is sized. Only after steps 2 to 4: per-session retention credit that busy
   workers can borrow, shared receive slots per worker and the ACK-box pool,
   each sized from the delayed and lossy decision rows with memory cost
   reported. Target: one flow at 5 Gbit/s at 7.5 ms with 0.5% and 1% loss on
   four workers, within two to four cores per daemon.

## November 8 checkpoint

The November 9 checkpoint supersedes this section's next steps.
Performance policies remain those of `35ed011`: bounded fresh expansion,
corrected routed benchmark ordering, failed-ACK retry and early ACK-buffer
recycling are retained. Subsequent parser hardening replaces the ancillary
metadata conversion's `unwrap` with a checked four-byte slice operation.
That cleanup passes 360 ordinary release-musl tests, strict Clippy, formatting
and all 98 ignored tests on each of Linux 6.8 and 7.0, including the full
returned/filtered-PTB and upward-retry checks. The latest completed performance
comparison rejects early successor requests and receive-side ACK retention. Both pass their documented correctness suites,
but the first still collapses under GSO overload and the second regresses
bridged input. [BENCH.md](BENCH.md#successor-requests-and-ack-retention-do-not-fix-both-input-paths-november-8)
contains the patches, all 24 measurements, drop attribution and test limits.

No 20G local, 5G routed-loss or universal overload-plateau gate has passed.
Next, measure ACK-box residence and receive-credit occupancy against owner
service, and profile the bridged worker that reaches 95.5% CPU plus kernel
softirq cost. The earlier claim that no thread approaches saturation does not
cover this profile. Preserve physical-send ordering and ACK/loss barriers in
any sender redesign. Stage 4 TAP lifetime/restart and production panic work
remain open; stage 5 must revisit every original performance gate.

## Rules for every change

- `libc` is the only dependency of the library. Feature-gated crates
  (`dns`, `uuid-compat`, `bin`) are the only exceptions, as the README lists.
- Every public item has a doc comment. Every `unsafe` block has a `// SAFETY:`
  comment on the line above. Use `const fn` where a function can be one.
- The transport core never touches ordinary files, environment variables or stdio.
  Kernel interfaces (sockets, ioctls, `/dev/net/tun`, `timerfd`, `eventfd`,
  `epoll`, `getrandom`) are fine. The optional iphost layer uses the selected
  resolver's facilities as documented in config-model.md.
- No async runtime, no wall-clock values on the wire, no host-specific facts in
  the repository.
- Keep strict flow pinning unless measured handoff costs justify relaxing it.
  A frame and its repairs always stay on their originally selected lane.
- Do not use eBPF; production deployment cannot depend on permission to load it.
  Classic socket BPF receive steering keeps its ordinary-hashing fallback.
- No locks or allocations on hot paths. Protocol transitions take supplied
  monotonic time and make no syscalls. Batch socket I/O, queue notifications
  and timer updates; measure before claiming a performance improvement.
- Production code must not panic, including through unchecked indexing,
  arithmetic or `unwrap`/`expect`. Handle failures without losing published TAPs.
  Test assertions are allowed; panic-abort and catch-unwind are not substitutes.
- Published TAPs survive transport failures and descriptor-preserving restarts;
  removal requires an explicit instruction. External renames are authoritative:
  use device identity, never restore the configured creation name.
- Receive admission checks cookies, wire validity and protocol state on the
  bound socket, regardless of sender address or port. Transmit destinations
  come only from configuration or the optional resolution layer.
- Integers on the wire are big-endian and encoded explicitly. Never cast a
  struct to bytes.
- A new branch, parser, state machine or table gets a unit test that fails
  when it breaks. Kernel behaviour gets a case in a `#[ignore]` container
  test. Protocol state machines are tested as pure functions on fixtures
  before they touch a socket.
- Before finishing a step, run:

  ```sh
  cargo test --target x86_64-unknown-linux-musl
  cargo clippy --target x86_64-unknown-linux-musl --all-targets -- -D warnings
  cargo fmt --all -- --check
  ```

  and run every `#[ignore]` test in a disposable container that has
  `/dev/net/tun`, `CAP_SYS_ADMIN`, `CAP_NET_ADMIN` and `CAP_NET_RAW`. The test
  binaries are static, so any minimal image works. Build with
  `cargo test --no-run`, copy the test executable in, run it with
  `--ignored`, and delete the container afterwards, also on failure. With an
  unprivileged Incus or LXC container, add the tun device node before the
  first start; hot-adding it to a running container fails.
- One step per commit. The commit message names the step, for example
  `wire: v1 header and record codec`.

## Module map

The target layout. Create files when their step arrives, not before.

| File | Owns | Step |
| - | - | - |
| `src/uuid.rs`, `src/mac.rs`, `src/sha256.rs` | Identity types | done |
| `src/socket.rs` | Receive and transmit sockets | done, extended in 2 and 3 |
| `src/tap.rs` | TAP ownership and frame I/O | done, extended in 2 |
| `src/sys.rs` | Thin `libc` wrappers | grows as needed |
| `src/wire.rs` | v1 header and record codec | 1a |
| `src/handshake.rs` | Roles, tags, four-message handshake | 1b |
| `src/config.rs` | Parsed configuration types and validation | 1c |
| `src/counters.rs` | Atomic counters | 1d |
| `src/spsc.rs` | Bounded single-producer single-consumer queue | 1e |
| `src/worker.rs` | Pinned worker loop: epoll, eventfd, timerfd | 1e |
| `src/runtime.rs` | `run()`, `Control` handle, thread ownership | 1e |
| `src/bin/unquic.rs` | Daemon and `gen-uuid` (feature `bin`) | 1f |
| `src/reassembly.rs` | Receive windows, frame assembly, TAP write queue | 1e, extended in 2 |
| `src/pack.rs` | Flow selection, segmentation and record packing | 1e, extended in 2 |
| `src/ack.rs` | ACK generation and processing | 3 |
| `src/loss.rs` | RTT, loss detection, PTO (RFC 9002 parameters) | 3 |
| `src/pmtu.rs` | DPLPMTUD per lane | 3 |
| `src/bbr.rs` | BBRv3 (draft 07) | 3 |
| `src/pacing.rs` | Grants, pacing deadlines, control ceiling | 3 |
| `src/pacing/budget.rs` | Section 9 replacement: published state, atomic inflight and pacing, per-ACK sampler | 3 |
| `src/trace.rs` | Diagnostics-only event rings (feature `trace`) | done |

## Stage 1: socket and device foundation — complete

Goal: two daemons on one host forward Ethernet frames between two TAPs over
UDP, with the real handshake, the real wire format and worker ownership, but
without repair, packing or congestion control. Datagrams stay at the 1232-byte
baseline.

### 1a Wire codec (`src/wire.rs`) — complete

Implemented in `src/wire.rs`, with fixed datagram vectors in
`tests/vectors/wire-v1` and rejection/round-trip tests in `tests/wire.rs`.
Musl tests, Clippy, formatting and the isolated foundation kernel test pass.

Implement section 6 exactly.

- `Header` with the 48-byte layout, `encode(&self, &mut [u8; 48])` and
  `decode(&[u8]) -> Result<Header, WireError>`. Reject wrong magic, version
  other than 1, nonzero flags, and a record-area length that does not equal
  the remaining payload.
- `Record` enum with one variant per type in the section 6 table, each with
  the exact body. `PADDING` carries only a length. `DATA` borrows its piece
  bytes from the input buffer; do not copy.
- A datagram parser that validates the whole record area before returning
  anything: at least one record, at most 64, exact lengths, reserved bytes
  zero, DATA offset plus piece length within total length and total length
  between 14 and 9022, ACK ranges 1 to 32 strictly descending and disjoint
  and nonadjacent with nonzero endpoints, handshake records alone in their
  datagram with zero tag, lane and packet number.
- Encoders for every record that write into a caller-provided slice and
  return the length, with no allocation.
- Tests: round-trip every record type; one hand-written byte vector per
  record type checked byte for byte, kept as the interoperability vectors
  section 16 asks for; a rejection test per rule above, including a valid
  datagram followed by one bad trailing byte.

Done when a buffer can be parsed and re-encoded to identical bytes for every
record type and every rejection rule has a failing input.

### 1b Handshake (`src/handshake.rs`) — complete

Implemented with automatic tag election, nonblocking tag generation, pure
liveness transitions and protocol regression tests in `tests/handshake.rs`.
The handshake and frame-window model cases now exercise real types in
`tests/handshake.rs` and `tests/reassembly.rs`; the old model files are removed.
Musl tests, Clippy, formatting and the isolated foundation kernel test pass.

Implement sections 5.1 and 5.2 as a pure state machine with no I/O.

- `Role` (elected initiator or responder), `Tag` (nonzero 128-bit), `Params`
  (12 bytes from section 5.2), `Connection` (confirmed `(I, R)` pair plus
  negotiated params).
- `Session` state: configured, establishing, active, degraded, inactive, plus
  the one pending attempt. Inputs: a decoded handshake record with its
  monotonic receive time, or a timer tick. Outputs: zero or more records to
  send, carrier up or down, and a commit event carrying the new connection.
- Elect roles automatically when unchallenged HELLOs cross: lower tag stays
  initiator; the other endpoint chooses a fresh responder tag. Equal tags
  restart with fresh randomness. Freeze roles once challenged and keep the
  active role separate from the pending role.
- The responder resends its existing challenge for the same pending `I` and
  identical parameters; a changed body with the same `I` is invalid. Pending
  attempts expire after 10 s. HELLO and CONFIRM retransmit every second. Apply
  the four-per-second per-session cap with a burst of four.
- Negotiated frame ceiling is the smaller advertised size, 1514 to 9022.
  A local MTU that needs more is a reported mismatch, not a panic.
- Tag generation lives in `src/sys.rs`: `getrandom` with `GRND_NONBLOCK`;
  `EAGAIN` means "retry later", everything else is an error. The state
  machine takes tags as input so tests use fixtures.
- Tests: port the handshake cases from `tests/design_model.rs` to run against
  these real types. Port the frame-window cases to real reassembly types when
  that module lands, then delete the model files. Add the message-loss, duplicate,
  reorder, simultaneous start and restart cases listed under "Protocol and
  ownership regression cases" in section 14.1.

Done when every handshake model case passes against real types, the old mixed
model file is gone, and the frame tests remain available for stage 2.

### 1c Configuration types (`src/config.rs`) — complete

Implemented raw IPv6 configuration with immutable numeric references and
one-to-many endpoint/peer/session relationships. The optional `dns` layer uses
iphost 0.4.4 directly. Default and DNS-enabled tests, Clippy, formatting, aarch64
checking and the isolated foundation kernel tests pass. Cookie lookup has a
repeatable benchmark; lookup and handshake transitions have allocation checks.

Plain structs the daemon fills from TOML and a library user fills by hand:
`Daemon` (memory ceiling), named `Endpoint` (listen address, listen port,
optional bind device), `Peer` (name, endpoint name, optional raw IPv6 address,
peer port, numeric scope ID, lane count, path group), `Session` (name, peer, cookie,
TAP name defaulting to session name, inner MTU, optional MAC, queue count).
Roles are elected and have no configuration field. Follow
[config-model.md](config-model.md). `validate()` returns the first error:
duplicate names or cookies via `CookieIndex`, unknown endpoint or peer names,
MTU out of range, port zero.
The core accepts raw IPv6 addresses only; None leaves a peer waiting for an
address without delaying TAP readiness. The optional `dns` layer uses
`iphost v0.4.4` types and resolution to supply IPv6 candidates. It stays outside
the core validator and packet workers. No file or environment access here.

### 1d Counters (`src/counters.rs`) — complete

Implemented atomic totals and gauges with explicit units, Relaxed updates,
Acquire reads and independent snapshots. Owner sets are cache-line aligned;
concurrent-update and wraparound tests pass. Runtime publication remains part
of 1e. Full musl tests, Clippy, formatting and the isolated kernel test pass.

One struct of `AtomicU64` per session, per peer and per lane, with the names
section 12 lists. Workers increment with `Relaxed`; the control handle reads
with `Acquire`. No locks, no strings in the hot path.

### 1e Worker runtime (`src/spsc.rs`, `src/worker.rs`, `src/runtime.rs`) — complete

The 1024-entry SPSC queue is implemented with cached cursors and fixed storage.
Tests cover full-ring ownership, cursor wraparound, endpoint destruction and
one million ordered transfers. Miri also checks the queue with thread preemption.
The pure two-phase commit barrier is implemented in src/handshake/barrier.rs.
Tests cover all 64 workers, stale acknowledgments, retirement, generation
exhaustion, and full command/reply queues. Worker loops, ownership tables and
barrier integration now forward frames with configured or supplied IPv6 peers. The reassembly/window foundation from stage 2 is
also implemented because step 1e requires permanent duplicate suppression.
Baseline segmentation and inner-flow selection are implemented in src/pack.rs.
Tests cover every supported frame length, identity exhaustion, IP fragments,
IPv6 extension headers and allocation-free operation. Scatter/gather encoding
borrows original frame bytes, and the socket APIs now batch up to 64 datagrams
per syscall. Tests cover partial submissions, independent truncation flags and
allocation-free I/O/error paths. The runtime now uses those APIs, preallocated
buffers and retained partial-send suffixes. Its container test covers two workers,
four TX lanes, both forwarding directions, VLAN materialization at 9022 bytes,
carrier loss and restart recovery, with allocation/deallocation checks during
forwarding. Startup checks reserved storage before TAP creation. Candidate
addresses now pass through a bounded control queue and prepare a second socket
set before new handshakes. Tests cover initial supply, an unreachable candidate,
migration and migration back, unchanged-address updates, and two peers/three
sessions per endpoint. Receive readiness is independent per endpoint.
Workers publish session, peer and lane counters in batches. Lane identities are
generation-qualified; tests cover reuse, expiry, duplicate pieces, receive
reservations and post-shutdown aggregation. The full-ring ownership test verifies
buffer/lane returns and barrier completion. Step 1e's container acceptance now
passes; broader pressure and overload coverage remains in stages 2 and 4.

Implement the ownership rules of section 10 under "Worker ownership and
routing (v1)".

- `spsc.rs`: fixed-capacity ring of 1024 entries, one producer and one
  consumer, `try_push` returning the rejected item when full. Test with two
  threads and a counter.
- `worker.rs`: a pinned thread owning one `ReceiveSocket`, its
  `TransmitSocket` lanes, one `TapQueue`, an `eventfd`, a `timerfd` and an
  `epoll` instance. The loop drains the eventfd and every incoming SPSC
  queue, then TAP reads, then socket receives, then timers, then sends. All
  descriptors are nonblocking. Pinning uses `sched_setaffinity`; failure to
  pin is logged through counters, not fatal.
- Owner table: lane and connection tag to worker index, written only by the
  session control owner (worker 0 in v1) and read by all. A datagram that
  lands on the wrong worker is forwarded whole through the SPSC queue to its
  owner; a full DATA queue drops without ACK.
- `runtime.rs`: `pub fn run(config: Config, control: Control) -> io::Result<()>`
  creates TAPs first (so readiness can be reported before any socket exists),
  then sockets, then spawns workers, then blocks until `Control::shutdown`.
  `Control` exposes counters, per-session state, carrier state, shutdown and
  a method to supply a new candidate peer address.
- Data path in this stage: a TAP frame becomes enough DATA records to fit the baseline
  payload ceiling (up to eight pieces for a 9022-byte frame), in separate datagrams of at most 1232 bytes of UDP payload, sent on the lane
  chosen by the flow key of section 8.3. The receiver assembles by frame ID
  into a per-lane table bounded to 64 incomplete or queued complete frames and
  writes complete frames to its TAP queue. Permanent packet/frame windows and
  TAP-write retention are pulled forward from stage 2 so the first runtime
  cannot redeliver delayed repairs. ACKs and repair remain in stage 3.
- Container test `tests/linux_runtime.rs`: two runtimes in one network
  namespace on loopback with different fixed ports and automatically elected
  roles, each with one TAP. Assert the handshake completes, carrier rises
  on both TAPs, a frame written to one TAP arrives unchanged on the other in
  both directions for 60, 1514, 1518 and 9022 bytes, carrier drops within
  30 s after one runtime shuts down, and the surviving runtime reconnects
  after a restart with fresh tags.

Done when the container test passes and no worker ever blocks on another
worker's queue.

### 1f Daemon (`src/bin/unquic.rs`, feature `bin`) — complete

Implemented feature-gated foreground run and UUID provisioning, TOML validation,
lossless uuid-compat conversions, and per-peer iphost resolver selection. Hostname
helpers start after TAP readiness and refresh candidates outside workers.
Parser tests use the documented examples and check rejection and redacted errors.
The isolated daemon test delays DNS until both TAPs exist, observes DATA sent to
a refreshed IPv6 address, checks forwarding and SIGTERM cleanup. Default,
all-feature and uuid-compat tests, Clippy, Rust 1.89 and aarch64 checks pass.
RA/DHCPv6 operation on real resolver networks remains unverified.


- Extend the existing `dns` feature with `uuid-compat` and `bin` as the
  README lists, with `bin` enabling `dns`, and `[[bin]]` gated on `bin`.
  The core keeps raw IPv6 addresses; daemon TOML hosts use iphost types and
  the optional resolution layer feeds candidates into the core.
- Subcommands: `gen-uuid` prints one UUIDv4 from `UnquicUuid::new_v4` and
  exits; `run <config.toml>` parses TOML with `serde` and `toml` into
  `config::*`, validates, and calls `runtime::run` in the foreground.
- Resolution: use the iphost layer on a helper thread after TAP readiness,
  feeding initial addresses and refreshed candidates through `Control`. The resolver mode is a
  config field; never fall back to the system resolver when a bypassing mode
  was selected.
- Readiness and double-fork from section 12.1 come in stage 4, not here.
  Foreground operation is enough to test.
- Test: a unit test that parses the example configuration from section 12
  and a rejection test for each `validate()` rule.

## Stage 2: batched transport — in progress

Goal: the packet rate and CPU baseline of section 13 can be measured. Final
local acceptance requires at least 20 Gbit/s in each direction simultaneously
at inner MTU 1500, with CPU use reported.

Receive processing now budgets expanded datagrams/records/bytes and interleaves
TAP delivery. The record budget matches the TAP-read batch for small-frame
bidirectional fairness. Lane IDs determine receive ownership independently of
reuseport collisions, so consecutive lanes spread across workers. Existing-frame tails can progress under pressure without ACKing
unretained new pieces. Regression tests cover over-capacity bursts, GRO budgets,
TAP WouldBlock and packed tails/heads; admission, queue, expiry and socket drops
have separate counters. Release rate sweeps and residual overload analysis are
recorded in BENCH.md; neither throughput acceptance gate is closed.

The receive foundation is implemented early for step 1e: fixed frame storage,
whole-packet ACK admission, permanent packet/frame windows, terminal bits,
100 ms expiry and retained TAP-write entries. Worker routing, initial memory
reservation checks and batch integration now exist. Socket sends now accept
caller-supplied lane limits and group scatter/gather datagrams through UDP_SEGMENT,
with a tested ordinary-send fallback. Receive sockets can enable UDP_GRO and
borrow each logical segment after ancillary/truncation checks. Worker GSO/GRO paths
pass the container forwarding/restart test and allocation checks. A mixed-cookie,
mixed-owner aggregate test verifies independent rejection and unchanged payload
storage. Fixed receive pools remain below the directional frame/byte ceilings;
per-origin transmit reservations enforce the same bounds through delayed returns.
The multi-record encoder and socket slice-list API are integrated into workers,
with borrowed payloads, bounded descriptors and round-robin batch service.
Batch metadata uses indices/ranges so original buffers can remain owned across
partial sends. UDP expansion now borrows source payloads and rewrites compact
headers. Ordinary frames share bounded payload slabs; dead ranges stay charged
until their slab can recycle. Fixed frame views replace one 64-KiB buffer per
expanded frame. Static worker quotas still need shared credits for skewed,
ACK-retained traffic; fully submitted views are currently released. Tests cover generated sequences, reverse/duplicate delivery,
small-frame bursts, releasing early slots, individual expiry and quiescence with
full return queues. The ten-second bidirectional TAP storm verifies integrity,
duplicate suppression, observed packing and zero allocations/deallocations while
reporting frame drops. The release baseline in BENCH.md now records single- and
multiple-flow small-frame and MTU-1500 traffic, per-direction delivery/loss, process
CPU and per-core CPU/softirq. The original admission-pressure collapse is fixed;
local TAP USO improves both MTU-1500 and small-frame delivery. The sampled
UDP-input sweeps still show small overload declines with eight workers.
Plateau checks and the measured CPU ceiling remain open.
The post-locality profile in BENCH.md records no empty-buffer TAP reads;
receive/TAP delivery and transmit flushing need finer attribution before
another optimization. Its improving instrumented pair does not explain the
remaining declines in uninstrumented repeats.
Unfragmented UDP with at most one VLAN tag uses the TAP reader's owned lanes
when a session has a queue on every worker; measured handoff savings justify
relaxing pinning there.
Stacked VLANs, TCP, fragments, other protocols and sessions with fewer queues
retain strict hashing. QinQ measurements show that full-lane hashing improves
throughput when TAP input concentrates on one queue. TAP queue migration may
reorder UDP frames; a frame and its repairs
keep one lane. The expanded benchmark history measures this reordering without
changing transport storage or admission. See BENCH.md for UDP/TCP comparisons.
The November 6 cookie controls reduce reordering with aligned placement but do
not establish a throughput gain. A separate full-batch queue rotation bug is
fixed with per-lane pending FIFOs. The earlier deferred-frame marker missed
new arrivals between bounded scans. A regression reproduces that inversion;
tests now cover interleaved arrivals, expiry, other-session progress and
connection replacement. Admission/expiry remains bounded to 64 pending
frames per turn, with round-robin service between ready lanes. Encapsulated IPv4/IPv6/Ethernet flow hashing now includes one
inner header while retaining outer identity and strict pinning. Tagged SRv6
release measurements verify that this spreads work and improves goodput;
remaining losses and the throughput gates are still open. BENCH.md separates
these findings from the remaining performance hypotheses.
The final throughput gate remains open. Partial completion, GSO/GRO boundaries, MTU errors
and allocation checks pass. The five frame-model regression cases
now run against the real implementation in tests/reassembly.rs.

VLAN-tagged frames are the expected deployment workload; untagged IP is a
control case. Prioritize 802.1Q/QinQ throughput and overload sweeps during
stage 2, including tags materialized by Linux TAP. Input segmentation and
lane selection parse tags. Output UDP coalescing now preserves up to two
802.1Q/802.1ad tags, with byte-exact kernel tests for both IP versions.
Keep VID, PCP/DEI, stacked tags, frame limits and per-flow ordering intact.
Do not defer this performance coverage to the stage 4 composition matrix.

- `socket.rs`: `recv_batch` with `recvmmsg`, `send_batch` with `sendmmsg`,
  UDP GSO via `UDP_SEGMENT` and GRO via `UDP_GRO` with runtime detection and
  a fallback path that stays correct. On GRO receive, split by the returned
  segment size and validate every datagram separately. Replace the fixed
  1232-byte cap with a per-lane limit the caller passes.
- `pack.rs`: fill one datagram with several records, including a frame tail
  followed by the next frame's head, flush when the queue empties, keep
  GSO segments equal-sized except the last. Property test: any sequence of
  frames packs and unpacks to the same frames.
- `reassembly.rs`: the 65536-packet and 65536-frame-sequence windows of
  section 8.2 with permanent lower bounds and terminal bits, the bounded
  TAP-write queue of section 8.1 with `EAGAIN` retention, receiver deadline
  of 100 ms, and the resource limits of section 8.2 (8192 frames, 64 MiB
  payload per direction, 256 MiB daemon ceiling). Reservation before
  acceptance; under pressure, validated tails may complete existing frames,
  but no packet is ACK-eligible until all its nonterminal DATA is retained.
- TAP reads retain the 64-frame budget and strip internal `IFF_VNET_HDR`
  metadata. The runtime enables checksum offload and completes partial checksums
  in place. UDP/TCP super-frame input uses bounded userspace segmentation from
  the existing pool after one whole-input handoff to the selected lane owner;
  TCP options and sequence/flag semantics are preserved. Compatible complete
  UDP/TCP frames can share a lossless USO/TSO TAP write, preserving per-flow order,
  backpressure and the existing receive deadline. Other frames use single writes.
  Workers use bounded nonblocking io_uring read batches when available,
  draining completions before releasing buffers and falling back to plain reads
  on unsupported kernels. UDP super-frame sweeps improve delivery but still
  miss the 20 Gbit/s gate and show overload declines. TCP output coalescing
  preserves sequence, checksum and header semantics with independent fallback.
- Preallocated buffer pools and descriptor rings in the worker; no allocation
  on the data path after startup. Verify by a test that counts allocations
  with a global allocator wrapper in the test binary.
- Tests: retain the real window tests in `tests/reassembly.rs`; the old model
  file has been removed. Add GRO split boundaries, partial
  `sendmmsg` completion returns the unsent entries, container test extended
  with a 10 s bidirectional frame storm and a check that no frame is
  duplicated or corrupted.
- The Rust UDP_SEGMENT/GRO load tool uses concurrent sockets, per-flow pacing,
  byte validation and duplicate tracking; it reports generator CPU separately.
  Direct veth probes exceed 20 Gbit/s each way. Four-/eight-worker harness
  smoke tests pass with plain UDP. B3 UDP/TCP super-frame input is implemented;
  unsupported offloads remain segmented by the kernel. Direct harness probes
  do not pass the tunnel gate; see BENCH.md for measured transport limits.
- Measure: record frames per second and CPU per core on the container
  loopback in a `BENCH.md` table with kernel version and CPU model. These
  numbers are a baseline until a complete-transport run demonstrates at least
  20 Gbit/s of delivered inner goodput in each direction simultaneously at
  inner MTU 1500. Also measure sustained small-frame bursts (60, 64 and 128 frame
  bytes excluding FCS): delivered frames/s, drops and CPU per direction, with
  single- and multiple-flow cases. Report each direction separately, with loss, CPU including
  softirq, core budget, flow count and the precise measurement layer. Keep
  this requirement open until a repeatable run passes.

The delayed benchmark now uses one endpoint veth NAPI queue. Captures traced
unintended reordering to the old per-CPU receive backlog; the corrected short
diagnostic runs have zero outer reordering and ordinary loss declarations.
This fixes the lab path, not the transport's outstanding throughput gaps.
BENCH.md retains the original results and the before/after evidence.

## Stage 3: recovery and congestion control

Goal: a production candidate under loss.

Session liveness already uses matching small probes on local lane 1.
ACK, handshake and PROBE_REPLY sends now share the path group's 4 MiB/s
ceiling and sixteen-datagram burst. A fixed scheduler charges successful outer bytes;
throttled handshakes retain one current flight and use deadline-driven retries.
Per-session clocks and tokens track successful submission after deferral.
Configuration groups default binding scopes independently of endpoint names and
ports, and shares explicit group names across endpoints. Tests cover burst
wraparound, byte sizes, failed sends, deferred phases, cancellation, expiry
and multiple sessions. Four reserved feedback buffers per worker carry ACKs
through the existing handoff rings. A shared control queue rotates blocked
groups and returns, with at most 64 construction visits and 64 service visits
per turn. Tests cover rejected packed tails/heads, full handoff and return
queues, stale identities, control-class fairness and actual-send duplicate
spacing. Failed ACK submission retains the reserved snapshot for a 1 ms retry;
stale connection/owner snapshots return to their pool. The `ack_send_deferred`
counter records these attempts separately from incoming ACK-buffer drops.
Incoming ACK transfer buffers now return once history copies their validated
ranges, while timing, pending-ACK accounting and loss fences survive the final
coordinator report. Tests hold that report while reusing every origin buffer,
then check duplicate completion and invalid/stale rejection. Both kernel matrices
pass; reduced light-load buffer drops do not establish a throughput gain.
Incoming ACKs now reach TX-owned histories; DATA and probes use shared
controller grants. Release forwarding, reconnect, allocation and routed-MTU
regression tests pass.
The pure shared grant ledger now reserves credit before handoff, keeps it through
100-microsecond expiry, and consumes each sent/unused report once. Partial
submissions convert only successful outer bytes to inflight charges. Workers
share the send quantum and sixty-four-maximum-datagram byte cap; PTO rounds share
at most two one-datagram grants, with epoch checks on unused reports.
Exact pacing deadlines, delayed completion, duplicate reports, limit reductions,
late ACK/loss settlement and allocation-free transitions have regression coverage.
BBR output, retained report routing, startup accounting and worker enforcement
are now connected. PMTU candidates must fit the current grant quantum and total
congestion window;
limit the candidate size without treating an unconfirmed probe as the effective
MSS. The socket batch API now supports a timestamp/permission callback immediately
before each syscall, including a retry without GSO. Tests force that fallback,
expire a grant before its retry, and verify no packets were sent. Callback
errors leave GSO capability unchanged; partial prefixes retain the successful
attempt's timestamp. Workers now use this check to reject data plans whose
frames expired during message preparation or before GSO fallback. Expiry keeps
bounded cleanup ownership and does not mark the socket blocked. Handshake
attempts revalidate expiry at submission; successful handshake/liveness timers
and PMTU proofs use pre-send time. ACK delay and duplicate spacing share that
timestamp, and the control service ceiling cannot bank a delayed send's elapsed
time. Receive batches retain post-receive time across expansion and handoff.
Regressions inject delayed clocks for these paths.
Receive-side ACK scheduling and range construction now use the existing
reassembly bitmap, with no second history or hot-path allocations. Tests cover
thresholds, gaps, duplicate rate limits, pressure rejection, word/window wrap,
maximum packet numbers, and reset/retirement. Pure sender-history validation now
distinguishes reservations, successful submissions and canceled holes, validates
whole ACKs before mutation, and preserves unresolved charges under window pressure.
Tests cover late ACKs, feedback-only eviction, ring wrap, maximum identities and
allocation-free transitions. ACK application can now yield after a caller-supplied
budget of bitmap scans, range transitions and callbacks. Whole-record validation
precedes application and trims already-acknowledged range boundaries without
discarding late-loss delivery or changing the original-largest RTT reference.
Pending ACKs pin their history window and protect covered
packets from interleaved loss. TX owners also validate queued ACKs before
enqueue and protect their covered packets during mandatory preceding loss
passes. Scans still finish, stale connections cannot block cleanup, and a fully
protected final pass requests continuation without an external wake.
Tests cover a full-window ACK, duplicate-only
scans, floor pressure, competing ACKs, late loss and RTT sampling across yields.
Outstanding-charge scans now yield on a bitmap-read budget, including empty
words. A 128-byte per-history summary skips empty ranges; summary reads consume
the same budget, while dense scans retain one read per packet. Each pass has a
fixed upper packet number and rechecks state after ACK,
loss and eviction; new sends cannot extend a pass. Empty, sparse, full-window,
maximum-number and allocation-free regressions pass. TX owners now run these bounded
scans and retain reports until the controller consumes them.
Worker integration retains the ACK's arrival time and reserves report space
before advancing it. Sender entries now use 16 bytes each, retaining full
`Duration` range and nanosecond precision; 64 histories save 32 MiB compared with
the previous 24-byte entries. Worker submission accounting and incoming ACK
application now use these histories. Outgoing ACKs are now emitted through the shared
control queue. Per-lane MTU probing and bounded payload recovery are connected
to the workers. Complete congestion-control integration remains pending;
README.md documents probe feedback, recovery and send-error handling.

The pure draft-07 delivery sampler now has fixed vectors for ACK compression,
idle restart, application bubbles, late ACKs after loss and duplicate suppression.
It preserves a single sample across bounded ACK chunks and counts each physical
transmission once. Allocation checks and sent-history integration tests pass.
The pure draft-07 controller now implements Startup, Drain, ProbeBW and ProbeRTT,
fixed model filters, pacing/window gains, loss bounds and recovery undo. Fixed
cycle vectors and boundary tests cover application limitation, filter expiry,
MTU changes, loss thresholds, RTT probing and a tenfold capacity reduction.
Sampler/controller/grant tests cover partial submissions and late originals.
Steady-state sampling and model updates allocate nothing. Pseudocode/prose
differences and integer rounding are recorded in tests/vectors/bbr-07/README.md.
Runtime feedback ordering, controller ownership and worker enforcement are
connected. Controller creation uses the runtime clock; a one-day-uptime regression
prevents the first ACK from spuriously entering ProbeRTT.
Successful window-use evidence now survives multiple ACKs of one packet-timed
round, including its boundary ACK. Unused reservations do not set it, and an
application-limited idle restart clears it. Zero inflight alone does not prove idle.
PMTU-only delivery samples cannot establish a bandwidth plateau: probe cadence
does not measure application supply. Per-transmission provenance survives
partial sends, flight reuse and late ACKs; mixed DATA/probe ACKs retain normal
capacity evidence. Delivery, loss and RTT accounting remain unchanged.
Application-limited bubbles now come from each path group's unsent-data mask,
following draft 07's prerequisites; congestion-control integration is not
complete.
Reserve sampling metadata against the startup memory ceiling; a full sampling
record in every lane's 65,536 history slots would exceed that ceiling. Use bounded
shared storage and retire only resolved sender-history prefixes, preserving
complete sampling state above the new floor. Constant-work prefix retirement
now stops at reservations/charges and rejects pending ACKs. Regression tests
cover canceled holes, lost and feedback-only packets, ring wrap, maximum
identities, and sampler reuse before a late ACK. Sampling handles now fit inside
the existing 16-byte history entries: ACK and retirement take ownership, loss
only copies it. A live-handle count prevents silent eviction/reset; the final
packet number can retire without wrapping. The shared sampling slab and ordered grant epochs now live in
`pacing::Sender`. Startup allocates at most 65,535 slots across the daemon and
one movable flight buffer per worker. Every slot checks group, session,
generation, lane and packet number. The first completion closes issuance,
including an unused grant; all reports must return before successful runs
commit in timestamp/grant/ordinal order. Commit work has a packet budget and
preserves each transmission's prefix inflight count. Empty groups can enter
feedback for pressure retirement without first reserving a slot. Tests cover
reversed reports, all 64 workers, partial sends, stale identities, bounded ACK
transactions, full-pool PTO/loss/retirement progress and counter exhaustion before
permission to send. Allocation instrumentation covers sorting and 66,000
history/slot reuse cycles; reserved-memory estimates match actual allocations.
Worker zero owns the controllers and ordered sampler; TX workers own histories.
Send-report storage is separate from feedback buffers. RTT uses original receive
time, while delivery accounting uses fresh owner observation time. The
first paced release sweep delivers only 0.67 to 0.76 Gbit/s per direction;
`BENCH.md` records the regression and ongoing scheduling investigation.
Ready completion/feedback batches now use the remaining coordinator budget,
and end-of-turn handoff avoids an extra worker-loop delay.
Request scans retain priority after their first successful grant; regressions
cover continuously ready workers in one group and across groups.
Coordinator feedback can overtake deferred DATA and probe numbers. Local grants
now refresh unsent headers and accounting identities without copying payloads or
extending permission. Regressions cover steady feedback, pacing-held requests,
queued permits, trimmed grants, probe identity and history-pressure retry. The
first three fail on the previous send path and pass with refresh.
Fully successful trimmed grants now retain the never-attempted DATA suffix,
avoiding packet-number gaps and repeated packet preparation. Partial sends,
expiry, PMTU shrink, retirement and overtaking controls still discard or refresh
the affected plans and cancel their reservations. The contiguous-number
regression fails on the previous path; split-frame, partial-send and reservation
cleanup tests pass. Returned plans now compact consumed metadata and append
waiting frames without copying payloads or changing reserved packet bytes.
Refill defers across packet-number gaps. Two matched release sweeps average
2.55 / 2.45 Gbit/s paced and 2.45 / 2.79 unlimited, still far below the gates;
individual directions remain variable. BENCH.md records the remaining loss,
forward decline in one sweep and thirteen reordered frames in another point.
The coordinator now runs a bounded handoff of newly produced local ACK/loss
reports to the shared owner before the next grant pass. A regression reproduces the previous
extra turn with settled local history but an outstanding shared inflight charge;
the same turn now applies the report and still hands off the next foreign permit.
The wider release sweep remains below 1% missing only through 2G offered per
direction; unlimited forward goodput falls 17.2% from its 4G peak. The original
throughput and overload gates remain open.
Earlier completion publication and sharing one bounded grant pass across ready
workers have now been measured in isolated normal-release builds. Neither
establishes a repeatable bidirectional gain, so both remain outside production.
BENCH.md retains their patches, loss/CPU data and limits. Continue bounded
payload recovery, then return to sender profiling and all original performance
gates in stage 5; no gate is waived and recovery is not the asserted cause of
the current scheduling regression.

The pure RTT estimator, loss-deadline calculation, PTO-period backoff and frame
lifetime now live in `loss.rs`, with exact-value and boundary tests. ACK-driven
RTT updates and indexed group PTO timers are connected. RTT-based frame admission
deadlines are connected to retained sources and frame views below.
The pure shared PTO state now limits each expiry to two aggregate probe
permissions, backs off across lanes and requests one loss-declaration pass on
the third unanswered expiry. It then stops ordinary grants and keeps backed-off
probes running until fresh directional feedback. A first matching probe reply
clears the silence count; only a new ACK of eliciting traffic resets backoff.
Tests cover multiple lanes, deferred probes, fresh probe replies,
late/duplicate/feedback-only ACKs, long pauses,
overflow and allocation-free transitions. Runtime grants now enforce the allowance;
workers run bounded loss passes and gate liveness/PMTU probes. Blocked PMTU requests
release their flight buffer; PTO dispatch preempts an ungranted ordinary request.
Pending probe commands survive generation changes with their round.
Once a group stalls, PTO targets rotate across installed sessions, then their
lanes, so an unanswered lane cannot exclude a working peer. Selection uses the
existing session links in constant time. The two-probe allowance and backoff
remain shared. Since November 10 a committed replacement resets the backoff
and silence, and backoff is capped at one second (see stage 4's V03 item).
Due liveness probes survive a busy DATA flight and rotate among sessions before
fresh DATA. Bounded visits and 1 ms history-pressure retries prevent monopolizing
a turn or spinning; the next one-second interval follows successful submission.
Kernel regressions cover busy DATA, history pressure, barriers and socket loss.
The coordinator now closes grants before each PTO loss check and waits for
physical send commits, bounded worker scans and final report returns. The third
unanswered expiry waits for forced settlement before probes. Stable per-worker
message ordering preserves this barrier under queue pressure; accepted ACKs
finish before scans, and new ACKs wait. Tests cover crossed groups, generation
replacement, elapsed loss deadlines and saturated handoff queues. Ordinary ACK
admission also waits for the accepted batch's resulting loss passes, preventing
continuous ACK input from postponing repair indefinitely. A pre-existing scan
finishes before a fresh pass uses newer ACK evidence. Payload repair
is connected through the bounded coverage and journal path below.

Pure per-frame byte coverage now lives in `loss::FrameRecovery`, with two-repair
limits, outstanding-generation checks, late ACK handling and bounded slot reuse.
Word masks are checked against a byte model and allocation instrumentation.
The packer can select exact missing ranges while retaining original frame lengths
and identities. Tests cover header/payload boundaries, partial submissions and
completion after an original piece and its first repair are both lost.
Frame-view storage now follows each worker's actual session quotas, capped at
8192 shared slots; startup accounting uses the same calculation. Tests cover
unequal queue/lane counts, multiple sessions, empty stores and slot recycling.
The pure packet-to-frame journal now reserves a whole packet's mappings before
I/O and unlinks them directly by packet or frame. Loss retains mappings for late
ACKs; expiry releases mappings without settling packet charges. Tests cover
packed tails/heads, partial sends, late ACKs after repair, prepared-plan cancellation,
history-slot reuse, bounded cleanup and allocation-free operations.
A component integration regression now drops a packed frame tail/next-frame head
and its first repair, then delivers both frames through the second repair. It
uses real packer output, receiver ACK ranges, budgeted history/coverage updates
and time-threshold loss detection at 1280/1500/9000 outer-byte ceilings. Late
originals and repairs produce no duplicate frame or double credit release.
This uses a synthetic clock and does not test worker scheduling or throughput.
Workers now allocate stable coverage state and journal mappings against actual
frame quotas and the startup memory ceiling. Successful sends retain payloads;
ACKs, loss and retirement update coverage before the sampler can recycle handles.
Fresh batches leave half the journal capacity for repairs. Coverage work and
frame cleanup yield under separate fixed budgets. Partial sends settle only the
successful prefix, and late ACKs invalidate obsolete repair selections.
A batch stops admitting new frames while the opposite fresh/repair queue waits,
so continuous refill cannot starve that queue. Existing plans and partial-grant
suffixes drain before the normal class alternation.
Sender deadlines use four smoothed RTTs, clamped to 20–100 ms, without extending
the original read deadline. The transmit owner sets that deadline once before
source admission; all expanded children inherit it. Overdue GSO sources discard
remaining children in bulk before expansion or checksum work.
Fresh logical work is now bounded separately at 2 MiB per TX lane, including
unexpanded GSO headers and unsent materialized ranges. One oversized legal
source requires an empty balance. Excess input is rejected before ownership
exchange; successful original submissions and cleanup release exact bytes.
This leaves physical quotas and deadlines unchanged. Tests cover synchronized
QinQ bursts, oversized sources, foreign credits, partial expansion/submission,
repair, cleanup and generation replacement. The matched single-lane release
sweep preserves about 1.9 Gbit/s through overload, instead of falling below
0.6 Gbit/s; eight-lane throughput remains about 1.6–1.8 Gbit/s. Neither original
performance target is met. See BENCH.md for CPU, loss and pressure attribution.

A second bound now limits materialized unsent bytes to 64 discovered UDP payloads
per lane, plus one frame. Remaining source children wait in order; other lanes
continue, and foreign-reader credits wait until sends or expiry reopen capacity.
RX credits and stale generations bypass that hold. Targeted tests cover source
order, credits, cross-lane progress, expiry and 1232/1452/8952-byte payload limits.
The initial delayed bridged sweep improves unlimited delivery to 0.855 / 0.606
Gbit/s while reducing daemon CPU, but ACK-buffer drops and the GSO ceiling remain.
Worker regressions
cover mapping pressure, bounded report processing, partial sends and late-ACK
cancellation. A bidirectional release runtime test drops the original prefix and first repair, verifies
second-repair delivery, then replays late originals without duplication or
hot-path allocation. Sustained impaired-path throughput remains unverified.
Counters now partition expiry into unmaterialized input, unsent/partial/full
transmit frames, and incomplete/TAP-queued receive frames. Worker counters also
separate mapping-capacity rejection from changed selections and record cleanup
and report-staging continuations. These identify work deferred by bounded
budgets without treating all expiry as lost inner traffic.
The current transmit store now indexes exact deadlines in a fixed-capacity heap
and removes pending frames directly. Cleanup visits at most 64 expired frame
views per turn, including already-prepared batches; pre-send checks defer
remaining expired frames without extending their deadlines. Regressions cover
out-of-order admission, slot reuse, source lifetime, blocked sockets and several
full expired batches sharing the cleanup budget.

- `ack.rs`: ACK scheduling (16 packets or 1 ms, immediate on gap), at most
  32 newest ranges, delay capped at 1000 µs, per-lane once-per-millisecond
  duplicate limit, and processing against retained sent entries only.
- `loss.rs`: RFC 9002 RTT estimator and loss detection with the parameters
  of section 8.3 (initial RTT 333 ms, max ACK delay 1 ms, granularity 1 ms,
  packet threshold 3, time threshold 9/8), PTO with at most two probes and
  backoff, "three PTOs without feedback declares outstanding lost once".
- Repair: selective byte-range repair with the two-repair limit, sender
  deadline `clamp(4 * srtt, 20 ms, 100 ms)`, FRAME_ABANDON origin 0 and 1
  with 100 ms lifetime and two retries, LANE_RETIRE with tombstone bits.
- Do not assume an access-line MTU from the audit or a local benchmark. Active
  probing must confirm usable sizes and detect reductions; configured limits
  are constraints, not discovery. This requirement supersedes audit A5's
  fixed-size recommendation for the final runtime. The production NTT NGN
  profile has a 1500-byte outer IPv6 ceiling; other deployments may differ.
  Cap discovery at 9000 outer bytes, including on loopback.
- `pmtu.rs`: RFC 8899 per lane from the 1232-byte UDP baseline, using larger
  confirmed path MTUs up to 9000 outer bytes. Socket I/O, packing and receive
  validation now support 8952-byte UDP payloads. The padded-probe encoder
  borrows shared zero padding for scatter/gather sends. The pure state machine
  now bounds proof history, checks full probe identity/length, narrows the search
  after loss, handles reductions and baseline failure, and retries upward.
  Deterministic tests cover every supported MTU, stale/forged replies, loss and
  backpressure. Runtime scheduling now visits at most 64 owned sockets per pass,
  routes feedback to lane owners and shares socket state across sessions.
  Probe-connection replacement now rearms confirmation without reopening a
  failed baseline or resetting an unchanged shared-session probe history.
  Static probe slots are temporary until the shared control pacer is available.
  Keep the hot path allocation-free. Use padded
  PROBE and matching PROBE_REPLY, with `EMSGSIZE` and validated `IPV6_RECVERR`
  Packet Too Big as "shrink now, re-probe later". Local interface MTU alone
  never confirms a path. Reset discovery for new paths and keep TAP MTU
  independent. Preserve `IPV6_DONTFRAG`; probing must be able to exceed a
  cached PMTU without fragmentation (RFC 8899 section 4.5). Socket discovery
  setup and bounded error-queue reads are implemented; a kernel test verifies
  actual PTB quotes, cached-MTU bypass, local reductions and recovery, and GSO
  retention after EMSGSIZE. Worker tests cover actual PTB errors, forged quotes,
  silent size filters, reduction/recovery, shared sessions and repacking unsent
  byte ranges. A release test now connects two real runtimes through a private
  router: every lane confirms 9000, reduces to 1500 with returned PTB, reduces
  to 1280 with PTB filtered, then recovers to 9000 after the real 600-second
  retry interval. It checks bidirectional QinQ frames and captures without outer
  fragmentation or capture drops. DATA-quoted PTB now matches the current
  socket destination, path, connection/generation and exact retained submitted
  header/length. Unsent reservations and hints predating newer MTU evidence are
  rejected. Kernel checks inject forged and valid DATA quotes; pure checks cover
  replay after reduction/reconfirmation and future send times. Retain independent-lane,
  lost-probe and forged/stale-feedback regression coverage.
- `bbr.rs`: BBRv3 as draft-ietf-ccwg-bbr-07 specifies, one owner per sending
  direction and path group, charging outer IP bytes, initial window ten
  datagrams. Pin deterministic vectors derived from the draft's pseudocode
  before optimising anything. Do not substitute a simpler controller.
- `pacing.rs`: the section 9 replacement sender. Each claim charges inflight
  within the published window and takes pacing credit of at most
  min(send quantum, 64 datagrams) from the group's atomic clock; `timerfd`
  deadlines; the separate 4 MiB/s control-traffic ceiling with a 16-datagram
  burst. The current grants with 100 µs expiry and ordered commits are
  measured as the throughput limit and are being replaced.
- Liveness from section 5.3: PROBE once per second on idle, degraded after
  3 s, inactive after 30 s, replacement handshake with automatic election.
- Tests: the BBR and feedback cases under "Protocol and ownership regression
  cases" in section 14.1; a container test with `tc netem` loss and reorder
  inside the namespace proving no duplicate delivery and bounded memory; a
  loss sweep (0, 0.1, 1, 5 percent) recorded in `BENCH.md`. Include RTTs of
  4.5 and 7.5 ms to exercise the real-access target: at least 5 Gbit/s over
  routed IPv6 10G-EPON with nonzero packet loss. Record independent and burst
  loss separately; simulated results still require validation on real access lines.

## Stage 4: composition and robustness

- Validate Ubuntu 26.04/Linux 7.0 as a deployment target while keeping support
  distribution- and init-system-independent. Check version-sensitive TAP,
  io_uring, offload, steering and namespace behavior on the deployed kernel.
  Record the tested kernel matrix and exercise unavailable-feature fallbacks;
  Linux 6.8 source citations and tests alone do not establish compatibility.
- Implement section 11's TAP lifetime contract. Keep published devices alive
  through worker/socket failures and shutdown unless removal was explicitly
  requested. Define versioned descriptor adoption and re-exec handover with no
  last-close gap; retain descriptors on failed exec. UDP sockets may be recreated.
  Version 1 of the handover format is now defined in design section 11
  (persistent TAPs, an `adopt-version` probe, systemd-style descriptor
  announcement, exec from a private descriptor table). Implement it in this
  order: persistence with an explicit removal command, the library's adoption
  of supplied queues, then the daemon's restart and re-exec. Persistence is
  done: TAPs survive shutdown and failed starts, a start reattaches an idle
  persistent TAP of its name (an rtnetlink query reads the attached-queue
  count) and `unquic remove-tap` deletes one. Library adoption is done:
  `Tap::adopt` validates handed-over queues and `runtime::run_adopting` runs
  sessions on them, with kernel tests on Linux 6.8 and 7.0. The daemon's
  restart is done: `unquic restart` signals SIGUSR2, the daemon probes the
  binary, hands its queues over through a re-exec and the new image adopts
  them; a container test shows the TAP, its MAC and a VLAN upper surviving on
  both kernels, and a refused probe leaves the daemon running. A failed exec
  after a passing probe is tested too: a successor script that deletes itself
  once probed makes the exec fail, the daemon resumes on the kept queues with
  its VLAN upper intact, and a later restart hands them to a real successor
  (Linux 6.8 and 7.0).
- Resolved: the routed PMTU test's 1500-byte phase failed in three of five
  local runs on Linux 6.8. The group controller took its maximum datagram
  from whichever lane sent last, so once the busy lane fell to 1500 the send
  quantum could shrink below the idle lane's 9000-byte confirmation probe,
  which was then abandoned on every attempt and never drew a Packet Too Big.
  The controller now uses the largest datagram any lane of the group sends;
  four local and two remote runs have passed since.
- Support foreign TAP renames during operation and across handover. Test unchanged
  ifindex, MAC, upper devices, bridge membership and forwarding, including reuse
  of the old name by an unrelated device. Never rename back or recreate by name.
- Support foreign moves into another network namespace where the kernel permits
  them. Keep workers and UDP sockets in place and forward through the existing
  TAP descriptors. The release runtime test now verifies changed ifindex,
  old-name reuse, repeated rename, bidirectional forwarding and carrier recovery
  across peer restart. Never move back or alter namespace policy.
  Do not assume existing upper devices or bridge membership can cross namespaces.
- Audit production panic paths, including implicit bounds/overflow failures and
  thread startup/join errors. Add malformed-input and fault-injection tests that
  verify error handling preserves published TAPs; keep assertions in tests only.
  The November 8 library-only check at `bed89aa` reports 96 `unwrap`/`expect`
  sites: 73 in `worker.rs`, nine in `wire.rs`, five in `handshake.rs`,
  three in worker feedback, two each in runtime and reassembly, and one
  each in packing and sockets. This identifies panic-capable operations;
  it does not prove their preconditions can fail. It excludes the daemon,
  implicit indexing, arithmetic overflow, allocation failure and other
  library calls that can panic. A passing ordinary Clippy run does not
  close this requirement. The subsequent ancillary-parser cleanup replaces
  its four-byte conversion with `first_chunk`, reducing the count to 95.
  On November 10 the command below, extended with `clippy::unreachable`,
  `clippy::todo` and `clippy::unimplemented`, reported 97 sites; all are gone.
  Broken invariants now return errors, or skip the item where a local
  fallback is safe. Every parser of untrusted bytes now has a randomized
  robustness test: 200,000 deterministic inputs each for wire decoding, flow
  keys, TAP completions, GSO segmentation, USO/TSO coalescing, reassembly,
  ancillary data and rtnetlink replies, as random bytes or mutated valid
  frames. None panicked. The 82 indexing sites on those paths whose bound
  came from a check in another function now use checked access; the rest have
  a local check or index fixed arrays. Every packet-derived value was then
  traced through the worker, sender, ACK history, pacing, PMTU and handshake
  code: none reaches an unchecked index or overflow, but a burst of ACK
  records while another worker lagged could fill the sender message queue and
  return an error that stopped the runtime. The queue now reserves room for
  every ACK buffer, and a randomized test drives 20,000 mutated datagrams
  through two linked workers' receive paths. A fault-injection runtime test
  uses seccomp to fail a syscall every worker needs (`timerfd_settime`) after
  the TAP exists, and then thread creation itself: each time the runtime
  stops with that error and the published TAP keeps its ifindex (Linux 6.8
  and 7.0). On November 12 the last 93 indexing sites Clippy's
  `indexing_slicing` lint reported in the encoders and the batch planner
  (`wire.rs`, `pack.rs`) became checked access, at equal per-frame cost
  ([BENCH.md](BENCH.md#checked-indexing-in-the-encoders-and-batch-planner-november-12)).

  ```sh
  cargo clippy --release --target x86_64-unknown-linux-musl --lib --all-features -- \
    -D clippy::unwrap_used -D clippy::expect_used -D clippy::panic
  ```

- Container tests for Linux bridge, OVS system port where OVS is available,
  VLAN and QinQ uppers, macvlan, multiple MACs, multicast, broadcast and LLC
  frames, as section 14.1 V04 lists. Each test creates and deletes its own
  devices. Done: one runtime test covers a bridge port, 802.1Q and QinQ
  uppers at full size, a macvlan, three source MACs, broadcast, multicast and
  LLC on Linux 6.8 and 7.0. A second makes a TAP an OVS system port beside a
  veth, with its own ovsdb-server and ovs-vswitchd in the test namespace
  (Open vSwitch 3.7.1 unpacked from Ubuntu packages into a private tree, not
  installed), and checks the same frame kinds, 9014-byte frames and a
  full-size 802.1ad/802.1Q stack in both directions on Linux 6.8 and 7.0. It
  skips when `UNQUIC_OVS_ROOT` has no OVS userspace.
- VRF binding test with a VRF device created in the namespace. Done: two
  runtimes bound to a VRF that holds both outer addresses, which the main
  table cannot reach, forward frames both ways on Linux 6.8 and 7.0.
- Resource stress: fill reassembly, TAP-write and handoff queues; sustained
  receiver overload; many sessions and lanes against the memory ceiling.
  Sustained overload is covered: a runtime test floods one session's TAP for
  two seconds while a second session on the same peer carries numbered
  frames, which arrive intact and once, with no transport allocation during
  the flood, and all arrive afterwards (Linux 6.8 and 7.0). Worker tests
  cover full queues. Another test starts sixteen sessions only under a
  sufficient ceiling and, under 64 MiB, refuses before creating any TAP.
- Restart, path change and DNS change cases from V03. Peer restart, address
  change and the 30-second fall to inactive are covered by the forwarding and
  reconnection runtime test. Loss of all feedback is covered by a relay test
  that drops every datagram both ways until both ends degrade (after 3 s),
  then restores the path. It found two faults, both fixed: PTO backoff had no
  ceiling, so after a few seconds of silence the next probe was seconds away
  and data stayed stopped; and a committed replacement kept the stalled
  group's backoff, so the first side to commit lost its first packets to the
  peer's not-yet-installed connection and waited a backed-off PTO before
  sending again (9 of 10 runs failed). Backoff is now capped at one second,
  or the unbacked period if longer, and a committed connection resets it like
  a new ACK. Both directions deliver again within 1.35 s of the path
  returning in ten of ten runs, with no duplicate frame and no charge left in
  flight. Remaining V03 cases already have unit coverage: random-tag
  establishment, delayed old packets and lane replacement in the handshake,
  reassembly and journal tests.
- Daemon: double-fork readiness exactly as section 12.1 describes, PID file,
  parent exit only after every TAP exists; a test that the parent waits while
  a TAP is missing and succeeds with no peer reachable. Done: `unquic start`.
  Its container test checks that the TAP exists when the parent returns with
  no peer reachable, that SIGTERM removes the TAP and PID file, and that a
  failure after the fork exits 1 with neither.
- Optional systemd unit template embedded in the daemon, with the exact
  `[Unit]`, `[Install]` and `[Service]` lines of section 12.1. No systemd
  linkage. Done: `unquic systemd-unit`, checked line by line in a unit test.

## Stage 5: measured optimisation

If evidence shows no further improvement is feasible at an earlier step,
record the blocker and proceed with the remaining stages. Return here to the
original targets: at least 20 Gbit/s each direction locally, stable goodput
under overload, and 5 Gbit/s over the specified lossy access path. Moving on
does not close those gates. Bounded queues and buffers may grow when controlled
measurements show a benefit; report memory, latency and overload behavior too.

Only with a profile showing the bottleneck. Candidates in the order the
design names them: reuseport steering, inner GSO representation as a
negotiated optional feature bit, single-flow striping with a bounded reorder
queue, FEC. Each one lands behind a config switch defaulting to off until its
`BENCH.md` row shows a win on the target CPU budget.

November 10 evidence for that list, all in BENCH.md:

- Single-flow striping: one flow stops near 1 Gbit/s per direction because one
  transmit worker carries it and saturates, even with `tolerate_random_loss`.
  On November 11 the same target reaches about 1.5 Gbit/s without striping.
  Sixteen-frame runs striped across lanes raise every median by 24–36% to
  about 1.95 Gbit/s but double reordering and missed their stated bar in one
  direction, so the switch was not kept
  ([BENCH.md](BENCH.md#striping-one-flow-across-lanes-november-11)). A
  profile on the remote host places the next single-flow limit downstream of
  the sender ([BENCH.md](BENCH.md#what-limits-one-flow-at-75-ms-november-11)).
- Lane placement for few flows: `flow_hash` uses fixed keys, so four flows
  land on two lanes in one direction and three in the other, every run, and
  leave workers idle. Load-aware assignment must still pin a flow while its
  frames are in flight, including when the kernel moves it between TAP queues.
  `daemon.balance_flows` does this with round-robin lanes and stays off by
  default. On the decision workload, against a bar stated first, every 2G
  direction rises 12–89% and every 5G row total 5–30% and weaker direction
  16–86%, and it was briefly the default. But the loopback overload check
  fell from 6.54 / 2.30 to 1.49 / 1.45 Gbit/s with no thread busy
  ([BENCH.md](BENCH.md#round-robin-lanes-by-default-november-11)): the window
  limit the RTT floor later removed. Re-evaluated after the floor
  ([BENCH.md](BENCH.md#balanced-lanes-after-the-window-floor-november-11)),
  the single-flow target rises 42–58% and decision medians 19%, but the
  loopback check and the formerly favoured decision directions still lose up
  to 14%, so it stays off. Its single-flow gain is incidental striping: tun
  steers a flow to the queue the daemon last wrote its reverse direction on,
  so with balanced lanes one flow's reads and sends spread over every worker.
  A deliberate version that keeps the strong direction is the next
  lane-placement candidate.
- FEC or a loss-agnostic response: at 5% random loss draft 07 collapses
  delivery below 11% of even a 0.5 Gbit/s offer. On November 12
  `daemon.loss_threshold_permille` (default 20, draft 07's 2%) landed: at 100
  one flow at 5% loss rises from under 10 Mbit/s to about 0.45 Gbit/s with a
  competing TCP flow's share unchanged, but costs 3% at 1% loss, so it stays
  at the draft's default
  ([BENCH.md](BENCH.md#a-configurable-loss-threshold-for-random-loss-november-12)).
  The 5% row still reaches under a tenth of its offer.
- Batching while application-limited: correctly marked light load stays in
  Startup and sends about 1.4 frames per claim, costing CPU at equal delivery.
  On November 12 the decision workload cost about 480% of a core per Gbit/s
  at 0.5G against 190–220% at 2G and 5G: every frame pays its own TAP read,
  grant round trip, send and TAP write, with about one frame per syscall
  ([BENCH.md](BENCH.md#cost-at-light-load-november-12)). Candidates: a short
  per-lane hold time, aggregation across lanes, or spendable credit that
  skips the grant round trip, each with its own latency or pacing bar. The
  hold landed on November 12 as `daemon.tx_hold_us`, off by default: at 200
  µs it cut CPU per Gbit/s at 0.5G by 5.1% (eight of eight pairs) with 5G
  unchanged, but one direction's missing frames rose from about 0.03% to
  0.4% through owner-buffer admission drops. Traces tied those to held
  data hiding idle moments from application-limited marking; counting a
  worker as supplying its group only from two datagrams (as Linux TCP
  ignores under one MSS) keeps the 5.5% gain and returns the drops to the
  control's range, though one run still missed 0.19% of a direction
  ([BENCH.md](BENCH.md#held-data-and-application-limited-marking)).
  `udp_load` now measures one-way delay: at 0.5G the hold adds about 33 µs
  at the median and 0.2 ms at p99, and nothing at 5G
  ([BENCH.md](BENCH.md#what-the-hold-costs-in-latency)). A rerun on the
  current build repeats the 6% saving but also a small loss: R→L missing
  median 0.145% against 0.065%, worst run 0.43%. A default change needs
  that loss removed. The same measurement found frames
  arriving seconds late at 5G overload: a worker owning a busy lane stopped
  reading its own TAP while other readers' handed-over frames filled its
  input queue, so tun's ring aged. Gating reads only on the worker's own
  inputs cut p99.9 from 0.1–0.4 s to 30–66 ms and the maximum from up to
  1.6 s to 161 ms, with throughput within noise
  ([BENCH.md](BENCH.md#frames-seconds-old-under-overload-november-12)).
  Most of the remaining overload delay is the kernel's TAP ring: on the
  loopback check 64 entries instead of 500 cut the median from 34–50 ms to
  4–8 ms with delivery unchanged. README recommends a short ring; whether
  the daemon should set one on the TAPs it creates is the user's call.
  Fixed per-turn work matters here too: sweeping only occupied receive slots
  for ACKs cut light-load CPU per Gbit/s by 4.1%
  ([BENCH.md](BENCH.md#scanning-only-occupied-receive-slots-for-acks-november-12));
  masking the TAP flush and deadline scans too gave a −2.4% median but missed
  its five-of-six bar and was rejected, and skipping the extra turn after
  handing sender messages to other workers changed nothing. Light-load pairs
  show a first-run advantage of about 3%, so light-load bars need alternating
  order and effects clearly larger than that.
- `tolerate_random_loss` by default: measured on November 11 against one TCP
  CUBIC flow at a 1 Gbit/s, 7.5 ms bottleneck
  ([BENCH.md](BENCH.md#coexistence-with-tolerate_random_loss-november-11)).
  With a one-BDP queue draft 07 and CUBIC share about evenly and the switch
  changes nothing; with a quarter-BDP queue it cut TCP's median by 20%. A
  queue check now also cuts on a loss round whose RTT samples rise more than
  max(min_rtt / 8, 250 µs): TCP keeps its share, the lossy target rises 4–26%,
  and the switch is on by default
  ([BENCH.md](BENCH.md#tolerating-random-loss-only-without-queueing-delay-november-11)).
  Checking each round's smallest RTT instead did not help. The 5% loss rows
  still need FEC or a loss-agnostic response.

## Stage 6: protocol freeze

Publish the wire vectors from 1a and 3 as fixed test files, resolve every
"open" row of section 16, repeat the full container suite, and record the
acceptance profile of section 14.2.

## What not to do

- Do not add a dependency to make a step easier. Write it in-tree.
- Do not add a reorder queue, FEC, ECN or kernel pacing before stage 5.
- Do not reconstruct connection state from CONFIRM or READY after a restart.
- Do not let a timer or ring reuse make an old packet number new again.
- Do not report readiness before every configured TAP exists, and do not
  make readiness wait for anything else.
- Do not put measurements from a particular machine in the design or README;
  they belong in `BENCH.md` with their hardware row.
