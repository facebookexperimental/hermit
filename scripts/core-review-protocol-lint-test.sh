#!/usr/bin/env bash
# Self-test for core-review-protocol-lint.sh.
#
# Feeds the linter fixture PR labels, body, KVM flag, exact head, and issue
# comments (inline or through a file), then asserts the exit status. Run locally
# or in CI:
#
#     scripts/core-review-protocol-lint-test.sh
#
# Exits 0 when every case matches its expected status, 1 otherwise.

set -euo pipefail

# Same rule as the check-status checkers: if the pinned review-label contract
# cannot be consulted, no case here is evaluable. Declare it and exit 0 so
# `make lint-checks` still passes and ci/lint-checks-node.sh classifies the run
# no_result (exit 75) instead of fail. A nonzero exit could never be reported as
# no_result -- classify_run lets a real failure outrank any marker.
_probe_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
_auth_rc=0
_authority_dir=$("$_probe_root/scripts/authority-available.sh" "$_probe_root/scripts/review_contract_adapter.py") || _auth_rc=$?
# ⚠️ ONLY EXIT 3 MAY SKIP, AND AN EARLIER VERSION OF THIS GUARD GOT IT WRONG.
# It treated ANY nonzero from the helper as "unavailable", so a TAMPERED
# authority -- fetched successfully, wrong bytes, AuthorityIntegrityError, exit
# 1 -- was laundered into a no-result skip. That is the most dangerous possible
# reading: the one failure mode the content pin exists to catch, silently
# reported as "could not evaluate". Measured 2026-09-04: checker exited 0 with a
# marker against a deliberately corrupted authority.
if [ "$_auth_rc" -eq 3 ]; then
    echo "NO-RESULT-CASE: core-review-protocol-lint-test.sh: the pinned review-label contract could not be consulted; no case in this checker was evaluated"
    exit 0
elif [ "$_auth_rc" -ne 0 ]; then
    echo "core-review-protocol-lint-test.sh: obtaining the pinned review-label contract failed with exit $_auth_rc; that is not an outage and is not being skipped" >&2
    exit "$_auth_rc"
fi
# ⚠️ EXPORTING THIS IS THE POINT, not tidiness. Every later adapter process in
# this checker now reads the authority from disk instead of fetching it again,
# so a 504 arriving after the check above cannot turn a case into a nonzero
# exit. Probing alone left exactly that window open; see
# scripts/authority-available.sh.
export DEV_HERMIT_PARENT="$_authority_dir"
trap 'rm -rf "$_authority_dir"' EXIT


SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
readonly LINT="$SCRIPT_DIR/core-review-protocol-lint.sh"
readonly CONTRACT_ADAPTER="$SCRIPT_DIR/review_contract_adapter.py"

# A complete, valid non-KVM PR body containing every required section.
readonly FULL_BODY='## Summary
Adds a thing.

## Determinism
Deterministic because reasons and an informal proof.

## Linux Semantics
Matches the kernel behavior described here.

## Validation
`cargo test -p hermit-detcore` passed at L2 (ptrace).

## Human Review Required
Trigger 4: core DetCore scheduling change.'

# The label set for a fully reviewed + approved PR (round 1).
readonly FULL_LABELS='post-facto-human-review
adversarial-review-codex1
adversarial-review-claude1
passed-review-codex
passed-review-claude'

# INERT FIXTURES. These two 40-hex strings are synthetic and name no commit in
# any repository, and every case below drives the linter purely through
# environment variables. Nothing here reads or writes GitHub, so no approval,
# label, review, or merge is ever planted to test a refusal.
readonly HEAD_SHA='0123456789abcdef0123456789abcdef01234567'
readonly OLD_SHA='fedcba9876543210fedcba9876543210fedcba98'

