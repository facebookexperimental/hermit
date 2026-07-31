#!/usr/bin/env bash
# Self-test for core-review-protocol-lint.sh.
#
# Feeds the linter a set of fixture PRs (labels + body + KVM flag) and asserts
# the exit status. Run locally or in CI:
#
#     scripts/core-review-protocol-lint-test.sh
#
# Exits 0 when every case matches its expected status, 1 otherwise.

set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
readonly LINT="$SCRIPT_DIR/core-review-protocol-lint.sh"

# A complete, valid non-KVM PR body containing every required section.
readonly FULL_BODY='## Summary
Adds a thing.

## Determinism
Deterministic because reasons and an informal proof.

## Linux Semantics
Matches the kernel behavior described here.

## Validation
`cargo test -p detcore` passed at L2 (ptrace).

## Human Review Required
Trigger 4: core DetCore scheduling change.'

# The label set for a fully reviewed + approved PR (round 1).
readonly FULL_LABELS='post-facto-human-review
adversarial-review-codex1
adversarial-review-claude1
passed-review-codex
passed-review-claude'

pass=0
fail=0

# run_case NAME EXPECTED_EXIT LABELS BODY IS_KVM
run_case() {
    local name=$1 expected=$2 labels=$3 body=$4 is_kvm=${5:-false}
    local actual=0
    PR_LABELS="$labels" PR_BODY="$body" PR_IS_KVM="$is_kvm" PR_NUMBER="test" \
        bash "$LINT" >/dev/null 2>&1 || actual=$?
    if [ "$actual" -eq "$expected" ]; then
        echo "ok   - ${name} (exit ${actual})"
        pass=$((pass + 1))
    else
        echo "FAIL - ${name}: expected exit ${expected}, got ${actual}"
        fail=$((fail + 1))
    fi
}

# --- Not applicable: no post-facto-human-review label always passes. ----------
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

echo
echo "core-review-protocol-lint self-test: ${pass} passed, ${fail} failed."
[ "$fail" -eq 0 ]
