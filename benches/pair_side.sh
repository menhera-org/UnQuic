#!/bin/sh
# One side of a two-host run of the daemon over a real path; see benches/udp_load.md.
# Run as root on each host, in a private mount namespace and the host's own
# network namespace, with the same START and settings on both:
#   unshare -m --propagation private sh pair_side.sh SIDE DAEMON UDP_LOAD OUT LOCAL6 PEER6 START
# SIDE is left or right. START is the epoch second at which both sides start
# the first point's senders; each later point starts DURATION + 10 s after the
# one before it. The hosts' clocks must agree to a fraction of a second.
# The daemon's sockets use the host's network as they are. Its TAP moves into a
# private namespace that dies with this script, and nothing else is changed
# unless OUTER_UDP_GRO=1 asks for it. OUT keeps addresses and device names:
# do not publish it as it is.
# Environment: WORKERS FLOWS DURATION BITRATE (bits/s per flow, one per point)
# SEGMENTS VLAN_DEPTH GENERATOR_PROCESSES DAEMON_CPUS GENERATOR_CPUS
# RECEIVER_CPUS (comma-separated lists) DAEMON_TOML OUTER_PAYLOAD_LIMIT PORT
# TAP_TXQUEUELEN SESSION_COOKIE OUTER_UDP_GRO BITRATE_LEFT BITRATE_RIGHT TX_LANES.
# KERNEL_TUNNEL=l2tp is a baseline: a static kernel L2TPv3 Ethernet pseudowire
# between the two addresses takes the daemon's place, over IPv6 itself or,
# with L2TP_ENCAP=udp, over UDP on PORT. Its packets exceed the path's MTU and
# the kernel fragments them. DAEMON is then unused and sweep_report cannot read
# the result; the load tool's reports and the CPU snapshots can.
# DECISION(2026-11-26): the pseudowire's device and its test addresses stay in the host's
#   namespace. Why: an L2TP device is not deleted with a namespace it was moved into, it
#   falls back to the machine's first namespace. Rejected: the private namespace the TAP gets.
set -eu
[ "$#" -eq 7 ] || { echo "usage: $0 SIDE DAEMON UDP_LOAD OUT LOCAL6 PEER6 START" >&2; exit 2; }
side=$1 daemon=$2 load=$3 out=$4 local6=$5 peer6=$6 start=$7
case "$side" in left) n=1; peer=2; reverse=0;; right) n=2; peer=1; reverse=1;; *) exit 2;; esac
workers=${WORKERS:-4}
flows=${FLOWS:-16}
duration=${DURATION:-20}
processes=${GENERATOR_PROCESSES:-2}
depth=${VLAN_DEPTH:-2}
segments=${SEGMENTS:-44}
port=${PORT:-40100}
[ "$((flows % processes))" = 0 ] || exit 2
share=$((flows / processes))
payload=1472
name=uq-pair
inner=/run/netns/uq-inner
mkdir -p "$out"
mount -t tmpfs tmpfs /run
mkdir -p /run/netns
pid=
servers=
clients=
gro_was=
tunnel=
case "${KERNEL_TUNNEL:-}" in ''|l2tp) ;; *) echo "KERNEL_TUNNEL must be l2tp" >&2; exit 2;; esac
case "${L2TP_ENCAP:-ip}" in ip|udp) ;; *) echo "L2TP_ENCAP must be ip or udp" >&2; exit 2;; esac
device=$(ip -6 route get "$peer6" | sed -n 's/.* dev \([^ ]*\).*/\1/p' | head -n 1)
cleanup() {
    for process in $clients $servers $pid; do
        kill "$process" 2>/dev/null || :
    done
    wait || :
    if [ -n "$tunnel" ]; then
        # Deleting the session removes its device and the VLAN devices on it.
        ip l2tp del session tunnel_id "$tunnel" session_id "$tunnel" 2>/dev/null || :
        ip l2tp del tunnel tunnel_id "$tunnel" 2>/dev/null || :
    fi
    # The TAP is persistent: it goes with its namespace, or by name if it never moved.
    ip netns del uq-inner 2>/dev/null || :
    [ -z "$gro_was" ] || ethtool -K "$device" $gro_was || :
    ip link show "$name" >/dev/null 2>&1 && "$daemon" remove-tap "$name" || :
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM
# Sleeps until the epoch time given, in seconds with a fraction.
until_time() {
    wait_for=$(awk -v at="$1" -v now="$(date +%s.%N)" 'BEGIN { d = at - now; printf "%.3f", (d > 0 ? d : 0) }')
    sleep "$wait_for"
}
# The k-th CPU of a comma-separated list, wrapping.
cpu_of() {
    echo "$1" | tr ',' '\n' | awk -v k="$2" '{ c[NR - 1] = $1 } END { print c[k % NR] }'
}
# OUTER_UDP_GRO=1 lets the outer device coalesce UDP for the daemon while this
# script runs, as a network card does; a veth needs both switches for it.
# The settings it found are put back on exit.
if [ "${OUTER_UDP_GRO:-0}" = 1 ]; then
    gro_was=$(ethtool -k "$device" | awk '
        $1 == "generic-receive-offload:" { g = $2 }
        $1 == "rx-udp-gro-forwarding:" { f = $2 }
        END { print "gro", g, "rx-udp-gro-forwarding", f }')
    ethtool -K "$device" gro on rx-udp-gro-forwarding on
fi
cat > "$out/$side.toml" <<CONFIG
${DAEMON_TOML:-}
[[endpoints]]
name = "rx"
listen_address = "$local6"
listen_port = $port
[[peers]]
name = "peer"
endpoint = "rx"
host = "$peer6"
peer_port = $port
tx_lanes = ${TX_LANES:-$workers}
outer_payload_limit = ${OUTER_PAYLOAD_LIMIT:-1452}
[[sessions]]
name = "$name"
peer = "peer"
cookie = "${SESSION_COOKIE:-00112233-4455-4677-8899-aabbccddeeff}"
inner_mtu = 1500
mac = "02:00:00:00:00:0$n"
queue_count = $workers
CONFIG
uname -srvm > "$out/$side.kernel"
sed -n '/model name/p' /proc/cpuinfo | head -n 1 > "$out/$side.cpu-model"
getconf CLK_TCK > "$out/$side.clock-ticks"
printf '%s\n' "${DAEMON_CPUS:-}" "${GENERATOR_CPUS:-}" "${RECEIVER_CPUS:-}" > "$out/$side.cpus"
ip -s link show dev "$device" > "$out/$side.outer-device"
if [ "${KERNEL_TUNNEL:-}" = l2tp ]; then
    tunnel=$((4000 + n))
    set -- tunnel_id "$tunnel" peer_tunnel_id "$((4000 + peer))" encap "${L2TP_ENCAP:-ip}" \
        local "$local6" remote "$peer6"
    [ "${L2TP_ENCAP:-ip}" = ip ] || set -- "$@" udp_sport "$port" udp_dport "$port"
    ip l2tp add tunnel "$@"
    ip l2tp add session name "$name" tunnel_id "$tunnel" session_id "$tunnel" \
        peer_session_id "$((4000 + peer))"
    # Room for a 1500-byte packet under two VLAN tags, as the TAP gives.
    ip link set "$name" address "02:00:00:00:00:0$n" mtu 1508
    echo 'TAPs ready' > "$out/$side.log"
    inner=/proc/self/ns/net
    # A stand-in for the daemon's process ID; it ignores the snapshot signal.
    sh -c 'trap "" USR1; exec sleep 1000000' &
else
    taskset -c "$DAEMON_CPUS" "$daemon" run "$out/$side.toml" > "$out/$side.log" 2>&1 &
fi
pid=$!
attempts=0
until grep -q 'TAPs ready' "$out/$side.log" 2>/dev/null && ip link show "$name" >/dev/null 2>&1; do
    kill -0 "$pid" || exit 1
    attempts=$((attempts + 1))
    [ "$attempts" -lt 500 ] || { echo "TAP readiness timeout" >&2; exit 1; }
    sleep 0.01
done
if [ -z "$tunnel" ]; then
    ip netns add uq-inner
    ip link set "$name" netns uq-inner
fi
in_ns() {
    nsenter --net="$inner" "$@"
}
in_ns ip link set lo up
in_ns ip link set "$name" txqueuelen "${TAP_TXQUEUELEN:-500}"
in_ns ip link set "$name" up
data=$name
if [ "$depth" = 2 ]; then
    in_ns ip link add link "$data" name uq-outer type vlan protocol 802.1ad id 42
    in_ns ip link set uq-outer up
    data=uq-outer
fi
if [ "$depth" != 0 ]; then
    in_ns ip link add link "$data" name uq-data type vlan id 7
    in_ns ip link set uq-data mtu 1500 up
    data=uq-data
fi
in_ns ip addr add "10.77.0.$n/24" dev "$data"
in_ns ip neigh add "10.77.0.$peer" lladdr "02:00:00:00:00:0$peer" nud permanent dev "$data"
snapshot() {
    count=$(grep -c '^unquic: worker [0-9][0-9]* Snapshot {.* }$' "$out/$side.log" || :)
    kill -USR1 "$pid"
    if [ "$1" != after ] && [ -n "$servers" ]; then
        kill -USR1 $servers 2>/dev/null || :
    fi
    attempts=0
    # A kernel tunnel prints no counters to wait for.
    [ -z "$tunnel" ] || count=$((count - workers))
    while [ "$(grep -c '^unquic: worker [0-9][0-9]* Snapshot {.* }$' "$out/$side.log" || :)" -lt "$((count + workers))" ]; do
        kill -0 "$pid" || exit 1
        attempts=$((attempts + 1))
        [ "$attempts" -lt 500 ] || { echo "counter snapshot timeout" >&2; exit 1; }
        sleep 0.01
    done
    cat "$out/$side.log" > "$point/$1.$side-log"
    cat /proc/uptime > "$point/$1.$side-uptime"
    cat /proc/stat > "$point/$1.$side-cpu"
    cat /proc/"$pid"/stat > "$point/$1.$side-process"
    cat /proc/"$pid"/task/*/stat > "$point/$1.$side-threads"
    for status in /proc/"$pid"/task/*/status; do
        printf '%s\n' "$status"
        sed -n '/^Name:/p; /^Pid:/p; /^voluntary_ctxt_switches:/p; /^nonvoluntary_ctxt_switches:/p' "$status"
    done > "$point/$1.$side-switches"
    sed -n '/^VmHWM:/p; /^VmRSS:/p' /proc/"$pid"/status > "$point/$1.$side-memory"
    cat /proc/net/snmp6 > "$point/$1.$side-outer"
    ip -s link show dev "$device" > "$point/$1.$side-outer-link"
    in_ns ip -s link show "$name" > "$point/$1.$side-link"
    in_ns tc -s qdisc show dev "$name" > "$point/$1.$side-tapqdisc"
}
index=0
# BITRATE_LEFT or BITRATE_RIGHT gives that side's senders rates of their own,
# for a load of one direction; both sides need the same number of points.
rates=${BITRATE:-62500000}
case "$side" in
    left) rates=${BITRATE_LEFT:-$rates};;
    right) rates=${BITRATE_RIGHT:-$rates};;
