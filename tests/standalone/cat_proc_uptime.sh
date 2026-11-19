#!/bin/bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# /proc/uptime is rendered deterministically from virtual time, so a strict
# verified run that reads it must be reproducible bit for bit, and the value
# must advance as virtual time advances: a frozen rendering would be
# reproducible too, so parity alone is not enough. The guest therefore reads
# uptime, sleeps a virtual second, reads again, sleeps, reads: the
# whole-second values must strictly increase, identically in both runs.
#
# This test used to assert the opposite: it ran a relaxed
# `--verify --no-sequentialize-threads --no-deterministic-io` and required the
# verify to FAIL because /proc/uptime had changed between the two runs. That
# premise is obsolete now that Detcore renders /proc/uptime deterministically.
# Worse, the relaxed form is refused before any comparison when threads are
# not sequentialized (determinism-loss policy refusal), and the old script
# counted any nonzero exit as "differences found", so it usually passed
# without measuring anything and failed only on hosts where the comparison
# actually ran and correctly found no difference.

set -u

if [ "$*" == "" ]; then
    hermit="hermit"
else
    hermit="$1"
fi

# The canonical test epoch (detcore-model's canonical_test_epoch!): --strict
# refuses an epoch drawn from the host clock.
out=$("$hermit" run --strict --verify --verify-strict --epoch=2026-01-01T00:00:00Z -- \
    bash -c 'echo hello; cat /proc/uptime; sleep 1; cat /proc/uptime; sleep 1; cat /proc/uptime' 2>&1)
res=$?
printf '%s\n' "$out"

if [ "$res" != 0 ]; then
    echo "Error!  hermit run --strict --verify --verify-strict exited $res."
    exit 1
fi

mapfile -t secs < <(printf '%s\n' "$out" | sed -nE 's/^([0-9]+)\.[0-9]{2} [0-9]+\.[0-9]{2}$/\1/p')
if [ "${#secs[@]}" != 3 ]; then
    echo "Error!  Expected 3 /proc/uptime lines, saw ${#secs[@]}."
    exit 1
fi
if ! [ "${secs[0]}" -lt "${secs[1]}" ] || ! [ "${secs[1]}" -lt "${secs[2]}" ]; then
    echo "Error!  /proc/uptime did not strictly increase across sleeps: ${secs[*]}."
    exit 1
fi

echo "Deterministic, advancing /proc/uptime, as expected."
exit 0
