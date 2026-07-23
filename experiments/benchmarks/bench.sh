#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# Measure hermit determinization overhead: native vs --strict vs non-strict.
# No external deps (hyperfine not required); uses wall-clock timing over N runs
# and reports the minimum (most stable) and mean per mode, plus overhead ratios.

set -uo pipefail

HERMIT="${HERMIT:-./target/release/hermit}"
# Non-strict = the documented opt-outs that --strict forbids.
NOSTRICT_FLAGS=(--no-sequentialize-threads --no-deterministic-io)

# Input for the bzip2 case must live OUTSIDE /tmp: hermit overlays the guest's
# /tmp with a private tmpfs, so host /tmp paths are invisible to the guest.
BENCH_DATA="${BENCH_DATA:-$HOME/hbench_2m.bin}"
if [ ! -f "$BENCH_DATA" ]; then
  head -c 2000000 /dev/urandom > "$BENCH_DATA"
fi

# min_mean RUNS CMD...  -> echoes "min mean" seconds (guest stdout+hermit stderr discarded)
min_mean() {
  local runs="$1"; shift
  local min="" sum=0 t
  for _ in $(seq 1 "$runs"); do
    local t0 t1
    t0=$(date +%s.%N)
    "$@" >/dev/null 2>/dev/null
    t1=$(date +%s.%N)
    t=$(echo "$t1 - $t0" | bc)
    sum=$(echo "$sum + $t" | bc)
    if [ -z "$min" ] || (( $(echo "$t < $min" | bc -l) )); then min="$t"; fi
  done
  local mean; mean=$(echo "scale=6; $sum / $runs" | bc)
  echo "$min $mean"
}

ratio() { echo "scale=1; $1 / $2" | bc; }

bench_case() {
  local name="$1" runs="$2"; shift 2
  precheck "$@"
  local native_read strict_read nostrict_read
  native_read=$(min_mean "$runs" "$@")
  strict_read=$(min_mean "$runs" "$HERMIT" run -- "$@")
  nostrict_read=$(min_mean "$runs" "$HERMIT" run "${NOSTRICT_FLAGS[@]}" -- "$@")
  local nmin nmean smin smean xmin xmean
  read -r nmin nmean <<<"$native_read"
  read -r smin smean <<<"$strict_read"
  read -r xmin xmean <<<"$nostrict_read"
  printf '%-12s | %3s | %9.4f | %9.4f (%5sx) | %9.4f (%5sx)\n' \
    "$name" "$runs" "$nmin" "$smin" "$(ratio "$smin" "$nmin")" "$xmin" "$(ratio "$xmin" "$nmin")"
}

echo "hermit: $HERMIT"
echo "host:   $(uname -srm) | $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2 | sed 's/^ //')"
echo "input:  bzip2 data = $(du -h "$BENCH_DATA" 2>/dev/null | cut -f1) at $BENCH_DATA"
echo "date:   $(date -u +%Y-%m-%dT%H:%M:%SZ)"
echo
printf '%-12s | %3s | %9s | %-18s | %-18s\n' "workload" "run" "native s" "--strict s (x)" "non-strict s (x)"
printf '%s\n' "-------------|-----|-----------|--------------------|-------------------"



# Refuse to time a mode that does not exit 0 -- a silently failing guest (e.g. a
# missing input path) would otherwise be recorded as a bogus "fast" result.
precheck() {
  "$@" >/dev/null 2>/dev/null || { echo "PRECHECK FAILED (native): $*" >&2; exit 3; }
  "$HERMIT" run -- "$@" >/dev/null 2>/dev/null || { echo "PRECHECK FAILED (strict): $*" >&2; exit 3; }
  "$HERMIT" run "${NOSTRICT_FLAGS[@]}" -- "$@" >/dev/null 2>/dev/null || { echo "PRECHECK FAILED (non-strict): $*" >&2; exit 3; }
}

# --- workloads -------------------------------------------------------------
# Fixed-overhead / tiny process workloads:
bench_case "true"    20 /bin/true
bench_case "echo"    20 /bin/echo hello
bench_case "ls"      20 /bin/ls -la /usr/bin
# CPU + streaming I/O (LULESH/compute stand-in; real LULESH not in this checkout):
bench_case "bzip2-2MB" 3 /bin/bzip2 -c "$BENCH_DATA"
# fork/exec/wait heavy (ninja_test stand-in: 200 process spawns):
bench_case "procx200" 5 /bin/sh -c 'i=0; while [ $i -lt 200 ]; do /bin/true; i=$((i+1)); done'
