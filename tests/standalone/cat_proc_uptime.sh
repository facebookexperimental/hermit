#!/bin/bash
# (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

set -u

if [ "$*" == "" ]; then
    hermit="hermit"
else
    hermit="$1"
fi

"$hermit" run --verify --no-sequentialize-threads --no-deterministic-io -- bash -c 'echo hello; cat /proc/uptime; cat /proc/uptime; cat /proc/uptime'
res=$?

if [ "$res" == 0 ]; then
    echo "Error!  Zero exit code where differences expected."
    exit 1
else
    echo "Differences found, as expected."
    exit 0
fi
