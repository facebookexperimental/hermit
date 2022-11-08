#!/bin/bash
# (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

# This goes along with the chaos/cas_sequence.rs test.

if [ "$2" == "" ]; then
    echo "Script expects two arguments! (hermit path and cas_sequence binary path)"
    exit 1;
fi

# Prereqs: expects the full path to both binaries to be passed as arguments:
HERMIT=$1
TESTBIN=$2

# Common arguments:
if [[ -z "$HERMIT_ARGS" ]]; then
    # hermit analyze args:
    HERMIT_ARGS="--seed=0 "
    HERMIT_ARGS+="--verbose "
    HERMIT_ARGS+="--minimize "
    HERMIT_ARGS+="--search "
    HERMIT_ARGS+=" -- "
    # hermit run args:
    HERMIT_ARGS+="--base-env=minimal "
    HERMIT_ARGS+="--chaos "
    HERMIT_ARGS+="--sched-seed=14230524012508565024 "
    HERMIT_ARGS+="--preemption-timeout=400000 "
fi

TEMP=$(mktemp /tmp/analyze_test_XXXXX.txt)

trap cleanup EXIT

function cleanup {
    rm "$TEMP"
}

echo "Test script invoking analyze with: $HERMIT analyze $HERMIT_ARGS -- $TESTBIN"

# shellcheck disable=SC2086 # Intended splitting of args and command:
$HERMIT analyze $HERMIT_ARGS -- "$TESTBIN" 2> >(tee "$TEMP") \
  || (echo "Analyze failed."; exit 1)

grep -q "Guest .* below backtrace" "$TEMP" \
   || (echo "Stack trace not printed by hermit analyze!"; exit 1)
exit $?
