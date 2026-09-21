#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# run-dag.sh — run a Hermit CI validation lane as a dagrun DAG.
#
# This entrypoint is the shared local/GitHub execution path for the centralized
# validation profiles. Each gate is an independently boxed node with explicit
# dependencies and resource limits (see ci/dag/README.md). Every standard
# profile selects a labelled subset of the same committed DAG; launch never
# rewrites commands, dependencies, or resource policy.
#
# Usage:
#   ci/run-dag.sh <label> [runner-args...]
#     <label>           quick | portable | full | super | privileged
#                       (selects labelled steps from ci/dag/validate.json)
#     runner-args       allowlisted non-selection controls forwarded to `dagrun run`
#                       (e.g. -j 8, --max-mem 32G, --perf-dir ./perf,
#                        -k/--keep-going, -v, -q)
#                       graph, label, selected-step, command, stress, and
#                       resource-policy overrides are refused
#
# Examples:
#   ci/run-dag.sh portable --max-mem 32G
#   ci/run-dag.sh privileged -j 1 --perf-dir ./perf
#   agent-utils/py/bin/dagrun ascii --dag ci/dag/validate.json
#
# Environment:
#   DAGRUN_BIN     override the runner executable to use.

set -uo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR" || exit 2

# shellcheck source=ci/configure-build-jobs.sh
source "$ROOT_DIR/ci/configure-build-jobs.sh" launcher || exit $?

if (($# < 1)); then
    echo "usage: ci/run-dag.sh <quick|portable|full|super|privileged> [runner-args...]" >&2
    exit 2
fi

lane=$1
shift

if [[ -n ${RUN_DAG_FILE_OVERRIDE:-} ]]; then
    echo "run-dag.sh: RUN_DAG_FILE_OVERRIDE was removed; runtime validation accepts only ci/dag/validate.json" >&2
    exit 2
fi
dag="$ROOT_DIR/ci/dag/validate.json"
if [[ ! $lane =~ ^(quick|portable|full|super|privileged)$ ]]; then
    echo "run-dag.sh: unknown validation label '$lane'" >&2
    echo "            known labels: quick, portable, full, super, privileged" >&2
    exit 2
fi
case $lane in
    portable) selection_label=hosted-portable ;;
    privileged) selection_label=hosted-privileged ;;
    *) selection_label=$lane ;;
esac

# The wrapper, not its caller, owns both graph identity and label selection.
# dagrun accepts repeated options with the final value winning, so merely
# placing these options first would let a trailing caller argument replace the
# committed DAG or select a different profile. Forward only runner controls
# that cannot change graph contents or the selected node population.
validate_runner_args() {
    local arg
    while (($# > 0)); do
        arg=$1
        shift
        case "$arg" in
            --dag|--dag=*|--labels|--labels=*|--selected|--selected=*|\
            --ignore-selected-deps|--ignore-selected-deps=*|--args|--args=*|\
            --stress|--stress=*|--resource-caps-path|--resource-caps-path=*|\
            --small-default-cap|--small-default-cap=*)
                echo "run-dag.sh: refusing caller graph/selection override '$arg'; this entry point owns --dag and --labels" >&2
                return 2
                ;;
            -s|-j|--max-steps|--max-cpus|--jobs|--cores|--cpuset|--pin|\
            --max-mem|--perf-dir|--profile-timeseries|--planner|--profile-sync|\
            --profile-sync-direction|--run-timeout|--cpu-timeout-multiplier)
                if (($# == 0)); then
                    echo "run-dag.sh: runner option '$arg' requires a value" >&2
                    return 2
                fi
                shift
                ;;
            --max-steps=*|--max-cpus=*|--jobs=*|--cores=*|--cpuset=*|--pin=*|\
            --max-mem=*|--perf-dir=*|--profile-timeseries=*|--planner=*|\
            --profile-sync=*|--profile-sync-direction=*|--run-timeout=*|\
            --cpu-timeout-multiplier=*|-s?*|-j?*)
                ;;
            --admission)
                # dagrun's admission wait is optional. It consumes the next
                # token only when that token is a nonnegative value rather
                # than another flag; dagrun remains responsible for validating
                # the number and its range.
                if (($# > 0)) && [[ $1 != -* ]]; then
                    shift
                fi
                ;;
            --admission=*|--no-profile|--profile|--show-plan|\
            --no-profile-feedback|--profile-memory-feedback|-k|--keep-going|\
            --no-color|--allow-cgroup-failure|--unsafe-no-cgroups|\
            --allow-unwise-nest-dagruns|-v|-q|--quiet)
                ;;
            *)
                echo "run-dag.sh: unsupported runner argument '$arg'; pass only documented non-selection run controls" >&2
                return 2
                ;;
        esac
    done
}

