#!/bin/sh
# Fetch, rebase the current branch onto its upstream, and push.
set -eu
git --no-pager fetch --prune
# --autostash: others edit this tree concurrently; keep their uncommitted
# changes out of the rebase's way.
git --no-pager rebase --autostash
git --no-pager push
