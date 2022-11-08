#!/bin/bash
# (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

set -xeuo pipefail

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

RUST_LOG=detcore=trace "$hermit" --log-file="$log1".log run --bind="$tmpdir" --base-env=minimal \
  --record-preemptions-to="$log1".trace \
  -- bash -c 'find ./hermetic_infra/hermit/src'

RUST_LOG=detcore=trace "$hermit" --log-file="$log2".log run --bind="$tmpdir" --base-env=minimal \
  --replay-schedule-from="$log1".trace \
  --record-preemptions-to="$log2".trace \
  -- bash -c 'find ./hermetic_infra/hermit/src'

wc "$log1".log "$log2".log "$log1".trace "$log2".trace

grep "Trace loaded" "$log2".log

if grep -s DESYNC "$log2".log; then
    echo "ERROR: Found DESYNC event on trace replay!"
    exit 1
fi

"$hermit" log-diff "$log1".log "$log2".log

echo "Test passed."
