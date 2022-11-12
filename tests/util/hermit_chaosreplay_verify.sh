#!/bin/bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Run once under chaos mode and a second time to replay the preemptions.

if [[ -z "$HERMIT_BIN" ]]; then
    HERMIT_BIN="hermit"
fi
command="$*"
if [[ -z "$HERMIT_ARGS" ]]; then
    HERMIT_ARGS=" run --chaos --base-env=empty --env=HERMIT_MODE=chaosreplay "
fi

echo ":: Running program twice to record then replay preemptions:"

tempdir=$(mktemp -d /tmp/hermit_verify_logs_XXXX)
echo ":: Placing output in temporary directory: $tempdir"
cd "$tempdir" || exit 1

mkdir -p "$tempdir/run1" "$tempdir/run2"

log1="$tempdir/log1"
stdout1="$tempdir/stdout1"
stderr1="$tempdir/stderr1"
preempts="$tempdir/preempts"

HERMIT_ARGS+="--bind $tempdir --workdir=/tmp/out "

# shellcheck disable=SC2086 # Intended splitting of args and command:
"$HERMIT_BIN" --log=debug --log-file="$log1" $HERMIT_ARGS \
               --mount="source=$tempdir/run1,target=/tmp/out" \
               --record-preemptions-to="$preempts" \
               -- $command 1> "$stdout1" 2> "$stderr1"
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

# shellcheck disable=SC2086 # Intended splitting of args and command:
if "$HERMIT_BIN" --log=debug --log-file="$log2" $HERMIT_ARGS \
              --mount="source=$tempdir/run2,target=/tmp/out" \
              --replay-preemptions-from="$preempts" \
             -- $command 1> "$stdout2" 2> "$stderr2"
then
    echo ":: Second run succeeded with zero exit code."
else
    echo ":: ERROR: second run exited with code $code"
    exit $code
fi

# ----------------------------------------
failed_comparison=0

echo ":: Comparing output from runs 1 and 2:"

if ! "$HERMIT_BIN" log-diff --ignore-lines="CHAOSRAND" --syscall-history=5 "$log1" "$log2"; then
    failed_comparison=1
fi

if ! diff "$stdout1" "$stdout2"; then
    echo ":: ERROR: difference found in stdouts"
    failed_comparison=1
fi

if ! diff "$stderr1" "$stderr2"; then
    echo ":: ERROR: difference found in stderrs"
    failed_comparison=1
fi

if [ $failed_comparison == 1 ]; then
    echo ":: Determinism check failed due to above mismatch(es) in output."
    echo "log1: $log1"
    echo "log2: $log2"
    exit 1
fi

# We intentionally leave results around if we exit in error.
echo ":: Finished.  Clearing temp files."
rm -f "$log1" "$stdout1" "$stderr1" "$log2" "$stdout2" "$stderr2"
rm -r "$tempdir"
