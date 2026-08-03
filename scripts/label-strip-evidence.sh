#!/usr/bin/env bash
# label-strip-evidence.sh — never lose the record of what a PR previously
# validated when the `locally-validated` label is stripped.
#
# Background: the `invalidate-local-validation` merge-gate job DELETES the
# `locally-validated` label on every push (`pull_request: synchronize`), and
# agents/tooling sometimes remove it by hand (`gh pr edit --remove-label`,
# `gh api DELETE .../labels/locally-validated`, or a remove+add re-fire toggle).
# Each strip silently erased the record of which commit was validated, with what
# profile, and where the durable log lives. This helper posts a durable comment
# at strip time that preserves that evidence, quoting the original add-time
# evidence comment that `validate.sh` leaves when it applies the label.
#
# Design contract: this is BEST-EFFORT observability. It ALWAYS exits 0 so it can
# never fail the invalidate job (whose success merge-gate requires) and can never
# block landing. Failures print a warning and are swallowed.
#
# Callers:
#   * merge-gate.yml `invalidate-local-validation` job (automated on-push strip).
#   * agents/tooling doing a MANUAL strip — run this BEFORE removing the label,
#     or pass --remove to have it strip the label after commenting.
#
# The add-time evidence comment (written by validate.sh) carries a machine
# marker `<!-- locally-validated-evidence sha=... profile=... host=... ts=... -->`
# so this helper can locate and quote it even across many PR comments.

set -uo pipefail

REPO="rrnewton/hermit"
PR=""
VALIDATED_SHA=""
NEW_SHA=""
REASON="the PR head advanced, so prior local validation no longer applies"
LABELS_CLEARED=""
ACTOR=""
DO_REMOVE=0
LABEL="locally-validated"

usage() {
    cat >&2 <<'EOF'
Usage: label-strip-evidence.sh --pr N [options]

Required:
  --pr N                 Pull request number.

Options:
  --validated-sha SHA    Commit the stripped validation applied to
                         (on-push strip: the pre-push head / github.event.before).
  --new-sha SHA          New head that invalidated it (github.event.after).
  --reason TEXT          Human reason for the strip (default: head advanced).
  --labels "a b c"       Space-separated labels being cleared (for the record).
  --actor NAME           Who/what triggered the strip (agent, automation, ...).
  --repo OWNER/NAME      Repository (default: rrnewton/hermit).
  --remove               Also delete the `locally-validated` label after
                         commenting (for MANUAL strips; the workflow deletes it
                         itself and must NOT pass this).
  -h, --help             Show this help.

Always exits 0 (best-effort; never blocks landing).
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --pr) PR="${2:-}"; shift 2 ;;
        --validated-sha) VALIDATED_SHA="${2:-}"; shift 2 ;;
        --new-sha) NEW_SHA="${2:-}"; shift 2 ;;
        --reason) REASON="${2:-}"; shift 2 ;;
        --labels) LABELS_CLEARED="${2:-}"; shift 2 ;;
        --actor) ACTOR="${2:-}"; shift 2 ;;
        --repo) REPO="${2:-}"; shift 2 ;;
        --remove) DO_REMOVE=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) printf '⚠️  label-strip-evidence: unknown argument %q (ignored)\n' "$1" >&2; shift ;;
    esac
done

if [[ -z $PR ]]; then
    printf '⚠️  label-strip-evidence: --pr is required; nothing to do\n' >&2
    exit 0
fi

# gh on Meta devservers needs the forward proxy; mirror validate.sh.
gh_cmd=(gh)
if command -v with-proxy >/dev/null 2>&1; then
    gh_cmd=(with-proxy gh)
fi
if ! command -v gh >/dev/null 2>&1; then
    printf '⚠️  label-strip-evidence: gh CLI not found; skipping evidence comment for PR #%s\n' "$PR" >&2
    exit 0
fi

