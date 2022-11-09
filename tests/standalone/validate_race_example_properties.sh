#!/bin/bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail

if [ "$*" == "" ]; then
    hermit="hermit"
else
    hermit="$1"
fi

dir=$(mktemp -d /tmp/validate_race_example_properties_XXXXX)
log1="$dir/log1"
log2="$dir/log2"
log3="$dir/log3"
log4="$dir/log4"
log5="$dir/log5"
log6="$dir/log6"

function run() {
    local seed="$1"
    local schedseed="$2"
    local log="$3"
    "$hermit" --log=info run \
       --base-env=empty --chaos --preemption-timeout=20000 --summary \
       --seed="$seed" --sched-seed="$schedseed" examples/race.sh 200 2> "$log"
}

echo ":: Property 1: race.sh is invariant to changing --seed"
run 0  1 "$log1" &
run 10 1 "$log2" &
wait
"$hermit" log-diff --syscall-history=5 "$log1" "$log2"

echo ":: Property 2: race.sh is sensitive to changing --sched-seed"
run 0 1 "$log3" &
run 0 2 "$log4" &
wait
if "$hermit" log-diff --syscall-history=5 "$log3" "$log4"; then
    echo "ERROR: expected difference in $log3 $log4"
fi

echo ":: Property 3: when both seeds are the same and we should see IDENTICAL runs:"
run 0 1 "$log5" &
run 0 1 "$log6" &
wait
"$hermit" log-diff --syscall-history=5 "$log5" "$log6"

echo ":: Cleanup."
rm -rf "$dir"
echo ":: Test passed."
