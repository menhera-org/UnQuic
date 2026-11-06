# UnQuic implementation roadmap

This file tells a coding agent what to build next, in what order, and how to
prove each step works. [initial-design.md](initial-design.md) is the
specification; this file never overrides it. Where this file names a section,
read that section before writing code. Keep the "Current status" paragraph in
section 15 of the design and the "Implementation status" section of
[README.md](README.md) true after every step.

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
partial sends. Tests cover generated sequences, reverse/duplicate delivery,
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
Unfragmented UDP now uses the TAP reader's owned lanes when a session has a
queue on every worker; measured handoff savings justify relaxing pinning there.
TCP, fragments, other protocols and sessions with fewer queues retain strict
hashing. TAP queue migration may reorder UDP frames; a frame and its repairs
keep one lane. The expanded benchmark history measures this reordering without
changing transport storage or admission. See BENCH.md for UDP/TCP comparisons.
The final throughput gate remains open. Partial completion, GSO/GRO boundaries, MTU errors
and allocation checks pass. The five frame-model regression cases
now run against the real implementation in tests/reassembly.rs.

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
  in place. UDP super-frame input uses bounded userspace segmentation from
  the existing pool; TCP read offloads remain disabled. Compatible complete
  UDP frames can share a lossless USO TAP write, preserving per-flow order,
  backpressure and the existing receive deadline. Other frames use single writes.
  Workers use bounded nonblocking io_uring read batches when available,
  draining completions before releasing buffers and falling back to plain reads
  on unsupported kernels. UDP super-frame sweeps improve delivery but still
  miss the 20 Gbit/s gate and show overload declines. TCP super-frame input
  remains unfinished.
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
  smoke tests pass with plain UDP. B3 UDP super-frame input is implemented;
  TCP input remains segmented by the kernel. Direct harness probes do not
  pass the tunnel gate; see BENCH.md for measured transport limits.
- Measure: record frames per second and CPU per core on the container
  loopback in a `BENCH.md` table with kernel version and CPU model. These
  numbers are a baseline until a complete-transport run demonstrates at least
  20 Gbit/s of delivered inner goodput in each direction simultaneously at
  inner MTU 1500. Also measure sustained small-frame bursts (60, 64 and 128 frame
  bytes excluding FCS): delivered frames/s, drops and CPU per direction, with
  single- and multiple-flow cases. Report each direction separately, with loss, CPU including
  softirq, core budget, flow count and the precise measurement layer. Keep
  this requirement open until a repeatable run passes.

## Stage 3: recovery and congestion control

Goal: a production candidate under loss.

Session liveness already uses matching small probes on local lane 1.
Receive-side ACK scheduling and range construction now use the existing
reassembly bitmap, with no second history or hot-path allocations. Tests cover
thresholds, gaps, duplicate rate limits, pressure rejection, word/window wrap,
maximum packet numbers, and reset/retirement. Sender-history validation and
worker feedback routing remain pending, so the runtime still emits no packet
ACKs. Per-lane MTU probing, recovery and congestion control are also pending;
README.md documents the baseline send-error behavior.

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
- `pmtu.rs`: RFC 8899 per lane from the 1232-byte UDP baseline, using larger
  confirmed path MTUs including above 1500 outer bytes. Expand the current
  1452-byte socket API and receive-validation ceilings together, within wire
  and reserved-buffer bounds; keep the hot path allocation-free. Use padded
  PROBE and matching PROBE_REPLY, with `EMSGSIZE` and validated `IPV6_RECVERR`
  Packet Too Big as "shrink now, re-probe later". Local interface MTU alone
  never confirms a path. Reset discovery for new paths and keep TAP MTU
  independent. Keep `IPV6_PMTUDISC_DO`. Test 1280-, 1500- and 9000-byte paths,
  independent lane limits, lost probes, forged/stale replies, MTU reduction
  and recovery in disposable network namespaces; never emit outer fragments.
- `bbr.rs`: BBRv3 as draft-ietf-ccwg-bbr-07 specifies, one owner per sending
  direction and path group, charging outer IP bytes, initial window ten
  datagrams. Pin deterministic vectors derived from the draft's pseudocode
  before optimising anything. Do not substitute a simpler controller.
- `pacing.rs`: grants with 100 µs expiry and reservation-before-grant,
  burst cap of min(send quantum, 16 datagrams), `timerfd` deadlines, the
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
