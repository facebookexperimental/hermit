#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

#
# Run the unit tests carried by rust-script entrypoints.
#
# rust-scripts are not Cargo workspace members, so `cargo test` never sees them.
# `build.rust_scripts` compiles all tracked entrypoints and their test harnesses
# before this graph consumer runs. This script executes those harnesses; outside
# the graph, rust-script retains its ordinary compile-and-run behavior. Measured
# 2026-08-25 at hermit main a5fef7ff7623: seven entrypoints carried 84 unit tests
# between them and no target anywhere passed `--test`, so all 84 were documentation.
# They all passed once run, so this gate starts green.
#
# The file list is DISCOVERED, not hard-coded: a new script that grows a test
# module is picked up with no edit here. A hard-coded list would reproduce exactly
# the "someone has to remember" failure this gate was added to close.
#
# The harnesses are independent processes, so up to HERMIT_SCRIPT_TEST_JOBS of
# them run at once (the CI node check.script_unit_tests sets it to its admitted
# width; otherwise every online CPU). Each harness's stdout and stderr are
# buffered separately and printed whole, to the same streams, as soon as that
# harness finishes, so a node stopped by its wall bound still shows every
# harness that completed. Measured serially at d44bbbb79acd, unthrottled: 362 s,
# of which ci/compat-envelope/scorecard.rs took 162 s and scripts/validate.rs
# 131 s.
set -euo pipefail

cd "$(dirname "$0")/.."

if ! command -v rust-script >/dev/null 2>&1; then
    echo 'error: rust-script is not installed (cargo install rust-script)' >&2
    exit 1
fi

jobs=${HERMIT_SCRIPT_TEST_JOBS:-$(nproc)}
if ! [[ $jobs =~ ^[1-9][0-9]*$ ]]; then
    echo "error: HERMIT_SCRIPT_TEST_JOBS must be a positive integer, got '${jobs}'" >&2
    exit 2
fi

files=()
for f in $(git ls-files '*.rs' ':!:third-party/**' ':!:scripts/lib/**'); do
    # Only standalone entrypoints: a rust-script shebang makes the file runnable
    # on its own, which is what `rust-script --test` requires.
    head -n 1 -- "$f" | grep -q 'rust-script' || continue
    grep -q '#\[cfg(test)\]' -- "$f" || continue
    files+=("$f")
done

total=${#files[@]}
if [ "$total" -eq 0 ]; then
    # Not a pass. Discovery returning nothing means the shebang or the test-module
    # spelling moved and this gate is now measuring an empty set -- the silent-green
    # shape. Fail loudly instead.
    echo 'error: run-script-tests discovered no test-carrying rust-scripts; expected at least one' >&2
    exit 1
fi

out=$(mktemp -d)
trap 'rm -rf "$out"' EXIT

run_one() {
    local index=$1 file=$2 status=0
    # Also cover ordinary rust-script outside the prepared validation graph.
    # The prepared dispatcher adds a nested scope of its own; both directories
    # are retained, and each harness starts with the original outer root.
    ./ci/rust-script-bin/run-test-harness "$file" -- rust-script --test "$file" \
        >"$out/$index.stdout" 2>"$out/$index.stderr" || status=$?
    printf '%s\n' "$status" >"$out/$index.status"
    {
        flock 9
        printf 'run-script-tests: %s\n' "$file"
        cat -- "$out/$index.stdout"
        cat -- "$out/$index.stderr" >&2
        if [ "$status" != 0 ]; then
            echo "run-script-tests: FAILED ${file} (status ${status})" >&2
        fi
    } 9>"$out/print.lock"
}

running=0
for index in "${!files[@]}"; do
    run_one "$index" "${files[$index]}" &
    running=$((running + 1))
    if [ "$running" -ge "$jobs" ]; then
        wait -n
        running=$((running - 1))
    fi
done
wait

failed=0
for index in "${!files[@]}"; do
    # A missing status file means the harness wrapper itself did not complete;
    # that is a failure, never a pass.
    status=$(cat -- "$out/$index.status" 2>/dev/null || echo missing)
    if [ "$status" != 0 ]; then
        if [ "$status" = missing ]; then
            echo "run-script-tests: FAILED ${files[$index]} (no status recorded)" >&2
        fi
        failed=$((failed + 1))
    fi
done

if [ "$failed" -ne 0 ]; then
    echo "run-script-tests: ${failed} of ${total} script test suites failed" >&2
    exit 1
fi

printf 'run-script-tests: OK -- %s script test suites passed\n' "$total"
