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
- The library never touches ordinary files, environment variables or stdio.
  Kernel interfaces (sockets, ioctls, `/dev/net/tun`, `timerfd`, `eventfd`,
  `epoll`, `getrandom`) are fine.
- No async runtime, no wall-clock values on the wire, no host-specific facts in
  the repository.
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
| `src/reassembly.rs` | Receive windows, frame assembly, TAP write queue | 2 |
| `src/pack.rs` | Outgoing record packing and segmentation | 2 |
| `src/ack.rs` | ACK generation and processing | 3 |
| `src/loss.rs` | RTT, loss detection, PTO (RFC 9002 parameters) | 3 |
| `src/pmtu.rs` | DPLPMTUD per lane | 3 |
| `src/bbr.rs` | BBRv3 (draft 07) | 3 |
| `src/pacing.rs` | Grants, pacing deadlines | 3 |

## Stage 1: socket and device foundation (in progress)

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
liveness transitions and 16 protocol regression tests in `tests/handshake.rs`.
The four handshake model tests now exercise real types. The five frame-window
model tests remain in `tests/frame_model.rs` until stage 2 replaces them.
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
  these real types. Keep its frame-window cases in `tests/frame_model.rs` for
  stage 2, then delete the old mixed model file. Add the message-loss, duplicate,
  reorder, simultaneous start and restart cases listed under "Protocol and
  ownership regression cases" in section 14.1.

Done when every handshake model case passes against real types, the old mixed
model file is gone, and the frame tests remain available for stage 2.

### 1c Configuration types (`src/config.rs`)

Plain structs the daemon fills from TOML and a library user fills by hand:
`Daemon` (memory ceiling), named `Endpoint` (listen address, listen port,
optional bind device), `Peer` (name, endpoint name, IPv6 host or FQDN,
peer port, lane count, path group), `Session` (name, peer, cookie,
TAP name defaulting to session name, inner MTU, optional MAC, queue count).
Roles are elected and have no configuration field. Follow
[config-model.md](config-model.md). `validate()` returns the first error:
duplicate names or cookies via `CookieIndex`, unknown endpoint or peer names,
MTU out of range, port zero.
Hosts given as FQDN are allowed only with the `dns` feature; without it the
validator rejects them. No file or environment access here.

### 1d Counters (`src/counters.rs`)

One struct of `AtomicU64` per session, per peer and per lane, with the names
section 12 lists. Workers increment with `Relaxed`; the control handle reads
with `Acquire`. No locks, no strings in the hot path.

### 1e Worker runtime (`src/spsc.rs`, `src/worker.rs`, `src/runtime.rs`)

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
- Data path in this stage: a TAP frame becomes one or two DATA records in
  separate datagrams of at most 1232 bytes of UDP payload, sent on the lane
  chosen by the flow key of section 8.3. The receiver assembles by frame ID
  into a per-lane table bounded to 64 incomplete frames and writes complete
  frames to its TAP queue. No ACKs, no repair, no windows yet; that is
  stage 2 and 3. Duplicate suppression by frame ID is already required so
  a frame never reaches the TAP twice.
- Container test `tests/linux_runtime.rs`: two runtimes in one network
  namespace on loopback with different fixed ports and automatically elected
  roles, each with one TAP. Assert the handshake completes, carrier rises
  on both TAPs, a frame written to one TAP arrives unchanged on the other in
  both directions for 60, 1514, 1518 and 9022 bytes, carrier drops within
  30 s after one runtime shuts down, and the surviving runtime reconnects
  after a restart with fresh tags.

Done when the container test passes and no worker ever blocks on another
worker's queue.

### 1f Daemon (`src/bin/unquic.rs`, feature `bin`)

- Add the `dns`, `uuid-compat` and `bin` features to `Cargo.toml` as the
  README lists, with `bin` enabling `dns`, and `[[bin]]` gated on `bin`.
- Subcommands: `gen-uuid` prints one UUIDv4 from `UnquicUuid::new_v4` and
  exits; `run <config.toml>` parses TOML with `serde` and `toml` into
  `config::*`, validates, and calls `runtime::run` in the foreground.
- Resolution: with `iphost`, resolve FQDN peers before `run` and refresh on a
  helper thread, feeding results through `Control`. The resolver mode is a
  config field; never fall back to the system resolver when a bypassing mode
  was selected.
- Readiness and double-fork from section 12.1 come in stage 4, not here.
  Foreground operation is enough to test.
- Test: a unit test that parses the example configuration from section 12
  and a rejection test for each `validate()` rule.

## Stage 2: batched transport

Goal: the packet rate and CPU baseline of section 13 can be measured.

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
  acceptance; a datagram that cannot be fully reserved is dropped whole.
- `tap.rs`: optional `IFF_VNET_HDR` stays off in v1; add `read_batch` using
  one `read` per frame in a loop until `EAGAIN`, with the buffer pool owned
  by the worker.
- Preallocated buffer pools and descriptor rings in the worker; no allocation
  on the data path after startup. Verify by a test that counts allocations
  with a global allocator wrapper in the test binary.
- Tests: window tests ported from `tests/frame_model.rs` to real types;
  delete that model file once the replacement tests pass, GRO split boundaries, partial
  `sendmmsg` completion returns the unsent entries, container test extended
  with a 10 s bidirectional frame storm and a check that no frame is
  duplicated or corrupted.
- Measure: record frames per second and CPU per core on the container
  loopback in a `BENCH.md` table with kernel version and CPU model. These
  numbers are a baseline, not an acceptance claim.

## Stage 3: recovery and congestion control

Goal: a production candidate under loss.

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
- `pmtu.rs`: RFC 8899 per lane from 1232 upward to 1452 using padded PROBE
  and PROBE_REPLY, `EMSGSIZE` and `IPV6_RECVERR` Packet Too Big as "shrink
  now, re-probe later". Keep `IPV6_PMTUDISC_DO`.
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
  loss sweep (0, 0.1, 1, 5 percent) recorded in `BENCH.md`.

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
