# Release transport overload investigation

The original receive-admission collapse and TAP readiness busy loop are fixed.
Compact transmit storage now keeps payloads in bounded source buffers and
slabs, with per-lane FIFOs and zero-copy ownership exchange. A rejected
global-queue prototype exposed a separate severe QinQ collapse; the FIFO
regression reproduces its frame-order inversion and verifies the fix.

The last 60-second QinQ sweep before shared sender grants delivered
7.76 / 16.70 Gbit/s at 21 Gbit/s requested each way, then 7.55 / 17.21
unlimited: a 2.7% forward decline and a 3.1% reverse increase.
Matched builds remain strongly asymmetric.
Earlier pre-ACK sweeps reached higher rates with another kernel build and TAP queue placement.
No workload proves a universal plateau or the theoretical CPU ceiling.

Neither the local **20 Gbit/s in each direction** gate nor the real
**5 Gbit/s under loss** gate has passed. Bounded runtime recovery is now connected;
complete congestion-control integration remains unfinished. Workers use the shared draft-07 controller and grant
accounting. Refreshing overtaken packet numbers removes repeated coordinator
DATA cancellations. Skipping empty loss-history ranges reduces CPU cost, but the
larger bounded DATA burst, retained packet-plan refill and earlier local feedback
raise the latest sixty-second QinQ sweep to 2.41–2.91 Gbit/s per direction
using about 2.2–2.4 CPUs per daemon. A subsequent 0.5G-to-unlimited sweep
still falls 17.2% forward from its 4G peak; both directions stay under 1%
missing only through the 2G offered point.
The preceding ACK-pruning sweep delivered 1.12 to 1.22 Gbit/s. These remain
severe integration regressions, far below earlier forwarding rates.
General application-limited detection remains open. The rates above predate
runtime payload retention and repair; they are not recovery-build measurements.
The recovery build regresses further: the matched sixty-second unlimited run
falls from 2.38 / 2.50 to 2.16 / 1.79 Gbit/s and introduces admission pressure.
Its ascending sweep falls 21.9 / 14.4% from peak to unlimited. Moving deadline
checks before source expansion does not restore throughput: the next
sixty-second unlimited point delivers 1.78 / 1.68 Gbit/s. The subsequent
fresh/repair fairness fix reaches 2.07 / 1.70 unlimited. Preventing ordinary
loss starvation under continuous ACK input then measures 1.82 / 1.64 unlimited;
admission pressure and overload declines remain.
PMTU discovery is active in the latest rate sweeps and
passes routed reduction/recovery tests, including filtered ICMPv6 PTB.
Missing recovery or congestion control does not explain away the remaining
scheduling and admission problems.

## Current performance gaps

The sender integration passes forwarding/restart and allocation checks,
plus control-pressure, stale-feedback and bounded transmit-expiry regressions.
The recovery build adds successful two-repair delivery checks, but its latest
rate sweeps reveal a new regression and nonzero reassembly admission pressure.
The initial sender-integration sweep already showed low CPU use and poor
throughput with zero admission pressure or outer socket overflow. Recovery then
introduced admission pressure; that observation does not explain the earlier
regression. Grant/feedback scheduling and recovery pressure need separate
attribution. Moving expiry checks before source expansion establishes no general
throughput gain. Earlier storage/batching sweeps predate ACK emission.
Sender-accounting and I/O-timestamp changes have correctness coverage and
post-integration rate sweeps, but no matched comparison isolates their costs.
Frame-view allocations now follow actual per-worker quotas instead of reserving
8192 slots on every worker. This reduces reserved metadata; it does not establish
a goodput gain or change the session retention limits.

The storage/batching results below use eight workers per daemon,
16 flows per direction, 500-entry TAP queues, and a 1452-byte outer UDP cap.
They keep active discovery enabled. L→R / R→L pairs are simultaneous.
The JSON records actual offered load, CPU and counters for each observation;
earlier tables retain their original builds and test conditions.

| Requirement | Current evidence | Remaining gap |
| --- | --- | --- |
| At least 20 Gbit/s each way at inner MTU 1500 | After the ACK/loss fairness fix, sixty-second QinQ sweep: 1.82 / 1.58 Gbit/s paced, 1.82 / 1.64 unlimited. Earlier pre-integration expiry build: 7.76 / 16.70 paced | Severe integration regression; paced missing fractions are 91.26 / 92.28%. The recovery ascending sweep stays under 1% missing in both directions only at 0.5G offered. Five successful 20G repeats remain unmet. |
| Goodput must plateau under excess traffic | The latest ascending sweep falls 11.73 / 15.58% from each direction's peak under unlimited load. Its sixty-second repeat remains near 1.82 / 1.64 Gbit/s and uses about 2.1 / 2.0 cores | No stable plateau or CPU ceiling is established. Source-expiry work, incomplete reassembly and grant/feedback scheduling require separate attribution. |
| Small-frame overload must remain efficient | Earlier pre-recovery tagged 64-byte runs range from 2.65–3.69M to 4.74–5.28M frames/s per direction with different placements. QinQ 128-byte reverse goodput falls from 2.48M to 2.19M under unlimited load | Worker utilization is uneven and placement-sensitive. High loss persists; mixed traffic, non-coalescible frames and repeated matched placement remain unverified. |
| VLAN-heavy deployment | Single- and double-tag output coalescing works. Earlier pre-recovery sixty-second QinQ points have zero reordering; one earlier refill point has thirteen reordered frames (0.0003125%). Single-tag unlimited traffic reports 32–36%. SRv6 reaches 5.24 / 5.98 Gbit/s unlimited | QinQ remains asymmetric. Single-tag lane migration/reordering remains a concern. SRv6 does not show a general throughput gain from compact storage. TCP input offloads remain unfinished. |
| Establish the CPU-limited forwarding ceiling | Process/thread CPU, source reads, TAP writes, overflow and queue counters are recorded. A sampled profile found retained foreign buffers starving a reader while owners held free buffers | No theoretical or measured zero-loss CPU ceiling is established. Socket placement, scheduling, kernel cost and inner-generator limits remain separate variables. |
| Prevent split-count amplification of outer loss | A release runtime regression loses each direction's original frame prefix and first repair, delivers via the second repair, then replays the original without duplicate TAP delivery or hot allocations | Sustained independent/burst loss and the real-path target remain unverified. A frame can still expire or exhaust its two-repair allowance. |
| Use larger confirmed path MTUs | Discovery runs up to 8952-byte UDP payloads. Linux 6.8 and 7.0 tests cover PTB, silent filters, shared sessions, external TAP moves/renames, and reconnect. The both-kernel routed tests confirm 9000→1500→1280→9000 without outer fragments | Throughput sweeps currently cap outer packets at 1500 bytes. Larger-MTU throughput, real routed-path validation and DATA-quoted PTB processing remain open. |
| At least 5 Gbit/s over routed IPv6 10G-EPON, RTT 4.5–7.5 ms, with nonzero loss | These throughput sweeps are local container tests without injected impairment | No real-path or independent/burst-loss result verifies the target. Recovery is connected; sustained lossy-path performance and complete congestion-control integration remain unverified. |

Unlimited loss remains high. TAP ingress drops, bounded worker queue drops,
outer socket overflows, expired frames and inner UDP receive-buffer errors
are reported separately. Their units differ: a dropped source SKB can contain
many frames, so these counters cannot be summed into an exact frame-loss total.

Stage 5 must return to the original performance targets after work on later
stages. No performance gate is waived.

## Recovery pressure diagnostics and kernel recheck (November 7)

Counters now partition `expired_frames` into unmaterialized source children,
materialized unsent/partial/full transmit frames, incomplete receive frames,
and completed frames awaiting TAP. The six subsets retain the existing total
and publish at worker, session and lane scope. Worker-only counters also report
cleanup budget exhaustion and peak queue depth, report-staging continuations,
and mapping-capacity versus changed-selection rejection. Updates add no clocks,
allocations, syscalls or scans. Tests verify exact classification, publication,
reset and repeated-cleanup behavior. The diagnostic build passes 345 all-feature
and 336 default release tests, Clippy, 30 targeted kernel checks and the
two-loss relay. Throughput measurements with these counters are pending.

The broader recheck found a post-address-migration forwarding timeout at
`tests/linux_runtime.rs:637` on both Linux 6.8 and Linux 7.0. The affected test
sends one frame after both peers publish newer generations; that frame did not
arrive within two seconds. A focused Linux 7.0 repeat passes, so the cause is
not yet established. Do not treat the full runtime matrix as passed. Published
generations prove installation-barrier completion but do not reset the shared
path group's congestion state or backed-off PTO. Whether that gate caused the
loss still needs evidence.

The corrected Linux 7.0 fixture passes 66 library checks, foundation, short
PMTU, external rename/move, two-loss recovery, CLI and generator tests. The
first fixture attempt omitted loopback setup inside its private namespace;
its address-binding failures were harness errors. The diagnostic Linux 6.8
build passes 68 library checks, foundation, short PMTU, rename/move and two-loss
recovery before the same migration timeout. Long routed PMTU checks are
running independently on both kernels.

## ACK admission and ordinary loss fairness (November 7)

Continuous duplicate ACK input could indefinitely postpone ordinary loss
detection, even after an accepted ACK proved a packet eligible for repair.
A deterministic worker regression reproduces this on `2765533`. ACK admission
now waits for the accepted batch and its resulting finite loss passes. A scan
already in progress finishes before a fresh pass applies newer ACK evidence.
The fix adds a per-lane pass count and an aggregate count, with no allocation
or new full-table scan. Accepted ACK transactions still finish before loss.

The regression covers multi-report ACKs, accepted ACKs on another lane, delayed
report returns, ordinary/explicit loss-scan crossover, resumption of waiting ACKs,
and generation replacement after an empty-history scan. All 25 sender kernel
checks and the two-loss/late-original runtime relay pass, along with 344
all-feature and 335 default release tests, Clippy and formatting.

[Twelve release observations](benches/tap-loss-fence.json) retain the preceding
QinQ profile, generator and placement. The ascending twenty-second sweep is:

| Requested Gbit/s each way | Delivered Gbit/s | Missing after drain % | Daemon CPU % |
| --- | ---: | ---: | ---: |
| 0.50 | 0.49 / 0.49 | 0.94 / 0.78 | 53.27 / 48.84 |
| 1.00 | 0.98 / 0.96 | 0.06 / 1.75 | 100.10 / 94.42 |
| 2.00 | 1.79 / 1.95 | 9.62 / 1.61 | 194.50 / 178.97 |
| 3.00 | 1.82 / 1.71 | 38.09 / 42.18 | 211.94 / 201.08 |
| 4.00 | 2.04 / 1.65 | 47.99 / 58.00 | 223.38 / 211.49 |
| 8.00 | 1.79 / 1.62 | 77.33 / 79.47 | 212.04 / 203.60 |
| 16.00 | 1.77 / 1.57 | 88.77 / 89.96 | 208.67 / 200.00 |
| 21.00 | 1.72 / 1.53 | 91.48 / 92.22 | 203.97 / 194.82 |
| Unlimited | 1.80 / 1.64 | 96.85 / 97.01 | 209.10 / 203.12 |

Unlimited delivery is 11.73 / 15.58% below each direction's observed peak.
Only the 0.5G point keeps both missing fractions below 1%. The sixty-second
sequence retains the same daemon processes across all three points:

| Requested Gbit/s each way | Delivered Gbit/s | Missing after drain % | Daemon CPU % |
| --- | ---: | ---: | ---: |
| 21.00 | 1.82 / 1.58 | 91.26 / 92.28 | 212.34 / 205.66 |
| Unlimited | 1.82 / 1.64 | 96.85 / 97.11 | 209.62 / 202.64 |
| 21.00 | 2.23 / 1.80 | 89.26 / 91.34 | 230.64 / 218.50 |

The unlimited point is 12.08 / 3.88% slower than the preceding batch-fairness
build's 2.07 / 1.70 Gbit/s. The return point improves, showing substantial
within-sequence variation. This correctness fix establishes neither a general
throughput gain nor an overload plateau.

At sixty-second unlimited load, endpoint L/R reports 6,424,733 / 6,202,688
TAP ingress SKB drops, 5,123 / 71,885 worker queue drops (including 717 / 481
ACK-buffer drops), 3,597,801 / 5,475,889 expired frames, 38,679 / 7,047
admission-pressure events and 362 / 238 submitted repair records. All twelve
observations have zero outer socket overflow, inner UDP receive errors,
corrupt or duplicate inner records and outer fragmentation. Complete-frame
reordering peaks at 0.00424%.

Expiry currently merges unmaterialized source children, materialized transmit
frames and receive frames. It cannot establish how many incomplete receive
slots resulted from sender abandonment. Bounded cleanup and coverage staging
can also span turns before buffers or shared inflight charges are released;
their contribution remains unmeasured. The next diagnostic separates expiry
classes and counts actual cleanup, staging and mapping-pressure events.
Missing FRAME_ABANDON is a verified orphan-slot mechanism, but its measured
contribution is still unknown.

## Bounded payload recovery integration (November 7)

The release runtime now retains submitted payloads and selectively repairs
missing byte ranges, with at most two repairs per byte. It reuses the existing
coverage state, piece journal and shared grant/accounting path. Packet mappings
are reserved before I/O; partial sends commit only their successful prefix.
ACK and retirement coverage changes finish before sampling handles can be reused.
Loss keeps mappings available for late original ACKs. Frame cleanup and report
processing have fixed work budgets, and repairs retain the original frame ID,
lane and deadline.

The new bidirectional runtime regression drops each target frame's original
prefix and first repair, then requires delivery through the second repair.
Replaying the original afterward must not duplicate TAP delivery. The same
fixture on `91c28ac` completes handshake and warmup but times out waiting for the
lost frames; the recovery build passes with zero hot-path allocations or
deallocations. Separate worker tests cover packed mapping budgets, late ACK
cancellation, partial sends, fresh-data FIFO order under mapping pressure,
reserved repair capacity and connection cleanup.

The build passes 344 all-feature and 335 default-feature release tests, both
Clippy configurations, formatting, and 69 short Linux 6.8 kernel/runtime checks.
These include external TAP renames and namespace moves, peer restart, a
bidirectional frame storm, the CLI and load generator. The three-session stress
fixture now explicitly permits 512 MiB because its recovery storage exceeds the
unchanged 256 MiB default. A separate accounting regression verifies that the
normal eight-worker/eight-lane configuration fits the default and that larger
configurations require an explicit ceiling.

The release daemon SHA-256 is
`d142c0b4d699c0c1f0d13e8cd99e4057c972c631ac5e68bd888d7f289a058f2b`.
The rate sweeps below expose a regression. Sustained independent/burst-loss
tests, the long routed-PMTU test and the Linux 7.0 matrix remain to be rerun on
this build. Fresh-data journal pressure can still request an unusable grant
before yielding it; profiling must determine its cost.
The `repairs` counter now counts successfully submitted repair DATA records.
Sender expiry can occur after peer TAP delivery if feedback is lost, so expiry
counts cannot be treated as an exact count of missing inner frames.

## Recovery-build overload regression (November 7)

[Fifteen release observations](benches/tap-runtime-recovery.json) compare
`d9867fa` with the preceding production binary. The profile remains eight
workers, sixteen flows each way, QinQ, 1522-byte frames, 44-segment input batches,
500-entry TAP queues and active discovery capped at 1452-byte outer UDP payloads.
No compilation or other tests overlapped measurement. CPU 100% means one core;
goodput excludes drain, while missing fractions and counter deltas include it.

| Requested Gbit/s each way | Delivered Gbit/s | Missing after drain % | Daemon CPU % |
| --- | ---: | ---: | ---: |
| 0.5 | 0.49 / 0.49 | 0.01 / 0.08 | 50.89 / 47.77 |
| 1 | 0.99 / 0.97 | 0.04 / 1.81 | 97.48 / 94.06 |
| 2 | 1.91 / 1.88 | 3.05 / 4.26 | 195.10 / 181.63 |
| 3 | 1.81 / 1.71 | 38.91 / 42.59 | 213.86 / 199.02 |
| 4 | 1.85 / 1.71 | 52.84 / 56.56 | 219.59 / 211.15 |
| 8 | 2.13 / 1.72 | 72.48 / 77.82 | 220.54 / 212.09 |
| 16 | 1.89 / 1.74 | 87.65 / 88.63 | 214.88 / 206.51 |
| 21 | 2.33 / 1.93 | 88.72 / 90.69 | 232.44 / 222.52 |
| Unlimited | 1.82 / 1.65 | 96.81 / 96.91 | 213.99 / 208.85 |

Unlimited goodput falls 21.9% forward and 14.4% reverse from the 21G point.
Only the 0.5G point keeps both missing fractions below 1%.

The matched sixty-second sequences use the same generator and harness, with
baseline first and recovery second. Each sequence keeps its daemons and sockets
through 21G, unlimited, then 21G again.

| Build / requested Gbit/s | Delivered Gbit/s | Missing after drain % | Daemon CPU % | Admission pressure L/R |
| --- | ---: | ---: | ---: | ---: |
| Baseline / 21 | 2.40 / 2.61 | 88.40 / 87.41 | 229.45 / 220.63 | 0 / 0 |
| Baseline / Unlimited | 2.38 / 2.50 | 95.76 / 95.41 | 220.81 / 214.47 | 0 / 0 |
| Baseline / 21 return | 2.38 / 2.65 | 88.60 / 87.31 | 228.32 / 220.83 | 0 / 0 |
| Recovery / 21 | 1.96 / 1.75 | 90.50 / 91.55 | 223.67 / 214.07 | 43,608 / 12,128 |
| Recovery / Unlimited | 2.16 / 1.79 | 96.18 / 96.79 | 229.63 / 218.57 | 42,265 / 9,850 |
| Recovery / 21 return | 2.13 / 1.82 | 89.77 / 91.28 | 232.66 / 220.00 | 44,909 / 10,695 |

The longer recovery sequence does not fall under unlimited load, but its
unlimited goodput is 9.1 / 28.6% below the matched baseline. This does not
establish an overload plateau.

At the recovery sixty-second unlimited point, endpoint L/R reports
6,255,453 / 6,062,004 TAP ingress SKB drops, 4,100 / 72,212 worker queue drops
(including 79 / 55 ACK-buffer drops), 2,368,700 / 4,811,904 expired frames and
42,265 / 9,850 admission-pressure events. Only 472 / 162 repair DATA records
were submitted. All fifteen observations have zero outer socket overflow,
inner UDP receive errors, corrupt or duplicate inner frames and outer IPv6
fragmentation. Some recovery observations reorder complete frames; the JSON
retains these counts.

Two mechanisms are verified from code. The sender shortens each materialized
frame's deadline using RTT, while its queued source still lives for 100 ms.
It can therefore expand and checksum GSO children already past their shorter
deadline. Also, sender expiry frees local payloads without emitting
FRAME_ABANDON; a partially transmitted frame can retain a receiver slot until
the receiver's independent 100 ms deadline. The amount each mechanism contributes
to measured loss and pressure is not yet isolated. Duplicate-piece counts
include local retries of packed datagrams after pressure relief, so they cannot
be attributed wholesale to network repair.

The first ascending attempt exited with ConnectionRefused before producing a
complete dataset. Its failure logs were not retained, leaving the cause
unconfirmed. The table reports the complete retry; subsequent wrappers retain
artifacts on failure before deleting their containers.

## Reject expired sources before expansion (November 7)

The transmit owner now applies the RTT lifetime once before admitting a source.
All expanded children inherit it. Previously, the source could remain queued
for 100 ms while each materialized child received a shorter deadline and was
discarded after header/checksum work. Regressions fail on `d9867fa` and pass
with the fix, covering plain/GSO input, local/foreign ownership, queued children,
RTT changes and exact buffer-credit return. The change passes 344 all-feature
and 335 default-feature release tests, both Clippy configurations, formatting
and four targeted kernel checks.

[Twelve release observations](benches/tap-source-deadline.json) use the same
profile as the recovery-build comparison. They establish no general throughput
gain. The source fix does not resolve admission pressure or the overload decline.

| Requested Gbit/s each way, 20 seconds | Delivered Gbit/s | Missing after drain % | Daemon CPU % |
| --- | ---: | ---: | ---: |
| 0.5 | 0.49 / 0.49 | 0.82 / 0.01 | 50.07 / 46.91 |
| 1 | 0.97 / 0.98 | 1.61 / 0.85 | 100.39 / 93.15 |
| 2 | 1.95 / 1.95 | 1.79 / 1.55 | 196.29 / 175.80 |
| 3 | 2.26 / 1.81 | 23.35 / 38.99 | 230.76 / 212.86 |
| 4 | 1.98 / 1.68 | 49.70 / 57.33 | 222.59 / 211.84 |
| 8 | 2.16 / 1.84 | 72.57 / 76.65 | 231.58 / 220.45 |
| 16 | 2.31 / 1.83 | 85.31 / 88.37 | 235.63 / 221.47 |
| 21 | 1.87 / 1.73 | 90.94 / 91.62 | 219.99 / 208.71 |
| Unlimited | 2.02 / 1.72 | 96.49 / 96.78 | 221.17 / 212.61 |

| Requested Gbit/s each way, 60 seconds | Delivered Gbit/s | Missing after drain % | Daemon CPU % | Admission pressure L/R |
| --- | ---: | ---: | ---: | ---: |
| 21 | 1.90 / 1.81 | 90.91 / 91.31 | 222.69 / 213.07 | 36,958 / 14,038 |
| Unlimited | 1.78 / 1.68 | 96.85 / 96.86 | 220.28 / 203.99 | 41,966 / 7,010 |
| 21 return | 2.04 / 1.72 | 90.21 / 91.76 | 224.10 / 213.37 | 42,143 / 9,043 |

The sixty-second unlimited point has thirteen inner UDP receive-buffer errors
at the right receiver; all other inner error deltas are zero. All twelve points
have zero outer socket overflow, outer fragmentation, corrupt or duplicate
inner frames. Reordering and all drop counters remain in the JSON. The daemon
SHA-256 is
`95a130265740033ec8e6cd906f2f429abc5f81c3b9cd86c628b02edde5957ac4`.

Further source inspection identifies a separate starvation path: class selection
runs only when a transmit batch becomes empty. Continuous refill can keep a
fresh batch nonempty while repairs wait, or vice versa. This needs its own
regression and controlled measurement; the deadline results do not isolate it.

## Bound fresh/repair batch service (November 7)

Continuous refill could keep a fresh batch nonempty forever, preventing queued
repairs from being selected. The same problem affected fresh frames behind
continuous repairs. A worker regression reproduces the fresh-data case on
`34c43c4`: after 129 actual UDP transmissions, the waiting repair has not
been submitted. The fix stops refilling a nonempty batch while the opposite
class waits. Existing frames, live plans and partial-grant suffixes drain before
the normal class switch.

