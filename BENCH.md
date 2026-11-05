# Transport benchmark baseline

The local throughput gate has **not passed**. At inner MTU 1500, the stage-2
transport delivers far less than the required 20 Gbit/s in each direction
simultaneously under unlimited offered load. Recovery, congestion control,
pacing and path-MTU discovery are not implemented in this baseline. The real
5 Gbit/s access-line target is also untested.

## Measurement profile

| Field | Value |
| --- | --- |
| Date / revision | 2026-11-05 / `74adc30` |
| Build | Rust 1.99.1, x86_64-unknown-linux-musl, all features, release |
| Release settings | opt-level 3, full LTO, strip, one codegen unit |
| Kernel | Linux 6.8.0-147-generic, x86_64 |
| CPU | Intel Xeon Gold 6138, nominal 2.00 GHz |
| Transport | Two daemon processes; one peer/session each; four workers, four TAP queues and four TX lanes per endpoint |
| Outer path | IPv6 UDP over loopback in a fresh container network namespace; 1232-byte UDP ceiling; GSO/GRO enabled when supported |
| Inner path | Separate left/right network namespaces; IPv4 UDP through the two TAPs; MTU 1500; fixed neighbors; no direct inner route between namespaces |
| Generator | iperf3 3.20, UDP, bidirectional, 1 or 16 flows per direction |
| Duration | One 10-second sample per row, both directions active together; no warmup |
| CPU budget | UnQuic left 0–3, right 4–7; generator left 8–11, right 12–15 |
| Placement | These 16 logical CPUs are distinct physical cores on one socket; shared machine, no exclusive CPU reservation |
| Impairment | No injected delay or loss; overload can still drop packets |

The generator's UDP payload length is frame length minus 42 bytes
(Ethernet + IPv4 + UDP). Frame sizes exclude FCS, preamble and inter-frame gap.
DF is enabled on inner IPv4 packets. The outer namespace's IPv6 fragment creation
and reassembly counters stayed at zero in every run.

Goodput below counts **received UDP payload bytes**, excluding every header.
Frames/s is received payload bytes divided by payload length and receiver
duration. Missing percentage is `100 * (sent - received) / sent`; it includes
missing tails, which iperf's sequence-gap percentage may omit. A sample ending
with traffic still in flight counts that traffic as missing. This is not a
zero-loss forwarding-capacity measurement.

L→R means left generator to right generator; R→L is the simultaneous reverse
direction. The 100 Mbit/s row is a separate paced diagnostic; other rows use
`-b 0` (unlimited).

## Delivered traffic

| Frame bytes | Flows/direction | Offered rate/flow | L→R frames/s | R→L frames/s | L→R Gbit/s | R→L Gbit/s | L→R missing % | R→L missing % |
| ---: | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 60 | 1 | unlimited | 192591 | 27955 | 0.0277 | 0.0040 | 43.259 | 91.603 |
| 60 | 16 | unlimited | 20915 | 21915 | 0.0030 | 0.0032 | 98.162 | 97.989 |
| 64 | 1 | unlimited | 187958 | 152864 | 0.0331 | 0.0269 | 44.364 | 49.890 |
| 64 | 16 | unlimited | 249628 | 295895 | 0.0439 | 0.0521 | 73.766 | 67.818 |
| 128 | 1 | unlimited | 191970 | 228259 | 0.1321 | 0.1570 | 43.892 | 30.054 |
| 128 | 16 | unlimited | 285007 | 254891 | 0.1961 | 0.1754 | 72.403 | 72.245 |
| 1514 | 1 | unlimited | 344 | 329 | 0.0041 | 0.0039 | 99.891 | 99.888 |
| 1514 | 16 | unlimited | 22079 | 21163 | 0.2600 | 0.2492 | 97.852 | 97.881 |
| 1514 | 1 | 100 Mbit/s | 8491 | 8490 | 0.1000 | 0.1000 | 0.000 | 0.020 |

