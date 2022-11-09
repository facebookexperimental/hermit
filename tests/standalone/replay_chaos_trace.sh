#!/bin/bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail

# An example of running chaos and then relpaying the raw schedule.

if [ "$*" == "" ]; then
    hermit="hermit"
else
    hermit="$1"
fi

tmpdir=$(mktemp -d)
log1=$(mktemp -p "$tmpdir")
log2=$(mktemp -p "$tmpdir")

function on_exit {
    rm -rf -- "$tmpdir"
}
trap on_exit EXIT

RUST_LOG=detcore=trace "$hermit" --log-file="$log1".log run --bind="$tmpdir" --base-env=minimal --chaos \
  --record-preemptions-to="$log1".trace \
  -- bash -c 'find ./hermetic_infra/hermit/hermit-cli/src'

RUST_LOG=detcore=trace "$hermit" --log-file="$log2".log run --bind="$tmpdir" --base-env=minimal \
  --replay-schedule-from="$log1".trace \
  --record-preemptions-to="$log2".trace \
  -- bash -c 'find ./hermetic_infra/hermit/hermit-cli/src'

wc "$log1".log "$log2".log "$log1".trace "$log2".trace

grep "Trace loaded" "$log2".log

if grep -s DESYNC "$log2".log; then
    echo "ERROR: Found DESYNC event on trace replay!"
    exit 1
fi

grep "finish syscall" "$log1".log | sed 's/.*detcore://' > "${log1}.syscalls"
grep "finish syscall" "$log2".log | sed 's/.*detcore://' > "${log2}.syscalls"

echo ":: Syscall counts:"
wc -l "${log1}.syscalls" "${log2}.syscalls"

if ! diff "${log1}.syscalls" "${log2}.syscalls"; then
    echo ":: ERROR: difference in linear syscall completion histories between trace/replay runs."
    exit 1
fi

echo "Test passed."
