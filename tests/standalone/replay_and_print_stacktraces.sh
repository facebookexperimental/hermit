#!/bin/bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -xeuo pipefail

if [ "$*" == "" ]; then
    hermit="hermit"
else
    hermit="$1"
fi

tmpdir=$(mktemp -d)
log1=$(mktemp -p "$tmpdir")
log2=$(mktemp -p "$tmpdir")

# Hermetic guest input: du a fixed tree in $tmpdir instead of the live host
# /usr/bin, whose directory metadata (nlink, entry sizes) changes between the
# sequential record and replay runs and desynchronizes hermit log-diff. The
# tree spans several subdirectories (multiple getdents batches) and includes
# symlinks, like the /usr/bin walk it replaces.
for d in sub1 sub2 sub3 sub4; do
  mkdir -p "$tmpdir/tree/$d"
  for i in $(seq 1 300); do
    printf 'hermit-standalone-fixture-%s-%s\n' "$d" "$i" > "$tmpdir/tree/$d/file$i"
  done
done
ln -s sub1/file1 "$tmpdir/tree/link1"
ln -s ../sub2/file2 "$tmpdir/tree/sub1/link2"

function on_exit {
    rm -rf -- "$tmpdir"
}
# trap on_exit EXIT

# Each run gets an equivalent fresh stdout sink under $tmpdir so the guest's
# fd 1 stat (size/blocks) starts identical; the caller's stdout wiring no
# longer reaches the compared DETLOG records.
RUST_LOG=detcore=trace "$hermit" --log-file="$log1".log run --bind="$tmpdir" --base-env=minimal \
  --record-preemptions-to="$log1".trace \
  -- bash -c 'du -sch "$1"' _ "$tmpdir/tree" > "$tmpdir/out1"

stack1=$(mktemp -p "$tmpdir")
stack2=$(mktemp -p "$tmpdir")

# Print a couple arbitrary points inside this particular guest program.
RUST_LOG=detcore=trace "$hermit" --log-file="$log2".log run --bind="$tmpdir" --base-env=minimal \
  --replay-schedule-from="$log1".trace \
  --stacktrace-event=10,"$stack1" --stacktrace-event=26,"$stack2" \
  --record-preemptions-to="$log2".trace \
  -- bash -c 'du -sch "$1"' _ "$tmpdir/tree" > "$tmpdir/out2"

wc "$log1".log "$log2".log "$log1".trace "$log2".trace

grep "Trace loaded" "$log2".log

if grep -s DESYNC "$log2".log; then
    echo "ERROR: Found DESYNC event on trace replay!"
    exit 1
fi

"$hermit" log-diff "$log1".log "$log2".log

event1_ip=$(jq -r ".frames[0].frame.ip" "$stack1")
event2_ip=$(jq -r ".frames[0].frame.ip" "$stack2")

if [ "$event1_ip" -eq "$event2_ip" ]; then
  echo "ERROR: event stacktraces point to the same instruction";
  exit 1
fi

echo "Test passed."
