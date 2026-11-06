# UnQuic Encapsulation

Ethernet transport over IPv6 UDP for Linux. It is intended for general use;
NTT NGN is one deployment and performance validation environment.

- Naming: `UnQuic`, because the transport borrows ideas from QUIC without being QUIC.
- 'Encapsulation' instead of 'VPN': people blindly expect a VPN to encrypt, and 'VPN' is a commercially contaminated marketing word.
- Development language: pure Rust.
- Dependencies: none except `libc`. Everything else (UUID types, congestion control) is written in-tree. The UUID type is named `UnquicUuid` to avoid collisions with external UUID types. Write C-like Rust, optimized for performance.
- Dependencies exceptions: feature-gate all the other deps: `dns` pulls `iphost v0.4.4` - otherwise the endpoints are configured manually. `uuid-compat` pulls in `uuid` crate, and the conversions from/into the in-tree UUID types. `bin` pulls in `serde` and `toml`, and enables `dns`. Obtain random bytes through Linux's `getrandom(2)` via `libc::syscall(SYS_getrandom, ...)`, without an additional crate. The `unquic gen-uuid` subcommand generates session cookies; the daemon never generates a session cookie at runtime.
- Protocol baseline: automatic handshake role election, random connection receive tags from `libc::syscall(SYS_getrandom, ...)`, bounded selective repair, and one in-tree BBRv3 controller per direction and path group. No wall-clock epochs or persistent restart counters.
- Crate shape: library only. The daemon binary exists only when the `bin` feature is enabled.
- Use `libc` for syscalls and socket options where appropriate; use `const fn` wherever a function can be one.
- Receive admission: bound receive sockets accept packets by cookie, wire validity and protocol state, regardless of source address or port. Peer addresses select transmit destinations only.
- Performance contract: no locks or allocations in hot paths. State transitions make no syscalls; workers batch I/O, notifications and timer updates. Performance claims require measurements.
- Performance targets: local container tests must sustain at least 20 Gbit/s in each direction simultaneously at inner MTU 1500, with CPU use reported. The deployment target is at least 5 Gbit/s over real routed IPv6 10G-EPON access lines with 4.5–7.5 ms RTT and nonzero packet loss. Neither target has been demonstrated; section 14.2 of the design defines the acceptance work.
- No async runtime: plain pinned threads with batched syscalls. `tokio` gives one syscall per packet, no GSO/GRO, cross-core task migration, and 1 ms timers, all wrong for a few always-busy sockets. See section 10 of the design.
- Platform contract: assume the Linux kernel and only the interfaces, files, and permissions needed by the selected features, such as `/dev/net/tun` for TAP creation and `/etc/resolv.conf` when using `iphost`'s system resolver. Its configurable `ipv6`, `ra`, and `dhcpv6` modes bypass system resolvers. Document each additional host assumption with the feature that needs it. Make no assumptions about the surrounding userspace: no required XDG or other freedesktop.org conventions, GNU userspace, Android or ChromeOS facilities, systemd, or D-Bus dependencies. Binaries are built for `*-unknown-linux-musl` targets.
- Library I/O: the transport core reads no ordinary files or environment variables and writes no diagnostics. Kernel interfaces such as `/dev/net/tun`, sockets, and ioctls are allowed. The optional iphost layer uses the selected resolver's operating-system facilities; see [resolver requirements](config-model.md#daemon-resolver-selection). Application configuration-file handling and command-line I/O belong to the caller or daemon.
- Future optional integration: systemd support without linking to systemd or D-Bus libraries, DEB/RPM packaging, and restarts that preserve TAPs by retaining and handing over their file descriptors, following rs-l2tpd's descriptor-preservation approach. These additions must leave standalone operation and the library I/O boundary intact.
- Planned daemon readiness: every configured TAP interface exists, whether newly created or adopted. The daemon forks and its original parent exits successfully only at readiness, so any init system or launcher can wait for that exit status. This works without systemd and does not wait for network connectivity or peer handshakes. When used, the optional embedded systemd unit must make both `network.target` and `network-online.target` require and wait for daemon readiness; the daemon must not depend on either target or any wait-online service.
- TAP configuration: creation accepts `Option<MacAddress>`. The in-tree `MacAddress` type accepts nonzero unicast addresses, whether globally or locally administered. `None` derives a stable address in Menhera's `60:a4:34:4x:xx:xx` MAC-M range from the current UTS hostname and interface name using SHA-256. UnQuic sets the configured MTU and optional MAC but leaves administrative state, addressing and bridge membership to the caller or network manager (for example, systemd-networkd or ifupdown). Transport state controls carrier, independently of administrative state.
- Design: see [initial-design.md](initial-design.md). Implementation order and per-step acceptance: see [ROADMAP.md](ROADMAP.md).

## Build and feature availability

UnQuic targets Linux across distributions and init systems. Ubuntu 26.04 with
Linux 7.0 is a deployment validation target, not a minimum version or exclusive
platform. Detect optional kernel facilities at runtime and retain documented
fallbacks. Source references to Linux 6.8 describe that implementation; they
do not define behavior for every kernel. Benchmark and compatibility results
must identify the kernel actually tested.

The crate requires Linux and declares Rust 1.89 or later. Use a
`*-unknown-linux-musl` target for builds. The default build depends only on
`libc`; enabling `dns` adds exactly `iphost 0.4.4` with RA and DHCPv6 support,
without its default features. `uuid-compat` adds lossless conversions to and from
`uuid::Uuid`, with no UUID generation dependency. `bin` builds the daemon and
enables `dns`, `serde`, `toml` and iphost's serde support. No optional feature is
enabled by default.

Build and run the foreground daemon:

```sh
cargo build --release --target x86_64-unknown-linux-musl --features bin
./target/x86_64-unknown-linux-musl/release/unquic gen-uuid
./target/x86_64-unknown-linux-musl/release/unquic run config.toml
```

Provision the printed cookie explicitly on both ends; startup never invents or
changes a session cookie. The [configuration model](config-model.md) documents
the TOML fields and resolver modes. Unknown fields fail, and parse errors report
a byte position without echoing configuration text or cookies.

