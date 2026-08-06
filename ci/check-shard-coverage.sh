#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# check-shard-coverage.sh — fail-closed correspondence guard for the parallel
# portable fan-out. Asserts that ci/portable-shards.json assigns EVERY portable
# DAG node to exactly one job, with no overlap and no unknown node names:
#
#   union(preflight, build_debug, build_dbt, build_aux, debug_shards, release_shards)
#     ==  { portable.json nodes } minus { e2e.manifest_* }
#
# The 13 e2e.manifest_* nodes are intentionally excluded here: they are covered by
# the audited e2e (category x backend) matrix (ci/test_harness.sh plan), exactly
# as the pre-existing ci-portable-fanout.yml already validates. This guard makes
# it impossible for the parallel workflow to silently cover a different set than
# the trusted portable DAG.
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

dag="ci/dag/portable.json"
shards="ci/portable-shards.json"
command -v jq >/dev/null 2>&1 || { echo "check-shard-coverage.sh: jq is required" >&2; exit 2; }
[[ -f $dag ]] || { echo "check-shard-coverage.sh: missing $dag" >&2; exit 2; }
[[ -f $shards ]] || { echo "check-shard-coverage.sh: missing $shards" >&2; exit 2; }

# All portable nodes except the e2e.manifest_* cells (covered by the e2e matrix).
mapfile -t expected < <(
    jq -r '.steps[] | "\(.group).\(.job)"
           | select(startswith("e2e.manifest_") | not)' "$dag" | sort -u
)

# Every node assigned by the shard map, across all job buckets.
mapfile -t assigned < <(
    jq -r '
        (.preflight_nodes // [])
      + (.build_debug_nodes // [])
      + (.build_dbt_nodes // [])
      + (.build_aux_nodes // [])
      + ([ (.debug_shards // [])[]   | .nodes[] ])
      + ([ (.release_shards // [])[] | .nodes[] ])
        | .[]
    ' "$shards" | sort
)

# Duplicate assignment (a node in two buckets) is a defect.
dupes=$(printf '%s\n' "${assigned[@]}" | uniq -d || true)
if [[ -n $dupes ]]; then
    echo "check-shard-coverage.sh: FAIL — node(s) assigned to more than one job:" >&2
    printf '  %s\n' $dupes >&2
    exit 1
fi

assigned_unique=$(printf '%s\n' "${assigned[@]}" | sort -u)
expected_list=$(printf '%s\n' "${expected[@]}")

missing=$(comm -23 <(printf '%s\n' "$expected_list") <(printf '%s\n' "$assigned_unique") || true)
extra=$(comm -13 <(printf '%s\n' "$expected_list") <(printf '%s\n' "$assigned_unique") || true)

status=0
if [[ -n $missing ]]; then
    echo "check-shard-coverage.sh: FAIL — portable nodes NOT assigned to any job:" >&2
    printf '  %s\n' $missing >&2
    status=1
fi
if [[ -n $extra ]]; then
    echo "check-shard-coverage.sh: FAIL — shard map names nodes absent from portable.json (or e2e.manifest_*):" >&2
    printf '  %s\n' $extra >&2
    status=1
fi

if ((status == 0)); then
    n=$(printf '%s\n' "$assigned_unique" | grep -c . || true)
    echo "check-shard-coverage.sh: OK — $n non-e2e portable nodes each assigned to exactly one job; e2e.manifest_* covered by the e2e matrix."
fi
exit "$status"
