#!/usr/bin/env bash
# Enforce the post-facto-human-review core-change review protocol on one PR.
#
# Background: after PR #1095 landed a core change without the required dual
# adversarial review, the review protocol lived only in agent skills that a
# forgetful actor could bypass. This script is the "code that never forgets"
# version: a preland/merge-gate lint that BLOCKS landing when a PR carries the
# `post-facto-human-review` label but has not actually been reviewed and
# approved, or is missing a required PR-body section.
#
# A PR labeled `post-facto-human-review` may only land when ALL hold:
#   (a) adversarial review happened for BOTH reviewers: at least one
#       `adversarial-review-codex<N>` AND at least one
#       `adversarial-review-claude<N>` label, for a round N in 1..4;
#   (b) the LATEST reviews approved: `passed-review-codex` AND
#       `passed-review-claude` (these are invalidated on every new push, so
#       their presence means the current revision is approved);
#   (c) the PR body contains the required sections: Summary, Determinism,
#       Linux Semantics, Validation, Human Review Required, and — when the PR
#       touches KVM — Relationship to gVisor.
#
# A PR WITHOUT the `post-facto-human-review` label passes unconditionally; this
# lint never second-guesses whether the label should have been applied.
#
# Inputs (environment variables):
#   PR_LABELS  newline-separated label names on the PR (may be empty)
#   PR_BODY    the PR description body text (may be empty)
#   PR_IS_KVM  "true" when the PR changes KVM code (default: "false")
#   PR_NUMBER  PR number, used only in diagnostics (default: "unknown")
#
# Exit status:
#   0  protocol satisfied, or the PR is not labeled post-facto-human-review
#   1  the PR is labeled but violates the protocol (landing must be blocked)
#   2  usage / internal error

set -euo pipefail

pr="${PR_NUMBER:-unknown}"
labels="${PR_LABELS-}"
body="${PR_BODY-}"
is_kvm="${PR_IS_KVM:-false}"

# The valid adversarial-review round labels, per reviewer (rounds 1..4).
readonly REVIEW_ROUND_RANGE='[1-4]'

# True when an exact label name is present (full-line match).
has_label() {
    printf '%s\n' "$labels" | grep -Fxq -- "$1"
}

# True when any label matches the given extended regex, anchored to a full line.
has_label_matching() {
    printf '%s\n' "$labels" | grep -Eq -- "^($1)\$"
}

# True when the body contains SECTION as a heading. Accepts a Markdown ATX
# heading (`## Section`), a bold label (`**Section**`), or a bare heading line
# ending in a colon (`Section:`), matched case-insensitively at line start. The
# leading marker requirement keeps prose mentions ("in summary, ...") from
# counting as the section.
has_section() {
    local section=$1
    printf '%s\n' "$body" | grep -Eiq \
        "^[[:space:]]*(#{1,6}[[:space:]]*${section}|\*\*[[:space:]]*${section}|${section}[[:space:]]*:)"
}

if ! has_label post-facto-human-review; then
    echo "PR #${pr}: no post-facto-human-review label; core-review protocol not applicable."
    exit 0
fi

echo "PR #${pr}: post-facto-human-review present; enforcing the core-change review protocol."

errors=0
fail() {
    echo "::error::PR #${pr}: $*"
    errors=$((errors + 1))
}

# (a) Adversarial review happened for both reviewers (any round 1..4).
if ! has_label_matching "adversarial-review-codex${REVIEW_ROUND_RANGE}"; then
    fail "no adversarial review from codex (need one of adversarial-review-codex1..4)."
fi
if ! has_label_matching "adversarial-review-claude${REVIEW_ROUND_RANGE}"; then
    fail "no adversarial review from claude (need one of adversarial-review-claude1..4)."
fi

# (b) The latest reviews approved.
has_label passed-review-codex \
    || fail "missing passed-review-codex (codex has not approved the current revision)."
has_label passed-review-claude \
    || fail "missing passed-review-claude (claude has not approved the current revision)."

# (c) Required PR-body sections.
for section in "Summary" "Determinism" "Linux Semantics" "Validation" "Human Review Required"; do
    has_section "$section" \
        || fail "PR body is missing the required \"${section}\" section."
done
if [ "$is_kvm" = true ]; then
    has_section "Relationship to gVisor" \
        || fail "KVM change: PR body is missing the required \"Relationship to gVisor\" section."
fi

if [ "$errors" -gt 0 ]; then
    echo "::error::PR #${pr}: core-change review protocol NOT satisfied (${errors} problem(s)); blocking landing."
    exit 1
fi

echo "PR #${pr}: core-change review protocol satisfied."