The daemon stays in the foreground. It reports `TAPs ready` on stderr once all
configured TAPs exist; SIGINT or SIGTERM requests shutdown and joins transport
workers. Administrative interface setup remains external. Double-fork readiness,
PID files, service integration and configuration reload are still pending.

## Implementation status

The first foundation provides `UnquicUuid` parsing and UUIDv4 provisioning,
duplicate-checked cookie lookup, checked `MacAddress` values, IPv6 receive and
transmit sockets, and nonpersistent multiqueue TAP ownership. Frame I/O and
cookie lookup allocate no memory. The default build depends only on `libc`;
`dns` enables exactly `iphost v0.4.4`, without its default Tokio, tracing or serde
features. Its direct, RA and DHCPv6 resolver modes are available.

Required device lifetime is stronger than the current implementation: published
TAPs must survive daemon operation and descriptor-preserving restarts, and may
be removed only on explicit instruction. UDP sockets may be recreated. External
TAP renames must be respected without renaming back, recreating the old name or
changing session identity. Production code must not panic. The current runtime
still closes nonpersistent TAPs on shutdown, has no descriptor adoption/re-exec
handover, and has not completed the panic-path audit.
These are tracked in Stage 4 of [ROADMAP.md](ROADMAP.md).

Open TAP descriptors already support foreign renames and network-namespace moves
where the kernel permits them. A release runtime test verifies forwarding after
a move that changes ifindex, another rename, reuse of the original name by an
unrelated device, and carrier loss/recovery across peer restart. Workers and UDP
sockets stay in the original namespace. The external administrator must bring
the moved device up and manage its attachments; UnQuic does not move it back.
Bridge/upper-device migration and descriptor handover remain unverified.

Receive sockets use fixed nonzero ports and expose no send method. Transmit
sockets inherit their receiver's binding scope, use stable dynamic ports, and
disable IPv6 fragmentation. Send APIs take a lane payload limit from 1232 through
8952 bytes (9000 outer IPv6 bytes). DATA starts at 1232; per-lane probing
confirms larger sizes within `outer_payload_limit`, whose daemon default is 8952. Batched
sends handle up to 64 logical datagrams per syscall. Sends can
gather a wire prefix and original frame slice without copying the piece into an
intermediate packet buffer. Partial sends return the successful prefix; each
truncated receive is flagged independently. Single receives reject truncation. TAP creation leaves the device
administratively down with carrier down, uses ordinary Ethernet frames without
virtio metadata, and closes all newly opened queues if setup fails. Queues can
be moved out of the `Tap` for per-worker ownership, and any queue controls
carrier.

Default MACs hash the exact bytes `<hostname>\0<interface name>`, with one NUL
separator and no trailing NUL or case normalization. For digest `h`, the address
is `[0x60, 0xa4, 0x34, 0x40 | (h[0] & 0x0f), h[1], h[2]]`. For example,
hostname `unquic-test-host` and interface `uq-test` produce
`60:a4:34:49:63:0a`. The library reads the hostname with `gethostname` and
hashes it in-tree without allocations or extra dependencies. A hostname lookup
failure aborts creation; an explicit MAC bypasses lookup. Name changes affect
future creation only.

The suffix has 20 bits, so collisions are possible; identical hostname/interface
pairs always produce identical MACs. Set an explicit MAC where needed to avoid
duplicates on the same Ethernet network.

```rust,no_run
use unquic::{MacAddress, tap::Tap};

fn create_tap() -> Result<Tap, Box<dyn std::error::Error>> {
    let mac: MacAddress = "02:00:00:00:00:01".parse()?;
    Ok(Tap::create(c"uq0", 1500, 2, Some(mac))?)
}
// The caller retains the TAP and delegates administrative setup to its network manager.
```

The v1 wire codec covers the 48-byte header and all eleven record types. It
validates each complete datagram before exposing records, borrows DATA payloads
and ACK ranges, and encodes into caller-owned buffers without allocation.
[Fixed byte vectors](tests/vectors/wire-v1/README.md) cover every record type;
tests exercise malformed lengths, reserved fields, lane identities, ACK ranges,
and handshake isolation.

The pure handshake state machine elects roles automatically, negotiates frame
limits, retries lost flights, and preserves active state through replacement.
Tags come from nonblocking kernel randomness; tests inject deterministic tags
and monotonic times. Commit, retirement and carrier changes are returned to the
caller. The pure two-phase barrier tracks quiescence and installation across up
to 64 workers. Commands stay pending when queues are full; stale, duplicate and
unpublished acknowledgments cannot advance it. The runtime applies this barrier
before changing connection state, carrier or sending the responder's READY.
Tests cover simultaneous startup, equal tags, each lost flight, stale replay,
role reversal, either/both endpoint restarts, reply limits and liveness deadlines.

`config::Config::validate` checks names, references, ports, addresses/scopes,
MTUs, queue/lane counts, TAP names and cookie uniqueness. It returns an immutable
configuration with numeric endpoint/peer/session indices. One endpoint serves
many peers; one peer serves many sessions. Core peers hold raw IPv6 addresses
or await a supplied address. The optional `dns` layer exposes iphost's types and a blocking resolver for
helper threads; it returns IPv6 candidates without changing the active
transport destination itself.

Cookie dispatch uses a short scan through 16 sessions and a randomized hash
table above that. Allocation-counting tests cover lookup and handshake paths.
Reproduce the lookup comparison with
`cargo bench --target x86_64-unknown-linux-musl --bench cookie_lookup`.

Workers publish local counter totals after one millisecond or 256 loop turns,
and once at shutdown. Quiet workers publish remaining totals on their next wake.
Control reads worker,
session, peer and lane snapshots without locks or allocation. Session and peer
totals survive reconnection; lane snapshots identify their connection generation.
See the accounting rules below.

The worker handoff foundation is a fixed 1024-entry SPSC ring with exclusive
producer/consumer handles and cached cursors. Push/pop use no locks, allocations
or syscalls. Wraparound, full-ring ownership, destruction and concurrent transfer
have unit tests and Miri coverage. Barrier tests fill command and reply queues
and check that draining both directions permits replacement to finish without
losing control messages. Workers now notify destinations through eventfd once per
handoff batch and keep draining queues while retrying retained control work.

