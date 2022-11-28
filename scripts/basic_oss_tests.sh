#!/bin/bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# The full hermit testing setup is Buck-based, and is not working in the OSS release yet.
# This extremely basic test harness is a stop-gap.

set -eEuo pipefail

rootdir="$(pwd)"
hermit="$rootdir/target/debug/hermit"
hverify="$rootdir/target/debug/hermit-verify"

function hermit_verify {
    "$hverify" --hermit-bin="$hermit" run --isolate-workdir \
               --hermit-arg="--base-env=empty" --hermit-arg="--env=HERMIT_MODE=strict" --hermit-arg="--preemption-timeout=80000000" \
               "$1"
}

for script in "$rootdir"/tests/shell/*.sh; do
    echo; echo "Running shell test: $script"
#    hermit_verify "$script"
done

for bin in $(ls "$rootdir"/target/debug/rustbin_* | grep -v '\.d'); do
    echo; echo "Running rust test: $bin"
    hermit_verify "$bin"
done
