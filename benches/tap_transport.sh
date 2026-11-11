#!/bin/sh
# Run only inside a disposable container with tun, iproute2, taskset and a generator.
# Usage: sh benches/tap_transport.sh /absolute/unquic FRAME_BYTES FLOWS SECONDS OUT
# DAEMON_TOML, when set, starts both daemon configurations, e.g. a [daemon] table.
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
for number in "${OUTER_DELAY_US:-0}" "${OUTER_LOSS_PERMILLE:-0}" "${OUTER_LOSS_BURST:-1}" "${OUTER_REORDER_PERMILLE:-0}" "${OUTER_RATE_MBIT:-0}" "${OUTER_QUEUE_PACKETS:-8192}"; do
    case "$number" in ''|*[!0-9]*|0[0-9]*) exit 2;; esac
done
[ "${OUTER_QUEUE_PACKETS:-8192}" -ge 1 ] || exit 2
# A competing outer TCP flow needs the routed outer path it shares.
case "${COMPETING_TCP:-0}" in 0) ;; 1) [ "${OUTER_ROUTED:-0}" = 1 ] || exit 2;; *) exit 2;; esac
# Netem reorders by letting datagrams skip the delay, so reordering needs one.
[ "${OUTER_REORDER_PERMILLE:-0}" -le 1000 ] || exit 2
[ "${OUTER_REORDER_PERMILLE:-0}" -eq 0 ] || [ "${OUTER_DELAY_US:-0}" -gt 0 ] || exit 2
[ "${OUTER_DELAY_US:-0}" -le 1000000 ] && [ "${OUTER_LOSS_PERMILLE:-0}" -le 1000 ] || exit 2
# A mean loss burst of 1 is independent loss; longer bursts need some delivery.
[ "${OUTER_LOSS_BURST:-1}" -ge 1 ] && [ "${OUTER_LOSS_BURST:-1}" -le 100 ] || exit 2
[ "${OUTER_LOSS_BURST:-1}" -eq 1 ] || [ "${OUTER_LOSS_PERMILLE:-0}" -lt 1000 ] || exit 2
case "${OUTER_ROUTED:-0}" in 0|1) ;; *) exit 2;; esac
case "${INNER_BRIDGE:-0}" in 0|1) ;; *) exit 2;; esac
case "${VLAN_DEPTH:-0}" in 0|1|2) ;; *) echo "VLAN_DEPTH must be 0, 1 or 2" >&2; exit 2;; esac
case "${SRV6:-0}" in 0|1) ;; *) echo "SRV6 must be 0 or 1" >&2; exit 2;; esac
[ "${SRV6:-0}" = 0 ] || [ -n "${UDP_LOAD:-}" ] || { echo "SRV6 requires UDP_LOAD" >&2; exit 2; }
[ "$2" -le "$((1514 + 4 * ${VLAN_DEPTH:-0}))" ] || exit 2
case "${WORKERS:-4}" in 2|4|8) ;; *) echo "WORKERS must be 2, 4 or 8" >&2; exit 2;; esac
case "${UDP_LOAD:-}" in ''|/*) ;; *) echo "UDP_LOAD must be an absolute path" >&2; exit 2;; esac
case "${IPERF_PROTOCOL:-udp}" in udp|tcp) ;; *) echo "IPERF_PROTOCOL must be udp or tcp" >&2; exit 2;; esac
[ "${IPERF_PROTOCOL:-udp}" = udp ] || [ -z "${UDP_LOAD:-}" ] || { echo "TCP requires iperf3, not UDP_LOAD" >&2; exit 2; }
if [ -n "${UDP_LOAD:-}" ]; then
    [ "$2" -ge "$((58 + 4 * ${VLAN_DEPTH:-0} + 84 * ${SRV6:-0}))" ] || { echo "UDP_LOAD needs 16 payload bytes after headers" >&2; exit 2; }
fi
case "${GENERATOR_PROCESSES:-1}" in 1|2|4|8) ;; *) echo "GENERATOR_PROCESSES must be 1, 2, 4 or 8" >&2; exit 2;; esac
[ "${GENERATOR_PROCESSES:-1}" = 1 ] || [ -n "${UDP_LOAD:-}" ] || { echo "GENERATOR_PROCESSES requires UDP_LOAD" >&2; exit 2; }
[ "$(($3 % ${GENERATOR_PROCESSES:-1}))" = 0 ] || { echo "FLOWS must be a multiple of GENERATOR_PROCESSES" >&2; exit 2; }
mkdir "$5"
# A private mount namespace keeps ip-netns bind mounts out of the caller.
exec unshare -m -n --propagation private sh -s -- "$@" <<'BENCH'
set -eu
daemon=$1
frame=$2
payload=$((frame - 42 - 4 * ${VLAN_DEPTH:-0} - 84 * ${SRV6:-0}))
left_address=10.77.0.1
right_address=10.77.0.2
if [ "${SRV6:-0}" = 1 ]; then
    left_address=fd77:1::1
    right_address=fd77:1::2
fi
flows=$3
seconds=$4
out=$(cd "$5" && pwd)
workers=${WORKERS:-4}
case "$workers" in
    2)
        GENERATOR_LEFT_CPUS=${GENERATOR_LEFT_CPUS:-4}
        RECEIVER_LEFT_CPUS=${RECEIVER_LEFT_CPUS:-5}
        GENERATOR_RIGHT_CPUS=${GENERATOR_RIGHT_CPUS:-6}
        RECEIVER_RIGHT_CPUS=${RECEIVER_RIGHT_CPUS:-7}
        ;;
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
servers=
clients=
competing=
processes=${GENERATOR_PROCESSES:-1}
cleanup() {
    for pid in $clients $servers $competing "$left" "$right"; do
        [ -z "$pid" ] || kill "$pid" 2>/dev/null || :
    done
    wait || :
    ip netns del uq-left 2>/dev/null || :
    ip netns del uq-right 2>/dev/null || :
    ip netns del uq-gen-left 2>/dev/null || :
    ip netns del uq-gen-right 2>/dev/null || :
    ip netns del uq-out-left 2>/dev/null || :
    ip netns del uq-out-right 2>/dev/null || :
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM
# One CPU per load process for a single A-B range; other CPU lists are shared.
process_cpu() {
    case "$1" in
        *[!0-9-]*|*-*-*|-*|*-) printf '%s\n' "$1";;
        *-*)
            first=${1%-*}
            last=${1#*-}
            printf '%s\n' "$((first + $2 % (last - first + 1)))"
            ;;
        *) printf '%s\n' "$1";;
    esac
}
netip() {
    namespace=$1
    shift
    nsenter --net="/run/netns/$namespace" ip "$@"
}
left_inner=/run/netns/uq-left
right_inner=/run/netns/uq-right
inner() {
    case "$1" in left) namespace=$left_inner;; right) namespace=$right_inner;; esac
    shift
    nsenter --net="$namespace" "$@"
}
left_netns=/proc/self/ns/net
right_netns=/proc/self/ns/net
outer() {
    case "$1" in left) namespace=$left_netns;; right) namespace=$right_netns;; esac
    shift
    nsenter --net="$namespace" "$@"
}
percent4() {
    printf '%s.%04d%%' "$(($1 / 10000))" "$(($1 % 10000))"
}
netem() {
    if [ "${OUTER_DELAY_US:-0}" -ne 0 ] || [ "${OUTER_LOSS_PERMILLE:-0}" -ne 0 ]; then
        loss=${OUTER_LOSS_PERMILLE:-0}
        burst=${OUTER_LOSS_BURST:-1}
        reorder=${OUTER_REORDER_PERMILLE:-0}
        if [ "$burst" -le 1 ] || [ "$loss" -eq 0 ]; then
            set -- "$1" loss "$((loss / 10)).$((loss % 10))%"
        else
            # Gilbert model: the bad state loses every packet and lasts $burst
            # packets on average; entry probability keeps the mean loss rate.
            set -- "$1" loss gemodel \
                "$(percent4 $((1000000 * loss / (burst * (1000 - loss)))))" \
                "$(percent4 $((1000000 / burst)))" 100% 0%
        fi
        dev=$1
        shift
        # Reordered datagrams skip the delay, so they overtake earlier ones.
        [ "$reorder" -eq 0 ] || set -- "$@" reorder "$((reorder / 10)).$((reorder % 10))%" 50%
        tc qdisc add dev "$dev" root netem limit 8192 delay "${OUTER_DELAY_US:-0}us" "$@"
    fi
}
ip link set lo up
if [ "${OUTER_ROUTED:-0}" = 1 ]; then
    for side in left right; do
        if [ "$side" = left ]; then n=1; peer=2; else n=2; peer=1; fi
        ip netns add "uq-out-$side"
        ip link add "uq-r-$side" type veth peer name uq-path netns "uq-out-$side"
        ip link set "uq-r-$side" address "02:77:ff:00:0$n:02" mtu 1500 up
        ip -6 addr add "fd77:ff:$n::2/64" dev "uq-r-$side" nodad
        netip "uq-out-$side" link set lo up
        netip "uq-out-$side" link set uq-path address "02:77:ff:00:0$n:01" mtu 1500 up
        netip "uq-out-$side" -6 addr add "fd77:ff:$n::1/64" dev uq-path nodad
        netip "uq-out-$side" -6 route add "fd77:ff:$peer::/64" via "fd77:ff:$n::2"
        netip "uq-out-$side" -6 neigh add "fd77:ff:$n::2" lladdr "02:77:ff:00:0$n:02" nud permanent dev uq-path
        ip -6 neigh add "fd77:ff:$n::1" lladdr "02:77:ff:00:0$n:01" nud permanent dev "uq-r-$side"
        # Segment before router netem so loss applies to individual datagrams.
        nsenter --net="/run/netns/uq-out-$side" ethtool -K uq-path gso off tso off tx-udp-segmentation off
        # Use one endpoint NAPI queue; per-CPU backlogs reorder delayed traffic.
        ethtool -K "uq-r-$side" gro off tso off
        nsenter --net="/run/netns/uq-out-$side" ethtool -K uq-path gro on
        ethtool -k "uq-r-$side" > "$out/$side.router-offloads"
        grep -q '^tcp-segmentation-offload: off' "$out/$side.router-offloads"
        if [ "${OUTER_RATE_MBIT:-0}" -ne 0 ]; then
            # The bottleneck sits before the router's delay line: netem's limit
            # would also count delayed packets, so a token bucket holds the queue.
            nsenter --net="/run/netns/uq-out-$side" tc qdisc add dev uq-path root tbf \
                rate "${OUTER_RATE_MBIT}mbit" burst 32kb limit "$((${OUTER_QUEUE_PACKETS:-8192} * 1500))"
        fi
        nsenter --net="/run/netns/uq-out-$side" ethtool -k uq-path > "$out/$side.outer-offloads"
        grep -q '^generic-receive-offload: on' "$out/$side.outer-offloads"
        grep -q '^generic-segmentation-offload: off' "$out/$side.outer-offloads"
        grep -q '^tx-udp-segmentation: off' "$out/$side.outer-offloads"
        netem "uq-r-$side"
    done
    sysctl -qw net.ipv6.conf.all.forwarding=1
    left_netns=/run/netns/uq-out-left
    right_netns=/run/netns/uq-out-right
else
    netem lo
fi
ip netns add uq-left
ip netns add uq-right
for side in left right; do
    if [ "$side" = left ]; then port=40100; remote=40101; mac=01; else port=40101; remote=40100; mac=02; fi
    listen=::1
    host=::1
    if [ "${OUTER_ROUTED:-0}" = 1 ]; then
        if [ "$side" = left ]; then listen=fd77:ff:1::1; host=fd77:ff:2::1; else listen=fd77:ff:2::1; host=fd77:ff:1::1; fi
    fi
    mac_prefix=00
    [ "${INNER_BRIDGE:-0}" = 0 ] || mac_prefix=01
    cat > "$out/$side.toml" <<CONFIG
${DAEMON_TOML:-}
[[endpoints]]
name = "rx"
listen_address = "$listen"
listen_port = $port
[[peers]]
name = "peer"
endpoint = "rx"
host = "$host"
peer_port = $remote
tx_lanes = $workers
outer_payload_limit = ${OUTER_PAYLOAD_LIMIT:-1232}
[[sessions]]
name = "uq-$side"
peer = "peer"
cookie = "${SESSION_COOKIE:-00112233-4455-4677-8899-aabbccddeeff}"
inner_mtu = 1500
mac = "02:00:00:00:$mac_prefix:$mac"
queue_count = $workers
CONFIG
done
nsenter --net="$left_netns" taskset -c "${DAEMON_LEFT_CPUS:-0-$((workers - 1))}" "$daemon" run "$out/left.toml" > "$out/left.log" 2>&1 &
left=$!
nsenter --net="$right_netns" taskset -c "${DAEMON_RIGHT_CPUS:-$workers-$((workers * 2 - 1))}" "$daemon" run "$out/right.toml" > "$out/right.log" 2>&1 &
right=$!
for side in left right; do
    attempts=0
    until outer "$side" ip link show "uq-$side" >/dev/null 2>&1; do
        kill -0 "$left" && kill -0 "$right" || exit 1
        attempts=$((attempts + 1))
        [ "$attempts" -lt 500 ] || { echo "TAP readiness timeout" >&2; exit 1; }
        sleep 0.01
    done
    outer "$side" ip link set "uq-$side" netns "uq-$side"
    netip "uq-$side" link set lo up
    netip "uq-$side" link set "uq-$side" txqueuelen "${TAP_TXQUEUELEN:-10000}"
    netip "uq-$side" link set "uq-$side" up
    data="uq-$side"
    data_ns="uq-$side"
    if [ "${INNER_BRIDGE:-0}" = 1 ]; then
        if [ "$side" = left ]; then n=1; else n=2; fi
        data_ns="uq-gen-$side"
        ip netns add "$data_ns"
        netip "$data_ns" link set lo up
        netip "uq-$side" link add uq-port type veth peer name uq-input netns "$data_ns"
        netip "uq-$side" link add uq-bridge address "02:00:00:00:02:0$n" type bridge
        netip "uq-$side" link set uq-port address "02:00:00:00:03:0$n" master uq-bridge up
        netip "uq-$side" link set "uq-$side" master uq-bridge
        netip "uq-$side" link set uq-bridge up
        netip "$data_ns" link set uq-input address "02:00:00:00:00:0$n" up
        if [ -n "${UDP_LOAD:-}" ] && [ "${SEGMENTS:-64}" = 1 ]; then
            nsenter --net="/run/netns/$data_ns" ethtool -K uq-input tx off gso off tso off tx-udp-segmentation off
            nsenter --net="/run/netns/uq-$side" ethtool -K uq-port gro off
        fi
        nsenter --net="/run/netns/$data_ns" ethtool -k uq-input > "$out/$side.inner-offloads"
        if [ -n "${UDP_LOAD:-}" ] && [ "${SEGMENTS:-64}" = 1 ]; then
            grep -q '^tx-checksumming: off' "$out/$side.inner-offloads"
        fi
        data=uq-input
        if [ "$side" = left ]; then left_inner="/run/netns/$data_ns"; else right_inner="/run/netns/$data_ns"; fi
    fi
    if [ "${VLAN_DEPTH:-0}" = 2 ]; then
        netip "$data_ns" link add link "$data" name uq-outer type vlan protocol 802.1ad id 42
        netip "$data_ns" link set uq-outer up
        data=uq-outer
    fi
    if [ "${VLAN_DEPTH:-0}" != 0 ]; then
        netip "$data_ns" link add link "$data" name uq-data type vlan id 7
        netip "$data_ns" link set uq-data mtu 1500 up
        data=uq-data
    fi
    if [ "$side" = left ]; then local=1; remote=2; else local=2; remote=1; fi
    netip "$data_ns" addr add "10.77.0.$local/24" dev "$data"
    netip "$data_ns" neigh add "10.77.0.$remote" lladdr "02:00:00:00:00:0$remote" nud permanent dev "$data"
    if [ "${SRV6:-0}" = 1 ]; then
        netip "$data_ns" -6 addr add "fd77:3::$local/64" dev "$data" nodad
        netip "$data_ns" -6 addr add "fd77:1::$local/128" dev lo nodad
        netip "$data_ns" -6 neigh add "fd77:3::$remote" lladdr "02:00:00:00:00:0$remote" nud permanent dev "$data"
        nsenter --net="/run/netns/$data_ns" sysctl -qw net.ipv6.conf.all.seg6_enabled=1 net.ipv6.conf.all.forwarding=1
        netip "$data_ns" -6 route add "fd77:2::$local/128" encap seg6local action End.DT6 table 255 count dev "$data"
        netip "$data_ns" -6 route add "fd77:2::$remote/128" via "fd77:3::$remote" dev "$data"
        netip "$data_ns" -6 route add "fd77:1::$remote/128" encap seg6 mode encap segs "fd77:2::$remote" via "fd77:3::$remote" dev "$data"
    fi
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
    left_count=$(grep -c '^unquic: worker [0-9][0-9]* Snapshot {.* }$' "$base/left.log" || :)
    right_count=$(grep -c '^unquic: worker [0-9][0-9]* Snapshot {.* }$' "$base/right.log" || :)
    left_count=$((left_count + workers))
    right_count=$((right_count + workers))
    kill -USR1 "$left" "$right"
    if [ -n "${UDP_LOAD:-}" ] && [ "$1" != after ]; then
        kill -USR1 $servers
    fi
    sleep 0.1
    attempts=0
    while [ "$(grep -c '^unquic: worker [0-9][0-9]* Snapshot {.* }$' "$base/left.log" || :)" -lt "$left_count" ] ||
          [ "$(grep -c '^unquic: worker [0-9][0-9]* Snapshot {.* }$' "$base/right.log" || :)" -lt "$right_count" ]; do
        kill -0 "$left" && kill -0 "$right" || exit 1
        attempts=$((attempts + 1))
        [ "$attempts" -lt 500 ] || { echo "counter snapshot timeout" >&2; exit 1; }
        sleep 0.01
    done
    cat "$base/left.log" > "$out/$1.left-log"
    cat "$base/right.log" > "$out/$1.right-log"
    cat /proc/uptime > "$out/$1.uptime"
    cat /proc/stat > "$out/$1.cpu"
    cat /proc/"$left"/stat /proc/"$right"/stat > "$out/$1.process"
    # Peak and current resident memory, left then right, to check bounded storage.
    sed -n '/^VmHWM:/p; /^VmRSS:/p' /proc/"$left"/status /proc/"$right"/status > "$out/$1.memory"
    cat /proc/"$left"/task/*/stat /proc/"$right"/task/*/stat > "$out/$1.threads"
    for process in "$left" "$right"; do
        for status in /proc/"$process"/task/*/status; do
            printf '%s\n' "$status"
            sed -n '/^Name:/p; /^Pid:/p; /^voluntary_ctxt_switches:/p; /^nonvoluntary_ctxt_switches:/p' "$status"
        done
    done > "$out/$1.switches"
    cat /proc/net/snmp6 > "$out/$1.outer"
    if [ "${OUTER_ROUTED:-0}" = 1 ]; then
        outer left cat /proc/net/snmp6 > "$out/$1.outer-left"
        outer right cat /proc/net/snmp6 > "$out/$1.outer-right"
        tc -s -d qdisc show dev uq-r-left > "$out/$1.qdisc-left"
        tc -s -d qdisc show dev uq-r-right > "$out/$1.qdisc-right"
        for side in left right; do
            # The left outer namespace's uq-path egress is the L→R bottleneck.
            outer "$side" tc -s -d qdisc show dev uq-path > "$out/$1.bottleneck-$side"
            outer "$side" ethtool -S uq-path > "$out/$1.$side-veth"
            outer "$side" ip -s link show uq-path > "$out/$1.$side-outer-link"
        done
    elif [ "${OUTER_DELAY_US:-0}" -ne 0 ] || [ "${OUTER_LOSS_PERMILLE:-0}" -ne 0 ]; then
        tc -s -d qdisc show dev lo > "$out/$1.qdisc"
    fi
    for side in left right; do
        inner "$side" cat /proc/net/snmp > "$out/$1.$side-ipv4"
        inner "$side" cat /proc/net/snmp6 > "$out/$1.$side-ipv6"
        inner "$side" ip -s -6 route show > "$out/$1.$side-routes"
    done
    netip uq-left -s link show uq-left > "$out/$1.left-link"
    netip uq-right -s link show uq-right > "$out/$1.right-link"
}
uname -srvm > "$out/kernel"
sed -n '/model name/p' /proc/cpuinfo | head -n 1 > "$out/cpu-model"
getconf CLK_TCK > "$out/clock-ticks"
if [ -n "${UDP_LOAD:-}" ]; then
    sha256sum "$UDP_LOAD" > "$out/generator-sha256"
    printf '%s\n' "udp_load segments=${SEGMENTS:-64} processes=$processes" > "$out/generator"
