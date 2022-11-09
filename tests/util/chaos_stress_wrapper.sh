#!/bin/bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail

hermit="$1"
program="$2"
preempt="$3"
max_iters="${4:-1000}"

RANDOM=42 # Set random seed

perf list hardware | grep -i "Hardware event" > /dev/null || {
  >&2 echo "It seems that perf is not supported, so this chaos test will be skipped."
  >&2 echo "Chaos mode requires hardware performance counters."
  exit 0
}

run_count=1
success_count=0
fail_count=0

# We're searching for failures in mostly-succeeding programs, but it's possible
# hermit could get a bug which causes all chaos runs to fail. To detect this, we
# run until we get at least one success and one failure. This should occur
# within the max iterations, or within the test timeout

function run_test_once {
  seed=$RANDOM
  >&2 echo "=========== Chaos run #$run_count ==========="
  if (set -x; time "$hermit" run --base-env=minimal --chaos --seed "$seed" --preemption-timeout "$preempt" -- "$program"); then
    ((success_count=success_count+1))
  else
    ((fail_count=fail_count+1))
  fi
  ((run_count=run_count+1))
}

for ((run=1; run <= max_iters; run++)); do
  run_test_once
  if (( fail_count > 0 )) && (( success_count > 0 )); then
    >&2 echo "Found one success and one failure"
    exit 0
  fi
done

>&2 echo "Could not find one success and one failure."
exit 1