`reassembly::Lane` now reserves a complete datagram before applying its records,
with 64 fixed frame slots and separate 65536-entry packet and frame windows.
Permanent lower bounds and terminal bits prevent delivery after replay, expiry,
abandonment or window reuse. Complete frames remain queued through TAP
backpressure until delivery, discard or the original 100 ms deadline. Overlapping
pieces keep the first retained bytes; changed frame lengths reject the datagram.
Tests cover full queues, overlap, retirement and allocation-free operation.

`pack::Lane` segments frames into baseline datagrams and keeps monotonic packet
and frame identities. Every Ethernet frame length from 14 through 9022 bytes
round-trips through the real encoder and reassembler in tests; a maximum frame
uses eight datagrams. Flow selection handles VLAN stacks, IPv4 and IPv6
extension headers, and keeps fragments on an address/protocol key. Encoding and
flow hashing have allocation checks. The scatter/gather encoder borrows frame
pieces directly; tests compare its wire bytes with the contiguous encoder.
Batched socket calls, WouldBlock and invalid/truncated input paths also have
allocation checks. The multi-record encoder packs frame tails with following
heads and flushes at queue exhaustion. Retained metadata addresses original
buffers by frame index and byte range, without self-references or payload copies.
`FrameInput` can also concatenate a rewritten header and borrowed payload;
DATA pieces crossing that boundary remain one wire record. Callers set `end` to
the complete frame length for fresh DATA; `offset..end` can select a repair
without truncating the original length or changing its frame identity.
Segmentation can
generate just the header, preserving the supplied UDP pseudo-header seed.
Workers use these spans for UDP segmentation. Ordinary frames share compact
payload slabs; each frame retains its own identity, deadline and submitted range.
Boundary, retry, capacity and allocation tests cover both representations.
Generated sequences, reversed arrival, duplicate packets, identity exhaustion
and small-frame bursts have tests. Two 1514-byte frames pack into three baseline
datagrams. Workers now use this encoder and retain partial batches.
Each lane has a FIFO of pending frame handles. New arrivals cannot overtake
older frames when a batch fills or a send completes only partly. A worker
admits or expires at most 64 pending frames per turn, then rotates to the next
ready lane. Full or blocked lanes cost one lane visit rather than a scan of
their waiting frames.

The runtime now forwards frames between TAPs with configured or supplied IPv6 peers.
The isolated test exercises two workers, two peers and three sessions per endpoint,
with four transmit lanes for the first peer,
automatic role election, bidirectional 60-, 1514-, 1518- and 9022-byte forwarding,
carrier loss and reconnection after peer restart. The largest frame enters
through a VLAN upper device, exercising kernel tag materialization. Allocation
instrumentation observes no allocations or deallocations during these forwarding
checks. This is correctness coverage, not a throughput measurement.

Receive ACK scheduling now follows storage admission: sixteen accepted
ACK-eliciting packets or one millisecond, immediate feedback on a new gap,
and rate-limited duplicate feedback. Reassembly's `ack_deadline` and `take_ack`
APIs construct at most the newest 32 ranges from the existing receive bitmap.
Tests cover pressure rejection, reordering, window wrap, delay calculation and
allocation-free construction. Workers now emit these ACKs from four reserved
feedback buffers per worker. They reserve handoff space before consuming a
schedule and retain at most one snapshot per receive lane; further accepted
packets coalesce in that lane's bitmap. A rotating queue shares control slots
between ACKs, probe replies and handshakes. Construction visits at most 64 lane
slots per turn; sending visits at most 64 queue entries. Blocked groups and
buffer returns yield to other entries and wake at their next deadline.
ACK delay includes queue residence, capped at 1 ms. Successful-send timestamps
return to the receive owner to enforce duplicate spacing. Connection tags and
lane identity are rechecked before sends and buffer returns. Incoming ACK
application to sender history, loss detection and recovery remains Stage 3 work.
The pure sender history supports bounded ACK application after whole-record
validation. It preserves callback progress and RTT accounting across yields,
pins retained metadata, and prevents covered packets from being declared lost
while their ACK is pending.
The pure piece journal reserves mappings before I/O and keeps packet charges
separate from payload lifetime. It retains mappings after loss for late ACKs;
ACKs and bounded frame cleanup unlink mappings without scanning other histories.
It is not yet connected to worker sends or recovery.

ACK, handshake and PROBE_REPLY sends share a 4 MiB/s outer-byte ceiling per
path group, with a burst of sixteen datagrams. Worker zero owns each fixed
scheduler; successful sends alone consume its budget. A throttled handshake
retains one flight per session and wakes at the next budget deadline.
Successful submission updates the handshake retry clock and token budget;
canceled, expired and superseded flights are discarded before sending.
Named groups share across endpoints. The default groups endpoints by local
address, scope ID and binding device, ignoring names and UDP ports.
Configure a shared group when different bindings use the same bottleneck.
Data/probe congestion grants remain unfinished.

`ack::History` separately tracks reservations and successful submissions in a
fixed 65,536-number window. It validates every ACK range before applying any,
rejects retained unsent holes and future numbers, and ignores older portions.
Bitmap checks bound work independently of an ACK's numeric span. Pending sends
and unresolved congestion charges prevent window advancement; successfully
sent, uncharged feedback can age out without waiting for another ACK. Late ACKs
retain send metadata but cannot release a lost packet's charge twice. Allocation
checks cover reservation, submission, ACK, loss, window wrap and reset.
This history does not yet retain frame payloads or schedule repairs, and workers
do not yet use it for submission accounting.

