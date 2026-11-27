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
- Performance targets: local container tests must sustain at least 20 Gbit/s in each direction simultaneously at inner MTU 1500, with CPU use reported. The deployment target is at least 5 Gbit/s over real routed IPv6 10G-EPON access lines with 4.5–7.5 ms RTT and nonzero packet loss. The local rate was first reached on November 19, with sixteen workers per daemon and a margin of one or two percent (BENCH.md); its acceptance profile was frozen on November 27. Across a real path of the target network four workers carry the deployment rate in one direction, and the other direction's path stays under it for plain UDP too (ROADMAP.md, stage 6). Section 14.2 of the design defines the acceptance work.
- No async runtime: plain pinned threads with batched syscalls. `tokio` gives one syscall per packet, no GSO/GRO, cross-core task migration, and 1 ms timers, all wrong for a few always-busy sockets. See section 10 of the design.
- Platform contract: assume the Linux kernel and only the interfaces, files, and permissions needed by the selected features, such as `/dev/net/tun` for TAP creation and `/etc/resolv.conf` when using `iphost`'s system resolver. Its configurable `ipv6`, `ra`, and `dhcpv6` modes bypass system resolvers. Document each additional host assumption with the feature that needs it. Make no assumptions about the surrounding userspace: no required XDG or other freedesktop.org conventions, GNU userspace, Android or ChromeOS facilities, systemd, or D-Bus dependencies. Binaries are built for `*-unknown-linux-musl` targets.
- Library I/O: the transport core reads no ordinary files or environment variables and writes no diagnostics. Kernel interfaces such as `/dev/net/tun`, sockets, and ioctls are allowed. The optional iphost layer uses the selected resolver's operating-system facilities; see [resolver requirements](config-model.md#daemon-resolver-selection). Application configuration-file handling and command-line I/O belong to the caller or daemon.
- Optional integration: restarts and reloads preserve TAPs by retaining and handing over their file descriptors, following rs-l2tpd's descriptor-preservation approach, and the daemon prints a systemd unit without linking to systemd or D-Bus libraries. Under that unit it speaks systemd's notification protocol and keeps its TAP queues in the manager's descriptor store, and `packaging/build.sh` builds a DEB and an RPM. None of this is needed to run it: standalone operation and the library I/O boundary stay as they are.
- Daemon readiness: every configured TAP interface exists, whether newly created or adopted. The daemon forks and its original parent exits successfully only at readiness, so any init system or launcher can wait for that exit status. This works without systemd and does not wait for network connectivity or peer handshakes. When used, the optional embedded systemd unit makes `network.target` require the daemon and wait for its readiness; the daemon must not depend on that target, on `network-online.target` or on any wait-online service.
- TAP configuration: creation accepts `Option<MacAddress>`. The in-tree `MacAddress` type accepts nonzero unicast addresses, whether globally or locally administered. `None` derives a stable address in Menhera's `60:a4:34:4x:xx:xx` MAC-M range from the current UTS hostname and interface name using SHA-256. UnQuic sets the configured MTU and optional MAC but leaves administrative state, addressing and bridge membership to the caller or network manager (for example, systemd-networkd or ifupdown). Transport state controls carrier, independently of administrative state.
- Design: see [initial-design.md](initial-design.md). Implementation order and per-step acceptance: see [ROADMAP.md](ROADMAP.md).

## Build and feature availability

UnQuic targets Linux across distributions and init systems. Ubuntu 26.04 with
Linux 7.0 is a deployment validation target, not a minimum version or exclusive
platform. Detect optional kernel facilities at runtime and retain documented
fallbacks. Source references to Linux 6.8 describe that implementation; they
do not define behavior for every kernel. Benchmark and compatibility results
must identify the kernel actually tested.

Linux 5.0 is the oldest kernel the daemon targets: it sets TAP carrier with
`TUNSETCARRIER`, which arrived there. The kernel suites run on Linux 6.8 and
7.0 in containers, and on Linux 5.0.0 in an emulated machine
(`tests/qemu-kernel.sh`, QEMU without KVM and without a network device).
On a kernel that lacks one of these the daemon does the following instead:

| Kernel interface | Since | Without it |
| --- | --- | --- |
| io_uring, for TAP reads in batches | 5.1 | One read call per frame |
| `TUNGETDEVNETNS` | 5.2 | Adoption compares device names only, so two TAPs of one name in two namespaces look like one device |
| `EMSGSIZE` for a GSO send above the device MTU | after 5.0, which says `EINVAL` | The daemon sends the same datagrams plainly; if they are too big as well it keeps GSO |
| `epoll_pwait2` | 5.11 | A timerfd carries the worker's deadline |
| UDP segmentation offload on a TAP | 6.2 | Frames are written to the TAP one at a time and read from it segmented; TCP segmentation offload still applies |

Tests that measure one of these check the kernel first and pass without it.

The crate requires Linux and declares Rust 1.89 or later. Use a
`*-unknown-linux-musl` target for builds. The default build depends only on
`libc`; enabling `dns` adds exactly `iphost 0.4.4` with RA and DHCPv6 support,
without its default features. `uuid-compat` adds lossless conversions to and from
`uuid::Uuid`, with no UUID generation dependency. `bin` builds the daemon and
enables `dns`, `serde`, `toml` and iphost's serde support. `trace` adds
diagnostics-only per-worker sender event rings; with it, the daemon writes them
to `UNQUIC_TRACE_DIR` when that variable is set. Never enable `trace` for
production or acceptance runs. No optional feature is enabled by default.

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

`unquic run` stays in the foreground. It reports `TAPs ready` on stderr once all
configured TAPs exist, and then logs the host's lines of the status report
(build, kernel, CPU, socket-buffer ceilings and each underlay device it knows
by then), so every start's log says what it ran on. SIGINT or SIGTERM requests
shutdown and joins transport
workers. Administrative interface setup remains external.

`unquic start /etc/unquic.toml /run/unquic.pid` daemonizes instead, as design
section 12.1 describes. It validates the configuration, forks twice, and the
original process exits 0 only after the daemon has written its PID file and
every configured TAP exists; it exits 1 if the daemon fails or exits first.
Readiness needs neither a reachable peer nor DNS. SIGINT or SIGTERM sent to the
waiting parent reaches the daemon too. The daemon keeps stderr for
diagnostics, reads `/dev/null` and removes its PID file on clean exit. Both
paths must be absolute.

`unquic systemd-unit /etc/unquic.toml /run/unquic.pid` prints a matching unit
for the running binary: `Type=forking` with that `PIDFile=`,
`TimeoutStartSec=infinity`, `Restart=on-failure`, `Before=network.target`,
`RequiredBy=network.target`, and no dependency on that target or on
`network-online.target`. Its `ExecReload=` runs `unquic restart` on the PID
file, so `systemctl reload` applies an edited configuration file as described
under "Reloading the configuration" below. Nothing links against systemd.

The unit runs the daemon as root with
`CapabilityBoundingSet=CAP_NET_ADMIN CAP_NET_RAW CAP_NET_BIND_SERVICE` and
`NoNewPrivileges=yes`. `CAP_NET_ADMIN` creates and configures TAPs and forces
socket buffers past the kernel's ceilings, `CAP_NET_RAW` binds sockets to a
device and serves the RA resolver, and `CAP_NET_BIND_SERVICE` allows ports
below 1024. The daemon uses no other privilege. A launcher that is not
systemd can grant the same three.

With `NotifyAccess=all` and `FileDescriptorStoreMax=4096` the unit also lets
the daemon use systemd's notification socket, which it speaks directly. When
`NOTIFY_SOCKET` is set, the daemon hands a duplicate of every TAP queue to
the manager's descriptor store once its TAPs are ready, then sends `READY=1`.
The queues then stay attached while the daemon is down: after a crash, and
across `systemctl restart`, the next start receives them and adopts every
session whose name and queue count still match, exactly as a reload does.
Queues it cannot adopt it has the manager forget, and it waits for that
before attaching by name. A clean `systemctl stop` empties the store.
Without `NOTIFY_SOCKET` none of this happens, and TAPs still persist as
described below.

`sh packaging/build.sh` builds a DEB and an RPM into `target/package/` with
`cargo-deb` and `cargo-generate-rpm`. Each holds the static binary at
`/usr/bin/unquic`, the documentation, and at
`/usr/lib/systemd/system/unquic.service` the unit that binary prints for
`/etc/unquic/unquic.toml` and `/run/unquic.pid`: a third argument to
`unquic systemd-unit` names the installed binary. Neither package depends on
anything, and neither enables or starts the service, since the daemon needs
a configuration file with provisioned cookies first. Write the file, then
`systemctl enable --now unquic`.

