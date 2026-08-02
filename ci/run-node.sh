#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# run-node.sh — run one or more named DAG nodes' commands straight from
# ci/dag/<lane>.json, WITHOUT running their dependencies.
#
# WHY: the parallel GitHub fan-out (ci-portable-parallel.yml) shards the portable
# lane across many small jobs. Each shard must execute an exact subset of the DAG
# against a prebuilt tree / restored cache produced by an upstream build job.
# The pinned safe-ci-dag-runner (agent-utils) predates its `run --only` node
# selector, so this shim provides the same "run exactly these nodes, inputs
# assumed present" behavior using only jq — keeping ci/dag/<lane>.json the single
# source of truth for every step's command (no hand-copied cargo lines to drift).
#
# Usage:
#   ci/run-node.sh <lane> <group.job>[,<group.job>...]
#     <lane>   portable | privileged  (selects ci/dag/<lane>.json)
#     nodes    one or more "group.job" keys, comma-separated; run in listed order,
#              stopping at the first failure. Dependencies are NOT run.
#
# Example:
#   ci/run-node.sh portable test.hermit_unit,test.detcore_unit
set -uo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR" || exit 2

lane=${1:-}
sel=${2:-}
if [[ -z $lane || -z $sel ]]; then
    echo "usage: ci/run-node.sh <lane> <group.job>[,<group.job>...]" >&2
    exit 2
fi

dag="$ROOT_DIR/ci/dag/${lane}.json"
if [[ ! -f $dag ]]; then
    echo "run-node.sh: unknown lane '$lane' (no such file: $dag)" >&2
    exit 2
fi
command -v jq >/dev/null 2>&1 || { echo "run-node.sh: jq is required" >&2; exit 2; }

IFS=',' read -r -a nodes <<<"$sel"
rc=0
for key in "${nodes[@]}"; do
    [[ -n $key ]] || continue
    cmd=$(jq -r --arg k "$key" '
        [ .steps[] | select("\(.group).\(.job)" == $k) ] as $m
        | if ($m | length) == 1 then $m[0].cmd else "" end
    ' "$dag")
    if [[ -z $cmd || $cmd == "null" ]]; then
        echo "run-node.sh: node '$key' not found (or ambiguous) in $dag" >&2
        exit 2
    fi
    echo "::group::run-node ${lane}:${key}"
    echo "run-node.sh: [${key}] ${cmd}" >&2
    bash -o pipefail -c "$cmd"
    node_rc=$?
    echo "::endgroup::"
    if ((node_rc != 0)); then
        echo "run-node.sh: node '${key}' FAILED (rc=${node_rc})" >&2
        rc=$node_rc
        break
    fi
done
exit "$rc"