validate_runner_args "$@" || exit $?

# Locate the runner. Prefer an explicit override, then the TRACKED, source-invoked
# engine resolver (agent-utils/common/bin/dagrun -> engine-resolver),
# then the tracked, source-invoked Python entrypoint. NEVER auto-select the
# untracked prebuilt Rust binary (rs/bin): a compiled artifact can silently drift
# from its source, which is exactly how a runner missing an enforcement guard (the
# historical cpu_timeout gap) can run while we believe we are boxed.
#
# The staleness axis is SOURCE-INVOKED vs PREBUILT-BINARY, not Rust vs Python.
# This entrypoint selects the tracked Rust engine by default because Hermit's
# committed DAG declares structured test results and the Python scheduler
# deliberately refuses that execution contract. An explicit DAGRUN_ENGINE or
# DAGRUN_BIN remains diagnostic override surface; either engine still logs its
# exact selection and never silently falls back.
find_runner() {
    if [[ -n ${DAGRUN_BIN:-} ]]; then
        printf '%s\n' "$DAGRUN_BIN"
        return 0
    fi
    local base="$ROOT_DIR/agent-utils"
    # Tracked, source-invoked resolver: deterministic engine selection that logs
    # which engine won. Preferred over any prebuilt binary.
    if [[ -x "$base/common/bin/dagrun" ]]; then
        printf '%s\n' "$base/common/bin/dagrun"
        return 0
    fi
    # Fallback: the tracked, source-invoked Python entrypoint directly.
    if [[ -x "$base/py/bin/dagrun" ]]; then
        printf '%s\n' "$base/py/bin/dagrun"
        return 0
    fi
    # Last resort: a resolver/runner already on PATH.
    if command -v dagrun >/dev/null 2>&1; then
        command -v dagrun
        return 0
    fi
    return 1
}

runner=$(find_runner) || {
    echo "run-dag.sh: dagrun not found." >&2
    echo "            Build it with: (cd agent-utils && ./setup) or set DAGRUN_BIN." >&2
    exit 2
}

if [[ -z ${DAGRUN_BIN:-} && -z ${DAGRUN_ENGINE:-} ]]; then
    export DAGRUN_ENGINE=rust
fi

# A leading non-`run` verb (list/ascii/dot/json) is passed straight through; the
# common case is `run` with scheduling flags.
verb=run
if (($# > 0)) && [[ $1 == list || $1 == ascii || $1 == dot || $1 == json ]]; then
    verb=$1
    shift
fi

if [[ $verb != run ]]; then
    echo "run-dag.sh: '$verb' cannot represent the '$lane' label selection." >&2
    echo "            Inspect the committed superset directly: $runner $verb --dag $dag" >&2
    exit 2
fi

echo "run-dag.sh: lane=$lane selection-label=$selection_label runner=$runner verb=$verb cargo-jobs=$CARGO_BUILD_JOBS reverie-dbt-budget=portable-build-child-only" >&2
if [[ $verb == run ]]; then
    export HERMIT_REAL_RUST_SCRIPT
    HERMIT_REAL_RUST_SCRIPT=$(command -v rust-script) || {
        echo "run-dag.sh: rust-script is required" >&2
        exit 2
    }
    export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$ROOT_DIR/target/ci/rust-scripts"
    export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1
    export PATH="$ROOT_DIR/ci/rust-script-bin:$PATH"
fi
if [[ $verb == run ]]; then
    if [[ -n ${VALIDATE_RUN_STATE:-} ]]; then
        echo "run-dag.sh: refusing inherited VALIDATE_RUN_STATE; every top-level run owns a unique state directory" >&2
        exit 2
    fi
    VALIDATE_RUN_STATE="$ROOT_DIR/target/validation/run-dag-${lane}-$$-$(date +%s%N)"
    export VALIDATE_RUN_STATE
    export E2E_RESULT_ROOT=${E2E_RESULT_ROOT:-"$VALIDATE_RUN_STATE/results"}
    export E2E_BUILD_ROOT=${E2E_BUILD_ROOT:-"$VALIDATE_RUN_STATE/build"}
    mkdir -p "$VALIDATE_RUN_STATE" "$E2E_RESULT_ROOT" "$E2E_BUILD_ROOT" || exit 2
    exec "$runner" "$verb" --dag "$dag" --labels "$selection_label" "$@"
fi
exec "$runner" "$verb" --dag "$dag" "$@"