`IMAGE=<incus image> sh tests/systemd.sh <binary or .deb>` runs the daemon
under systemd in a disposable Incus container. It checks the unit's links
and ordering, the capability set and `NoNewPrivileges`, the report, the
descriptor store across a crash, a reload onto another session and a restart
onto a resized one, and a clean stop. It passes on systemd 255 with the
binary and with the DEB; the RPM installs, verifies and runs its commands in
a Rocky Linux 9 container.

`unquic check-config config.toml` reads a file as a start would and exits 0
when the daemon could start on it: the file parses, every field is valid, and
its storage fits `memory_limit`. It opens no device or socket.

## Implementation status

The first foundation provides `UnquicUuid` parsing and UUIDv4 provisioning,
duplicate-checked cookie lookup, checked `MacAddress` values, IPv6 receive and
transmit sockets, and persistent multiqueue TAP ownership. Frame I/O and
cookie lookup allocate no memory. The default build depends only on `libc`;
`dns` enables exactly `iphost v0.4.4`, without its default Tokio, tracing or serde
features. Its direct, RA and DHCPv6 resolver modes are available.

Required device lifetime is stronger than the current implementation: published
TAPs must survive daemon operation and descriptor-preserving restarts, and may
be removed only on explicit instruction. UDP sockets may be recreated. External
TAP renames must be respected without renaming back, recreating the old name or
changing session identity. Production code must not panic.

TAPs are persistent. Shutdown, a crash or a failed start closes their queues
but leaves each device, with its MAC, attachments and upper devices, for the
next start to reattach. A start reattaches a persistent multiqueue TAP of the
configured name only when no process holds one of its queues, keeping its MAC;
a device in use or of another kind fails startup with EBUSY. Only
`unquic remove-tap <name>` (or `tap::remove`) deletes one, once its last queue
closes. For handover, `TapQueue::into_fd` releases a queue's descriptor and
`Tap::adopt` accepts descriptors a predecessor handed over: each must be a
multiqueue TAP queue with virtio headers and no packet information, all of one
device, which keeps its current name, so an external rename survives.
`runtime::run_adopting` runs sessions on adopted TAPs of the configured queue
count.

`unquic restart /run/unquic.pid` asks a running daemon to re-execute its
binary, for example after an upgrade, as handover format version 1 in design
section 11 defines. The daemon first runs the binary with `adopt-version`, which
must print `unquic-adopt 1`; otherwise it logs the refusal and keeps running.
It then stops its workers, keeps every TAP queue open and execs the same
arguments with the queues announced through `LISTEN_FDS`. The new image adopts
them by device, so TAPs keep their ifindex, MAC, attachments and upper devices,
and republishes the PID file once ready. UDP sockets and transport state
restart. If exec fails, the daemon resumes on the same queues. A container test
checks the ifindex, MAC, a VLAN upper and the PID across a restart on Linux 6.8
and 7.0, and another checks that a successor answering another adoption
protocol leaves the daemon running unchanged. A third removes the binary after
it passes the probe: the exec fails, the daemon resumes on the queues it kept
without losing the VLAN upper, and a later restart hands those queues to a real
successor.

### Reloading the configuration

SIGHUP, SIGUSR2, `unquic restart <pid-file>` and `systemctl reload` all do the
same thing: the daemon re-executes on the configuration file as it is now.
Before it stops anything, it runs the binary with `check-config` on that file.
If the binary rejects the file, the daemon logs the refusal and keeps running
on its old configuration.

The new image adopts the TAP queues of every session whose name and queue
count are unchanged. Those devices keep their ifindex, MAC, attachments and
upper devices. Every session reconnects, because UDP sockets and transport
state restart, so forwarding pauses until the new handshakes complete. How
long that takes is not measured.

- A new session gets its TAP created, or reattached if a device of that name
  was left behind.
- A session removed from the file leaves its TAP behind with no queue
  attached, as a shutdown does. `unquic remove-tap <name>` deletes it.
- A session whose `queue_count` changed closes its queues and reattaches the
  same device by name, as a stop and a start would.
- `tap_name` and `mac` describe a device at creation, and an adopted device
  keeps its MTU. To change any of the three on an existing device, use
  `ip link`, or remove the TAP and reload.

`check-config` cannot foresee every failure. A listen port that another
process holds, or a TAP name taken by another kind of device, fails after the
handover and ends the daemon with its TAPs left in place; under the printed
unit, systemd then starts it again. A container test changes the session set
under a running daemon and checks that the kept TAP and its VLAN upper keep
their ifindex, that the new TAP appears, that the removed session's TAP is
released, and that a rejected file leaves the daemon untouched.

Library code no longer calls `unwrap`, `expect`, `panic!` or
`unreachable!`. Each parser of untrusted bytes takes 200,000 random and
mutated inputs in a test without panicking, and indexing on those paths is
checked. Packet-derived values reach no unchecked index in the workers, and a
seccomp test that fails a worker syscall, then thread creation, shows the
runtime stopping with the error while its TAP stays published. The encoders
and the batch planner use checked access too, at equal per-frame cost. Stage 4
of [ROADMAP.md](ROADMAP.md) records these.

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
[Six transcripts](tests/vectors/handshake-v1/README.md) fix what each input
must produce, effect by effect, for another implementation to check itself
against.

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
A worker about to block sets a sleeping flag and rechecks its queues, and a
producer writes the eventfd only when that flag is set, so a busy destination
costs no syscall; fences on both sides ensure one of them sees the other
(BENCH.md). A worker about to block passes its next deadline to
`epoll_pwait2` as the timeout, so it never sets a timer; on kernels older
than 5.11 it falls back to arming its timerfd. Workers set their timer
slack to 1 ns, since the default 50 µs let paced sends leave late. A turn
with pending work, or whose next deadline has already passed, polls
instead.

`reassembly::Lane` now reserves a complete datagram before applying its records,
with 128 fixed frame slots and separate 65536-entry packet and frame windows.
Permanent lower bounds and terminal bits prevent delivery after replay, expiry,
abandonment or window reuse. Complete frames remain queued through TAP
backpressure until delivery, discard or the original 100 ms deadline. Overlapping
pieces keep the first retained bytes; changed frame lengths reject the datagram.
Tests cover full queues, overlap, retirement and allocation-free operation.

A sender that expires a frame with sent but unacknowledged bytes announces it
with FRAME_ABANDON (origin 0), up to 64 per datagram on the frame's lane, so
the receiver frees the frame's slot at once instead of at its own later
deadline. These datagrams use only an idle send slot and are paced like
probes; `tx_frame_abandons` counts the frames announced. On a lossy path
this cut lost frames by about half (BENCH.md).

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

Receive ACK scheduling now follows storage admission: 48 accepted
ACK-eliciting packets or one millisecond, immediate feedback on a new gap,
and rate-limited duplicate feedback. ACKs that are due together for several
lanes of one session leave in one datagram while they fit. Reassembly's `ack_deadline` and `take_ack`
APIs construct at most the newest 32 ranges from the existing receive bitmap.
Tests cover pressure rejection, reordering, window wrap, delay calculation and
allocation-free construction. Workers now emit these ACKs from four reserved
feedback buffers per worker. They reserve handoff space before consuming a
schedule and retain at most one snapshot per receive lane; further accepted
packets coalesce in that lane's bitmap. A rotating queue shares control slots
between ACKs, probe replies and handshakes. Construction visits at most 64 lane
slots per turn; sending visits at most 64 queue entries. Blocked groups and
buffer returns yield to other entries and wake at their next deadline.
An unsuccessful ACK send retains its snapshot for a 1 ms retry; the
`ack_send_deferred` counter is separate from incoming ACK-buffer drops.
ACK delay includes queue residence, capped at 1 ms. Successful-send timestamps
return to the receive owner to enforce duplicate spacing. ACK delay is finalized
using that send timestamp; control service starts no earlier than submission,
so a delayed worker cannot accumulate a catch-up burst. Connection tags and
lane identity are rechecked before sends and buffer returns. Incoming ACKs now
reach TX-owned histories, bounded loss scans and selective payload recovery.
Once history copies validated ranges, the transfer buffer returns immediately;
ACK timing and loss protection remain until the final coordinator report returns.
Each worker owns 32 transfer buffers. While a lane's earlier ACK still waits
for its turn, a newer ACK for that lane replaces it and the older buffer goes
back at once.
UDP receive batches retain a timestamp taken after the receive call, including
across GRO expansion and worker handoff. Data sends recheck frame deadlines
immediately before each syscall and GSO fallback. Expired plans return to bounded
cleanup without being mistaken for socket backpressure. Handshake retries,
liveness probes and PMTU proofs use the successful attempt's pre-send time.

