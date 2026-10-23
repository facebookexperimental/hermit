#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
set -euo pipefail

case ${1:-} in
    --prepare) test -x /usr/bin/python3 ;;
    --run)
        # CPython randomizes str/bytes hashing per process (SipHash keyed from
        # the AT_RANDOM/getrandom entropy the kernel supplies at exec). That
        # makes set iteration order and hash() values vary between native runs.
        # Unset PYTHONHASHSEED so randomization is guaranteed active regardless
        # of the ambient environment; Hermit still determinizes the underlying
        # randomness source, so every Hermit run is byte-identical.
        unset PYTHONHASHSEED
        # Use /usr/bin/python3 explicitly (matching the --prepare gate) so the
        # workload is the light system interpreter, not a heavier PATH build.
        exec /usr/bin/python3 -c '
words = ["alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel"]
bag = set(words)
print("ITER " + ",".join(bag))
print("HASH " + str(hash("alpha")))
print("SUM " + str(sum(hash(w) for w in words)))
'
        ;;
    *) echo "usage: $0 --prepare|--run" >&2; exit 2 ;;
esac
