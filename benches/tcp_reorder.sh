#!/bin/sh
# Usage: sh benches/tcp_reorder.sh SENDER.pcap RECEIVER.pcap SRC DST
# Reads TAP_CAPTURE header captures of iperf3 data from SRC to DST:5201 and
# counts receiver arrivals below the flow's highest sequence that the sender
# sent only once (reordered originals), apart from retransmission refills.
set -eu
[ "$#" -eq 4 ] || { echo "usage: $0 SENDER.pcap RECEIVER.pcap SRC DST" >&2; exit 2; }
segments() {
    tcpdump -r "$2" -nn -tt -S "src host $3 and dst host $4 and dst port 5201" 2>/dev/null |
        awk -v tag="$1" '{
            for (i = 1; i <= NF; i++) if ($i == "seq") { split($(i + 1), r, "[:,]"); if (r[2] != "") print tag, $1, $3, r[1], r[2]; break }
        }'
}
{ segments S "$1" "$3" "$4"; segments R "$2" "$3" "$4"; } | awk '
function before(a, b,   d) { d = (b - a) % 4294967296; if (d < 0) d += 4294967296; return d > 0 && d < 2147483648 }
$1 == "S" {
    f = $3
    if (f in send_max && !before(send_max[f], $5) && before($4, send_max[f])) { n = ++rtx[f]; ra[f, n] = $4; rb[f, n] = $5 }
    else if (!(f in send_max) || before(send_max[f], $5)) send_max[f] = $5
    next
}
{
    f = $3; received++
    if (f in recv_max && before($4, recv_max[f])) {
        hit = 0
        for (i = 1; i <= rtx[f]; i++) if (before($4, rb[f, i]) && before(ra[f, i], $5)) { hit = 1; break }
        if (hit) { refills++; prev[f] = 0 }
        else {
            late++; lag = $2 - max_at[f]
            if (!prev[f]) events++
            prev[f] = 1
            b = lag < 0.0001 ? "under_0.1ms" : lag < 0.001 ? "0.1_to_1ms" : lag < 0.01 ? "1_to_10ms" : "10ms_or_more"; hist[b]++
        }
    } else prev[f] = 0
    if (!(f in recv_max) || before(recv_max[f], $5)) { recv_max[f] = $5; max_at[f] = $2 }
}
END {
    printf "received=%d refills=%d reordered_originals=%d events=%d", received, refills, late, events
    for (b in hist) printf " %s=%d", b, hist[b]
    print ""
}'
