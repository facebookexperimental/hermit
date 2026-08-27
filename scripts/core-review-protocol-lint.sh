#!/usr/bin/env bash
# Enforce the core-change review protocol on one pull request.
#
# Background: after PR #1095 landed a core change without the required dual
# adversarial review, the review protocol lived only in agent skills that a
# forgetful actor could bypass. This script is the "code that never forgets"
# version: a preland/merge-gate lint that BLOCKS landing when a PR carries the
# contract's post-facto label but has not actually been reviewed and approved,
# or is missing a required PR-body section.
#
# A PR carrying that label may only land when ALL hold:
#   (a) every review family in the shared contract has a numbered activity label;
#   (b) every family has its approval label;
#   (b') every family's newest verdict comment binds the exact current head with
#        `APPROVED-AT: <family> <40-hex>`;
#   (c) the PR body contains the required sections: Summary, Determinism,
#       Linux Semantics, Validation, Human Review Required, and — when the PR
#       touches KVM — Relationship to gVisor.
#
# A PR without the contract's post-facto label passes unconditionally; this
# lint never second-guesses whether the label should have been applied.
#
# WHY (b') EXISTS. Until 2026-08-13 this lint checked only (b) — label presence
# — and there was no SHA anywhere in it. A label is a cache: it survives new
# commits and force-pushes, while an approval does not. The producer that was
# supposed to strip a stale label (`merge-gate.yml`'s `invalidate-local-validation`)
# is itself a `pull_request` job on a self-hosted runner, so it fails open
# silently whenever no run fires or the runner is down — and this consumer
# trusted it unconditionally, while its own failure text asserted "has not
# approved the current revision", a claim it had no means to evaluate.
# Composed, the merge gate was satisfiable by an approval of different code.
# Observed live on PR #2176: both `passed-review-*` labels present, NEITHER lane
# binding the current head, and the newest claude-lane verdict at that head a
# REJECTION with nine open findings.
#
# The fix is the one in ai_docs/2026-08-10-stale-approval-label-scope-and-binding.md
# (R1): derive validity at read time instead of depending on an event to remove
# an artifact. There is no run to miss and no runner to be offline; if the head
# moved, the comparison simply fails here. Nothing is deleted, so no review
# history is lost — a superseded approval stops satisfying the gate and remains
# visible forever as the record of what was reviewed and when.
#
# The labels are still required. Keeping (b) alongside (b') makes this gate
# strictly stronger than before in every case, which is the only safe direction
# for a change to an authorization check.
#
# GRAMMAR. Mirrors the reference verifier `ci-hub/health/approval_binding.py`
# in the dev-hermit parent, which the landing path (`ci-hub/landing/land-pr.sh`)
# and `ci-hub/health/pr_status.py` already call. Exactly one shape carries
# authority:
#
#     APPROVED-AT: <claude|codex> <40-hex> [BY <agent>]
#
# matched case-insensitively against the whole line after whole-line markdown
# emphasis is removed. Rejections mirror it and win within a comment:
#
#     CHANGES-REQUESTED-AT: <claude|codex> <40-hex> [BY <agent>]
#     CHANGES-REQUESTED-WITHDRAWN-AT: <claude|codex> <40-hex> [BY <agent>]
#     REQUEST CHANGES AT <40-hex>              (historical, lane-less)
#
# Verdicts are chronological: a rejection clears that lane's earlier approvals.
# It remains a standing refusal until its own issuer later approves that lane,
# withdraws its outstanding refusals with an unquoted canonical withdrawal, or precisely
# names the refusing comment with `RETIRES <comment-id>`. A different reviewer
# cannot discharge an attributed refusal. A lane binds only when the NEWEST SHA
# it bound itself to equals the current head and no refusal remains. A line
# carrying a verdict-ish keyword and a 40-hex that matches no known shape is
# reported and BLOCKS rather than being skipped — an unrecognised variant that
# is silently ignored reads as no approval, and a heading-prefixed verdict line
# (`## APPROVED-AT: ...`) is exactly how one real rejection went unseen.
#
# Inputs (environment variables):
#   PR_LABELS         newline-separated label names on the PR (may be empty)
#   PR_BODY           the PR description body text (may be empty)
#   PR_IS_KVM         "true" when the PR changes KVM code (default: "false")
#   PR_NUMBER         PR number, used only in diagnostics (default: "unknown")
#   PR_HEAD_SHA       the exact 40-hex head the approvals must bind
#   PR_COMMENTS_FILE  path to a file holding a JSON array of the PR's issue
#                     comments, oldest first, each object carrying at least
#                     `body`; `id` is required for `RETIRES`. PREFERRED, and what
#                     the workflow uses.
#   PR_COMMENTS_JSON  the same JSON inline. Convenient for tests and small PRs;
#                     PR_COMMENTS_FILE takes precedence when both are set.
#
# USE THE FILE FORM FOR ANYTHING REAL. A single environment variable is capped
# at MAX_ARG_STRLEN (128 KiB on Linux), and PR #2176's comment array is
# 154,666 bytes — passing it inline fails exec with E2BIG, which surfaces as
# exit 126 and blocks a PR for a reason that has nothing to do with its reviews.
# Comment volume grows with exactly the review activity this gate reads, so the
# inline form breaks first on the PRs that need the gate most.
#
# PR_HEAD_SHA and one of PR_COMMENTS_FILE / PR_COMMENTS_JSON are REQUIRED
# whenever the `post-facto-human-review` label is present; a missing, malformed,
# or unparseable value BLOCKS. They are deliberately not optional: a check that
# goes quietly inert when its caller forgets an input is the same fail-open
# shape this change exists to remove.
#
# Exit status:
#   0  protocol satisfied, or the PR does not carry the post-facto label
#   1  the PR is labeled but violates the protocol (landing must be blocked)
#   2  usage / internal error
#
# Those are the only statuses this script emits deliberately.  In particular,
# a command used by a match predicate may return 2 for its own error, 126 when
# it cannot be executed, 127 when it cannot be found, or another nonzero status.
# None of those means "no match": every predicate accepts only 0 (match) and 1
# (no match), and converts every other status to this script's internal-error 2.

set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
readonly REVIEW_CONTRACT_ADAPTER="$SCRIPT_DIR/review_contract_adapter.py"

if ! contract_output=$(python3 "$REVIEW_CONTRACT_ADAPTER" --format lint-records); then
    echo "::error::PR #${PR_NUMBER:-unknown}: cannot load the accepted review labels; refusing to guess." >&2
    exit 2
fi

post_facto_label=
declare -a review_families=()
declare -A approval_labels=()
declare -A round_labels_csv=()
while IFS=$'\t' read -r first second third extra; do
    if [ "$first" = post-facto ]; then
        if [ -n "$post_facto_label" ] || [ -z "$second" ] || [ -n "$third" ] || [ -n "$extra" ]; then
            echo "::error::PR #${PR_NUMBER:-unknown}: malformed post-facto review-label contract record." >&2
            exit 2
        fi
        post_facto_label=$second
    else
        if [ -z "$first" ] || [ -z "$second" ] || [ -z "$third" ] || [ -n "$extra" ] \
            || [ -n "${approval_labels[$first]+set}" ]; then
            echo "::error::PR #${PR_NUMBER:-unknown}: malformed review-family contract record." >&2
            exit 2
        fi
        review_families+=("$first")
        approval_labels[$first]=$second
        round_labels_csv[$first]=$third
    fi
