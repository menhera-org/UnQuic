#!/bin/sh
# Run only inside a disposable container, as for tap_transport.sh.
# Usage: UDP_LOAD=/absolute/udp_load sh benches/netem_check.sh /absolute/unquic OUT
#
# Stage 3 check: a routed 7.5 ms path with 1% loss in bursts of four datagrams
# and 1% reordering, three equal 20-second points of four bridged QinQ flows.
# It fails when an inner receiver sees a duplicate, invalid or foreign
# datagram, when no traffic arrives, when either daemon's peak resident memory
# exceeds the default 256 MiB memory_limit plus 64 MiB for code and stacks, or
# when that peak still grows by more than 4 MiB between the last two points.
set -eu
[ "$#" -eq 2 ] || { echo "usage: $0 DAEMON OUT" >&2; exit 2; }
here=$(dirname "$0")
out=$2
env OUTER_ROUTED=1 OUTER_DELAY_US=3750 OUTER_LOSS_PERMILLE=10 OUTER_LOSS_BURST=4 \
    OUTER_REORDER_PERMILLE=10 INNER_BRIDGE=1 VLAN_DEPTH=2 TAP_TXQUEUELEN=500 \
    OUTER_PAYLOAD_LIMIT=1452 SEGMENTS=1 GENERATOR_PROCESSES=4 WORKERS=4 \
    BITRATE="125000000 125000000 125000000" \
    sh "$here/tap_transport.sh" "$1" 1522 4 20 "$out"

status=0
fail() {
    echo "netem check: $*" >&2
    status=1
}
# Peak resident KiB of the left (1) or right (2) daemon after a point.
peak() {
    sed -n 's/^VmHWM:[[:space:]]*\([0-9]*\) kB$/\1/p' "$out/$1/after.memory" | sed -n "$2p"
}
for point in 0 1 2; do
    for file in server.json reverse-server.json; do
        path=$out/$point/$file
        [ -s "$path" ] || { fail "point $point: missing $file"; continue; }
        for key in duplicates invalid wrong_run; do
            if grep -o "\"$key\":[0-9]*" "$path" | grep -v -q ":0\$"; then
                fail "point $point $file: nonzero $key"
            fi
        done
        grep -o '"unique":[0-9]*' "$path" | grep -v -q ':0$' ||
            fail "point $point $file: nothing delivered"
    done
    for side in 1 2; do
        hwm=$(peak "$point" "$side")
        [ -n "$hwm" ] || { fail "point $point: no peak memory for daemon $side"; continue; }
        [ "$hwm" -le $(((256 + 64) * 1024)) ] ||
            fail "point $point: daemon $side peak ${hwm} KiB exceeds the bound"
    done
done
for side in 1 2; do
    before=$(peak 1 "$side")
    after=$(peak 2 "$side")
    if [ -n "$before" ] && [ -n "$after" ] && [ $((after - before)) -gt 4096 ]; then
        fail "daemon $side peak grew from ${before} to ${after} KiB"
    fi
done
[ "$status" -ne 0 ] || echo "netem check: no duplicates, bounded memory"
exit "$status"