# A GitHub commit-status-style short SHA for headings; tolerate empty/unknown.
short() { local s="${1:-}"; if [[ -n $s && $s != 0000000000000000000000000000000000000000 ]]; then printf '%s' "${s:0:12}"; else printf 'unknown'; fi; }

timestamp="$(date -u +'%Y-%m-%dT%H:%M:%SZ' 2>/dev/null || echo unknown)"

# Locate the prior add-time evidence comment so the record survives even if this
# is the only place the strip is noticed. Prefer one whose marker SHA matches the
# validated SHA; otherwise take the most recent evidence comment on the PR.
prior_evidence=""
comments_json="$("${gh_cmd[@]}" api --paginate \
    "repos/${REPO}/issues/${PR}/comments" 2>/dev/null || true)"
if [[ -n $comments_json ]]; then
    if [[ -n $VALIDATED_SHA ]]; then
        prior_evidence="$(jq -r --arg sha "$VALIDATED_SHA" '
            [.[] | select(.body != null)
                 | select(.body | contains("locally-validated-evidence"))
                 | select(.body | contains("sha=" + $sha))]
            | last // {} | .body // ""' <<<"$comments_json" 2>/dev/null || true)"
    fi
    if [[ -z $prior_evidence ]]; then
        prior_evidence="$(jq -r '
            [.[] | select(.body != null)
                 | select(.body | contains("locally-validated-evidence"))]
            | last // {} | .body // ""' <<<"$comments_json" 2>/dev/null || true)"
    fi
fi

# Quote the prior evidence as a Markdown blockquote so it renders verbatim.
if [[ -n $prior_evidence ]]; then
    quoted="$(printf '%s\n' "$prior_evidence" | sed 's/^/> /')"
    evidence_block=$'**Preserved prior validation evidence (add-time comment):**\n\n'"$quoted"
else
    evidence_block=$'**Preserved prior validation evidence:** none found on this PR — the label may have been applied outside `validate.sh`. The validated commit is recorded above so the record is not wholly lost.'
fi

actor_line=""
[[ -n $ACTOR ]] && actor_line=$'\n- Triggered by: `'"$ACTOR"$'`'
labels_line=""
[[ -n $LABELS_CLEARED ]] && labels_line=$'\n- Labels cleared: `'"$LABELS_CLEARED"$'`'

body="$(cat <<EOF
[merge-gate, label-strip-evidence]

🔻 \`${LABEL}\` was stripped from this PR — recording the evidence so the validation record is never lost.

- Validation applied to SHA: \`$(short "$VALIDATED_SHA")\` (\`${VALIDATED_SHA:-unknown}\`)
- New head SHA: \`$(short "$NEW_SHA")\` (\`${NEW_SHA:-n/a}\`)
- Reason: ${REASON}
- Stripped at (UTC): \`${timestamp}\`${actor_line}${labels_line}

${evidence_block}

<!-- locally-validated-strip sha=${VALIDATED_SHA:-unknown} new=${NEW_SHA:-none} ts=${timestamp} -->
EOF
)"

if "${gh_cmd[@]}" pr comment "$PR" --repo "$REPO" --body "$body" >/dev/null 2>&1; then
    printf '💬 label-strip-evidence: recorded strip evidence on PR #%s (validated %s)\n' \
        "$PR" "$(short "$VALIDATED_SHA")"
else
    printf '⚠️  label-strip-evidence: failed to comment strip evidence on PR #%s (non-fatal)\n' "$PR" >&2
fi

if [[ $DO_REMOVE -eq 1 ]]; then
    if "${gh_cmd[@]}" api --method DELETE \
        "repos/${REPO}/issues/${PR}/labels/${LABEL}" --silent >/dev/null 2>&1; then
        printf '🏷️  label-strip-evidence: removed %s from PR #%s\n' "$LABEL" "$PR"
    else
        printf 'ℹ️  label-strip-evidence: %s not present / not removed on PR #%s\n' "$LABEL" "$PR" >&2
    fi
fi

exit 0
