#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# Ruby's Random default seed is drawn from the kernel CSPRNG (getrandom(2)) at
# interpreter startup, so bare `rand` output varies natively but is determinized
# by Hermit. The deterministic arithmetic, string sort, and length checks
# confirm interpreter semantics are preserved under syscall interception. The
# program is passed inline via `ruby -e` so it runs as a single process with no
# temporary file (Hermit isolates /tmp per repeat).
set -euo pipefail

prog='a=rand(1000000); b=rand(1000000); c=rand(1000000); s=(1..100).map{|i| i*i}.sum; w=%w[hermit determinism ruby].sort.join("-"); puts "rand=#{a},#{b},#{c} sumsq=#{s} sorted=#{w} len=#{"abcdefghij".length}"'

case ${1:-} in
    # Say WHY on failure. A bare `command -v ruby >/dev/null` exits nonzero under
    # `set -e` having printed nothing, and the harness then reports only
    # "prepare failed for language-runtimes/ruby-random.sh" with no cause —
    # unattributable from the CI log (hermit #1711).
    --prepare)
        command -v ruby >/dev/null || {
            echo "ruby not found on PATH" >&2
            exit 1
        }
        ;;
    --run) exec ruby --disable-gems -e "$prog" ;;
    *) echo "usage: $0 --prepare|--run" >&2; exit 2 ;;
esac
