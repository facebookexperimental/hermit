#!/usr/bin/env bash
# Compile the workspace in the DEFAULT feature configuration with warnings
# denied -- the exact configuration CI lints in, and the one local testing skips.
#
# WHY THIS EXISTS. `cargo test` does not set `-D warnings`, but every workflow
# does (`ci-dag.yml`, `ci-portable.yml`, `ci-privileged.yml`, `demo-hot-path.yml`,
# `docs.yml`, `validation-levels.yml` all set `RUSTFLAGS: -D warnings ...`). So a
# reviewer can run the tests, see green, and certify a change that cannot compile
# in CI. That is not hypothetical: rrnewton/hermit#2359 passed two independent
# review lanes and broke `main`, because an ungated
# `use super::verify::write_pending_verification_json` in
# `hermit-cli/src/bin/hermit/backends.rs` had its only consumer inside
# `#[cfg(feature = "dbt")]`, and `dbt` is not a default feature. The repair was
# https://github.com/rrnewton/hermit/pull/2381. The author's own builds missed it
# too, because work on a backend is normally done with that backend's feature
# enabled -- the break only appears in the DEFAULT configuration.
#
# The command below is deliberately IDENTICAL to the `lint.clippy` node in
# `ci/dag/validate.json`, minus that node's `CARGO_BUILD_JOBS=8` memory pin,
# which exists for CI's cgroup cap and only makes a developer wait longer. Do not
# substitute a narrower command: a gate that lints something other than what CI
# lints can go green while CI goes red, which is the failure this closes.
#
# COST, measured 2026-08-24 on a 316-core x86_64 Linux build host, green tree:
#   warm, nothing touched .................... 0.3s
#   warm, one source file touched ............ 1.0s
#   cold, empty CARGO_TARGET_DIR ............. 25s
# The warm numbers are what a push actually pays and are dominated by
# recompiling the touched crates, not by core count. The cold number is a
# once-per-target-directory cost and scales with the machine.

set -euo pipefail

ROOT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
readonly ROOT_DIR

if ! command -v cargo >/dev/null 2>&1; then
    echo "check-default-build-warnings: cargo not on PATH; skipping." >&2
    echo "  This is a skip, NOT a pass: nothing was checked." >&2
    exit 0
fi

cd "$ROOT_DIR"

if [[ ${1-} == "--quiet" ]]; then
    output=$(mktemp)
    trap 'rm -f "$output"' EXIT
    # Capture the status with `|| status=$?` and NOT with `if cargo ...; then`.
    # An `if` whose condition fails and which has no `else` branch is itself a
    # successful compound command, so a `status=$?` placed after its `fi` reads
    # 0 and this gate reports a pass on a tree that does not compile. That was
    # the first draft of this script, and the old-fails proof caught it: on
    # 888e8fc506 it printed the unused-import error and still exited 0. It is
    # the same swallowed-exit-status family that has bitten this project through
    # pipelines; keep the assignment explicit.
    status=0
    cargo clippy --workspace --all-targets -- -D warnings >"$output" 2>&1 || status=$?
    if [[ $status -ne 0 ]]; then
        cat "$output" >&2
    fi
    exit "$status"
fi

exec cargo clippy --workspace --all-targets -- -D warnings
