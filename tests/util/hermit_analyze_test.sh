#!/bin/bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# This goes along with the chaos/cas_sequence.rs test.

if [ "$2" == "" ]; then
    echo "Script expects two arguments! (hermit path and cas_sequence binary path)"
    exit 1;
fi

# Prereqs: expects the full path to both binaries to be passed as arguments:
HERMIT=$1
TESTBIN=$2

if [[ -z "$KEEP_LOGS" ]]; then
    KEEP_LOGS=0
fi

if [[ -z "$DO_MINIMIZE" ]]; then
    DO_MINIMIZE=0
fi


if [[ "$KEEP_LOGS" != "0" ]]; then
    set -x
fi

# Common arguments:
if [[ -z "$HERMIT_ARGS" ]]; then
    # hermit analyze args:
    HERMIT_ARGS="--analyze-seed=0 "
    if [[ "$DO_MINIMIZE" != "0" ]]; then
        HERMIT_ARGS+="--minimize "
    fi
    HERMIT_ARGS+="--search "
    if [[ "$KEEP_LOGS" != "0" ]]; then
        HERMIT_ARGS+="--verbose "
    fi
    HERMIT_ARGS+=" -- "
    # hermit run args:
    HERMIT_ARGS+="--base-env=minimal "
    HERMIT_ARGS+="--chaos "
    HERMIT_ARGS+="--preemption-timeout=400000 "
fi
set -eu
TEMP=$(mktemp -d /tmp/analyze_test_XXXXX)
echo ":: [analyze_test] Temporary workspace: $TEMP"
TEMPLOG="${TEMP}/log.txt"
HERMIT_ARGS="--report-file=${TEMP}/report.json $HERMIT_ARGS"

trap cleanup EXIT

function cleanup {
    if [[ "$KEEP_LOGS" == "0" ]]; then
        rm -rf "$TEMP"
    fi
}

echo ":: [analyze_test] Invoking analyze with: $HERMIT analyze $HERMIT_ARGS -- $TESTBIN"

# shellcheck disable=SC2086 # Intended splitting of args and command:
$HERMIT analyze $HERMIT_ARGS -- "$TESTBIN" > >(tee "$TEMPLOG") \
  || (echo "Analyze failed."; exit 1)

echo ":: [analyze_test] Searching for printed backtraces in $TEMPLOG"
grep -q "Guest .* below backtrace" "$TEMPLOG" \
   || (echo "Stack trace not printed by hermit analyze!"; exit 1)
code=$?
exit $code