The regression now passes in both directions, with zero expiry or grant
cancellation. The change passes 344 all-feature and 335 default release tests,
Clippy, formatting, 23 sender kernel tests and the end-to-end two-loss/late-original
recovery test. No buffer, deadline or grant limit was enlarged.

[Twelve release observations](benches/tap-repair-fairness.json) retain the same
QinQ profile. The defect is reproduced and fixed, but throughput remains far
below target and the measurements establish no general performance gain.

| Requested Gbit/s each way, 20 seconds | Delivered Gbit/s | Missing after drain % | Daemon CPU % |
| --- | ---: | ---: | ---: |
| 0.5 | 0.49 / 0.49 | 0.95 / 0.87 | 51.43 / 48.18 |
| 1 | 0.97 / 0.98 | 1.79 / 0.92 | 101.04 / 93.42 |
| 2 | 1.93 / 1.94 | 2.65 / 1.83 | 200.15 / 180.69 |
| 3 | 1.83 / 1.78 | 37.89 / 40.02 | 218.89 / 203.21 |
| 4 | 2.18 / 1.83 | 44.54 / 53.57 | 229.99 / 219.89 |
| 8 | 2.09 / 1.66 | 73.40 / 79.00 | 224.85 / 213.86 |
| 16 | 2.22 / 1.85 | 85.89 / 88.21 | 229.75 / 221.83 |
| 21 | 2.31 / 1.68 | 88.84 / 91.86 | 230.05 / 218.07 |
| Unlimited | 2.08 / 1.79 | 96.39 / 96.84 | 223.96 / 213.91 |

| Requested Gbit/s each way, 60 seconds | Delivered Gbit/s | Missing after drain % | Daemon CPU % | Admission pressure L/R |
| --- | ---: | ---: | ---: | ---: |
| 21 | 2.04 / 1.73 | 90.20 / 91.69 | 223.43 / 217.39 | 34,645 / 10,745 |
| Unlimited | 2.07 / 1.70 | 96.44 / 96.98 | 222.66 / 214.68 | 43,811 / 8,259 |
| 21 return | 2.14 / 1.74 | 89.75 / 91.68 | 228.39 / 217.55 | 50,609 / 10,386 |

Unlimited goodput in the ascending sweep is 10.0 / 8.0% below each direction's
peak. The sixty-second unlimited point submits 979 / 126 repair DATA records,
versus 136 / 81 in the preceding source-deadline run, but admission pressure
remains high. All twelve points have zero socket overflow, inner UDP receive
errors, corrupt or duplicate inner frames and outer fragmentation. Complete-frame
reordering and per-worker drop counters are retained in the JSON.

The daemon SHA-256 is
`1b647b8e47a5ed3b363cfd40a1fd39bf748607f7b93d0392df259bb09d8d012b`.
A separate review found that continuous ACK admission can defer ordinary loss
scans. That hypothesis needs a distinct regression and measurement.

## Sender integration verification (November 7)

DATA, PMTU and liveness probes now request shared pacing/window grants.
The earlier throughput tables measured the runtime before this integration;
they do not measure its cost. The first post-integration sweep below records
a severe regression.

Initial release integration tests exposed small-burst stalls: one run delivered
18 of 32 frames before the remaining sender frames expired. Admission pressure,
socket overflow and worker queue drops were zero in that observation.
Code inspection and targeted regressions found that blocked PMTU requests could
retain a worker's sole flight buffer, and held ordinary requests could prevent
PTO probes from running. The owner now returns blocked PMTU requests and preempts
ungranted ordinary work when dispatching a PTO probe. Tests also cover partial
commit continuation, bounded ACK processing, control packets overtaking deferred
DATA, and connection-generation changes. These fixes close specific scheduling
defects; they do not establish an overload plateau.

The release runtime test now passes its 512-frame burst/USO allocation checks,
a ten-second mixed-frame storm and reconnection. One storm observation injected
1,294,336 frames each way and received 475,906 / 522,958, with no payload
corruption or duplicate delivery. Worker queue-drop counters were
181,796 / 176,203. This is a correctness stress test with mixed frame lengths,
not a controlled goodput or CPU measurement; its losses remain substantial.
No throughput or CPU improvement follows from this stress test.

The foreign-rename/namespace-move test also passes after waiting for new-generation
ACK progress. In one release run that took 5.35 seconds after carrier returned:
the unchanged path group retains PTO backoff after the remote daemon's outage.
The test bounds this wait and then checks both frame sizes in both directions.
A new handshake does not itself reset congestion or PTO state.

Routed discovery also exposed a probe-sizing deadlock after silent MTU reduction:
a 5140-byte candidate fit the pacing burst but exceeded the 5120-byte congestion
window. It could never receive permission to send. Candidates now fit both the
burst and the total window, without raising the confirmed DATA limit. A regression
reproduces this exact mismatch and verifies that the resized probe receives a grant.
The release routed test then passed 9000 → 1500 → 1280 → 9000, including
returned and filtered PTB, the normal upward retry and no outer fragmentation.
Default-feature tests, the all-feature release suite, kernel regressions,
Clippy and the Rust 1.89 compatibility check pass. These checks establish correctness for the
tested cases, not a throughput improvement.

Further controlled release rate sweeps, unlimited-load profiling, general
application-limited detection and runtime payload repair remain required.
Grant issuance, expiry and cancellation counters now distinguish send
permission failures from admission pressure, queue drops and socket overflow.

The loss-before-PTO barrier now waits for send commits, every worker's bounded
loss scan and the final report return. Third-expiry forced loss settlement also
finishes before probes. Saturated handoffs retain per-destination message order.
The combined barrier and rejected-grant fix pass 322 release tests, 51 short
kernel tests and the routed 9000 → 1500 → 1280 → 9000 test (651.69 seconds),
plus Clippy and Rust 1.89 checks. The routed test verifies no outer fragments.
One reconnect needed 28.90 seconds after carrier returned before new-generation
ACK progress; that service delay remains a gap despite the regression test passing.

## First paced sender sweep (November 7)

[Four release observations](benches/tap-sender-integration.json) measure
`c999431` with eight workers per daemon, sixteen flows each way, QinQ,
1522-byte frames, 44-segment generator batches, 500-entry TAP queues and a
1452-byte outer UDP ceiling. Each point lasts twenty seconds; the same daemons
and sockets persist through the sweep. Builds and other tests were stopped
during measurement.

| Requested Gbit/s each way | Actual offered Gbit/s | Delivered Gbit/s | Missing after drain % | Daemon CPU % |
| --- | ---: | ---: | ---: | ---: |
| 16 | 15.95 / 15.94 | 0.69 / 0.67 | 95.46 / 95.59 | 164.59 / 156.73 |
| 21 | 20.95 / 20.84 | 0.76 / 0.70 | 96.29 / 96.56 | 169.40 / 163.17 |
| Unlimited | 59.51 / 53.62 | 0.75 / 0.71 | 98.71 / 98.65 | 170.55 / 167.50 |
| 16 return | 15.98 / 15.93 | 0.73 / 0.73 | 95.29 / 95.35 | 167.57 / 171.58 |

This is a severe integration regression, not a CPU-limited plateau. Earlier
sixty-second expiry runs reached 7.76 / 16.70 Gbit/s at 21G requested, but other
sender changes and duration differ, so that comparison does not isolate a cause.

All four points have zero admission pressure, outer socket overflow, inner UDP
receive errors, invalid records, duplicates, reordering and outer fragmentation.
At 21G, endpoint L/R counters report 770,589 / 757,044 TAP TX drops,
111,842 / 228,251 expired frames, and 5,890 / 14,609 worker queue drops.
Of 164,797 / 167,474 received send grants, 1,047 / 865 expired and
14,533 / 19,015 were canceled. These units differ; they cannot be summed into
an exact frame-loss total. Final pacing gauges were sampled after draining and
must not be used as steady-load controller rates.

## Refresh overtaken sender identities (November 7)

[The matched release sweep](benches/tap-sender-refresh.json) uses the fairness
build plus local refresh of unsent packet numbers. Feedback keeps its scheduling
priority. DATA headers, History reservations and sampler identities move together
under the original grant; payloads, byte limits and deadlines stay unchanged.
History pressure returns the unused grant and retains the frame for retry.

Three regressions fail with the previous send path and pass with refresh:
steady feedback, a pacing-held request, and a permit behind a full bounded
message turn. Further tests cover trimmed grants with actual ACK accounting,
probe reply identity, pressure retry, validation before mutation and packet-number
exhaustion. The build passes 332 release tests, default-feature tests, Clippy,
Rust 1.89 and 58 container tests. Routed PMTU confirms every lane through
9000→1500→1280→9000 in 650.77 seconds, including returned and filtered PTB,
the normal 600-second upward retry and zero outer fragmentation.
Foreign-move reconnect ACK readiness takes 22.59 seconds; this delay remains open.

| Requested Gbit/s each way | Actual offered Gbit/s | Delivered Gbit/s | Missing after drain % | Daemon CPU % |
| --- | ---: | ---: | ---: | ---: |
| 16 | 16.00 / 15.96 | 0.73 / 0.80 | 95.22 / 94.74 | 174.72 / 152.70 |
| 21 | 20.98 / 20.97 | 0.83 / 0.80 | 95.93 / 96.11 | 160.05 / 150.62 |
| Unlimited | 60.51 / 57.34 | 0.84 / 0.81 | 98.58 / 98.55 | 161.42 / 151.68 |
| 16 return | 15.99 / 15.98 | 0.83 / 0.79 | 94.70 / 94.91 | 158.98 / 152.15 |

At 21G, coordinator cancellations fall from 33,305 / 30,204 to 1 / 1.
Total grants are 142,549 / 139,904, with 1,147 / 650 expired and 10 / 9 canceled.
This verifies the cancellation fix, but goodput does not improve: the previous
fairness build delivered 0.86 / 0.83 Gbit/s. Cancellation alone therefore does
not explain the remaining sender regression.

TAP TX drops are 769,134 / 762,668, frame expiries 8,482 / 16,742, and worker
queue drops 234 / 7,151 at 21G. All four points have zero admission pressure,
outer socket overflow, inner UDP errors, invalid records, duplicates, reordering,
out-of-window/wrong-run records and outer fragments. Counts have different units
and cannot be added into an exact loss total. Placement, duration, load-generator
binary and release options match the fairness sweep; no builds or tests overlap
measurement. Grant scheduling and controller limitation still need attribution.

## Larger burst after identity refresh: still an experiment (November 7)

[The normal-release cap64 candidate](benches/tap-sender-refresh-cap64.json)
changes only the three DATA burst limits from 16 to 64 maximum datagrams.
BBR's runtime quantum still caps the combined outstanding allowance at 64 KiB.
Packet MTUs, congestion windows, grant lifetimes and buffer sizes are unchanged.
The candidate passes 332 release tests, Clippy and 57 short container/probe tests.

| Requested Gbit/s each way | Actual offered Gbit/s | Delivered Gbit/s | Missing after drain % | Daemon CPU % |
| --- | ---: | ---: | ---: | ---: |
| 16 | 15.99 / 15.90 | 1.60 / 1.49 | 89.81 / 90.46 | 184.17 / 173.08 |
| 21 | 20.99 / 20.73 | 1.37 / 1.56 | 93.34 / 92.32 | 195.89 / 162.43 |
| Unlimited | 59.42 / 52.31 | 1.09 / 1.58 | 98.14 / 96.93 | 201.68 / 162.67 |
| 16 return | 15.97 / 15.90 | 1.55 / 1.52 | 90.14 / 90.30 | 191.00 / 174.63 |

At 21G, goodput rises from the cap16 build's 0.83 / 0.80 Gbit/s to
1.37 / 1.56. Forward unlimited goodput then falls 20.7% to 1.09 Gbit/s,
while reverse remains 1.58; the return point recovers. This does not satisfy
the overload requirement. The larger cap was not adopted at this point;
the later indexed-loss-scan comparisons below support adopting it.

At 21G, TAP TX drops are 748,870 / 722,772, frame expiries 58,425 / 14,648,
and worker queue drops 3,174 / 14,541. Unlimited frame expiries rise to
119,500 / 30,647. All points have zero admission pressure, outer socket overflow,
inner UDP errors, invalid/duplicate/out-of-window/wrong-run records, reordering
and outer fragments. Left worker 2 rises from 39.6% to 65.5% CPU under unlimited
load while its successful outer bytes fall from 826 MB to 426 MB. Its expired
grants rise from 1,670 of 23,090 to 1,764 of 11,187. This motivates checking
deadline and loss-scan work; it does not establish their contribution.

## Controller timing diagnostic (November 7)

[Two instrumented 60-second 21G points](benches/tap-sender-controller-diagnostic.json)
deliver 1.60 / 1.54 and 1.53 / 1.48 Gbit/s, using 188 / 180 and 189 / 180%
daemon CPU. They use cap64 and temporary fixed counters, clock reads and 1Hz
monitor snapshots. The [reproduction patch](benches/sender-controller-diagnostic.patch)
applies to `13cdebd`; these are not normal-release acceptance results.

Across the load intervals, issuance is open 81–86% of the time, closing takes
10–15%, committing takes under 0.5%, and feedback takes 3–4%. Open time includes
pacing and congestion-window waits. Attempt counters identify both, with no
sampling-pool blockage; they are not duration measurements. Successful grants
take 17–18 microseconds to reach the pre-send timestamp, then 36–42 microseconds
to reach owner completion observation, including the send syscall. ACK arrival
to owner application averages 68–92 microseconds. Raw sample send/ACK intervals
average 445–457 / 476–492 microseconds; these intervals are not RTT.
Sampled model bandwidth ranges from 1.29 to 8.60 Gbit/s of outer IPv6 traffic, and
median sampled congestion windows are about 100–114 KiB. Neither the controller
rate nor these timings identify a CPU ceiling.

The diagnostic exposes a separate startup bug: `Owner::new` initialized BBR
at time zero, so the first real-clock ACK could enter ProbeRTT immediately.
The first transition occurs after only 3,840 / 2,560 delivered outer bytes.
Both controllers return to Startup before DATA begins, with the full-bandwidth
flag clear. This contradicts the proposed pre-load plateau explanation for
these runs. The constructor now uses the runtime's monotonic clock; a regression
at zero and one-day uptime fails before that fix and passes after it. No
throughput gain is attributed to the clock correction. The fix passes 333
release tests, Clippy and 56 short container tests.

## Sparse loss-scan diagnostic (November 7)

A completed ACK schedules a fresh loss pass from the retained history floor.
Even one outstanding tail packet can require walking roughly 1,024 bitmap words
over 16 worker turns. [A separate instrumented sweep](benches/tap-sender-loss-scan-diagnostic.json)
counts that work using the controller diagnostic plus
[fixed scan counters](benches/sender-loss-scan-diagnostic.patch).

| Requested Gbit/s each way | Delivered Gbit/s | Missing after drain % | Daemon CPU % | Bitmap reads, millions | Empty reads % | Scan continuations |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 16 | 1.61 / 1.50 | 89.54 / 90.17 | 191.83 / 178.96 | 40.35 / 33.48 | 98.58 / 98.64 | 608,624 / 503,589 |
| 21 | 1.24 / 1.27 | 93.97 / 93.84 | 162.48 / 164.99 | 31.20 / 28.26 | 98.57 / 98.34 | 470,298 / 423,124 |
| Unlimited | 1.20 / 1.43 | 97.98 / 97.48 | 196.73 / 168.02 | 45.01 / 38.58 | 98.49 / 98.62 | 682,577 / 581,146 |
| 16 return | 1.58 / 1.49 | 89.91 / 90.48 | 193.07 / 179.60 | 38.67 / 38.68 | 98.60 / 98.68 | 583,677 / 583,396 |

These twenty-second points confirm redundant scan work. They do not isolate its
throughput cost. Left worker 2 alone reads 15.01 million words under unlimited
load, of which 14.84 million are empty, and yields 229,560 times mid-scan.
The next experiment skips empty ranges without retiring late-ACK history.
At 21G, TAP TX drops are 754,258 / 744,165, frame expiries 170,482 / 58,296,
and queue drops 736 / 10,653. Admission pressure, socket overflow, inner UDP
errors, invalid/duplicate/out-of-window/wrong-run records, reordering and outer
fragmentation remain zero at every point. These are diagnostic results, not a
normal-release acceptance sweep.

The [matched summary-index treatment](benches/tap-sender-loss-summary-diagnostic.json)
adds 128 bytes per history and charges both index and primary-word reads to the
existing work budget. It preserves late-ACK metadata, fixed scan endpoints and
one read per packet in dense histories. The
[treatment patch](benches/sender-loss-summary-diagnostic.patch) applies after the
two diagnostic patches above.

| Requested Gbit/s each way | Delivered Gbit/s | Missing after drain % | Daemon CPU % | Bitmap reads, millions | Scan continuations |
| --- | ---: | ---: | ---: | ---: | ---: |
| 16 | 1.55 / 1.17 | 90.06 / 92.53 | 165.76 / 160.66 | 0.93 / 1.22 | 44 / 161 |
| 21 | 1.53 / 1.43 | 92.59 / 93.06 | 168.99 / 160.39 | 0.88 / 0.85 | 21 / 9 |
| Unlimited | 1.65 / 1.45 | 97.20 / 97.39 | 167.98 / 158.92 | 0.89 / 0.84 | 30 / 24 |
| 16 return | 1.53 / 1.46 | 90.26 / 90.71 | 171.22 / 162.76 | 0.84 / 0.94 | 20 / 21 |

At 21G, bitmap reads fall by about 97% and mid-scan continuations nearly
disappear. Unlimited forward goodput rises from 1.20 to 1.65 Gbit/s while its
daemon CPU falls from 197% to 168%. Other points vary, including a slower first
reverse point; this is not evidence of a general throughput gain or a CPU
ceiling. At 21G, TAP TX drops are 749,070 / 740,528, frame expiries 4,423 / 10,842,
and queue drops 599 / 11,826. All four points retain zero admission pressure,
socket overflow, inner UDP errors, record errors, duplicates, reordering and
outer fragments. The isolated scan cost is reduced; the large sender regression
remains. Ordinary release measurements follow separately.

## Empty-history index: normal release comparison (November 7)

The [clock-corrected baseline](benches/tap-sender-clock.json) and
[summary-index build](benches/tap-sender-loss-summary.json) retain the normal
sixteen-datagram cap. Each runs the same four twenty-second points with retained
daemons and no diagnostic instrumentation or concurrent builds/tests.

| Requested Gbit/s each way | Baseline delivered Gbit/s | Indexed delivered Gbit/s | Baseline CPU % | Indexed CPU % | Indexed missing after drain % |
| --- | ---: | ---: | ---: | ---: | ---: |
| 16 | 0.72 / 0.81 | 0.85 / 0.72 | 169.15 / 152.50 | 147.75 / 141.57 | 94.54 / 95.38 |
| 21 | 0.84 / 0.80 | 0.85 / 0.83 | 163.43 / 152.65 | 152.89 / 143.01 | 95.86 / 95.95 |
| Unlimited | 0.83 / 0.81 | 0.84 / 0.81 | 157.67 / 152.37 | 151.76 / 140.18 | 98.55 / 98.56 |
| 16 return | 0.85 / 0.82 | 0.84 / 0.81 | 158.54 / 152.10 | 150.84 / 142.53 | 94.59 / 94.79 |

At 21G, CPU falls about 6% per daemon with similar goodput. The sparse scan is
a verified source of avoidable work, but removing it does not resolve the sender
regression. No general throughput gain or overload acceptance follows from this
single pair. Indexed TAP TX drops at 21G are 775,698 / 769,348, frame expiries
4,600 / 760, and queue drops 369 / 7,168. Unlimited frame expiries are
13,231 / 11,591. All points have zero admission pressure, socket overflow,
inner UDP errors, invalid/duplicate/out-of-window/wrong-run records, reordering
and outer fragmentation.

The indexed build passes 335 release tests, Clippy, formatting and 56 short
container tests, including allocation checks, retained late ACKs, sparse/dense
scans, ring wrap, maximum packet numbers and mutations between scan steps.
Foreign-move reconnect takes 18.62 seconds in this run; the service-delay gap
remains. The last full routed PMTU test precedes this index change.

## Packet-round window evidence (November 7)

A separate controller-integration defect cleared congestion-window-use evidence
after every delivering ACK. In ProbeBW_UP with a finite long-term inflight cap,
later ACKs from the same full flight could therefore stop contributing to cap
growth. A first-flight regression reproduces that loss of evidence.

The owner now records successful window use, excluding unused grant reservations.
The controller retains it through the packet-round boundary ACK and carries fresh
boundary evidence into the new round. A proven application-limited idle restart
clears both layers; zero inflight with queued work does not. Regressions cover
first-flight and boundary ACKs, unused/full-window grants, and idle versus queued
restarts. The precise tracking interpretation is documented with the
[draft-07 vectors](tests/vectors/bbr-07/README.md). No throughput gain is attributed
to this correctness fix before measurement. The fix passes 338 release tests,
Clippy and 56 short container tests; foreign-move reconnect takes 1.00 second
in this observation.

## Larger burst with indexed loss scans (November 7)

[Two independent ordinary-release sweeps](benches/tap-sender-summary-cap64.json)
repeat the cap64 candidate on `0894c5d`, after the clock and sparse-scan fixes
but before the packet-round correction. Each retains its own daemon pair across
four twenty-second points. The candidate changes only the three DATA burst caps;
BBR's 64 KiB quantum still bounds combined outstanding permission. It passes
335 release tests, Clippy and 56 short container tests.

| Run / requested Gbit/s each way | Delivered Gbit/s | Missing after drain % | Daemon CPU % |
| --- | ---: | ---: | ---: |
| 1 / 16 | 1.19 / 1.56 | 92.40 / 89.99 | 158.46 / 152.87 |
| 1 / 21 | 1.62 / 1.56 | 92.09 / 92.28 | 165.09 / 157.33 |
| 1 / unlimited | 1.77 / 1.65 | 96.90 / 96.78 | 161.85 / 158.29 |
| 1 / 16 return | 1.65 / 1.56 | 89.49 / 90.02 | 163.55 / 160.34 |
| 2 / 16 | 1.64 / 1.33 | 89.52 / 91.47 | 161.16 / 156.74 |
| 2 / 21 | 1.65 / 1.57 | 92.01 / 92.30 | 165.93 / 161.63 |
| 2 / unlimited | 1.76 / 1.62 | 96.97 / 96.84 | 164.30 / 161.15 |
| 2 / 16 return | 1.59 / 1.54 | 89.82 / 90.08 | 162.50 / 161.91 |

