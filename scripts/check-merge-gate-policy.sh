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
PRODUCER_AUTHORITY_REF=cb78bf76a498809c7b24b1a973574e7c863d5109
RECEIPT_VERIFIER_SHA256=1b0792415134afed7066ee70e1bc35319a204c5192cac69d33a8ca96b2f01082
QUALIFYING_RECEIPT_SHA256=09f01dd1435ac7cd6ebbcf28b619ff9ff739587b19bf88f1dd23a53f5c881760
PRODUCER_DEFINITION_SHA256=fab77f72485776a4bbd00e8674e0315d26443177d80d6a715743846814b5546c

fail() {
    echo "check-merge-gate-policy.sh: $*" >&2
    exit 1
}

[[ -f $WORKFLOW ]] || fail "missing $WORKFLOW"

# Scope core-review assertions to that job and remove YAML comments before
# matching. A literal pasted into an unrelated job or a `#` comment is not
# executable wiring and must not keep this policy check green.
core_review_job=$(awk '
    /^  core-review-protocol:[[:space:]]*$/ { in_job = 1 }
    in_job && /^  [[:alnum:]_-]+:[[:space:]]*$/ \
        && $0 !~ /^  core-review-protocol:[[:space:]]*$/ { exit }
    in_job {
        line = $0
        if (line ~ /^[[:space:]]*#/) next
        sub(/[[:space:]]+#.*$/, "", line)
        print line
    }
' "$WORKFLOW")
[[ -n $core_review_job ]] || fail "missing core-review-protocol job"
core_job_has() {
    grep -Fq -- "$1" <<<"$core_review_job"
}

# Recover the one logical shell command that invokes the review linter.  A
# binding elsewhere in the job is not evidence that the linter receives it.
core_linter_invocation=$(awk '
    {
        lines[NR] = $0
        if ($0 ~ /(^|[[:space:]])bash scripts\/core-review-protocol-lint[.]sh/) {
            invocation_line = NR
            invocation_count++
        }
    }
    END {
        if (invocation_count != 1) exit 1
        first = invocation_line
        while (first > 1 && lines[first - 1] ~ /\\[[:space:]]*$/) first--
        for (line = first; line <= invocation_line; line++) {
            sub(/\\[[:space:]]*$/, "", lines[line])
            printf "%s%s", (line == first ? "" : " "), lines[line]
        }
        print ""
    }
' <<<"$core_review_job") ||
    fail "core-review protocol must contain exactly one linter invocation"

validate_core_linter_invocation() {
    local invocation=$1 bash_index=-1 binding_count index word
    local -a invocation_words

    # Match assignment words in the command prefix, not their text inside an
    # unused variable or another executable command.
    read -r -a invocation_words <<<"$invocation"
    [[ ${invocation_words[0]-} == if ]] || {
        echo "core-review linter invocation must be the command governed by its if statement" >&2
        return 1
    }
    for index in "${!invocation_words[@]}"; do
        [[ ${invocation_words[$index]} == bash ]] && bash_index=$index
    done
    if [[ $bash_index -lt 2 || ${invocation_words[$((bash_index + 1))]-} != 'scripts/core-review-protocol-lint.sh;' ||
          ${invocation_words[$((bash_index + 2))]-} != 'then' ||
          $((bash_index + 3)) -ne ${#invocation_words[@]} ]]; then
        echo "core-review linter invocation must execute scripts/core-review-protocol-lint.sh directly" >&2
        return 1
    fi

    binding_count=0
    for ((index = 1; index < bash_index; index++)); do
        word=${invocation_words[$index]}
        [[ $word == "PR_HEAD_SHA=\"\$pr_head\"" ]] && ((binding_count += 1))
    done
    if [[ $binding_count -ne 1 ]]; then
        echo "core-review linter invocation must bind PR_HEAD_SHA exactly once" >&2
        return 1
    fi

    binding_count=0
    for ((index = 1; index < bash_index; index++)); do
        word=${invocation_words[$index]}
        [[ $word == "PR_COMMENTS_FILE=\"\$pr_comments_file\"" ]] && ((binding_count += 1))
    done
    if [[ $binding_count -ne 1 ]]; then
        echo "core-review linter invocation must bind PR_COMMENTS_FILE exactly once" >&2
        return 1
    fi

    for ((index = 1; index < bash_index; index++)); do
        word=${invocation_words[$index]}
        [[ $word =~ ^[a-zA-Z_][a-zA-Z_0-9]*= ]] || {
            echo "core-review linter invocation contains executable token before the linter: $word" >&2
            return 1
        }
    done
}

invocation_error=$(validate_core_linter_invocation "$core_linter_invocation" 2>&1) ||
    fail "$invocation_error"

# Negative controls exercise the production invocation parser.  Both deleting
# a binding and retaining its literal text as an argument to an executable
# shell command must fail with the missing binding's name.
expect_binding_mutation_rejected() {
    local test_name=$1 expected_name=$2 mutation=$3 error
    if error=$(validate_core_linter_invocation "$mutation" 2>&1); then
        fail "$test_name mutation was accepted"
    fi
    [[ $error == *"$expected_name"* ]] ||
        fail "$test_name mutation did not name $expected_name: $error"
}

head_binding=" PR_HEAD_SHA=\"\$pr_head\""
head_deleted=${core_linter_invocation/"$head_binding"/}
expect_binding_mutation_rejected "deleted head binding" PR_HEAD_SHA "$head_deleted"
head_decoy_replacement=" printf '%s' 'PR_HEAD_SHA=\"\$pr_head\"' &&"
head_decoy=${core_linter_invocation/"$head_binding"/$head_decoy_replacement}
expect_binding_mutation_rejected "executable head decoy" PR_HEAD_SHA "$head_decoy"
comments_binding=" PR_COMMENTS_FILE=\"\$pr_comments_file\""
comments_deleted=${core_linter_invocation/"$comments_binding"/}
expect_binding_mutation_rejected "deleted comment binding" PR_COMMENTS_FILE "$comments_deleted"
comments_decoy_replacement=" printf '%s' 'PR_COMMENTS_FILE=\"\$pr_comments_file\"' &&"
comments_decoy=${core_linter_invocation/"$comments_binding"/$comments_decoy_replacement}
expect_binding_mutation_rejected "executable comment decoy" PR_COMMENTS_FILE "$comments_decoy"

core_job_has 'pr_head="$(jq -r '\''.head.sha'\'' <<< "$pr_json")"' ||
    fail "core-review protocol must read the exact pull-request head"
core_job_has 'issues/${pr_number}/comments?per_page=100' ||
    fail "core-review protocol must fetch the complete issue-comment history"
core_job_has '--paginate --slurp | jq -c '\''add // []'\'' > "$pr_comments_file"' ||
    fail "core-review comment fetch must preserve all pages in one JSON array"
core_job_has 'grep -qiE '\''kvm'\'' <<< "$files" || files_kvm_status=$?' ||
    fail "KVM changed-file grep status must be captured"
core_job_has 'grep -Fixq kvm <<< "$labels" || labels_kvm_status=$?' ||
    fail "KVM label grep status must be captured"
core_job_has 'case "$files_kvm_status" in' ||
    fail "KVM changed-file grep errors must be handled"
core_job_has 'case "$labels_kvm_status" in' ||
    fail "KVM label grep errors must be handled"
core_job_has 'if [ "$files_kvm_status" -eq 0 ] || [ "$labels_kvm_status" -eq 0 ]; then' ||
    fail "KVM classification must use only checked match statuses"
files_kvm_case=$(awk '
    /case "\$files_kvm_status" in/ { in_case = 1 }
    in_case { print }
    in_case && /esac/ { exit }
' <<<"$core_review_job")
labels_kvm_case=$(awk '
    /case "\$labels_kvm_status" in/ { in_case = 1 }
    in_case { print }
    in_case && /esac/ { exit }
' <<<"$core_review_job")
[[ $files_kvm_case == *'0 | 1)'* && $files_kvm_case == *'exit 2'* ]] ||
    fail "KVM changed-file grep must accept only status 0/1 and refuse every error"
[[ $labels_kvm_case == *'0 | 1)'* && $labels_kvm_case == *'exit 2'* ]] ||
    fail "KVM label grep must accept only status 0/1 and refuse every error"
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
grep -Fq 'scripts/check_outcome_adapter.py' "$ROOT_DIR/scripts/classify-required-check.sh" ||
    fail "local shell adapter must delegate to the parent status authority"
grep -Fq 'from check_outcome_adapter import' "$ROOT_DIR/scripts/pr_status.py" ||
    fail "PR rollup must import the parent-authority adapter"
grep -Fq '"rrnewton/hermit": ("merge-gate-v4",)' "$ROOT_DIR/scripts/pr_status.py" ||
    fail "Hermit PR rollup must read the live versioned gate context"
grep -Fq 'check_outcome_adapter.py" --annotate-rollups' "$ROOT_DIR/scripts/pr-dag-health.sh" ||
    fail "lander rollup must call the parent-authority adapter"
grep -Fq '[[ $REPO == rrnewton/hermit ]] && GATE_CONTEXT=merge-gate-v4' "$ROOT_DIR/scripts/pr-dag-health.sh" ||
    fail "lander rollup must use Hermit's live versioned gate context"
grep -Fq 'latest_named($r; $gate_context)' "$ROOT_DIR/scripts/pr-dag-health.sh" ||
    fail "lander rollup must select the repository-specific gate context"
grep -Fq -- '--select-latest-rollup --head-sha "$MAIN_FULL_SHA"' "$ROOT_DIR/scripts/pr-dag-health.sh" ||
    fail "main-health rollup must select the latest check at the exact head"
# The assertions above match literal strings inside Hermit's own scripts. A
# string keeps matching long after the file it names is deleted: the reference
# to agent-utils/py/ci_hub_check_outcome.py matched here for months after
# agent-utils commit 5ef91c5 removed that file, so this lint stayed green while
# the classifier it guards pointed at nothing. Resolve every classifier path
# these scripts name, then run the classifier, so neither a rename nor a
# command-line interface that quietly disappears can pass.
for consumer in scripts/classify-required-check.sh scripts/pr-dag-health.sh \
    scripts/test-check-status-outcome.sh scripts/pr_status.py; do
    while read -r reference; do
        [[ -n $reference ]] || continue
        resolved=${reference//\$root_dir/$ROOT_DIR}
        resolved=${resolved//\$SCRIPT_DIR/$ROOT_DIR/scripts}
        resolved=${resolved//\$ROOT_DIR/$ROOT_DIR}
        [[ -f $resolved ]] ||
            fail "$consumer names check-status classifier '$reference', which does not resolve to a file ($resolved)"
    done < <(grep -oE '"[^"]*check_outcome[^"]*\.py"' "$ROOT_DIR/$consumer" | tr -d '"')
done

CLASSIFIER="$ROOT_DIR/scripts/check_outcome_adapter.py"

# ⚠️ THE AUTHORITY MUST BE REACHABLE BEFORE ANY CASE HERE MEANS ANYTHING. If it
# is not, every case below is unevaluable, so this declares that and exits 0
# rather than failing. `make lint-checks` then still passes and
# ci/lint-checks-node.sh classifies the run no_result (exit 75), which the
# ledger records as no_result rather than fail.
#
# ⚠️ EXIT 0 IS REQUIRED HERE, NOT MERELY TIDY. classify_run in
# ci/lint-checks-node.sh deliberately lets a real failure outrank any marker --
# "A REAL FAILURE ALWAYS WINS" -- so a nonzero exit could never be reported as
# no_result no matter what this printed. The marker channel only works on a
# passing target.
#
# Only exit 3 from the probe takes this path; see scripts/authority-available.sh
# for why 3 cannot be a refusal, a bug, or a tampered authority.
_auth_rc=0
_authority_dir=$("$ROOT_DIR/scripts/authority-available.sh") || _auth_rc=$?
# ⚠️ ONLY EXIT 3 MAY SKIP, AND AN EARLIER VERSION OF THIS GUARD GOT IT WRONG.
# It treated ANY nonzero from the helper as "unavailable", so a TAMPERED
# authority -- fetched successfully, wrong bytes, AuthorityIntegrityError, exit
# 1 -- was laundered into a no-result skip. That is the most dangerous possible
# reading: the one failure mode the content pin exists to catch, silently
# reported as "could not evaluate". Measured 2026-09-04: checker exited 0 with a
# marker against a deliberately corrupted authority.
if [ "$_auth_rc" -eq 3 ]; then
    echo "NO-RESULT-CASE: check-merge-gate-policy.sh: the pinned check-status authority could not be consulted; no case in this checker was evaluated"
    exit 0
elif [ "$_auth_rc" -ne 0 ]; then
    echo "check-merge-gate-policy.sh: obtaining the pinned check-status authority failed with exit $_auth_rc; that is not an outage and is not being skipped" >&2
    exit "$_auth_rc"
fi
# ⚠️ EXPORTING THIS IS THE POINT, not tidiness. Every later adapter process in
# this checker now reads the authority from disk instead of fetching it again,
# so a 504 arriving after the check above cannot turn a case into a nonzero
# exit. Probing alone left exactly that window open; see
# scripts/authority-available.sh.
export DEV_HERMIT_PARENT="$_authority_dir"
trap 'rm -rf "$_authority_dir"' EXIT
CONSUMER_TEST="$ROOT_DIR/scripts/test-check-status-outcome.sh"
[[ -f $CLASSIFIER ]] || fail "the check-status adapter is missing at $CLASSIFIER"
[[ -x $CONSUMER_TEST ]] || fail "the check-status consumer test is missing at $CONSUMER_TEST"
grep -Fq 'AUTHORITY_COMMIT = "4b78d727f35bc8612ac460a6e270dda5f5df304c"' "$CLASSIFIER" ||
    fail "the local adapter must pin the reviewed parent authority commit"
grep -Fq 'AUTHORITY_SHA256 = "2f1c61d5ec9d98b9697317fd9e66b705161defb69b808d23e6d83384e1e2a1e8"' "$CLASSIFIER" ||
    fail "the local adapter must content-pin the reviewed parent authority"

# The consumer test executes the shell adapter, pr_status.py, and
# pr-dag-health.sh with deterministic GitHub responses. Importing the adapter
# alone would not detect a broken executable path in any of those callers.
consumer_output=$("$CONSUMER_TEST" 2>&1) ||
    fail "real check-status consumer paths failed: $consumer_output"
[[ $consumer_output == *"PASS: lazy content pin and real classify-required-check, pr_status, and pr-dag-health consumers"* ]] ||
    fail "real check-status consumer test did not report completion: $consumer_output"

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
