#!/bin/bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Run once under strict mode and a second time to replay full event trace.

if [[ -z "$HERMIT_BIN" ]]; then
    HERMIT_BIN="hermit"
fi
command="$*"
if [[ -z "$HERMIT_ARGS" ]]; then
    HERMIT_ARGS=" --log=trace run --base-env=empty --env=HERMIT_MODE=tracereplay "
fi

if [[ -z "$KEEP_LOGS" ]]; then
    KEEP_LOGS=0
fi

function diff() {
    git --no-pager diff --color --color-words -w "$1" "$2"
}

if [[ "$KEEP_LOGS" != "0" ]]; then
    set -x
fi
set -eu
echo ":: Running program twice to record then replay event traces: $HERMIT_BIN $HERMIT_ARGS $command"

tempdir=$(mktemp -d /tmp/hermit_verify_logs_XXXX)
echo ":: Placing output in temporary directory: $tempdir"

log1="$tempdir/log1"
stdout1="$tempdir/stdout1"
stderr1="$tempdir/stderr1"
sched1="$tempdir/sched1"

export RUST_BACKTRACE=1

# shellcheck disable=SC2086 # Intended splitting of args and command:
"$HERMIT_BIN" --log-file="$log1" $HERMIT_ARGS --bind "$tempdir" --record-preemptions-to="$sched1" -- $command 1> "$stdout1" 2> "$stderr1"
code=$?
echo ":: First run complete... Stdout:"
echo ":: ------------------Stdout----------------------"
cat "$stdout1"
echo ":: ----------------End Stdout--------------------"
echo
echo ":: ------------------Stderr----------------------"
cat "$stderr1"
echo ":: ----------------End Stderr--------------------"

echo
if [ $code == 0 ]; then
    echo ":: First run succeeded with zero exit code."
else
    echo ":: ERROR: first run exited with code $code"
    exit $code
fi

# ----------------------------------------
log2="$tempdir/log2"
stdout2="$tempdir/stdout2"
stderr2="$tempdir/stderr2"
sched2="$tempdir/sched2"

# shellcheck disable=SC2086 # Intended splitting of args and command:
if "$HERMIT_BIN" --log-file="$log2" $HERMIT_ARGS --bind "$tempdir" --replay-schedule-from="$sched1" --record-preemptions-to="$sched2" -- $command 1> "$stdout2" 2> "$stderr2";
then
    echo ":: Second run succeeded with zero exit code."
else
    echo ":: ERROR: second run exited with code $code"
    exit $code
fi

# ----------------------------------------
failed_comparison=0

function help_debugging() {
    sed 's/^2022-..-..T..:..:..\.......Z *//' "$log1" > "${log1}.stripped"
    sed 's/^2022-..-..T..:..:..\.......Z *//' "$log2" > "${log2}.stripped"
}

function exit_failure() {
    # For more convenient debugging, strip the timestamps:
    help_debugging
    exit 1
}

echo ":: Comparing output from runs 1 and 2:"

# We don't have the right options yet for log-diff to do this "weak" comparison
# where the turns don't match but the syscalls do:
# if ! "$HERMIT_BIN" log-diff "$log1" "$log2"; then
#     failed_comparison=1
# fi

if ! diff "$stdout1" "$stdout2"; then
    echo ":: ERROR: difference found in stdouts"
    failed_comparison=1
fi

if ! diff "$stderr1" "$stderr2"; then
    echo ":: ERROR: difference found in stderrs"
    failed_comparison=1
fi

function get_det_lines() {
    grep "DETLOG" "$1" | sed 's/.*detcore://' > "${1}.detlogs"
}

get_det_lines "$log1"
get_det_lines "$log2"

echo ":: Comparing deterministic per-thread entries.  Log counts:"
wc -l "${log1}.detlogs" "${log2}.detlogs"
if ! diff "${log1}.detlogs" "${log2}.detlogs"; then
    echo ":: ERROR: difference in detlog histories (syscalls etc    ) between trace/replay runs."
    failed_comparison=1
fi

echo ":: Looking for desync events:"
if grep DESYNC "$log2"; then
    echo "WARNING: DESYNCs found!!"
    failed_comparison=1
fi

if [ $failed_comparison == 1 ]; then
    echo ":: Determinism check failed due to above mismatch(es) in output."
    echo "log1: $log1"
    echo "log2: $log2"
    exit_failure
fi

echo ":: Checking that event schedules match (fixed point)"
jq '.global' < "${sched1}" > "${sched1}.pretty"
jq '.global' < "${sched2}" > "${sched2}.pretty"

if ! diff "${sched2}.pretty" "${sched1}.pretty"; then
    echo ":: Schedules did not match. Failing."
    exit_failure
fi

# We intentionally leave results around if we exit in error.
echo ":: Finished.  Test passed!"
if [[ "$KEEP_LOGS" == "0" ]]; then
    echo ":: Clearing temp files."
    rm -f "$log1" "$stdout1" "$stderr1" "$log2" "$stdout2" "$stderr2"
    rm -r "$tempdir"
else
    help_debugging
fi