Both 21G points improve substantially over the indexed cap16 build's
0.85 / 0.83 Gbit/s, and neither shows an unlimited-load decline. First-point
variation remains. Both builds remain far below the CPU and goodput targets.
At 21G, frame expiries are 8,339 / 9,765 in run 1 and 3,432 / 9,233 in run 2;
queue drops are 2,418 / 14,193 and 2,687 / 14,277. The JSON preserves TAP TX
drops separately. Admission pressure, socket overflow, inner UDP errors, invalid
records, duplicates, reordering and outer fragmentation remain zero.

## Adopted bounded DATA burst (November 7)

The [combined normal-release sweep](benches/tap-sender-window-cap64.json)
adds the packet-round window-use correction to the indexed cap64 build.
The production DATA cap is now 64 maximum datagrams, still bounded by the
controller's 64 KiB quantum across all workers. Control traffic keeps its
separate sixteen-datagram cap. No queue, buffer, MTU or deadline changes.

| Requested Gbit/s each way | Actual offered Gbit/s | Delivered Gbit/s | Missing after drain % | Daemon CPU % |
| --- | ---: | ---: | ---: | ---: |
| 16 | 15.98 / 15.94 | 1.49 / 1.45 | 90.28 / 90.53 | 167.71 / 158.70 |
| 21 | 20.98 / 20.93 | 1.59 / 1.49 | 92.30 / 92.75 | 170.84 / 161.58 |
| Unlimited | 58.82 / 57.48 | 1.62 / 1.50 | 97.20 / 97.35 | 168.94 / 159.48 |
| 16 return | 15.98 / 15.97 | 1.54 / 1.43 | 90.21 / 90.85 | 163.14 / 158.83 |

Unlimited goodput rises by 2.0 / 0.4% from the 21G point. These results support
the larger burst; they do not establish a CPU ceiling or isolate the effect of
the packet-round correction. Goodput remains below both original targets.
At 21G, endpoint counters report 745,518 / 733,256 TAP TX drops,
7,280 / 5,526 frame expiries and 760 / 12,255 worker queue drops.
All four points have zero admission pressure, socket overflow, inner UDP errors,
invalid/duplicate/out-of-window/wrong-run records, reordering and outer fragments.
The combined build passes 338 release tests, Clippy and 56 short container
tests. Both additional PMTU tests pass; routed discovery takes 651.15 seconds
and confirms every lane at 9000 → 1500 → 1280 → 9000, including returned
and filtered PTB, the normal upward retry and zero outer fragmentation.

## Grant-wait attribution (November 7)

The [Open-phase diagnostic](benches/tap-sender-open-diagnostic.json) separates
pacing, window, burst, sampling-pool and ready waits from absence of queued
requests and work inside owner operations. Its
[patch](benches/sender-open-diagnostic.patch) builds on the earlier diagnostic
patches; the clock and packet-round corrections remain absent for comparison.
It adds counters, clocks and bounded request scans, so it is not an acceptance
build. The load-only timing summaries use before/sent monitor snapshots;
the seven categories account for about 99.8% of the existing Open timer.

At 21G, delivered goodput is 1.59 / 1.51 Gbit/s using 174.95 / 165.54% CPU.
Issuance is Open for 81.85 / 84.46% of the load interval. Within that time:

| Open-state category | Left % | Right % |
| --- | ---: | ---: |
| Pacing deadline pending | 44.57 | 43.45 |
| Window blocks after pacing is eligible | 45.41 | 45.42 |
| No queued owner request | 1.63 | 1.95 |
| Burst/reservation limit | 0.65 | 0.09 |
| Sampling pool | 0 | 0 |
| Ready for service or cleanup | 3.40 | 4.13 |
| Inside owner operations, including instrumentation | 4.15 | 4.74 |

Average grant budgets are 33.75 / 32.61 KB, with about 1.3% unused.
Requests wait 530 / 602 microseconds for permission. Issuance follows the later
of request arrival and the previous pacing deadline by 71 / 72 microseconds;
that includes epoch/window/host waits and is not timer-only latency. Sampled
median pacing rates are 3.18 / 4.27 Gbit/s, windows 108.7 / 113.5 KB and minimum
RTTs about 32 microseconds. These measurements direct the next investigation
toward feedback timing and window use; they do not prove that ACK batching is
the cause. A per-lane sixteen-packet/1-ms ACK threshold can delay a shared
window, but packet-number gaps can trigger earlier ACKs. Their actual frequency
and ACK queue residence need measurement.

## ACK range overhead and control-budget pressure (November 7)

The [fresh-clock ACK diagnostic](benches/tap-sender-ack-timing-diagnostic.json)
measures actual socket-receive, snapshot-construction, coordinator-enqueue and
successful pre-syscall times. The
[reason patch](benches/sender-ack-reason-diagnostic.patch) and
[timing patch](benches/sender-ack-timing-diagnostic.patch) extend the Open
diagnostic above. An earlier counter used the cached worker-turn timestamp as
construction time; its apparent queue delay included work before construction.
The corrected counters separate that offset and reconcile queue time exactly
as handoff plus coordinator residence.

At 21G, 99.80 / 99.84% of successfully emitted ACKs contain all 32 ranges.
Each such ACK occupies 620 outer bytes. ACK traffic uses
4.00 / 3.95 MB/s, about 95 / 94% of the 4 MiB/s control ceiling.
Timer expiry is the earliest eligibility cause for only 5.44 / 4.51% of ACKs;
threshold and packet-gap causes can overlap on equal deadlines.

| Mean ACK interval at 21G | Left µs | Right µs |
| --- | ---: | ---: |
| Cached turn start to actual construction | 37.79 | 35.42 |
| Actual construction to coordinator enqueue | 17.01 | 9.52 |
| Coordinator enqueue to successful syscall | 204.01 | 179.67 |
| Required control-pacing wait within that residence | 170.79 | 149.49 |
| Actual construction to successful syscall | 221.02 | 189.19 |
| Highest acknowledged packet's socket receive to construction | 92.94 | 78.18 |

The [matched control-rate experiment](benches/tap-sender-ack-rate-diagnostic.json)
changes only the control byte ceiling from 4 to 16 MiB/s; its
[patch](benches/sender-ack-rate-diagnostic.patch) retains the sixteen-packet
burst and all other scheduling rules. Both are instrumented release builds
from the same diagnostic lineage, without the later startup-clock and
packet-round corrections. Each uses a fresh container and four retained-daemon
twenty-second points; no builds or other tests overlap load.

| Control MiB/s / requested Gbit/s each way | Delivered Gbit/s | Missing after drain % | Daemon CPU % |
| --- | ---: | ---: | ---: |
| 4 / 16 | 1.66 / 1.38 | 89.39 / 91.11 | 169.05 / 161.40 |
| 4 / 21 | 1.60 / 1.56 | 92.24 / 92.37 | 172.93 / 165.07 |
| 4 / Unlimited | 1.62 / 1.53 | 97.27 / 97.07 | 174.09 / 164.84 |
| 4 / 16 return | 1.53 / 1.45 | 90.24 / 90.74 | 169.50 / 163.87 |
| 16 / 16 | 3.30 / 2.75 | 78.88 / 82.36 | 250.02 / 240.18 |
| 16 / 21 | 3.16 / 2.90 | 84.70 / 85.91 | 252.67 / 238.25 |
| 16 / Unlimited | 3.23 / 3.12 | 94.34 / 94.40 | 257.15 / 247.70 |
| 16 / 16 return | 3.33 / 3.18 | 78.89 / 79.81 | 266.24 / 250.37 |

At 21G the higher ceiling raises goodput by 98 / 86% and eliminates explicit
control-pacing deferrals. Actual ACK queue time falls to 17.40 / 16.42 µs;
ACK traffic rises to 5.74 / 6.01 MB/s. This isolates the control ceiling as a
substantial bottleneck in this workload. It does not explain all remaining
cost: Closing and Feedback together occupy roughly 40 / 34% of sender time
in that treatment, and neither performance gate is close.

With the 4 MiB/s build, 21G counters report 742,018 / 724,012 TAP TX drops,
13,972 / 5,662 frame expiries and 1,838 / 13,751 worker queue drops.
With 16 MiB/s they report 680,446 / 663,332, 3,889 / 4,878 and
1,846 / 25,993 respectively. All eight points report zero admission pressure,
socket overflow, inner UDP errors, invalid/duplicate/out-of-window/wrong-run
records, reordering and outer fragmentation.

The production ceiling remains 4 MiB/s. Before the change below, a grant that sent
only part of a prepared batch discarded its never-attempted suffix, leaving
packet-number holes. Receive history retains those holes and emits its newest
32 ranges; sender-side ACK pruning cannot reduce those wire bytes.
Reducing the ACK timer is not supported by these
measurements, and dropping older ACK ranges without delivery evidence would
change recovery behavior.

## Retain never-attempted DATA plans (November 7)

A fully successful granted prefix now advances the batch head while preserving
the never-attempted suffix's packet numbers, metadata and history reservations.
Its source slots stay fixed, so retention adds neither payload copies nor
allocations. Partial socket sends still discard the remaining plan and retry
unsent frame ranges with fresh numbers. A shared discard path cancels retained
reservations on expiry, MTU reduction, rejected grants and connection retirement.
Overtaking controls refresh the plan without reusing canceled identities.

The regression adds a contiguous-number assertion to the existing split-frame
fixture. Unchanged production code sends packet 44 where 11 is expected; the
new path passes and produces one ACK range when no controls intervene.
Other regressions cover reservation survival through grant recycling, physical
partial sends, canceled higher reservations, expiry, MTU shrink and quiescence.
Controls and discarded plans can still create gaps in production.

The [normal release sweep](benches/tap-sender-retained-suffix.json) uses the same
controller, control ceiling, memory limits, PMTU discovery and harness as the
cap64 baseline. Four twenty-second points share retained daemons; builds and
other tests do not overlap load. L→R / R→L pairs are simultaneous, and 100% CPU
means one core.

| Requested Gbit/s each way | Delivered Gbit/s | Missing after drain % | Daemon CPU % |
| --- | ---: | ---: | ---: |
| 16 | 2.59 / 2.03 | 83.49 / 87.04 | 224.84 / 210.89 |
| 21 | 2.41 / 2.09 | 88.36 / 89.88 | 218.09 / 207.07 |
| Unlimited | 2.53 / 2.23 | 95.63 / 96.10 | 222.53 / 213.71 |
| 16 return | 2.74 / 2.03 | 82.63 / 87.10 | 230.00 / 211.22 |

At 21G, goodput improves 52 / 40% over the baseline's 1.59 / 1.49 Gbit/s.
Unlimited load improves 57 / 49% over its 1.62 / 1.50 result and does not decline
from this treatment's 21G point. CPU use rises with delivered work. These results
remain far below both the target and the earlier runtime; they do not establish
a CPU ceiling or a universal overload plateau.

At 21G, counters report 711,854 / 700,220 TAP TX drops, 18,445 / 13,072 frame
expiries and 1,030 / 19,060 worker queue drops. Unlimited load reports
2,169,877 / 2,130,616, 5,140 / 6,347 and 912 / 20,730 respectively. All four
points report zero admission pressure, socket overflow, inner UDP errors,
invalid/duplicate/out-of-window/wrong-run records, reordering and outer
fragmentation. TAP drops count source SKBs, so these counters cannot be summed
as exact frame loss. ACK-range and control-wait measurements are still needed
to attribute the remaining limit after retention.

The change passes 339 ordinary release tests, 60 kernel/integration tests,
Clippy and formatting. The kernel checks include allocation instrumentation,
TAP rename/namespace movement, daemon restart and short MTU tests. The full
routed test also passes 9000 → 1500 → 1280 → 9000 with returned and filtered
ICMPv6 PTB, the normal upward retry and zero outer fragmentation.

## ACK timing after suffix retention (November 7)

The [matched current-source diagnostic](benches/tap-sender-retained-ack-diagnostic.json)
applies identical [instrumentation](benches/sender-current-ack-diagnostic.patch)
to the cap64 baseline and retained-suffix build. Both include the startup-clock
and packet-round corrections. Each runs four twenty-second points with the
same placement and active PMTU discovery, without concurrent builds or tests.
These instrumented observations supplement the normal release sweep above.

| At 21G offered each way | Baseline L / R | Retained suffix L / R |
| --- | ---: | ---: |
| Delivered Gbit/s | 1.60 / 1.55 | 2.34 / 1.99 |
| Daemon CPU % | 170.21 / 165.61 | 225.27 / 209.10 |
| Mean ranges per ACK | 31.97 / 31.97 | 24.53 / 22.04 |
| ACKs carrying all 32 ranges % | 99.80 / 99.83 | 58.27 / 39.98 |
| Packet gap among earliest ACK causes % | 81.02 / 83.52 | 1.41 / 1.32 |
| Timer among earliest ACK causes % | 5.43 / 4.53 | 15.32 / 8.86 |
| ACK traffic MB/s | 4.01 / 3.99 | 3.80 / 3.77 |
| Mean actual ACK queue µs | 215.17 / 192.85 | 35.65 / 34.85 |
| Mean required control-pacing wait µs | 165.04 / 152.78 | 15.69 / 15.41 |

Reason categories can overlap on tied deadlines. Actual queue time excludes
the cached-turn timestamp offset; the control-wait counter excludes wakeup
lateness. Retention substantially reduces artificial packet gaps and control
waiting, but ACK range history remains costly. All eight points have zero
admission pressure, socket overflow, inner UDP errors, invalid/duplicate/
out-of-window/wrong-run records, reordering and outer fragmentation.

After retention, Open occupies 74.73 / 78.07% of sender time. Within Open,
pacing blocks 46.43 / 43.59% and the congestion window blocks 35.76 / 39.59%;
ready work waiting for service occupies 8.91 / 9.43%. Closing occupies
20.30 / 17.16% of total sender time. These separate remaining scheduling and
window constraints from the reduced ACK-control delay; they do not establish
the CPU ceiling.

A [normal-release control-rate experiment](benches/tap-sender-retained-control16.json)
then raises only the control ceiling from 4 to 16 MiB/s, retaining the literal
sixteen-datagram burst and all other limits.

| Requested Gbit/s each way | Delivered Gbit/s | Missing after drain % | Daemon CPU % |
| --- | ---: | ---: | ---: |
| 16 | 2.55 / 2.26 | 83.76 / 85.66 | 235.72 / 217.96 |
| 21 | 2.61 / 2.26 | 87.38 / 89.04 | 234.67 / 219.39 |
| Unlimited | 2.61 / 2.16 | 95.38 / 96.00 | 235.97 / 218.21 |
| 16 return | 2.74 / 2.43 | 82.62 / 84.61 | 244.15 / 230.07 |

The paced point improves about 8% each way over the normal retained-suffix
baseline, but reverse goodput falls 4.5% under unlimited load. At 21G, counters
report 704,430 / 693,896 TAP TX drops, 9,488 / 13,113 frame expiries and
575 / 20,792 queue drops. All four points pass the same zero-error checks above.
The three control-pacing tests and Clippy pass. This ceiling change remains
an experiment: the gain is modest, it does not fix the plateau, and it has not
passed the complete runtime acceptance suite. Production stays at 4 MiB/s.

## ACK packing and offload-window experiments (November 7)

Three normal release experiments use `aa1ca3d`, the same generator and
twenty-second retained-daemon sweep. None is adopted. Reproduction patches
and full four-point results are retained for
[ACK packing](benches/tap-sender-ack-pack-experiment.json),
[a three-quanta offload floor](benches/tap-sender-offload-budget-experiment.json)
and [that floor with 16 MiB/s control](benches/tap-sender-offload-control-experiment.json).

ACK packing combines up to four already eligible same-session ACK records
within the base packet ceiling. It preserves the 64-record control turn
budget, charges physical packet bytes and finalizes every ACK delay at send
time. The experiment passes 339 ordinary release tests and 65 short kernel
tests, including multi-record dispatch, duplicate suppression, queue bounds,
session boundaries and failed-send cleanup. Its reverse unlimited goodput
falls 5.2% from the retained-suffix baseline. There is no clear overall gain;
the experiment does not measure how often records actually combine.