`loss::Rtt` implements the pure RFC 9002 timing calculations: RTT sampling,
packet/time loss thresholds, exponential PTO periods and sender frame lifetime.
It applies [verified erratum 7539](https://www.rfc-editor.org/errata/eid7539),
which computes deviation before updating the smoothed RTT. Tests cover delay
bounds, backward time, numeric limits and exact estimator values. These
calculations are not yet connected to worker ACK processing or the shared
PTO probe budget; they do not provide recovery on their own.

The pure `loss::FrameRecovery` tracks acknowledged and outstanding byte ranges
and permits at most two repairs per byte. It excludes unsent bytes, ignores
obsolete loss notifications and accepts a late original ACK while a repair is
outstanding. Its 3392-byte fixed state can be reused without allocation; reset
clears only words needed by the new frame length. Old frame-ID mappings must
be rejected after reuse, while their packet charges remain in `History`.
A byte-by-byte reference model and boundary tests verify transitions. Packer and
reassembly tests lose every possible original piece and its first repair, then
verify one complete delivery after the second repair. Workers
still need payload retention, packet-to-frame mappings and repair submission.

Recovery, BBRv3,
descriptor adoption and daemon process integration remain unfinished. No throughput
target has been demonstrated. [BENCH.md](BENCH.md) records the release
TAP-to-TAP baseline, including severe overload loss, small-frame rates and CPU.

## Runtime lifecycle and ownership

`runtime::run(config, control)` validates configuration and checks reserved
transport storage against `daemon.memory_limit` before creating TAPs. It blocks
until shutdown or a fatal worker error. Clone `runtime::Control` before calling
run on a thread; use `ready()`, `session(index)`, counter snapshots and
`shutdown()` from the caller. Indices follow configuration order. A control
handle is single-use. Shutdown wakes workers, joins them and closes their TAP
queues before run returns; `stopped()` then becomes true. A startup error also
sets `stopped()`; inspect the run result to distinguish failure from shutdown.
Calling `shutdown()` before run requests immediate exit, but does not bypass
configuration validation or device setup. Keep and join the run thread to collect
errors. The library emits no logs.

An idle control handle does not own TAP queues. Keeping it after run returns
retains configuration, counter storage and wake descriptors, allowing final
statistics to be read. Drop all handle clones to release that storage. The current
runtime creates nonpersistent TAPs; descriptor adoption and daemon re-exec
handover are not implemented.

Readiness means every configured TAP exists. Administrative state remains the
network manager's responsibility. Receive bindings and missing transmit sockets
retry without deleting ready TAPs. Each endpoint becomes usable independently;
an unavailable endpoint does not block peers on another endpoint.

Call `supply_peer_address(peer_index, ipv6, scope_id)` after readiness to supply
an initial address or candidate. The peer's configured port stays fixed. The
method queues a bounded command and wakes the control owner; success means
queued, not connected. WouldBlock means retry later because startup or a queued
update still occupies that slot. Invalid, multicast, unspecified and IPv4-mapped
addresses fail; link-local candidates require a nonzero scope ID.

Each peer reserves two transmit socket sets. Sessions keep using their active
set while fresh handshakes run on the candidate. Each session switches through
its commit barrier; duplicate active-connection replies still use its active
path. A newer candidate can replace a failed uncommitted one. If sessions
currently use both sets, one further update waits until a set is free.
Supplying the selected address again leaves connection numbering unchanged.
A session status includes the last completed local barrier generation so callers
can observe replacement. Status fields are sampled independently.

The control queue's mutex serializes caller threads only. Workers consume its
SPSC endpoint without locking; DNS work stays in the caller or resolution
helper. Socket preparation is cold lifecycle work and does not replace packet
state before confirmation.

The worker count is the largest session queue count. Each worker owns one
reuseport receive socket per endpoint, its assigned TAP queues and transmit
lanes. The control worker binds each endpoint's entire receive group in worker
order, then transfers sockets through bounded control queues. A failed bind
closes the partial group before retrying. Connections wait until all members
are installed.

Classic reuseport BPF selects a socket using the lane minus one plus the
cookie's low 32 bits, with 32-bit wrapping addition, modulo the worker count.
The control owner registers remote lanes with the same sum modulo the session's
queue count. Steering failure counts as a socket error and retains ordinary
reuseport hashing. Handshakes, initial registration, mixed-cookie GRO aggregates,
and sessions with fewer queues may still cross workers. Datagrams that arrive
on another worker move through SPSC queues as owned buffers. Full data queues drop without ACK; buffer returns, barrier
messages and pending lane retirements remain bounded and retry.

Startup reserves 128 packet/frame buffers per worker, each with 64 KiB of
payload storage for GRO, and 64 receive-lane tables per session. Eight pool
entries are reserved for socket receive work; receive work cannot borrow the
other 120 entries, and TAP reads cannot consume the receive reserve. Reservations
stay charged until a buffer returns to its originating worker. This prevents
receive backlog from starving transmit batches. Each lane has
64 frame slots. The memory check includes these stores, handoff rings, indices,
configuration capacities, thread stacks and an allocator allowance. This is a
reservation check, not a process RSS limit or a limit on kernel socket buffers.
These conservative reservations can reject large configurations before startup.

Fixed receive storage permits at most 4096 frames and 36,954,112 reserved frame
bytes per session, below the 8192-frame/64-MiB directional ceilings. Transmit
reservations divide the frame and 64-MiB byte budgets among the union of the
session's TAP readers and transmit owners. Each worker preallocates the sum
of its participating sessions' frame quotas, capped at 8192 shared views,
plus source metadata for its 128 pool buffers. Workers outside a session's
TAP/transmit ownership set reserve no views for that session. A view holds a rewritten header of at most 128 bytes and a range in
a retained source. Larger headers use the ordinary-frame copy path. Sources
and views both consume frame credits; headers and queue links are included in
the startup memory reservation.

Ordinary frames share 64-KiB payload slabs from the existing pool. A slab's
used bytes stay charged until its last frame is released; dead ranges are
not reused while another frame refers to it. Borrowed super-frame payloads
are charged once on the owner retaining them. A foreign source exchanges
ownership with an empty owner buffer; the empty buffer returns the reader's
credit without copying payload. Transfer credits remain charged until the
return is consumed. Insufficient owner capacity drops the input and increments
`queue_drops`. Reads leave room for a source and slab; materialization checks
frame, byte and physical-buffer capacity.
No shared reservation lock or atomic is needed. Static per-worker quotas
still limit a skewed flow below the full session allowance; shared credits
remain necessary before the retained-data recovery gate.

Transmit DATA gathers rewritten headers and payload ranges from retained sources.
Partial or failed sends retain the unsent frame ranges and rebuild packet
headers with fresh numbers on retry. Only successful submissions advance frame
offsets or charge transmitted bytes; an intervening control packet cannot leave
the retry using older numbers.
Fully submitted frames release their views; a source returns to its pool after
expansion finishes and its last view is released. Retaining sent data for
repair remains unfinished.
If a frame reaches its 100 ms deadline, it is removed; remaining frames keep their
IDs and submitted offsets and are repacked with fresh packet numbers. Connection
retirement discards every queued batch before the worker acknowledges quiescence.
Receive reassembly copies pieces into 64 fixed frame slots per lane and retains
complete frames through TAP backpressure until delivery or the original 100 ms
expiry. Receive work yields after 64 logical datagrams, 64 records or 64 KiB of
UDP payload, counting GRO-expanded segments and handoffs. The record bound
matches the 64-frame TAP-read batch so small-frame receive bursts cannot take
twice the work allowance of the reverse direction. A final datagram is
indivisible and may exceed the record/byte target; malformed record areas are
charged the maximum 64 records. Unprocessed GRO bytes stay in their original
pooled buffer. An idle worker waiting for buffers held by other workers suppresses
socket read readiness until a buffer returns; the existing handoff notification
wakes it. This avoids polling a readable socket with no receive storage.
The bounded pending queue stores only buffer ownership and counts
toward the memory ceiling. Workers drain a lane after 32 completions, on admission
pressure, and at the end of each turn. Final delivery also refreshes TAP read
interest after transmit work has released buffer credits. TX, timers, control queues and readiness
polling run between receive turns. Ready-event service rotates so busy endpoints
share the receive reserve. TAP WouldBlock retains frames and waits for
EPOLLOUT before another write attempt. When the transmit buffer share is
exhausted, workers suppress TAP read interest while retaining write readiness.
Buffer returns wake the owner; reads resume when a transmit buffer is available.

On pressure, fully validated pieces for existing frames may complete them.
After TAP delivery frees slots, admission retries once. New DATA storage, packet
history and controls commit only when the whole datagram fits; a rejected
packet is never ACK-eligible. Retried tails cannot redeliver a terminal frame.
Unrelated incomplete frames are not evicted to make room. Workers batch socket
I/O and notifications, update epoll write interests only when needed, and arm
absolute monotonic timers. No worker waits for another worker's queue.

Each owned transmit lane reserves metadata for 64 frames and 64 datagrams.
A bounded round-robin queue serves ready session/lane batches. One batch may
contain several frames; a long queue continues across batches without resetting
frame identities. Planning stops before starting a frame that would cross the
GSO aggregate's byte limit. The final UDP datagram may be short. Thus local
aggregate drops do not leave orphan pieces in adjacent batches. Individual
on-wire datagram losses still require the planned bounded selective repair. Batch storage and scheduling entries count toward startup's
memory reservation. Available read rings batch TAP reads, and eligible UDP
frames share a TAP write as described below. No throughput target follows
from allocation checks alone. Small-frame acceptance must report delivered
frames per second, CPU and drops separately from large-frame Gbit/s.

## Delivery and liveness

The current runtime sends each DATA piece once. It retains a frame only until
all pieces have been submitted, the connection changes, or its 100 ms sender
deadline expires. It emits packet ACKs after full admission, but does not yet
apply received ACKs, retain sent data for repair or apply congestion control.
A lost piece can therefore prevent that entire
frame from reaching TAP. Receive expiry starts at the first retained piece and
does not move when duplicates arrive.

Completed frames are delivered as they become available, without a per-flow
reorder queue. For unfragmented UDP with at most one VLAN tag, in sessions
with a TAP queue on every worker, each reader hashes among its own transmit
lanes. This removes measured handoff costs. The hash stays stable while TAP queue placement stays stable; Linux queue migration may
put later frames of the same flow on another lane and cause reordering.
A frame's pieces and any future repairs stay on its original lane.

Stacked VLANs, TCP, fragmented IP and other traffic retain strict flow hashing.
Stacked VLAN traffic can concentrate on one TAP queue; hashing it across all
lanes avoids stranding the other transmit workers. Sessions with fewer TAP queues than global workers also retain strict flow hashing
across all transmit lanes, so those workers still provide transmit capacity.
A reader owning no transmit lane also retains hashing and handoffs. Queue and
lane counts remain independent; peers need not use identical values. eBPF is
excluded from deployment.

Flow hashing looks through one unfragmented IPv4, IPv6 or Ethernet
encapsulation, including an IPv6 segment routing header before the inner
packet. It retains the outer key and adds the inner addresses, protocol and
available ports; inner Ethernet also retains its MAC addresses and VLAN stack.
Fragments omit ports and stop descent. Truncated inner headers retain only
the outer key. Encapsulated flows use strict lane pinning.

Liveness currently sends a 52-byte UDP PROBE on local lane 1 every second,
including during data traffic. Only the first reply matching the outstanding
probe's packet number and payload length refreshes feedback; ordinary DATA and
old replies do not. This checks the session's control path, not every transmit
lane or its usable MTU. After 3 seconds without feedback the session becomes
degraded and attempts replacement while retaining its active connection. After
30 seconds it retires that connection and lowers carrier through the worker
barrier. Recovery and per-lane path monitoring remain stage 3 work.

## TAP read batching

Each worker attempts to create one 64-entry io_uring for its TAP queues.
A read batch borrows existing pool buffers and submits `READV` with
`RWF_NOWAIT`; the virtio header has a separate iovec, so frame bytes need
no shift or copy. Every submitted completion is drained before buffers are
forwarded or returned. No request crosses a worker turn. The batch size
follows recent occupancy, from 1 to 64 frames, and cannot consume the RX reserve.

Initialization failure or an unsupported read operation keeps ordinary reads.
Partial submissions and interruptions preserve accepted results; unsubmitted
requests can be withdrawn because there is no SQPOLL thread. Disabled rings
retain their mappings until shutdown. Three page-rounded mappings per worker
count toward the memory reservation. No hot-path allocation or lock is added.

An unrecoverable enter failure with outstanding completions, or a violated
ring invariant while kernel pointers are live, aborts the process. Returning
borrowed storage in that state would be unsafe. Ordinary capability failures
and interruptions use the fallback and retry paths above.

Worker/session counters expose read syscalls (`tap_read_calls`), submitted
read requests (`tap_read_requests`), ring batches (`tap_read_batches`),
valid ring frames (`tap_ring_frames`) and fallback events
(`tap_read_fallbacks`). Startup fallback counts belong to workers.
`tap_checksums` counts completed partial checksums, including segmented outputs.
`tap_gso_reads` counts retained UDP super-frames on the reader;
`tap_gso_frames` counts ordinary frames expanded on the transmit owner.

## TAP write coalescing

TAP queues use an internal 10-byte virtio header. The runtime enables checksum
offload and completes partial Internet checksums in place after validating the
metadata bounds. UDP L4 read offloads let the runtime accept host super-frames
up to 65,535 bytes. The reader selects the lane and hands the whole input to
its transmit owner once. That owner segments it into ordinary Ethernet frames
before packing; TCP read offloads remain disabled. Older kernels that reject USO
use checksum-only reads and kernel segmentation. Standalone `Tap::create`
keeps read offloads disabled until runtime setup.

UDP expansion accepts IPv4 options, VLAN tags, and IPv6 hop-by-hop, routing
and destination headers. It validates protocol, lengths and checksum offsets,
adjusts the supplied pseudo-header checksum seed, and advances IPv4 IDs.
Unknown flags, unsupported GSO types, and fragmented GSO inputs are rejected.
Pending input retains its original connection tag and 100 ms deadline.
Expansion yields after 64 frames or 64 KiB per worker turn; the last frame
may cross the byte target. Sessions take turns, and each session preserves
input order. Adaptive ring reads allow up to four GSO sources per batch after
recognizing GSO input; a preceding ordinary batch may discover more. If a
source needs local expansion, the next read limit returns to one. Pending
local expansion stops further reads. Expansion normally rewrites only the header and borrows the
payload in place. Ordinary frames and headers larger than 128 bytes use
bounded payload slabs. Frame and source credits remain charged until release.
Wire DATA remains ordinary Ethernet frames on the input's selected lane,
with unchanged packet-admission rules.

A receive flush may combine already-complete UDP frames into one USO
`writev`, borrowing their payloads directly from reassembly slots. Supported
frames contain IPv4 without options, with DF set, or IPv6 without extension
headers, optionally behind one or two 802.1Q/802.1ad tags. The complete VLAN
stack, including VID and PCP/DEI, must match and is preserved. IP/UDP lengths and checksums must be valid; UDP checksums
must be present. Headers must match apart from lengths, checksums and consecutive
IPv4 IDs. The final payload may be shorter. Other frames use ordinary writes.

Within a receive lane, grouping preserves order within the existing flow-hash definition and may
pass frames from other flows. An incompatible frame in the same flow stops
grouping. A flush still handles at most 64 frames per lane, with no wait for
more traffic; each aggregate is at most 65,535 bytes. No new frame storage,
heap allocation or lock is required.

WouldBlock and Interrupted retain every selected frame. A successful write
releases exactly those frames; a permanent failure makes them terminal.
EINVAL or EOPNOTSUPP disables USO on that TAP queue and retries a single frame.
Counters report TAP write calls, successful USO batches and frames, and offload
fallbacks as `tap_write_calls`, `tap_uso_batches`, `tap_uso_frames` and
`tap_uso_fallbacks`. Delivered bytes count the original Ethernet frames.

## Socket offloads

Transmit batches group adjacent equal-sized UDP payloads, with an optional
shorter final payload, into UDP_SEGMENT messages. The kernel gathers wire
prefixes and original frame slices directly; grouping adds no payload copy.
Groups stay within 64 logical datagrams and a conservative 64 KiB outer-packet
bound. SendDatagram now takes a borrowed list of slices, allowing multiple
record prefixes and original frame pieces. A call uses at most 1024 descriptors
and returns the complete datagram prefix it submitted when that budget fills;
validation covers the entire input before any send. Callers retain the rest.
Offload capability errors disable GSO on that socket and retry through ordinary
sendmmsg. Partial results count successful logical datagrams, so callers retain
exactly the unsent suffix. EMSGSIZE returns directly and preserves GSO support;
an oversized probe must not disable offloads for subsequent DATA.

ReceiveSocket::enable_gro attempts UDP_GRO during setup and reports whether it
succeeded. recv_batch then reports the coalesced length and segment size in each
ReceiveDatagram. Its segments() iterator borrows the logical UDP payloads,
including a shorter final segment. Payload truncation, ancillary truncation and
invalid GRO metadata reject that buffer. Each segment still needs independent
wire, cookie and connection validation. The single-datagram recv API refuses a
GRO-enabled socket because it cannot represent the aggregate.

Workers enable GRO where supported and otherwise retain ordinary receives.
An aggregate moves between owners with a segment cursor; its payload is never
copied merely to split or hand it off. Every segment gets separate cookie, tag,
lane and record validation. A malformed, stale or unknown-cookie sibling does
not hide valid later segments. The socket tests cover GSO and ordinary sends, empty datagrams, short final segments,
large-batch splitting, malformed metadata, truncation and real checksum-offload
failure with fallback. Allocation checks include offload and fallback paths.
These APIs follow the Linux [UDP socket options](https://man7.org/linux/man-pages/man7/udp.7.html);
they do not implement path probing or promise a throughput increase.

## Statistics and accounting

All control indices follow the original configuration order. TX lane IDs start
at one; RX lane IDs are the remote peer's wire IDs. These APIs return None before
runtime state is published or for an unknown index:

| Control method | Scope and lifetime |
| --- | --- |
| `worker_counters(worker)` | Physical ingress, sends, drops and errors owned by that worker |
| `session_counters(session)` | Sum of a session's worker totals across connection replacements |
| `peer_counters(peer)` | Its sessions plus transmit-socket setup errors shared by the peer |
| `transmit_lane_counters(session, lane)` | Current local lane, with its local commit generation |
| `receive_lane_counters(session, lane)` | Currently registered remote lane, with its local commit generation |

Lane totals reset on installation and disappear on retirement. A lane read can
return None while its identity changes; retry on the next sampling interval.
An installing lane can report a generation newer than `session().generation`,
which advances only after every worker completes the barrier. Fields are sampled
independently, and aggregation can span batches. Snapshots are observations, not
synchronization for transport state.

`delivered_frame_bytes` counts complete Ethernet frames successfully written
to TAP, excluding FCS. Outer byte counters include the 40-byte IPv6 and 8-byte
UDP headers. TX counts successful kernel submissions, not proof of remote
delivery. Worker RX totals count nontruncated datagrams, including malformed and
unknown-cookie input. Session RX totals count fully decoded datagrams with that
session's cookie, including stale tags, once even if handed to another worker.
RX lane totals start after current-tag and lane-owner checks. Handshake traffic
belongs to session/peer totals; it has no established lane.

`duplicate_packets` counts packet-history rejection. `duplicate_pieces`
counts whole DATA pieces already covered or terminal, including admission retries; partial
overlap that adds bytes is not a duplicate piece. `expired_frames` counts sender
and receiver deadlines, not connection retirement. `reassembly_bytes` is the
sum of reserved full frame lengths, including incomplete frames, rather than the
number of piece bytes received so far. `reassembly_frames` includes complete
frames waiting for TAP.

Pressure is attributed where detected. `admission_pressure` counts datagrams
still not fully admitted after the drain/retry attempt; useful existing pieces
may have been retained, but that packet is not ACK-eligible. `queue_drops`
counts bounded handoff/retention queue failures separately. Drops before lane
admission appear only in worker/session totals. `socket_overflow` counts
SO_RXQ_OVFL increments per receive socket, in kernel skb units, at worker scope.
GRO means these are not necessarily logical datagrams. The kernel reports a
cumulative 32-bit count on subsequent receives. Modular forward deltas below
2³¹ handle wrap and ignore stale samples; an actual jump of 2³¹ or more
between observations is ambiguous and is not counted. A final drop burst
without a later receive may not yet appear. Use namespace UDP receive-buffer
error counters alongside it.

Each receive socket requests 8 MiB with `SO_RCVBUFFORCE`. On `EPERM` it falls
back to `SO_RCVBUF`, subject to the kernel's configured limit. Worker counters
report `receive_buffer_fallbacks` and `receive_buffer_bytes`: the latter sums
the actual kernel accounting limits, including Linux's doubled overhead
allowance. It is a capacity gauge, not current occupancy. These kernel queues
are outside `daemon.memory_limit`; each worker owns one per receive endpoint.

Send SIGUSR1 to the foreground daemon to print independently sampled worker
counters to stderr. Formatting runs in the daemon's supervisory thread, outside
the workers and signal handler. The benchmark script captures these before and
after traffic. Malformed and unknown-cookie input that cannot
be attributed to a session, receive-bind failures and affinity failures remain
worker-only. Peer socket-setup failures appear in worker and peer totals; they
are not duplicated into every session. These scopes overlap: do not add worker,
session, peer and lane totals together.

Counts and occupancy gauges sum across owners. RTT and feedback delay use the
maximum; path MTU uses the smallest nonzero value. Aggregate MTU state prefers
failed, probing, baseline, configured, confirmed, then unknown. Active TX lanes
report their current DATA limit plus 48 bytes, or zero when baseline failure
suspends DATA. State is baseline (1), probing (2), confirmed (3), or failed (4).
Configured (5) remains a public counter value but workers no longer emit it.
RTT, feedback delay, repairs, pacing
and bytes-in-flight remain zero until recovery and congestion control are
implemented. Cumulative counts wrap modulo 2^64.

## Path MTU status

Each transmit socket now probes its path from the 1232-byte UDP baseline.
The daemon's per-peer `outer_payload_limit` defaults to 8952 and accepts
1232–8952; this bounds discovery. DATA uses larger sizes only after an exact
reply to a successfully submitted padded probe. Sessions sharing a socket
share its state, while independent lanes confirm their own limits. Inner TAP
MTU stays configured; the transport splits Ethernet frames into DATA pieces.

The NTT NGN production target has a 1500-byte outer IPv6 ceiling. Other paths
may differ. Discovery stops at 9000 outer bytes even on loopback. Probes gather
a 56-byte prefix and shared zero padding without a packet-sized copy.

| Size, without outer extension headers | Baseline | Confirmed 1500-byte path |
| --- | ---: | ---: |
| Outer IPv6 packet | 1280 bytes | 1500 bytes |
| UDP payload | 1232 bytes | 1452 bytes |
| Frame bytes in a single DATA record | 1164 bytes | 1384 bytes |

Each DATA datagram has a 48-byte UnQuic header and a 20-byte record prefix.
At the baseline, a 1514-byte Ethernet frame becomes pieces of 1164 and 350
bytes; a 9022-byte frame needs eight datagrams. These are transport pieces,
not outer IPv6 fragments.

Transmit setup enables `IPV6_PMTUDISC_PROBE` and `IPV6_RECVERR`, preserving
`IPV6_DONTFRAG`. This permits probes above a learned kernel PMTU while still
rejecting packets above the local interface MTU. Workers consume one error
entry per socket event. Remote Packet Too Big reports must match a live sent
probe's quoted header and destination. Reports quoting DATA await the retained
send history in the recovery stage; periodic probes still detect reductions.

EMSGSIZE and validated size hints lower the limit and replan unsent ranges with
their original frame IDs and byte offsets, using fresh packet numbers. GSO
remains enabled. If EMSGSIZE supplies no usable hint, the attempted size is
excluded and DATA falls back as needed. Other nontransient send errors still
drop unsent frames; WouldBlock and interruptions retain them until their deadlines.

The discovery state machine retains at most three submitted proofs, each scoped
by session, connection tag, lane and packet number. Two unanswered larger probes
trigger confirmation of the working size before a narrower binary search.
Three consecutive failures restart the baseline; baseline failure suspends DATA
until a matching baseline reply arrives. Validated size hints never confirm a
larger DATA limit. Delayed replies cannot revive proofs invalidated by a reduction.

The initial timeout is 16 seconds. After a verified probe, it becomes four times
that probe's RTT, with a one-second floor. This deliberately uses a shorter
timeout than RFC 8899's greater-than-15-second recommendation on measured low-RTT
paths, to limit black-hole loss. Loss can make the selected size conservative;
the state machine retries upward after 600 seconds. Current-size confirmations
run no faster than the probe timeout. They support the current unacknowledged
DATA path and must give way to transport loss feedback when ACK processing is
connected. [RFC 8899](https://www.rfc-editor.org/rfc/rfc8899.html)

Both returned and silently filtered ICMPv6 are supported. Without a valid size
hint, timed probes narrow the search; loss can therefore delay discovery and
temporarily reduce throughput. A new path resets discovery. Changing the
connection used for a shared socket's probes invalidates old proofs and
makes confirmation immediately eligible under the probe budget. Confirmed
sizes remain usable; a failed baseline stays blocked until a fresh baseline
reply succeeds. Other sessions do not reset an unchanged probe connection.
Small liveness probes remain separate and cannot confirm a DATA-sized path.

Each timer pass visits at most 64 owned sockets before returning to ordinary
work. Rotating scan order prevents a failing path from monopolising probes.
Static worker slots allow nominally one padded probe per 3 ms across workers,
at most 9000 bytes each. Padded probes still need the shared congestion grants
from Stage 3; replies already use the separate path-group control ceiling.
All steady-state discovery storage and packet data are bounded
and allocated before workers start.

## Operational checks

Readiness, carrier and administrative link state are separate. The daemon
currently exposes readiness and errors on stderr; detailed counters and session
state are available through the library's `Control` API.

| Observation | Check |
| --- | --- |
| Startup fails before readiness | Configuration validation, `memory_limit`, TAP name availability and permission to open/create TAP queues |
| TAP exists but is administratively down | External network-manager configuration; UnQuic never brings the interface up |
| TAP is ready but carrier stays down | Peer address availability, receive binding, configured fixed ports, matching cookies and compatible negotiated frame ceilings |
| Carrier drops despite incoming DATA | Fresh matching PROBE_REPLY traffic must return to the configured fixed port; DATA alone does not refresh liveness |
| Carrier is up but frames expire | Queue drops, TAP write errors, underlay loss and socket errors; small probes do not test DATA-sized delivery |
| Throughput stays low with many lanes | Flow count, TAP queue placement, worker/receive distribution and CPU use; each frame stays on one lane |

Both directions send from dynamic source ports to configured fixed receive
ports. Replies use the configured peer address and fixed port, never the
observed source tuple. Network filtering must permit that traffic pattern.

Cookies and receive tags travel in cleartext. They provide session admission
and stale-connection rejection, with no payload encryption or cryptographic
authentication. See the [security boundary](initial-design.md#5-session-cookie-and-lifecycle).

## Validation

Run the ordinary tests and lint checks with a Linux musl target:

```sh
cargo test --target x86_64-unknown-linux-musl
cargo clippy --target x86_64-unknown-linux-musl --all-targets -- -D warnings
cargo fmt --all -- --check
cargo test --target x86_64-unknown-linux-musl --features dns
cargo test --release --all-features --target x86_64-unknown-linux-musl
```

The release profile uses full LTO. Randomness goes through the Linux syscall
directly: an optimized musl build otherwise called a null `getrandom` symbol
after merging with [std's weak declaration](https://doc.rust-lang.org/src/std/sys/random/linux.rs.html).
The existing CLI provisioning test covers this failure when run in release mode.

The ignored `linux_foundation`, `linux_runtime`, `cli` and worker polling tests require
a disposable container with
`/dev/net/tun`, `CAP_SYS_ADMIN`, `CAP_NET_ADMIN` and `CAP_NET_RAW`. It creates a
fresh network and UTS namespaces before creating interfaces or changing the
hostname. Run the built
test executable there with `--ignored`, or run this command inside the container
if a Rust toolchain and the source are available:

```sh
cargo test --target x86_64-unknown-linux-musl --test linux_foundation -- --ignored
```

`linux_pmtu` also requires `ip` and `tc`. Its routed discovery test takes about
11 minutes because it waits through the real 600-second upward retry. Two
runtimes communicate through a third, private router namespace: the path starts
at 9000 outer bytes, drops to 1500 with returned ICMPv6 Packet Too Big, drops to
1280 with those errors filtered, then recovers to 9000. Each phase checks every
lane and bidirectional QinQ frames up to 9022 bytes. Packet captures check for
outer Fragment headers and capture drops; filter counters prove the silent-error
case was exercised. Run its release binary with:

```sh
./linux_pmtu --ignored --test-threads=1 --nocapture
```

The kernel test checks actual IPv6 UDP headers and port use, explicit device
binding, TAP flags/MTU/MAC, deterministic MAC recreation, administrative state,
multiqueue frame I/O,
VLAN/QinQ bytes, carrier transitions and deletion after the last queue closes.
Maximum-size QinQ is currently checked in the TAP-write direction; testing its
TAP-read direction through VLAN upper devices belongs to the composition tests.
Run every ignored test binary, including the library unit-test binary. The
runtime test also covers initial address supply, failed-candidate preservation,
migration and migration back, unchanged-address updates, multiple peers/sessions,
shutdown cleanup, forwarding allocation checks, and
TAP readiness with an unavailable receive address and unresolved peer. A worker
ownership test fills both handoff rings and checks buffer returns, lane returns,
commit completion and duplicate-install packet-number preservation. Transmit
batch tests cover successful prefixes, releasing early frame slots while later
packets remain, expiry without discarding live frames, and quiescence with full
return queues. The ownership test also
routes one unchanged GRO buffer across two sessions and two workers with bad
siblings, verifies byte accounting and returns frame reservations through full
queues. Counter tests
cover lane reuse, duplicate/expiry accounting, reservation gauges, connection
generations and peer aggregation after shutdown.
The runtime test also sends 60-, 64-, 128- and 1514-byte bursts in both directions
for ten seconds, checks every captured frame for corruption and duplicates, and
observes multi-record DATA on loopback. It checks zero allocations/deallocations
throughout that interval and reports sent/delivered counts, including drops.
This instrumented debug test is not a throughput benchmark.
The daemon test delays DNS answers until both TAPs exist, refreshes an AAAA
record, observes DATA sent to the new IPv6 destination, checks forwarding and
stops both processes with SIGTERM. Build it with `--all-features`; when copying
test executables into a container, also copy the daemon and set
`UNQUIC_TEST_BINARY` to its container path. Otherwise the test uses Cargo's
built daemon path.

Bridge/OVS/VRF matrices, real RA/DHCPv6 resolver networks, physical-NIC offload
qualification, Linux 5.0 qualification and performance acceptance remain open.

## License

Dual-licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- Mozilla Public License, Version 2.0 ([LICENSE-MPL](LICENSE-MPL))

at your option.
