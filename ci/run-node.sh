#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# run-node.sh — execute exact committed validation-DAG nodes without their
# externally supplied dependencies.
#
# Hosted jobs restore build outputs before invoking this entry point. The
# selected nodes still execute through scripts/validate.rs and dagrun, retaining
# their committed wall, CPU, memory, dependency, and result-ownership policy.
# This script never edits the committed DAG. To iterate on a different command,
# invoke that command directly; the result is not evidence for the DAG node.
#
# Usage:
#   ci/run-node.sh <lane> <group.job>[,<group.job>...]
#     <lane>   portable | privileged
#     nodes    exact comma-separated tags selected from the matching hosted
#              graph. Dependencies outside the selection are intentionally
#              omitted because hosted build jobs provide their outputs.
#
# Environment:
#   RUN_NODE_JOBS        optional outer scheduler width.
#   RUN_NODE_PRINT_ONLY  print the selected plan without execution.

set -uo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR" || exit 2

# shellcheck source=ci/configure-build-jobs.sh
source "$ROOT_DIR/ci/configure-build-jobs.sh" launcher || exit $?

usage() {
    echo "usage: ci/run-node.sh <portable|privileged> <group.job>[,<group.job>...]" >&2
}

lane=${1:-}
sel=${2:-}
if [[ -z $lane || -z $sel ]]; then
    usage
    exit 2
fi
shift 2

if (($# > 0)); then
    echo "run-node.sh: trailing command replacement arguments were removed; the runtime accepts only ci/dag/validate.json" >&2
    echo "             invoke a modified command directly for local iteration; it is not validation evidence" >&2
    usage
    exit 2
fi

case "$lane" in
    portable) profile=(--hosted-portable-only) ;;
    privileged) profile=(--hosted-privileged-only) ;;
    *)
        echo "run-node.sh: unknown lane '$lane' (expected portable or privileged)" >&2
        exit 2
        ;;
esac

extra=(--allow-local-off-the-record-run --selected "$sel" \
    --ignore-selected-deps --no-label-pr --verbose)
scheduler_width=validate-default
if [[ -n ${RUN_NODE_JOBS:-} ]]; then
    extra+=(-j "$RUN_NODE_JOBS")
    scheduler_width=-j$RUN_NODE_JOBS
fi
if [[ -n ${GITHUB_ACTIONS:-} || -n ${CI:-} ]]; then
    extra+=(--allow-cgroup-failure \
        --skip-inner-dirty-working-tree-and-rebase-freshness-checks)
fi
if [[ -n ${RUN_NODE_PRINT_ONLY:-} ]]; then
    extra+=(--show-plan)
fi

echo "run-node.sh: lane=$lane nodes=$sel scheduler-width=$scheduler_width via the committed hosted selection" >&2
exec ./scripts/validate.rs "${profile[@]}" "${extra[@]}"