The offload experiment raises the congestion-window floor from one to three
send quanta. The one-quantum DATA burst, 64-datagram cap and loss/recovery
bounds stay unchanged. Draft-07 permits a
[transport-specific offload budget](https://www.ietf.org/archive/id/draft-ietf-ccwg-bbr-07.html#section-5.5.8);
this tests a policy choice, not a verified defect in the existing floor.
It improves paced goodput by 12% / 28% and unlimited goodput by 13% / 20%.
Both offload builds pass ordinary release tests and Clippy, but full kernel
and routed PMTU acceptance remain pending.

| Variant / requested Gbit/s each way | Actual offered Gbit/s | Delivered Gbit/s | Missing after drain % | Daemon CPU % |
| --- | ---: | ---: | ---: | ---: |
| Retained suffix / 21 | 20.99 / 20.96 | 2.41 / 2.09 | 88.36 / 89.88 | 218.09 / 207.07 |
| Retained suffix / unlimited | 58.84 / 58.06 | 2.53 / 2.23 | 95.63 / 96.10 | 222.53 / 213.71 |
| ACK packing / 21 | 20.98 / 20.96 | 2.40 / 2.22 | 88.39 / 89.26 | 226.02 / 212.08 |
| ACK packing / unlimited | 57.33 / 54.61 | 2.46 / 2.10 | 95.63 / 96.08 | 222.38 / 206.88 |
| Three-quanta floor / 21 | 20.99 / 20.96 | 2.71 / 2.68 | 86.92 / 87.03 | 249.73 / 236.96 |
| Three-quanta floor / unlimited | 57.08 / 55.49 | 2.85 / 2.68 | 94.93 / 95.09 | 249.61 / 235.70 |
| Floor + 16 MiB/s control / 21 | 20.98 / 20.97 | 3.13 / 2.93 | 84.84 / 85.84 | 271.22 / 253.71 |
| Floor + 16 MiB/s control / unlimited | 57.52 / 47.12 | 2.93 / 2.67 | 94.83 / 94.26 | 249.61 / 233.94 |

The combined floor/control experiment declines 6.5% / 8.8% from paced to
unlimited load. Its right inner receiver reports 350 UDP receive-buffer
errors, also visible through socket overflow reporting; these count SKBs,
not exact missing frames. The reverse receiver has zero such errors yet
also declines. A repeat and further attribution are needed before changing
production policy. No experiment establishes a CPU ceiling or either target.

| Variant, unlimited | TAP TX drops (SKBs) | Expired frames | Worker queue drops |
| --- | ---: | ---: | ---: |
| Retained suffix | 2169877 / 2130616 | 5140 / 6347 | 912 / 20730 |
| ACK packing | 2111869 / 2001600 | 5186 / 13191 | 490 / 19491 |
| Three-quanta floor | 2083941 / 2003580 | 8468 / 10542 | 669 / 25425 |
| Floor + 16 MiB/s control | 2104290 / 1686535 | 10309 / 4204 | 1563 / 29439 |

All other inner UDP error counts are zero. Every observation has zero outer
socket overflow, outer fragmentation, admission pressure, malformed packets,
and invalid, duplicate, reordered or out-of-window receiver records.
Worker queue drops remain substantial and are not yet attributed by cause.
The JSON files retain every point's per-worker counters and CPU use.
No builds or other tests ran during the load intervals.

## Incoming ACK storage pressure (November 7)

The new `ack_buffer_drops` counter isolates ACK records rejected when all
reserved incoming ACK buffers are busy. It remains included in `queue_drops`.
The [matched four-versus-sixteen-buffer sweep](benches/tap-sender-ack-pool.json)
uses `aa1ca3d` plus that counter, with retained DATA suffixes and the production
window/control policy. Existing dependent queue capacities and memory accounting
follow the buffer constant; the treatment changes no processing budget.

| Buffers / requested Gbit/s each way | Delivered Gbit/s | Missing after drain % | Daemon CPU % | ACK-buffer drops | Other queue drops |
| --- | ---: | ---: | ---: | ---: | ---: |
| 4 / 16 | 2.28 / 2.16 | 85.45 / 86.17 | 219.21 / 211.86 | 67 / 90 | 942 / 22571 |
| 4 / 21 | 2.15 / 1.96 | 89.57 / 90.50 | 213.51 / 200.74 | 65 / 68 | 880 / 18146 |
| 4 / unlimited | 2.24 / 1.99 | 96.10 / 96.33 | 216.02 / 202.51 | 54 / 56 | 655 / 18566 |
| 4 / 16 return | 2.26 / 1.95 | 85.58 / 87.55 | 217.92 / 202.87 | 39 / 43 | 738 / 17885 |
| 16 / 16 | 2.32 / 2.17 | 85.25 / 86.21 | 220.99 / 211.72 | 0 / 0 | 784 / 20287 |
| 16 / 21 | 2.35 / 2.01 | 88.58 / 90.19 | 220.36 / 206.51 | 0 / 0 | 466 / 18859 |
| 16 / unlimited | 2.29 / 2.06 | 95.98 / 96.15 | 219.35 / 207.47 | 0 / 0 | 667 / 19059 |
| 16 / 16 return | 2.22 / 1.92 | 85.79 / 87.71 | 209.17 / 193.10 | 0 / 0 | 794 / 18050 |

Incoming ACK exhaustion explains only 0.37% of the right endpoint's queue
drops at 21G with four buffers. Sixteen removes that loss, but its return
point is slower and the paced gain is within the spread of preceding normal
release sweeps. Production keeps four buffers. The new counter and pressure
regression are retained.

Code inspection attributes the right transmit-only worker's remaining drops
to foreign source admission, which checks its free buffers, source queue and
frame/byte quotas before exchanging ownership. These drops are consistent
with ingress far exceeding paced output; their presence does not establish
a cause of the low sender ceiling. Enlarging those queues has not been justified.

Both builds pass 339 ordinary release tests, all sixteen sender kernel tests,
Clippy and formatting. The regression fills the exact ACK capacity, rejects
and counts two excess records, completes real ACK/report handoffs, then
verifies returned buffers accept another ACK and sampled inflight bytes settle.
All eight observations have zero admission pressure, outer socket overflow,
inner UDP errors, outer fragments and receiver corruption, duplicates or
reordering. The full JSON includes expiry, TAP drops and per-worker CPU.
No build or other test overlapped a load interval.

## Retained-tail request sizes (November 7)

A [release diagnostic](benches/tap-sender-request-size-diagnostic.json) counts
fresh and retained requests, received DATA grants and successful small-tail
submissions with waiting same-lane frames. It uses the production four-buffer,
one-quantum and 4 MiB/s policies. Histograms count grants, not packets; their
byte totals include outer headers.

| Request each way | Mean grant datagrams, L/R | Grants ≤16 datagrams % | Short retained-tail grants with backlog % | Their share of granted bytes % |
| --- | ---: | ---: | ---: | ---: |
| 16 Gbit/s | 18.12 / 17.80 | 51.14 / 52.51 | 18.86 / 18.65 | 7.43 / 7.21 |
| 21 Gbit/s | 18.26 / 17.75 | 50.57 / 52.18 | 19.57 / 18.38 | 7.50 / 7.28 |
| Unlimited | 18.69 / 17.73 | 49.60 / 52.14 | 21.03 / 18.52 | 7.55 / 7.37 |
| 16 Gbit/s return | 18.25 / 17.74 | 50.57 / 52.31 | 19.33 / 18.40 | 7.37 / 7.36 |

At 21G, 51% of requests use a retained plan. Fresh requests average
37.9 / 36.6 datagrams, retained requests 16.1 / 16.5, and grants 18.3 / 17.7.
The small retained tails above pay a separate request/permission/completion
exchange while the lane has more queued work. At that revision, the flush path refuses
to add frames until the retained plan empties. This verifies the mechanism and
its frequency; the following refill experiment measures the goodput effect.
Window-trimmed prefixes also contribute to small grants, so filling tails
cannot remove all their overhead.

The diagnostic delivers 2.18 / 2.04 Gbit/s at 21G and 2.39 / 2.07 unlimited,
using 214.3 / 202.8% and 222.7 / 206.1% daemon CPU. Missing fractions are
89.43 / 90.08% and 96.02 / 96.21%. The added counters and bounded packet-size
sums have a cost; these observations are not an exact production baseline.
All four points have zero admission pressure, socket overflow, inner UDP errors,
outer fragments and receiver corruption, duplicates or reordering.
The JSON retains all loss counters and per-worker CPU.

The diagnostic passes 340 ordinary release tests, fifteen sender kernel
regressions, Clippy and formatting. A separate broader check of the production
ACK-counter build passes 54 library kernel tests, foundation, load-tool and CLI
checks, plus foreign rename/namespace move. Its runtime forwarding test times
out on a single additional-session frame after the ten-second storm while a
diagnostic build is running. The identical binary passes alone in 43.60 seconds.
Build contention is a hypothesis; the failure is not attributed or fixed.
Keep builds separate from timing-sensitive runtime tests as well as throughput
measurements. No build or other test overlaps the diagnostic load intervals.

## Refilling retained DATA plans (November 7)

The worker now adds waiting frames before requesting another grant for a retained
tail. It compacts consumed packet/piece metadata and frame handles, then uses
the existing packer to append new datagrams. Payloads stay in their source
buffers. Retained packet numbers, bytes and lengths stay unchanged, including
the final short datagram. The existing frame, packet, byte and turn limits apply;
no persistent storage or syscall is added.

[Two matched release sweeps per build](benches/tap-sender-retained-refill.json)
compare the production four-ACK-buffer build with refill, in baseline/refill/
refill/baseline order. Each sweep retains its daemons through twenty-second
16G, 21G, unlimited and 16G-return points. Workload, CPU placement, active PMTU
discovery and the 1452-byte outer UDP ceiling are unchanged. Builds and other
tests do not overlap measurement.

| Build / sweep / requested Gbit/s each way | Delivered Gbit/s | Missing after drain % | Daemon CPU % |
| --- | ---: | ---: | ---: |
| Baseline / first / 21 | 1.95 / 1.83 | 90.53 / 91.01 | 196.49 / 184.72 |
| Baseline / first / unlimited | 1.90 / 1.85 | 96.69 / 96.51 | 188.32 / 181.99 |
| Refill / first / 21 | 2.85 / 2.54 | 86.20 / 87.73 | 237.88 / 225.17 |
| Refill / first / unlimited | 2.34 / 2.88 | 95.88 / 95.05 | 230.83 / 227.56 |
| Refill / repeat / 21 | 2.25 / 2.35 | 89.10 / 88.59 | 218.79 / 208.90 |
| Refill / repeat / unlimited | 2.56 / 2.69 | 95.52 / 94.87 | 233.83 / 222.60 |
| Baseline / repeat / 21 | 2.17 / 2.02 | 89.48 / 90.18 | 212.96 / 201.09 |
| Baseline / repeat / unlimited | 2.26 / 1.88 | 96.10 / 96.68 | 210.93 / 197.23 |

The two-sweep means improve 23.9 / 26.9% at 21G and 17.7 / 49.5% unlimited.
This supports refilling tails as a useful batching change, but does not establish
a plateau: the first refill sweep falls 18.1% forward under unlimited load,
while the repeat rises 13.9%. Directional variation and the low throughput
ceiling remain unresolved. The 20 Gbit/s and under-1% loss gates remain open.

| Build / sweep, unlimited | TAP TX drops (SKBs) | Expired frames | Queue drops | ACK-buffer drops (included in queue drops) |
| --- | ---: | ---: | ---: | ---: |
| Baseline / first | 2176472 / 1988384 | 4942 / 46 | 2735 / 18032 | 653 / 440 |
| Refill / first | 2127025 / 2133866 | 18854 / 6363 | 850 / 23912 | 64 / 64 |
| Refill / repeat | 2130329 / 1918907 | 9548 / 12496 | 1629 / 22795 | 60 / 86 |
| Baseline / repeat | 2182134 / 2132660 | 4898 / 13294 | 782 / 17298 | 84 / 114 |

All sixteen points have zero admission pressure, outer socket overflow,
inner UDP errors, malformed/invalid/duplicate/out-of-window/wrong-run records
and outer IPv6 fragmentation. The first refill sweep has thirteen reordered
frames in one flow at 16G, or 0.0003125% of that direction's delivered frames.
All other points have zero reordering. Its cause is unverified; source review
alone does not attribute it to either refill or the kernel.

The initial candidate failed the runtime storm with a missing send reservation.
A focused regression reproduced retained packet numbers 11–22 followed by 24
after probe 23 was canceled. Completion accounting expects consecutive numbers.
Refill now defers across that gap, drains the valid suffix, then prepares fresh
packets. The regression verifies actual submission and ACK settlement to zero
inflight. Canceled identities are never reused.

The final tree, including the registration fix below, passes 341 ordinary
release tests, 332 default-feature tests, 64 short kernel/integration tests,
Clippy and formatting. Tests cover unchanged retained wire bytes, split offsets,
bounded metadata, MTU changes, packet-number exhaustion, partial sends, expiry,
retirement and progress across two workers/lanes. Both runtime tests pass without
concurrent compilation. The full routed test passes in 650.20 seconds:
9000 → 1500 → 1280 → 9000, with returned and filtered PTB, the normal upward
retry interval and no outer fragmentation.

## Receive registration ordering (November 7)

A separate regression reproduces an initial lane-registration inversion on the
pre-refill code. The destination worker can queue a newer direct receive while
the older packet is still traveling through registration. Installing the lane
then appended that older buffer behind the newer one, delivering frame 2 before
frame 1.

Registration now places its first retained buffer at the front of the receive
queue. The usual GRO, datagram, record and byte budgets still apply. The test
uses both worker handoffs, verifies frame IDs and all bytes in order, and fails
on the former enqueue path. This fixes the demonstrated initial-buffer race;
it does not add a general reorder queue for packets arriving through different
routes. It is not established attribution for the thirteen reordered frames
in the refill sweep.

## Sixty-second refill follow-up (November 7)

[Three sixty-second points](benches/tap-sender-refill-register-long.json)
measure the final tree with both refill and registration ordering. The daemons
remain running through paced, unlimited and paced-return points, with the same
QinQ workload and CPU placement as the shorter sweeps. No builds, other tests
or Git packing overlap load.

| Requested Gbit/s each way | Actual offered Gbit/s | Delivered Gbit/s | Missing after drain % | Daemon CPU % |
| --- | ---: | ---: | ---: | ---: |
| 21 | 20.97 / 20.94 | 2.41 / 2.44 | 88.41 / 88.27 | 223.86 / 215.96 |
| Unlimited | 61.37 / 56.08 | 2.47 / 2.35 | 95.95 / 95.78 | 222.99 / 213.47 |
| 21 return | 20.97 / 20.94 | 2.51 / 2.37 | 87.95 / 88.61 | 225.96 / 216.40 |

Unlimited goodput rises 2.4% forward and falls 3.4% reverse. The larger
forward decline from the first short sweep does not recur, but these three
points do not prove a repeatable plateau or a CPU ceiling. Delivery remains
far below 20 Gbit/s each way, with severe loss.

At unlimited load, endpoint L/R counters report 6806774 / 6157168 TAP TX
drops, 12803 / 26991 expired frames and 6486 / 59713 queue drops.
The queue totals include 376 / 145 ACK-buffer drops. All three points have
zero admission pressure, outer socket overflow, inner UDP errors, malformed
or invalid records, duplicates, reordering and outer fragmentation.
The earlier thirteen reordered frames remain unattributed. This follow-up
does not isolate the registration fix's performance effect.

## Coordinator-local feedback scheduling (November 7)

A focused regression reproduces another avoidable sender delay: local history
has applied an ACK, but the shared controller still counts 176 bytes in flight
after the worker's service turn. The coordinator produced its report after its
first outgoing handoff and called the shared owner before handing off that report.

An additional bounded handoff before the owner advances removes that extra turn
when the outgoing queue has room. It preserves destination FIFO, blocked-queue
behavior and the 64-entry handoff budget. The regression now settles the shared
charge and still hands off the next foreign worker's permit. Long or blocked
queues may defer reports; this is not an unbounded drain.

[Matched normal release sweeps](benches/tap-sender-local-feedback.json) compare
the previous combined build with this change. Each keeps its daemons through
sixty-second 21G, unlimited and 21G-return points. QinQ frames, CPU placement,
active discovery and the 1452-byte outer UDP ceiling are unchanged. No builds,
other tests or Git packing overlap load.

| Build / requested Gbit/s each way | Delivered Gbit/s | Missing after drain % | Daemon CPU % |
| --- | ---: | ---: | ---: |
| Baseline / 21 | 2.42 / 2.37 | 88.38 / 88.56 | 218.82 / 211.46 |
| Baseline / unlimited | 2.31 / 2.39 | 95.94 / 95.52 | 214.57 / 209.23 |
| Baseline / 21 return | 2.81 / 2.33 | 86.47 / 88.74 | 235.10 / 221.44 |
| Earlier feedback / 21 | 2.63 / 2.55 | 87.32 / 87.69 | 233.14 / 222.41 |
| Earlier feedback / unlimited | 2.56 / 2.61 | 95.50 / 95.30 | 230.03 / 221.56 |
| Earlier feedback / 21 return | 2.91 / 2.41 | 86.02 / 88.39 | 238.81 / 222.72 |

The first paced point improves 8.8 / 7.5%, unlimited improves 10.6 / 9.0%,
and the return point improves 3.5 / 3.2%. This is one matched pair. The
candidate still falls 2.8% forward under unlimited load; reverse rises 2.3%.
It does not establish a repeatable plateau, a CPU ceiling or either throughput
gate. The zero-loss forwarding limit and notification delays still need profiling.

| Build, unlimited | TAP TX drops (SKBs) | Expired frames | Queue drops | ACK-buffer drops (included in queue drops) |
| --- | ---: | ---: | ---: | ---: |
| Baseline | 6336366 / 5859332 | 22994 / 32228 | 7003 / 61260 | 625 / 495 |
| Earlier feedback | 6299603 / 6073092 | 19800 / 30796 | 6538 / 66606 | 371 / 228 |

All six points have zero admission pressure, outer socket overflow, inner UDP
errors, malformed/invalid/duplicate/out-of-window/wrong-run/reordered records
and outer IPv6 fragmentation. These counters distinguish observed loss locations;
their different units do not permit an exact frame-loss sum.

The change passes 341 ordinary release tests, 332 default-feature tests,
65 short kernel/integration tests, Clippy and formatting. Runtime tests pass
in 83.96 seconds without concurrent compilation, including foreign rename/move,
restart, control pressure and the frame storm. The sender change adds no
allocation, lock, payload copy or syscall. The full routed PMTU test passes
in 653.11 seconds: 9000 → 1500 → 1280 → 9000 on every lane, returned and
filtered PTB, the normal upward retry interval and no outer fragments.

## Offered-rate range after local feedback fix (November 7)

[Nine twenty-second release points](benches/tap-sender-local-feedback-low-rate.json)
start at 0.5 Gbit/s each way, rise to 21 Gbit/s, then run unlimited. They retain
the same daemons and controller state, testing load adaptation as well as
forwarding. The binary is the normal release build of `db69631`; placement,
QinQ traffic and active PMTU discovery match the sixty-second comparison.
No builds, other tests or Git packing overlap load.

| Requested Gbit/s each way | Delivered Gbit/s | Missing after drain % | Daemon CPU % |
| --- | ---: | ---: | ---: |
| 0.5 | 0.49 / 0.49 | 0 / 0.42 | 44.12 / 41.45 |
| 1 | 0.99 / 0.99 | 0 / 0.43 | 88.38 / 83.93 |
| 2 | 1.96 / 1.97 | 0.84 / 0.63 | 166.88 / 156.14 |
| 3 | 2.33 / 2.42 | 21.03 / 18.07 | 220.63 / 208.66 |
| 4 | 2.68 / 2.70 | 32.04 / 31.65 | 237.80 / 229.44 |
| 8 | 2.64 / 2.71 | 66.44 / 65.54 | 224.38 / 217.03 |
| 16 | 2.42 / 2.66 | 84.35 / 82.83 | 230.13 / 222.56 |
| 21 | 2.42 / 2.67 | 88.30 / 87.07 | 228.75 / 219.45 |
| Unlimited | 2.22 / 2.57 | 96.10 / 95.20 | 219.44 / 210.85 |

Actual unlimited offered load is 57.76 / 54.23 Gbit/s. Forward delivery falls
17.2% from the 4G point; reverse falls 5.4% from its 8G peak. Even comparing
only the last paced point, unlimited falls 8.2 / 3.7%. The earlier narrow
sixty-second comparison therefore does not establish an overload plateau.

Both directions stay below 1% missing through 2G offered, then lose
21.0 / 18.1% at 3G. This is one ascending sweep, not a repeated acceptance
test or a zero-loss ceiling. At 0.5G and 1G, TAP TX drops are zero but reverse
loss is already 0.42 / 0.43%; expiry and non-ACK queue drops are nonzero.
Their individual causal contribution needs finer attribution.

| Requested Gbit/s each way | TAP TX drops (SKBs) | Expired frames | Queue drops | ACK-buffer drops (included in queue drops) |
| --- | ---: | ---: | ---: | ---: |
| 0.5 | 0 / 0 | 31 / 2732 | 126 / 151 | 126 / 132 |
| 1 | 0 / 0 | 50 / 3519 | 96 / 236 | 96 / 149 |
| 2 | 233 / 40 | 7372 / 4241 | 361 / 499 | 113 / 146 |
| 3 | 22514 / 5304 | 5433 / 11615 | 1788 / 15388 | 90 / 60 |
| 4 | 44888 / 25680 | 13661 / 6429 | 2646 / 21242 | 68 / 106 |
| Unlimited | 2140824 / 1970264 | 5259 / 6673 | 1924 / 22538 | 141 / 81 |

All nine points have zero admission pressure, outer socket overflow,
inner UDP errors, malformed/invalid/duplicate/out-of-window/wrong-run/reordered
records and outer IPv6 fragmentation. Runtime selective repair remains open;
that does not explain away the sender's overload decline.

## Foreign sender handoff timing (November 7)

A [temporary diagnostic](benches/sender-handoff-diagnostic-v1.patch) measures
foreign Permit, Complete and Report handoffs without changing notification
placement, pacing or transport timestamps.
[The release sweep](benches/tap-sender-handoff-diagnostic-v1.json) repeats the
same nine offered rates as the normal build above. It uses pooled timestamp
fields and fixed worker-local counters; no hot allocation or lock is added.
The compiled musl timing test makes no `clock_gettime` syscall on the tested
host. Other deployment kernels may use a different clock path.

Each timing starts immediately before the successful SPSC push, then ends
either immediately before the existing eventfd notification call or at the
consumer handler. The start can include producer preemption before publication.
Local synchronous messages and failed pushes are excluded. These are load-only
before/sent snapshots, separate from post-drain throughput counters.

| Requested Gbit/s each way / message | Pre-push → notification, mean µs (L / R) | Pre-push → consumer observation, mean µs (L / R) |
| --- | ---: | ---: |
| 21 / Permit | 4.30 / 6.56 | 17.40 / 14.26 |
| 21 / Complete | 0.147 / 0.104 | 9.76 / 11.69 |
| 21 / Report | 7.08 / 3.31 | 11.40 / 12.75 |
| Unlimited / Permit | 4.41 / 6.87 | 18.84 / 14.93 |
| Unlimited / Complete | 0.146 / 0.104 | 11.03 / 12.02 |
| Unlimited / Report | 6.86 / 3.20 | 13.74 / 13.01 |

Notification delays are measurable, but these intervals do not establish the
throughput bottleneck. In particular, Complete's short notification interval
starts after it leaves the producer's outgoing queue. Its earlier wait until
the end-of-turn handoff is unmeasured here. The next diagnostic includes that
boundary.

All failed-push, censored-timestamp and pending-notification counts are zero;
each producer's enqueued and notified totals agree. Consumer counts differ
by up to 442 messages, or 0.181%, because snapshots and in-flight messages
cross interval boundaries. Raw counts and signed residuals are retained.
Observation minus notification is not direct wakeup latency: a running
consumer can handle a message before notification.

Instrumented delivery is 2.58 / 2.72 Gbit/s at 3G offered, 2.79 / 2.36 at
4G and 2.20 / 2.48 unlimited. The normal build gave 2.33 / 2.42,
2.68 / 2.70 and 2.22 / 2.57 respectively. Instrumentation and run variation
prevent treating these differences as gains. The overload decline remains.
All nine points have zero admission pressure, outer socket overflow,
inner UDP errors, malformed/invalid/duplicate/out-of-window/wrong-run/reordered
records and outer fragmentation.

The diagnostic passes 176 pure tests, 59 ignored kernel library regressions,
Clippy and formatting. Tests cover timing sums, failed pushes, local-message
exclusion and a consumer observing before notification. The production source
is unchanged.

## Outgoing sender queue timing (November 7)

The [second diagnostic](benches/sender-handoff-diagnostic-v2.patch) adds the
missing outgoing-queue boundary. Its first timestamp survives failed pushes;
only successful foreign handoffs contribute to the queue mean. Local messages
are excluded. The existing notification and consumer timestamps are unchanged.
[All nine release observations](benches/tap-sender-handoff-diagnostic-v2.json)
retain the same ascending rates, twenty-second duration and placement as above.

| Requested Gbit/s each way / message | Queue insertion → pre-push, mean µs (L / R) | Pre-push → notification, mean µs (L / R) | Pre-push → consumer, mean µs (L / R) |
| --- | ---: | ---: | ---: |
| 21 / Permit | 0.46 / 0.54 | 4.42 / 6.18 | 18.29 / 15.62 |
| 21 / Complete | 11.87 / 9.41 | 0.162 / 0.116 | 11.52 / 11.02 |
| 21 / Report | 0.12 / 0.09 | 6.81 / 3.32 | 13.78 / 11.63 |
| Unlimited / Permit | 0.45 / 0.60 | 4.34 / 7.05 | 18.85 / 16.12 |
| Unlimited / Complete | 11.98 / 9.61 | 0.154 / 0.115 | 11.85 / 12.19 |
| Unlimited / Report | 0.12 / 0.09 | 6.70 / 3.37 | 13.87 / 12.87 |

Completions wait materially longer than permits and reports before publication.
The worker creates them during sender service and defers their handoff until
after receive/TAP work. This establishes a scheduling delay, not its causal
throughput cost. The earlier notification experiment on an older sender was
inconclusive; the current sender needs a matched comparison.

Instrumented delivery is 2.24 / 2.57 Gbit/s at 4G, 2.75 / 2.68 at 8G and
2.51 / 2.64 unlimited. Extra clock reads and run variation prevent treating
differences against v1 or normal release as gains. Unlimited loss is
95.56 / 94.79%, with 226.9 / 221.3% daemon CPU. TAP TX drops are
2,105,080 / 1,851,524 SKBs, expiries 4,031 / 6,672 frames and queue drops
2,539 / 23,362; ACK-buffer drops account for 128 / 130 of the queue drops.

All retry, censor and pending counts are zero, and enqueued equals notified.
Consumer-count skew reaches 329 messages (0.137%) across snapshot boundaries.
Queue and observation means use separate intervals; adding them does not
produce a per-message latency distribution. All nine observations have zero
admission pressure, socket overflow, inner UDP errors, invalid/duplicate/
out-of-window/wrong-run/reordered records and outer fragmentation.

The diagnostic passes 176 pure tests, 59 kernel library tests, Clippy and
formatting. Tests cover retention of the first timestamp across retry and
exclusion of failed pushes and local messages. The compiled musl timing test
again makes zero clock syscalls on this host. Production source is unchanged.

## Earlier completion handoff: normal release comparison (November 7)

A [two-line experiment](benches/sender-early-handoff.patch) flushes sender
messages and notifications at the end of sender service, before receive/TAP
work. The existing turn-tail flush, bounded message sweep, destination FIFO,
pacing and queue limits remain unchanged. It adds no hot allocation or lock,
but can cause more notification syscalls.

[Twenty-two normal-release observations](benches/tap-sender-early-handoff.json)
compare this with `0c4b5ac`, without diagnostic timestamps. Four fresh-daemon
sweeps run in baseline/candidate/candidate/baseline order, each at 4G, 8G, 21G
and unlimited for twenty seconds per point. The same daemons persist within
each sweep. No builds or other tests overlap load.

| Requested Gbit/s each way | Baseline mean delivered Gbit/s | Candidate mean delivered Gbit/s | Baseline mean CPU % | Candidate mean CPU % |
| --- | ---: | ---: | ---: | ---: |
| 4 | 2.67 / 2.64 | 2.75 / 2.92 | 236.3 / 228.7 | 245.1 / 236.7 |
| 8 | 2.64 / 2.54 | 2.67 / 2.90 | 231.4 / 223.3 | 242.3 / 234.0 |
| 21 | 2.72 / 2.38 | 2.79 / 2.96 | 231.9 / 219.0 | 247.0 / 238.4 |
| Unlimited | 2.58 / 2.47 | 2.61 / 2.82 | 224.7 / 216.8 | 239.4 / 229.6 |

Reverse means improve 10.4–24.4%; forward means improve only 1.0–3.1%.
Between-run variation remains large. A longer baseline/candidate pair uses
sixty-second points at 21G, unlimited and 21G again:

| Build / requested Gbit/s each way | Delivered Gbit/s | Missing after drain % | Daemon CPU % |
| --- | ---: | ---: | ---: |
| Baseline / 21 | 2.752 / 2.575 | 86.81 / 87.64 | 235.6 / 223.9 |
| Candidate / 21 | 2.809 / 3.249 | 86.53 / 84.43 | 257.4 / 249.3 |
| Baseline / unlimited | 2.848 / 2.836 | 95.05 / 94.86 | 241.6 / 232.7 |
| Candidate / unlimited | 2.758 / 3.195 | 95.31 / 94.45 | 255.0 / 247.3 |
| Baseline / 21 return | 3.020 / 2.848 | 85.50 / 86.36 | 253.4 / 240.5 |
| Candidate / 21 return | 3.039 / 3.325 | 85.44 / 84.06 | 270.1 / 256.6 |

The longer pair confirms higher reverse delivery, but forward unlimited
goodput falls 3.2% and CPU rises. This does not establish a general
bidirectional win. The extra flush remains outside production. Its separate
notification-syscall cost is unmeasured. A shorter queue wait alone is not
enough to adopt the scheduling change.

Drop attribution for the sixty-second unlimited points:

| Build | TAP TX drops (SKBs) | Expired frames | Queue drops | ACK-buffer drops (subset) |
| --- | ---: | ---: | ---: | ---: |
| Baseline | 6337246 / 5997588 | 29101 / 28095 | 6461 / 69545 | 511 / 398 |
| Candidate | 6508178 / 6227400 | 23374 / 21895 | 4465 / 78435 | 660 / 315 |

All twenty-two points have zero admission pressure, socket overflow,
inner UDP errors, invalid/duplicate/out-of-window/wrong-run/reordered records
and outer fragmentation. Raw observations retain actual offered load,
per-worker CPU and every drop counter.

The experiment passes 341 ordinary release tests, Clippy, formatting and
66 short kernel/integration checks. Its regression fails on the baseline
because a completion remains queued when sender service returns; the candidate
publishes it and wakes its owner before other worker work. Reconnect ACK
readiness takes 20.813 seconds in the foreign-namespace test; the existing
service-delay gap remains. Full routed PMTU and default-feature checks were
not repeated for this unadopted experiment.

## Sharing a bounded grant pass: unadopted experiment (November 7)

The owner uses one timestamp for its request scan. The first successful grant
advances the pacing deadline, so later workers cannot fill the same available
quantum in that pass. A two-worker regression reproduces this: two 1280-byte
requests fit the current window and quantum, but only one receives permission.

The [isolated candidate](benches/sender-grant-pass.patch) lets one explicit
request pass share that burst. Its first grant meets the ordinary deadline;
additional grants append their service time. All existing window, quantum,
metadata, probe and work limits remain. A changed rate or burst cap, another
pass, or issuance closure ends continuation. Unused completions do not refund
pacing time. Public strict reservation APIs are unchanged.

[The release measurements](benches/tap-sender-grant-pass.json) contain nine
ascending twenty-second candidate points and a fresh baseline/candidate pair,
each with sixty-second 21G, unlimited and 21G-return points. Daemons persist
within each sweep. The usual eight-worker QinQ profile, active discovery,
1452-byte outer UDP ceiling and CPU placement apply. No builds, other tests
or Git packing overlap load.

| Candidate requested Gbit/s each way | Delivered Gbit/s | Missing after drain % | Daemon CPU % |
| --- | ---: | ---: | ---: |
| 0.5 | 0.49 / 0.49 | 0.42 / 0.20 | 46.26 / 43.30 |
| 1 | 0.99 / 0.99 | 0.16 / 0.47 | 85.59 / 81.19 |
| 2 | 1.97 / 1.96 | 0.61 / 0.75 | 170.61 / 157.50 |
| 3 | 2.59 / 2.74 | 12.52 / 7.61 | 232.64 / 219.04 |
| 4 | 2.48 / 2.95 | 36.95 / 25.39 | 240.62 / 230.38 |
| 8 | 2.42 / 2.87 | 69.15 / 63.44 | 237.16 / 233.30 |
| 16 | 2.43 / 2.48 | 84.56 / 84.24 | 230.89 / 219.70 |
| 21 | 2.23 / 3.16 | 89.21 / 84.74 | 237.99 / 238.24 |
| Unlimited | 2.47 / 2.70 | 95.70 / 95.32 | 238.05 / 228.85 |

| Longer comparison | Delivered Gbit/s | Missing after drain % | Daemon CPU % |
| --- | ---: | ---: | ---: |
| Baseline / 21G | 2.24 / 2.41 | 89.22 / 88.24 | 217.10 / 210.15 |
| Baseline / unlimited | 2.60 / 2.90 | 95.48 / 94.71 | 242.41 / 232.92 |
| Baseline / 21G return | 2.74 / 3.04 | 86.86 / 85.45 | 250.35 / 241.73 |
| Candidate / 21G | 2.82 / 2.60 | 86.48 / 87.54 | 243.92 / 235.39 |
| Candidate / unlimited | 2.77 / 2.88 | 95.08 / 94.99 | 246.31 / 237.53 |
| Candidate / 21G return | 2.95 / 2.85 | 85.84 / 86.30 | 254.31 / 243.03 |

The candidate changes goodput by +25.7 / +7.7% at the first 21G point,
+6.4 / −0.9% unlimited, and +7.5 / −6.1% on return. The ascending candidate
sweep still loses goodput under load: unlimited is 4.6% below its forward 3G
peak and 14.5% below its reverse 21G peak. Run-order and controller variation
remain substantial. These results establish neither a repeatable bidirectional
gain nor a CPU ceiling, so the change remains outside production.

At unlimited load, baseline/candidate TAP ingress drops are
6388534 / 5967724 versus 6217682 / 6243524 SKBs; expired frames are
23977 / 31843 versus 21831 / 38684; queue drops are 3120 / 73404 versus
861 / 86703. ACK-buffer drops, included in those queue totals, are
175 / 171 versus 357 / 364. Every point has zero admission pressure, socket
overflow, inner UDP errors, corruption, duplicates, reordering and outer
fragmentation. Counter units differ and cannot be summed into exact frame loss.

The candidate passes 349 all-feature and 340 default-feature release tests,
Clippy, formatting and 65 short kernel/integration tests. Regressions cover
reverse completion order and per-packet inflight accounting, stale passes,
changed limits, unused grants, PTO allowances, quantum limits, overflow and
worker/group fairness. Full routed PMTU and Linux 7.0 checks were not repeated
for this unadopted candidate.

After these inconclusive scheduling experiments, work returns to the remaining
stage-3 bounded recovery integration. Stage 5 must revisit profiling, the
overload plateau and the original performance gates. Missing recovery does not
explain away the current sender regression.

## Request fairness and burst-size experiments (November 7)

Two normal-release sweeps use the ACK-pruning baseline, twenty-second points,
retained daemons and the same placement as above. Neither uses temporary
instrumentation, and no builds or tests overlap measurement.

[Advancing priority after the first grant](benches/tap-sender-fairness.json)
fixes deterministic worker starvation. The single-group regression previously
selected worker 0 six times while worker 1 remained ready; it now alternates.
A second regression checks three ready workers across two path groups.

| Requested Gbit/s each way | Actual offered Gbit/s | Delivered Gbit/s | Missing after drain % | Daemon CPU % |
| --- | ---: | ---: | ---: | ---: |
| 16 | 15.99 / 15.97 | 0.88 / 0.77 | 94.34 / 95.05 | 159.33 / 157.21 |
| 21 | 20.98 / 20.89 | 0.86 / 0.83 | 95.80 / 95.92 | 159.13 / 148.59 |
| Unlimited | 59.96 / 57.05 | 0.88 / 0.84 | 98.49 / 98.49 | 161.30 / 151.48 |
| 16 return | 15.98 / 15.97 | 0.87 / 0.83 | 94.40 / 94.64 | 162.23 / 149.11 |

This intermediate change reduces goodput. At 21G, worker 0 receives
34,439 / 31,060 grants and cancels 33,305 / 30,204, with zero grant expiry.
Other workers have almost no cancellations. Inspection identifies repeated
control overtaking: the loop prepares lane-0 DATA, then sends ACKs with larger
packet numbers before its grant arrives. The stale DATA grant is rejected.
Fairer access gives this path more grants, exposing more wasted pacing credit.

At 21G, total endpoint counters report 756,926 / 747,544 TAP TX drops,
103,783 / 137,480 frame expiries and 163 / 9,085 worker queue drops.
Every point has zero admission pressure, socket overflow, inner UDP errors,
invalid/duplicate/out-of-window/wrong-run records, reordering and outer fragments.
The fairness change passes 330 release tests, default-feature tests, Clippy
and 51 short kernel tests. One reconnect takes 19.19 seconds; the existing
service-delay gap remains. Full routed discovery subsequently passes with the
control-overtaking fix, as recorded above.

[Increasing only the DATA cap from 16 to 64 maximum datagrams](benches/tap-sender-current-cap64.json)
uses the original request priority. It includes the current loss-before-PTO
barrier, unlike the earlier instrumented cap experiment. BBR's quantum remains
at most 64 KiB; the larger cap does not change the congestion window or packet MTU.

| Requested Gbit/s each way | Actual offered Gbit/s | Delivered Gbit/s | Missing after drain % | Daemon CPU % |
| --- | ---: | ---: | ---: | ---: |
| 16 | 15.98 / 15.99 | 1.81 / 1.90 | 88.50 / 87.90 | 182.25 / 178.19 |
| 21 | 20.94 / 20.92 | 1.78 / 1.90 | 91.34 / 90.77 | 180.56 / 182.68 |
| Unlimited | 61.51 / 55.08 | 1.88 / 1.84 | 96.90 / 96.60 | 186.05 / 183.87 |
| 16 return | 15.59 / 15.76 | 1.89 / 1.88 | 87.66 / 87.89 | 187.09 / 181.00 |

The larger burst raises 21G goodput from 1.18 / 1.20 to 1.78 / 1.90 Gbit/s
while using slightly less CPU. This is a measured gain, still far below target.
At 21G, endpoint counters report 715,178 / 677,696 TAP TX drops,
41,777 / 62,350 frame expiries, 15,968 / 45,415 worker queue drops and
23,036 / 23,000 canceled grants. There are eight inner UDP receive errors on
the right at that point; other points have none. All points have zero admission
pressure, outer socket overflow, invalid/duplicate/out-of-window/wrong-run records
and outer fragmentation. Reordering stays below 0.0013%.

## Exact sender timing and notification experiment (November 7)

[Two instrumented pairs](benches/tap-sender-notify-diagnostic.json) measure
phase transitions and successful-grant transit directly. The isolated build
includes the experimental 64-datagram cap and ACK pruning, but predates the
production loss-before-PTO barrier. Each point requests 21 Gbit/s each way
for ten seconds. No builds or tests overlap measurement.

Exact phase totals put 87–88% of baseline time in Open, 7–9% in Closing,
about 0.4% in Commit and 3–4% in Feedback. These totals include the drain
interval. Open includes pacing waits and flight/request transit; it does not
mean sending was unconstrained. Older sampled-state phase counters cannot
attribute the interval preceding a sample reliably.

| Build / point | Delivered Gbit/s | Daemon CPU % | Grant to pre-send µs | Pre-send to owner receipt µs |
| --- | ---: | ---: | ---: | ---: |
| Baseline / 1 | 1.89 / 1.76 | 185.31 / 181.23 | 17.68 / 18.41 | 39.79 / 39.30 |
| Baseline / 2 | 1.90 / 1.88 | 189.52 / 186.29 | 17.85 / 18.44 | 40.40 / 39.64 |
| Earlier notifications / 1 | 1.87 / 1.91 | 187.34 / 180.23 | 16.77 / 15.98 | 41.35 / 34.91 |
| Earlier notifications / 2 | 1.85 / 2.17 | 196.76 / 194.12 | 16.22 / 15.79 | 37.40 / 35.26 |

The treatment flushes sender messages and worker notifications before the
remaining receive/TAP work, retaining the turn-tail flush. Timing means count
successful-prefix completions; the second interval includes both the syscall
and completion handoff. There is no consistent bidirectional throughput gain,
and additional notification syscall cost was not isolated. The change remains
outside production. The JSON retains loss, per-worker CPU and drop attribution.

## ACK-pruning normal release sweep (November 7)

[Four normal release observations](benches/tap-ack-prune.json) measure the
ACK-boundary change on `2692895`, retaining the production 16-datagram burst
cap. The profile and twenty-second points match the preceding sweep.

| Requested Gbit/s each way | Actual offered Gbit/s | Delivered Gbit/s | Missing after drain % | Daemon CPU % |
| --- | ---: | ---: | ---: | ---: |
| 16 | 15.96 / 15.95 | 1.17 / 1.14 | 92.41 / 92.60 | 191.58 / 187.77 |
| 21 | 20.96 / 20.94 | 1.18 / 1.20 | 94.27 / 94.20 | 194.74 / 190.37 |
| Unlimited | 58.69 / 55.17 | 1.16 / 1.22 | 97.99 / 97.76 | 195.89 / 193.02 |
| 16 return | 15.96 / 15.78 | 1.12 / 1.15 | 92.83 / 92.59 | 186.94 / 188.18 |

This confirms no large throughput gain from pruning: delivery remains
1.12–1.22 Gbit/s with roughly two CPUs per daemon. Unlimited load does not
collapse in this sweep, but the low plateau remains an integration regression.

All points have zero admission pressure, socket overflow, inner UDP receive
errors, invalid/duplicate/out-of-window/wrong-run records and outer fragments.
Maximum reordering is 0.00075%. At 21G, L/R counters report
721,802 / 684,420 TAP TX drops, 337,813 / 362,143 frame expiries and
22,101 / 54,549 worker queue drops. These units cannot be summed.
Of 186,192 / 193,691 grants, 927 / 443 expired and 4,130 / 8,706 were canceled.

The change passes 328 release tests, default-feature tests, Clippy, Rust 1.89
and 53 kernel tests, including foreign rename, namespace move and reconnect.
The routed test confirms 9000 → 1500 → 1280 → 9000 in 652.67 seconds with
returned/filtered PTB and no outer fragments. One reconnect took 19.99 seconds;
that service delay remains unresolved.

## Sender scheduling release sweep (November 7)

[Four normal release observations](benches/tap-sender-turn.json) measure
`f0ccac0` with the same profile, twenty-second duration and retained daemons
as the first paced sender sweep. Temporary diagnostics are absent. No builds
or other tests overlap the workload.

| Requested Gbit/s each way | Actual offered Gbit/s | Delivered Gbit/s | Missing after drain % | Daemon CPU % |
| --- | ---: | ---: | ---: | ---: |
| 16 | 15.97 / 15.89 | 1.11 / 1.13 | 92.93 / 92.76 | 190.99 / 185.30 |
| 21 | 20.96 / 20.70 | 1.10 / 1.14 | 94.64 / 94.39 | 188.37 / 186.44 |
| Unlimited | 61.22 / 57.16 | 1.12 / 1.17 | 98.13 / 97.93 | 193.50 / 190.89 |
| 16 return | 15.98 / 15.97 | 1.16 / 1.18 | 92.60 / 92.45 | 197.57 / 195.50 |

Compared with `c999431`, this includes loss-before-PTO ordering, blocked-plan
preservation, bounded coordinator draining and same-turn handoff. The 21G point
improves from 0.76 / 0.70 to 1.10 / 1.14 Gbit/s. Their individual effects are
not isolated by this sweep. Unlimited load does not reduce goodput in this run;
the low CPU utilization still rules out treating this as the CPU ceiling.

All points have zero admission pressure, outer socket overflow, inner UDP receive
errors, malformed payloads, duplicate delivery and outer fragmentation.
Occasional reordering reaches 0.0021%. At 21G, endpoint L/R counters report
724,174 / 679,880 TAP TX drops, 341,490 / 406,065 expired frames and
17,742 / 51,627 worker queue drops. Of 168,905 / 178,758 send grants,
1,318 / 708 expired and 4,365 / 8,233 were canceled. Drop units differ.
The combined build passes 324 release tests and 51 short kernel tests.
One reconnect resumed ACK progress after 8.91 seconds; the earlier 28.90-second
observation still demonstrates an unresolved service-delay risk.

### Rejected grant shrinks later batches

A regression test reproduces a scheduling defect: a closed sender trims a
prepared sixteen-packet request to ten packets before rejecting the grant.
Even when later credit fits sixteen, the request retains only ten. Reserving
the prefix now restores the original plan on rejection; the same test then
receives all sixteen packets in its grant.

[Instrumented observations](benches/tap-sender-prefix-diagnostic.json) compare
`c999431` with this fix alone. Both builds contain identical temporary counters
and receive one statistics signal per second. Each pair uses two consecutive
ten-second points at 21 Gbit/s requested each way, with the same QinQ profile
as the release sweep above. Builds and other tests were stopped during measurement.

| Build / point | Delivered Gbit/s | Missing after drain % | Daemon CPU % |
| --- | ---: | ---: | ---: |
| Instrumented baseline / 1 | 0.68 / 0.55 | 96.52 / 97.22 | 163.51 / 145.46 |
| Instrumented baseline / 2 | 0.71 / 0.65 | 96.41 / 96.72 | 165.14 / 152.15 |
| Preserve rejected batch / 1 | 0.98 / 0.95 | 95.10 / 95.31 | 166.44 / 164.00 |
| Preserve rejected batch / 2 | 0.97 / 0.95 | 95.18 / 95.31 | 174.71 / 172.16 |

The regression test proves the defect. The measured gain leaves throughput far
below the target. Sampled waiting time is concentrated in send-commit/feedback
serialization and pacing; sampling storage never fills. These timings are
approximate observations at coordinator visits, not exact phase-transition
measurements. The diagnostic builds' hashes were not captured, so these points
are exploratory evidence and do not replace the normal release acceptance sweep.

Adding bounded coordinator draining and end-of-turn message handoff to the
instrumented prefix fix delivered 1.07 / 1.13 and 1.11 / 1.06 Gbit/s in two
more matched points. Missing fractions were 94.67 / 94.36% and 94.51 / 94.68%;
daemon CPU was 186.17 / 181.79% and 186.19 / 183.06%.
The linked diagnostic file records these observations and this build's hash.
The coordinator previously processed only one completed flight and one report
per group per turn, even with work budget remaining. It now drains ready work
within the same 64-unit budget; reports that cannot fit wait for the next turn.
Requests and completions also reach handoff queues before the notification flush
in the turn that creates them. Two regressions verify draining and budget limits.
The measurements combine these scheduling changes and do not isolate their
individual gains. The throughput target remains unmet.

An additional diagnostic build raised the extra burst cap from sixteen to
sixty-four maximum-sized datagrams while retaining BBR's quantum (at most
65,536 bytes), window, pacing rate and 100-microsecond grant lifetime.
It delivered 1.81 / 1.87 and 1.95 / 1.93 Gbit/s, with 182.16 / 178.33%
and 191.28 / 187.37% daemon CPU. Missing fractions remained 90.39 to 91.03%.
Admission pressure and outer socket overflow stayed zero. Main retains the
sixteen-datagram policy: this experiment identifies batching sensitivity but
does not establish an acceptable burst policy or meet the performance target.
The linked diagnostic file includes its hash and counters.

