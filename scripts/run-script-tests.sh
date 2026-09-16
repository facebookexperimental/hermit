#!/usr/bin/env bash
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
set -euo pipefail

cd "$(dirname "$0")/.."

if ! command -v rust-script >/dev/null 2>&1; then
    echo 'error: rust-script is not installed (cargo install rust-script)' >&2
    exit 1
fi

failed=0
total=0
for f in $(git ls-files '*.rs' ':!:third-party/**' ':!:scripts/lib/**'); do
    # Only standalone entrypoints: a rust-script shebang makes the file runnable
    # on its own, which is what `rust-script --test` requires.
    head -n 1 -- "$f" | grep -q 'rust-script' || continue
    grep -q '#\[cfg(test)\]' -- "$f" || continue

    total=$((total + 1))
    printf 'run-script-tests: %s\n' "$f"
    # Also cover ordinary rust-script outside the prepared validation graph.
    # The prepared dispatcher adds a nested scope of its own; both directories
    # are retained, and each loop iteration starts with the original outer root.
    if ! ./ci/rust-script-bin/run-test-harness "$f" -- rust-script --test "$f"; then
        echo "run-script-tests: FAILED ${f}" >&2
        failed=$((failed + 1))
    fi
done

if [ "$total" -eq 0 ]; then
    # Not a pass. Discovery returning nothing means the shebang or the test-module
    # spelling moved and this gate is now measuring an empty set -- the silent-green
    # shape. Fail loudly instead.
    echo 'error: run-script-tests discovered no test-carrying rust-scripts; expected at least one' >&2
    exit 1
fi

if [ "$failed" -ne 0 ]; then
    echo "run-script-tests: ${failed} of ${total} script test suites failed" >&2
    exit 1
fi

printf 'run-script-tests: OK -- %s script test suites passed\n' "$total"
