#!/bin/bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

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
