#!/bin/sh
# Runs the ignored kernel tests under another kernel, in a QEMU guest with no
# network device. It needs no KVM: emulation is fast enough (the suite takes
# about a quarter of an hour, ten minutes of it one path-MTU test's timer).
#
#   sh tests/qemu-kernel.sh <vmlinuz> <modules directory> <log> <daemon> <test executable>...
#
# <modules directory> is the kernel's lib/modules/<release>. The guest is an
# initramfs built from a static busybox (BUSYBOX, default the one on PATH),
# the host's ip and tc with the libraries they need, the kernel's network
# modules, the daemon and the test executables, which must be static (musl)
# builds: `cargo test --release --no-run` lists them. OVS_ROOT, when set, is
# copied in for the Open vSwitch test, with the libraries its programs need.
# Also needs qemu-system-x86_64, depmod, cpio and gzip.
#
# Prints every failed test and a total; exits 0 when none failed.
set -eu
[ $# -ge 5 ] || {
    echo "usage: sh $0 <vmlinuz> <modules directory> <log> <daemon> <test executable>..." >&2
    exit 2
}
kernel=$1
modules=$2
log=$3
daemon=$4
shift 4
release=${modules##*/}
work=$(mktemp -d)
trap 'rm -rf "$work"' 0
trap 'exit 1' HUP INT TERM
root=$work/root
mkdir -p "$root/bin" "$root/t" "$root/proc" "$root/sys" "$root/dev" "$root/tmp" "$root/run" \
    "$root/usr/sbin" "$root/lib/modules/$release/kernel/drivers"
busybox=${BUSYBOX:-$(command -v busybox)}
cp "$busybox" "$root/bin/busybox"
for applet in sh mount mkdir mknod cat echo modprobe sleep poweroff uname grep sed date; do
    ln -s busybox "$root/bin/$applet"
done
# The kernel loads a module on demand through /sbin/modprobe.
mkdir "$root/sbin"
ln -s ../bin/busybox "$root/sbin/modprobe"
cp -R "$modules/kernel/net" "$modules/kernel/lib" "$modules/kernel/crypto" "$root/lib/modules/$release/kernel/"
cp -R "$modules/kernel/drivers/net" "$root/lib/modules/$release/kernel/drivers/"
cp "$modules/modules.builtin" "$modules/modules.order" "$root/lib/modules/$release/"
depmod -b "$root" "$release" 2> /dev/null
# Copies a dynamically linked host program's libraries to the same paths.
libraries() {
    for library in $(ldd "$1" 2> /dev/null | grep -o '/[^ ]*'); do
        [ -e "$root$library" ] || {
            mkdir -p "$root$(dirname "$library")"
            cp -L "$library" "$root$library"
        }
    done
}
for tool in ip tc; do
    path=$(command -v "$tool")
    cp "$path" "$root/usr/sbin/$tool"
    libraries "$path"
done
if [ -n "${OVS_ROOT:-}" ]; then
    cp -R "$OVS_ROOT" "$root/t/ovs-root"
    for program in "$root"/t/ovs-root/usr/bin/* "$root"/t/ovs-root/usr/sbin/* \
        "$root"/t/ovs-root/usr/lib/openvswitch-switch/*; do
        [ -f "$program" ] && libraries "$program"
    done
fi
cp "$daemon" "$root/t/daemon"
number=0
for executable in "$@"; do
    cp "$executable" "$root/t/test$number"
    number=$((number + 1))
done
cat > "$root/init" << 'INIT'
#!/bin/sh
export PATH=/usr/sbin:/usr/bin:/sbin:/bin
mount -t proc proc /proc
mount -t sysfs sys /sys
mount -t devtmpfs dev /dev 2> /dev/null || mount -t tmpfs dev /dev
mkdir -p /dev/net /dev/shm
[ -e /dev/net/tun ] || mknod /dev/net/tun c 10 200
mount -t tmpfs tmp /tmp
mount -t tmpfs run /run
mkdir -p /run/netns /var/run
for module in veth bridge 8021q vrf dummy macvlan openvswitch sch_netem ifb sch_ingress \
    cls_flower cls_u32 cls_matchall act_gact act_mirred; do
    modprobe "$module" 2> /dev/null || echo "no module $module"
done
ip link set lo up
echo "KERNEL $(uname -sr)"
for test in /t/test*; do
    echo "=== ${test##*/}"
    UNQUIC_OVS_ROOT=/t/ovs-root UNQUIC_TEST_BINARY=/t/daemon "$test" --ignored --test-threads=1 2>&1
done
echo ALL-DONE
poweroff -f
INIT
chmod 755 "$root/init"
(cd "$root" && find . | cpio -o -H newc 2> /dev/null | gzip -1 > "$work/initramfs.gz")
rm -f "$log"
qemu-system-x86_64 -accel tcg,thread=multi -cpu max -smp 4 -m 4096 \
    -kernel "$kernel" -initrd "$work/initramfs.gz" \
    -append "console=ttyS0 panic=-1 random.trust_cpu=on quiet" \
    -display none -serial "file:$log" -nic none -no-reboot < /dev/null
tr -d '\r' < "$log" > "$work/log"
grep -q '^ALL-DONE$' "$work/log" || {
    echo "the guest did not finish; see $log"
    exit 1
}
grep '^KERNEL ' "$work/log"
grep '^test .* FAILED$' "$work/log" || :
awk '/^test result:/ { passed += $4; failed += $6 } END { print passed + 0, "passed,", failed + 0, "failed"; exit failed != 0 }' "$work/log"
