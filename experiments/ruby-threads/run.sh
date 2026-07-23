#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# Ruby thread-scheduling (non)determinism probe with a dual assertion:
#   1. NATIVE runs must DIFFER   -> nondeterminism exists (thread scheduling).
#   2. HERMIT --strict runs must be IDENTICAL -> determinism achieved.
#
# Current result on this host: assertion (1) holds; assertion (2) cannot be
# evaluated because Ruby 3.0 multithreading LIVELOCKS under hermit --strict (a
# worker thread spins on read() of Ruby's internal thread-wakeup pipe while the
# writer is never scheduled). Non-strict hermit completes, isolating the issue
# to the sequentialized scheduler. The script records exactly that.

set -uo pipefail
cd "$(dirname -- "${BASH_SOURCE[0]}")"

RUBY=${RUBY:-$(command -v ruby)}
# The host's system RubyGems is broken (did_you_mean/RbConfig); --disable-gems
# skips the prelude. The test itself uses no gems.
RUBY_ARGS=(--disable-gems)
HERMIT=${HERMIT:-../../target/release/hermit}
PROG=thread_order.rb
NTHREADS=${NTHREADS:-24}
NRUNS=${NRUNS:-5}
STRICT_TIMEOUT=${STRICT_TIMEOUT:-60}

run_native() { "$RUBY" "${RUBY_ARGS[@]}" "$PROG" "$NTHREADS" 2>/dev/null | md5sum | cut -d' ' -f1; }

echo "ruby: $("$RUBY" --version)"
echo "program: $PROG  threads: $NTHREADS  runs: $NRUNS"
echo "hermit: $HERMIT"
echo

echo "== native (expect DIFFERING hashes) =="
declare -A seen=()
for _ in $(seq 1 "$NRUNS"); do h=$(run_native); echo "  $h"; seen[$h]=1; done
native_distinct=${#seen[@]}
echo "distinct native outputs: $native_distinct / $NRUNS"
[ "$native_distinct" -ge 2 ] && echo "ASSERT-1 PASS: native is nondeterministic" \
  || { echo "ASSERT-1 FAIL: native output did not vary"; }
echo

echo "== hermit --strict (want IDENTICAL; bounded to ${STRICT_TIMEOUT}s) =="
strict_status="unknown"
timeout "$STRICT_TIMEOUT" "$HERMIT" run -- "$RUBY" "${RUBY_ARGS[@]}" "$PROG" "$NTHREADS" >/tmp/ruby_strict1.out 2>/dev/null
rc1=$?
if [ "$rc1" -eq 124 ]; then
  strict_status="DEADLOCK/TIMEOUT"
  echo "  strict run did not complete within ${STRICT_TIMEOUT}s -> $strict_status"
  echo "ASSERT-2 BLOCKED: Ruby multithreading livelocks under hermit --strict"
else
  timeout "$STRICT_TIMEOUT" "$HERMIT" run -- "$RUBY" "${RUBY_ARGS[@]}" "$PROG" "$NTHREADS" >/tmp/ruby_strict2.out 2>/dev/null
  h1=$(md5sum </tmp/ruby_strict1.out | cut -d' ' -f1)
  h2=$(md5sum </tmp/ruby_strict2.out | cut -d' ' -f1)
  echo "  run1=$h1 run2=$h2"
  if [ "$h1" = "$h2" ]; then strict_status="deterministic"; echo "ASSERT-2 PASS: strict is deterministic";
  else strict_status="NONDETERMINISTIC"; echo "ASSERT-2 FAIL: strict outputs differ"; fi
fi
echo

echo "== hermit non-strict (control: should complete) =="
timeout "$STRICT_TIMEOUT" "$HERMIT" run --no-sequentialize-threads --no-deterministic-io \
  -- "$RUBY" "${RUBY_ARGS[@]}" mini.rb 2 >/dev/null 2>&1
nonstrict_rc=$?
echo "  non-strict (mini.rb, 2 threads) exit=$nonstrict_rc"

{
  echo "field,value"
  echo "ruby_version,$("$RUBY" --version | tr ',' ' ')"
  echo "program,$PROG"
  echo "threads,$NTHREADS"
  echo "native_runs,$NRUNS"
  echo "native_distinct,$native_distinct"
  echo "strict_status,$strict_status"
  echo "strict_timeout_s,$STRICT_TIMEOUT"
  echo "nonstrict_rc,$nonstrict_rc"
} > results.csv
echo "wrote results.csv"
