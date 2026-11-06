# BBRv3 draft-07 delivery vectors

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
receive timestamp separately. No runtime scheduling or BBR state transitions
are exercised by these vectors.
