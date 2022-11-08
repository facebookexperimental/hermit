#!/bin/bash
# (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

# A poor-man's version of `hermit run --verify` which does NOT use
# hermit as a library. This exists for debugging certain
# inconsistencies that seem to be possible when launching hermit twice
# from the same process.

if [[ -z "$HERMIT_BIN" ]]; then
    HERMIT_BIN="hermit"
fi
# Common arguments:
if [[ -z "$HERMIT_ARGS" ]]; then
    HERMIT_ARGS="--seed=3"
fi
command="$*"

diff="git --no-pager diff  --color --color-words -w"

echo ":: Recording trace then verify determinism on replay..."

tempdir=$(mktemp -d /tmp/hermit_verify_trace_replay_logs_XXXX)
echo ":: Placing output in temporary directory: $tempdir"

log1=$(mktemp "$tempdir"/log1_XXXX)
sched1=$(mktemp "$tempdir"/sched1_XXXX)
stdout1=$(mktemp "$tempdir"/stdout1_XXXX)
stderr1=$(mktemp "$tempdir"/stderr1_XXXX)

echo ":: Running to record preemptions to $sched1, using args $HERMIT_ARGS"
# shellcheck disable=SC2086 # Intended splitting of args and command:
RUST_BACKTRACE=1 RUST_LOG=detcore=trace "$HERMIT_BIN" --log-file="$log1" run \
  $HERMIT_ARGS --record-preemptions-to="$sched1" \
  --base-env=minimal --chaos --bind "$tempdir" -- $command 1> "$stdout1" 2> "$stderr1"
code1=$?
echo ":: First run complete... Stdout:"
echo ":: ------------------Stdout----------------------"
cat "$stdout1"
echo ":: ----------------End Stdout--------------------"
echo
echo ":: ------------------Stderr----------------------"
cat "$stderr1"
echo ":: ----------------End Stderr--------------------"

echo
echo ":: First run exited with code $code1"

# ----------------------------------------
log2=$(mktemp "$tempdir"/log2_XXXX)
sched2=$(mktemp "$tempdir"/sched2_XXXX)
stdout2=$(mktemp "$tempdir"/stdout2_XXXX)
stderr2=$(mktemp "$tempdir"/stderr2_XXXX)

# shellcheck disable=SC2086 # Intended splitting of args and command:
RUST_BACKTRACE=1 RUST_LOG=detcore=trace "$HERMIT_BIN" --log-file="$log2" run \
   --replay-schedule-from="$sched1" --record-preemptions-to="$sched2" \
   --base-env=minimal --preemption-timeout=10000000000000000000 --bind "$tempdir" -- $command 1> "$stdout2" 2> "$stderr2";
code2=$?
echo ":: Second run exited with code $code2"

# ----------------------------------------
failed_comparison=0

if [ $code1 == $code2 ]; then
    echo "Exit codes matched, good."
else
    echo ":: ERROR: exit codes were not the same!"
    failed_comparison=1
fi

echo ":: Comparing output from runs 1 and 2:"

if ! $diff "$stdout1" "$stdout2"; then
    echo ":: ERROR: difference found in stdouts"
    failed_comparison=1
fi

if ! $diff "$stderr1" "$stderr2"; then
    echo ":: ERROR: difference found in stderrs"
    failed_comparison=1
fi

grep "finish syscall" "$log1" | sed 's/.*detcore://' > "${log1}.syscalls"
grep "finish syscall" "$log2" | sed 's/.*detcore://' > "${log2}.syscalls"

echo ":: Syscall counts:"
wc -l "${log1}.syscalls" "${log2}.syscalls"

if ! $diff "${log1}.syscalls" "${log2}.syscalls"; then
    echo ":: ERROR: difference in linear syscall completion histories between trace/replay runs."
    failed_comparison=1
fi

if [ $failed_comparison == 1 ]; then
    echo ":: Determinism check failed due to above mismatch(es) in output."
    echo "log1: $log1"
    echo "log2: $log2"
    exit 1
fi

echo ":: Syscall histories matched.  Output matched.  Test passes."

# We intentionally leave results around if we exit in error.
echo ":: Finished.  Clearing temp files."
rm -r "$tempdir"
