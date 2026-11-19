#!/bin/sh
# Usage: sh benches/tunnel_summary.sh POINT...
# One line per load point of a benches/tap_transport.sh run with UDP_LOAD:
# offered and delivered Gbit/s of UDP payload and missing frames per direction,
# from the load tool's per-flow counts, and the busy CPU of the whole system
# between the point's before and sent snapshots. Unlike sweep_report it reads no
# daemon counters, so it also reads KERNEL_TUNNEL runs.
set -eu
[ "$#" -ge 1 ] || { echo "usage: $0 POINT..." >&2; exit 2; }
# The flows array of each named file that exists, one field per line.
flows() {
    for file in "$@"; do
        [ -f "$file" ] || continue
        awk 'BEGIN { RS = "\001" } {
            start = index($0, "\"flows\":[")
            rest = substr($0, start)
            print substr(rest, 1, index(rest, "]"))
        }' "$file"
    done | tr ',{' '\n\n'
}
total() {
    key=$1
    shift
    flows "$@" | awk -F: -v key="\"$key\"" '$1 == key { sum += $2 } END { printf "%.0f", sum }'
}
first() {
    tr ',{' '\n\n' < "$2" | awk -F: -v key="\"$1\"" '$1 == key { print $2; exit }'
}
for point in "$@"; do
    payload=$(first payload_bytes "$point/client.json")
    seconds=$(first seconds "$point/client.json")
    left_sent=$(total sent "$point"/client.json "$point"/client-[0-9].json)
    right_sent=$(total sent "$point"/reverse-client.json "$point"/reverse-client-[0-9].json)
    left_unique=$(total unique "$point"/server.json "$point"/server-[0-9].json)
    right_unique=$(total unique "$point"/reverse-server.json "$point"/reverse-server-[0-9].json)
    busy=$(awk -v ticks="$(cat "$point/clock-ticks")" '
        FNR == 1 && FILENAME ~ /uptime$/ { up[++times] = $1 }
        FNR == 1 && FILENAME ~ /cpu$/ { busy[++stats] = $2 + $3 + $4 + $7 + $8 + $9 }
        END { printf "%.1f", (busy[2] - busy[1]) / ticks / (up[2] - up[1]) }
    ' "$point/before.uptime" "$point/sent.uptime" "$point/before.cpu" "$point/sent.cpu")
    awk -v point="$point" -v payload="$payload" -v seconds="$seconds" \
        -v ls="$left_sent" -v rs="$right_sent" -v lu="$left_unique" -v ru="$right_unique" -v busy="$busy" '
        BEGIN {
            gbit = payload * 8 / seconds / 1e9
            printf "%s: offered %.2f + %.2f, delivered %.2f + %.2f Gbit/s, missing %.2f%% %.2f%%, %s cores busy, %.2f per Gbit/s\n",
                point, ls * gbit, rs * gbit, lu * gbit, ru * gbit,
                ls ? 100 * (1 - lu / ls) : 0, rs ? 100 * (1 - ru / rs) : 0,
                busy, lu + ru ? busy / ((lu + ru) * gbit) : 0
        }'
done