These runs demonstrate severe overload loss, not the intended throughput.
Kernel UDP receive-buffer errors occurred in all unlimited runs. The paced
100 Mbit/s run had none. Other losses are not attributed by this measurement;
profiling and transport counters are needed before choosing an optimization.
Flow hashing, receive-socket distribution and shared-machine scheduling vary
between runs, so one sample is insufficient to rank small differences.

The existing ten-second frame-storm test separately checks corruption,
duplicates and allocations/deallocations. iperf measures delivery here; it does
not replace that correctness test.

## CPU accounting

Each process percentage uses one core as 100%; a four-core process can reach
400%. UnQuic percentages come from process user+system tick deltas over the
snapshot interval (100 ticks/s here). Generator percentages come from iperf.
The snapshot interval includes the short control-connection setup/teardown.
Each endpoint handles both directions, so its CPU cannot be assigned entirely
to one traffic direction.

| Frame/flows | UnQuic left % | UnQuic right % | Generator left % | Generator right % |
| --- | ---: | ---: | ---: | ---: |
| 60/1 | 196.8 | 163.7 | 105.8 | 148.0 |
| 60/16 | 292.6 | 304.8 | 393.5 | 394.9 |
| 64/1 | 111.6 | 135.0 | 135.7 | 146.2 |
| 64/16 | 346.6 | 333.1 | 390.5 | 397.4 |
| 128/1 | 195.9 | 217.1 | 154.0 | 150.5 |
| 128/16 | 315.5 | 347.6 | 395.2 | 394.0 |
| 1514/1 | 200.9 | 160.2 | 100.0 | 99.8 |
| 1514/16 | 311.6 | 322.8 | 394.8 | 393.3 |
| 1514/1 (100M) | 35.3 | 40.3 | 12.3 | 11.9 |

Each entry below is **busy%/softirq%** for the listed CPUs, in order. Busy
includes user, nice, system, IRQ and softirq time; the denominator also includes
idle, iowait and steal time. These are per-CPU `/proc/stat` deltas, including
unrelated work on the shared machine. They must not be added to process CPU
percentages: some work is already represented there.

