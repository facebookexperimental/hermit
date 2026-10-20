#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

#
# Demo 1: deterministic run.
#
# Hermit preserves the guest exit status and output while replacing common
# nondeterministic inputs (random bytes, wall-clock time, address layout, and
# Python hash seeding) with virtual, reproducible values.

set -euo pipefail

# shellcheck source=demos/lib/display.sh
source "$(dirname "${BASH_SOURCE[0]}")/../lib/display.sh"

# shellcheck disable=SC2034  # consumed by common.sh demo_success/demo_failure
DEMO_LABEL="Demo 1: Deterministic Run"
demo_header "$DEMO_LABEL"
echo 'Hermit preserves the guest exit status and output while making random bytes,'
echo 'wall-clock time, Python hash seeding, and heap address layout stable across'
echo 'runs. hermit run --verify runs the guest twice and compares exit status, output,'
echo "and Hermit's deterministic execution log. The guest must be idempotent: a first"
echo 'run that changes a file, database, cache, or external service can legitimately'
echo 'change the second run.'
echo ''
echo '=========================================='

# shellcheck source=demos/lib/common.sh
source "$(dirname "${BASH_SOURCE[0]}")/../lib/common.sh"

demo_banner "Basic execution"
run_hermit -- /bin/echo hello

demo_banner "Virtual random bytes are stable across runs"
for attempt in 1 2; do
  run_hermit -- /bin/sh -c 'od -An -N8 -tx1 /dev/urandom' | tee "$DEMO_TMP/urandom-hermit-$attempt.txt"
done
cmp "$DEMO_TMP/urandom-hermit-1.txt" "$DEMO_TMP/urandom-hermit-2.txt"

demo_banner "Virtual wall-clock time is stable across runs"
# The virtual clock starts at an epoch and then advances with the guest's own
# progress, not with real time. By default `hermit run` takes the epoch from
# one reading of the host clock and prints it on stderr ("hermit: virtual-time
# epoch=... reproduce with --epoch=..."), so two default runs start from
# different instants. Pinning the epoch makes the clock readings identical.
DEMO_EPOCH=2026-01-01T00:00:00Z
echo "-- both runs use --epoch=$DEMO_EPOCH --"
for attempt in 1 2; do
  run_hermit --epoch="$DEMO_EPOCH" -- /bin/date +%s.%N | tee "$DEMO_TMP/date-hermit-$attempt.txt"
done
cmp "$DEMO_TMP/date-hermit-1.txt" "$DEMO_TMP/date-hermit-2.txt"

demo_banner "Python entropy and hash ordering match under Hermit"
export PYTHON="${PYTHON:-python3}"
export PYTHON_DEMO='import os; print("random="+os.urandom(16).hex()); print("hash="+str(hash("hermit-demo"))); print("set="+",".join(set(["alpha","beta","gamma","delta","epsilon"])))'
echo "-- native (normally differs) --"
for attempt in 1 2; do
  "$PYTHON" -c "$PYTHON_DEMO"
done
echo "-- hermit (matches exactly) --"
for attempt in 1 2; do
  run_hermit -- "$PYTHON" -c "$PYTHON_DEMO" | tee "$DEMO_TMP/python-hermit-$attempt.txt"
done
cmp "$DEMO_TMP/python-hermit-1.txt" "$DEMO_TMP/python-hermit-2.txt"

demo_banner "Address layout is stable across runs"
for attempt in 1 2; do
  run_hermit -- "$HEAP_PTRS" | tee "$DEMO_TMP/heap-hermit-$attempt.txt"
done
cmp "$DEMO_TMP/heap-hermit-1.txt" "$DEMO_TMP/heap-hermit-2.txt"

demo_banner "Built-in --verify determinizes a racy multi-process guest"
# examples/race.sh forks two shells that print interleaved output, so the
# interleaving differs on every native run. Show that nondeterminism first:
echo "-- native race: output interleaving differs each run (checksum of output) --"
for attempt in 1 2; do
  race_out="$(/bin/bash "$RACE_SH")"
  printf 'native run %s: cksum=%s\n' "$attempt" "$(printf '%s' "$race_out" | cksum | cut -d' ' -f1)"
done
# --verify runs the guest twice under Hermit and compares exit status, output,
# and the deterministic execution log (thousands of scheduler messages at
# --log=info). This step uses PMU-based preemption (see verify_hermit in
# lib/common.sh) and therefore needs user-accessible performance counters.
echo "-- hermit --verify (identical output + verified execution log) --"
verify_hermit -- /bin/bash "$RACE_SH"

demo_success
