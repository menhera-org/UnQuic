#!/bin/sh
# Commit with Signed-off-by and a GPG signature.
#
#   ./commit.sh -m MSG [--include|--exclude] [FILE...]
#
# --include (default): stage FILEs and commit only them; with no FILE,
#                      commit what is already staged.
# --exclude:           stage and commit everything except FILEs.
# Options and files may appear in any order.
set -eu

usage() {
    echo "usage: $0 -m MSG [--include|--exclude] [FILE...]" >&2
    exit 2
}

msg=
mode=include
n=$#
while [ "$n" -gt 0 ]; do
    a=$1
    shift
    n=$((n - 1))
    case $a in
    -m)
        [ "$n" -gt 0 ] || usage
        msg=$1
        shift
        n=$((n - 1))
        ;;
    --include) mode=include ;;
    --exclude) mode=exclude ;;
    -*) usage ;;
    *) set -- "$@" "$a" ;;
    esac
done
[ -n "$msg" ] || usage

if [ "$mode" = exclude ]; then
    # Turn each FILE into an exclusion; ':/' anchors the rest to the repo root.
    n=$#
    while [ "$n" -gt 0 ]; do
        a=$1
        shift
        n=$((n - 1))
        set -- "$@" ":(exclude)$a"
    done
    set -- ':/' "$@"
    git --no-pager add -A -- "$@"
fi

[ $# -eq 0 ] || git --no-pager add -- "$@"
git --no-pager commit -s -S -m "$msg" -- "$@"