# Both lanes bound to the current head: the approval state the gate must accept.
readonly FULL_APPROVALS="[
  {\"body\": \"APPROVED-AT: codex ${HEAD_SHA}\"},
  {\"body\": \"APPROVED-AT: claude ${HEAD_SHA}\"}
]"

pass=0
fail=0

# run_case NAME EXPECTED_EXIT LABELS BODY [IS_KVM] [COMMENTS_JSON] [HEAD_SHA]
#
# Comments and head default to a valid dual-lane exact-head approval, so the
# pre-existing label and body cases keep testing exactly what they always did.
# Give every comment fixture a reviewer role tag unless it names one itself.
#
# The cases below are about the verdict GRAMMAR — markdown wrappers, lane case,
# supersession — not about who may approve. Since 2026-08-23 an approval must
# also carry a role tag, and spelling one into each of those fixtures would
# bury the thing each is actually testing. So the default is a reviewer with
# recognizable role tag, and the role-tag cases further down state their comment
# shapes explicitly. Deliberately non-destructive: a
# fixture that sets `user` or `author_association` keeps its own, and a
# fixture that is not a JSON array (the input-validation cases) passes through
# untouched rather than being silently repaired into something valid.
readonly DEFAULT_ROLE_TAG='[hermit2, reviewer-default, unresolved, devbig014, role=reviewer]'
readonly PR_AUTHOR_FIXTURE='pr-author-fixture'
authenticate_comments() {
    local json=$1 out
    # Add the trusted OWNER association used by this repository's real relay
    # comments, and prefix a role tag onto any body that does not already open
    # with one, so the pre-existing GRAMMAR cases keep testing grammar. A
    # fixture that states its own association/tag is untouched:
    # `test("^\\s*\\[")` looks only at what is there. A non-array payload (the
    # input-validation cases) passes through so it can still be refused.
    out=$(jq -c --arg tag "$DEFAULT_ROLE_TAG" '
        if type == "array" then
            map(if type == "object" then
                    .author_association = (.author_association // "OWNER")
                    | if (.body // "" | test("^\\s*\\[") | not)
                      then .body = ($tag + "\n" + (.body // ""))
                      else . end
                else . end)
        else . end' <<<"$json" 2>/dev/null) || { printf '%s' "$json"; return; }
    printf '%s' "$out"
}

run_case() {
    local name=$1 expected=$2 labels=$3 body=$4 is_kvm=${5:-false}
    local comments=${6-$FULL_APPROVALS} head=${7-$HEAD_SHA}
    local actual=0
    comments=$(authenticate_comments "$comments")
    PR_LABELS="$labels" PR_BODY="$body" PR_IS_KVM="$is_kvm" PR_NUMBER="test" \
        PR_HEAD_SHA="$head" PR_COMMENTS_JSON="$comments" \
        bash "$LINT" >/dev/null 2>&1 || actual=$?
    if [ "$actual" -eq "$expected" ]; then
        echo "ok   - ${name} (exit ${actual})"
        pass=$((pass + 1))
    else
        echo "FAIL - ${name}: expected exit ${expected}, got ${actual}"
        fail=$((fail + 1))
    fi
}

# run_message_case NAME EXPECTED_EXIT EXPECTED_SUBSTRING LABELS BODY COMMENTS_JSON HEAD
#
# Exit status alone cannot tell these cases apart: the input-validation checks
# are deliberately redundant with the binding check that follows them, so
# deleting one still yields exit 1 — via a MISLEADING diagnosis ("superseded
# approval ... current head is") instead of the true one ("PR_HEAD_SHA is
# missing"). Asserting the message is what makes those checks discriminable;
# without it a mutation that removes them leaves the suite green.
run_message_case() {
    local name=$1 expected=$2 needle=$3 labels=$4 body=$5 comments=$6 head=$7
    local actual=0 output
    comments=$(authenticate_comments "$comments")
    output=$(PR_LABELS="$labels" PR_BODY="$body" PR_IS_KVM=false PR_NUMBER="test" \
        PR_HEAD_SHA="$head" PR_COMMENTS_JSON="$comments" \
        bash "$LINT" 2>&1) || actual=$?
    if [ "$actual" -eq "$expected" ] && [[ $output == *"$needle"* ]]; then
        echo "ok   - ${name} (exit ${actual}, diagnosed)"
        pass=$((pass + 1))
    elif [ "$actual" -ne "$expected" ]; then
        echo "FAIL - ${name}: expected exit ${expected}, got ${actual}"
        fail=$((fail + 1))
    else
        echo "FAIL - ${name}: exit ${expected} but missing diagnosis '${needle}'"
        fail=$((fail + 1))
    fi
}

run_message_absent_case() {
    local name=$1 expected=$2 forbidden=$3 labels=$4 body=$5 comments=$6 head=$7
    local actual=0 output
    comments=$(authenticate_comments "$comments")
    output=$(PR_LABELS="$labels" PR_BODY="$body" PR_IS_KVM=false PR_NUMBER="test" \
        PR_HEAD_SHA="$head" PR_COMMENTS_JSON="$comments" \
        bash "$LINT" 2>&1) || actual=$?
    if [ "$actual" -eq "$expected" ] && [[ $output != *"$forbidden"* ]]; then
        echo "ok   - ${name} (exit ${actual}, absent)"
        pass=$((pass + 1))
    elif [ "$actual" -ne "$expected" ]; then
        echo "FAIL - ${name}: expected exit ${expected}, got ${actual}"
        fail=$((fail + 1))
    else
        echo "FAIL - ${name}: exit ${expected} but forbidden diagnosis '${forbidden}' was present"
        fail=$((fail + 1))
    fi
}

# --- Not applicable: no post-facto-human-review label always passes. ----------
# --- An INERT trigger label must refuse, not fast-path ------------------------
#
# The gate keys on exactly one label. A PR carrying a different `*-human-review`
# label used to take the not-applicable path and exit 0, so it read as
# "human review required" while being evaluated by nothing. That was live on
# hermit#2216 itself -- the change that redefines what counts as an approval,
# admitted without either version of that definition being applied to it.
run_case "an inert *-human-review trigger refuses rather than fast-pathing" 2 \
    "$(printf 'pre-land-human-review\nadversarial-review-claude1\npassed-review-claude')" \
    "irrelevant body"

# The refusal must NOT fire when the real trigger is also present: then the
# protocol is applicable and must be evaluated on its merits (exit 0 or 1),
# never short-circuited by the inert label sitting beside it.
run_case "the real trigger beside an inert one is still evaluated, not refused" 1 \
    "$(printf 'post-facto-human-review\npre-land-human-review')" \
    "body missing the required sections"

# Shape-matched, not name-matched: a label invented tomorrow is caught today.
run_case "an unknown *-human-review variant is caught by shape" 2 \
    "$(printf 'some-future-human-review\ncategory:infra')" \
    "irrelevant body"

# And the ordinary fast path is untouched: a label merely CONTAINING "review"
# is not a trigger, so it must still pass.
run_case "a non-trigger review label still takes the fast path" 0 \
    "$(printf 'passed-review-claude\ncategory:infra')" \
    "irrelevant body"

run_case "unlabeled PR passes even with empty body" 0 \
    $'random-label\nlocally-validated' ""
run_case "unlabeled PR passes even missing everything the protocol wants" 0 \
    "" ""

# --- Happy paths -------------------------------------------------------------
run_case "labeled + full labels + all sections (non-KVM) passes" 0 \
    "$FULL_LABELS" "$FULL_BODY"

run_case "later review round (round 2 labels) still passes" 0 \
    $'post-facto-human-review\nadversarial-review-codex2\nadversarial-review-claude3\npassed-review-codex\npassed-review-claude' \
    "$FULL_BODY"

run_case "KVM PR with Relationship to gVisor section passes" 0 \
    "$FULL_LABELS" "${FULL_BODY}"$'\n\n## Relationship to gVisor\nN/A: no gVisor analog.' \
    true

run_case "bold-style headings are accepted" 0 \
    "$FULL_LABELS" \
    $'**Summary** foo\n**Determinism** bar\n**Linux Semantics** baz\n**Validation** qux\n**Human Review Required** trigger 4'

# --- Missing review labels blocks --------------------------------------------
run_case "missing adversarial-review-codex blocks" 1 \
    $'post-facto-human-review\nadversarial-review-claude1\npassed-review-codex\npassed-review-claude' \
    "$FULL_BODY"

run_case "missing adversarial-review-claude blocks" 1 \
    $'post-facto-human-review\nadversarial-review-codex1\npassed-review-codex\npassed-review-claude' \
    "$FULL_BODY"

run_case "missing passed-review-codex blocks" 1 \
    $'post-facto-human-review\nadversarial-review-codex1\nadversarial-review-claude1\npassed-review-claude' \
    "$FULL_BODY"

run_case "missing passed-review-claude blocks" 1 \
    $'post-facto-human-review\nadversarial-review-codex1\nadversarial-review-claude1\npassed-review-codex' \
    "$FULL_BODY"

run_case "adversarial review present but not approved blocks" 1 \
    $'post-facto-human-review\nadversarial-review-codex1\nadversarial-review-claude1' \
    "$FULL_BODY"

run_case "round label out of range (round 5) does not count, blocks" 1 \
    $'post-facto-human-review\nadversarial-review-codex5\nadversarial-review-claude5\npassed-review-codex\npassed-review-claude' \
    "$FULL_BODY"

run_case "non-label review-round-codex does not count, blocks" 1 \
    $'post-facto-human-review\nreview-round-codex\nadversarial-review-claude1\npassed-review-codex\npassed-review-claude' \
    "$FULL_BODY"

# Every numbered label accepted by the writer's shared contract must also pass
# this reader when the other families use their first accepted label.
contract_output=$(python3 "$CONTRACT_ADAPTER" --format lint-records)
declare -a contract_families=()
declare -A contract_approval=()
declare -A contract_rounds=()
contract_post_facto=
while IFS=$'\t' read -r first second third; do
    if [ "$first" = post-facto ]; then
        contract_post_facto=$second
        continue
    fi
    contract_families+=("$first")
    contract_approval[$first]=$second
    contract_rounds[$first]=$third
done <<<"$contract_output"

for family in "${contract_families[@]}"; do
    IFS=, read -r -a family_rounds <<<"${contract_rounds[$family]}"
    for candidate in "${family_rounds[@]}"; do
        labels=$contract_post_facto
        for fixture_family in "${contract_families[@]}"; do
            IFS=, read -r first_round _ <<<"${contract_rounds[$fixture_family]}"
            if [ "$fixture_family" = "$family" ]; then
                labels+=$'\n'"$candidate"
            else
                labels+=$'\n'"$first_round"
            fi
            labels+=$'\n'"${contract_approval[$fixture_family]}"
        done
        run_case "accepted label ${candidate} passes through the reader" 0 \
            "$labels" "$FULL_BODY"
    done
done

diagnostic_status=0
diagnostic=$(PR_LABELS=$'post-facto-human-review\nreview-round-codex\nadversarial-review-claude1\npassed-review-codex\npassed-review-claude' \
    PR_BODY="$FULL_BODY" PR_NUMBER=test bash "$LINT" 2>&1) || diagnostic_status=$?
if [ "$diagnostic_status" -eq 1 ] \
    && [[ $diagnostic == *"adversarial-review-codex1, adversarial-review-codex2, adversarial-review-codex3, adversarial-review-codex4"* ]] \
    && [[ $diagnostic != *"review-round-codex"* ]]; then
    echo "ok   - missing-round diagnostic names exact accepted alternatives"
    pass=$((pass + 1))
else
    echo "FAIL - missing-round diagnostic did not name exact accepted alternatives"
    fail=$((fail + 1))
fi

# --- Missing body sections blocks --------------------------------------------
run_case "missing Summary section blocks" 1 \
    "$FULL_LABELS" \
    $'## Determinism\nx\n## Linux Semantics\ny\n## Validation\nz\n## Human Review Required\nt'

run_case "missing Linux Semantics section blocks" 1 \
    "$FULL_LABELS" \
    $'## Summary\nx\n## Determinism\ny\n## Validation\nz\n## Human Review Required\nt'

run_case "missing Human Review Required section blocks" 1 \
    "$FULL_LABELS" \
    $'## Summary\nx\n## Determinism\ny\n## Linux Semantics\nl\n## Validation\nz'

run_case "empty body blocks a labeled PR" 1 \
    "$FULL_LABELS" ""

# --- KVM-specific section -----------------------------------------------------
run_case "KVM PR without Relationship to gVisor section blocks" 1 \
    "$FULL_LABELS" "$FULL_BODY" true

run_case "non-KVM PR does not require Relationship to gVisor" 0 \
    "$FULL_LABELS" "$FULL_BODY" false

# --- Prose must not satisfy a section ----------------------------------------
run_case "prose mention of a section keyword does not satisfy it" 1 \
    "$FULL_LABELS" \
    $'## Summary\nIn summary, this changes determinism and validation broadly.\n## Determinism\nd\n## Validation\nv\n## Human Review Required\nt'

# --- UNSET vs EMPTY: the defect this gate had, in both directions -----------
# run_case cannot express "unset", because it always assigns the variables.
# These cases invoke the lint directly with the variable removed from the
# environment, which is the whole point: the previous `${PR_LABELS-}` made the
# unset case indistinguishable from an empty one and returned a PASS having
# checked nothing.
run_unset_case() { # NAME EXPECTED_EXIT UNSET_VAR [ENV...]
    local name=$1 expected=$2 unset_var=$3; shift 3
    local actual=0
    env -u "$unset_var" PR_NUMBER=test "$@" bash "$LINT" >/dev/null 2>&1 || actual=$?
    if [ "$actual" -eq "$expected" ]; then
        echo "ok   - ${name} (exit ${actual})"; pass=$((pass + 1))
    else
        echo "FAIL - ${name}: expected exit ${expected}, got ${actual}"; fail=$((fail + 1))
    fi
}

run_unset_case "PR_LABELS unset REFUSES (was a silent pass)" 2 PR_LABELS PR_BODY=""
run_case "PR_LABELS empty is a PASS and is not the unset case" 0 "" ""
run_unset_case "PR_BODY unset REFUSES when the protocol applies" 2 PR_BODY \
    "PR_LABELS=$FULL_LABELS"
run_unset_case "PR_BODY unset is IGNORED when the protocol does not apply" 0 PR_BODY \
    "PR_LABELS=category:detcore"

# The genuinely-empty label set must still take the not-applicable path, so the
# refusal above cannot be mistaken for "any missing label now blocks".
run_case "empty labels + empty body still passes (not applicable)" 0 "" ""

# --- A failed predicate is not a clean no-match -------------------------------
#
# The linter's grep predicates accept only grep's 0 (match) and 1 (no match).
# Every other observable shell status must become the linter's internal-error 2.
# The fake python3 supplies the already-validated contract records so these
# checks isolate the predicate status rather than depending on network access.
run_predicate_status_case() { # NAME MODE RAW_STATUS OPERATION [LABELS]
    local name=$1 mode=$2 raw_status=$3 operation=$4
    local labels=${5:-post-facto-human-review}
    local actual=0 output

    # These functions are exported and invoked by the child Bash, which the
    # static check cannot see from this process.
    # shellcheck disable=SC2317
    python3() {
        printf '%s\n' \
            $'post-facto\tpost-facto-human-review' \
            $'codex\tpassed-review-codex\tadversarial-review-codex1,adversarial-review-codex2,adversarial-review-codex3,adversarial-review-codex4' \
            $'claude\tpassed-review-claude\tadversarial-review-claude1,adversarial-review-claude2,adversarial-review-claude3,adversarial-review-claude4'
    }
    # shellcheck disable=SC2317
    grep() {
        case "$PREDICATE_FAILURE_MODE" in
            status)
                while IFS= read -r _; do :; done
                return "$PREDICATE_FAILURE_STATUS"
                ;;
            section-status)
                while IFS= read -r _; do :; done
                if [ "$1" = -Eiq ]; then
                    return "$PREDICATE_FAILURE_STATUS"
                fi
                return 0
                ;;
            inert-status)
                if [ "${1-}" = -E ] && [ "${3-}" = '-human-review$' ]; then
                    while IFS= read -r _; do :; done
                    return "$PREDICATE_FAILURE_STATUS"
                fi
                command grep "$@"
                ;;
            not-executable)
                while IFS= read -r _; do :; done
                command / "$@"
                ;;
            not-found)
                while IFS= read -r _; do :; done
                command /tmp/core-review-protocol-lint-no-such-command "$@"
                ;;
        esac
    }
    export -f python3 grep

    output=$(PREDICATE_FAILURE_MODE="$mode" PREDICATE_FAILURE_STATUS="$raw_status" \
        PR_LABELS="$labels" PR_BODY='' PR_IS_KVM=false PR_NUMBER=test \
        bash "$LINT" 2>&1) || actual=$?
    unset -f python3 grep

    if [ "$actual" -eq 2 ] \
        && [[ $output == *"${operation} could not decide (exit ${raw_status})"* ]]; then
        echo "ok   - ${name} (predicate exit ${raw_status} -> linter exit 2)"
        pass=$((pass + 1))
    else
        echo "FAIL - ${name}: predicate exit ${raw_status}, linter exit ${actual}"
        printf '%s\n' "$output"
        fail=$((fail + 1))
    fi
}

run_predicate_status_case \
    "command found but not executable refuses" not-executable 126 "label lookup"
run_predicate_status_case \
    "command not found refuses" not-found 127 "label lookup"
run_predicate_status_case \
    "inert-trigger grep error refuses instead of returning not applicable" \
    inert-status 2 "review-trigger label lookup" category:infra

predicate_status_failures=0
for predicate_mode in status section-status; do
    if [ "$predicate_mode" = status ]; then
        operation="label lookup"
    else
        operation="PR-body section lookup"
    fi
    for ((predicate_status = 2; predicate_status <= 255; predicate_status++)); do
        before_fail=$fail
        run_predicate_status_case \
            "${operation} status ${predicate_status} is not a clean no-match" \
            "$predicate_mode" "$predicate_status" "$operation" >/dev/null
        if [ "$fail" -ne "$before_fail" ]; then
            predicate_status_failures=$((predicate_status_failures + 1))
        fi
    done
done
if [ "$predicate_status_failures" -eq 0 ]; then
    echo "ok   - label and section statuses 2..255 refuse as linter exit 2"
else
    echo "FAIL - ${predicate_status_failures} label/section statuses in 2..255 did not refuse"
fi

# Prove that an unreviewed local or fetched parent contract cannot become the
# authority silently. A changed local file falls back to the reviewed bytes;
# changed fetched bytes are refused by the content pin.
ROOT_DIR="$SCRIPT_DIR/.." python3 - <<'PY'
import hashlib
import os
from pathlib import Path
import sys
import tempfile

root = Path(os.environ["ROOT_DIR"])
sys.path.insert(0, str(root / "scripts"))
import review_contract_adapter as adapter

source = adapter._verified_source()
assert hashlib.sha256(source).hexdigest() == adapter.AUTHORITY_SHA256

with tempfile.TemporaryDirectory(prefix="review-contract-adapter-") as tmp:
    parent = Path(tmp)
    authority = parent / adapter.AUTHORITY_RELATIVE_PATH
    authority.parent.mkdir(parents=True)
    authority.write_text("raise RuntimeError('unreviewed local contract executed')\n")
    os.environ["DEV_HERMIT_PARENT"] = str(parent)
    adapter._fetch_pinned_source = lambda: source
    assert adapter._verified_source() == source

    adapter._fetch_pinned_source = lambda: source + b"# changed\n"
    try:
        adapter._verified_source()
    except RuntimeError as error:
        assert "digest mismatch" in str(error)
    else:
        raise AssertionError("changed fetched review contract passed its content pin")
PY
echo "ok   - review-contract adapter accepts only content-pinned authority bytes"
pass=$((pass + 1))

# --- Exact-head approval binding, BOTH DIRECTIONS ----------------------------
#
# The gate must accept a genuine approval at the current head and refuse
# everything else. The positive half is not optional: a check that refuses
# everything is as useless as one that accepts everything, and the defect this
# section exists to catch — labels present, no lane binding the head — was live
# on PR #2176 with the newest claude verdict at that head a REJECTION.

# POSITIVE: the gate must FIRE, not sit inert.
run_case "both lanes bound to the exact head passes" 0 \
    "$FULL_LABELS" "$FULL_BODY" false "$FULL_APPROVALS" "$HEAD_SHA"

run_case "whole-line markdown emphasis around a binding still passes" 0 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"**APPROVED-AT: codex ${HEAD_SHA}**\"},
      {\"body\": \"\`APPROVED-AT: claude ${HEAD_SHA}\`\"}]" "$HEAD_SHA"

run_case "uppercase lane binds; an upper-case SHA does NOT (mirrors the reference) blocks" 1 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"APPROVED-AT: CODEX ${HEAD_SHA}\"},
      {\"body\": \"approved-at: claude $(printf '%s' "$HEAD_SHA" | tr 'a-f' 'A-F')\"}]" \
    "$HEAD_SHA"

run_case "a rejection followed by a later approval at the head passes" 0 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"APPROVED-AT: codex ${HEAD_SHA}\"},
      {\"body\": \"CHANGES-REQUESTED-AT: claude ${OLD_SHA}\"},
      {\"body\": \"APPROVED-AT: claude ${HEAD_SHA}\"}]" "$HEAD_SHA"

# A lane contains multiple reviewers. A refusal belongs to its issuer: that
# reviewer may discharge it, while a different reviewer's approval may bind the
# lane but must leave the refusal standing.
readonly REVIEWER_901='[hermit2, hermit-901, unresolved, devbig014, role=reviewer]'
readonly REVIEWER_902='[hermit2, hermit-902, unresolved, devbig014, role=reviewer]'
readonly REVIEWER_903='[hermit2, hermit-903, unresolved, devbig014, role=reviewer]'
readonly REFUSAL_ID=900001
readonly SECOND_REFUSAL_ID=900002
readonly WITHDRAWAL_ID=900003
run_case "issuer control: no refusal admits approvals from different reviewers" 0 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"${REVIEWER_901}\\nAPPROVED-AT: codex ${HEAD_SHA}\"},
      {\"body\": \"${REVIEWER_901}\\nAPPROVED-AT: claude ${HEAD_SHA}\"},
      {\"body\": \"${REVIEWER_902}\\nAPPROVED-AT: claude ${HEAD_SHA}\"}]" "$HEAD_SHA"

run_case "issuer control: same-reviewer approval discharges its refusal" 0 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"${REVIEWER_901}\\nAPPROVED-AT: codex ${HEAD_SHA}\"},
      {\"body\": \"${REVIEWER_901}\\nCHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"},
      {\"body\": \"${REVIEWER_901}\\nAPPROVED-AT: claude ${HEAD_SHA}\"}]" "$HEAD_SHA"

run_case "issuer control: different-reviewer approval leaves the refusal standing" 1 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"${REVIEWER_901}\\nAPPROVED-AT: codex ${HEAD_SHA}\"},
      {\"body\": \"${REVIEWER_901}\\nCHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"},
      {\"body\": \"${REVIEWER_902}\\nAPPROVED-AT: claude ${HEAD_SHA}\"}]" "$HEAD_SHA"

run_case "an identical repeated approval is a citation and does not discharge" 1 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"${REVIEWER_901}\\nAPPROVED-AT: codex ${HEAD_SHA}\"},
      {\"body\": \"${REVIEWER_901}\\nAPPROVED-AT: claude ${HEAD_SHA}\"},
      {\"body\": \"${REVIEWER_901}\\nCHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"},
      {\"body\": \"${REVIEWER_901}\\nAPPROVED-AT: claude ${HEAD_SHA}\"}]" "$HEAD_SHA"

run_case "a same-author shortened approval copy is a citation" 0 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"${REVIEWER_901}\\nAPPROVED-AT: codex ${HEAD_SHA}\"},
      {\"body\": \"${REVIEWER_901}\\nAPPROVED-AT: claude ${HEAD_SHA}\"},
      {\"body\": \"${REVIEWER_901}\\nAPPROVED-AT: claude ${HEAD_SHA:0:12}\"}]" "$HEAD_SHA"

run_case "a different-author shortened approval copy remains malformed" 1 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"${REVIEWER_901}\\nAPPROVED-AT: codex ${HEAD_SHA}\"},
      {\"body\": \"${REVIEWER_901}\\nAPPROVED-AT: claude ${HEAD_SHA}\"},
      {\"body\": \"${REVIEWER_902}\\nAPPROVED-AT: claude ${HEAD_SHA:0:12}\"}]" "$HEAD_SHA"

run_case "a marker's BY issuer takes precedence over the comment disclosure" 0 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"${REVIEWER_901}\\nAPPROVED-AT: codex ${HEAD_SHA}\"},
      {\"body\": \"${REVIEWER_902}\\nCHANGES-REQUESTED-AT: claude ${HEAD_SHA} BY hermit-901\"},
      {\"body\": \"${REVIEWER_902}\\nAPPROVED-AT: claude ${HEAD_SHA} BY hermit-901\"}]" "$HEAD_SHA"

run_case "a marker BY another issuer does not inherit the comment disclosure" 1 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"${REVIEWER_901}\\nAPPROVED-AT: codex ${HEAD_SHA}\"},
      {\"body\": \"${REVIEWER_901}\\nCHANGES-REQUESTED-AT: claude ${HEAD_SHA} BY hermit-901\"},
      {\"body\": \"${REVIEWER_901}\\nAPPROVED-AT: claude ${HEAD_SHA} BY hermit-902\"}]" "$HEAD_SHA"

run_case "another line's BY does not transfer refusal ownership" 0 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"id\": ${REFUSAL_ID}, \"body\": \"${REVIEWER_902}\\nAPPROVED-AT: codex ${HEAD_SHA} BY hermit-901\\nCHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"},
      {\"body\": \"${REVIEWER_902}\\nAPPROVED-AT: claude ${HEAD_SHA}\"}]" "$HEAD_SHA"

run_case "another line's BY does not transfer unquoted-withdrawal ownership" 0 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"id\": ${REFUSAL_ID}, \"body\": \"${REVIEWER_902}\\nCHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"},
      {\"id\": ${WITHDRAWAL_ID}, \"body\": \"${REVIEWER_902}\\nAPPROVED-AT: codex ${HEAD_SHA} BY hermit-901\\nCHANGES-REQUESTED-WITHDRAWN-AT: claude ${HEAD_SHA}\"},
      {\"body\": \"${REVIEWER_903}\\nAPPROVED-AT: claude ${HEAD_SHA}\"}]" "$HEAD_SHA"

run_case "precise RETIRES uses the comment claimant" 0 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"id\": ${REFUSAL_ID}, \"body\": \"${REVIEWER_901}\\nCHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"},
      {\"id\": ${WITHDRAWAL_ID}, \"body\": \"${REVIEWER_902}\\nAPPROVED-AT: codex ${HEAD_SHA} BY hermit-901\\nCHANGES-REQUESTED-WITHDRAWN-AT: claude ${HEAD_SHA}\\nRETIRES ${REFUSAL_ID}\"},
      {\"body\": \"${REVIEWER_903}\\nAPPROVED-AT: claude ${HEAD_SHA}\"}]" "$HEAD_SHA"

run_case "same-reviewer unquoted withdrawal retires one refusal" 0 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"${REVIEWER_901}\\nAPPROVED-AT: codex ${HEAD_SHA}\"},
      {\"id\": ${REFUSAL_ID}, \"body\": \"${REVIEWER_901}\\nCHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"},
      {\"id\": ${WITHDRAWAL_ID}, \"body\": \"${REVIEWER_901}\\nCHANGES-REQUESTED-WITHDRAWN-AT: claude ${HEAD_SHA}\"},
      {\"body\": \"${REVIEWER_902}\\nAPPROVED-AT: claude ${HEAD_SHA}\"}]" "$HEAD_SHA"

run_case "different-reviewer unquoted withdrawal leaves the refusal standing" 1 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"${REVIEWER_901}\\nAPPROVED-AT: codex ${HEAD_SHA}\"},
      {\"id\": ${REFUSAL_ID}, \"body\": \"${REVIEWER_901}\\nCHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"},
      {\"id\": ${WITHDRAWAL_ID}, \"body\": \"${REVIEWER_902}\\nCHANGES-REQUESTED-WITHDRAWN-AT: claude ${HEAD_SHA}\"},
      {\"body\": \"${REVIEWER_902}\\nAPPROVED-AT: claude ${HEAD_SHA}\"}]" "$HEAD_SHA"

run_message_absent_case "an unedited blockquoted withdrawal is inert" \
    1 "edited verdict comment" "$FULL_LABELS" "$FULL_BODY" \
    "[{\"body\": \"${REVIEWER_901}\\nAPPROVED-AT: codex ${HEAD_SHA}\"},
      {\"id\": ${REFUSAL_ID}, \"body\": \"${REVIEWER_901}\\nCHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"},
      {\"id\": ${WITHDRAWAL_ID}, \"created_at\": \"2026-08-27T01:00:00Z\", \"updated_at\": \"2026-08-27T01:00:00Z\", \"body\": \"${REVIEWER_901}\\n> CHANGES-REQUESTED-WITHDRAWN-AT: claude ${HEAD_SHA}\"},
      {\"body\": \"${REVIEWER_902}\\nAPPROVED-AT: claude ${HEAD_SHA}\"}]" "$HEAD_SHA"

run_message_absent_case "an edited blockquoted withdrawal remains inert" \
    1 "edited verdict comment" "$FULL_LABELS" "$FULL_BODY" \
    "[{\"body\": \"${REVIEWER_901}\\nAPPROVED-AT: codex ${HEAD_SHA}\"},
      {\"id\": ${REFUSAL_ID}, \"body\": \"${REVIEWER_901}\\nCHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"},
      {\"id\": ${WITHDRAWAL_ID}, \"created_at\": \"2026-08-27T01:00:00Z\", \"updated_at\": \"2026-08-27T02:00:00Z\", \"body\": \"${REVIEWER_901}\\n> CHANGES-REQUESTED-WITHDRAWN-AT: claude ${HEAD_SHA}\"},
      {\"body\": \"${REVIEWER_902}\\nAPPROVED-AT: claude ${HEAD_SHA}\"}]" "$HEAD_SHA"

# Review protocol examples are documentation, not verdicts. Both fenced and
# CommonMark indented code are removed once before any consumer reads a role
# tag, issuer, approval, refusal, withdrawal, RETIRES claim, edit, or malformed
# line. Genuine markers outside the examples remain authoritative.
run_case "outside markers bind while fenced documentation is ignored" 0 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"${REVIEWER_901}\\nAPPROVED-AT: codex ${HEAD_SHA}\\n~~~text\\nCHANGES-REQUESTED-AT: codex ${HEAD_SHA}\\nAPPROVED-AT: claude bad\\n~~~\"},
      {\"body\": \"${REVIEWER_902}\\nAPPROVED-AT: claude ${HEAD_SHA}\"}]" "$HEAD_SHA"

run_case "outside markers bind while indented documentation is ignored" 0 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"${REVIEWER_901}\\nAPPROVED-AT: codex ${HEAD_SHA}\\n\\n    CHANGES-REQUESTED-AT: codex ${HEAD_SHA}\\n    APPROVED-AT: claude bad\"},
      {\"body\": \"${REVIEWER_902}\\nAPPROVED-AT: claude ${HEAD_SHA}\"}]" "$HEAD_SHA"

run_case "a fenced approval example does not bind" 1 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"${REVIEWER_901}\\nAPPROVED-AT: codex ${HEAD_SHA}\"},
      {\"body\": \"${REVIEWER_902}\\n~~~\\nAPPROVED-AT: claude ${HEAD_SHA}\\n~~~\"}]" "$HEAD_SHA"

run_case "an indented approval example does not bind" 1 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"${REVIEWER_901}\\nAPPROVED-AT: codex ${HEAD_SHA}\"},
      {\"body\": \"${REVIEWER_902}\\n\\n    APPROVED-AT: claude ${HEAD_SHA}\"}]" "$HEAD_SHA"

run_case "a fenced withdrawal example does not discharge" 1 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"${REVIEWER_903}\\nAPPROVED-AT: codex ${HEAD_SHA}\"},
      {\"id\": ${REFUSAL_ID}, \"body\": \"${REVIEWER_901}\\nCHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"},
      {\"body\": \"${REVIEWER_901}\\n~~~\\nCHANGES-REQUESTED-WITHDRAWN-AT: claude ${HEAD_SHA}\\n~~~\"},
      {\"body\": \"${REVIEWER_902}\\nAPPROVED-AT: claude ${HEAD_SHA}\"}]" "$HEAD_SHA"

run_case "an indented withdrawal example does not discharge" 1 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"${REVIEWER_903}\\nAPPROVED-AT: codex ${HEAD_SHA}\"},
      {\"id\": ${REFUSAL_ID}, \"body\": \"${REVIEWER_901}\\nCHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"},
      {\"body\": \"${REVIEWER_901}\\n\\n    CHANGES-REQUESTED-WITHDRAWN-AT: claude ${HEAD_SHA}\"},
      {\"body\": \"${REVIEWER_902}\\nAPPROVED-AT: claude ${HEAD_SHA}\"}]" "$HEAD_SHA"

run_case "a fenced marker BY cannot transfer precise retirement authority" 1 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"${REVIEWER_903}\\nAPPROVED-AT: codex ${HEAD_SHA}\"},
      {\"id\": ${REFUSAL_ID}, \"body\": \"${REVIEWER_901}\\nCHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"},
      {\"body\": \"${REVIEWER_902}\\nCHANGES-REQUESTED-WITHDRAWN-AT: claude ${HEAD_SHA}\\nRETIRES ${REFUSAL_ID}\\n~~~\\nAPPROVED-AT: codex ${HEAD_SHA} BY hermit-901\\n~~~\"},
      {\"body\": \"${REVIEWER_902}\\nAPPROVED-AT: claude ${HEAD_SHA}\"}]" "$HEAD_SHA"

run_message_absent_case "edited fenced marker documentation is inert" \
    0 "edited verdict comment" "$FULL_LABELS" "$FULL_BODY" \
    "[{\"body\": \"${REVIEWER_901}\\nAPPROVED-AT: codex ${HEAD_SHA}\"},
      {\"created_at\": \"2026-08-27T01:00:00Z\", \"updated_at\": \"2026-08-27T02:00:00Z\", \"body\": \"${REVIEWER_901}\\n~~~\\nCHANGES-REQUESTED-AT: claude ${HEAD_SHA}\\n~~~\"},
      {\"body\": \"${REVIEWER_902}\\nAPPROVED-AT: claude ${HEAD_SHA}\"}]" "$HEAD_SHA"

run_message_absent_case "edited indented marker documentation is inert" \
    0 "edited verdict comment" "$FULL_LABELS" "$FULL_BODY" \
    "[{\"body\": \"${REVIEWER_901}\\nAPPROVED-AT: codex ${HEAD_SHA}\"},
      {\"created_at\": \"2026-08-27T01:00:00Z\", \"updated_at\": \"2026-08-27T02:00:00Z\", \"body\": \"${REVIEWER_901}\\n\\n    CHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"},
      {\"body\": \"${REVIEWER_902}\\nAPPROVED-AT: claude ${HEAD_SHA}\"}]" "$HEAD_SHA"

run_case "the issuer's RETIRES clears the named refusal" 0 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"${REVIEWER_901}\\nAPPROVED-AT: codex ${HEAD_SHA}\"},
      {\"id\": ${REFUSAL_ID}, \"body\": \"${REVIEWER_901}\\nCHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"},
      {\"id\": ${WITHDRAWAL_ID}, \"body\": \"${REVIEWER_901}\\nCHANGES-REQUESTED-WITHDRAWN-AT: claude ${HEAD_SHA}\\nRETIRES ${REFUSAL_ID}\"},
      {\"body\": \"${REVIEWER_902}\\nAPPROVED-AT: claude ${HEAD_SHA}\"}]" "$HEAD_SHA"

run_case "a different reviewer's RETIRES cannot clear a signed refusal" 1 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"${REVIEWER_901}\\nAPPROVED-AT: codex ${HEAD_SHA}\"},
      {\"id\": ${REFUSAL_ID}, \"body\": \"${REVIEWER_901}\\nCHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"},
      {\"id\": ${WITHDRAWAL_ID}, \"body\": \"${REVIEWER_902}\\nCHANGES-REQUESTED-WITHDRAWN-AT: claude ${HEAD_SHA}\\nRETIRES ${REFUSAL_ID}\"},
      {\"body\": \"${REVIEWER_902}\\nAPPROVED-AT: claude ${HEAD_SHA}\"}]" "$HEAD_SHA"

run_case "RETIRES naming a different comment retires nothing" 1 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"${REVIEWER_901}\\nAPPROVED-AT: codex ${HEAD_SHA}\"},
      {\"id\": ${REFUSAL_ID}, \"body\": \"${REVIEWER_901}\\nCHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"},
      {\"id\": ${WITHDRAWAL_ID}, \"body\": \"${REVIEWER_901}\\nCHANGES-REQUESTED-WITHDRAWN-AT: claude ${HEAD_SHA}\\nRETIRES 999999\"},
      {\"body\": \"${REVIEWER_902}\\nAPPROVED-AT: claude ${HEAD_SHA}\"}]" "$HEAD_SHA"

run_case "RETIRES is order- and lane-independent because it names the refusing comment" 0 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"${REVIEWER_901}\\nAPPROVED-AT: codex ${HEAD_SHA}\"},
      {\"id\": ${WITHDRAWAL_ID}, \"body\": \"${REVIEWER_901}\\nCHANGES-REQUESTED-WITHDRAWN-AT: codex ${HEAD_SHA}\\nRETIRES ${REFUSAL_ID}\"},
      {\"id\": ${REFUSAL_ID}, \"body\": \"${REVIEWER_901}\\nCHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"},
      {\"body\": \"${REVIEWER_902}\\nAPPROVED-AT: claude ${HEAD_SHA}\"}]" "$HEAD_SHA"

run_message_case "RETIRES clears only the named refusal comment" \
    1 "1 standing refusal(s) remain for claude" "$FULL_LABELS" "$FULL_BODY" \
    "[{\"body\": \"${REVIEWER_901}\\nAPPROVED-AT: codex ${HEAD_SHA}\"},
      {\"id\": ${REFUSAL_ID}, \"body\": \"${REVIEWER_901}\\nCHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"},
      {\"id\": ${SECOND_REFUSAL_ID}, \"body\": \"${REVIEWER_902}\\nCHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"},
      {\"id\": ${WITHDRAWAL_ID}, \"body\": \"${REVIEWER_901}\\nCHANGES-REQUESTED-WITHDRAWN-AT: claude ${HEAD_SHA}\\nRETIRES ${REFUSAL_ID}\"},
      {\"body\": \"${REVIEWER_903}\\nAPPROVED-AT: claude ${HEAD_SHA}\"}]" "$HEAD_SHA"

run_message_case "one unquoted withdrawal leaves another issuer's refusal standing" \
    1 "1 standing refusal(s) remain for claude" "$FULL_LABELS" "$FULL_BODY" \
    "[{\"body\": \"${REVIEWER_901}\\nAPPROVED-AT: codex ${HEAD_SHA}\"},
      {\"id\": ${REFUSAL_ID}, \"body\": \"${REVIEWER_901}\\nCHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"},
      {\"id\": ${SECOND_REFUSAL_ID}, \"body\": \"${REVIEWER_902}\\nCHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"},
      {\"id\": ${WITHDRAWAL_ID}, \"body\": \"${REVIEWER_901}\\nCHANGES-REQUESTED-WITHDRAWN-AT: claude ${HEAD_SHA}\"},
      {\"body\": \"${REVIEWER_903}\\nAPPROVED-AT: claude ${HEAD_SHA}\"}]" "$HEAD_SHA"

run_case "precise and unquoted withdrawals in separate comments both apply" 0 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"${REVIEWER_901}\\nAPPROVED-AT: codex ${HEAD_SHA}\"},
      {\"id\": ${REFUSAL_ID}, \"body\": \"${REVIEWER_901}\\nCHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"},
      {\"id\": ${SECOND_REFUSAL_ID}, \"body\": \"${REVIEWER_902}\\nCHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"},
      {\"id\": ${WITHDRAWAL_ID}, \"body\": \"${REVIEWER_902}\\nCHANGES-REQUESTED-WITHDRAWN-AT: claude ${HEAD_SHA}\"},
      {\"id\": 900004, \"body\": \"${REVIEWER_901}\\nCHANGES-REQUESTED-WITHDRAWN-AT: claude ${HEAD_SHA}\\nRETIRES ${REFUSAL_ID}\"},
      {\"body\": \"${REVIEWER_903}\\nAPPROVED-AT: claude ${HEAD_SHA}\"}]" "$HEAD_SHA"

run_case "one unquoted withdrawal clears all earlier refusals by its issuer" 0 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"${REVIEWER_901}\\nAPPROVED-AT: codex ${HEAD_SHA}\"},
      {\"id\": ${REFUSAL_ID}, \"body\": \"${REVIEWER_901}\\nCHANGES-REQUESTED-AT: claude ${OLD_SHA}\"},
      {\"id\": ${SECOND_REFUSAL_ID}, \"body\": \"${REVIEWER_901}\\nCHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"},
      {\"id\": ${WITHDRAWAL_ID}, \"body\": \"${REVIEWER_901}\\nCHANGES-REQUESTED-WITHDRAWN-AT: claude ${HEAD_SHA}\"},
      {\"body\": \"${REVIEWER_902}\\nAPPROVED-AT: claude ${HEAD_SHA}\"}]" "$HEAD_SHA"

run_message_case "a stale refusal remains standing after another reviewer approves" \
    1 "$OLD_SHA" "$FULL_LABELS" "$FULL_BODY" \
    "[{\"body\": \"${REVIEWER_901}\\nAPPROVED-AT: codex ${HEAD_SHA}\"},
      {\"id\": ${REFUSAL_ID}, \"body\": \"${REVIEWER_901}\\nCHANGES-REQUESTED-AT: claude ${OLD_SHA}\"},
      {\"body\": \"${REVIEWER_902}\\nAPPROVED-AT: claude ${HEAD_SHA}\"}]" "$HEAD_SHA"

# NEGATIVE: the exact defect — labels present, nothing binding the head.
run_case "THE DEFECT: both labels present but NO binding at all blocks" 1 \
    "$FULL_LABELS" "$FULL_BODY" false "[]" "$HEAD_SHA"

run_case "THE DEFECT: both labels present but bindings are at an OLD head blocks" 1 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"APPROVED-AT: codex ${OLD_SHA}\"},
      {\"body\": \"APPROVED-AT: claude ${OLD_SHA}\"}]" "$HEAD_SHA"

run_case "one lane bound, the other stale, blocks" 1 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"APPROVED-AT: codex ${HEAD_SHA}\"},
      {\"body\": \"APPROVED-AT: claude ${OLD_SHA}\"}]" "$HEAD_SHA"

run_case "a later rejection at the head revokes an earlier approval, blocks" 1 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"APPROVED-AT: codex ${HEAD_SHA}\"},
      {\"body\": \"APPROVED-AT: claude ${HEAD_SHA}\"},
      {\"body\": \"CHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"}]" "$HEAD_SHA"

run_case "the historical lane-less rejection revokes both lanes, blocks" 1 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"APPROVED-AT: codex ${HEAD_SHA}\"},
      {\"body\": \"APPROVED-AT: claude ${HEAD_SHA}\"},
      {\"body\": \"REQUEST CHANGES AT ${HEAD_SHA}\"}]" "$HEAD_SHA"

run_case "an approval quoted inside a rejection comment does not bind, blocks" 1 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"APPROVED-AT: codex ${HEAD_SHA}\"},
      {\"body\": \"CHANGES-REQUESTED-AT: claude ${HEAD_SHA}\nSupersedes:\nAPPROVED-AT: claude ${HEAD_SHA}\"}]" \
    "$HEAD_SHA"

# NEGATIVE: unparseable verdict shapes must block, never be skipped. A heading
# prefix is the real case — one Claude-lane rejection was invisible to the
# reference parser for exactly this reason, leaving a stale approval standing.
run_case "a heading-prefixed verdict line is unparseable and blocks" 1 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"APPROVED-AT: codex ${HEAD_SHA}\"},
      {\"body\": \"## APPROVED-AT: claude ${HEAD_SHA}\"}]" "$HEAD_SHA"

run_case "a lane-less APPROVED-AT is unparseable and blocks" 1 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"APPROVED-AT: codex ${HEAD_SHA}\"},
      {\"body\": \"APPROVED-AT ${HEAD_SHA}\"}]" "$HEAD_SHA"

run_case "a prose verdict headline is unparseable and blocks" 1 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"APPROVED-AT: codex ${HEAD_SHA}\"},
      {\"body\": \"APPROVED AT EXACT HEAD ${HEAD_SHA} -- claude review.\"}]" "$HEAD_SHA"

# The three cases above would ALSO block for a second reason — the affected lane
# ends up with no binding — so on their own they do not prove the unparseable-line
# check does anything. These two isolate it: BOTH lanes are validly bound at the
# head, so the only thing that can block is the unparseable line itself. Deleting
# the malformed check leaves the cases above green and fails only these.
run_case "ISOLATES malformed: unparseable line blocks though both lanes bind" 1 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"APPROVED-AT: codex ${HEAD_SHA}\"},
      {\"body\": \"APPROVED-AT: claude ${HEAD_SHA}\"},
      {\"body\": \"LGTM ${HEAD_SHA}\"}]" "$HEAD_SHA"

run_case "ISOLATES malformed: heading-prefixed line blocks though both lanes bind" 1 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"APPROVED-AT: codex ${HEAD_SHA}\"},
      {\"body\": \"APPROVED-AT: claude ${HEAD_SHA}\"},
      {\"body\": \"## APPROVED-AT: claude ${HEAD_SHA}\"}]" "$HEAD_SHA"

# THE PR #2176 CASE, and the reason the leading-marker class exists. A lane
# approves at the head, then posts a rejection at the SAME head as a markdown
# heading. Matching the reference exactly would read the lane as approved and
# PASS the gate, because a heading-prefixed line matches neither the verdict
# grammar nor a suspect pattern without that class. It must refuse.
run_case "a heading-masked rejection at the head must not read as approved" 1 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"APPROVED-AT: codex ${HEAD_SHA}\"},
      {\"body\": \"APPROVED-AT: claude ${HEAD_SHA}\"},
      {\"body\": \"## CHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"}]" "$HEAD_SHA"

# Recovery must work, or the gate is unsatisfiable after any masked rejection.
# Before the rejection grammar learned the prefix class, this case BLOCKED even
# after a clean re-approval: the masked line was merely flagged as unparseable,
# and the OTHER lane had not spoken past it, so the union kept reporting it.
# Honouring the rejection instead of flagging it is what makes recovery clean —
# and it is what the reference verifier does, so the two now agree here.
run_case "a masked rejection followed by its issuer's fresh approval passes again" 0 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"APPROVED-AT: codex ${HEAD_SHA}\"},
      {\"body\": \"${REVIEWER_902}\\nAPPROVED-AT: claude ${HEAD_SHA}\"},
      {\"body\": \"${REVIEWER_901}\\n## CHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"},
      {\"body\": \"${REVIEWER_901}\\nAPPROVED-AT: claude ${HEAD_SHA}\"}]" "$HEAD_SHA"

run_case "a masked claude rejection does not revoke the codex lane" 1 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"APPROVED-AT: codex ${HEAD_SHA}\"},
      {\"body\": \"APPROVED-AT: claude ${HEAD_SHA}\"},
      {\"body\": \"## CHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"}]" "$HEAD_SHA"

run_case "a blockquote-masked rejection at the head must not read as approved" 1 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"APPROVED-AT: codex ${HEAD_SHA}\"},
      {\"body\": \"APPROVED-AT: claude ${HEAD_SHA}\"},
      {\"body\": \"> CHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"}]" "$HEAD_SHA"

run_case "a list-marker-masked rejection at the head must not read as approved" 1 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"APPROVED-AT: codex ${HEAD_SHA}\"},
      {\"body\": \"APPROVED-AT: claude ${HEAD_SHA}\"},
      {\"body\": \"- REQUEST CHANGES AT ${HEAD_SHA}\"}]" "$HEAD_SHA"

# A well-formed rejection for the OTHER lane opens with a verdict keyword and
# must NOT be mistaken for an unparseable line. Without this, tightening the
# malformed check into a false positive would go unnoticed.
run_case "a valid other-lane rejection is not treated as unparseable" 0 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"CHANGES-REQUESTED-AT: claude ${OLD_SHA}\"},
      {\"body\": \"APPROVED-AT: claude ${HEAD_SHA}\"},
      {\"body\": \"APPROVED-AT: codex ${HEAD_SHA}\"}]" "$HEAD_SHA"

# NEGATIVE: the binding inputs themselves must fail closed, or the whole check
# goes quietly inert the first time a caller forgets to pass them.
run_case "missing PR_HEAD_SHA blocks (does not fall back to label presence)" 1 \
    "$FULL_LABELS" "$FULL_BODY" false "$FULL_APPROVALS" ""

run_case "a non-40-hex PR_HEAD_SHA blocks" 1 \
    "$FULL_LABELS" "$FULL_BODY" false "$FULL_APPROVALS" "not-a-sha"

run_case "a truncated PR_HEAD_SHA blocks" 1 \
    "$FULL_LABELS" "$FULL_BODY" false "$FULL_APPROVALS" "${HEAD_SHA:0:12}"

run_case "missing PR_COMMENTS_JSON blocks" 1 \
    "$FULL_LABELS" "$FULL_BODY" false "" "$HEAD_SHA"

run_case "malformed (non-array) PR_COMMENTS_JSON blocks" 1 \
    "$FULL_LABELS" "$FULL_BODY" false '{"body": "not an array"}' "$HEAD_SHA"

run_case "unparseable PR_COMMENTS_JSON blocks" 1 \
    "$FULL_LABELS" "$FULL_BODY" false 'this is not json' "$HEAD_SHA"

# ...and they must be blocked FOR THE RIGHT REASON. See run_message_case.
run_message_case "missing PR_HEAD_SHA is diagnosed as a missing head, not a stale approval" \
    1 "PR_HEAD_SHA is missing" "$FULL_LABELS" "$FULL_BODY" "$FULL_APPROVALS" ""

run_message_case "a non-40-hex PR_HEAD_SHA is diagnosed as a bad head" \
    1 "PR_HEAD_SHA is missing" "$FULL_LABELS" "$FULL_BODY" "$FULL_APPROVALS" "not-a-sha"

run_message_case "missing PR_COMMENTS_JSON is diagnosed as missing comments, not missing approval" \
    1 "PR_COMMENTS_JSON is missing" "$FULL_LABELS" "$FULL_BODY" "" "$HEAD_SHA"

run_message_case "non-array PR_COMMENTS_JSON is diagnosed as a bad comment payload" \
    1 "PR_COMMENTS_JSON is missing" "$FULL_LABELS" "$FULL_BODY" '{"body": "x"}' "$HEAD_SHA"

# The positive direction for the diagnosis too: a genuinely stale approval must
# be reported as stale and must name both SHAs, so an operator can act on it.
run_message_case "a stale approval is diagnosed as superseded and names both SHAs" \
    1 "superseded approval from claude" "$FULL_LABELS" "$FULL_BODY" \
    "[{\"body\": \"APPROVED-AT: codex ${HEAD_SHA}\"},
      {\"body\": \"APPROVED-AT: claude ${OLD_SHA}\"}]" "$HEAD_SHA"

run_message_case "a missing binding is diagnosed as absent, not as stale" \
    1 "no exact-head approval from codex" "$FULL_LABELS" "$FULL_BODY" "[]" "$HEAD_SHA"

# An unlabeled PR is still out of scope even with no binding inputs at all: this
# lint never second-guesses whether the label should have been applied.
run_case "unlabeled PR still passes with no binding inputs" 0 \
    'some-other-label' "" false "" ""

# --- Malformed lines: superseded history vs an open question -----------------
#
# A prose verdict headline that predates the lane's current binding is history.
# One that comes after it could be the rejection the parser could not read.
# Blocking on both would make the gate unsatisfiable for #2176 and #2172, which
# already carry four such lines; blocking on neither would restore the hole.

run_case "an OLD malformed line before a valid re-approval does not block" 0 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"APPROVED AT EXACT HEAD ${OLD_SHA} -- prose headline, unparseable\"},
      {\"body\": \"APPROVED-AT: claude ${HEAD_SHA}\"},
      {\"body\": \"APPROVED-AT: codex ${HEAD_SHA}\"}]" "$HEAD_SHA"

run_case "a malformed line AFTER the newest binding still blocks" 1 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"APPROVED-AT: claude ${HEAD_SHA}\"},
      {\"body\": \"APPROVED-AT: codex ${HEAD_SHA}\"},
      {\"body\": \"APPROVED AT EXACT HEAD ${HEAD_SHA} -- prose headline, unparseable\"}]" \
    "$HEAD_SHA"

run_case "a malformed line in the SAME comment as the newest binding blocks" 1 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"APPROVED-AT: codex ${HEAD_SHA}\"},
      {\"body\": \"APPROVED-AT: claude ${HEAD_SHA}\nLGTM ${HEAD_SHA}\"}]" "$HEAD_SHA"

run_case "an old malformed line still blocks when that lane never re-bound" 1 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"APPROVED AT EXACT HEAD ${OLD_SHA} -- prose headline\"},
      {\"body\": \"APPROVED-AT: codex ${HEAD_SHA}\"}]" "$HEAD_SHA"

# --- WHO MAY APPROVE ----------------------------------------------------------
#
# These fixtures bypass authenticate_comments and state every byte themselves,
# because what they test IS the tagging. Measured against the pre-fix script,
# every "blocks" case below exited 0.
run_raw_case() {
    local name=$1 expected=$2 comments=$3 actual=0
    PR_LABELS="$FULL_LABELS" PR_BODY="$FULL_BODY" PR_IS_KVM=false PR_NUMBER=test \
        PR_HEAD_SHA="$HEAD_SHA" PR_COMMENTS_JSON="$comments" \
        bash "$LINT" >/dev/null 2>&1 || actual=$?
    if [ "$actual" -eq "$expected" ]; then
        echo "ok   - ${name} (exit ${actual})"; pass=$((pass + 1))
    else
        echo "FAIL - ${name}: expected exit ${expected}, got ${actual}"; fail=$((fail + 1))
    fi
}
run_raw_message_case() {
    local name=$1 expected=$2 needle=$3 comments=$4 actual=0 output
    output=$(PR_LABELS="$FULL_LABELS" PR_BODY="$FULL_BODY" PR_IS_KVM=false PR_NUMBER=test \
        PR_HEAD_SHA="$HEAD_SHA" PR_COMMENTS_JSON="$comments" \
        bash "$LINT" 2>&1) || actual=$?
    if [ "$actual" -eq "$expected" ] && [[ $output == *"$needle"* ]]; then
        echo "ok   - ${name} (exit ${actual}, diagnosed)"; pass=$((pass + 1))
    else
        echo "FAIL - ${name}: exit ${actual} (wanted ${expected}), diagnosis '${needle}' $([[ $output == *"$needle"* ]] && echo present || echo MISSING)"
        fail=$((fail + 1))
    fi
}
readonly TAG_REVIEWER='[hermit2, hermit-902, unresolved, devbig030, role=reviewer]'
readonly TAG_RELAY='[hermit2, hermit-bpf-post, unresolved, devbig030, role=relay]'
readonly TAG_LEGACY='[adversarial-reviewer agent, gpt-5.6-sol]'

# THE HOLE. A bare binding, no role tag, posted by the repository OWNER who is
# also the pull request's author. This is the case the previous revision still
# admitted, and it is the one the whole change exists to refuse.
run_raw_case "THE DEFECT: a BARE binding with no role tag does not approve" 1 \
    "[{\"user\": {\"login\": \"owner-and-author\"}, \"author_association\": \"OWNER\",
       \"body\": \"APPROVED-AT: codex ${HEAD_SHA}\"},
      {\"user\": {\"login\": \"owner-and-author\"}, \"author_association\": \"OWNER\",
       \"body\": \"APPROVED-AT: claude ${HEAD_SHA}\"}]"

# ... and it stays refused however high the permission goes.
run_raw_case "a bare binding from an OWNER still does not approve" 1 \
    "[{\"author_association\": \"OWNER\", \"body\": \"APPROVED-AT: codex ${HEAD_SHA}\"},
      {\"author_association\": \"OWNER\", \"body\": \"APPROVED-AT: claude ${HEAD_SHA}\"}]"

# THE REAL SHAPES, which the association model refused. On this very PR both
# approvals were posted by the owner, who was also the author, and one is a
# relay for a reviewer with no route to api.github.com.
run_raw_case "a role-tagged approval posted by the PR AUTHOR is accepted" 0 \
    "[{\"user\": {\"login\": \"owner-and-author\"}, \"author_association\": \"OWNER\",
       \"body\": \"${TAG_REVIEWER}\nAPPROVED-AT: codex ${HEAD_SHA}\"},
      {\"user\": {\"login\": \"owner-and-author\"}, \"author_association\": \"OWNER\",
       \"body\": \"${TAG_RELAY}\nAPPROVED-AT: claude ${HEAD_SHA}\nPROVENANCE: transcribed verbatim.\"}]"

# Association is not reviewer identity, but it is a minimum posting-account
# trust boundary: an arbitrary public commenter cannot mint an approval.
run_raw_case "a role-tagged approval with association NONE is refused" 1 \
    "[{\"user\": {\"login\": \"relay-bot\"}, \"author_association\": \"NONE\",
       \"body\": \"${TAG_REVIEWER}\nAPPROVED-AT: codex ${HEAD_SHA}\"},
      {\"user\": {\"login\": \"relay-bot\"}, \"author_association\": \"NONE\",
       \"body\": \"${TAG_RELAY}\nAPPROVED-AT: claude ${HEAD_SHA}\"}]"

run_raw_case "an untrusted approval copy cannot consume a later trusted issuance" 0 \
    "[{\"author_association\": \"OWNER\", \"body\": \"${REVIEWER_903}\nAPPROVED-AT: codex ${HEAD_SHA}\"},
      {\"author_association\": \"NONE\", \"body\": \"${REVIEWER_901}\nAPPROVED-AT: claude ${HEAD_SHA}\"},
      {\"author_association\": \"OWNER\", \"body\": \"${REVIEWER_901}\nAPPROVED-AT: claude ${HEAD_SHA}\"}]"

# Public comments may copy a disclosure but cannot establish or use citation
# history to suppress a refusal. They still count as standing refusals.
run_raw_message_case "an untrusted refusal cannot consume a later trusted refusal" 1 "1 standing refusal(s) remain for claude" \
    "[
  {
    \"author_association\": \"OWNER\",
    \"body\": \"${REVIEWER_903}\\nAPPROVED-AT: codex ${HEAD_SHA}\"
  },
  {
    \"author_association\": \"NONE\",
    \"body\": \"${REVIEWER_901}\\nCHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"
  },
  {
    \"author_association\": \"OWNER\",
    \"body\": \"${REVIEWER_901}\\nAPPROVED-AT: claude ${HEAD_SHA}\"
  },
  {
    \"author_association\": \"OWNER\",
    \"body\": \"${REVIEWER_901}\\nCHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"
  }
]"

run_raw_message_case "an untrusted legacy refusal cannot consume a later trusted refusal" 1 "1 standing refusal(s) remain for claude" \
    "[
  {
    \"author_association\": \"NONE\",
    \"body\": \"${REVIEWER_901}\\nREQUEST CHANGES AT ${HEAD_SHA}\"
  },
  {
    \"author_association\": \"OWNER\",
    \"body\": \"${REVIEWER_901}\\nAPPROVED-AT: codex ${HEAD_SHA}\"
  },
  {
    \"author_association\": \"OWNER\",
    \"body\": \"${REVIEWER_901}\\nAPPROVED-AT: claude ${HEAD_SHA}\"
  },
  {
    \"author_association\": \"OWNER\",
    \"body\": \"${REVIEWER_901}\\nCHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"
  }
]"

run_raw_message_case "an untrusted refusal cannot use a trusted refusal's citation history" 1 "1 standing refusal(s) remain for claude" \
    "[
  {
    \"author_association\": \"OWNER\",
    \"body\": \"${REVIEWER_903}\\nAPPROVED-AT: codex ${HEAD_SHA}\"
  },
  {
    \"author_association\": \"OWNER\",
    \"body\": \"${REVIEWER_901}\\nCHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"
  },
  {
    \"author_association\": \"OWNER\",
    \"body\": \"${REVIEWER_901}\\nAPPROVED-AT: claude ${HEAD_SHA}\"
  },
  {
    \"author_association\": \"NONE\",
    \"body\": \"${REVIEWER_901}\\nCHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"
  }
]"

run_raw_case "an authenticated repeated refusal retains its citation behavior" 0 \
    "[
  {
    \"author_association\": \"OWNER\",
    \"body\": \"${REVIEWER_903}\\nAPPROVED-AT: codex ${HEAD_SHA}\"
  },
  {
    \"author_association\": \"OWNER\",
    \"body\": \"${REVIEWER_901}\\nCHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"
  },
  {
    \"author_association\": \"OWNER\",
    \"body\": \"${REVIEWER_901}\\nAPPROVED-AT: claude ${HEAD_SHA}\"
  },
  {
    \"author_association\": \"OWNER\",
    \"body\": \"${REVIEWER_901}\\nCHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"
  }
]"

run_raw_case "an untrusted legacy refusal cannot use trusted citation history" 1 \
    "[
  {
    \"author_association\": \"OWNER\",
    \"body\": \"${REVIEWER_901}\\nREQUEST CHANGES AT ${HEAD_SHA}\"
  },
  {
    \"author_association\": \"OWNER\",
    \"body\": \"${REVIEWER_901}\\nAPPROVED-AT: codex ${HEAD_SHA}\"
  },
  {
    \"author_association\": \"OWNER\",
    \"body\": \"${REVIEWER_901}\\nAPPROVED-AT: claude ${HEAD_SHA}\"
  },
  {
    \"author_association\": \"NONE\",
    \"body\": \"${REVIEWER_901}\\nREQUEST CHANGES AT ${HEAD_SHA}\"
  }
]"

run_raw_case "an authenticated repeated legacy refusal retains its citation behavior" 0 \
    "[
  {
    \"author_association\": \"OWNER\",
    \"body\": \"${REVIEWER_901}\\nREQUEST CHANGES AT ${HEAD_SHA}\"
  },
  {
    \"author_association\": \"OWNER\",
    \"body\": \"${REVIEWER_901}\\nAPPROVED-AT: codex ${HEAD_SHA}\"
  },
  {
    \"author_association\": \"OWNER\",
    \"body\": \"${REVIEWER_901}\\nAPPROVED-AT: claude ${HEAD_SHA}\"
  },
  {
    \"author_association\": \"OWNER\",
    \"body\": \"${REVIEWER_901}\\nREQUEST CHANGES AT ${HEAD_SHA}\"
  }
]"

# The fleet's tag shapes vary and the interior is deliberately not parsed;
# approval_binding.py measured that reading it produces wrong verdicts.
run_raw_case "a legacy-shaped role tag is accepted (interior is not parsed)" 0 \
    "[{\"author_association\": \"OWNER\", \"body\": \"${TAG_LEGACY}\nAPPROVED-AT: codex ${HEAD_SHA}\"},
      {\"author_association\": \"OWNER\", \"body\": \"${TAG_LEGACY}\nAPPROVED-AT: claude ${HEAD_SHA}\"}]"

run_raw_case "a tag below an opening heading still counts as a review comment" 0 \
    "[{\"author_association\": \"OWNER\", \"body\": \"## Adversarial review\n${TAG_REVIEWER}\nAPPROVED-AT: codex ${HEAD_SHA}\"},
      {\"author_association\": \"OWNER\", \"body\": \"## Adversarial review\n${TAG_REVIEWER}\nAPPROVED-AT: claude ${HEAD_SHA}\"}]"

run_raw_case "a fenced role tag cannot authenticate an outside approval" 1 \
    "[{\"author_association\": \"OWNER\", \"body\": \"${TAG_REVIEWER}\nAPPROVED-AT: codex ${HEAD_SHA}\"},
      {\"author_association\": \"OWNER\", \"body\": \"~~~\n${TAG_REVIEWER}\n~~~\nAPPROVED-AT: claude ${HEAD_SHA}\"}]"

run_raw_case "an indented role tag cannot authenticate an outside approval" 1 \
    "[{\"author_association\": \"OWNER\", \"body\": \"${TAG_REVIEWER}\nAPPROVED-AT: codex ${HEAD_SHA}\"},
      {\"author_association\": \"OWNER\", \"body\": \"Documentation:\n\n    ${TAG_REVIEWER}\nAPPROVED-AT: claude ${HEAD_SHA}\"}]"

run_raw_case "a fenced disclosure cannot own an outside refusal" 1 \
    "[{\"author_association\": \"OWNER\", \"body\": \"${REVIEWER_903}\nAPPROVED-AT: codex ${HEAD_SHA}\"},
      {\"id\": ${REFUSAL_ID}, \"body\": \"~~~\n${REVIEWER_901}\n~~~\nCHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"},
      {\"author_association\": \"OWNER\", \"body\": \"${REVIEWER_901}\nAPPROVED-AT: claude ${HEAD_SHA}\"}]"

run_raw_case "an indented disclosure cannot own an outside refusal" 1 \
    "[{\"author_association\": \"OWNER\", \"body\": \"${REVIEWER_903}\nAPPROVED-AT: codex ${HEAD_SHA}\"},
      {\"id\": ${REFUSAL_ID}, \"body\": \"Documentation:\n\n    ${REVIEWER_901}\nCHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"},
      {\"author_association\": \"OWNER\", \"body\": \"${REVIEWER_901}\nAPPROVED-AT: claude ${HEAD_SHA}\"}]"

run_raw_case "a fenced RETIRES example does not clear an unsigned refusal" 1 \
    "[{\"author_association\": \"OWNER\", \"body\": \"${REVIEWER_903}\nAPPROVED-AT: codex ${HEAD_SHA}\"},
      {\"id\": ${REFUSAL_ID}, \"body\": \"CHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"},
      {\"author_association\": \"OWNER\", \"body\": \"${REVIEWER_901}\nCHANGES-REQUESTED-WITHDRAWN-AT: claude ${HEAD_SHA}\n~~~\nRETIRES ${REFUSAL_ID}\n~~~\"},
      {\"author_association\": \"OWNER\", \"body\": \"${REVIEWER_902}\nAPPROVED-AT: claude ${HEAD_SHA}\"}]"

run_raw_case "an indented RETIRES example does not clear an unsigned refusal" 1 \
    "[{\"author_association\": \"OWNER\", \"body\": \"${REVIEWER_903}\nAPPROVED-AT: codex ${HEAD_SHA}\"},
      {\"id\": ${REFUSAL_ID}, \"body\": \"CHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"},
      {\"author_association\": \"OWNER\", \"body\": \"${REVIEWER_901}\nCHANGES-REQUESTED-WITHDRAWN-AT: claude ${HEAD_SHA}\n\n    RETIRES ${REFUSAL_ID}\"},
      {\"author_association\": \"OWNER\", \"body\": \"${REVIEWER_902}\nAPPROVED-AT: claude ${HEAD_SHA}\"}]"

# A REJECTION IS NEVER REQUIRED TO BE TAGGED: authority may only be removed,
# never granted, by this check.
run_raw_case "an UNTAGGED rejection still supersedes a tagged approval" 1 \
    "[{\"author_association\": \"OWNER\", \"body\": \"${TAG_REVIEWER}\nAPPROVED-AT: codex ${HEAD_SHA}\"},
      {\"author_association\": \"OWNER\", \"body\": \"${TAG_REVIEWER}\nAPPROVED-AT: claude ${HEAD_SHA}\"},
      {\"body\": \"CHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"}]"

run_raw_case "an unattributed refusal is not discharged by a later approval" 1 \
    "[{\"author_association\": \"OWNER\", \"body\": \"${TAG_REVIEWER}\nAPPROVED-AT: codex ${HEAD_SHA}\"},
      {\"id\": ${REFUSAL_ID}, \"body\": \"CHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"},
      {\"author_association\": \"OWNER\", \"body\": \"${TAG_REVIEWER}\nAPPROVED-AT: claude ${HEAD_SHA}\"}]"

run_raw_case "RETIRES keeps an unsigned refusal's precise exit" 0 \
    "[{\"author_association\": \"OWNER\", \"body\": \"${TAG_REVIEWER}\nAPPROVED-AT: codex ${HEAD_SHA}\"},
      {\"id\": ${REFUSAL_ID}, \"body\": \"CHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"},
      {\"id\": ${WITHDRAWAL_ID}, \"author_association\": \"OWNER\", \"body\": \"${TAG_REVIEWER}\nCHANGES-REQUESTED-WITHDRAWN-AT: claude ${HEAD_SHA}\nRETIRES ${REFUSAL_ID}\"},
      {\"author_association\": \"OWNER\", \"body\": \"${TAG_REVIEWER}\nAPPROVED-AT: claude ${HEAD_SHA}\"}]"

run_raw_message_case "RETIRES of one unsigned refusal leaves the other comment standing" \
    1 "1 standing refusal(s) remain for claude" \
    "[{\"author_association\": \"OWNER\", \"body\": \"${TAG_REVIEWER}\nAPPROVED-AT: codex ${HEAD_SHA}\"},
      {\"id\": ${REFUSAL_ID}, \"body\": \"CHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"},
      {\"id\": ${SECOND_REFUSAL_ID}, \"body\": \"CHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"},
      {\"id\": ${WITHDRAWAL_ID}, \"author_association\": \"OWNER\", \"body\": \"${TAG_REVIEWER}\nCHANGES-REQUESTED-WITHDRAWN-AT: claude ${HEAD_SHA}\nRETIRES ${REFUSAL_ID}\"},
      {\"author_association\": \"OWNER\", \"body\": \"${TAG_REVIEWER}\nAPPROVED-AT: claude ${HEAD_SHA}\"}]"

run_raw_case "an unsigned RETIRES cannot clear a signed refusal" 1 \
    "[{\"author_association\": \"OWNER\", \"body\": \"${TAG_REVIEWER}\nAPPROVED-AT: codex ${HEAD_SHA}\"},
      {\"id\": ${REFUSAL_ID}, \"body\": \"${TAG_REVIEWER}\nCHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"},
      {\"id\": ${WITHDRAWAL_ID}, \"body\": \"CHANGES-REQUESTED-WITHDRAWN-AT: claude ${HEAD_SHA}\nRETIRES ${REFUSAL_ID}\"},
      {\"author_association\": \"OWNER\", \"body\": \"${TAG_REVIEWER}\nAPPROVED-AT: claude ${HEAD_SHA} BY hermit-903\"}]"

run_raw_case "an unquoted withdrawal cannot clear an unsigned refusal" 1 \
    "[{\"author_association\": \"OWNER\", \"body\": \"${TAG_REVIEWER}\nAPPROVED-AT: codex ${HEAD_SHA}\"},
      {\"id\": ${REFUSAL_ID}, \"body\": \"CHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"},
      {\"id\": ${WITHDRAWAL_ID}, \"author_association\": \"OWNER\", \"body\": \"${TAG_REVIEWER}\nCHANGES-REQUESTED-WITHDRAWN-AT: claude ${HEAD_SHA}\"},
      {\"author_association\": \"OWNER\", \"body\": \"${TAG_REVIEWER}\nAPPROVED-AT: claude ${HEAD_SHA}\"}]"

run_raw_case "a forged-disclosure non-member unquoted withdrawal cannot discharge" 1 \
    "[{\"author_association\": \"OWNER\", \"body\": \"${REVIEWER_903}\nAPPROVED-AT: codex ${HEAD_SHA}\"},
      {\"id\": ${REFUSAL_ID}, \"author_association\": \"OWNER\", \"body\": \"${REVIEWER_901}\nCHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"},
      {\"id\": ${WITHDRAWAL_ID}, \"author_association\": \"NONE\", \"body\": \"${REVIEWER_901}\nCHANGES-REQUESTED-WITHDRAWN-AT: claude ${HEAD_SHA}\"},
      {\"author_association\": \"OWNER\", \"body\": \"${REVIEWER_902}\nAPPROVED-AT: claude ${HEAD_SHA}\"}]"

run_raw_case "trusted unquoted withdrawal with the same text still discharges" 0 \
    "[{\"author_association\": \"OWNER\", \"body\": \"${REVIEWER_903}\nAPPROVED-AT: codex ${HEAD_SHA}\"},
      {\"id\": ${REFUSAL_ID}, \"author_association\": \"OWNER\", \"body\": \"${REVIEWER_901}\nCHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"},
      {\"id\": ${WITHDRAWAL_ID}, \"author_association\": \"OWNER\", \"body\": \"${REVIEWER_901}\nCHANGES-REQUESTED-WITHDRAWN-AT: claude ${HEAD_SHA}\"},
      {\"author_association\": \"OWNER\", \"body\": \"${REVIEWER_902}\nAPPROVED-AT: claude ${HEAD_SHA}\"}]"

run_raw_case "a forged-disclosure non-member precise RETIRES cannot discharge" 1 \
    "[{\"author_association\": \"OWNER\", \"body\": \"${REVIEWER_903}\nAPPROVED-AT: codex ${HEAD_SHA}\"},
      {\"id\": ${REFUSAL_ID}, \"author_association\": \"OWNER\", \"body\": \"${REVIEWER_901}\nCHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"},
      {\"id\": ${WITHDRAWAL_ID}, \"author_association\": \"NONE\", \"body\": \"${REVIEWER_901}\nCHANGES-REQUESTED-WITHDRAWN-AT: claude ${HEAD_SHA}\nRETIRES ${REFUSAL_ID}\"},
      {\"author_association\": \"OWNER\", \"body\": \"${REVIEWER_902}\nAPPROVED-AT: claude ${HEAD_SHA}\"}]"

run_raw_case "trusted precise RETIRES with the same text still discharges" 0 \
    "[{\"author_association\": \"OWNER\", \"body\": \"${REVIEWER_903}\nAPPROVED-AT: codex ${HEAD_SHA}\"},
      {\"id\": ${REFUSAL_ID}, \"author_association\": \"OWNER\", \"body\": \"${REVIEWER_901}\nCHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"},
      {\"id\": ${WITHDRAWAL_ID}, \"author_association\": \"OWNER\", \"body\": \"${REVIEWER_901}\nCHANGES-REQUESTED-WITHDRAWN-AT: claude ${HEAD_SHA}\nRETIRES ${REFUSAL_ID}\"},
      {\"author_association\": \"OWNER\", \"body\": \"${REVIEWER_902}\nAPPROVED-AT: claude ${HEAD_SHA}\"}]"

# ⚠️ A LEADING WORD IS NOT STRUCTURE, AND EVERY ONE OF THESE ONCE ADMITTED.
# `strip_structural_prefix` removes headings, quotes and list markers; it does not
# remove an arbitrary word, and the detectors were all `^`-anchored, so a rejection
# opening with one matched NOTHING -- not a binding, and not the malformed check --
# and was silently dropped. For a rejection, dropped means ADMITTED.
#
# ⚠️ THE THIRD AND FOURTH CASES ARE THE FORM FROM THE INCIDENT THIS GATE CITES:
# `## CODEX-LANE CHANGES-REQUESTED-AT: codex <sha>` was live on
# https://github.com/rrnewton/hermit/pull/2176, named in the linter's own header as
# the motivating case. The gate closed the heading variant and left this one open.
# Measured before the fix: cases 1 and 2 blocked, cases 3 to 6 admitted, against a
# valid admit-control.
for prefixed_rejection in \
    "## CODEX-LANE CHANGES-REQUESTED-AT: claude ${HEAD_SHA}" \
    "CODEX-LANE CHANGES-REQUESTED-AT: claude ${HEAD_SHA}" \
    "Note: CHANGES-REQUESTED-AT: claude ${HEAD_SHA}" \
    "**Codex lane** CHANGES-REQUESTED-AT: claude ${HEAD_SHA}"; do
    run_case "a word-prefixed rejection is not silently dropped: ${prefixed_rejection:0:44}" 1 \
        "$FULL_LABELS" "$FULL_BODY" false \
        "[{\"body\": \"APPROVED-AT: claude ${HEAD_SHA}\"},
          {\"body\": \"APPROVED-AT: codex ${HEAD_SHA}\"},
          {\"body\": \"${prefixed_rejection}\"}]" "$HEAD_SHA"
done

# THE CONTROL FOR THE FOUR ABOVE. Every one of them asserts exit 1, and a linter
# that refused everything would satisfy all four. The same inputs WITHOUT the
# prefixed line must still admit, or they prove nothing.
run_case "the prefixed-rejection control still admits without one" 0 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"APPROVED-AT: claude ${HEAD_SHA}\"},
      {\"body\": \"APPROVED-AT: codex ${HEAD_SHA}\"}]" "$HEAD_SHA"

run_case "an ordered-list rejection still revokes" 1 "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"APPROVED-AT: codex ${HEAD_SHA}\"},
      {\"body\": \"APPROVED-AT: claude ${HEAD_SHA}\"},
      {\"body\": \"1. CHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"}]" "$HEAD_SHA"

run_case "a task-list rejection still revokes" 1 "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"APPROVED-AT: codex ${HEAD_SHA}\"},
      {\"body\": \"APPROVED-AT: claude ${HEAD_SHA}\"},
      {\"body\": \"- [ ] CHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"}]" "$HEAD_SHA"

run_case "a truncated explicit rejection is unparseable and blocks" 1 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"APPROVED-AT: codex ${HEAD_SHA}\"},
      {\"body\": \"APPROVED-AT: claude ${HEAD_SHA}\"},
      {\"body\": \"CHANGES-REQUESTED-AT: claude 0123456789ab\"}]" "$HEAD_SHA"
run_case "a no-space truncated rejection is unparseable and blocks" 1 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"APPROVED-AT: codex ${HEAD_SHA}\"},
      {\"body\": \"APPROVED-AT: claude ${HEAD_SHA}\"},
      {\"body\": \"CHANGES-REQUESTED-AT:claude 0123456789ab\"}]" "$HEAD_SHA"

run_case "a no-space truncated approval is unparseable and blocks" 1 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"APPROVED-AT: codex ${HEAD_SHA}\"},
      {\"body\": \"APPROVED-AT: claude ${HEAD_SHA}\"},
      {\"body\": \"APPROVED-AT:claude 0123456789ab\"}]" "$HEAD_SHA"


run_case "an edited older verdict comment blocks instead of using creation order" 1 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"created_at\": \"2026-08-25T01:00:00Z\", \"updated_at\": \"2026-08-25T04:00:00Z\", \"body\": \"CHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"},
      {\"created_at\": \"2026-08-25T02:00:00Z\", \"updated_at\": \"2026-08-25T02:00:00Z\", \"body\": \"APPROVED-AT: codex ${HEAD_SHA}\"},
      {\"created_at\": \"2026-08-25T03:00:00Z\", \"updated_at\": \"2026-08-25T03:00:00Z\", \"body\": \"APPROVED-AT: claude ${HEAD_SHA}\"}]" "$HEAD_SHA"

# MALFORMED PAYLOAD MUST FAIL CLOSED. A non-object element makes jq raise and
# ABORT, so rows after it are dropped. Placed between the approval and the
# rejection that supersedes it, that deletes the rejection and the stale
# approval stands: a fail-OPEN reachable from comment content.
run_raw_case "THE DEFECT: a non-object element hiding a later rejection blocks" 1 \
    "[{\"body\": \"${TAG_REVIEWER}\nAPPROVED-AT: codex ${HEAD_SHA}\"},
      {\"body\": \"${TAG_REVIEWER}\nAPPROVED-AT: claude ${HEAD_SHA}\"},
      \"not-an-object\",
      {\"body\": \"CHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"}]"

run_raw_message_case "a malformed element is diagnosed as a refusal to read a truncated stream" \
    1 "not a JSON object" \
    "[{\"body\": \"${TAG_REVIEWER}\nAPPROVED-AT: codex ${HEAD_SHA}\"}, 42]"

run_raw_message_case "a bare binding is diagnosed as not-a-review-comment, not as absent" \
    1 "not a review comment" \
    "[{\"body\": \"APPROVED-AT: codex ${HEAD_SHA}\"},
      {\"body\": \"APPROVED-AT: claude ${HEAD_SHA}\"}]"

# --- Diagnostic specificity --------------------------------------------------
run_raw_message_case "a bare binding diagnosis names the role-tag requirement" \
    1 "bracketed reviewer or relay role tag" \
    "[{\"author_association\": \"OWNER\", \"body\": \"APPROVED-AT: codex ${HEAD_SHA}\"},
      {\"author_association\": \"OWNER\", \"body\": \"APPROVED-AT: claude ${HEAD_SHA}\"}]"

# --- PR_COMMENTS_FILE, the form the workflow actually uses --------------------

comments_tmp=$(mktemp)
authenticate_comments "$FULL_APPROVALS" > "$comments_tmp"
actual=0
PR_LABELS="$FULL_LABELS" PR_BODY="$FULL_BODY" PR_IS_KVM=false PR_NUMBER=test \
    PR_HEAD_SHA="$HEAD_SHA" PR_COMMENTS_FILE="$comments_tmp" \
    PR_AUTHOR="$PR_AUTHOR_FIXTURE" \
    bash "$LINT" >/dev/null 2>&1 || actual=$?
if [ "$actual" -eq 0 ]; then
    echo "ok   - PR_COMMENTS_FILE is read and a valid binding passes (exit 0)"
    pass=$((pass + 1))
else
    echo "FAIL - PR_COMMENTS_FILE is read and a valid binding passes: got ${actual}"
    fail=$((fail + 1))
fi
rm -f "$comments_tmp"

# PR_COMMENTS_FILE must WIN over PR_COMMENTS_JSON, or the workflow's real input
# could be silently shadowed by a stale inline value.
comments_tmp=$(mktemp)
authenticate_comments "$(printf '[{"body": "APPROVED-AT: codex %s"}]' "$OLD_SHA")" > "$comments_tmp"
actual=0
PR_LABELS="$FULL_LABELS" PR_BODY="$FULL_BODY" PR_IS_KVM=false PR_NUMBER=test \
    PR_HEAD_SHA="$HEAD_SHA" PR_COMMENTS_FILE="$comments_tmp" \
    PR_COMMENTS_JSON="$FULL_APPROVALS" PR_AUTHOR="$PR_AUTHOR_FIXTURE" \
    bash "$LINT" >/dev/null 2>&1 || actual=$?
if [ "$actual" -eq 1 ]; then
    echo "ok   - PR_COMMENTS_FILE takes precedence over PR_COMMENTS_JSON (exit 1)"
    pass=$((pass + 1))
else
    echo "FAIL - PR_COMMENTS_FILE takes precedence over PR_COMMENTS_JSON: got ${actual}"
    fail=$((fail + 1))
fi
rm -f "$comments_tmp"

PR_COMMENTS_FILE=/nonexistent/path/comments.json \
  run_message_case "an unreadable PR_COMMENTS_FILE is diagnosed as a plumbing fault" \
    1 "is not readable" "$FULL_LABELS" "$FULL_BODY" "" "$HEAD_SHA"

echo
echo "core-review-protocol-lint self-test: ${pass} passed, ${fail} failed."
[ "$fail" -eq 0 ]
