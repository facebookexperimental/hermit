#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

HERMIT_BIN=${HERMIT_BIN:-target/debug/hermit}
EXAMPLE_TIMEOUT=${HERMIT_EXAMPLE_TIMEOUT:-120s}
readonly ROOT_DIR HERMIT_BIN EXAMPLE_TIMEOUT

# Keep documentation and every runnable example visible in this inventory. The
# directory check below makes adding a new example without CI coverage fail.
readonly -a EXPECTED_EXAMPLE_ENTRIES=(
    README.md
    date.sh
    devrand.sh
    race.sh
    rand.py
    timed-progress-bar.py
)

# ============================================
# BUCKET: hosted-fast (ptrace, no PMU/CPUID hardware)
# ============================================
readonly -a HOSTED_FAST_EXAMPLES=(
    date.sh
    devrand.sh
    race.sh
    rand.py
    timed-progress-bar.py
)

function check_example_inventory {
    local expected
    local actual

    expected=$(printf '%s\n' "${EXPECTED_EXAMPLE_ENTRIES[@]}" | LC_ALL=C sort)
    actual=$(find examples -mindepth 1 -maxdepth 1 -type f -printf '%f\n' | LC_ALL=C sort)
    if [[ $actual != "$expected" ]]; then
        echo "examples/ inventory changed; update ci/e2e_commands_bucketed.sh" >&2
        diff -u <(printf '%s\n' "$expected") <(printf '%s\n' "$actual") >&2 || true
        return 1
    fi
}

function run_example {
    local example=$1

    if [[ ! -x examples/$example ]]; then
        echo "example is not executable: examples/$example" >&2
        return 1
    fi

    printf '==> ptrace L2: examples/%s\n' "$example"
    timeout --foreground --kill-after=10s "$EXAMPLE_TIMEOUT" \
        "$HERMIT_BIN" --log=info run \
        --backend ptrace \
        --strict --verify \
        --no-virtualize-cpuid --max-timeslice=disabled \
        -- "./examples/$example"
}

check_example_inventory
if [[ ! -x $HERMIT_BIN ]]; then
    echo "Hermit binary is not executable: $HERMIT_BIN" >&2
    exit 1
fi

for example in "${HOSTED_FAST_EXAMPLES[@]}"; do
    run_example "$example"
done

printf 'PASS: %s/%s executable examples reached ptrace L2\n' \
    "${#HOSTED_FAST_EXAMPLES[@]}" "${#HOSTED_FAST_EXAMPLES[@]}"