### ACK application work

[ACK scan diagnostics](benches/tap-ack-scan-diagnostic.json) use the experimental
cap64 build with counters for admitted ACKs, bitmap reads, application calls and
new callbacks. Each ACK averaged 47.6 to 49.0 application bitmap reads and
31.6 to 33.6 newly acknowledged packets, requiring 2.04 to 2.11 bounded calls.
This workload does not establish that ACKs scan the entire retained window.

Pruning already-acknowledged range boundaries during full validation reduced
application reads to 2.01 to 2.10 and calls to 1.04 to 1.06 per ACK. New callbacks
remained near 32. Original range validation still visits about 47 words per ACK;
lost originals remain eligible for late delivery accounting, and the original
largest packet still determines RTT eligibility. Tests cover duplicate/new-tail
ACKs, late lost originals, invalid holes, ring wrap and maximum packet numbers.

Baseline diagnostic goodput was 2.00 / 1.96 and 1.80 / 1.81 Gbit/s; pruning
delivered 1.90 / 2.06 and 1.93 / 1.98. CPU stayed near 1.8 to 1.9 cores per
daemon. The reduction in repeated application work is verified; these short
runs do not establish a throughput gain beyond run variation.

The verified facts are low delivered throughput, low CPU utilization and heavy
upstream dropping/expiry. Grant and feedback serialization, congestion-window
behavior and worker wakeups need attribution. Missing repair alone does not
explain this result. No performance target is waived.

## Bounded transmit expiry (November 6)

[Twelve release observations](benches/tap-expiry.json) compare `edf2513`
with `ad17f54` (indexed transmit expiry) and include an intermediate heap prototype.
The final change bounds cleanup to 64 expired frame views per worker turn,
including prepared batches, and defers remaining expired data without sending
it. Exact deadlines replace repeated scans of all retained frames. Heap
updates visit at most 13 levels; pending-queue removal is constant-time.
The heap allocates only at startup. Its two compact indices and queue links
use the same per-frame metadata bytes as the old links on 64-bit builds.

All three builds use the same musl release profile, dependencies, generator,
kernel and 60-second QinQ workload as the ACK comparison below. Each sweep
retains its daemons and sockets across 16G, 21G, unlimited and 16G return.
No owned builds or other tests overlap load. These are single sweeps, not
the five successful repeats required for acceptance.

| Build / requested Gbit/s each way | Actual offered Gbit/s | Delivered Gbit/s | Missing after drain % | Daemon CPU % |
| --- | ---: | ---: | ---: | ---: |
| Before / 16G | 15.96 / 15.97 | 7.80 / 14.73 | 50.98 / 7.45 | 448.13 / 419.44 |
| Before / 21G | 20.95 / 20.94 | 7.98 / 17.27 | 61.78 / 17.26 | 478.31 / 477.50 |
| Before / unlimited | 52.75 / 39.98 | 7.54 / 16.71 | 85.66 / 58.07 | 470.56 / 472.70 |
| Before / 16G return | 15.97 / 15.97 | 7.56 / 14.74 | 52.45 / 7.37 | 450.03 / 410.03 |
| Indexed expiry / 16G | 15.97 / 15.97 | 7.76 / 14.88 | 51.25 / 6.56 | 455.72 / 424.20 |
| Indexed expiry / 21G | 20.96 / 20.96 | 7.76 / 16.70 | 62.84 / 20.08 | 476.73 / 479.59 |
| Indexed expiry / unlimited | 52.25 / 42.08 | 7.55 / 17.21 | 85.49 / 58.96 | 477.36 / 488.67 |
| Indexed expiry / 16G return | 15.96 / 15.98 | 7.63 / 14.71 | 52.05 / 7.65 | 445.77 / 418.62 |

