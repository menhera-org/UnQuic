# UnQuic implementation roadmap

This file tells a coding agent what to build next, in what order, and how to
prove each step works. [initial-design.md](initial-design.md) is the
specification; this file never overrides it. Where this file names a section,
read that section before writing code. Keep the "Current status" paragraph in
section 15 of the design and the "Implementation status" section of
[README.md](README.md) true after every step.

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
numbers. No performance gate has passed.

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
pacing clock per group, send records kept in each lane's history, ACKs applied
by the lane owner with batched reports to the controller, and ACKs sent from
the receive owner under a per-worker control ceiling. Implement it in this
order, one commit each: extend the chronological oracle and draft-07 vectors
to concurrent workers; publish controller state and charge inflight and
pacing atomically; move send records into lane histories and retire the
ordered sampler; send ACKs from receive owners. Measure each on the decision
workload. No performance gate has passed.

## November 8 checkpoint

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
| `src/pacing.rs` | Grants, pacing deadlines | 3 |

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
General application-limited detection remains unfinished;
congestion-control integration is not complete.
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
Pending probe commands survive generation changes without resetting backoff.
Once a group stalls, PTO targets rotate across installed sessions, then their
lanes, so an unanswered lane cannot exclude a working peer. Selection uses the
existing session links in constant time. The two-probe allowance and backoff
remain shared; a replacement handshake does not reset them.
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
- `pacing.rs`: grants with 100 µs expiry and reservation-before-grant,
  burst cap of min(send quantum, 64 datagrams), `timerfd` deadlines, the
  separate 4 MiB/s control-traffic ceiling with a 16-datagram burst.
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
  This remains unfinished: current nonpersistent TAPs disappear on last close.
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

  ```sh
  cargo clippy --release --target x86_64-unknown-linux-musl --lib --all-features -- \
    -D clippy::unwrap_used -D clippy::expect_used -D clippy::panic
  ```

- Container tests for Linux bridge, OVS system port where OVS is available,
  VLAN and QinQ uppers, macvlan, multiple MACs, multicast, broadcast and LLC
  frames, as section 14.1 V04 lists. Each test creates and deletes its own
  devices.
- VRF binding test with a VRF device created in the namespace.
- Resource stress: fill reassembly, TAP-write and handoff queues; sustained
  receiver overload; many sessions and lanes against the memory ceiling.
- Restart, path change and DNS change cases from V03.
- Daemon: double-fork readiness exactly as section 12.1 describes, PID file,
  parent exit only after every TAP exists; a test that the parent waits while
  a TAP is missing and succeeds with no peer reachable.
- Optional systemd unit template embedded in the daemon, with the exact
  `[Unit]`, `[Install]` and `[Service]` lines of section 12.1. No systemd
  linkage.

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