esac
for rate in $rates; do
    point=$out/$index
    mkdir "$point"
    echo "$rate" > "$point/bitrate"
    at=$((start + index * (duration + 10)))
    run=$((index + 1))
    # Left to right uses ports 51000.. to 52000..; right to left adds 2000 to both.
    until_time "$((at - 3))"
    k=0
    while [ "$k" -lt "$processes" ]; do
        suffix=
        [ "$k" = 0 ] || suffix=-$k
        base=$(((1 - reverse) * 2000 + k * share))
        prefix=
        [ "$reverse" = 1 ] || prefix=reverse-
        # Called directly, so that $! is the load process and not a subshell.
        nsenter --net="$inner" taskset -c "$(cpu_of "$RECEIVER_CPUS" "$k")" "$load" recv "10.77.0.$n" "10.77.0.$peer" \
            "$((52000 + base))" "$((51000 + base))" "$share" "$payload" "$((duration + 6))" 0 "$run" "$segments" \
            > "$point/${prefix}server$suffix.json" &
        servers="$servers $!"
        k=$((k + 1))
    done
    until_time "$((at - 1)).6"
    snapshot before
    until_time "$at"
    k=0
    while [ "$k" -lt "$processes" ]; do
        suffix=
        [ "$k" = 0 ] || suffix=-$k
        base=$((reverse * 2000 + k * share))
        prefix=
        [ "$reverse" = 0 ] || prefix=reverse-
        nsenter --net="$inner" taskset -c "$(cpu_of "$GENERATOR_CPUS" "$k")" "$load" send "10.77.0.$n" "10.77.0.$peer" \
            "$((51000 + base))" "$((52000 + base))" "$share" "$payload" "$duration" "$rate" "$run" "$segments" \
            > "$point/${prefix}client$suffix.json" &
        clients="$clients $!"
        k=$((k + 1))
    done
    for process in $clients; do wait "$process" || :; done
    clients=
    snapshot sent
    for process in $servers; do wait "$process" || :; done
    servers=
    snapshot after
    index=$((index + 1))
done
kill "$pid"
wait "$pid" || :
pid=
cat "$out/$side.log" > "$out/$side.final-log"