else
    iperf3 --version > "$out/iperf-version"
    printf 'iperf3 protocol=%s\n' "${IPERF_PROTOCOL:-udp}" > "$out/generator"
fi
printf '%s\n' "${DAEMON_LEFT_CPUS:-0-$((workers - 1))}" "${DAEMON_RIGHT_CPUS:-$workers-$((workers * 2 - 1))}" > "$out/daemon-cpus"
printf '%s\n' "$GENERATOR_LEFT_CPUS" "$RECEIVER_LEFT_CPUS" \
    "$GENERATOR_RIGHT_CPUS" "$RECEIVER_RIGHT_CPUS" > "$out/generator-cpus"
printf '%s\n' "${VLAN_DEPTH:-0}" > "$out/vlan-depth"
printf '%s\n' "${SRV6:-0}" > "$out/srv6"
printf '%s\n' "${INNER_BRIDGE:-0}" > "$out/inner-bridge"
printf '%s\n' "$frame" > "$out/frame-bytes"
printf 'routed=%s\none_way_delay_us=%s\nloss_permille=%s\nloss_burst=%s\nreorder_permille=%s\nrate_mbit=%s\nqueue_packets=%s\ncompeting_tcp=%s\n' "${OUTER_ROUTED:-0}" "${OUTER_DELAY_US:-0}" "${OUTER_LOSS_PERMILLE:-0}" "${OUTER_LOSS_BURST:-1}" "${OUTER_REORDER_PERMILLE:-0}" "${OUTER_RATE_MBIT:-0}" "${OUTER_QUEUE_PACKETS:-8192}" "${COMPETING_TCP:-0}" > "$out/outer-path"
printf '%s\n' "${DAEMON_TOML:-}" > "$out/daemon-toml"
base=$out
index=0
for rate in ${BITRATE:-0}; do
    case "$rate" in *[!0-9.KMG]*|'') echo "invalid BITRATE: $rate" >&2; exit 2;; esac
    case "${BITRATE:-0}" in
        *[[:space:]]*)
            out=$base/$index
            mkdir "$out"
            cp "$base/clock-ticks" "$base/kernel" "$base/cpu-model" "$out/"
            ;;
    esac
    echo "$rate" > "$out/bitrate"
    if [ -n "${UDP_LOAD:-}" ]; then
        case "$rate" in *[!0-9]*) echo "UDP_LOAD rates must be integer bits/s per flow" >&2; exit 2;; esac
        run=$((index + 1))
        share=$((flows / processes))
        # tun hashes ports symmetrically, so equal port pairs make flow j share a hash both ways.
        reverse=${REVERSE_PORT_OFFSET:-0}
        k=0
        while [ "$k" -lt "$processes" ]; do
            # Process k carries flows k*share.. on its own port range; process 0 keeps the original file names.
            suffix=
            [ "$k" = 0 ] || suffix=-$k
            offset=$((k * share))
            nsenter --net="$right_inner" taskset -c "$(process_cpu "$RECEIVER_RIGHT_CPUS" "$k")" "$UDP_LOAD" recv \
                "$right_address" "$left_address" "$((52000 + offset))" "$((51000 + offset))" "$share" "$payload" "$((seconds + 2))" 0 "$run" "${SEGMENTS:-64}" > "$out/server$suffix.json" &
            servers="$servers $!"
            nsenter --net="$left_inner" taskset -c "$(process_cpu "$RECEIVER_LEFT_CPUS" "$k")" "$UDP_LOAD" recv \
                "$left_address" "$right_address" "$((52000 + reverse + offset))" "$((51000 + reverse + offset))" "$share" "$payload" "$((seconds + 2))" 0 "$run" "${SEGMENTS:-64}" > "$out/reverse-server$suffix.json" &
            servers="$servers $!"
            k=$((k + 1))
        done
        if [ "${COMPETING_TCP:-0}" = 1 ]; then
            # One outer TCP flow left to right shares the routed bottleneck with the tunnel;
            # the receiver counts the bytes that arrive.
            # Kept out of $servers, which snapshots signal with SIGUSR1.
            nsenter --net="/run/netns/uq-out-right" taskset -c "$RECEIVER_RIGHT_CPUS" sh -c \
                'nc -6 -l -s fd77:ff:2::1 -p 5201 | wc -c' > "$out/competing-bytes" &
            competing=$!
        fi
        sleep 0.2
        for pid in $servers; do kill -0 "$pid" || exit 1; done
        snapshot before
        if [ "${COMPETING_TCP:-0}" = 1 ]; then
            nsenter --net="/run/netns/uq-out-left" taskset -c "$GENERATOR_LEFT_CPUS" sh -c \
                "timeout $seconds sh -c 'nc -6 -N fd77:ff:2::1 5201 < /dev/zero'; true" &
            clients="$clients $!"
        fi
        k=0
        while [ "$k" -lt "$processes" ]; do
            suffix=
            [ "$k" = 0 ] || suffix=-$k
            offset=$((k * share))
            nsenter --net="$left_inner" taskset -c "$(process_cpu "$GENERATOR_LEFT_CPUS" "$k")" "$UDP_LOAD" send \
                "$left_address" "$right_address" "$((51000 + offset))" "$((52000 + offset))" "$share" "$payload" "$seconds" "$rate" "$run" "${SEGMENTS:-64}" > "$out/client$suffix.json" &
            clients="$clients $!"
            nsenter --net="$right_inner" taskset -c "$(process_cpu "$GENERATOR_RIGHT_CPUS" "$k")" "$UDP_LOAD" send \
                "$right_address" "$left_address" "$((51000 + reverse + offset))" "$((52000 + reverse + offset))" "$share" "$payload" "$seconds" "$rate" "$run" "${SEGMENTS:-64}" > "$out/reverse-client$suffix.json" &
            clients="$clients $!"
            k=$((k + 1))
        done
    else
        nsenter --net="$right_inner" taskset -c "$RECEIVER_RIGHT_CPUS" iperf3 -s -1 -J > "$out/server.json" &
        servers="$servers $!"
        nsenter --net="$left_inner" taskset -c "$RECEIVER_LEFT_CPUS" iperf3 -s -1 -J > "$out/reverse-server.json" &
        servers="$servers $!"
        sleep 0.2
        snapshot before
        if [ "${IPERF_PROTOCOL:-udp}" = tcp ]; then
            set -- -l 65536
        else
            set -- -u --dont-fragment -l "$payload"
        fi
        nsenter --net="$left_inner" taskset -c "$GENERATOR_LEFT_CPUS" iperf3 -c 10.77.0.2 -b "$rate" "$@" -P "$flows" --cport 51000 -t "$seconds" -J \
            --connect-timeout 3000 > "$out/client.json" &
        clients="$clients $!"
        nsenter --net="$right_inner" taskset -c "$GENERATOR_RIGHT_CPUS" iperf3 -c 10.77.0.1 -b "$rate" "$@" -P "$flows" --cport 51000 -t "$seconds" -J \
            --connect-timeout 3000 > "$out/reverse-client.json" &
        clients="$clients $!"
    fi
    for pid in $clients; do wait "$pid"; done
    clients=
    [ -z "$competing" ] || wait "$competing"
    competing=
    if [ -n "${UDP_LOAD:-}" ]; then snapshot sent; fi
    for pid in $servers; do wait "$pid"; done
    servers=
    snapshot after
    index=$((index + 1))
done
BENCH
