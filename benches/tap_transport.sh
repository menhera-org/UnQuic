#!/bin/sh
# Run only inside a disposable container with tun, iproute2, taskset and a generator.
# Usage: sh benches/tap_transport.sh /absolute/unquic FRAME_BYTES FLOWS SECONDS OUT
set -eu
[ "$#" -eq 5 ] || { echo "usage: $0 DAEMON FRAME_BYTES FLOWS SECONDS OUT" >&2; exit 2; }
case "$1" in /*) ;; *) echo "DAEMON must be an absolute path" >&2; exit 2;; esac
case "$2" in 60|64|128|1514|1518|1522) ;; *) echo "frame size must be 60, 64, 128, 1514, 1518 or 1522" >&2; exit 2;; esac
for number in "$3" "$4"; do
    case "$number" in ''|*[!0-9]*) exit 2;; esac
    [ "$number" -ge 1 ] && [ "$number" -le 60 ] || exit 2
done
case "${OUTER_PAYLOAD_LIMIT:-1232}" in ''|*[!0-9]*) exit 2;; esac
[ "${OUTER_PAYLOAD_LIMIT:-1232}" -ge 1232 ] && [ "${OUTER_PAYLOAD_LIMIT:-1232}" -le 1452 ] || exit 2
case "${TAP_TXQUEUELEN:-10000}" in ''|*[!0-9]*) exit 2;; esac
[ "${TAP_TXQUEUELEN:-10000}" -ge 1 ] && [ "${TAP_TXQUEUELEN:-10000}" -le 10000 ] || exit 2
case "${VLAN_DEPTH:-0}" in 0|1|2) ;; *) echo "VLAN_DEPTH must be 0, 1 or 2" >&2; exit 2;; esac
[ "$2" -le "$((1514 + 4 * ${VLAN_DEPTH:-0}))" ] || exit 2
case "${WORKERS:-4}" in 4|8) ;; *) echo "WORKERS must be 4 or 8" >&2; exit 2;; esac
case "${UDP_LOAD:-}" in ''|/*) ;; *) echo "UDP_LOAD must be an absolute path" >&2; exit 2;; esac
if [ -n "${UDP_LOAD:-}" ]; then
    [ "$2" -ge "$((58 + 4 * ${VLAN_DEPTH:-0}))" ] || { echo "UDP_LOAD needs 16 payload bytes after tags" >&2; exit 2; }
fi
case "${BITRATE:-0}" in
    *[[:space:]]*)
        mkdir "$5"
        index=0
        for rate in ${BITRATE:-0}; do
            BITRATE=$rate sh "$0" "$1" "$2" "$3" "$4" "$5/$index"
            index=$((index + 1))
        done
        exit 0
        ;;
esac
mkdir "$5"
# A private mount namespace keeps ip-netns bind mounts out of the caller.
exec unshare -m -n --propagation private sh -s -- "$@" <<'BENCH'
set -eu
daemon=$1
frame=$2
payload=$((frame - 42 - 4 * ${VLAN_DEPTH:-0}))
flows=$3
seconds=$4
out=$(cd "$5" && pwd)
workers=${WORKERS:-4}
case "$workers" in
    4)
        GENERATOR_LEFT_CPUS=${GENERATOR_LEFT_CPUS:-8-11}
        RECEIVER_LEFT_CPUS=${RECEIVER_LEFT_CPUS:-12-13}
        GENERATOR_RIGHT_CPUS=${GENERATOR_RIGHT_CPUS:-14-17}
        RECEIVER_RIGHT_CPUS=${RECEIVER_RIGHT_CPUS:-18-19}
        ;;
    8)
        GENERATOR_LEFT_CPUS=${GENERATOR_LEFT_CPUS:-16}
        RECEIVER_LEFT_CPUS=${RECEIVER_LEFT_CPUS:-17}
        GENERATOR_RIGHT_CPUS=${GENERATOR_RIGHT_CPUS:-18}
        RECEIVER_RIGHT_CPUS=${RECEIVER_RIGHT_CPUS:-19}
        ;;
esac
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
tx_lanes = $workers
outer_payload_limit = ${OUTER_PAYLOAD_LIMIT:-1232}
[[sessions]]
name = "uq-$side"
peer = "peer"
cookie = "00112233-4455-4677-8899-aabbccddeeff"
inner_mtu = 1500
mac = "02:00:00:00:00:$mac"
queue_count = $workers
CONFIG
done
taskset -c "${DAEMON_LEFT_CPUS:-0-$((workers - 1))}" "$daemon" run "$out/left.toml" > "$out/left.log" 2>&1 &
left=$!
taskset -c "${DAEMON_RIGHT_CPUS:-$workers-$((workers * 2 - 1))}" "$daemon" run "$out/right.toml" > "$out/right.log" 2>&1 &
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
    netip "uq-$side" link set "uq-$side" txqueuelen "${TAP_TXQUEUELEN:-10000}"
    netip "uq-$side" link set "uq-$side" up
    data="uq-$side"
    if [ "${VLAN_DEPTH:-0}" = 2 ]; then
        netip "uq-$side" link add link "$data" name uq-outer type vlan protocol 802.1ad id 42
        netip "uq-$side" link set uq-outer up
        data=uq-outer
    fi
    if [ "${VLAN_DEPTH:-0}" != 0 ]; then
        netip "uq-$side" link add link "$data" name uq-data type vlan id 7
        netip "uq-$side" link set uq-data mtu 1500 up
        data=uq-data
    fi
    if [ "$side" = left ]; then local=1; remote=2; else local=2; remote=1; fi
    netip "uq-$side" addr add "10.77.0.$local/24" dev "$data"
    netip "uq-$side" neigh add "10.77.0.$remote" lladdr "02:00:00:00:00:0$remote" nud permanent dev "$data"
done
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
    if [ -n "${UDP_LOAD:-}" ] && [ "$1" != after ]; then
        kill -USR1 "$server" "$reverse_server"
    fi
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
if [ -n "${UDP_LOAD:-}" ]; then
    sha256sum "$UDP_LOAD" > "$out/generator-sha256"
    printf '%s\n' "udp_load segments=${SEGMENTS:-64}" > "$out/generator"
else
    iperf3 --version > "$out/iperf-version"
fi
printf '%s\n' "${DAEMON_LEFT_CPUS:-0-$((workers - 1))}" "${DAEMON_RIGHT_CPUS:-$workers-$((workers * 2 - 1))}" > "$out/daemon-cpus"
printf '%s\n' "$GENERATOR_LEFT_CPUS" "$RECEIVER_LEFT_CPUS" \
    "$GENERATOR_RIGHT_CPUS" "$RECEIVER_RIGHT_CPUS" > "$out/generator-cpus"
printf '%s\n' "${VLAN_DEPTH:-0}" > "$out/vlan-depth"
printf '%s\n' "$frame" > "$out/frame-bytes"
base=$out
index=0
for rate in ${BITRATE:-0}; do
    case "$rate" in *[!0-9.KMG]*|'') echo "invalid BITRATE: $rate" >&2; exit 2;; esac
    case "${BITRATE:-0}" in
        *[[:space:]]*) out=$base/$index; mkdir "$out";;
    esac
    echo "$rate" > "$out/bitrate"
    if [ -n "${UDP_LOAD:-}" ]; then
        case "$rate" in *[!0-9]*) echo "UDP_LOAD rates must be integer bits/s per flow" >&2; exit 2;; esac
        run=$((index + 1))
        nsenter --net=/run/netns/uq-right taskset -c "$RECEIVER_RIGHT_CPUS" "$UDP_LOAD" recv \
            10.77.0.2 10.77.0.1 52000 51000 "$flows" "$payload" "$((seconds + 2))" 0 "$run" "${SEGMENTS:-64}" > "$out/server.json" &
        server=$!
        nsenter --net=/run/netns/uq-left taskset -c "$RECEIVER_LEFT_CPUS" "$UDP_LOAD" recv \
            10.77.0.1 10.77.0.2 52000 51000 "$flows" "$payload" "$((seconds + 2))" 0 "$run" "${SEGMENTS:-64}" > "$out/reverse-server.json" &
        reverse_server=$!
        sleep 0.2
        kill -0 "$server" && kill -0 "$reverse_server" || exit 1
        snapshot before
        nsenter --net=/run/netns/uq-left taskset -c "$GENERATOR_LEFT_CPUS" "$UDP_LOAD" send \
            10.77.0.1 10.77.0.2 51000 52000 "$flows" "$payload" "$seconds" "$rate" "$run" "${SEGMENTS:-64}" > "$out/client.json" &
        client=$!
        nsenter --net=/run/netns/uq-right taskset -c "$GENERATOR_RIGHT_CPUS" "$UDP_LOAD" send \
            10.77.0.2 10.77.0.1 51000 52000 "$flows" "$payload" "$seconds" "$rate" "$run" "${SEGMENTS:-64}" > "$out/reverse-client.json" &
        reverse_client=$!
    else
        nsenter --net=/run/netns/uq-right taskset -c "$RECEIVER_RIGHT_CPUS" iperf3 -s -1 -J > "$out/server.json" &
        server=$!
        nsenter --net=/run/netns/uq-left taskset -c "$RECEIVER_LEFT_CPUS" iperf3 -s -1 -J > "$out/reverse-server.json" &
        reverse_server=$!
        sleep 0.2
        snapshot before
        nsenter --net=/run/netns/uq-left taskset -c "$GENERATOR_LEFT_CPUS" iperf3 -c 10.77.0.2 -u -b "$rate" --dont-fragment \
            -l "$payload" -P "$flows" --cport 51000 -t "$seconds" -J \
            --connect-timeout 3000 > "$out/client.json" &
        client=$!
        nsenter --net=/run/netns/uq-right taskset -c "$GENERATOR_RIGHT_CPUS" iperf3 -c 10.77.0.1 -u -b "$rate" --dont-fragment \
            -l "$payload" -P "$flows" --cport 51000 -t "$seconds" -J \
            --connect-timeout 3000 > "$out/reverse-client.json" &
        reverse_client=$!
    fi
    wait "$client"
    client=
    wait "$reverse_client"
    reverse_client=
    if [ -n "${UDP_LOAD:-}" ]; then snapshot sent; fi
    wait "$server"
    server=
    wait "$reverse_server"
    reverse_server=
    snapshot after
    index=$((index + 1))
done
BENCH