done <<<"$contract_output"

if [ -z "$post_facto_label" ] || [ "${#review_families[@]}" -eq 0 ]; then
    echo "::error::PR #${PR_NUMBER:-unknown}: review-label contract is incomplete." >&2
    exit 2
fi

pr="${PR_NUMBER:-unknown}"
is_kvm="${PR_IS_KVM:-false}"
head_sha="${PR_HEAD_SHA-}"
comments_file="${PR_COMMENTS_FILE-}"
comments_json="${PR_COMMENTS_JSON-}"
comments_source_error=""
if [ -n "$comments_file" ]; then
    if [ -r "$comments_file" ]; then
        comments_json=$(cat -- "$comments_file")
    else
        # Recorded rather than ignored: an unreadable path must not silently
        # fall through to PR_COMMENTS_JSON (likely unset), which would report
        # "no approval" for what is really a plumbing fault.
        comments_json=""
        comments_source_error="PR_COMMENTS_FILE '${comments_file}' is not readable."
    fi
fi

# A grep predicate has three possible outcomes even though a shell condition is
# only true or false: 0 is a match, 1 is a clean no-match, and every other status
# means the comparison did not complete.  Keep that third outcome distinct;
# otherwise a missing or unexecutable grep (127/126) is read as "label absent",
# and the first label check below reports a not-applicable pass.
match_status_or_refuse() {
    local operation=$1 status=$2
    case "$status" in
        0 | 1)
            return "$status"
            ;;
        *)
            echo "::error::PR #${pr}: ${operation} could not decide (exit ${status}); refusing rather than treating it as no match." >&2
            exit 2
            ;;
    esac
}

# UNSET IS NOT THE SAME STATE AS EMPTY, AND ONLY ONE OF THEM IS AN ANSWER.
#
# This previously read `labels="${PR_LABELS-}"`, which collapses "the caller
# never supplied the labels" into "the PR has no labels". The second is a fact
# about the PR; the first is a fact about the invocation. Collapsed together,
# a hand spot-check that forgot the variable took the not-applicable fast path
# and printed a PASS having checked NOTHING -- the gate answered a question it
# had not been given the inputs to answer.
#
# CI is unaffected either way: .github/workflows/merge-gate.yml sets PR_LABELS
# and PR_BODY explicitly. The silent pass only ever reached a human running
# this by hand, which is exactly the reader least able to notice.
if [ -z "${PR_LABELS+set}" ]; then
    echo "::error::PR #${pr}: PR_LABELS is not set. This gate cannot decide anything" >&2
    echo "  without the PR's labels, and it will not report a pass it did not establish." >&2
    echo "  Pass PR_LABELS as newline-separated label names; pass an EMPTY string to" >&2
    echo "  mean the PR genuinely has no labels. Those are different states." >&2
    exit 2
fi
labels="$PR_LABELS"

# WHICH COMMENTS MAY CARRY AN APPROVAL. Until 2026-08-23 nothing here read
# anything but `.body`: the
# scan consumed `jq -r '.[]? | .body'` and matched the text, while the workflow
# handed it the complete, unfiltered `issues/<pr>/comments` array. So ANY account
# that can comment could mint `APPROVED-AT: claude <head>`. The label cache was
# already correctly treated as "not authority"; the comment body was quietly
# trusted as if it were.
#
# AN APPROVAL NEEDS BOTH a role-tagged review comment and a trusted GitHub
# posting association (OWNER, MEMBER, or COLLABORATOR). The association is not
# proof that a review happened, so it cannot replace the role tag; the role tag
# is self-asserted text, so it cannot replace the posting-account boundary.
# Requiring both prevents an arbitrary public commenter from minting authority.
#
# The posting account is deliberately allowed to equal the pull-request author.
# This repository uses owner-posted relays when the independent reviewer cannot
# reach api.github.com. Rejecting every author-posted comment would reject those
# real attestations. The lane still comes only from the explicit
# `APPROVED-AT: <lane> <40-hex>` line, not from the tag interior or association.
#
# The role tag's interior is deliberately not parsed: its shape varies across
# the fleet and does not reliably encode the lane. Presence of a bracketed tag
# marks a review/relay comment; OWNER/MEMBER/COLLABORATOR constrains who may post
# it; the exact binding line names the lane and head.
readonly ROLE_TAG_RE='^\[[^][]+\]$'
#
# WHAT THIS DOES AND DOES NOT ESTABLISH, stated because overclaiming here is
# itself a defect. It closes both the bare-line hole and the arbitrary-public-
# commenter hole. It still does NOT cryptographically authenticate the human or
# agent named inside a role tag: a repository collaborator, including the PR
# author, can write one. Cryptographic reviewer identity must come from the
# registered boundary rather than from mutable comment text; this lint can only
# require the strongest provenance the GitHub comment payload itself exposes.
#
# ONLY APPROVALS ARE CHECKED, NEVER REJECTIONS. Requiring a role tag to REJECT
# would let an untagged "this is broken" be discarded, so a real defect report
# could be silenced by the gate itself. This may only ever remove a positive,
# never a negative; that keeps every divergence from the reference in the
# refusing direction, the same property the SUSPECT_RE divergence preserves.

# True when an exact label name is present (full-line match).
has_label() {
    local status=0
    grep -Fxq -- "$1" <<<"$labels" || status=$?
    match_status_or_refuse "label lookup" "$status"
}

# True when one of the contract's exact numbered labels is present.
has_round_label() {
    local family=$1 round_label
    local -a accepted=()
    IFS=, read -r -a accepted <<<"${round_labels_csv[$family]}"
    for round_label in "${accepted[@]}"; do
        has_label "$round_label" && return 0
    done
    return 1
}

# Spell the exact accepted alternatives from the shared contract.
round_label_alternatives() {
    local family=$1
    printf '%s' "${round_labels_csv[$family]//,/, }"
}

# True when the body contains SECTION as a heading. Accepts a Markdown ATX
# heading (`## Section`), a bold label (`**Section**`), or a bare heading line
# ending in a colon (`Section:`), matched case-insensitively at line start. The
# leading marker requirement keeps prose mentions ("in summary, ...") from
# counting as the section.
has_section() {
    local section=$1
    local status=0
    grep -Eiq \
        "^[[:space:]]*(#{1,6}[[:space:]]*${section}|\*\*[[:space:]]*${section}|${section}[[:space:]]*:)" \
        <<<"$body" \
        || status=$?
    match_status_or_refuse "PR-body section lookup" "$status"
}