The pure sender history supports bounded ACK application after whole-record
validation. It preserves callback progress and RTT accounting across yields,
pins retained metadata, and prevents covered packets from being declared lost
while their ACK is pending.
Validation trims already-acknowledged range boundaries before application,
while preserving late-loss delivery and the original largest number for RTT.
Outstanding-packet scans also yield after a bitmap-work budget and stop at a
fixed packet number, including across sparse histories and concurrent sends.
A 128-byte summary per history skips empty bitmap ranges without retiring
late-ACK metadata. Both summary and packet-word reads consume the work budget.
The pure piece journal reserves mappings before I/O and keeps packet charges
separate from payload lifetime. It retains mappings after loss for late ACKs;
ACKs and bounded frame cleanup unlink mappings without scanning other histories.
Workers reserve these mappings before sending DATA and settle only the successful
prefix. They apply coverage changes before returning a report's record handles,
preventing premature handle reuse.
The path-group PTO state provides shared backoff and the three-PTO
stop/settlement transition. Indexed timers and per-round probe commands drive
worker loss dispatch. Before PTO, the coordinator waits for every worker to
finish a bounded loss scan and receive its final report back. A third unanswered PTO also waits for forced loss settlement before
issuing probes. New ACK work waits behind these finite scans; already accepted
ACK transactions finish first. Ordinary ACK admission also pauses until the
accepted transactions and their resulting loss scans finish, so continuous ACK
traffic cannot starve repair. A scan already in progress finishes before a fresh
pass applies the newer ACK evidence. Waiting ACKs stay in bounded storage.
An ACK that leaves no packet outstanding below its largest number owes no
pass, since a pass could act on nothing; its report then comes back without
waking a sleeping worker. A returned report, ACK or feedback box wakes its
owner only when the owner sleeps with work held behind one; a lane whose
feedback box is away keeps a timer on its next ACK instead. A notice of sent
packets wakes the controller owner only when no lane of its session is being
timed, when it restarts from idle or when it changes the datagram size. A
worker that hands work to another does not take a further turn for it.

An ACK costs its lane one report to the controller owner, whatever it
covers. The lane settles the acknowledged packets in passes of at most 64,
releasing their inflight charge and frame coverage as it goes, and adds up
what the controller needs: settled packets, delivered bytes, the newest
send record and whether anything but a path-MTU probe was delivered. The
pass that ends the ACK sends that summary. Loss reports still carry one
entry per packet.

ACK, handshake and PROBE_REPLY sends share a 4 MiB/s outer-byte ceiling per
path group, with a burst of sixteen datagrams. Received traffic raises the
ceiling: each ACK reaches worker zero with the outer bytes its lane received
since the lane's previous ACK, a thirty-second of them may be sent as
control beyond the fixed rate, and an idle group keeps at most sixteen
maximum-size datagrams of that allowance. Worker zero owns each
scheduler; successful sends alone consume its budget. A throttled handshake
retains one flight per session and wakes at the next budget deadline.
Successful submission updates the handshake retry clock and token budget;
canceled, expired and superseded flights are discarded before sending.
Named groups share across endpoints. The default groups endpoints by local
address, scope ID and binding device, ignoring names and UDP ports.
Configure a shared group when different bindings use the same bottleneck.
DATA and probes claim the separate shared congestion and pacing budget.

`ack::History` separately tracks reservations and successful submissions in a
fixed 65,536-number window. It validates every ACK range before applying any,
rejects retained unsent holes and future numbers, and ignores older portions.
Bitmap checks bound work independently of an ACK's numeric span. Pending sends
and unresolved congestion charges prevent window advancement; successfully
sent, uncharged feedback can age out without waiting for another ACK. Late ACKs
retain send metadata but cannot release a lost packet's charge twice. Allocation
checks cover reservation, submission, ACK, loss, window wrap and reset.
Workers use this history for submission accounting and incoming ACKs.
Entries without a sampling handle (acknowledged packets, canceled holes and
uncharged feedback) retire when each ACK finishes, without an owner report.
A declared loss keeps its handle for a late ACK; once the span passes half the
history, the lane reports such handles for retirement before DATA is refused.
Workers check that a number can be reserved before allocating it, so a refused
attempt leaves no hole. DATA stops 128 numbers and control 64 numbers short of
the history limit, so a PTO probe can always reach an unacknowledged tail.
Worker-local frame storage and the piece journal retain payloads and schedule repairs.

