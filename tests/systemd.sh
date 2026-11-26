#!/bin/sh
# Runs the daemon under systemd in a disposable Incus container and checks the
# printed unit: its links and ordering, its privileges, the descriptor store
# across a crash, a reload and two manager restarts, and a clean stop.
#
#   IMAGE=<incus image with systemd and iproute2> sh tests/systemd.sh <unquic binary | unquic .deb>
#
# Given a .deb (the image must then have dpkg), it installs the package and
# tests the unit and binary the package ships, and removes it at the end.
# INCUS_INIT_ARGS adds arguments to `incus init`, for example a storage pool
# on a host whose default profile cannot start a container. The container
# gets /dev/net/tun and no network; it is deleted on exit, on failure too.
# Prints PASS and the systemd version, or FAIL and the first failed check.
set -eu
[ $# -eq 1 ] && [ -n "${IMAGE:-}" ] || {
    echo "usage: IMAGE=<incus image> sh $0 <unquic binary | unquic .deb>" >&2
    exit 2
}
name=unquic-systemd-$$
# shellcheck disable=SC2086 # INCUS_INIT_ARGS is a list of arguments.
incus init "$IMAGE" "$name" ${INCUS_INIT_ARGS:-} > /dev/null
cleanup() { incus delete -f "$name" > /dev/null 2>&1 || :; }
trap cleanup 0
trap 'exit 1' HUP INT TERM
incus config device add "$name" tun unix-char path=/dev/net/tun > /dev/null
incus start "$name" > /dev/null 2>&1
i=0
until incus exec "$name" -- systemctl is-system-running 2> /dev/null | grep -Eq 'running|degraded'; do
    sleep 1
    i=$((i + 1))
    [ "$i" -lt 90 ] || {
        echo "FAIL: systemd did not come up in the container"
        exit 1
    }
done
case $1 in
*.deb)
    packaged=1
    incus -q file push "$1" "$name/root/unquic.deb"
    ;;
*)
    packaged=0
    incus -q file push --mode 0755 "$1" "$name/usr/local/bin/unquic"
    ;;
esac
incus exec "$name" --env PACKAGED="$packaged" -- sh -s << 'IN'
set -eu
C=/etc/unquic/unquic.toml
fail() {
    echo "FAIL: $*"
    systemctl --no-pager status unquic 2>&1 | tail -n 15
    journalctl --no-pager -n 30 -u unquic 2>&1 | tail -n 30
    exit 1
}
# cfg NAME[:QUEUES]...: writes a configuration with one session per argument.
cfg() {
    {
        printf "[[endpoints]]\nname = 'rx'\nlisten_address = '::'\nlisten_port = 40900\n"
        printf "[[peers]]\nname = 'peer'\nendpoint = 'rx'\nhost = '::1'\npeer_port = 40901\n"
        n=1
        for s in "$@"; do
            printf "[[sessions]]\nname = '%s'\npeer = 'peer'\n" "${s%%:*}"
            printf "cookie = '00112233-4455-4677-8899-aabbccddee%02x'\ninner_mtu = 1500\n" "$n"
            [ "${s#*:}" = "$s" ] || printf "queue_count = %s\n" "${s#*:}"
            n=$((n + 1))
        done
    } > "$C"
}
prop() { systemctl show -p "$1" --value unquic; }
# wait_for SECONDS COMMAND...: retries a check until it holds.
wait_for() {
    t=$1
    shift
    i=0
    until "$@" > /dev/null 2>&1; do
        sleep 0.2
        i=$((i + 1))
        [ "$i" -lt $((t * 5)) ] || return 1
    done
}
stored() { [ "$(prop NFileDescriptorStore)" = "$1" ]; }
queues() { ip -d link show "$1" | grep -q "numqueues $2 "; }
active() { [ "$(systemctl is-active unquic)" = active ]; }

mkdir -p /etc/unquic
if [ "$PACKAGED" = 1 ]; then
    dpkg -i /root/unquic.deb > /dev/null || fail "dpkg -i"
    unit=/usr/lib/systemd/system/unquic.service
    [ -e "$unit" ] || fail "the package ships no unit"
    # One template: the packaged unit is what the installed binary prints.
    unquic systemd-unit "$C" /run/unquic.pid /usr/bin/unquic | cmp -s - "$unit" ||
        fail "the packaged unit differs from the one the binary prints"
    [ "$(systemctl is-enabled unquic 2> /dev/null)" != enabled ] || fail "the package enabled the service"
else
    unit=/etc/systemd/system/unquic.service
    unquic systemd-unit "$C" /run/unquic.pid > "$unit"
fi
cfg uq-a
unquic check-config "$C" || fail "check-config"
# Container images may turn NoNewPrivileges off for every service; a drop-in
# of the unit's own that sorts last puts the unit's line back in force.
mkdir -p /etc/systemd/system/unquic.service.d
grep '^NoNewPrivileges=' "$unit" | sed '1i [Service]' \
    > /etc/systemd/system/unquic.service.d/zzzz-unit.conf