| Frame/flows | CPUs 0,1,2,3 | CPUs 4,5,6,7 | CPUs 8,9,10,11 | CPUs 12,13,14,15 |
| --- | --- | --- | --- | --- |
| 60/1 | 36.0/0.0, 91.1/8.0, 14.4/0.2, 68.4/0.4 | 65.5/0.3, 1.2/0.0, 1.1/0.1, 99.6/1.0 | 1.7/0.0, 99.1/0.5, 4.3/0.0, 4.9/0.0 | 57.2/0.0, 28.4/0.0, 15.8/0.0, 60.1/0.0 |
| 60/16 | 91.1/1.6, 25.3/1.3, 97.4/1.5, 82.1/2.2 | 85.7/1.7, 55.9/1.5, 94.4/2.3, 72.9/2.4 | 97.3/0.2, 97.2/0.2, 95.3/0.0, 96.6/0.0 | 97.6/0.0, 99.1/0.0, 99.3/0.0, 99.2/0.0 |
| 64/1 | 14.7/0.7, 0.3/0.0, 97.3/0.4, 0.1/0.0 | 96.1/0.3, 0.6/0.0, 39.0/1.8, 0.3/0.0 | 0.2/0.0, 97.3/0.3, 18.0/0.0, 19.2/0.0 | 97.1/0.0, 14.9/0.0, 29.1/0.0, 10.7/0.0 |
| 64/16 | 93.2/1.3, 93.9/0.9, 98.8/1.1, 63.1/1.5 | 47.6/2.1, 97.3/1.7, 92.5/1.3, 99.0/1.0 | 99.0/0.5, 99.3/0.7, 99.1/0.0, 95.2/0.0 | 99.2/0.0, 99.5/0.0, 99.3/0.0, 99.3/0.0 |
| 128/1 | 0.3/0.0, 0.6/0.1, 98.9/0.5, 97.7/1.8 | 0.5/0.0, 62.6/5.7, 60.1/0.2, 97.0/0.3 | 28.6/0.1, 33.6/0.2, 55.8/0.0, 41.4/0.0 | 30.9/0.0, 65.5/0.0, 28.2/0.0, 40.1/0.0 |
| 128/16 | 62.1/2.0, 93.6/1.4, 65.6/1.5, 97.4/0.9 | 88.7/1.7, 66.9/1.3, 97.0/0.9, 97.7/1.6 | 99.5/0.3, 99.5/0.6, 96.9/0.0, 99.1/0.0 | 99.1/0.0, 98.3/0.0, 99.2/0.0, 99.0/0.0 |
| 1514/1 | 62.3/0.0, 63.9/0.4, 0.3/0.0, 81.5/10.6 | 85.6/6.3, 1.6/0.1, 83.2/0.3, 0.5/0.0 | 0.4/0.0, 97.7/0.4, 5.1/0.0, 3.0/0.0 | 97.6/0.0, 4.3/0.0, 5.2/0.0, 1.6/0.0 |
| 1514/16 | 93.4/2.2, 52.8/0.8, 72.4/2.0, 96.4/2.2 | 62.9/1.8, 97.4/2.4, 88.9/2.4, 75.2/1.6 | 95.6/0.3, 95.5/0.5, 94.2/0.0, 95.1/0.0 | 97.4/0.0, 94.2/0.0, 97.3/0.0, 97.4/0.0 |
| 1514/1 (100M) | 26.6/1.8, 8.5/0.3, 3.7/0.0, 3.3/0.0 | 1.2/0.1, 26.5/0.5, 0.4/0.0, 13.2/1.9 | 4.7/0.1, 5.0/0.3, 4.2/0.0, 4.4/0.0 | 7.7/0.0, 6.3/0.0, 5.4/0.0, 4.4/0.0 |

Exact counts, durations, process ticks, per-core ticks and outer UDP/fragment
counter deltas are in [tap_transport-baseline.json](benches/tap_transport-baseline.json).
The ordered arrays and tick-field names are defined at the top of that file.
It preserves inputs to these calculations rather than rounded table values.

## Reproduce

Build the release daemon:

```sh
cargo build --release --all-features --target x86_64-unknown-linux-musl
```

Copy that static binary and [tap_transport.sh](benches/tap_transport.sh) into
a disposable container with no external network. It needs `/dev/net/tun`,
CAP_SYS_ADMIN, CAP_NET_ADMIN, CAP_NET_RAW, iproute2, iperf3 with `--bidir`,
taskset, unshare, nsenter and POSIX shell utilities. CPUs 0–15 must be available;
the script uses the fixed budget above. Delete the container after collecting
results, including on failure.

Inside the container, with the daemon at `/root/unquic`:

```sh
for frame in 60 64 128 1514; do
    for flows in 1 16; do
        sh /root/tap_transport.sh /root/unquic "$frame" "$flows" 10 \
            "/root/result-$frame-$flows"
    done
done
BITRATE=100M sh /root/tap_transport.sh /root/unquic 1514 1 10 /root/result-paced
```

Each output directory must be new. The script creates private mount/network
namespaces, starts both daemons on disjoint CPU sets, moves their TAPs into the
two inner namespaces, and waits for carrier before starting traffic. It saves
iperf JSON, process/thread CPU snapshots, per-core counters, link counters,
kernel/CPU descriptions and daemon logs, then stops the processes and removes
the inner namespaces. The setup uses existing tools; no new traffic generator
or runtime dependency is needed.

Repeat the matrix after stage 3 adds recovery, pacing, congestion control and
confirmed larger path MTUs. Keep both acceptance gates open until repeatable
complete-transport measurements pass. No access-line or netem loss-sweep result
is claimed by this loopback baseline.