The final unlimited point declines 2.7% forward and rises 3.1% reverse from
21G. Compared with the matched baseline, its paced goodput is 2.7 / 3.3%
lower, while unlimited goodput is 0.2 / 3.0% higher. These mixed results
establish no general throughput gain. The cleanup bound is a correctness
and scheduling improvement; the overload plateau and CPU ceiling remain open.

The intermediate prototype ordered equal deadlines by frame slot. It delivered
7.67 / 16.77 Gbit/s paced and 7.45 / 16.44 unlimited. The final heap compares
only deadlines, avoiding unnecessary work on equal timestamps. All four
prototype observations remain in the JSON; one reports 0.000054% reverse
reordering. Baseline and final observations report no reordering.

At unlimited load, drop attribution is:

| Build | TAP ingress SKBs | Socket overflow SKBs | Worker queue-drop events | Expired frames | Inner UDP receive-buffer errors |
| --- | ---: | ---: | ---: | ---: | ---: |
| Before | 5230614 / 2343107 | 720624 / 3252 | 10 / 1196 | 0 / 0 | 2743 / 0 |
| Indexed expiry | 5170437 / 2520275 | 1383217 / 3143 | 0 / 2779 | 0 / 0 | 1976 / 124 |

Admission pressure, invalid/duplicate/out-of-window/wrong-run payloads and
outer fragmentation are zero in all twelve observations. TAP and socket
counters count SKBs, queue events may discard aggregates, and expiry counts
frames; they cannot be summed into an exact inner loss total. The final run's
higher socket overflow remains unresolved. The JSON includes every point's
counters, per-thread CPU, generator/receiver CPU and per-core softirq time.

All three builds concentrate QinQ super-frame reads on worker 3 at both
endpoints. That worker also receives substantial outer traffic on the left,
while the right worker receives only control traffic. This placement remains
a verified imbalance; the expiry change does not remove it. Neither missing
recovery nor congestion control explains away the remaining decline.

## ACK-enabled release comparison (November 6)

[Eight observations](benches/tap-ack-control.json) compare `623beb5`, immediately
before runtime ACK emission, with `8ac7608`. Both use musl release builds with
full LTO, one codegen unit and identical dependencies. Each sweep keeps the same
daemons, TAPs, sockets and private namespaces through four 60-second points;
PID/start-time checks confirm persistence. No owned build or other test overlaps
load.

Both run on Linux 6.8.0-149-generic, Intel Xeon Gold 6138, with eight workers
per daemon on CPUs 0–7 and 8–15. Separate generator/receiver CPUs are 16/17
and 18/19. Sixteen flows each way carry 1472-byte UDP payloads in 1522-byte
QinQ frames, using 44-segment sources, 500-entry TAP queues and active discovery
capped at 1452 outer UDP bytes. Goodput excludes drain; missing fractions include
it. A CPU value of 100% means one CPU.

| Build / requested Gbit/s each way | Actual offered Gbit/s | Delivered Gbit/s | Missing after drain % | Daemon CPU % |
| --- | ---: | ---: | ---: | ---: |
| Before ACK / 16G | 15.94 / 15.96 | 8.16 / 14.82 | 48.63 / 6.81 | 363.47 / 356.32 |
| Before ACK / 21G | 20.87 / 20.86 | 8.67 / 17.07 | 58.28 / 17.85 | 408.39 / 428.52 |
| Before ACK / unlimited | 50.76 / 39.94 | 8.55 / 17.74 | 83.09 / 55.43 | 408.28 / 428.99 |
| Before ACK / 16G return | 15.97 / 15.97 | 8.15 / 15.06 | 48.77 / 5.43 | 366.25 / 355.56 |
| ACK enabled / 16G | 15.95 / 15.95 | 8.16 / 14.84 | 48.64 / 6.67 | 442.52 / 420.40 |
| ACK enabled / 21G | 20.95 / 20.95 | 8.04 / 17.65 | 61.49 / 15.47 | 468.42 / 474.64 |
| ACK enabled / unlimited | 49.67 / 40.42 | 7.83 / 16.89 | 84.18 / 58.05 | 461.91 / 477.47 |
| ACK enabled / 16G return | 15.93 / 15.93 | 7.77 / 14.81 | 51.06 / 6.75 | 450.64 / 429.14 |

The first 16G point delivers similar goodput with 21.7 / 18.0% more daemon CPU
in the ACK-enabled build. At unlimited load its CPU use is 13.1 / 11.3% higher,
while goodput is lower than the matched baseline. This build also includes
quota-sized frame stores; the comparison does not isolate every change.
Neither build reaches the target. The new ACK-enabled decline and its CPU cost
remain performance work, not an accepted tradeoff that closes the gate.

Drop counts below use endpoint L/R order; the goodput table uses direction
L→R / R→L. TAP and outer-socket drops count SKBs; worker queue-drop events may
discard aggregates, while expiry counts frames. They cannot be summed into
an exact frame-loss total.

| Build / load | TAP TX drops | Outer socket overflow | Worker queue drops | Expired frames | Inner UDP receive-buffer errors |
| --- | ---: | ---: | ---: | ---: | ---: |
| Before ACK / 21G | 1402494 / 134015 | 546743 / 7669 | 968 / 9914 | 5327 / 20523 | 95006 / 1311 |
| Before ACK / unlimited | 4880618 / 2306319 | 512173 / 4638 | 0 / 2826 | 0 / 0 | 6011 / 205 |
| ACK enabled / 21G | 1489314 / 66213 | 771023 / 4874 | 0 / 1619 | 0 / 0 | 1487 / 71 |
| ACK enabled / unlimited | 4837971 / 2404384 | 1000758 / 7041 | 764 / 3552 | 2340 / 10341 | 1453 / 33 |

All eight points have zero admission pressure, receiver corruption, duplicates,
out-of-window records, wrong-run records, reordering and outer fragmentation.
The JSON includes per-flow results, per-worker counters, thread CPU and per-core
softirq CPU. At unlimited load, summed softirq use on the assigned twenty CPUs
is 23.76% before ACKs and 30.88% with ACKs, separately from daemon CPU.

All super-frame reads concentrate on worker 3 at both endpoints. In the
ACK-enabled unlimited point, left worker 3 reads 914237 super-frames and receives
25.26 GB of outer traffic; right worker 3 reads 2276048 and receives only control
traffic. The left reader is near 99% CPU while worker 5 is nearly idle. These
counters verify concentration and shared read/receive work. The older flush32
sweep placed all left super-frame reads on worker 5, which received only control
traffic. That placement difference is verified; its full throughput cost is not.
These observations do not establish the CPU maximum. Scheduler
profiling and controlled redistribution remain necessary; missing recovery is
not sufficient attribution.

One sweep per build does not satisfy the repeatability gate. The older higher
rates used a different kernel build and TAP queue placement and are retained as
historical observations, not a matched control for ACK overhead.

## Compact transmit storage and source ownership (November 6)

The next experiment replaces one 64-KiB buffer per expanded frame with
bounded frame views. UDP segments borrow their source payload and retain
small rewritten headers; ordinary frames share compact slabs. The original
global pending queue was unsuitable for this layout: it let new arrivals
overtake an older deferred frame and repeatedly scanned queued frames.
A regression reproduces the inversion. Per-lane FIFOs fix it while keeping
64-frame admission/expiry budgets and round-robin lane service.

The storage-only prototype is rejected. Its QinQ delivery fell to about
3.6–3.9 Gbit/s in one direction above the 8-Gbit/s point. FIFO service
removes that severe collapse and measured QinQ reordering, but does not
by itself establish a throughput improvement over the previous layout.

A sampled FIFO profile then found 490–626 queued frame views on two owners
while those owners still averaged 121–122 free physical buffers. Their
shared TAP reader averaged only 12.7 free buffers: retained foreign sources
still consumed the reader's pool. The deadline/arming block consumed about
2.4–2.9% of CPU on those two owners; other corrected samples were within
clock-noise range. This supports fixing source ownership before adding a
deadline cache.

The ownership-exchange candidate swaps an empty owner buffer for the
reader's retained source, preserving the original payload address. Both
sides keep their fixed pool entitlement; delayed returns retain their
credits. Insufficient owner capacity rejects the source and increments
`queue_drops`. No buffer pool or deadline is enlarged.

All rows below are simultaneous 60-second release tests with eight workers
per daemon, 16 flows each way and 500-entry TAP queues. The outer UDP ceiling
is 1452 bytes; discovery remains enabled. QinQ and single-tag UDP use
44-segment sources. SRv6 uses tagged, separately encapsulated IPv6 packets.
Actual offered rate, CPU, per-worker counters and binary hashes are in
[tap-transmit-storage.json](benches/tap-transmit-storage.json).
Fresh process/socket placement can differ between observations; these
single observations are not sufficient to attribute every throughput change.

| Observation | Actual offered Gbit/s | Delivered Gbit/s | Missing % | Daemon CPU % | Reordered % |
| --- | ---: | ---: | ---: | ---: | ---: |
| storage-base-qinq-0 | 45.78 / 42.40 | 12.90 / 14.03 | 71.71 / 66.80 | 477.7 / 485.0 | 10.007 / 0.278 |
| storage-qinq-0 | 7.96 / 7.94 | 7.92 / 7.85 | 0.23 / 0.82 | 267.4 / 246.0 | 1.014 / 0.006 |
| storage-qinq-1 | 15.91 / 15.91 | 3.85 / 15.13 | 75.72 / 4.57 | 349.6 / 296.6 | 12.505 / 0.458 |
| storage-qinq-2 | 20.91 / 20.91 | 3.64 / 15.81 | 82.50 / 24.14 | 366.0 / 363.5 | 12.744 / 15.470 |
| storage-qinq-3 | 57.94 / 42.72 | 3.91 / 16.46 | 93.22 / 61.34 | 375.3 / 375.5 | 12.673 / 12.782 |
| fifo-storage-qinq-0 | 7.94 / 7.91 | 7.44 / 6.74 | 5.31 / 13.88 | 247.3 / 229.1 | 0.000 / 0.000 |
| fifo-storage-qinq-1 | 15.89 / 15.91 | 12.30 / 13.47 | 22.25 / 14.98 | 454.9 / 440.2 | 0.000 / 0.000 |
| fifo-storage-qinq-2 | 20.83 / 20.75 | 11.83 / 13.88 | 42.93 / 32.82 | 450.6 / 468.5 | 0.000 / 0.000 |
| fifo-storage-qinq-3 | 46.80 / 44.15 | 12.25 / 13.94 | 73.72 / 68.30 | 457.5 / 479.1 | 0.000 / 0.000 |
| exchange-qinq-0 | 7.99 / 7.98 | 7.95 / 7.89 | 0.11 / 0.83 | 258.0 / 246.3 | 0.000 / 0.000 |
| exchange-qinq-1 | 15.94 / 15.94 | 14.15 / 13.45 | 10.92 / 15.34 | 474.3 / 475.5 | 0.000 / 0.000 |
| exchange-qinq-2 | 20.79 / 20.82 | 16.30 / 13.33 | 21.32 / 35.75 | 519.5 / 521.7 | 0.000 / 0.000 |
| exchange-qinq-3 | 35.53 / 46.73 | 18.60 / 11.96 | 47.31 / 74.25 | 545.6 / 528.3 | 0.000 / 0.000 |
| storage-base-tag-0 | 20.79 / 20.80 | 16.29 / 14.70 | 21.34 / 29.12 | 638.6 / 639.6 | 56.468 / 45.115 |
| storage-base-tag-1 | 29.33 / 29.07 | 15.51 / 15.57 | 46.86 / 46.20 | 682.6 / 693.4 | 47.843 / 43.516 |
| fifo-storage-tag-0 | 20.79 / 20.83 | 16.35 / 15.37 | 21.04 / 25.96 | 593.2 / 596.1 | 46.473 / 41.123 |
| fifo-storage-tag-1 | 30.76 / 30.50 | 16.69 / 15.63 | 45.37 / 48.41 | 695.5 / 684.9 | 38.668 / 35.897 |
| exchange-tag-0 | 20.85 / 20.86 | 16.58 / 15.48 | 20.17 / 25.50 | 596.7 / 599.7 | 47.262 / 40.126 |
| exchange-tag-1 | 33.59 / 35.41 | 16.38 / 14.02 | 51.03 / 60.25 | 626.8 / 633.0 | 33.772 / 37.835 |
| storage-base-srv6-0 | 4.98 / 4.98 | 4.56 / 4.90 | 8.13 / 1.23 | 430.8 / 403.0 | 0.323 / 0.206 |
| storage-base-srv6-1 | 7.06 / 6.93 | 5.71 / 5.89 | 18.82 / 14.73 | 525.3 / 484.4 | 0.488 / 3.184 |
| fifo-storage-srv6-0 | 4.98 / 4.98 | 4.40 / 4.89 | 11.28 / 1.29 | 451.4 / 411.0 | 0.000 / 0.000 |
| fifo-storage-srv6-1 | 6.95 / 7.13 | 5.32 / 6.01 | 23.04 / 15.28 | 572.2 / 514.0 | 0.000 / 0.001 |

The first exchange sweep improves QinQ aggregate delivery but still loses
reverse-direction goodput under unlimited load: 13.33 → 11.96 Gbit/s.
Single-tag reordering remains substantial. Neither result meets the gate.

Drop attribution for the exchange QinQ points follows. Values are left /
right daemon counters, not direction-paired frame losses. TAP and socket
drops count SKBs; source queue drops can discard multiple frames. Do not
sum these counters as though their units matched.

| Point | TAP TX drops | Worker queue drops | Admission pressure | Expired frames | Socket overflow | Inner UDP receive errors |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| exchange-qinq-2 | 12530 / 510652 | 464953 / 995 | 0 / 0 | 0 / 0 | 677179 / 43382 | 5881 / 26349 |
| exchange-qinq-3 | 1190358 / 3806425 | 717107 / 6100 | 0 / 0 | 0 / 16820 | 455456 / 49930 | 10410 / 23769 |

The right QinQ reader reads only one GSO source per worker turn. As reverse
receive delivery rises from 9.94 to 12.15 GB between those points, its source
reads fall from 1.900 to 1.605 million. The left reader handles almost no
receive delivery and grows from 2.395 to 2.924 million reads. This is a
verified scheduling asymmetry; its contribution to the goodput decline
requires the bounded source-batching comparison below.

The same validation exposed an independent reconnect bug: a successful
new handshake reused a failed PMTU path whose next probe was still in the
future. Frames expired before baseline confirmation resumed. Changing the
selected probe connection now invalidates old proofs and makes a fresh probe
due, preserving a failed path's zero DATA limit until confirmation succeeds.
Unchanged shared-session probes retain their history. The global probe
budget still applies; reconnect does not guarantee immediate confirmation
of every lane.

A later Linux 7.0 reconnect repeat caught a separate test precondition error:
carrier was active while the transmit PMTU was still failed. One frame
expired; no queue, admission or socket-overflow drop occurred. PMTU recovered
on a later probe. The log does not identify why the first probe went unanswered.
The device-lifecycle test now waits for usable PMTU in the current generation
on all four transmit lanes before injecting its single frame. A deterministic
test loses the first reconnect probe and checks continued DATA blocking until
the scheduled retry succeeds. Carrier alone does not promise lossless DATA
during baseline recovery.

Correctness checks pass on Linux 6.8.0-147-generic and Ubuntu 26.04.1 /
Linux 7.0.0-43-generic: kernel unit tests, runtime forwarding/reconnect,
external TAP renames and namespace moves. Storm checks observe no hot-path
allocations/deallocations, corruption or duplicates. They report bounded
queue drops under overload instead of assuming the queues cannot fill.
Both kernels also pass the full 650-second routed MTU sequence, including
the real 600-second upward retry, without captured outer fragments or capture
drops. Release/default tests, Clippy with warnings denied, Rust 1.89 checks
and formatting pass. A short-write regression now tests the common result validator directly;
the prior eventfd fixture returned a different error on Linux 7.0.
No kernel or distribution is treated as the sole platform contract.
The range-packer build repeats the full routed sequence on both kernels
(650.59 / 650.41 seconds). After the PMTU readiness correction, both runtime
tests pass on each kernel and the external-move/reconnect test passes three
additional Linux 7.0 repeats. Cross-compilation also passes for aarch64,
riscv64gc and powerpc64le Linux musl; these are compile checks, not runtime
compatibility evidence.

### Bounded source batches and persistent overload

Allowing up to four GSO source reads per batch reduced the busy QinQ
reader's read syscalls from 1.605 million to 0.434 million in the unlimited
observations. Delivery reached 20.34 / 12.20 Gbit/s, still below the
bidirectional requirement. The narrower owner-batch variant restores a
one-source read limit whenever an input needs expansion on the reader;
foreign inputs can grow the next batch from one to two to four sources.
Expansion still stops after 64 frames or 64 KiB.

| Variant / workload / load | Actual offered Gbit/s | Delivered Gbit/s | Missing % | Daemon CPU % |
| --- | ---: | ---: | ---: | ---: |
| four-source / QinQ / 21G | 20.85 / 20.84 | 16.31 / 13.83 | 21.47 / 33.40 | 532.5 / 556.7 |
| four-source / QinQ / unlimited | 31.47 / 43.78 | 20.34 / 12.20 | 35.14 / 72.03 | 579.3 / 567.2 |
| four-source / single tag / 21G | 20.69 / 20.72 | 14.95 / 14.19 | 27.47 / 31.29 | 570.8 / 586.7 |
| four-source / single tag / unlimited | 29.94 / 30.46 | 15.32 / 15.57 | 48.61 / 48.67 | 678.4 / 694.6 |
| owner-batch / QinQ / 21G | 20.85 / 20.87 | 16.27 / 13.64 | 21.70 / 34.39 | 526.8 / 550.1 |
| owner-batch / QinQ / unlimited | 32.53 / 43.40 | 20.05 / 12.07 | 38.14 / 72.09 | 563.3 / 558.0 |
| owner-batch / single tag / 21G | 20.88 / 20.92 | 14.21 / 12.72 | 31.68 / 38.98 | 508.6 / 492.7 |
| owner-batch / single tag / unlimited | 32.04 / 30.88 | 17.52 / 15.41 | 45.08 / 49.91 | 684.0 / 706.1 |
| owner-batch / SRv6 / 5G | 4.98 / 4.98 | 4.35 / 4.85 | 12.19 / 2.25 | 442.5 / 412.8 |
| owner-batch / SRv6 / unlimited | 6.74 / 6.69 | 5.06 / 5.96 | 24.69 / 10.68 | 572.4 / 510.3 |

The single-tag results are mixed, with substantial reordering; SRv6 shows
no general throughput improvement from compact storage. Two additional
four-source SRv6 observations used one generator segment per send. They
saturated the generators at 1.86–1.96 Gbit/s offered and delivered
1.85–1.95 Gbit/s. They are retained in the JSON as generator-limited controls,
not compared with the 44-segment transport ceilings.

The earlier multi-rate wrapper restarted the daemons at every point.
The harness now uses its existing inner rate loop, retaining the same TAPs,
sockets, daemons and namespaces. PID/start-time checks confirm this across
points. Separate generator run IDs reject late traffic from earlier points.
This removes transport restart as a confounder in overload/recovery sweeps.

The owner-batch persistent QinQ sweep is:

| Requested load | Actual offered Gbit/s | Delivered Gbit/s | Missing % | Daemon CPU % |
| --- | ---: | ---: | ---: | ---: |
| 16G | 15.94 / 15.95 | 14.17 / 13.34 | 10.78 / 16.10 | 477.2 / 469.2 |
| 21G | 20.90 / 20.91 | 16.61 / 13.78 | 20.27 / 33.90 | 532.9 / 555.9 |
| unlimited | 31.49 / 42.02 | 20.51 / 12.75 | 34.64 / 69.55 | 573.9 / 570.6 |
| 16G recovery | 15.93 / 15.95 | 14.09 / 13.36 | 11.25 / 15.98 | 466.4 / 466.3 |

Every point reports zero reordering. The reverse direction still declines
7.5% between 21G and unlimited load while forward delivery rises. Returning
to 16G restores the earlier goodput: this sweep shows recovery, not an
accumulating poisoned state. Shared reader/receive work remains a measured
asymmetry; its full contribution to the decline and the CPU ceiling are
not yet isolated.

### Receive delivery batching

The 16-completion intermediate drain fragmented receive-side USO groups.
Raising it to 32, while retaining the same 64-record/64-datagram/64-KiB
receive budgets and pressure drain/retry, increased the mean unlimited
QinQ group from 12.13 / 11.90 to 18.52 / 17.63 frames. TAP write calls fell
from 5.49 / 8.97 million to 3.78 / 6.04 million. These are separate
persistent sweeps, so socket placement can still affect their comparison.

| Workload / requested load | Actual offered Gbit/s | Delivered Gbit/s | Missing % | Daemon CPU % |
| --- | ---: | ---: | ---: | ---: |
| qinq / 16G | 15.96 / 15.96 | 14.43 / 13.88 | 9.25 / 12.70 | 472.5 / 463.0 |
| qinq / 21G | 20.83 / 20.89 | 16.68 / 14.43 | 19.65 / 30.70 | 529.2 / 548.3 |
| qinq / unlimited | 33.57 / 41.67 | 20.63 / 13.46 | 38.36 / 67.58 | 575.0 / 577.2 |
| qinq / 16G recovery | 15.95 / 15.96 | 14.39 / 13.73 | 9.36 / 13.61 | 463.4 / 460.8 |
| tag / 21G | 20.93 / 20.94 | 17.10 / 15.27 | 18.02 / 26.83 | 573.2 / 570.6 |
| tag / unlimited | 34.45 / 34.38 | 18.26 / 16.49 | 46.75 / 51.83 | 706.2 / 720.1 |
| srv6 / 5G | 4.98 / 4.99 | 4.56 / 4.93 | 8.21 / 0.74 | 457.1 / 419.9 |
| srv6 / unlimited | 6.87 / 6.71 | 5.24 / 5.98 | 23.46 / 10.67 | 587.6 / 516.1 |

The reverse QinQ direction still loses 6.7% between 21G and unlimited,
then recovers at 16G. Single-tag traffic improves at unlimited load but
reports 32–36% reordering. SRv6 remains about 5.24 / 5.98 Gbit/s unlimited.
Reducing writes helps this workload; it does not establish a CPU ceiling
or satisfy the throughput, loss or overload-stability gates.

Removing the intermediate drain entirely was also tested and reverted.
It kept the same receive budgets, pressure drain/retry and final TAP-interest
refresh after transmit work, and passed all 28 kernel tests. Its persistent
QinQ sweep was worse:

