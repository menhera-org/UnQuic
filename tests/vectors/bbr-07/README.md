# BBRv3 draft-07 vectors

These hand-calculated vectors fix the delivery sampler's behavior against
[draft-ietf-ccwg-bbr-07, sections 2.3 and 4.1.2](https://www.ietf.org/archive/id/draft-ietf-ccwg-bbr-07.html#section-4.1.2).
They cover the send/ACK interval maximum, idle restart, timestamps beginning
at zero, short intervals, late ACKs after loss, duplicates and application bubbles.

All times are nanoseconds; rates are whole outer bytes per second, rounded down.
A rate is absent when its interval is zero or below the supplied minimum RTT.
Transmission indices start at zero; sampler IDs start at one and span lanes.

- `S time bytes inflight_before host_limited`: successful physical send.
- `M inflight`: mark an application-limited bubble after checking the draft's prerequisites.
- `L index changed`: declare loss; `changed` is one only on the first effective declaration.
- `A time min_rtt host_limited indices rate interval delivered newly_acked prior_delivered lost tx_in_flight app_limited sample_host_limited newest_id`: accumulate and finish an ACK.
- `E time host_limited indices`: ACK produces no sample.

Flags use zero/one; indices are comma-separated. A dash denotes no indices or
no rate. ACK observation time belongs to the shared owner; RTT uses the original
receive timestamp separately. Delivery vectors do not exercise runtime scheduling.

## Controller cycle

`controller-cycle.txt` fixes a loss-free 1.5 MB/s, 10-ms path with 1500-byte
maximum datagrams, a 15,000-byte initial window and random seed one. ACKs
advance packet-timed rounds and deliver 15,000 outer bytes. The vector checks
Startup's three-round plateau, Drain, and a complete ProbeBW cycle. Its columns
are:

`time_ms rate newly_acked inflight app_limited cwnd_limited state cwnd pacing quantum`

Rates are bytes/s and volumes are outer bytes. The controller starts with an
RTT estimate of 10 ms. Each line names the limits after processing its ACK.
These are model vectors, not a network simulator or throughput measurement.
Unit tests additionally cover RTT refresh, loss, recovery undo, filter expiry,
application limitation, integer boundaries and a tenfold capacity reduction.

For the window-use flag defined in [section 2.2](https://www.ietf.org/archive/id/draft-ietf-ccwg-bbr-07.html#section-2.2),
the transport reports fresh evidence from successful transmissions, excluding
unused reservations. The controller accumulates it across ACKs. A round-boundary
ACK consumes the accumulated flag, then retains only fresh evidence for the new
round. This preserves the first full flight's evidence across its ACKs; a
zero-inflight, application-limited restart clears it. Draining a flight while
work remains queued does not prove idle. The draft does not prescribe this
transport-side tracking algorithm. Regressions cover first-flight ACKs, boundary
ACKs with and without fresh evidence, and idle restart.

## Pinned draft interpretations

The controller follows the draft-07 pseudocode when its prose differs:

- Drain's timeout uses `round_count > drain_start_round + 3`, hence the fourth
  following round. Section 5.3.2's prose says three.
- Probe loss sets the long-term inflight bound only for non-application-limited
  samples (§5.5.10.2). Appendix A6 describes an application-limited update.
- Drain applies the short-term inflight cap; the long-term cap follows
  `BoundCwndForModel` (§5.6.4.7), despite the Drain row in §5.6.1.
- Idle restart uses `SetPacingRateWithGain(1)`, including the 1% margin, though
  §5.4.1's prose says exactly the measured bandwidth.
- Reno probe timing converts the target volume to maximum-size datagrams,
  rounded up, before comparing it with the 63-round limit (§5.3.3.8).
  The pseudocode otherwise compares a byte volume with a round count.
- An RTO allows one maximum datagram beyond inflight (§5.6.4.4). PTO alone is
  not an RTO and must not trigger this reduction.

All model quantities use integers. BDP rounds up to bytes; rates, gains,
headroom and probe increments round down. A negative loss-prefix estimate
clamps to zero. Volume arithmetic saturates; lifetime delivered/lost counters
instead return an exhaustion error before mutation. Unknown RTT is represented
separately from a zero RTT sample. The two-cycle bandwidth and ten-round ACK
aggregation maxima use exact fixed arrays; Startup retains only the current
aggregation round. Probe timing uses caller-seeded SplitMix64 without syscalls.

Each path group must deliver loss samples in cumulative-counter order and every
newly delivering ACK exactly once. Enter recovery before its loss reports; end
it before applying the terminating ACK to reapply model bounds. Repeated recovery
entry is rejected. ProbeRTT's return flag tells the caller to mark the sampler
application-limited even if data remains queued. Successful transmission reports
must precede feedback, as specified in the shared grant-epoch design.

## PMTU sample provenance

The transport tags successful PMTU probes through flight completion and ACK
accumulation. A sample is PMTU-only only when every newly delivered packet is
a PMTU probe. Duplicate DATA ACKs do not change that classification; newly
delivered DATA does. Such samples still update delivery, RTT, bandwidth filters,
loss and inflight accounting, but cannot establish the three-round bandwidth
plateau. Sparse probe cadence does not demonstrate a full application path.
This is a transport-specific exclusion from `check_full_bw`, not a global
application-limited claim. The runtime's general application-limited detection
marks bubbles from each path group's unsent-data mask; see design section 9.

Existing text vectors describe ordinary DATA and set `pmtu_only=false`.
Unit tests cover all probe/DATA combinations and ACK orders for three packets,
late and duplicate ACKs, zero/partial sends, flight reuse, and a probe-only
sequence followed by genuine non-growing DATA rounds.
