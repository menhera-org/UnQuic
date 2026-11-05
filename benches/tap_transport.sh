#!/bin/sh
# Run only inside a disposable container with tun, iproute2, iperf3 and taskset.
# Usage: sh benches/tap_transport.sh /absolute/unquic FRAME_BYTES FLOWS SECONDS OUT
set -eu
[ "$#" -eq 5 ] || { echo "usage: $0 DAEMON FRAME_BYTES FLOWS SECONDS OUT" >&2; exit 2; }
case "$1" in /*) ;; *) echo "DAEMON must be an absolute path" >&2; exit 2;; esac
case "$2" in 60|64|128|1514) ;; *) echo "frame size must be 60, 64, 128 or 1514" >&2; exit 2;; esac
for number in "$3" "$4"; do
    case "$number" in ''|*[!0-9]*) exit 2;; esac
    [ "$number" -ge 1 ] && [ "$number" -le 60 ] || exit 2
done
case "${OUTER_PAYLOAD_LIMIT:-1232}" in ''|*[!0-9]*) exit 2;; esac
[ "${OUTER_PAYLOAD_LIMIT:-1232}" -ge 1232 ] && [ "${OUTER_PAYLOAD_LIMIT:-1232}" -le 1452 ] || exit 2
mkdir "$5"
# A private mount namespace keeps ip-netns bind mounts out of the caller.
exec unshare -m -n --propagation private sh -s -- "$@" <<'BENCH'
set -eu
daemon=$1
frame=$2
flows=$3
seconds=$4
out=$(cd "$5" && pwd)
left=
right=
server=
reverse_server=
client=
reverse_client=
cleanup() {
    for pid in "$client" "$reverse_client" "$server" "$reverse_server" "$left" "$right"; do
        [ -z "$pid" ] || kill "$pid" 2>/dev/null || :
    done
    wait || :
    ip netns del uq-left 2>/dev/null || :
    ip netns del uq-right 2>/dev/null || :
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM
netip() {
    namespace=$1
    shift
    nsenter --net="/run/netns/$namespace" ip "$@"
}
ip link set lo up
ip netns add uq-left
ip netns add uq-right
for side in left right; do
    if [ "$side" = left ]; then port=40100; remote=40101; mac=01; else port=40101; remote=40100; mac=02; fi
    cat > "$out/$side.toml" <<CONFIG
[[endpoints]]
name = "rx"
listen_address = "::1"
listen_port = $port
[[peers]]
name = "peer"
endpoint = "rx"
host = "::1"
peer_port = $remote
tx_lanes = 4
outer_payload_limit = ${OUTER_PAYLOAD_LIMIT:-1232}
[[sessions]]
name = "uq-$side"
peer = "peer"
cookie = "00112233-4455-4677-8899-aabbccddeeff"
inner_mtu = 1500
mac = "02:00:00:00:00:$mac"
queue_count = 4
CONFIG
done
taskset -c 0-3 "$daemon" run "$out/left.toml" > "$out/left.log" 2>&1 &
left=$!
taskset -c 4-7 "$daemon" run "$out/right.toml" > "$out/right.log" 2>&1 &
right=$!
for side in left right; do
    attempts=0
    until ip link show "uq-$side" >/dev/null 2>&1; do
        kill -0 "$left" && kill -0 "$right" || exit 1
        attempts=$((attempts + 1))
        [ "$attempts" -lt 500 ] || { echo "TAP readiness timeout" >&2; exit 1; }
        sleep 0.01
    done
    ip link set "uq-$side" netns "uq-$side"
    netip "uq-$side" link set lo up
    netip "uq-$side" link set "uq-$side" txqueuelen 10000
    netip "uq-$side" link set "uq-$side" up
done
netip uq-left addr add 10.77.0.1/24 dev uq-left
netip uq-right addr add 10.77.0.2/24 dev uq-right
netip uq-left neigh add 10.77.0.2 lladdr 02:00:00:00:00:02 nud permanent dev uq-left
netip uq-right neigh add 10.77.0.1 lladdr 02:00:00:00:00:01 nud permanent dev uq-right
for side in left right; do
    attempts=0
    until netip "uq-$side" -o link show "uq-$side" | grep -q LOWER_UP; do
        kill -0 "$left" && kill -0 "$right" || exit 1
        attempts=$((attempts + 1))
        [ "$attempts" -lt 500 ] || { echo "carrier timeout" >&2; exit 1; }
        sleep 0.01
    done
done
snapshot() {
    kill -USR1 "$left" "$right"
    sleep 0.1
    cp "$base/left.log" "$out/$1.left-log"
    cp "$base/right.log" "$out/$1.right-log"
    cat /proc/uptime > "$out/$1.uptime"
    cat /proc/stat > "$out/$1.cpu"
    cat /proc/"$left"/stat /proc/"$right"/stat > "$out/$1.process"
    cat /proc/"$left"/task/*/stat /proc/"$right"/task/*/stat > "$out/$1.threads"
    cat /proc/net/snmp6 > "$out/$1.outer"
    for side in left right; do
        nsenter --net="/run/netns/uq-$side" cat /proc/net/snmp > "$out/$1.$side-ipv4"
    done
    netip uq-left -s link show uq-left > "$out/$1.left-link"
    netip uq-right -s link show uq-right > "$out/$1.right-link"
}
uname -srvm > "$out/kernel"
sed -n '/model name/p' /proc/cpuinfo | head -n 1 > "$out/cpu-model"
getconf CLK_TCK > "$out/clock-ticks"
iperf3 --version > "$out/iperf-version"
printf '%s\n' "${GENERATOR_LEFT_CPUS:-8-11}" "${RECEIVER_LEFT_CPUS:-12-15}" \
    "${GENERATOR_RIGHT_CPUS:-16-19}" "${RECEIVER_RIGHT_CPUS:-20-23}" > "$out/generator-cpus"
base=$out
index=0
for rate in ${BITRATE:-0}; do
    case "$rate" in *[!0-9.KMG]*|'') echo "invalid BITRATE: $rate" >&2; exit 2;; esac
    case "${BITRATE:-0}" in
        *[[:space:]]*) out=$base/$index; mkdir "$out";;
    esac
    echo "$rate" > "$out/bitrate"
    nsenter --net=/run/netns/uq-right taskset -c "${RECEIVER_RIGHT_CPUS:-20-23}" iperf3 -s -1 -J > "$out/server.json" &
    server=$!
    nsenter --net=/run/netns/uq-left taskset -c "${RECEIVER_LEFT_CPUS:-12-15}" iperf3 -s -1 -J > "$out/reverse-server.json" &
    reverse_server=$!
    sleep 0.2
    snapshot before
    nsenter --net=/run/netns/uq-left taskset -c "${GENERATOR_LEFT_CPUS:-8-11}" iperf3 -c 10.77.0.2 -u -b "$rate" --dont-fragment \
        -l "$((frame - 42))" -P "$flows" --cport 51000 -t "$seconds" -J \
        --connect-timeout 3000 > "$out/client.json" &
    client=$!
    nsenter --net=/run/netns/uq-right taskset -c "${GENERATOR_RIGHT_CPUS:-16-19}" iperf3 -c 10.77.0.1 -u -b "$rate" --dont-fragment \
        -l "$((frame - 42))" -P "$flows" --cport 51000 -t "$seconds" -J \
        --connect-timeout 3000 > "$out/reverse-client.json" &
    reverse_client=$!
    wait "$client"
    client=
    wait "$reverse_client"
    reverse_client=
    wait "$server"
    server=
    wait "$reverse_server"
    reverse_server=
    snapshot after
    index=$((index + 1))
done
BENCH
