#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Pin the workflow wiring around the tested trinary predicate. The exhaustive
# state test proves the predicate; this lint proves every gate leg uses it.
set -euo pipefail

ROOT_DIR=${1:-$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)}
WORKFLOW="$ROOT_DIR/.github/workflows/merge-gate.yml"
PRODUCER_AUTHORITY_REF=660981eba98d578ae1397be2a4fb784df58deda8
RECEIPT_VERIFIER_SHA256=1b0792415134afed7066ee70e1bc35319a204c5192cac69d33a8ca96b2f01082
QUALIFYING_RECEIPT_SHA256=09f01dd1435ac7cd6ebbcf28b619ff9ff739587b19bf88f1dd23a53f5c881760
PRODUCER_DEFINITION_SHA256=e87d2d927262de2fd65e60b9ab2a00312019dffe8307aa7428d6d92d4585198f

fail() {
    echo "check-merge-gate-policy.sh: $*" >&2
    exit 1
}

[[ -f $WORKFLOW ]] || fail "missing $WORKFLOW"
grep -Fq 'actions: write' "$WORKFLOW" || fail "NO_RESULT must be able to re-dispatch and cancel"
grep -Fq 'ref=4b78d727f35bc8612ac460a6e270dda5f5df304c' "$WORKFLOW" ||
    fail "gate must pin the parent authority commit"
grep -Fq '2f1c61d5ec9d98b9697317fd9e66b705161defb69b808d23e6d83384e1e2a1e8' "$WORKFLOW" ||
    fail "gate must content-pin the check-status authority"
grep -Fq '"$CHECK_OUTCOME_AUTHORITY"' "$WORKFLOW" ||
    fail "gate must call the parent check-status authority"
[[ $(grep -Fc -- '--select-latest-run' "$WORKFLOW") -eq 3 ]] ||
    fail "portable, privileged, and demo selectors must use the exact-head/latest authority"
grep -Fq 'job_status=missing' "$WORKFLOW" ||
    fail "a missing portable job must start as NO_RESULT"
grep -Fq 'priv_status=missing' "$WORKFLOW" ||
    fail "a missing privileged job must start as NO_RESULT"
if grep -Fq 'job_status=$run_status' "$WORKFLOW"; then
    fail "workflow success must not stand in for a missing authoritative job"
fi
grep -Fq '[ "$job_found" != true ] && [ "$run_state" = FAILED ]' "$WORKFLOW" ||
    fail "a complete workflow failure must remain a failure fallback"
grep -Fq '[ "$priv_job_found" != true ] && [ "$priv_run_state" = FAILED ]' "$WORKFLOW" ||
    fail "a complete privileged workflow failure must remain a failure fallback"
grep -Fq 'agent-utils/py/ci_hub_check_outcome.py' "$ROOT_DIR/scripts/classify-required-check.sh" ||
    fail "local shell adapter must delegate to the parent status authority"
grep -Fq 'from ci_hub_check_outcome import' "$ROOT_DIR/scripts/pr_status.py" ||
    fail "PR rollup must import the parent-authority adapter"
grep -Fq '"rrnewton/hermit": ("merge-gate-v4",)' "$ROOT_DIR/scripts/pr_status.py" ||
    fail "Hermit PR rollup must read the live versioned gate context"
grep -Fq 'agent-utils/py/ci_hub_check_outcome.py" --annotate-rollups' "$ROOT_DIR/scripts/pr-dag-health.sh" ||
    fail "lander rollup must call the parent-authority adapter"
grep -Fq '[[ $REPO == rrnewton/hermit ]] && GATE_CONTEXT=merge-gate-v4' "$ROOT_DIR/scripts/pr-dag-health.sh" ||
    fail "lander rollup must use Hermit's live versioned gate context"
grep -Fq 'latest_named($r; $gate_context)' "$ROOT_DIR/scripts/pr-dag-health.sh" ||
    fail "lander rollup must select the repository-specific gate context"
grep -Fq -- '--select-latest-rollup --head-sha "$MAIN_FULL_SHA"' "$ROOT_DIR/scripts/pr-dag-health.sh" ||
    fail "main-health rollup must select the latest check at the exact head"
[[ ! -e $ROOT_DIR/scripts/check_outcome.jq ]] ||
    fail "duplicate jq status classifier must not exist"
[[ ! -e $ROOT_DIR/scripts/check_status_outcome.py ]] ||
    fail "duplicate Hermit status adapter must not exist"
[[ $(grep -Fc "REGISTRY_REF: $PRODUCER_AUTHORITY_REF" "$WORKFLOW") -eq 2 ]] ||
    fail "both gate legs must pin the producer registries to the parent authority commit"
[[ $(grep -Fc "VERIFIER_REF: $PRODUCER_AUTHORITY_REF" "$WORKFLOW") -eq 2 ]] ||
    fail "both gate legs must pin the receipt verifier to the parent authority commit"
[[ $(grep -Fc "$RECEIPT_VERIFIER_SHA256" "$WORKFLOW") -eq 2 ]] ||
    fail "both gate legs must content-pin the parent receipt verifier"
[[ $(grep -Fc "$QUALIFYING_RECEIPT_SHA256" "$WORKFLOW") -eq 2 ]] ||
    fail "both gate legs must content-pin the qualifying-receipt registry"
[[ $(grep -Fc "$PRODUCER_DEFINITION_SHA256" "$WORKFLOW") -eq 2 ]] ||
    fail "both gate legs must content-pin the producer-definition registry"
grep -Fq '"$RECEIPT_VERIFIER"' "$WORKFLOW" ||
    fail "local alternate leg must call the parent receipt verifier"
if grep -Eq 'scripts/(check|verify)-local-validation' "$WORKFLOW"; then
    fail "gate must not call a PR-local validation-evidence verifier"
fi
grep -Fq 'NO_RESULT)' "$WORKFLOW" || fail "gate must handle NO_RESULT explicitly"
grep -Fq 'dispatch_no_result' "$WORKFLOW" || fail "NO_RESULT must re-dispatch"
grep -Fq 'queue_hosted_retry "$demo_status" demo-hot-path-rerun' "$WORKFLOW" ||
    fail "demo NO_RESULT must rerun the selected pull-request run"
grep -Fq 'queued | in_progress | waiting | requested | pending)' "$WORKFLOW" ||
    fail "active NO_RESULT runs must wait for workflow_run completion, not rerun"
grep -Fq 'actions/runs/${run_id}/rerun' "$WORKFLOW" ||
    fail "demo recovery must use the selected run ID"
if grep -Fq 'queue_dispatch demo-hot-path.yml' "$WORKFLOW"; then
    fail "workflow_dispatch demo runs are ineligible and must not be queued"
fi
grep -Fq 'cancel_no_result_gate' "$WORKFLOW" || fail "NO_RESULT must not exit red or green"
grep -Fq '/force-cancel' "$WORKFLOW" || fail "if: always() gate requires force-cancel for NO_RESULT"
grep -Fq 'GATE_RUN_ID' "$WORKFLOW" || fail "self-cancellation must identify the exact gate run"
if grep -Eq 'success[[:space:]]*\|[[:space:]]*skipped|success[[:space:]]+or[[:space:]]+skipped' "$WORKFLOW"; then
    fail "skipped must never satisfy a required check"
fi

echo "check-merge-gate-policy.sh: OK - PASSED/FAILED/NO_RESULT gate wiring enforced"
