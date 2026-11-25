#!/bin/sh
# Merges the two sides of a benches/pair_side.sh run into the layout that
# examples/sweep_report.rs reads for one repeat:
#   sh benches/pair_merge.sh LEFT_OUT RIGHT_OUT RUN
# RUN becomes ROOT/BUILD/COMBO/REPEAT of the report's tree. The sides' files
# with addresses and device names (*.toml, *.outer-device) stay where they are.
set -eu
[ "$#" -eq 3 ] || { echo "usage: $0 LEFT_OUT RIGHT_OUT RUN" >&2; exit 2; }
left=$1 right=$2 run=$3
mkdir -p "$run"
cp "$left/left.final-log" "$run/left.log"
cp "$right/right.final-log" "$run/right.log"
cp "$left/left.clock-ticks" "$run/clock-ticks"
for point in "$left"/[0-9]*; do
    index=${point##*/}
    merged=$run/$index
    mkdir -p "$merged"
    cp "$left/$index"/* "$merged/"
    cp "$right/$index"/* "$merged/"
    cp "$run/clock-ticks" "$merged/clock-ticks"
    for snap in before sent after; do
        # Left first, as the report expects. Both sides snapshot at the same
        # moments, so the left host's uptime gives the elapsed time for both.
        cat "$merged/$snap.left-process" "$merged/$snap.right-process" > "$merged/$snap.process"
        cp "$merged/$snap.left-uptime" "$merged/$snap.uptime"
        # Thread and process ids of two hosts can collide: shift the right side's.
        {
            cat "$merged/$snap.left-threads"
            awk '{ $1 = $1 + 100000000; print }' "$merged/$snap.right-threads"
        } > "$merged/$snap.threads"
        {
            cat "$merged/$snap.left-switches"
            awk -F/ 'BEGIN { OFS = "/" } /^\/proc\// { $3 = $3 + 100000000; $5 = $5 + 100000000 } { print }' \
                "$merged/$snap.right-switches"
        } > "$merged/$snap.switches"
    done
done
