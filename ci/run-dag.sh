#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# run-dag.sh — run a Hermit CI validation lane as a safe-ci-dag-runner DAG.
#
# This entrypoint maps the hand-rolled serial/parallel gate structure in
# validate.sh onto the safe-ci-dag-runner scheduler, so each gate runs as an
# independently boxed node with explicit dependencies and resource limits (see
# ci/dag/README.md). The portable GitHub Actions lane uses this path directly;
# validate.sh remains the source of truth for the individual gate commands.
#
# Usage:
#   ci/run-dag.sh <lane> [runner-args...]
#     <lane>            portable | privileged  (selects ci/dag/<lane>.json)
#     runner-args       forwarded verbatim to `safe-ci-dag-runner run`
#                       (e.g. -j 8, --max-mem 32G, --perf-dir ./perf, --cgroups,
#                        -k/--keep-going, -v, -q)
#
# Examples:
#   ci/run-dag.sh portable --max-mem 32G
#   ci/run-dag.sh privileged -j 1 --perf-dir ./perf
#   ci/run-dag.sh portable ascii   # any non-`run` verb also works
#
# Environment:
#   SAFE_CI_DAG_RUNNER   override the runner executable to use.

set -uo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR" || exit 2

if (($# < 1)); then
    echo "usage: ci/run-dag.sh <portable|privileged> [runner-args...]" >&2
    exit 2
fi

lane=$1
shift

dag="$ROOT_DIR/ci/dag/${lane}.json"
if [[ ! -f $dag ]]; then
    echo "run-dag.sh: unknown lane '$lane' (no such file: $dag)" >&2
    echo "            known lanes: portable, privileged" >&2
    exit 2
fi

# Locate the runner. Prefer an explicit override, then the compiled Rust binary
# (fast, dependency-free), then the Python entrypoint (reference behavior; the
# only implementation with Linux cgroup boxing + perf logging in 0.1).
find_runner() {
    if [[ -n ${SAFE_CI_DAG_RUNNER:-} ]]; then
        printf '%s\n' "$SAFE_CI_DAG_RUNNER"
        return 0
    fi
    local base="$ROOT_DIR/agent-utils"
    if [[ -x "$base/rs/bin/safe-ci-dag-runner" ]]; then
        printf '%s\n' "$base/rs/bin/safe-ci-dag-runner"
        return 0
    fi
    if [[ -x "$base/py/bin/safe-ci-dag-runner" ]]; then
        printf '%s\n' "$base/py/bin/safe-ci-dag-runner"
        return 0
    fi
    if command -v safe-ci-dag-runner >/dev/null 2>&1; then
        command -v safe-ci-dag-runner
        return 0
    fi
    return 1
}

runner=$(find_runner) || {
    echo "run-dag.sh: safe-ci-dag-runner not found." >&2
    echo "            Build it with: (cd agent-utils && ./setup) or set SAFE_CI_DAG_RUNNER." >&2
    exit 2
}

# A leading non-`run` verb (list/ascii/dot/json) is passed straight through; the
# common case is `run` with scheduling flags.
verb=run
if (($# > 0)) && [[ $1 == list || $1 == ascii || $1 == dot || $1 == json ]]; then
    verb=$1
    shift
fi

echo "run-dag.sh: lane=$lane runner=$runner verb=$verb" >&2
exec "$runner" "$verb" --dag "$dag" "$@"
