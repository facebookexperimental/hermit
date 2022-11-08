#!/bin/bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

if [[ -z "$HERMIT_BIN" ]]; then
    HERMIT_BIN="hermit"
fi
echo Running with minimal_hello binary at path = "$MINIMAL_HELLO"
if [[ -z "$MINIMAL_HELLO" ]]; then
    echo "ERROR: needs MINIMAL_HELLO env var set to path of binary."
    exit 1
fi
if [[ -z "$TMPDIR" ]]; then
    TMPDIR="/tmp"
fi
set -eu

summary=$(mktemp "$TMPDIR"/summary_XXXXXX.txt)

echo ":: First run: count the number of events. "
"$HERMIT_BIN" run --record-preemptions --summary "$MINIMAL_HELLO" 2> "$summary"
echo ":: Run complete."

events=$(grep -Eo -e "recorded [[:digit:]]+ events" "$summary" | sed 's/recorded //' | sed 's/ events//')
echo ":: Counted events ${events}"

args=()
for ((ix = 0; ix < events; ix++)); do
    args+=("--stacktrace-event=${ix}")
done

echo ":: Now run and print stack traces for all those events"
run2_out=$(mktemp "$TMPDIR"/run2_out_XXXXXX.txt)
"$HERMIT_BIN" --log=info run --record-preemptions --summary "${args[@]}" "$MINIMAL_HELLO" 2> "$run2_out"
echo ":: Run2 produced $(wc -l "$run2_out" | awk '{print $1}') lines of stderr output to ${run2_out}"

num_stack_traces=$(grep -c "Printing stack trace for scheduled event" "$run2_out")
echo ":: Observed ${num_stack_traces} stack trace print messages."

if [ "${num_stack_traces}" != "${events}" ]; then
    echo "ERROR: ${num_stack_traces} did not match expected ${events}."
    exit 1
else
    echo "PASSED."
    rm "$summary" "$run2_out"
fi