`loss::Rtt` implements the pure RFC 9002 timing calculations: RTT sampling,
packet/time loss thresholds, exponential PTO periods capped at one second, and
sender frame lifetime.
It applies [verified erratum 7539](https://www.rfc-editor.org/errata/eid7539),
which computes deviation before updating the smoothed RTT. Tests cover delay
bounds, backward time, numeric limits and exact estimator values. Worker ACK
processing and shared PTO timers use these calculations. The transmit owner
shortens a source's read-time deadline once at admission using the measured
RTT-based lifetime, bounded between 20 and 100 ms. Every expanded frame inherits
that deadline; queued expansion and repairs never extend it.

The pure `loss::FrameRecovery` tracks acknowledged and outstanding byte ranges
and permits at most two repairs per byte. It excludes unsent bytes, ignores
obsolete loss notifications and accepts a late original ACK while a repair is
outstanding. Its 3392-byte fixed state can be reused without allocation; reset
clears only words needed by the new frame length. Old frame-ID mappings must
be rejected after reuse, while their packet charges remain in `History`.
A byte-by-byte reference model and boundary tests verify transitions. Packer and
reassembly tests lose every possible original piece and its first repair, then
verify one complete delivery after the second repair. Workers retain source
views until acknowledgment, expiry or connection retirement. Fresh and repair
batches alternate when both have work. Once the opposite class waits, stop
refilling the current batch and drain its existing frames before switching;
continuous arrivals cannot postpone the switch indefinitely. Half the bounded
journal capacity is reserved for repairs.
Recovery storage counts toward `memory_limit`; configurations with many sessions
or lanes may require an explicit higher ceiling. Startup rejects an oversized
configuration instead of silently reducing its quotas. Where the ceiling has
room, startup doubles or quadruples each session's retained transmit
frames, from 8192 to 16,384 or 32,768, at about 46 MiB per session for each
8192. Frames in flight hold slots as well as frames that wait, so a fast path
of several milliseconds needs the larger budget: with four workers and one
session the default 256 MiB pays for 16,384, and 311 MiB for 32,768. A release runtime
regression drops each direction's original frame prefix and first repair,
requires delivery through the second repair, and replays the late original
without duplicate TAP delivery or hot-path allocation.

[BENCH.md](BENCH.md) records the measurements against the performance
targets, including severe overload loss, small-frame rates and CPU; its table
of current gaps says which targets are met and on what evidence.

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
statistics to be read. Drop all handle clones to release that storage. The
runtime creates or reattaches persistent TAPs, and `runtime::run_adopting`
runs on TAPs a previous process handed over. `runtime::check` reports what
run would refuse before it opens a device.

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
other 120 entries, and TAP reads cannot consume the receive reserve. When
other workers also transmit for a session, TAP reads also leave 16 further
entries free, so a lane owner can still accept frames other readers hand it;
without that margin, owners that also read the TAP filled their pools with
their own reads and refused handoffs, and four flows delivered less at 5 Gbit/s
offered than at 2. Reservations
stay charged until a buffer returns to its originating worker. This prevents
receive backlog from starving transmit batches. Each receive lane has
128 frame slots. The memory check includes these stores, handoff rings, indices,
configuration capacities, thread stacks and an allocator allowance. This is a
reservation check, not a process RSS limit or a limit on kernel socket buffers.
These conservative reservations can reject large configurations before startup.

Fixed receive storage permits at most 4096 frames and 36,954,112 reserved frame
bytes per session, below the 8192-frame/64-MiB directional ceilings. Transmit
reservations divide the frame and 64-MiB byte budgets among the union of the
session's TAP readers and transmit owners. The frame budget is 8192 times one,
two or four, the largest factor `memory_limit` pays for that still leaves
every session its own share of each store; an owner's share never exceeds
8192. Each worker preallocates the sum
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
still limit a skewed flow below the full session allowance. This remains a
performance limitation for the Stage 5 skewed-flow and lossy-path tests.

With the shared sender enabled, each transmit lane admits at most 2 MiB of
fresh logical bytes: unexpanded GSO children with their repeated headers, plus
materialized original ranges not yet submitted. One larger legal source may
enter only when that balance is zero. Excess sources are rejected before
ownership exchange; admitted tails keep their original deadlines. Successful
original submissions release their exact byte ranges, while source/frame
cleanup releases the unsent remainder. Repairs and ACK retirement do not
change this balance. This bounds queued work separately from retained payload
without enlarging physical buffers or memory limits.

Materialization stops once a lane has 64 current path UDP payloads of unsent
frame bytes, with at most one frame of overshoot. Extra children stay in their
source buffers. This follows discovered MTU rather than assuming a packet size.
Blocked lanes keep their source order while other lanes can expand. Owners hold
foreign readers' replacement credits until materialized work drains; RX credits
and stale connections return immediately. Send and expiry work release credits
before the next sleep, without polling held credits every millisecond.

Transmit DATA gathers rewritten headers and payload ranges from retained sources.
When a claim sends its entire prefix, the never-attempted suffix keeps its
packet numbers, wire metadata and reservations in the same bounded batch.
After the send completes, the worker compacts consumed metadata and adds
waiting frames under the existing turn and batch limits. Payloads stay in place;
new datagrams follow the unchanged suffix, including its short final packet.
Refill waits across packet-number gaps left by canceled probes or controls,
because submission accounting requires consecutive numbers.
Partial or failed sends retain the unsent frame ranges and rebuild packet
headers with fresh numbers on retry. Only successful submissions advance frame
offsets or charge transmitted bytes; an intervening control packet cannot leave
the retry using older numbers.
Fully submitted frames retain their views for ACK or bounded repair. A source
returns to its pool after expansion finishes and its last view is released.
If a frame reaches its original 20–100 ms sender deadline, it is removed;
remaining frames keep their
IDs and submitted offsets and are repacked with fresh packet numbers. Connection
retirement discards every queued batch before the worker acknowledges quiescence.
Receive reassembly copies pieces into 128 fixed slots of 4,544 bytes per lane
(the storage of 64 jumbo frames): a frame of a 1500-byte MTU takes one slot, a
jumbo frame two and an offload frame up to fifteen. It retains complete frames
through TAP backpressure until delivery or the original 100 ms expiry. Receive work yields after 64 logical datagrams, 64 records or 64 KiB of
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
on-wire datagram losses use bounded selective repair. Batch storage, recovery
coverage, journal mappings and scheduling entries count toward startup's
memory reservation. Available read rings batch TAP reads, and eligible UDP
frames share a TAP write as described below. No throughput target follows
from allocation checks alone. Small-frame acceptance must report delivered
frames per second, CPU and drops separately from large-frame Gbit/s.

## Delivery and liveness

The runtime sends each DATA range once and permits at most two repairs per byte.
It retains a frame until all bytes are acknowledged, the connection changes,
or its original sender deadline expires. An indexed heap finds the next transmit deadline without
scanning retained frames. Each worker turn cleans up at most 64 expired frame
views, including frames in prepared batches; sends defer any expired views
left for the next turn. Heap updates visit at most 13 levels and pending-queue
removal is constant-time. All storage is reserved at startup.
It emits packet ACKs only after full admission and applies received ACK/loss
reports to the shared congestion controller. Lost pieces remain eligible for
repair within the original frame lifetime and shared congestion/pacing limits.
Worker coverage processing yields after 128 mappings or 64 KiB of mapped bytes;
it processes each packet atomically. Journal cleanup unlinks at most 128 mappings
or frame views per turn, independently of packet-charge settlement.
Receive expiry starts at the first retained piece and
does not move when duplicates arrive.

Completed frames are delivered as they become available, without a per-flow
reorder queue. For unfragmented UDP with at most one VLAN tag, in sessions
with a TAP queue on every worker, each reader hashes among its own transmit
lanes. This removes measured handoff costs. The hash stays stable while TAP queue placement stays stable; Linux queue migration may
put later frames of the same flow on another lane and cause reordering.
A frame's pieces and repairs stay on its original lane. New frames leave a
lane whose path has failed its baseline for the next data lane that has not,
and return when a baseline probe is answered.

Stacked VLANs, TCP, fragmented IP and other traffic retain strict flow hashing.
Stacked VLAN traffic can concentrate on one TAP queue; hashing it across all
lanes avoids stranding the other transmit workers. For locally routed
double-tagged frames, tun's flow dissector appears to read the IP header as
a second tag, so every flow hashes alike and one worker reads them all. On
the loopback overload check that worker limits both directions: it cannot
read fast enough for its own, and its receive socket overflows for the
other (BENCH.md). Sessions with fewer TAP queues than global workers also retain strict flow hashing
across all transmit lanes, so those workers still provide transmit capacity.
A reader owning no transmit lane also retains hashing and handoffs. Queue and
lane counts remain independent; peers need not use identical values. eBPF is
excluded from deployment.

Fixed-key hashing can put a few heavy flows on the same transmit worker. With
`daemon.balance_flows`, on by default since November 13, frames on the strict-hash path go
through a 16,384-entry table that every TAP reader shares (128 KiB): a new
flow takes the session's next lane round-robin and keeps it while it sends,
moving only after a second of silence, even when the kernel moves it to
another TAP queue. A flow whose entry another active flow holds keeps its
hash lane. It lifts the decision workload by 1–36% in total. One flow still
runs on one lane, as with hashing; the per-reader tables it replaced let a
flow change lanes whenever the kernel moved it between TAP queues, which
lifted one lossy flow 50–90% at the cost of 1.7–2.0 million reordered
frames per direction. On its own it costs the loopback overload check about
12%; with `daemon.control_lane` (below) it gains 8–14% there instead, so
both are on by default. Set both to false for plain hash lanes (BENCH.md).

ACKs and other control records always leave on lane 0 and take their packet
numbers from its history, which spans 65,536 numbers. Under loss, lane 0's
data can fill that history while lost packets wait to be retired, and then
ACKs wait for numbers, which causes more loss. `daemon.control_lane`, on by
default since November 13, keeps lane 0 for ACKs and control when a peer has more than one
transmit lane; data uses the others. On the loopback overload check it
removed every deferred ACK send. With balanced lanes it delivered 13.7–15.9
Gbit/s at 21 Gbit/s offered, against 10.7–10.8 without it and 12.3 with hash
lanes. On the four-lane decision workload it leaves balanced lanes level but
costs hash lanes up to 10%, since data loses one lane of four, so turn it
off along with `balance_flows` (BENCH.md).

At light load each lane tends to send one frame at a time, and at a
1452-byte payload limit a 1514-byte frame takes two datagrams, the second
holding only its tail. `daemon.tx_hold_us`, 0 (off) by default and at most
1000, lets an idle lane with less than two datagrams of frames wait that long
for more, so tails share datagrams with the next frame. While it is on, a
worker counts as having unsent data for application-limited marking only
from two datagrams, the most a hold keeps, much as Linux TCP ignores under
one MSS; otherwise held frames would hide every idle moment from BBR. At
200 µs it cut CPU per delivered Gbit/s 5.5% at 0.5 Gbit/s on the decision
workload and left 5 Gbit/s unchanged. Its apparent extra loss turned out to
come from test-host stalls and BBR's ProbeRTT, not from the hold (BENCH.md).
It adds about 33 µs to the median one-way delay and 0.2 ms at p99. It
stays off: the turn hold below covers the same light load and saves more.

Frames that arrive one at a time cost several times what the same bytes
cost in a GSO train, because a worker then turns for every four frames or
so and each turn pays a TAP read, a `sendmmsg` and their wakes.
`daemon.turn_hold_us`, 300 by default since November 27 and at most 1000,
makes those turns fewer and larger: a turn that drained its sockets and its
TAP queue and took in less than sixteen frames' worth of input waits until
that long after its start before it polls again. An idle worker, one with
pending work and one whose read came back full never wait. At 200 µs it cut
a daemon's CPU 20–30% on single frames at 0.5 to 2 Gbit/s each way across a
real path, and 300 µs takes 4–8% more off, for about 0.1 ms of one-way
delay per 100 µs of hold; trains cost the same with it and without
(BENCH.md). Longer holds save more CPU and begin to lose frames at
1 Gbit/s. Set it to 0 to turn it off.

The light end is where a longer hold pays most. With four queues and lanes
on the relay bed, 1000 µs took a daemon from 67–68% of a core to 44–45% at
0.25 Gbit/s each way of single frames and from 90–92% to 62% at 0.5, for
0.8 ms more one-way delay and no more loss (BENCH.md). The default stays at
300 because every lightly loaded tunnel pays that delay. Where CPU is the
scarcer thing, as on a box with four slow cores, set
`daemon.turn_hold_us = 1000`.

BBR's ProbeRTT halves the window to half of max_bw × min_rtt for 200 ms. A
steady offer's bytes stay in flight for min_rtt plus the receiver's ACK delay
(up to 1 ms), so on a 4.5 ms path ProbeRTT can cut an offer well below half
the estimated bandwidth, and frames expire. `daemon.probe_rtt_ack_delay`,
on by default, sizes that window over min_rtt plus 1 ms instead; set it to
false for draft 07's window. On the decision workload at 0.5 Gbit/s it
kept every ProbeRTT episode off its window and, with host stalls counted
out, halved the remaining loss (BENCH.md).

`daemon.offload_frames`, on by default since November 20, advertises the optional handshake
feature that carries TAP GSO super-frames as offload frames (design section
6). When both peers advertise it, the sender cuts each super-frame into runs
of whole segments, at most 18,044 bytes each (12 segments of a 1500-byte
MTU), and sends each run as one transport frame with a virtio-net
descriptor and a partial transport checksum, instead of one frame per
segment. The receiver checks the descriptor, writes the run to its TAP with
that header, or segments it in software when the TAP refuses the offload. On
the full-size efficiency point, where nearly every TAP read is a 44-segment
super-frame, it cut CPU per delivered Gbit/s 14.7% in all six rounds and
lost no more frames. On a routed 4.5 ms path with 1% loss a frame that
expires takes its whole run with it, and until November 16 that showed as
more loss than ordinary frames had. Since then the difference lies inside
what two runs of the same settings differ by, and offload frames use 8.5%
less CPU per delivered bit there (BENCH.md).
Receiving uses the ordinary reassembly slots. Sending reserves coverage
planes for 64 offload frames per worker, about 420 KiB, and for an eighth
of the worker's frame slots where `memory_limit` still has room once the
frame slots are paid for: 3.3 MiB per worker with four workers under the
default limit. A lane whose round trip holds more runs than it
has planes sends the rest one frame per segment. Loss narrows that: an
offload frame with a hole holds up to four of its receive lane's 128 slots
until a repair arrives, so each one a worker sees halves the number of
planes it may use at once, down to 64, and each offload frame that
completes whole gives one back. Set `offload_frames` to false to send one
frame per segment always (decision 8 in ROADMAP.md).

`daemon.partial_checksums`, on by default, advertises the optional handshake
feature that leaves transport checksums partial (design section 6). When
both peers advertise it, each frame the sender cuts from a TAP GSO
super-frame carries only its TCP or UDP pseudo-header sum and a DATA flag
saying so, and the receiver writes it to its TAP with a NEEDS_CSUM header,
so neither daemon sums the payload. Frames read with complete checksums are
still verified before the receiver coalesces them. `tx_partial_frames`
counts the frames sent this way. On the full-size efficiency point it cut
CPU per delivered Gbit/s 2.5% at the median, in all six rounds, and lost
no more frames on the lossy path (BENCH.md). Set it to false to keep
complete checksums on the wire.

`daemon.stripe_flows`, off by default, turns on single-flow striping
(decision 7, design section 13.1). It is experimental: on the lossy
single-flow target it about doubles one flow's delivery, to about 2
Gbit/s in both directions, with the same share of frames reordered as
without it (BENCH.md); the worker that reads the flow from the TAP and
writes the reverse direction to it then limits the flow. It advertises the
optional `STRIPED_FLOWS` feature; it acts only where both peers advertise
it, `balance_flows` is on, and the session has at least two data lanes.
When a flow's transmit worker has kept refusing frames for its lane for
10 ms (it has no room for more), the flow takes one of the session's 16
stripes: each later TAP read of it goes
to the next data lane in turn (leaving out lanes the reading worker
transmits itself while two others remain), and every frame carries an 8-byte tag with
its stripe and sequence. Such frames are never sent as offload frames. On
the receiving side, the workers that reassemble striped frames pass them to
one writer per stripe, which writes them to the TAP in stripe order. It
holds frames behind a gap only until every lane carrying the stripe has
delivered a later frame, and for 5 ms at most; a repaired frame then
arrives late, as on any flow. `rx_striped_frames`,
`stripe_unordered` and `stripe_drops` count received ones. A stripe ends
when its flow's table entry goes idle for a second.

A receive lane belongs to worker `(cookie_low32 + lane − 1) mod n`, where
the cookie is the session's, and the socket steering program delivers each
datagram to that worker. Nothing ties this to the worker that reads the
session's TAP. Traffic the kernel cannot spread over TAP queues, such as
QinQ frames, is all read by one worker. Unless the cookie happens to give
that worker the peer's control lane, it also receives one of the peer's
data lanes while it transmits a lane of its own, and it saturates first.
`daemon.rotate_lanes`, on by default since November 20, corrects that at
run time. Once a second worker 0 checks whether one worker made at least
nine tenths of a session's TAP reads, and at least 1,000 of them. If so,
it rotates the session's receive lanes so that this worker owns the
peer's control lane and no data lane: each worker passes its lane to the
next owner with its frames and ACK state, packets still arriving at the
old owner follow it through the same queue, and a second later the
steering program follows. A session rotates at most once in ten seconds.
Two workers are a case of their own: worker 1 transmits the only data
lane, so worker 0 takes the peer's data lane whoever reads the TAP.
Rotation applies only to a session with as many TAP queues as there are
workers, on an endpoint that serves no other session, because one
steering program serves a whole endpoint. With four workers and sixteen
QinQ flows it delivers 14–19% more past saturation, and with offload
frames 42% more; at light load it costs nothing, and on a routed path
with loss its effect lies inside what two runs of the same settings
differ by (BENCH.md, decision 9 in ROADMAP.md). Set it to false to keep
every receive lane on the worker its cookie names. The wire format and
the peer are unaffected.

A TAP reader places no new flow on a lane it transmits itself while at
least six other data lanes remain, so a worker that reads a whole session
keeps its core for reading. With fewer lanes, as with four workers, flows
take the lanes in turn as before. Sessions whose TAP queues all carry
traffic lose nothing by it, since the other readers still fill each
worker's lane.

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

Each path group has one shared send budget, `pacing::Budget`. Worker zero owns
the group's BBRv3 controller and, after each feedback pass, publishes its pacing
rate, congestion window, send quantum and delivery state. Workers read the
publication under a sequence check, without a lock. Immediately before each
syscall a worker charges the plan's outer bytes to the group's atomic inflight
counter, only if the sum stays within the published window, and claims pacing
credit from the group's atomic next-send time. One claim covers at most the
smaller of BBR's send quantum and 64 maximum-size datagrams; the controller caps
the quantum at 64 KiB. Unsent bytes return after the syscall; pacing credit is
not refunded. A refused claim keeps the prepared plan and retries at the pacing
time, or after 50 microseconds while the window is full.

PTO probes bypass the window but are paced and charged. A PTO round sends at
most two probes, and feedback that ends the round voids its remaining probe
commands. A probe command preempts an unclaimed DATA plan. PMTU probes must fit
the published quantum and the whole window. A stalled zero window does not
constrain discovery, and a probe that cannot fit lowers its candidate without
confirming a larger DATA MTU. Due liveness probes wait for the current plan and
rotate among sessions before fresh DATA; history pressure retries after 1 ms
without spinning, and successful submission starts the next one-second
interval. ACK-only and PROBE_REPLY traffic retain their separate control
ceiling.

Each successful transmission's delivery snapshot is a 64-byte record in its
worker's `pacing::Records`, addressed by the handle its sent-history entry
keeps; the same handle keys the worker's recovery journal. Every worker reserves
65,535 records at startup. When a lane owner stages an ACK or loss report, it
copies the records into the report, releases the inflight charge of each newly
acknowledged or lost packet, and returns the handles of acknowledged ones. A lost
packet's record stays for a late ACK until retirement, which frees handles
without involving worker zero. Each syscall also sends worker zero a summary of
its successful packets for the PTO timers, the MTU and window-use evidence.

Worker zero applies one report per ACK: the newest acknowledged record and the
newly delivered bytes give the draft-07 rate sample through `pacing::Sampler`,
and each loss record gives a loss sample. A randomized oracle and the draft-07
vectors show these samples equal the chronological sampler's. A send that races
an idle restart or an ACK publication may read the preceding state.
Controllers start from the runtime's monotonic clock, so host uptime does not
make the first ACK trigger an expired ProbeRTT timer. Successful window use is
retained across ACKs in a packet-timed round; an application-limited idle
restart clears that evidence.

The pure `bbr::Delivery` sampler now follows draft-07's delivery-rate rules,
with fixed vectors under `tests/vectors/bbr-07`. It counts newly acknowledged
physical transmissions, including late originals and repairs, and preserves
one ACK sample across bounded processing turns. It distinguishes application
bubbles from host-queue limitation. The pure BBRv3 controller implements draft-07's
model and state machine. Worker integration includes recovery. The local
rate gate was first met on November 19 and the lossy-path gate remains
open. Each path group's budget keeps one bit per
worker holding unsent data. New data that finds no bit set, inflight below the
window, none of the worker's own frames awaiting repair and no refused
admission since the worker last held data starts draft 07's
application-limited bubble at delivered plus inflight; sends join it until
delivery passes that point. Without this, ACKs of sparse traffic before a load
could end Startup before the load arrived. A probe claimed while no bit is set
marks the bubble too, or an idle session's one liveness probe a second ends
Startup within three seconds of the handshake. A pacing claim made after its
time keeps the lateness, up to half its own interval, so a worker that sleeps
until its turn still sends at the published rate. The daemon-wide
`tolerate_random_loss` switch, on by default, deviates from draft 07: outside
bandwidth probing, a loss round cuts the short-term bounds only when it lost
more than 2% or even its smallest RTT sample, less the receiver's reported
ACK hold, rose more than max(min_rtt / 8, 250 µs) above min_rtt, the sign of
a standing queue. Random loss on an unqueued path no longer ratchets the rate
down, while congestion loss at a shared bottleneck still does: against one
TCP CUBIC flow at a quarter-BDP queue the TCP share is not reduced, and on a
4.5 ms path with 1% random loss sixteen flows at 2 Gbit/s each way lose
0.02–0.04% of their frames instead of about 1% (BENCH.md). Until November 16
the check used the round's largest sample, which the hosts' own jitter
tripped in most rounds. While it is on, `daemon.loss_threshold_permille`
(20–200, default 20, draft 07's 2%) sets the loss rate above which a round
counts as too lossy, in every loss-rate test: the probe-time inflight bound,
the short-term cut, Startup's loss exit and ProbeBW_UP. The queue check still
cuts regardless of rate. At 5% random loss the default collapses one flow to
under 10 Mbit/s and 100 (10%) delivers about 0.45 Gbit/s, with the TCP share
at a quarter-BDP bottleneck unchanged; at 1% loss it costs one direction 3%,
so raise it only where random loss exceeds 2% (BENCH.md).
Windows also deviate on sub-millisecond paths: their BDP uses at least the
receiver's 1 ms maximum ACK delay, because RTT samples exclude that delay
while the feedback loop does not. This more than doubled the loopback check's
window-limited direction; paths longer than 1 ms are unchanged.
PMTU provenance follows each successful probe through delivery sampling.
An ACK containing only newly delivered PMTU probes cannot establish a bandwidth
plateau or end Startup on that evidence. Mixed DATA/probe ACKs remain eligible;
all probe bytes still count toward delivery, loss and congestion accounting.

TX owners retain sent histories and route ACKs by the acknowledged lane,
independently of the lane carrying the ACK. ACKs are validated before entering
the TX owner's queue. Valid queued ACKs protect their covered packets while
earlier finite loss passes finish; stale, future and known-unsent claims do not.
Finishing a fully protected pass schedules continuation for waiting ACK work.
Bounded ACK and loss reports use one reserved report per worker and return to it
after worker zero applies them. Worker zero applies reports within a 64-entry
work budget per turn. Startup memory accounting includes these histories, every
worker's records and all reserved message buffers.

If local control traffic takes a waiting plan's packet numbers, the worker
renumbers the unsent packets and their history reservations before the
syscall. Payload spans, sizes and deadlines stay unchanged; history pressure
discards the plan and preserves its frames for retry. A plan larger than the
claimable credit sends its fitting prefix and keeps the rest reserved. Pending
probe commands survive connection replacement. PTO backoff stops at one second,
or at the unbacked period if that is longer, and a committed connection resets
it like a new ACK, so DATA resumes about a second after a path returns. A relay
test that drops all traffic until both ends degrade sees both directions
deliver again within 1.35 seconds, with no duplicate frame.

Release forwarding, reconnect, allocation and routed-MTU regression tests pass.
Sixteen workers per daemon first delivered 20 Gbit/s each way on
November 19, and they pass 5 Gbit/s each way over a local routed path
with loss. Since November 20 four workers pass it too when the path's
emulation runs on other cores. Runs across a real path of the target
network began on November 25, between two shared container hosts. Since
November 26 four workers carry 5.5 Gbit/s across it in one direction. The
other is held near 2 Gbit/s by the receiving machine's network card, which
limits plain UDP the same way. The lossy-path gate is set for two to four
cores on a real line and remains open in that direction. BENCH.md records
the measurements.

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

TAP writes stay plain calls: one `writev` for a frame or for a run of frames
of one flow that coalesce. Submitting single frames through the same ring
was measured and not kept, because it cost CPU on a real path where the
cards already deliver most frames in coalescible runs (BENCH.md).

Worker/session counters expose read syscalls (`tap_read_calls`), submitted
read requests (`tap_read_requests`), ring batches (`tap_read_batches`),
valid ring frames (`tap_ring_frames`) and fallback events
(`tap_read_fallbacks`). Startup fallback counts belong to workers.
`tap_checksums` counts completed partial checksums, including segmented outputs.
`tap_gso_reads` counts retained UDP/TCP super-frames on the reader;
`tap_gso_frames` counts ordinary frames expanded on the transmit owner.

## TAP write coalescing

TAP queues use an internal 10-byte virtio header. The runtime enables checksum
offload and completes partial Internet checksums in place after validating the
metadata bounds. UDP L4 and TCPv4/v6 read offloads let the runtime accept host
super-frames up to 65,535 bytes. The reader selects the lane and hands the whole input to
its transmit owner once. That owner segments it into ordinary Ethernet frames
before packing. Older kernels that reject USO retain TCP and checksum offloads,
with UDP segmentation performed by the kernel. Standalone `Tap::create`
keeps read offloads disabled until runtime setup.

At startup the runtime also raises the TAP's transmit queue, the frames the
kernel holds for the daemon's reads, to at least 1000 through rtnetlink, as
`ip link set ... txqueuelen` does; the older `SIOCSIFTXQLEN` ioctl is refused
once queues are attached. Current kernels already give new tun devices 1000,
so this matters for a device someone set shorter, and a longer queue stays.
Without `CAP_NET_ADMIN` the queue is left as it is.

UDP/TCP expansion accepts IPv4 options, VLAN tags, and IPv6 hop-by-hop, routing
and destination headers. It validates protocol, lengths and checksum offsets,
adjusts the supplied pseudo-header checksum seed, and advances IPv4 IDs.
TCP expansion preserves options, advances sequence numbers modulo 2³², places
FIN/PSH only on the final segment and CWR only on the first.
Unknown flags, unsupported GSO types, and fragmented GSO inputs are rejected.
Pending input retains its original connection tag. The transmit owner shortens
the initial 100 ms read-time deadline to its current 20–100 ms RTT-based lifetime
before admitting the source. Expired sources release all unexpanded children
without computing their headers or checksums.
Expansion yields after 64 frames or 64 KiB per worker turn; the last frame
may cross the byte target. Sessions take turns, and each session preserves
input order. Adaptive ring reads allow up to four GSO sources per batch after
recognizing GSO input; a preceding ordinary batch may discover more. If a
source needs local expansion, the next read limit returns to one. Pending
expansion of the worker's own reads stops further reads; inputs other readers
handed over do not, since a busy owner that waited for them left its own TAP
ring unread and its frames seconds old under overload (BENCH.md). Expansion
normally rewrites only the header and borrows the
payload in place. Ordinary frames and headers larger than 128 bytes use
bounded payload slabs. Frame and source credits remain charged until release.
Wire DATA remains ordinary Ethernet frames on the input's selected lane,
with unchanged packet-admission rules.

A receive flush may combine already-complete UDP frames into one USO
`writev` or consecutive TCP data frames into one TSO `writev`, borrowing their
payloads directly from reassembly slots. Supported UDP
frames contain IPv4 without options, with DF set, or IPv6 without extension
headers, optionally behind one or two 802.1Q/802.1ad tags. The complete VLAN
stack, including VID and PCP/DEI, must match and is preserved. IP/UDP lengths and checksums must be valid; UDP checksums
must be present. Headers must match apart from lengths, checksums and consecutive
IPv4 IDs. The final payload may be shorter.

TCP requires consecutive sequence numbers, identical ACK numbers, windows,
options and other header fields. ACK data segments may carry ECE. PSH closes a
group; SYN, FIN, RST, URG, CWR, reserved flags and empty payloads use ordinary
writes. TCP supports IPv4 options and IPv6 hop-by-hop, destination and type-4
routing headers within a 128-byte total header bound. Active routing headers
use the final destination for checksum validation. Fragments, encapsulated IP,
unsupported headers and noncanonical zero checksums use ordinary writes.
These fallbacks preserve the original frame bytes.

Within a receive lane, grouping preserves order within the existing flow-hash definition and may
pass frames from other flows. An incompatible frame in the same flow stops
grouping. A flush still handles at most 64 frames per lane, with no wait for
more traffic; each aggregate is at most 65,535 bytes. No new frame storage,
heap allocation or lock is required.

WouldBlock and Interrupted retain every selected frame. A successful write
releases exactly those frames; a permanent failure makes them terminal.
EINVAL or EOPNOTSUPP disables the rejected offload on that TAP queue and retries
a single frame; UDP and TCP fallback states are independent.
`tap_write_calls` counts syscalls. Successful groups and their original frames
use `tap_uso_batches` / `tap_uso_frames` for UDP and `tap_tso_batches` /
`tap_tso_frames` for TCP. Capability rejections use `tap_uso_fallbacks` and
`tap_tso_fallbacks`. Delivered bytes count the original Ethernet frames.

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

Expiry totals are partitioned into source children not yet materialized
(`tx_expired_input_frames`), materialized frames with no submitted DATA
(`tx_expired_unsent_frames`), submitted prefixes
(`tx_expired_partial_frames`), fully submitted frames awaiting ACK completion
(`tx_expired_sent_frames`), and incomplete or TAP-queued receive frames
(`rx_expired_incomplete_frames` / `rx_expired_ready_frames`). These are subsets
of `expired_frames`, published at worker, session and lane scope. They do not
measure application loss: a fully submitted frame may already have reached TAP.

Worker-only recovery counters report cleanup budget exhaustion and maximum
queue depth (`recovery_cleanup_yields` / `recovery_cleanup_peak`), reports
dispatched to shared accounting and staging continuations
(`recovery_staged_reports` / `recovery_staging_yields`), and claimed DATA
prefixes rejected for mapping capacity or changed coverage
(`recovery_mapping_pressure` / `recovery_selection_changed`). The cleanup
peak is a lifetime maximum, not a current queue gauge. Updates reuse existing
work and publication paths without new clocks, allocations or syscalls.

Pressure is attributed where detected. `admission_pressure` counts datagrams
still not fully admitted after the drain/retry attempt; useful existing pieces
may have been retained, but that packet is not ACK-eligible. `queue_drops`
counts bounded handoff/retention queue failures separately. Its
`ack_buffer_drops` subset counts incoming ACK records rejected because all
reserved ACK buffers are busy; do not add it to `queue_drops`. Both appear
in worker/session totals. `ack_coalesced` counts waiting ACK records replaced
by a newer ACK for the same lane; these are not drops. Drops before lane admission have no lane total. `socket_overflow` counts
SO_RXQ_OVFL increments per receive socket, in kernel skb units, at worker scope.
GRO means these are not necessarily logical datagrams. The kernel reports a
cumulative 32-bit count on subsequent receives. Modular forward deltas below
2³¹ handle wrap and ignore stale samples; an actual jump of 2³¹ or more
between observations is ambiguous and is not counted. A final drop burst
without a later receive may not yet appear. Use namespace UDP receive-buffer
error counters alongside it.

`tx_backlog_drops` counts TAP sources rejected by the per-lane fresh-byte
budget. It is a subset of `queue_drops`, published at worker, session and TX
lane scope. A GSO source can represent many frames; this is not an additional
frame-loss count. Two further subsets appear at worker and session scope:
`tx_admission_drops` counts TAP sources rejected by worker retention quotas,
the receive-buffer reserve or a full owner input queue, and `handoff_drops`
counts frames and datagrams dropped because another worker's handoff queue was
full. Neither is additive with `queue_drops`.

Each receive socket requests 8 MiB with `SO_RCVBUFFORCE`. On `EPERM` it falls
back to `SO_RCVBUF`, subject to the kernel's configured limit. Worker counters
report `receive_buffer_fallbacks` and `receive_buffer_bytes`: the latter sums
the actual kernel accounting limits, including Linux's doubled overhead
allowance. It is a capacity gauge, not current occupancy. These kernel queues
are outside `daemon.memory_limit`; each worker owns one per receive endpoint.

Each transmit lane's socket likewise requests 2 MiB of send buffer with
`SO_SNDBUFFORCE`, ten times a usual `wmem_default`, and on `EPERM` falls back
to `SO_SNDBUF` under `net.core.wmem_max`; `transmit_buffer_fallbacks` counts
the fallbacks. At 64 lanes a peer's worst case is about 128 MiB of queued
kernel memory, also outside `daemon.memory_limit`. Both forcing options need
`CAP_NET_ADMIN` in the initial user namespace: as root inside an
unprivileged container they still fall back.

Send SIGUSR1 to the daemon to print a report to stderr. It starts with the
host: the build's version, the kernel release and the number of CPUs the
daemon may use; the CPU's model, its clock and the allowed CPU list; the
kernel's `rmem_max` and `wmem_max`; and one line for each underlay device,
which is every endpoint's `bind_device` and the device each peer's current
address routes through. A device line gives the driver and its version, the
bus address, the MTU, the transmit queue length, the channel counts, the
header fields the card hashes UDP over IPv6 by (`udp6_hash`, from `src`,
`dst`, `sport`, `dport` and `flowlabel`), the packets each receive queue has
counted since the device came up (`rx_queue_packets`, in queue order with
`/` between),
`gro_flush_timeout` and `napi_defer_hard_irqs`, and whether checksum
offload, generic and UDP segmentation, GRO and UDP GRO forwarding are on:
the settings "Operational checks" below asks about. A value the kernel does
not give reads `unknown`, as the hash and the queue counts do on a virtual
device such as a macvlan; the queue counts also need a driver that names
them `rxN_packets`, `rx_queue_N_packets` or `rx-N.packets`. Two reports
taken under load show which queues the lanes land on. The report cannot say
which queue one flow lands on, because the kernel records that only for
connected UDP sockets. Then come each worker's independently sampled
counters, each session's
state, carrier, generation and totals followed by its transmit and receive
lanes, and each peer's totals. Sessions and peers appear under their
configured names; cookies and addresses do not appear. A daemon that has a
PID file also writes the report to `<pid-file>.status`, replacing it whole
each time and removing it on clean exit, and `unquic status <pid-file>`
sends the signal, waits up to three seconds for the new file and prints it.
Formatting runs in the daemon's supervisory thread, outside the workers and
signal handler. The benchmark script captures the worker lines before and
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
TX lanes now report smoothed RTT and charged outer bytes in flight. Worker
zero reports the sum of group pacing rates; each session reports its group's
shared rate, so summing session rates can count a group more than once.
`send_grants` counts successful claims of shared send credit; `expired_grants`
counts plans whose frame deadline passed before or at the syscall;
`cancelled_grants` counts claimed or waiting plans dropped for changed paths,
connection identities or history pressure. The names predate the shared budget.
`app_limited_marks` counts application-limited bubbles a worker started for
new data; the bubbles its probes start on an idle session are not counted.
`repairs` counts successfully submitted repair DATA records, including separate
pieces of the same range. Feedback delay remains zero until its integration.
Cumulative counts wrap modulo 2^64.

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
entry per socket event. Remote Packet Too Big reports must match the socket's
destination and either a live probe or retained successful send history.
DATA quotes must match the entire header, including cookie, connection tag,
lane, packet number and actual submitted length, on the current path and
generation. Unsent reservations cannot justify a size change. Retained sends
older than newer MTU evidence are ignored, so replayed hints cannot undo a
fresh confirmation. Periodic probes continue to detect silently dropped PTB.

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
run no faster than the probe timeout, so by themselves they need three
timeouts, three seconds or more, to notice that a path stopped carrying
datagrams above the baseline. A lane's own ACKs and losses notice sooner
(`pmtu::BlackHole`). They count alternations: a datagram of at most 1280
outer bytes acknowledged after a lost larger one and before another lost
larger one, with no larger one acknowledged since the count began. Eight
alternations restart that path's discovery from the baseline and add one to
`path_mtu_black_holes`. A frame's small tail, a probe or an abandonment
supplies the small datagrams, so a flow of single frames takes about ten
frames to notice. An outage loses small datagrams too and adds at most one
alternation when it ends. [RFC 8899](https://www.rfc-editor.org/rfc/rfc8899.html)

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
The workers share one pace for padded probes: one probe per 3 ms across all
of them, at most 9000 bytes each, whichever worker has one due. Padded probes
also claim the shared congestion and pacing budget; replies use the separate
path-group control ceiling.
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
| Startup fails with "TAP queues belong to different devices" | Something renamed the new TAP or moved it to another namespace while its queues were still attaching, so later queues landed on a second device of the old name. Let the daemon report readiness before a script or network manager touches the TAP |
| `tap_errors` rises | `tap_error_code` holds the errno of the latest refused TAP read or write: `EIO` (5) means the device a queue belongs to is administratively down, `EBADFD` (77) that the queue is no longer attached to a device. Zero means a frame failed the daemon's own checks |
| Throughput stays low with many lanes | Flow count, TAP queue placement, worker/receive distribution and CPU use; each frame stays on one lane |
| The daemon uses more CPU than a light load of single frames seems to need | `queue_count` and `tx_lanes`. Every queue is a worker and every lane past the control lane spreads the sending over another worker, so the same frames are handled in more and smaller batches. On the relay bed four queues and four lanes cost two thirds more CPU than two and two at 0.5 Gbit/s each way of single frames, and four queues with two lanes a third more (BENCH.md). Across a real path `tx_lanes = 2` in place of 4 took 18% off at 0.5 Gbit/s each way of single frames and 12% at 1, and could not carry 2; for trains it took a quarter off at 1 and 2 Gbit/s each way. Size both for the usual load ([config-model.md](config-model.md)). `turn_hold_us` trades delay for larger batches: 1000 µs in place of the default 300 cost a third less CPU at 0.25–0.5 Gbit/s each way over four queues, for 0.8 ms more one-way delay. And check what feeds the TAP: frames that arrive as GSO trains (a local sender with segmentation offload, or traffic a card's GRO merged before forwarding it) cost about a third of what the same bytes cost as single frames, 35–36% of a core against 97–100% at 1 Gbit/s each way with two workers. The kernel merges forwarded TCP by default and forwarded UDP only with `rx-udp-gro-forwarding` on the card it arrives on; that switch was measured on the underlay side only (the row on receiving below) |
| Both directions lose several percent in the same seconds, at any rate, and `tx_expired_sent_frames` jumps on both sides with few `repairs` | The path, before the daemon. Compare each side's `tx_outer_bytes` with the other side's `rx_outer_bytes`, then send plain UDP across the same path and echo every router that answers, from both ends, ten times a second. On one real path a gateway that stopped forwarding for 0.17 s at a time, on a half-hour schedule, produced exactly this: both senders' windows filled, and delivery resumed when the gateway did (BENCH.md) |
| A transmit lane reports `path_mtu_state` 4 (failed) and `path_mtu_bytes` 0, and some flows lose a burst of frames | Each lane has its own source port and may take its own path. A lane whose path stops carrying baseline datagrams suspends its DATA until a baseline probe is answered. New frames of its flows then take the next data lane whose path has not failed, and return when the probe is answered; frames already on the failed lane are lost, and a few may arrive out of order at each move. Flows stop only while every data lane has failed, and striped flows (`stripe_flows`, experimental) keep their lanes. The session stays active meanwhile, because liveness is the connection's and its probes use the first lane |
| Losses and repairs climb with load though CPU is free | `receive_buffer_fallbacks` and `socket_overflow`. Without `CAP_NET_ADMIN` the receive sockets get only `net.core.rmem_max` (often about 416 KiB after doubling), and with UDP GRO each overflow drops a coalesced batch of up to 64 datagrams. On such a host TCP through the tunnel retransmitted about 1% of its segments under unlimited load (BENCH.md); give the daemon `CAP_NET_ADMIN` or raise `rmem_max` |
| One direction stops near 2 Gbit/s with idle workers, no `socket_overflow`, and several TCP streams pass more | The receiving host's network card. Many cards hash UDP by the two addresses alone, and then every lane lands on one receive queue and one core however many source ports the sender uses. `ethtool -n DEV rx-flow-hash udp6` shows the hashed fields, as does `udp6_hash` on the report's device line where the daemon can see the card (`src,dst` is this case, and one number of `rx_queue_packets` then grows while the others stand), and `ethtool -N DEV rx-flow-hash udp6 sdfn` adds the ports. On one real path a single queue passed 2 Gbit/s cleanly and plain UDP lost a fifth or more above that (BENCH.md) |
| Receiving costs two to three times the CPU it costs on the card's own host, with the daemon in a container, behind a macvlan child or behind a container that routes to it | `ethtool -k CARD` for `rx-udp-gro-forwarding`. The kernel coalesces received UDP for a socket only where it can see the socket; datagrams it forwards to another namespace stay single unless that switch is on for the card (with `gro` on). A VLAN device in the card's own namespace needs nothing. On the test bed a daemon behind a routing namespace and a macvlan pair spent 3,030 cycles a datagram in its socket read without the switch and 1,100 with it, the same as on the link itself (BENCH.md). If coalesced trains stay short because a fast core empties the card's queue after a few packets, the card's `gro_flush_timeout` and `napi_defer_hard_irqs` let a poll wait for more: on two real hosts they raised 4–14 datagrams a received message to 32–34 and halved the receive cost again, for under a millisecond more round trip on a quiet path |
| Delay grows when the tunnel is overloaded | The TAP's transmit queue length. Under overload frames wait in the kernel's TAP ring before the daemon reads them, and with GSO input each entry can hold a 64 KB super-frame. On the loopback overload check 64 entries instead of 500 cut the median one-way delay from 34–50 ms to 4–8 ms with no loss of delivery (BENCH.md); set it with `ip link set DEV txqueuelen 64` |

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
case was exercised. A second test on the same topology, with a control lane
and two data lanes, drops everything one client sends on the data lane its
flow uses: the lane's baseline fails within seconds, the flow's new frames
cross on the other data lane, and they return to their own lane once the
filter is gone and a baseline probe is answered. Run its release binary with:

```sh
./linux_pmtu --ignored --test-threads=1 --nocapture
```

The kernel test checks actual IPv6 UDP headers and port use, explicit device
binding, TAP flags/MTU/MAC, deterministic MAC recreation, administrative state,
multiqueue frame I/O,
VLAN/QinQ bytes, carrier transitions and deletion after the last queue closes.
Maximum-size QinQ is checked here in the TAP-write direction; the composition
test below covers the TAP-read direction through VLAN upper devices.
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

`benches/netem_check.sh` is stage 3's loss-and-reordering check. In the same
kind of disposable container as the transport benchmark, it runs four bridged
QinQ flows over a routed 7.5 ms path with 1% loss in bursts of four datagrams
and 1% reordering. It fails if an inner receiver sees a duplicate, invalid or
foreign datagram, if a daemon's peak resident memory exceeds the default
`memory_limit` plus 64 MiB, or if that peak still grows between the last two of
three equal points:

```sh
UDP_LOAD=/absolute/udp_load sh benches/netem_check.sh /absolute/unquic /tmp/out
```

`examples/stack_sample.rs` samples where a running daemon's threads spend
their CPU time, for hosts that do not allow `perf`. It stops each running
thread for an instant with ptrace and prints the program counter, or the
system call when the thread is in the kernel:

```sh
CARGO_PROFILE_RELEASE_DEBUG=line-tables-only CARGO_PROFILE_RELEASE_STRIP=none \
    cargo build --release --features bin --bin unquic
stack_sample PID 10 1000 > samples.txt    # as root beside the daemon
awk '$2 == "u" && $3 != "other" { print $3 }' samples.txt | addr2line -f -i -e unquic
```

It samples a thread that sleeps often less than its CPU time warrants, so
compare shares within one thread. BENCH.md has what it found on November 27.

A runtime test creates a VRF in its namespace holding both outer addresses,
which the main table cannot reach, and forwards frames both ways between two
runtimes whose endpoints bind to it.

A composition test puts one TAP in a Linux bridge fed from a veth and gives the
other an 802.1Q upper, an 802.1ad/802.1Q stack and a macvlan. Unicast frames
from three source MACs, broadcast, multicast and an 802.3 LLC frame cross
unchanged; a 9018-byte single-tagged frame and a 1522-byte QinQ frame arrive at
their uppers without the tags, which covers the TAP-read direction for both.

An Open vSwitch test makes one TAP a system port of an OVS bridge beside a
veth. It starts its own ovsdb-server and ovs-vswitchd inside the test's network
namespace, using the userspace tree named by `UNQUIC_OVS_ROOT` (default `/`),
and skips when that tree has no OVS. Unicast from three source MACs,
broadcast, multicast, LLC and 9014-byte frames cross unchanged in both
directions, and a 1522-byte 802.1ad/802.1Q frame arrives intact at a stacked
upper on the far side. The test needs the openvswitch kernel module.

An overload test floods one session's TAP with full-size frames for two seconds
while a second session on the same peer carries numbered frames. Every frame
the quiet session delivers matches what was sent and arrives once, the
transport allocates nothing during the flood, and once the backlog drains the
quiet session delivers every frame again. Sixteen sessions on one peer start
under a sufficient `memory_limit` with every TAP present; under 64 MiB startup
fails with OutOfMemory before creating any.

Real RA/DHCPv6 resolver networks, physical-NIC offload qualification and
performance acceptance remain open. Linux 5.0 is qualified in an emulated
machine without a network card: all 132 kernel tests pass there.

## License

Dual-licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- Mozilla Public License, Version 2.0 ([LICENSE-MPL](LICENSE-MPL))

at your option.

The BBR sampler and controller include code derived from the IETF draft under the
Revised BSD License; retain its notice in [LICENSE-BBR](LICENSE-BBR).
