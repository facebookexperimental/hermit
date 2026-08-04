#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail

ROOT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
SHELL_CLASSIFIER="$ROOT_DIR/scripts/classify-required-check.sh"
PYTHON_CLASSIFIER="$ROOT_DIR/agent-utils/py/ci_hub_check_outcome.py"

check() {
    local expected=$1 status=$2 conclusion=$3 python_result shell_result
    python_result=$("$PYTHON_CLASSIFIER" --status "$status" --conclusion "$conclusion")
    shell_result=$("$SHELL_CLASSIFIER" "$status" "$conclusion")
    [[ $python_result == "$expected" && $shell_result == "$expected" ]] || {
        echo "mismatch: $status/$conclusion expected=$expected python=$python_result shell=$shell_result" >&2
        exit 1
    }
}

check PASSED completed success
check PASSED "" success
for conclusion in failure timed_out error startup_failure; do
    check FAILED completed "$conclusion"
done
while IFS=: read -r status conclusion; do
    check NO_RESULT "$status" "$conclusion"
done <<'EOF'
completed:cancelled
completed:skipped
completed:neutral
completed:stale
completed:action_required
queued:
in_progress:
waiting:
requested:
pending:
missing:
completed:future_state
EOF

fixture='[{"statusCheckRollup":[{"status":"COMPLETED","conclusion":"CANCELLED"},{"state":"SUCCESS"}]}]'
annotated=$(printf '%s' "$fixture" | "$PYTHON_CLASSIFIER" --annotate-rollups)
[[ $(jq -r '.[0].statusCheckRollup[0]._checkOutcome' <<<"$annotated") == NO_RESULT ]]
[[ $(jq -r '.[0].statusCheckRollup[1]._checkOutcome' <<<"$annotated") == PASSED ]]

# Plant the #1597 shape: two opposite gate conclusions at one exact head.
# Both input orders must select the later run, and a different-head run must
# never enter the verdict.
head_sha=01e5653f2a59fdf5ce090c12aa45e944f7237c3f
older='{"name":"merge-gate","headSha":"'$head_sha'","status":"COMPLETED","conclusion":"FAILURE","startedAt":"2026-08-04T15:12:05Z","detailsUrl":"https://github.com/o/r/actions/runs/30922888575/job/1"}'
newer='{"name":"merge-gate","headSha":"'$head_sha'","status":"COMPLETED","conclusion":"SUCCESS","startedAt":"2026-08-04T15:24:36Z","detailsUrl":"https://github.com/o/r/actions/runs/30923975433/job/2"}'
wrong_head='{"name":"merge-gate","headSha":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","status":"COMPLETED","conclusion":"FAILURE","startedAt":"2026-08-04T15:25:00Z","detailsUrl":"https://github.com/o/r/actions/runs/30924000000/job/3"}'
for rollup in "[$older,$newer,$wrong_head]" "[$wrong_head,$newer,$older]"; do
    selected=$(printf '%s' "$rollup" | "$PYTHON_CLASSIFIER" \
        --select-latest-rollup --head-sha "$head_sha")
    [[ $(jq 'length' <<<"$selected") -eq 1 ]]
    [[ $(jq -r '.[0].conclusion' <<<"$selected") == SUCCESS ]]
done

echo "PASS: one authority handles N=2 passed, N=4 failed, N=12 no-result and exact-head/latest rollups"