| Requested load | Actual offered Gbit/s | Delivered Gbit/s | Missing % | Daemon CPU % |
| --- | ---: | ---: | ---: | ---: |
| 16G | 15.97 / 15.95 | 14.07 / 13.51 | 11.52 / 14.99 | 467.9 / 459.8 |
| 21G | 20.87 / 20.92 | 16.07 / 13.17 | 22.76 / 36.80 | 525.9 / 523.7 |
| unlimited | 28.26 / 39.73 | 19.14 / 12.25 | 32.01 / 69.05 | 560.4 / 550.4 |
| 16G recovery | 15.89 / 15.90 | 14.25 / 13.44 | 10.00 / 15.21 | 473.3 / 468.9 |

The retained implementation uses the 32-frame intermediate threshold.
Final TAP delivery and readability updates remain after transmit processing:
transmission can release credits after an earlier update disabled TAP reads.
Moving that update before transmit could leave readable input disabled while
the worker sleeps.

### Per-worker evidence in the retained build

In the persistent 32-completion QinQ sweep, the right TAP reader (worker 5)
is already at 98% CPU under paced load. Unlimited load increases its receive
work while its source intake falls:

| Right reader, 60 seconds | 21G requested | Unlimited |
| --- | ---: | ---: |
| Total CPU % | 98.01 | 98.19 |
| User / system CPU % | 25.19 / 72.82 | 26.76 / 71.43 |
| GSO sources read | 2,178,214 | 1,957,130 |
| TAP read calls | 722,755 | 640,356 |
| Outer receive GB | 11.06 | 16.18 |
| TAP write calls | 302,459 | 445,019 |

The left reader receives almost no DATA and grows from 2,410,112 to
3,543,328 sources. This is a measured imbalance within the same running
processes. It supports the shared reader/receive CPU bottleneck hypothesis;
it does not isolate every kernel cost or prove that this alone accounts for
the 6.7% reverse-goodput decline.

The busiest left receive worker reaches 97% CPU, while another worker uses
20%. Left workers 2 and 3 record 536,455 and 541,448 queue drops under
unlimited load; their socket-overflow counts are 273,367 and 920,559.
Total daemon CPU remains 575% / 577% despite individual saturated cores.
Aggregate idle CPU therefore does not establish forwarding headroom.
Stage 5 must revisit work placement and shared-reader service with per-worker
measurements, alongside kernel I/O cost. Counter deltas include the drain
interval; CPU samples cover offered load.

### Tagged small-frame follow-up

These ten release observations use the same baseline and retained storage
binaries as the larger-frame comparison. Each run starts fresh processes
and sockets, then keeps them for a paced and unlimited point, 60 seconds
each. Eight workers per daemon, 16 flows per direction, 44 segments per
source, 500-entry TAP queues and a 1452-byte outer UDP ceiling are fixed.
The paced request is eight million frames/s per direction. A 64-byte frame
has one VLAN and 18 UDP payload bytes; a 128-byte frame has QinQ and
78 payload bytes. Gbit/s below counts UDP payload, not Ethernet overhead.
Full counters, per-flow counts, CPU samples and binary hashes are in
[tap-small-frame-storage.json](benches/tap-small-frame-storage.json).

| Observation | Actual offered Gbit/s | Delivered Gbit/s | Million frames/s | Missing % | Daemon CPU % |
| --- | ---: | ---: | ---: | ---: | ---: |
| small-base-tag-0 | 1.134 / 1.114 | 0.683 / 0.737 | 4.75 / 5.12 | 39.49 / 33.58 | 721.5 / 717.1 |
| small-base-tag-1 | 1.583 / 1.644 | 0.762 / 0.726 | 5.29 / 5.04 | 51.71 / 55.66 | 728.9 / 720.6 |
| small-storage-tag-0 | 1.141 / 1.143 | 0.381 / 0.513 | 2.65 / 3.56 | 65.99 / 54.34 | 466.9 / 467.8 |
| small-storage-tag-1 | 1.769 / 1.651 | 0.437 / 0.531 | 3.03 / 3.69 | 75.20 / 67.72 | 499.4 / 498.9 |
| small-storage-tag-repeat-0 | 1.132 / 1.137 | 0.683 / 0.738 | 4.74 / 5.12 | 39.46 / 34.86 | 710.0 / 707.0 |
| small-storage-tag-repeat-1 | 1.584 / 1.617 | 0.731 / 0.760 | 5.07 / 5.28 | 53.72 / 52.83 | 756.5 / 745.0 |
| small-base-qinq-0 | 4.954 / 4.961 | 2.112 / 0.630 | 3.38 / 1.01 | 57.21 / 87.26 | 350.8 / 385.1 |
| small-base-qinq-1 | 7.274 / 7.482 | 2.050 / 0.687 | 3.29 / 1.10 | 71.72 / 90.79 | 355.2 / 384.7 |
| small-storage-qinq-0 | 4.931 / 4.943 | 2.188 / 1.550 | 3.51 / 2.48 | 55.45 / 68.53 | 452.1 / 476.6 |
| small-storage-qinq-1 | 7.245 / 7.524 | 2.181 / 1.365 | 3.50 / 2.19 | 69.79 / 81.80 | 441.2 / 458.0 |

The first retained 64-byte run is much slower than the baseline. Four
workers per daemon approach saturation while the other four use only
27–31% CPU. A fresh run of the same binary restores roughly baseline
delivery and spreads work across all eight workers. This demonstrates
placement sensitivity; it does not isolate compact storage as the cause
of the first regression. Single-tag traffic still reorders: the repeated
unlimited run reports 25.1% / 25.5%.

The retained QinQ run improves reverse delivery over this baseline
placement, but its reverse rate falls 11.9% from paced to unlimited load.
This is another unresolved overload decline. Neither these single
comparisons nor the fresh-placement repeat establish a CPU ceiling.

| Retained unlimited run | TAP TX drops | Worker queue drops | Socket overflow | Inner UDP receive errors |
| --- | ---: | ---: | ---: | ---: |
| small-storage-tag-1 | 12539491 / 10265753 | 0 / 0 | 667951 / 112470 | 1066 / 0 |
| small-storage-tag-repeat-1 | 7783246 / 7754017 | 0 / 0 | 607422 / 554973 | 298 / 10095 |
| small-storage-qinq-1 | 10676868 / 13283654 | 252950 / 3561 | 144095 / 93945 | 0 / 165 |

Admission pressure and expired frames are zero in all ten observations.
All report zero corrupted, duplicate, too-old and wrong-run application
packets, and zero outer IPv6 fragment creations. Source/SKB drop counters
can each represent many frames and cannot be summed into frame loss.
Mixed traffic and non-coalescible small frames still need validation.

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
A later Linux 7.0 check accepts the initial no-offload send and delivers two
complete frames. The portability test now checks either that software delivery
or Linux 6.8's EIO with no output, then verifies byte-exact segmented and USO
delivery with offloads enabled. Both kernel versions pass. The earlier EIO
observation is specific to that kernel, not a universal prerequisite.

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

## VLAN and QinQ output coalescing

The runtime now coalesces compatible UDP frames behind up to two
802.1Q/802.1ad tags. It checks IP/UDP offsets after the tag stack and preserves
every tag byte, including PCP/DEI. The existing checksum checks, same-flow
barriers, aggregate bounds and ordinary-write fallback remain. The change
adds eight stack bytes for headers and no allocation, lock or syscall.

The kernel test compares single writes and coalesced writes byte for byte
across a veth for IPv4/IPv6, zero/one/two tags, 1/2/43/44 frames, short final
segments and IPv4 ID wrap. Ancillary VLAN metadata is reconstructed during
capture. It also compares input expansion against Linux segmentation.
Pure tests reject every truncation, bad UDP checksums and excess tag depth.

The harness accepts `VLAN_DEPTH=0|1|2` and creates actual VLAN uppers inside
the test namespaces. Frame size includes tags: use 1518 or 1522 for a
1500-byte inner IP packet. The Rust generator requires at least 16 payload
bytes, so a 64-byte double-tagged test must use a larger frame or another
generator. The original untagged default remains available for controls.

### Controlled tagged release comparison

[All eight observations](benches/tap-vlan-coalescing.json) use eight workers
per daemon, 16 flows each way, 20 seconds per point, 44-segment sends,
500-entry TAP queues and the same 20-core placement as the preceding runs.
The 1452-byte outer payload is a controlled diagnostic setting, not evidence
of PMTU. The final runtime must probe available sizes rather than assume
that setting or a 1500-byte access path.

| Tags / build | Request Gbit/s each way | Actual offered Gbit/s | Delivered Gbit/s | Missing after drain % | Daemon CPU % |
| --- | ---: | ---: | ---: | ---: | ---: |
| 1 / before | 21 | 20.709 / 20.512 | 5.260 / 5.100 | 74.15 / 74.71 | 697.0 / 705.1 |
| 1 / before | unlimited | 29.979 / 31.594 | 5.036 / 4.962 | 82.92 / 84.05 | 766.2 / 774.7 |
| 1 / after | 21 | 20.796 / 20.730 | 16.003 / 15.530 | 22.15 / 24.20 | 620.2 / 621.0 |
| 1 / after | unlimited | 26.949 / 27.623 | 15.477 / 15.335 | 41.88 / 43.81 | 672.3 / 677.7 |
| 2 / before | 21 | 20.787 / 20.929 | 2.714 / 2.815 | 86.79 / 86.40 | 197.9 / 196.7 |
| 2 / before | unlimited | 51.875 / 52.143 | 2.684 / 2.641 | 94.77 / 94.88 | 197.8 / 197.3 |
| 2 / after | 21 | 20.952 / 20.905 | 6.193 / 6.030 | 70.12 / 70.85 | 196.4 / 195.7 |
| 2 / after | unlimited | 52.916 / 53.332 | 6.173 / 6.276 | 88.21 / 88.10 | 197.6 / 196.1 |

Single-tag paced TAP writes fall from 18.65M / 19.54M to 2.91M / 3.09M
over the observation windows. Delivery improves about threefold while
daemon CPU falls. The new unlimited point still declines 3.28% / 1.25%
from paced delivery; this is not a verified plateau.

QinQ gains roughly 2.2–2.4 times in delivery, but uses only about two cores
per daemon. Worker counters show virtually all TAP input on one queue, with
the corresponding traffic on one transmit lane. The current worker-local
selection therefore leaves most transmit workers unused in this workload.
Compare strict hashing before changing that policy; the profile does not
prove that it will remove the reader bottleneck.

| Tags / build / request | TAP TX drops (SKBs) | Transport socket overflow (SKBs) | Inner UDP errors | Expired frames |
| --- | ---: | ---: | ---: | ---: |
| 1 / before / 21 | 258039 / 238219 | 763077 / 574088 | 9834632 / 10446166 | 1 / 6 |
| 1 / before / unlimited | 571544 / 612037 | 958237 / 773411 | 11034315 / 11384284 | 10 / 7 |
| 1 / after / 21 | 66518 / 47819 | 823101 / 579611 | 18110 / 57234 | 0 / 0 |
| 1 / after / unlimited | 307190 / 316058 | 858952 / 745180 | 27425 / 14276 | 0 / 5 |
| 2 / before / 21 | 446475 / 434769 | 289525 / 274906 | 153 / 155 | 0 / 0 |
| 2 / before / unlimited | 1646926 / 1645772 | 290259 / 275767 | 131 / 0 | 0 / 0 |
| 2 / after / 21 | 468678 / 443076 | 141481 / 108227 | 0 / 70 | 0 / 0 |
| 2 / after / unlimited | 1697604 / 1699383 | 125753 / 114495 | 69 / 14 | 0 / 0 |

All eight points have zero worker admission pressure and queue drops, zero
invalid/duplicate/out-of-window/wrong-run receiver records, and zero outer
fragmentation. These drop counters use different units and cannot be summed
into an exact logical-frame loss attribution.

The release suite, all 18 serial kernel library tests, foundation,
reconnect/allocation, CLI and both load-tool kernel tests pass, as do
Clippy, formatting and Rust 1.89 checks. The first parallel kernel run
failed two process-wide thread-count assertions; the serial run passed.
The tagged coalescing test passed in both runs. No instrumentation remains
in the production binary.

### Keep strict hashing for stacked VLANs

The full-lane hashing comparison improves QinQ paced delivery from
6.193 / 6.030 to 10.491 / 9.914 Gbit/s, with CPU rising from roughly
196% per daemon to 369% / 360%. Unlimited delivery rises from
6.173 / 6.276 to 10.405 / 9.974. Outer socket drops fall from
141481 / 108227 to 8487 / 2941 in the paced pair. This is evidence that
concentrating all transmit work on the TAP reader wasted available capacity.

The runtime therefore retains strict hashing for stacked VLANs. Untagged
and single-tag unfragmented UDP keep reader-local selection when all workers
have TAP queues; the prior TCP and fragment rules remain. The policy reuses
the existing parsed Ethernet header length and adds no table or allocation.
Regression tests cover tag types, both IP versions and every worker/lane
count through 64. A frame and its repairs still retain one original lane.

The final executable is byte-identical to the trial:
`3a695dd915ff8749bed4ed0efaa1847b2ae88fe7607f6cd1c0d5fb8e8b13afb7`.
A fresh four-rate sweep gives:

| Requested Gbit/s each way | Actual offered Gbit/s | Delivered Gbit/s | Missing after drain % | Daemon CPU % |
| --- | ---: | ---: | ---: | ---: |
| 8 | 7.987 / 7.966 | 7.834 / 7.842 | 0.89 / 0.58 | 277.5 / 270.4 |
| 16 | 15.950 / 15.890 | 10.283 / 9.634 | 34.80 / 38.68 | 372.2 / 368.4 |
| 21 | 20.809 / 20.831 | 10.218 / 9.587 | 50.39 / 53.49 | 366.2 / 361.3 |
| unlimited | 48.646 / 47.126 | 9.900 / 10.135 | 79.44 / 78.28 | 359.9 / 359.7 |

At 8 Gbit/s requested, missing frames stay below 1% in each direction.
The unlimited L→R result still declines 3.72% from its 16 Gbit/s point;
R→L improves. The one-reader bottleneck and overload stability remain open.
Full worker and CPU counters are in the same JSON artifact.

| Run | TAP TX drops (SKBs) | Transport socket overflow (SKBs) | Inner UDP errors | Expired frames |
| --- | ---: | ---: | ---: | ---: |
| rx-vlan2-strict/0 | 397184 / 411172 | 8487 / 2941 | 8403 / 334 | 0 / 0 |
| rx-vlan2-strict/1 | 1354398 / 1398194 | 9364 / 2578 | 657 / 0 | 0 / 0 |
| rx-vlan2-final-sweep/0 | 2397 / 0 | 3488 / 719 | 0 / 0 | 0 / 0 |
| rx-vlan2-final-sweep/1 | 213222 / 233085 | 7310 / 2014 | 774 / 3 | 0 / 0 |
| rx-vlan2-final-sweep/2 | 402870 / 426310 | 7018 / 3043 | 452 / 823 | 0 / 0 |
| rx-vlan2-final-sweep/3 | 1490571 / 1414188 | 18138 / 1342 | 273 / 1453 | 0 / 0 |

All six additional points have zero admission pressure, worker queue drops
and expiry, with no invalid, duplicate, out-of-window or wrong-run receiver
records and no outer fragmentation. The final policy passes the release
suite, all 18 serial kernel library tests, Clippy and Rust 1.89 checks.
These gains do not establish the 20 Gbit/s gate or replace active MTU probing.

## Outer packet ceiling and probe encoding, November 6

Socket I/O, DATA packing, configuration validation and worker admission now
support UDP payloads through 8952 bytes, corresponding to a 9000-byte outer
IPv6 packet without extension headers. The default remains 1232. Configured
larger limits are still assertions: active discovery is unfinished. The
production NTT NGN acceptance profile remains capped at 1500 outer bytes.

The padded-probe encoder uses a 56-byte prefix and shared read-only zero padding.
It allocates no packet buffer and copies no padding. Small control records keep
their baseline-sized scratch buffer. Production receive pools and socket-buffer
requests are unchanged. This is preparation for discovery, not a throughput
result or a claim of kernel zero-copy.

Validation: the release/all-features musl suite passes, including every probe
size from 1232 through 8952 bytes, packet-number exhaustion, invalid input
without output mutation, mixed-size GSO/GRO boundaries and reordered packed
frames at 1232, 1452, 4096 and 8952 bytes. All 18 isolated kernel library tests
pass serially. The kernel foundation test captures 1280-, 1500- and 9000-byte
IPv6 packets without Fragment headers. The full runtime test passes with
8952-byte payloads one way and 1452 the other, byte-exact 9014-byte Ethernet
frames, zero forwarding allocations/deallocations, and restart handling.
Rust 1.89 checking and Clippy also pass.

A first socket test sent 64 maximum-size datagrams without draining and lost
15 to the bounded receive queue. Its burst had grown from about 93 KB to
573 KB when the API ceiling increased. The corrected boundary test sends just
enough datagrams to cross the GSO aggregate limit. That fixture correction
does not establish an overload plateau. Larger production queues remain an
option when controlled measurements demonstrate a gain within the memory
budget; the performance gaps above remain open.


## MTU socket errors and GSO retention, November 6

EMSGSIZE previously disabled GSO for the socket before retrying the same
datagrams. An oversized discovery probe would therefore disable offloads for
later traffic. Size errors now return directly and preserve GSO; capability
errors retain the ordinary-send fallback. This is a verified failure mode for
MTU probing, not an attribution for the earlier throughput collapse.

Explicit socket discovery setup now enables nonfragmenting probes above a
learned PMTU and exposes bounded error-queue reads. A release-build kernel test
injects an ICMPv6 Packet Too Big message quoting an actual 9000-byte packet,
verifies the retained protocol header and 1500-byte cached PMTU, then sends
9000-byte probes after enabling discovery. It also verifies local reduction to
1400, recovery to 9000, retained GSO, and packet captures without Fragment
headers. All changes occur in a disposable network namespace.

The release/all-features suite, Rust 1.89 all-target check and Clippy pass.
Workers still need the probe state machine, feedback validation and error-queue
integration. No new throughput measurement or acceptance claim accompanies
these socket changes.


The pure per-lane discovery state machine is now implemented separately from
worker I/O. Release tests cover every supported payload size, silent probe
loss, bounded proof history, full identity/length checks, reductions, baseline
failure and the 600-second upward retry. This adds no runtime throughput
evidence: workers still use the configured payload assertion until discovery
is connected.



## Worker MTU discovery, November 6

Workers now start DATA at the baseline and discover each transmit socket's
limit. The daemon default is an 8952-byte discovery ceiling, replacing the
earlier 1232-byte default and operator assertions. Shared sessions use the
same socket state; lane limits remain independent. No queue or receive buffer
was enlarged.

Release kernel tests verify returned PTB messages, forged quotes, GSO retention
and a local MTU reduction while a frame is queued. A queued remote error can
surface as EMSGSIZE on the next send: the worker checks its origin and proof
before applying a reduction. Unsent ranges keep their frame IDs and offsets
and are repacked with new packet numbers.

A worker test with actual UDP sends and simulated time silently discards
oversized probes. It confirms independent payload limits of 1452 and 4096,
then detects a reduction to 1232 and an increase to 8952 after the upward retry
interval. This validates state and routing; it is not a routed-path throughput
measurement. The isolated socket test still captures nonfragmented 9000-byte
probes above a cached 1500-byte PMTU.

The runtime test now requires confirmed, rather than configured, MTU gauges.
Its first release runs exposed a five-second assertion timeout shorter than
the initial 16-second probe retry. Allowing that retry passed; the protocol
timeout was not shortened. Startup probe loss can delay confirmation, and
ordinary loss can also make the selected MTU conservative. Transport feedback
must eventually replace periodic size confirmation to distinguish that case
more effectively.

No new rate sweep, CPU ceiling or real-access acceptance result accompanies
this change. The previous performance gaps remain open. DATA-quoted PTB
validation, full control pacing and loss recovery still need the Stage 3
sender history and controller.


Validation passes in release mode: the complete all-features musl suite, all
21 isolated kernel library tests, socket/foundation checks, the CLI integration
test and the full runtime test. The latter confirms 9000/1500-byte outer limits
in opposite directions, forwards tagged and jumbo frames, handles restart,
and counts zero allocations/deallocations during forwarding and periodic MTU
confirmation. Rust 1.89 all-target checking and Clippy with warnings denied pass.

## Routed MTU reduction and recovery, November 6

The release `linux_pmtu` integration test passed in 650.45 seconds. Two runtimes
with two transmit lanes each communicated through a router in a third disposable
network namespace. Endpoint link MTUs stayed at 9000; router route limits supplied
the bottleneck. Every lane confirmed 9000 outer bytes, reduced to 1500 with real
router-generated ICMPv6 Packet Too Big, then reduced to 1280 while `tc` filtered
those errors. Filter counters showed drops in both directions and endpoint
captures saw no PTB during that phase. After restoring the route limits, every
lane reconfirmed 9000 through the normal 600-second upward retry.

Each phase delivered byte-exact 60-, 1518- and 9022-byte QinQ frames in both
directions. Outer captures contained no IPv6 Fragment headers and reported no
capture drops. The test uses the real worker runtime and real elapsed time;
it does not inject probe replies or advance a simulated clock.

This verifies routed discovery with delivered and silently dropped errors,
including recovery to a larger MTU. It does not measure goodput, loss under
load or CPU usage. The local and real-access performance gates remain open.

## Cookie placement and transmit FIFO investigation (2026-11-06)