# Remove only markdown that WRAPS a complete line, repeatedly, then trim.
# `**APPROVED-AT: claude <sha>**` is a binding; a code span embedded in prose is
# not, so a lone leading backtick must not be stripped. Faithful port of
# `undecorate` in the reference verifier.
undecorate() {
    local line=$1 wrapper n changed=1
    line="${line#"${line%%[![:space:]]*}"}"
    line="${line%"${line##*[![:space:]]}"}"
    while [ "$changed" -eq 1 ]; do
        changed=0
        for wrapper in '`' '**' '__' '*' '_'; do
            n=${#wrapper}
            if [ "${#line}" -gt $((2 * n)) ] \
               && [ "${line:0:n}" = "$wrapper" ] \
               && [ "${line: -n}" = "$wrapper" ]; then
                line="${line:n:${#line} - 2 * n}"
                line="${line#"${line%%[![:space:]]*}"}"
                line="${line%"${line##*[![:space:]]}"}"
                changed=1
                break
            fi
        done
    done
    printf '%s' "$line"
}

readonly SHA40_RE='[0-9a-fA-F]{40}'
readonly AGENT_ID_RE='[a-z0-9][a-z0-9-]*'
readonly MARKER_ISSUER_SUFFIX_RE="([[:space:]]+BY[[:space:]]+(${AGENT_ID_RE}))?"
# Structural Markdown is accepted for rejections, never approvals. Normalise
# headings, block quotes, unordered/ordered lists, and task-list markers one
# layer at a time so nested forms cannot hide a negative verdict. Applying the
# same normalisation to suspect detection makes malformed or truncated verdicts
# visible too. An approval still binds only when APPROVE_RE matches the original
# undecorated line exactly; `## APPROVED-AT: ...` therefore refuses rather than
# authorising.
readonly APPROVE_RE="^APPROVED-AT:[[:space:]]*(claude|codex)[[:space:]]+(${SHA40_RE})${MARKER_ISSUER_SUFFIX_RE}$"
readonly REJECT_RE="^CHANGES-REQUESTED-AT:[[:space:]]*(claude|codex)[[:space:]]+(${SHA40_RE})${MARKER_ISSUER_SUFFIX_RE}$"
readonly WITHDRAW_RE="^CHANGES-REQUESTED-WITHDRAWN-AT:[[:space:]]*(claude|codex)[[:space:]]+(${SHA40_RE})${MARKER_ISSUER_SUFFIX_RE}$"
readonly REJECT_LEGACY_RE="^REQUEST[[:space:]]+CHANGES[[:space:]]+AT[[:space:]]+(${SHA40_RE})$"
readonly EXPLICIT_VERDICT_RE='^(APPROVED-AT:|CHANGES-REQUESTED-AT:|CHANGES-REQUESTED-WITHDRAWN-AT:|REQUEST[[:space:]]+CHANGES[[:space:]]+AT)'
# The same keywords ANYWHERE on the line, not only at column 0.
#
# ⚠️ A LEADING WORD IS NOT STRUCTURE, AND THAT IS HOW A REJECTION WENT MISSING.
# `strip_structural_prefix` removes headings, quotes and list markers, so
# `## CHANGES-REQUESTED-AT: ...` is caught. It does not remove an arbitrary word,
# and every detector above is `^`-anchored, so a line beginning with one matched
# NOTHING -- not a binding, and not the malformed check either. It was silently
# ignored, which for a rejection means ADMITTED. Measured at f57f6549904b with a
# valid admit-control:
#
#     CHANGES-REQUESTED-AT: claude <head>                    BLOCKS
#     ## CHANGES-REQUESTED-AT: claude <head>                 BLOCKS
#     ## CODEX-LANE CHANGES-REQUESTED-AT: claude <head>      ADMITTED
#     CODEX-LANE CHANGES-REQUESTED-AT: claude <head>         ADMITTED
#     Note: CHANGES-REQUESTED-AT: claude <head>              ADMITTED
#     **Codex lane** CHANGES-REQUESTED-AT: claude <head>     ADMITTED
#
# ⚠️ AND THE ADMITTED FORM IS THE ONE FROM THE INCIDENT THIS GATE CITES.
# `## CODEX-LANE CHANGES-REQUESTED-AT: codex <sha>` is what was live on
# https://github.com/rrnewton/hermit/pull/2176, named in this file's own header as
# the motivating case. The gate closed the heading variant and left the variant
# that actually happened.
#
# THIS IS USED ONLY TO DECIDE "SUSPECT", NEVER TO BIND. Binding stays anchored, so
# a prefixed line still binds nothing -- it now REFUSES as unparseable instead of
# being dropped. That direction is deliberate: the cost is that an unindented
# prose quotation of a marker also refuses, and a refusal that says "this line
# looks like a verdict and binds nothing" is recoverable, while a silently
# swallowed rejection is not.
readonly EXPLICIT_VERDICT_ANYWHERE_RE='(APPROVED-AT:|CHANGES-REQUESTED-AT:|CHANGES-REQUESTED-WITHDRAWN-AT:|REQUEST[[:space:]]+CHANGES[[:space:]]+AT)'
readonly STRUCTURAL_PREFIX_RE='^(#{1,6}[[:space:]]*|>[[:space:]]*|[-+*][[:space:]]+(\[[[:space:]xX]\][[:space:]]+)?|[0-9]+[.)][[:space:]]+(\[[[:space:]xX]\][[:space:]]+)?)(.*)$'
readonly AUTHORITY_PREFIX_RE='^(#{1,6}[[:space:]]*|[-+*][[:space:]]+(\[[[:space:]xX]\][[:space:]]+)?|[0-9]+[.)][[:space:]]+(\[[[:space:]xX]\][[:space:]]+)?)(.*)$'
readonly FENCE_RE='^ {0,3}(`{3,}|~{3,})[[:space:]]*(.*)$'

# Emit only prose lines. Review comments routinely quote this protocol inside
# fenced examples and CommonMark indented code; those examples must not grant an
# approval, create or withdraw a refusal, name an issuer, or authenticate a
# comment. An indented block starts only after a blank line, matching CommonMark
# closely enough not to discard a deliberately indented marker under a list.
marker_lines() {
    local body=$1 raw candidate info first
    local fence='' indented=0 prev_blank=1 blank=0
    while IFS= read -r raw; do
        blank=0
        [ -z "${raw//[[:space:]]/}" ] && blank=1
        if [ -n "$fence" ]; then
            if [[ $raw =~ $FENCE_RE ]]; then
                candidate=${BASH_REMATCH[1]}
                info=${BASH_REMATCH[2]}
                info="${info#"${info%%[![:space:]]*}"}"
                info="${info%"${info##*[![:space:]]}"}"
                first=${candidate:0:1}
                if [ "$first" = "${fence:0:1}" ] \
                   && [ "${#candidate}" -ge "${#fence}" ] \
                   && [ -z "$info" ]; then
                    fence=''
                fi
            fi
            prev_blank=$blank
            continue
        fi
        if [[ $raw =~ $FENCE_RE ]]; then
            fence=${BASH_REMATCH[1]}
            indented=0
            prev_blank=0
            continue
        fi
        if [ "$indented" -eq 1 ]; then
            if [ "$blank" -eq 1 ]; then
                prev_blank=1
                continue
            fi
            if [[ $raw == '    '* ]] || [[ $raw == $'\t'* ]]; then
                continue
            fi
            indented=0
        elif [ "$prev_blank" -eq 1 ] && [ "$blank" -eq 0 ] \
             && { [[ $raw == '    '* ]] || [[ $raw == $'\t'* ]]; }; then
            indented=1
            prev_blank=0
            continue
        fi
        printf '%s\n' "$raw"
        prev_blank=$blank
    done <<< "$body"
}

strip_structural_prefix() {
    local line=$1
    while [[ $line =~ $STRUCTURAL_PREFIX_RE ]]; do
        line=${BASH_REMATCH[4]}
        line="${line#"${line%%[![:space:]]*}"}"
        line="${line%"${line##*[![:space:]]}"}"
    done
    printf '%s' "$line"
}

# The shared authority accepts headings and list markers but not blockquotes:
# quoted text may describe a verdict, but cannot issue or withdraw one.
strip_authority_prefix() {
    local line=$1
    while [[ $line =~ $AUTHORITY_PREFIX_RE ]]; do
        line=${BASH_REMATCH[4]}
        line="${line#"${line%%[![:space:]]*}"}"
        line="${line%"${line##*[![:space:]]}"}"
    done
    printf '%s' "$line"
}

# A line plainly trying to be a verdict binding but matching no known shape.
#
# DELIBERATELY STRICTER THAN THE REFERENCE, IN THE REFUSING DIRECTION ONLY.
# Explicit APPROVED-AT / CHANGES-REQUESTED-AT / REQUEST CHANGES AT prefixes are
# suspect even without a full SHA. Otherwise a truncated rejection such as
# `CHANGES-REQUESTED-AT: claude 0123456789ab` silently disappears. The broader
# pattern retains the historical verdict-ish spellings when they do carry a
# full SHA, without treating ordinary prose beginning with "approval" as a
# verdict.
#
# This gate therefore refuses such a line instead of ignoring it. The divergence
# can only ever turn a reference PASS into a refusal here, never the reverse —
# a property the differential in the PR description measures rather than
# asserts. The correct end state is for the reference to adopt the same class so
# both consumers agree again; that belongs in a dev-hermit change, not here.
readonly SUSPECT_RE="^(APPROV|CHANGES-REQUESTED|REQUEST[[:space:]]+CHANGES|REJECT|LGTM|SIGN(ED)?[-[:space:]]?OFF|ACK).*${SHA40_RE}"
readonly RETIRES_RE='(^|[^[:alnum:]_])RETIRES[[:space:]]+#?([0-9]{6,})([^[:alnum:]_]|$)'
readonly CITATION_MARKER_RE="^(APPROVED-AT|CHANGES-REQUESTED-AT|CHANGES-REQUESTED-WITHDRAWN-AT):[[:space:]]+(claude|codex)[[:space:]]+([0-9a-fA-F]{7,40})${MARKER_ISSUER_SUFFIX_RE}$"

# The GitHub account is shared for relayed review comments, so `.user.login`
# cannot identify the reviewer. Agent comments carry their writer in the
# existing `[team, agent, ...]` disclosure tag. Read the first such tag from an
# unquoted, unindented line; bracketed role tags may precede it on the same line.
comment_author() {
    local line rest tag
    for line in "$@"; do
        rest=$line
        while [[ $rest =~ ^\[([^][]*)\][[:space:]]*(.*)$ ]]; do
            tag=${BASH_REMATCH[1]}
            rest=${BASH_REMATCH[2]}
            if [[ $tag =~ ^[[:space:]]*[a-z0-9]+[[:space:]]*,[[:space:]]*(${AGENT_ID_RE})[[:space:]]*, ]]; then
                printf '%s' "${BASH_REMATCH[1],,}"
                return
            fi
        done
    done
}

# The refusal gate attributes withdrawal and RETIRES authority to the comment:
# the first marker-level BY in the body wins, then the disclosure author.
comment_issuer() {
    local author=$1 line undecorated verdict_line named
    shift
    for line in "$@"; do
        undecorated=$(undecorate "$line")
        verdict_line=$(strip_authority_prefix "$undecorated")
        if [[ $verdict_line =~ $APPROVE_RE ]] \
           || [[ $verdict_line =~ $REJECT_RE ]] \
           || [[ $verdict_line =~ $WITHDRAW_RE ]]; then
            named=${BASH_REMATCH[4]-}
            if [ -n "$named" ]; then
                printf '%s' "${named,,}"
                return
            fi
        fi
    done
    printf '%s' "$author"
}

# Two readable issuers are the same reviewer. Unattributed markers match nobody.
# Keep this as the single ownership comparison so the mutation check can prove
# every issuer-scoped discharge depends on it.
same_issuer() {
    local left=$1 right=$2
    [ -n "$left" ] && [ "$left" != '<unattributed>' ] \
        && [ "$left" = "$right" ]
}

# Remove all refusals owned by ISSUER after that issuer posts a fresh approval.
# Rows are `<comment-id> <comment-index> <sha> <issuer>`.
discharge_refusals() {
    local issuer=$1 held held_issuer
    shift
    for held in "$@"; do
        held_issuer=${held##* }
        if same_issuer "$held_issuer" "$issuer"; then
            continue
        fi
        printf '%s\n' "$held"
    done
}

# Emit every numeric comment id named by `RETIRES` in the supplied prose lines,
# in text order.
retire_targets() {
    local line rest match
    for line in "$@"; do
        rest=$line
        while [[ $rest =~ $RETIRES_RE ]]; do
            printf '%s\n' "${BASH_REMATCH[2]}"
            match=${BASH_REMATCH[0]}
            rest=${rest#*"$match"}
        done
    done
}

# A citation may abbreviate the original SHA, but it cannot name a different
# head. This mirrors the shared authority's prefix-compatible comparison.
shas_name_same_head() {
    local left=$1 right=$2
    [ -n "$left" ] && [ -n "$right" ] \
        && { [ "${left#"$right"}" != "$left" ] \
             || [ "${right#"$left"}" != "$right" ]; }
}

# A verdict is issued once. A later exact or SHA-abbreviated copy from the same
# comment author is a citation, not a fresh decision after an intervening refusal.
verdict_is_citation() {
    local author=$1 kind=$2 lane=$3 sha=$4 issued
    local seen_author seen_kind seen_lane seen_sha
    shift 4
    [ -n "$author" ] || return 1
    for issued in "$@"; do
        IFS=$'\x1f' read -r seen_author seen_kind seen_lane seen_sha <<< "$issued"
        if [ "$seen_author" = "$author" ] \
           && [ "$seen_kind" = "$kind" ] \
           && { [ "$seen_lane" = "$lane" ] || [ "$seen_lane" = '*' ] || [ "$lane" = '*' ]; } \
           && shas_name_same_head "$seen_sha" "$sha"; then
            return 0
        fi
    done
    return 1
}


# Scan every comment for LANE and emit one tagged row per line of stdout:
#
#   S <index> <sha>  a SHA this lane bound itself to, oldest first
#   R <comment-id> <index> <sha> <issuer>  a refusal not discharged
#   M <line>   a verdict-ish line matching no known shape
#
# Everything travels out through stdout ON PURPOSE. The caller reads this with
# `mapfile < <(...)`, which runs the function in a subshell, so a global
# assigned in here would be silently discarded in the parent — an earlier draft
# collected malformed lines that way and the malformed check was inert while its
# tests still passed, because those cases also failed the binding check for an
# unrelated reason. Tagged stdout is what makes both results actually observable.
scan_lane() {
    local lane=$1 cid encoded body line undecorated verdict_line authority_line login assoc created updated author comment_issuer_value withdrawer
    local -a comment_lines=()
    local -a found=()
    local -a outstanding=()
    local -a withdrawals=()
    local -a issued_verdicts=()
    local idx=-1
    # The reference grammar is case-insensitive throughout, so `APPROVED-AT:
    # CODEX <SHA>` binds exactly as `approved-at: codex <sha>` does. Scoped to
    # this function and restored on return.
    local had_nocasematch=0
    shopt -q nocasematch && had_nocasematch=1
    shopt -s nocasematch
    while IFS=$'\x1f' read -r cid login assoc created updated encoded; do
        [ -n "$encoded" ] || continue
        idx=$((idx + 1))
        cid=${cid:-index-$idx}
        body=$(printf '%s' "$encoded" | base64 -d)
        mapfile -t comment_lines < <(marker_lines "$body")
        author=$(comment_author "${comment_lines[@]}")
        comment_issuer_value=$(comment_issuer "$author" "${comment_lines[@]}")
        # GitHub returns issue comments in creation order, but comments are
        # mutable. If a verdict-bearing comment was edited, the current payload
        # cannot prove where the edited verdict belongs in the chronology or
        # what verdict it replaced. Refuse it instead of treating creation order
        # as an answer about edit order.
        if [ -n "$created" ] && [ -n "$updated" ] && [ "$created" != "$updated" ]; then
            local edited_verdict=0
            while IFS= read -r line; do
                undecorated=$(undecorate "$line")
                verdict_line=$(strip_structural_prefix "$undecorated")
                authority_line=$(strip_authority_prefix "$undecorated")
                if [[ $undecorated =~ $APPROVE_RE ]] \
                   || [[ $verdict_line =~ $REJECT_RE ]] \
                   || [[ $authority_line =~ $WITHDRAW_RE ]] \
                   || [[ $verdict_line =~ $REJECT_LEGACY_RE ]] \
                   || { ! [[ $verdict_line =~ $WITHDRAW_RE ]] \
                        && { [[ $verdict_line =~ $EXPLICIT_VERDICT_RE ]] \
                             || [[ $verdict_line =~ $SUSPECT_RE ]]; }; }; then
                    printf 'E %s edited verdict comment cannot establish chronology: %s\n' \
                        "$idx" "${verdict_line:0:80}"
                    edited_verdict=1
                    break
                fi
            done < <(printf '%s\n' "${comment_lines[@]}")
            [ "$edited_verdict" -eq 0 ] || continue
        fi
        # Decided per comment, before any line of it is read, so the same
        # verdict applies to every APPROVED-AT line the comment carries.
        # Scanned over every prose line, not just the first: the relayed
        # attestations on this pull request put the tag first, but a review that
        # opens with a heading and tags itself lower down is still a review.
        local approver_refusal="carries no role tag, so it is not a review comment"
        local tag_line
        while IFS= read -r tag_line; do
            # Trailing \r survives from web-posted comments and would defeat the
            # anchored match.
            tag_line=${tag_line%$'\r'}
            tag_line="${tag_line#"${tag_line%%[![:space:]]*}"}"
            tag_line="${tag_line%"${tag_line##*[![:space:]]}"}"
            if [[ $tag_line =~ $ROLE_TAG_RE ]]; then
                approver_refusal=""
                break
            fi
        done < <(printf '%s\n' "${comment_lines[@]}")
        if [ -z "$approver_refusal" ]; then
            case ${assoc^^} in
                OWNER|MEMBER|COLLABORATOR) ;;
                *) approver_refusal="author_association '${assoc:-<missing>}' is not OWNER, MEMBER, or COLLABORATOR" ;;
            esac
        fi
        # Record canonical withdrawals for application after the whole history
        # is known. A RETIRES claim is precise to its named comment; an unquoted
        # withdrawal remains an independent issuer-scoped operation.
        while IFS= read -r line; do
            undecorated=$(undecorate "$line")
            verdict_line=$(strip_authority_prefix "$undecorated")
            if [[ $verdict_line =~ $WITHDRAW_RE ]]; then
                local withdrawn_lane=${BASH_REMATCH[1]}
                local withdrawn_sha=${BASH_REMATCH[2]}
                local withdrawal_marker_issuer=${BASH_REMATCH[4]-}
                local withdrawal_refusal=$approver_refusal
                withdrawn_lane=${withdrawn_lane,,}
                if [ -n "$withdrawal_refusal" ]; then
                    printf 'W %s %s association=%s (%s)\n' \
                        "$idx" "${login:-<unidentified>}" \
                        "${assoc:-<unknown>}" "$withdrawal_refusal"
                elif ! verdict_is_citation "$author" withdrawn "$withdrawn_lane" \
                    "$withdrawn_sha" "${issued_verdicts[@]}"; then
                    issued_verdicts+=("$author"$'\x1f'"withdrawn"$'\x1f'"$withdrawn_lane"$'\x1f'"$withdrawn_sha")
                    local -a targets=()
                    local targets_csv='-'
                    mapfile -t targets < <(retire_targets "${comment_lines[@]}")
                    if [ "${#targets[@]}" -gt 0 ]; then
                        local old_ifs=$IFS
                        IFS=,
                        targets_csv=${targets[*]}
                        IFS=$old_ifs
                        withdrawer=$comment_issuer_value
                    else
                        withdrawer=${withdrawal_marker_issuer:-$author}
                    fi
                    withdrawer=${withdrawer,,}
                    withdrawals+=("$idx $cid $withdrawn_lane $withdrawn_sha ${withdrawer:-<unattributed>} $targets_csv")
                fi
            fi
        done < <(printf '%s\n' "${comment_lines[@]}")

        # A rejection contributes NOTHING from this comment, even if the same
        # comment also carries an APPROVED-AT-shaped line: a comment quoting the
        # approval it supersedes must not bind as a positive. Clearing rather
        # than skipping is load-bearing, or APPROVED-then-CHANGES-REQUESTED
        # would read as approved forever.
        local rejected=0 rejected_sha refusing_issuer
        while IFS= read -r line; do
            undecorated=$(undecorate "$line")
            verdict_line=$(strip_structural_prefix "$undecorated")
            if [[ $verdict_line =~ $REJECT_LEGACY_RE ]]; then
                rejected_sha=${BASH_REMATCH[1]}
                if verdict_is_citation "$author" refused '*' "$rejected_sha" \
                    "${issued_verdicts[@]}"; then
                    continue
                fi
                issued_verdicts+=("$author"$'\x1f'"refused"$'\x1f'"*"$'\x1f'"$rejected_sha")
                rejected=1
                refusing_issuer=$author
                outstanding+=("$cid $idx $rejected_sha ${refusing_issuer:-<unattributed>}")
                continue
            fi
            if [[ $verdict_line =~ $REJECT_RE ]]; then
                local rejected_lane=${BASH_REMATCH[1]}
                if [ "${rejected_lane,,}" = "$lane" ]; then
                    rejected_sha=${BASH_REMATCH[2]}
                    local refusal_marker_issuer=${BASH_REMATCH[4]-}
                    if verdict_is_citation "$author" refused "$lane" "$rejected_sha" \
                        "${issued_verdicts[@]}"; then
                        continue
                    fi
                    issued_verdicts+=("$author"$'\x1f'"refused"$'\x1f'"$lane"$'\x1f'"$rejected_sha")
                    rejected=1
                    refusing_issuer=${refusal_marker_issuer:-$author}
                    refusing_issuer=${refusing_issuer,,}
                    outstanding+=("$cid $idx $rejected_sha ${refusing_issuer:-<unattributed>}")
                fi
            fi
        done < <(printf '%s\n' "${comment_lines[@]}")
        if [ "$rejected" -eq 1 ]; then
            found=()
            continue
        fi
        while IFS= read -r line; do
            undecorated=$(undecorate "$line")
            verdict_line=$(strip_structural_prefix "$undecorated")
            if [[ $undecorated =~ $APPROVE_RE ]]; then
                local matched_lane=${BASH_REMATCH[1]} matched_sha=${BASH_REMATCH[2]}
                local approver=${BASH_REMATCH[4]-}
                matched_lane=${matched_lane,,}
                if [ -n "$approver_refusal" ]; then
                    if [ "$matched_lane" = "$lane" ]; then
                        # Reported, not silently dropped. A refused approval that
                        # vanished would surface only as the generic "no approval
                        # from <lane>", which reads as "the reviewer never got to
                        # it" rather than "someone tried to mint this".
                        printf 'U %s %s association=%s (%s)\n' \
                            "$idx" "${login:-<unidentified>}" \
                            "${assoc:-<unknown>}" "$approver_refusal"
                    fi
                    # An untrusted copy cannot consume a later trusted issuance
                    # by making it look like a citation.
                    continue
                fi
                # Citation history spans both lanes. Otherwise a shortened
                # claude copy is suppressed while scanning claude but is still
                # reported malformed while scanning codex.
                if verdict_is_citation "$author" approved "$matched_lane" "$matched_sha" \
                    "${issued_verdicts[@]}"; then
                    continue
                fi
                issued_verdicts+=("$author"$'\x1f'"approved"$'\x1f'"$matched_lane"$'\x1f'"$matched_sha")
                if [ "$matched_lane" = "$lane" ]; then
                    # Recorded EXACTLY as written, deliberately not lowercased.
                    # The reference verifier compares the captured text against
                    # the API's lowercase head, so an upper-case SHA does not
                    # bind there. Normalising here would make this gate accept
                    # an approval the reference rejects, and two consumers
                    # disagreeing about what approval means is the whole defect.
                    # The same author repeating the same lane/head approval is a
                    # citation of its earlier verdict, not a fresh verdict after
                    # an intervening refusal. This uses the disclosure author,
                    # matching the parent authority's citation rule; marker BY
                    # remains the more specific issuer for refusal ownership.
                    approver=${approver:-$author}
                    approver=${approver,,}
                    mapfile -t outstanding < <(discharge_refusals "$approver" \
                        "${outstanding[@]}")
                    found+=("$idx $matched_sha")
                fi
            elif { [[ $verdict_line =~ $EXPLICIT_VERDICT_RE ]] \
                   || [[ $verdict_line =~ $EXPLICIT_VERDICT_ANYWHERE_RE ]] \
                   || [[ $verdict_line =~ $SUSPECT_RE ]]; } \
                 && ! [[ $verdict_line =~ $REJECT_RE ]] \
                 && ! [[ $verdict_line =~ $WITHDRAW_RE ]] \
                 && ! [[ $verdict_line =~ $REJECT_LEGACY_RE ]]; then
                if [[ $undecorated =~ $CITATION_MARKER_RE ]]; then
                    local citation_marker=${BASH_REMATCH[1]^^}
                    local citation_lane=${BASH_REMATCH[2],,}
                    local citation_sha=${BASH_REMATCH[3]}
                    local citation_kind=approved
                    case $citation_marker in
                        CHANGES-REQUESTED-AT) citation_kind=refused ;;
                        CHANGES-REQUESTED-WITHDRAWN-AT) citation_kind=withdrawn ;;
                    esac
                    if verdict_is_citation "$author" "$citation_kind" "$citation_lane" \
                        "$citation_sha" "${issued_verdicts[@]}"; then
                        continue
                    fi
                fi
                # A well-formed rejection for the OTHER lane reaches here (it is
                # not this lane's rejection, and it is not an approval) and it
                # opens with a verdict keyword, so it matches SUSPECT_RE. It is
                # perfectly parseable and must not be reported as malformed.
                printf 'M %s %s\n' "$idx" "${verdict_line:0:120}"
            fi
        done < <(printf '%s\n' "${comment_lines[@]}")
    # Carries comment id, commenter identity, and creation/edit timestamps
    # alongside the text. A fixed 6-field row keeps the split unambiguous, and
    # the body stays base64 so an embedded tab or newline cannot shift columns.
    #
    # TOTAL BY CONSTRUCTION. Every accessor is guarded, so no element can make
    # jq raise and truncate the stream mid-way. The input validation above
    # already refuses a non-object element; this is the second line of defence,
    # because the failure it prevents is silent and reads as "fewer comments"
    # rather than as an error.
    done < <(printf '%s' "$comments_json" \
        | jq -r '.[]? | if type == "object" then
                            [(.id // "" | tostring),
                             ((.user // {}) | if type == "object" then (.login // "") else "" end),
                             (.author_association // ""),
                             (.created_at // ""),
                             (.updated_at // ""),
                             (.body // "" | tostring | @base64)]
                        else ["", "", "", "", "", ""] end | join("\u001f")')
    # Truncation is still checked rather than assumed: if the loop saw fewer
    # comments than the payload holds, something dropped rows and the lane's
    # verdict was computed from a partial history.
    local seen=$((idx + 1)) declared
    declared=$(printf '%s' "$comments_json" | jq -r 'length' 2>/dev/null || echo -1)
    if [ "$declared" -ge 0 ] && [ "$seen" -ne "$declared" ]; then
        printf 'X read %s of %s comments; the stream was truncated\n' "$seen" "$declared"
    fi
    # Apply withdrawals after the complete lane history is known. RETIRES is
    # order-independent because the comment id identifies its target exactly.
    # An unquoted withdrawal is chronological and removes all earlier refusals
    # from its own readable issuer; it does not become inert merely because a
    # different withdrawal comment uses RETIRES.
    local withdrawal w_idx w_cid w_lane w_sha w_issuer w_targets
    for withdrawal in ${withdrawals[@]+"${withdrawals[@]}"}; do
        read -r w_idx w_cid w_lane w_sha w_issuer w_targets <<< "$withdrawal"
        : "$w_cid" "$w_sha"
        if [ "$w_targets" != '-' ]; then
            local -a target_ids=() retained=()
            local target held r_cid r_idx r_sha r_issuer
            IFS=, read -r -a target_ids <<< "$w_targets"
            for held in ${outstanding[@]+"${outstanding[@]}"}; do
                read -r r_cid r_idx r_sha r_issuer <<< "$held"
                : "$r_idx" "$r_sha"
                local retire=0
                for target in "${target_ids[@]}"; do
                    if [ "$r_cid" = "$target" ]; then
                        if [ "$r_issuer" = '<unattributed>' ] \
                           || same_issuer "$r_issuer" "$w_issuer"; then
                            retire=1
                        fi
                    fi
                done
                [ "$retire" -eq 1 ] || retained+=("$held")
            done
            outstanding=("${retained[@]}")
        elif [ "$w_lane" = "$lane" ]; then
            local -a retained=()
            local held r_cid r_idx r_sha r_issuer
            for held in ${outstanding[@]+"${outstanding[@]}"}; do
                read -r r_cid r_idx r_sha r_issuer <<< "$held"
                : "$r_cid" "$r_sha"
                if [ "$r_idx" -lt "$w_idx" ] \
                   && same_issuer "$r_issuer" "$w_issuer"; then
                    continue
                fi
                retained+=("$held")
            done
            outstanding=("${retained[@]}")
        fi
    done

    [ "$had_nocasematch" -eq 1 ] || shopt -u nocasematch
    local row
    for row in ${found[@]+"${found[@]}"}; do
        printf 'S %s\n' "$row"
    done
    for row in ${outstanding[@]+"${outstanding[@]}"}; do
        printf 'R %s\n' "$row"
    done
}

# AN INERT TRIGGER LABEL IS NOT A PASS.
#
# This gate keys on exactly one label. A PR carrying a DIFFERENT
# `*-human-review` label gets the not-applicable fast path and exits 0 -- it
# reads as "human review required" to anyone skimming, and is evaluated by
# nothing. Measured on this repository: `pre-land-human-review` has ZERO
# references across `.github/` and `scripts/`, so it triggers no gate anywhere,
# while a `passed-review-claude` label sitting beside it reads as approved.
#
# Matched by SHAPE rather than by a list of known names, so a future variant is
# caught the day it is invented instead of the day someone remembers to add it
# here. That is deliberate: an enumerated set of bad labels would narrow
# silently exactly as an enumerated rejection set does.
#
# Exit 2 (a plumbing refusal), not 1: the protocol has not failed, the caller
# has asked an unanswerable question by labelling the PR with a trigger nothing
# reads. Removing the label or replacing it with the real one both resolve it.
if ! has_label "$post_facto_label"; then
    inert_triggers=$(printf '%s\n' "$labels" \
        | grep -E -- '-human-review$' \
        | grep -Fxv -- "$post_facto_label" || true)
    if [ -n "$inert_triggers" ]; then
        echo "::error::PR #${pr}: carries a review-trigger label that NO gate reads:" >&2
        printf '  %s\n' $inert_triggers >&2
        echo "  This gate keys only on '${post_facto_label}'. As labelled, the PR takes" >&2
        echo "  the not-applicable fast path and this lint checks NOTHING, while the label" >&2
        echo "  reads as though human review were required. Either apply" >&2
        echo "  '${post_facto_label}' so the protocol is actually evaluated, or remove" >&2
        echo "  the inert label so the PR does not claim a review that no gate enforces." >&2
        exit 2
    fi

    # Report the genuinely-empty case distinctly from "has labels, but not this
    # one". Both are correct passes, and a reader who cannot tell them apart
    # cannot tell a real not-applicable from a lost label set.
    if [ -z "$labels" ]; then
        echo "PR #${pr}: the PR has NO labels at all (empty set, supplied); \
core-review protocol not applicable."
    else
        echo "PR #${pr}: no ${post_facto_label} label; core-review protocol not applicable."
    fi
    exit 0
fi

# Only now is the body load-bearing, so only now is its absence an error --
# validating it earlier would refuse callers on the not-applicable fast path
# that never read it. Same distinction as above: unset is a broken invocation,
# empty is a PR with no description (which then legitimately fails (c) below).
if [ -z "${PR_BODY+set}" ]; then
    echo "::error::PR #${pr}: PR_BODY is not set, and this PR is labeled" >&2
    echo "  ${post_facto_label}, so the required-section checks below cannot run." >&2
    echo "  Refusing rather than reporting five phantom 'missing section' errors for a" >&2
    echo "  body that was never supplied. Pass an EMPTY string for a PR with no body." >&2
    exit 2
fi
body="$PR_BODY"

echo "PR #${pr}: ${post_facto_label} present; enforcing the core-change review protocol."

errors=0
fail() {
    echo "::error::PR #${pr}: $*"
    errors=$((errors + 1))
}

# (a) Adversarial review happened for every family (any accepted round).
#
# STATE THE OBSERVATION, NOT A CAUSE THIS GATE CANNOT SEE. A label is a cache,
# not the event: this script reads label names and has no view of reviews,
# commits, or which revision anything was approved against. "codex has not
# approved the current revision" is a DIAGNOSIS; "the label is absent from the
# supplied set" is the OBSERVATION, and only the second is established here.
#
# The distinction is not pedantry. An absent approval label has more than one
# cause, and the routine one is not misconduct: the invalidator strips approval
# labels when a new commit lands, so "approved, then the PR moved" and
# "never approved" look identical from here. Naming only the second sends the
# author to argue with a reviewer instead of re-requesting review after a push.
#
# Where the gate CAN narrow it, it does: the round label is evidence it holds.
# This is why the message below is not a flat "could be anything" -- an
# unconditional list of candidates would be the same defect wearing humility.
missing_approval() {
    local reviewer=$1
    local approval_label=${approval_labels[$reviewer]}
    local alternatives
    alternatives=$(round_label_alternatives "$reviewer")
    if has_round_label "$reviewer"; then
        fail "${approval_label} is absent from the supplied labels, but one of \
${alternatives} is present. Review ran; the approval label is not here. \
Either ${reviewer} has not approved, or it approved an earlier revision and a later push \
invalidated the label -- re-request review at the current head."
    else
        fail "${approval_label} is absent from the supplied labels, and none of \
${alternatives} is present: no round label for ${reviewer} is present at all. \
No ${reviewer} review is recorded on this PR."
    fi
}

for reviewer in "${review_families[@]}"; do
    if ! has_round_label "$reviewer"; then
        fail "no accepted ${reviewer} round label is present in the supplied labels \
(need one of $(round_label_alternatives "$reviewer"))."
    fi
done

# (b) The latest reviews approved.
for reviewer in "${review_families[@]}"; do
    has_label "${approval_labels[$reviewer]}" || missing_approval "$reviewer"
done

# (b') Each lane's newest verdict binds the EXACT current head.
#
# Fail closed on the inputs first. A missing or malformed head, or comments that
# are not a JSON array, must BLOCK — never silently skip the binding check and
# fall back to (b), which is the fail-open shape being removed here.
binding_inputs_ok=1
if ! [[ $head_sha =~ ^${SHA40_RE}$ ]]; then
    binding_inputs_ok=0
    fail "PR_HEAD_SHA is missing or is not a 40-hex commit id (got: '${head_sha}'); cannot bind approvals to the current head."
fi
if [ -n "$comments_source_error" ]; then
    binding_inputs_ok=0
    fail "$comments_source_error"
elif ! command -v jq >/dev/null 2>&1; then
    binding_inputs_ok=0
    fail "jq is required to read the PR comments but is not on PATH."
elif [ -z "${comments_json//[[:space:]]/}" ]; then
    # Checked separately because `jq -e` exits 0 on EMPTY input: an unset
    # PR_COMMENTS_JSON would otherwise pass this validation and be misreported
    # downstream as "no approval" rather than "you did not pass the comments".
    # An empty JSON array is a different thing and is legitimate here: it means
    # the PR genuinely has no comments, which the binding check then refuses.
    binding_inputs_ok=0
    fail "PR_COMMENTS_JSON is missing or is not a JSON array; cannot verify exact-head approval."
elif ! printf '%s' "$comments_json" | jq -e 'type == "array"' >/dev/null 2>&1; then
    binding_inputs_ok=0
    fail "PR_COMMENTS_JSON is missing or is not a JSON array; cannot verify exact-head approval."
elif ! printf '%s' "$comments_json" | jq -e 'all(type == "object")' >/dev/null 2>&1; then
    # EVERY element must be an object, checked HERE and not left to the scan.
    # The scan's extraction indexes each element; on a non-object jq raises and
    # ABORTS THE STREAM. Rows already emitted stand and everything after the bad
    # element is silently dropped — so a crafted element placed between an
    # approval and the rejection that supersedes it deletes the rejection and
    # the stale approval stands. That is a fail-OPEN reachable from comment
    # content, which is the shape this gate exists to remove. The abort is also
    # invisible to the scan: it reads through a process substitution, whose exit
    # status bash discards, so the loop cannot tell a truncated stream from a
    # short one.
    binding_inputs_ok=0
    fail "PR_COMMENTS_JSON contains an element that is not a JSON object; refusing rather than reading a stream that may be truncated at that element."
fi

if [ "$binding_inputs_ok" -eq 1 ]; then
    head_lc=${head_sha,,}
    malformed_seen=""
    for lane in codex claude; do
        lane_bound=()
        lane_malformed=()
        lane_unauthorized=()
        lane_edited=()
        lane_truncated=()
        lane_refused=()
        lane_unauthorized_withdrawal=()
        newest_idx=-1
        while IFS= read -r row; do
            case $row in
                'S '*)
                    row=${row#S }
                    newest_idx=${row%% *}
                    lane_bound+=("${row#* }")
                    ;;
                'M '*) lane_malformed+=("${row#M }") ;;
                'U '*) lane_unauthorized+=("${row#U }") ;;
                'E '*) lane_edited+=("${row#E }") ;;
                'X '*) lane_truncated+=("${row#X }") ;;
                'R '*) lane_refused+=("${row#R }") ;;
                'W '*) lane_unauthorized_withdrawal+=("${row#W }") ;;
            esac
        done < <(scan_lane "$lane")

        # Before any verdict: a partial read cannot support one in either
        # direction, because the rows that went missing could be the rejection.
        for entry in ${lane_truncated[@]+"${lane_truncated[@]}"}; do
            fail "cannot verify ${lane}: ${entry}. A verdict computed from part of the comment history is not a verdict."
        done

        for entry in ${lane_edited[@]+"${lane_edited[@]}"}; do
            fail "cannot verify ${lane}: ${entry}. Edited verdict comments have no trustworthy chronology."
        done

        # Said out loud even when the lane also has a genuine approval: an
        # attempt to mint one is worth seeing in the log either way.
        for entry in ${lane_unauthorized[@]+"${lane_unauthorized[@]}"}; do
            echo "PR #${pr}: ignoring an unauthenticated ${lane} approval at comment ${entry% (*}: ${entry#* }"
        done
        for entry in ${lane_unauthorized_withdrawal[@]+"${lane_unauthorized_withdrawal[@]}"}; do
            echo "PR #${pr}: ignoring an unauthenticated withdrawal at comment ${entry% (*}: ${entry#* }"
        done

        # An unparseable verdict line only matters if it could be a verdict this
        # lane has not yet superseded. A malformed line in a comment OLDER than
        # the lane's newest binding cannot express a newer opinion than that
        # binding does, so it is history, not an open question.
        #
        # Without this the gate is UNSATISFIABLE for any PR that ever carried a
        # prose verdict headline: #2176 and #2172 hold four such lines between
        # them, and no amount of correct re-approval would clear them. A gate
        # that cannot be satisfied by doing the right thing gets routed around,
        # which is how it stops protecting anything. Lines at or after the
        # newest binding still block, because those could be the rejection the
        # parser failed to read.
        for entry in ${lane_malformed[@]+"${lane_malformed[@]}"}; do
            m_idx=${entry%% *}
            m_line=${entry#* }
            if [ "$newest_idx" -ge 0 ] && [ "$m_idx" -lt "$newest_idx" ]; then
                continue
            fi
            case $'\n'"$malformed_seen" in
                *$'\n'"$m_line"$'\n'*) ;;
                *) malformed_seen+="${m_line}"$'\n' ;;
            esac
        done

        if [ "${#lane_refused[@]}" -gt 0 ]; then
            latest_refusal=${lane_refused[-1]}
            refusal_cid=${latest_refusal%% *}
            refusal_detail=${latest_refusal#* }
            refusal_idx=${refusal_detail%% *}
            refusal_detail=${refusal_detail#* }
            refusal_sha=${refusal_detail%% *}
            refusal_issuer=${refusal_detail#* }
            fail "${#lane_refused[@]} standing refusal(s) remain for ${lane}; latest is at ${refusal_sha} from ${refusal_issuer} (comment ${refusal_cid}, index ${refusal_idx}). Only that issuer's later approval or withdrawal, or an entitled RETIRES naming that comment, can discharge it."
        elif [ "${#lane_bound[@]}" -eq 0 ] && [ "${#lane_unauthorized[@]}" -gt 0 ]; then
            fail "no trusted role-tagged exact-head approval from ${lane}: ${#lane_unauthorized[@]} \`APPROVED-AT: ${lane}\` line(s) were present but none came from an eligible review comment (${lane_unauthorized[0]}). A bare or public-commenter body is not authority; the comment must contain a bracketed reviewer or relay role tag and have OWNER, MEMBER, or COLLABORATOR association."
        elif [ "${#lane_bound[@]}" -eq 0 ]; then
            fail "no exact-head approval from ${lane}: found no \`APPROVED-AT: ${lane} <40-hex>\` line in any comment (the passed-review-${lane} label is a cache, not authority)."
        elif [ "${lane_bound[-1]}" != "$head_lc" ]; then
            fail "superseded approval from ${lane}: its newest binding is ${lane_bound[-1]}, but the current head is ${head_lc}; the approval must be re-earned at this head."
        fi
    done

    while IFS= read -r bad; do
        [ -n "$bad" ] || continue
        fail "unparseable verdict line (matches no known APPROVED-AT / CHANGES-REQUESTED-AT shape, so it binds nothing): ${bad}"
    done <<< "$malformed_seen"
fi

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