systemctl daemon-reload
[ "$(systemctl show -p NoNewPrivileges --value unquic)" = yes ] || fail "NoNewPrivileges is not in force"
systemctl enable unquic 2> /dev/null
[ -e /etc/systemd/system/network.target.requires/unquic.service ] ||
    fail "enable did not link the unit under network.target.requires"
systemctl show -p After --value network.target | grep -q unquic.service ||
    fail "network.target is not ordered after the unit"

systemctl start unquic || fail "start"
active || fail "not active after start"
ip link show uq-a > /dev/null || fail "no TAP after start"
pid=$(prop MainPID)
grep -q '^NoNewPrivs:[[:space:]]*1' "/proc/$pid/status" || fail "the daemon may gain privileges"
# CAP_NET_BIND_SERVICE (10), CAP_NET_ADMIN (12) and CAP_NET_RAW (13).
[ "$(awk '/^CapBnd:/ { print $2 }' "/proc/$pid/status")" = 0000000000003400 ] ||
    fail "bounding set $(grep CapBnd "/proc/$pid/status")"
wait_for 5 stored 1 || fail "store holds $(prop NFileDescriptorStore) after start, wanted 1"
unquic status /run/unquic.pid > /tmp/status || fail "status"
for line in 'unquic: version ' 'unquic: cpu ' 'unquic: limits rmem_max=' \
    'unquic: device lo (peer peer) driver=' 'unquic: session uq-a SessionStatus'; do
    grep -q "^$line" /tmp/status || fail "status lacks: $line"
done

# A crash: the manager restarts the daemon, which adopts the stored queue.
# Attaching by name would be refused while the store holds that queue.
index=$(cat /sys/class/net/uq-a/ifindex)
kill -9 "$pid"
changed() { [ "$(prop MainPID)" != "$pid" ] && [ "$(prop MainPID)" != 0 ] && active; }
wait_for 15 changed || fail "no restart after a crash"
[ "$(cat /sys/class/net/uq-a/ifindex)" = "$index" ] || fail "ifindex changed across a crash"
queues uq-a 1 || fail "queue count after a crash: $(ip -d link show uq-a | grep numqueues)"
wait_for 5 stored 1 || fail "store holds $(prop NFileDescriptorStore) after a crash, wanted 1"

# A reload onto another session: the old queue leaves the store, the new one enters.
pid=$(prop MainPID)
cfg uq-b
systemctl reload unquic || fail "reload"
wait_for 10 ip link show uq-b || fail "no new TAP after reload"
active || fail "not active after reload"
[ "$(prop MainPID)" = "$pid" ] || fail "main PID changed across a reload"
wait_for 5 stored 1 || fail "store holds $(prop NFileDescriptorStore) after reload, wanted 1"
wait_for 5 queues uq-a 0 || fail "the removed session's queue is still attached"
unquic remove-tap uq-a || fail "remove-tap of the removed session"
wait_for 5 sh -c '! ip link show uq-a' || fail "removed TAP still exists"

# A manager restart onto a resized session: the stored queue no longer fits.
index=$(cat /sys/class/net/uq-b/ifindex)
cfg uq-b:2
systemctl restart unquic || fail "restart onto two queues"
active || fail "not active after the resizing restart"
[ "$(cat /sys/class/net/uq-b/ifindex)" = "$index" ] || fail "ifindex changed across a restart"
wait_for 5 queues uq-b 2 || fail "queue count after resize: $(ip -d link show uq-b | grep numqueues)"
wait_for 5 stored 2 || fail "store holds $(prop NFileDescriptorStore) after resize, wanted 2"

# A manager restart with nothing changed adopts both queues.
systemctl restart unquic || fail "plain restart"
active || fail "not active after a plain restart"
queues uq-b 2 || fail "queue count after a plain restart"
wait_for 5 stored 2 || fail "store holds $(prop NFileDescriptorStore) after a plain restart, wanted 2"
unquic status /run/unquic.pid | grep -q '^unquic: session uq-b ' || fail "status after restarts"

systemctl stop unquic || fail "stop"
[ "$(systemctl is-active unquic)" = inactive ] || fail "state after stop: $(systemctl is-active unquic)"
[ ! -e /run/unquic.pid ] || fail "PID file left after stop"
[ ! -e /run/unquic.pid.status ] || fail "status file left after stop"
stored 0 || fail "store holds $(prop NFileDescriptorStore) after stop"
queues uq-b 0 || fail "queues still attached after stop"
unquic remove-tap uq-b || fail "remove-tap after stop"
if [ "$PACKAGED" = 1 ]; then
    dpkg -r unquic > /dev/null || fail "dpkg -r"
    [ ! -e /usr/bin/unquic ] || fail "the binary is left after removal"
fi
echo "PASS ($(systemctl --version | head -n 1))"
IN