The November 6 performance review proposed a feedback loop between reader-local
lane selection and Linux TAP's learned reverse-flow queue. Linux's
[tun flow updates](https://github.com/torvalds/linux/blob/v6.8/drivers/net/tun.c)
and [symmetric flow hashing](https://github.com/torvalds/linux/blob/master/net/core/flow_dissector.c)
support the mechanism: writing a reverse flow can change its next TAP reader.
With eight readers/lanes, the current receiver mapping shifts each direction
by the cookie's low word modulo eight. A low word ending in `ff` therefore
does not provide a stable reader; `f8` and `f4` provide fixed points for the
round trip. This predicts placement behavior, not throughput.

[Controlled release observations](benches/tap-cookie-placement.json) retain
the per-flow results, per-worker counters, thread CPU, socket overflow, TAP
drops and inner UDP errors. Each point runs for 60 seconds with 16 flows each
way, 44-segment sends, eight workers per daemon, 500-entry TAP queues and an
outer payload ceiling of 1452. The original CPU placement is unchanged.
These are untagged controls. Each point starts fresh processes and namespaces.
Use `SESSION_COOKIE` with `benches/tap_transport.sh` to reproduce the variants.

At `dce9054`, changing only the cookie gives:

| Cookie suffix / request Gbit/s each way | Delivered Gbit/s | Missing after drain % | Reordered / received % | Daemon CPU % |
| --- | ---: | ---: | ---: | ---: |
| ff / 21 | 15.711 / 14.602 | 23.06 / 29.22 | 50.60 / 42.55 | 596.6 / 602.0 |
| ff / unlimited | 15.351 / 13.250 | 40.02 / 50.70 | 54.22 / 50.19 | 594.6 / 598.4 |
| f8 / 21 | 13.585 / 13.845 | 33.26 / 32.64 | 16.39 / 15.59 | 542.1 / 543.8 |
| f8 / unlimited | 16.873 / 14.535 | 41.48 / 51.40 | 18.25 / 17.05 | 626.2 / 634.3 |
| f4 / 21 | 15.136 / 13.142 | 26.86 / 36.62 | 16.02 / 13.90 | 548.8 / 547.6 |
| f4 / unlimited | 15.418 / 14.537 | 34.05 / 41.37 | 22.14 / 22.34 | 629.3 / 631.1 |

Alignment reduces reordering, but does not produce the predicted near-zero
reordering or a paced-throughput improvement. The busiest aligned workers
reach about 98% CPU while others remain below 50%. Original-cookie worker
loads are more even. These observations support a placement effect but do
not establish migration as the sole cause of overload decline. Receiver
processes exceed 90% CPU in these baseline points, further limiting attribution.
No production lane-number permutation is justified by these throughput results
alone.

A separate, reproduced bug explains some within-lane reordering. When a
transmit batch filled during a bounded pending-queue scan, deferred frames
rotated behind the unexamined tail. A release kernel regression sent frame
128 before frames 65–127 on the same lane. Each batch now remembers its
oldest deferred frame; newer frames wait for that frame to be admitted or
discarded. The scan still examines at most 64 entries, and other lanes can
progress. The marker adds eight bytes per batch and no allocation or syscall.
It is cleared on expiry and connection replacement.

The review's encapsulation concern is also present in the code: IPv6 next
headers 4, 41 and 143 stop flow-key parsing before the encapsulated flow.
Its throughput impact has not yet been measured. The proposed sender-retention
layout still needs implementation with recovery. At 5 Gbit/s and 7.5 ms RTT,
one RTT holds about 4.7 MB; retaining every frame for the full 30 ms deadline
would hold about 18.8 MB. The latter is a stress bound, not evidence that
normally acknowledged frames must remain buffered for that whole deadline.
No payload buffers or socket buffers were enlarged for this investigation.

The FIFO fix alone, with the same settings, gives:

| Cookie suffix / request Gbit/s each way | Delivered Gbit/s | Missing after drain % | Reordered / received % | Daemon CPU % |
| --- | ---: | ---: | ---: | ---: |
| ff / 21 | 15.496 / 14.224 | 23.86 / 30.53 | 46.81 / 39.05 | 576.5 / 582.9 |
| ff / unlimited | 15.618 / 14.177 | 32.16 / 39.75 | 47.65 / 41.80 | 602.8 / 607.8 |
| f8 / 21 | 13.506 / 13.529 | 34.12 / 34.16 | 2.29 / 1.89 | 519.7 / 523.8 |
| f8 / unlimited | 15.052 / 14.804 | 41.22 / 46.00 | 2.99 / 3.32 | 584.4 / 588.5 |

This fixes demonstrated queue overtaking and substantially reduces reordering
with aligned placement. It does not establish a throughput improvement:
paced goodput falls slightly in both controls, and unlimited changes differ
by direction and cookie. These are single pairs, not repeatability results.
The original-cookie run still has substantial reordering after the FIFO fix.

Drop attribution for the FIFO build, ordered by left/right daemon or interface:

| Cookie / request | TAP TX drops (SKBs) | Socket overflow (SKBs) | Inner UDP receive errors | Expired frames |
| --- | ---: | ---: | ---: | ---: |
| ff / 21 | 274826 / 109908 | 1216633 / 718455 | 127251 / 91805 | 2 / 1 |
| ff / unlimited | 537374 / 427794 | 1321658 / 772703 | 98206 / 72331 | 9 / 3955 |
| f8 / 21 | 488762 / 331449 | 679112 / 433292 | 62907 / 51252 | 12971 / 6623 |
| f8 / unlimited | 842500 / 973359 | 828527 / 543429 | 103692 / 62891 | 11785 / 26189 |

All ten points have zero admission-pressure and worker-queue drops, zero
receiver invalid/duplicate/out-of-window/wrong-run records, and zero outer
fragmentation. Socket and TAP counters count SKBs, which can contain multiple
datagrams or frames. The JSON retains the baseline drop counters as well.
The 20 Gbit/s gate, repeatable overload plateau and real-path recovery target
remain open.

Validation includes the release FIFO regression, the full release test suite,
Clippy and the minimum supported Rust version. One runtime test timed out on
a single frame after address migration; four diagnostic reruns passed. The
fixture now waits for both peers' generations before sending that frame,
because local installation alone does not establish peer readiness. The
initial timeout's cause was not captured and remains unattributed.

## Completed-frame drain threshold experiment (2026-11-06)

At `58e70fb`, changing only the completed-frame drain threshold from 16 to
32 retains the same 64-datagram, 64-record and 64-KiB receive-turn budgets,
64 reassembly slots, deadlines and pressure-triggered drain/retry. All 22
release kernel tests pass, including bursts, GRO expansion, blocked TAP and
tail-plus-new-head admission.

[Full observations](benches/tap-drain-threshold.json) use the preceding
60-second, 16-flow, eight-worker setup and the original load tool. Each row
starts fresh processes and namespaces. Untagged frames contain 1514 bytes;
single-tag frames contain 1518. Both carry 1472-byte UDP payloads at inner
MTU 1500. The outer payload ceiling remains 1452.

| Traffic / cookie / threshold / request Gbit/s each way | Delivered Gbit/s | Missing after drain % | Daemon CPU % |
| --- | ---: | ---: | ---: |
| Untagged / ff / 32 / 21 | 12.452 / 12.117 | 39.42 / 41.26 | 456.3 / 460.5 |
| Untagged / ff / 32 / unlimited | 18.171 / 15.005 | 45.32 / 55.98 | 642.7 / 661.6 |
| Untagged / f8 / 32 / 21 | 15.892 / 14.705 | 23.53 / 29.26 | 526.0 / 522.3 |
| Untagged / f8 / 32 / unlimited | 18.318 / 17.075 | 45.49 / 50.66 | 639.7 / 642.8 |
| Untagged repeat / ff / 16 / 21 | 16.810 / 15.542 | 18.41 / 24.62 | 569.7 / 583.7 |
| Untagged repeat / ff / 32 / 21 | 17.170 / 15.842 | 16.43 / 23.09 | 557.3 / 573.5 |
| Untagged repeat / f8 / 16 / 21 | 14.525 / 14.151 | 29.41 / 31.07 | 524.1 / 520.1 |
| Untagged repeat / f8 / 32 / 21 | 15.600 / 14.668 | 24.77 / 29.31 | 528.6 / 529.1 |
| Single tag / ff / 16 / 21 | 16.506 / 14.962 | 20.23 / 27.73 | 612.4 / 611.7 |
| Single tag / ff / 16 / unlimited | 15.851 / 12.372 | 54.08 / 66.82 | 607.7 / 603.7 |
| Single tag / ff / 32 / 21 | 16.449 / 15.551 | 20.12 / 24.54 | 613.9 / 618.4 |
| Single tag / ff / 32 / unlimited | 15.252 / 14.913 | 53.27 / 53.03 | 637.4 / 631.1 |

The larger threshold raises mean USO groups from roughly 10–11 to 14–17
frames. Aligned untagged placement improves in both paced comparisons.
However, the first original-cookie paced run concentrates work on four queues
per daemon: those workers reach 81–97% CPU while the others stay near 21–24%.
Its cause was not isolated. The repeat spreads work more evenly and performs
much better, demonstrating substantial run-to-run variation.

Tagged results also fail to establish an improvement in both directions.
At unlimited load, threshold 32 delivers less left-to-right traffic than
threshold 16, and both still fall below their paced goodput. **The experiment
is rejected; production retains threshold 16.** Larger write groups alone
have not solved worker imbalance or overload decline.

Tagged drop attribution, left/right:

| Threshold / request | TAP TX drops (SKBs) | Socket overflow (SKBs) | Inner UDP receive errors | Expired frames |
| --- | ---: | ---: | ---: | ---: |
| 16 / 21 | 256017 / 155314 | 2070095 / 1256899 | 52417 / 70299 | 5 / 2659 |
| 16 / unlimited | 1772759 / 2276780 | 1962955 / 1411254 | 84600 / 165296 | 3736 / 37537 |
| 32 / 21 | 208158 / 87520 | 2325207 / 1413031 | 55233 / 46887 | 4 / 1341 |
| 32 / unlimited | 1527179 / 1249358 | 1812179 / 1554049 | 79624 / 101215 | 18155 / 13032 |

All twelve new observations have zero admission-pressure and worker-queue
drops, invalid/duplicate/out-of-window/wrong-run receiver records, and outer
fragmentation. Full drop counters, reordering and per-worker CPU are in the
JSON. Most receiver processes remain above 89% CPU, so the next harness
check batches their UDP receives while keeping the daemon unchanged.

## Load receiver syscall reduction (2026-11-06)

The load tool now reuses the library's checked `recvmmsg` implementation and
services only sockets reported ready by `poll`. Each socket still receives
at most 16 kernel messages per turn. Payload, sequence, duplicate, GRO and
overflow validation remain unchanged; about 1 MiB of payload storage is
allocated before timing. The duplicate ancillary parser is removed.

[Release observations](benches/udp-load-receive-batch.json) use the unchanged
`58e70fb` daemon, threshold 16, and the single-tag settings above. These
measure the harness, not a daemon optimization.

| Receiver / request Gbit/s each way | Delivered Gbit/s | Missing after drain % | Daemon CPU % | Receiver CPU % |
| --- | ---: | ---: | ---: | ---: |
| Original / 21 | 16.506 / 14.962 | 20.23 / 27.73 | 612.4 / 611.7 | 93.3 / 92.6 |
| Batch only / 21 | 17.208 / 15.395 | 17.24 / 25.95 | 625.1 / 619.8 | 95.9 / 95.6 |
| Batch only / unlimited | 16.813 / 15.305 | 51.30 / 50.50 | 669.4 / 679.5 | 95.9 / 95.7 |
| Ready sockets / 21 | 16.047 / 14.761 | 21.79 / 28.24 | 615.4 / 611.2 | 85.4 / 81.0 |
| Ready sockets / unlimited | 16.085 / 14.745 | 44.55 / 48.89 | 689.9 / 681.9 | 86.2 / 83.5 |
| Final, including poll counter / 21 | 15.985 / 14.679 | 22.36 / 29.16 | 622.1 / 634.6 | 87.9 / 83.2 |

The original paced receivers made 23.46 / 25.04 million receive calls. The
final receivers make 5.63 / 5.53 million receive calls plus 2.52 / 2.86
million readiness polls. The combined count is about two thirds lower.
Batching alone reduced receive calls but kept probing empty sockets and
did not reduce CPU use.

The final paced row has left/right TAP drops 242842 / 128832 SKBs, socket
overflow 2702282 / 1423571 SKBs, inner UDP errors 39892 / 40106, and expired
frames 7 / 1321. All five new observations have zero admission-pressure and
worker-queue drops, invalid/duplicate/out-of-window/wrong-run receiver records,
and outer fragmentation; the JSON retains every row's drop attribution.

The syscall reduction is verified. A transport throughput improvement is not:
goodput remains variable and below target, and receiver CPU still exceeds
80% at these rates. More receiver capacity may be needed for the final gate.
Release tests cover multiple valid messages around an invalid sibling in one
receive call, IPv4/IPv6, GRO, truncation and allocation-free processing.
Clippy and Rust 1.89 checks pass.

## Encapsulated-flow hashing and tagged SRv6 (2026-11-06)

The flow key now looks through one IPv4, IPv6 or Ethernet encapsulation,
retaining the outer identity. It skips supported IPv6 extension headers,
stops at fragments and one inner level, and falls back to the outer key for
truncated inner headers. Encapsulated traffic retains strict pinning. No
allocation, queue growth, deadline change or wire change is involved.

[Seventeen release observations](benches/tap-srv6-inner-flow.json) compare the
FIFO-fixed baseline with this change. Both use the revised load receiver,
eight workers, 16 flows per direction unless noted, 60 seconds, a single VLAN,
500-entry TAP queues and a 1452-byte outer UDP ceiling. Every point starts
fresh processes and namespaces; builds and tests do not overlap load.

The new harness mode uses real kernel IPv6-in-IPv6 SRv6 encapsulation with a
one-segment routing header and End.DT6 decapsulation. The tagged frames are
1518 bytes and carry 1388 bytes of UDP payload. SRv6 traffic reaches TAP as
individual frames in these runs, even with 44-segment generator sends.
A low-rate smoke test delivered every frame both ways and reported zero
decapsulation errors. The JSON includes endpoint IPv6 and SRv6 route counters.

| Key / requested Gbit/s each way | Actual offered Gbit/s | Delivered Gbit/s | Missing after drain % | Daemon CPU % |
| --- | ---: | ---: | ---: | ---: |
| Outer only / 1 | 1.000 / 1.000 | 0.820 / 0.690 | 17.676 / 30.751 | 60.0 / 65.5 |
| Inner flow / 1 | 1.000 / 1.000 | 0.997 / 0.997 | 0.0008 / 0 | 96.1 / 89.7 |
| Outer only / 5 | 4.993 / 4.993 | 1.265 / 1.199 | 74.569 / 75.899 | 115.0 / 122.4 |
| Inner flow / 5 | 4.979 / 4.974 | 4.634 / 4.864 | 6.454 / 1.704 | 429.4 / 393.9 |
| Outer only / 21 | 8.027 / 7.896 | 1.271 / 1.263 | 84.093 / 83.937 | 116.3 / 127.6 |
| Inner flow / 21 | 7.184 / 7.149 | 5.622 / 5.882 | 21.473 / 17.437 | 539.9 / 489.5 |
| Outer only / unlimited | 7.893 / 7.607 | 1.229 / 1.181 | 84.369 / 84.415 | 118.5 / 124.4 |
| Inner flow / unlimited | 7.122 / 7.093 | 5.558 / 5.816 | 21.689 / 17.733 | 529.7 / 486.3 |

The baseline puts each direction on one transmit lane and one receive worker.
At higher rates those receive workers reach 91–99% CPU. The new key spreads
the inner flows and raises the unlimited result by about 4.5–4.9 times.
This verifies the report's encapsulation bottleneck. It does not establish
the 20 Gbit/s gate or the lossy real-path target.

The generators saturate near 8 Gbit/s with the baseline and 7 Gbit/s with the
busier candidate. Thus the 21 Gbit/s request is not a 21 Gbit/s offered load.
Candidate receivers reach about 90% CPU, and its busiest workers reach
94–97%. Receiver CPU includes its two-second drain; daemon CPU covers the
load interval. The unlimited SRv6 result is about 1.1% below its requested-21 result,
with a slightly lower offered rate. Repeated plateau validation remains open.

Drop attribution at the 5 Gbit/s request, left/right:

| Key | TAP TX drops (SKBs) | Socket overflow (SKBs) | Inner UDPv6 receive errors | Expired frames |
| --- | ---: | ---: | ---: | ---: |
| Outer only | 13635728 / 13494796 | 372366 / 340476 | 0 / 0 | 1190 / 2380 |
| Inner flow | 190128 / 255995 | 21644 / 215375 | 8942 / 23935 | 0 / 0 |

At 1 Gbit/s, the baseline's large bursts already overflow TAP and socket
queues despite low average CPU. With one-segment sends, baseline delivery
rises to 0.990 / 0.982 Gbit/s with 0.412 / 1.320% missing. Its sustained
ceiling still stays near 1.3 Gbit/s. The inner-flow one-segment unlimited
control delivers 1.889 / 1.948 Gbit/s, with 0.398 / 0.385% missing and
560.3 / 505.4% daemon CPU. Its generators offer only 1.903 / 1.962 Gbit/s;
smaller sends cost much more CPU. These controls separate burst pressure
from the verified single-lane ceiling.

A single-flow SRv6 unlimited control with the new key delivers only
1.208 / 1.169 Gbit/s at 113.5 / 127.1% daemon CPU. Hashing cannot divide one
flow. Its 84.98 / 84.63% missing and per-frame cost remain deployment gaps.

Ordinary single-tag UDP retains its existing key and reader-local lane
policy. Its new paced control delivers 16.394 / 14.456 Gbit/s, with
20.67 / 30.15% missing at 622.3 / 620.4% CPU. Unlimited load falls to
12.421 / 12.975 Gbit/s, with 67.09 / 65.64% missing at 530.6 / 534.5% CPU.
The latter concentrates load on alternating workers: roughly 78–92% CPU on
the busy halves and 40–51% on the others. This repeats the earlier imbalance
pattern; its exact cause and the remaining overload decline are unresolved.

All seventeen observations have zero admission-pressure and worker-queue
drops, invalid/duplicate/out-of-window/wrong-run receiver records, and outer
fragmentation. The JSON retains every run's drop counters, per-worker CPU
and reordering. The parser tests cover both outer IP versions, inner IP and
tagged Ethernet, SRH, fragments, depth limits, truncation and non-IP fallback.

Default and release suites, Clippy and Rust 1.89 checks pass. All 22 library
kernel tests, the socket/TAP foundation test and both runtime tests pass,
including allocation checks, reconnect, foreign rename and namespace move.

## Whole-super-frame handoff (2026-11-06)

The TAP reader now hands each retained super-frame to its selected transmit
owner once. Expansion happens there, with the same 64-frame/64-KiB turn
budget, original deadline and lane. This removes per-child handoffs and
moves segmentation work off a busy reader. It still copies each child's
payload. Input queues remain bounded to 128 entries; one reservation budget
covers both readers and transmit owners.

[Fourteen release observations](benches/tap-super-frame-handoff.json) compare
`7895a64` with this change, using the previous section's load tool, CPU
placement, outer ceiling and 60-second isolation. QinQ uses 1522-byte frames
and strict hashing; single-tag controls use 1518-byte frames and the existing
reader-local policy. Both carry 1472-byte UDP payloads.

| Variant / requested Gbit/s each way | Actual offered Gbit/s | Delivered Gbit/s | Missing after drain % | Daemon CPU % |
| --- | ---: | ---: | ---: | ---: |
| QinQ baseline / 8 | 7.978 / 7.984 | 7.890 / 7.865 | 0.260 / 0.646 | 291.2 / 278.9 |
| QinQ handoff / 8 | 7.980 / 7.972 | 7.934 / 7.866 | 0.239 / 0.989 | 261.9 / 248.5 |
| QinQ baseline / 16 | 15.891 / 15.901 | 9.686 / 9.967 | 38.731 / 36.994 | 363.1 / 358.3 |
| QinQ handoff / 16 | 15.843 / 15.882 | 13.070 / 13.763 | 16.768 / 12.594 | 458.4 / 462.9 |
| QinQ baseline / 21 | 20.914 / 20.907 | 8.368 / 10.202 | 59.835 / 51.020 | 350.2 / 337.8 |
| QinQ handoff / 21 | 20.812 / 20.872 | 13.001 / 14.331 | 37.198 / 30.988 | 469.5 / 496.9 |
| QinQ baseline / unlimited | 51.649 / 49.751 | 9.322 / 10.087 | 81.881 / 79.643 | 364.9 / 346.0 |
| QinQ handoff / unlimited | 46.432 / 44.295 | 12.999 / 14.146 | 71.840 / 67.880 | 464.3 / 488.7 |
| Single-tag baseline / 21 | 20.748 / 20.768 | 16.394 / 14.456 | 20.669 / 30.145 | 622.3 / 620.4 |
| Single-tag handoff / 21 | 20.740 / 20.802 | 15.873 / 14.920 | 23.145 / 27.994 | 624.1 / 628.4 |
| Single-tag baseline / unlimited | 37.905 / 37.923 | 12.421 / 12.975 | 67.089 / 65.642 | 530.6 / 534.5 |
| Single-tag handoff / unlimited | 30.416 / 31.475 | 16.049 / 14.867 | 47.028 / 52.578 | 671.2 / 661.1 |

QinQ repeats deliver 13.218 / 13.396 Gbit/s at the 16 Gbit/s request and
13.546 / 14.000 unlimited, with 459.6 / 458.9% and 471.7 / 488.1% daemon CPU.
Moving expansion verifies a throughput gain for this workload. At 8 Gbit/s,
similar delivery uses about 10% less daemon CPU. At higher rates, more workers
do useful work and total CPU rises. The single-tag unlimited result no longer
collapses relative to its paced point in this sweep. Neither this sweep nor
the two QinQ repeats establish the final five-run gate.

Drop attribution at unlimited load, left/right:

| Variant | TAP TX drops (SKBs) | Socket overflow (SKBs) | Inner UDP receive errors | Expired frames |
| --- | ---: | ---: | ---: | ---: |
| QinQ baseline | 4891638 / 4569473 | 36402 / 9969 | 1155 / 1324 | 0 / 0 |
| QinQ handoff | 3835642 / 3162056 | 783383 / 33313 | 6385 / 17616 | 0 / 0 |
| Single-tag baseline | 2551184 / 2349650 | 2023171 / 966511 | 35759 / 47666 | 14059 / 1348 |
| Single-tag handoff | 1168292 / 1253968 | 1859603 / 1618021 | 65329 / 97988 | 5702 / 14222 |

The additional QinQ unlimited repeat expires 97160 frames on the right.
Pressure has shifted from TAP ingress toward socket delivery; loss remains
far above target. Generator CPU reaches 94–98% in unlimited runs. Receiver
CPU reaches 76–81% for QinQ and 84–88% for single-tag traffic.

QinQ reordering rises from under 0.02% to roughly 9–10% in one direction
under load; the other stays near 0.2%. The repeat confirms this increase.
The targeted handoff test preserves source and frame-ID order, but the
end-to-end cause remains unverified. Single-tag reordering was already high
and remains about 48–52% under unlimited load. These are unresolved gaps,
especially for TCP and QUIC; throughput alone does not close them.

All fourteen observations have zero admission-pressure and worker-queue
drops, invalid/duplicate/out-of-window/wrong-run receiver records, and outer
fragmentation. The JSON includes per-flow counts, per-worker CPU and every
drop counter.

Release/default suites, Clippy, Rust 1.89 and formatting pass. All 23 library
kernel tests and both runtime tests pass. The new handoff regression covers
44 normal segments, 10000 one-byte segments, a following ordinary frame,
bounded expansion, full input/return queues, quiesce and reservation return.
