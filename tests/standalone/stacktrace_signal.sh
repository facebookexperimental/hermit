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

# Make sure that we can successfully interrupt the guest:
stderr=$(mktemp)

# Equivalent if we use SIGABRT, SIGINT, etc
SIG=SIGQUIT
"$hermit" --log=info run -u --stacktrace-signal="$SIG" --stacktrace-event=10 --record-preemptions /bin/date 2> "$stderr"

if grep -s "$SIG" "$stderr"; then
    echo "Successfully interrupted the guest with $SIG as expected."
    rm "$stderr"
else
    echo "ERROR: Did not find $SIG as expected in log!"
    exit 1
fi
