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
#     APPROVED-AT: <claude|codex> <40-hex>
#
# matched case-insensitively against the whole line after whole-line markdown
# emphasis is removed. Rejections mirror it and win within a comment:
#
#     CHANGES-REQUESTED-AT: <claude|codex> <40-hex>
#     REQUEST CHANGES AT <40-hex>              (historical, lane-less)
#
# Verdicts are chronological: a rejection clears that lane's earlier approvals
# and a later approval can re-establish authority. A lane binds only when the
# NEWEST SHA it bound itself to equals the current head. A line carrying a
# verdict-ish keyword and a 40-hex that matches no known shape is reported and
# BLOCKS rather than being skipped — an unrecognised variant that is silently
# ignored reads as no approval, and a heading-prefixed verdict line
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
#                     `body`. PREFERRED, and what the workflow uses.
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
# PR_HEAD_SHA and PR_COMMENTS_JSON are REQUIRED whenever the
# `post-facto-human-review` label is present, and a missing, malformed, or
# unparseable value BLOCKS. They are deliberately not optional: a check that
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

# WHO MAY APPROVE. Until 2026-08-23 nothing here read anything but `.body`: the
# scan consumed `jq -r '.[]? | .body'` and matched the text, while the workflow
# handed it the complete, unfiltered `issues/<pr>/comments` array. So ANY account
# that can comment could mint `APPROVED-AT: claude <head>`. The label cache was
# already correctly treated as "not authority"; the comment body was quietly
# trusted as if it were.
#
# THE AUTHORITY IS THE ROLE-TAGGED REVIEW COMMENT, NOT GITHUB WRITE ACCESS.
# `ci-hub/health/approval_binding.py` states the rule this gate must agree with:
# "an approval holds only if a role-tagged review comment names the EXACT
# current head SHA for that lane", restating policy that "role-tagged review
# comments carrying the full head SHA are authority; numbered review and
# `passed-review-*` labels are caches."
#
# An earlier revision of this file gated on `author_association` instead and was
# rejected on review, for two measured reasons:
#
#   * It is not the authority. Association is a GitHub repository permission; it
#     says nothing about whether a review happened.
#   * It REJECTED THE REAL ATTESTATIONS. On this very pull request every genuine
#     approval was posted by the repository owner, who was also the author, and
#     one is an explicit relay: a reviewer with no route to api.github.com had
#     its verdict transcribed by another process. A commenter-is-not-the-author
#     rule refuses all of those, and "the reviewer could not reach GitHub" must
#     not read as "nobody reviewed".
#
# So the posting account is recorded for the audit trail and is NOT gated on.
# What is required is that the binding appear in a comment that presents itself
# as a review — a bracketed role tag, the convention already used across this
# fleet. The tag's INTERIOR is deliberately not parsed: approval_binding.py
# measured that guessing from it produces confidently wrong verdicts, because
# the tag does not encode the lane and its shape varies across the fleet
# (`[impl agent, claude-opus-5]`, `[adversarial-reviewer agent, GPT-5 Codex]`,
# `[hermit2, hermit-902, unresolved, devbig030, role=reviewer]`). The lane comes
# only from the explicit `APPROVED-AT: <lane> <40-hex>` binding, exactly as
# there. Presence of the tag is the signal; its contents are not interpreted.
readonly ROLE_TAG_RE='^\[[^][]+\]$'

# WHAT THIS DOES AND DOES NOT ESTABLISH, stated because overclaiming here is
# itself a defect. It closes the bare-line hole: a comment carrying nothing but
# `APPROVED-AT: <lane> <head>` no longer binds, whoever posts it and whatever
# their repository permission. It does NOT authenticate the tag's contents — a
# determined author can write a role tag. This fleet already says so in band:
# a disclosure line on this pull request reads "Disclosure, not authentication."
# Cryptographic reviewer identity would have to come from the registered
# boundary, not from a comment body, and that is not something this lint can
# assert on its own.
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
# The structural-prefix class is on the REJECTIONS and deliberately NOT on
# APPROVE_RE. `undecorate` removes markdown that WRAPS a line; a marker like
# `##`, `>`, or `-` is a prefix with no closer and survives it. Each direction
# here is the refusing one: a masked rejection is HONOURED so it revokes (being
# generous about recognising a rejection only ever withdraws authority), while a
# masked APPROVAL still cannot bind — extending the class to APPROVE_RE would
# let `## APPROVED-AT: ...` start authorising. Matches the reference verifier
# `ci-hub/health/approval_binding.py`; see its REJECT_PREFIX comment.
readonly VERDICT_PREFIX='[[:space:]#>*+-]*'
readonly APPROVE_RE="^APPROVED-AT:[[:space:]]*(claude|codex)[[:space:]]+(${SHA40_RE})$"
readonly REJECT_RE="^${VERDICT_PREFIX}CHANGES-REQUESTED-AT:[[:space:]]*(claude|codex)[[:space:]]+${SHA40_RE}$"
readonly REJECT_LEGACY_RE="^${VERDICT_PREFIX}REQUEST[[:space:]]+CHANGES[[:space:]]+AT[[:space:]]+${SHA40_RE}$"
# A line plainly trying to be a verdict binding but matching no known shape.
#
# DELIBERATELY STRICTER THAN THE REFERENCE, IN THE REFUSING DIRECTION ONLY.
# The leading `[#>*+-]` class is not in `approval_binding.py`. Without it a
# verdict line carrying a markdown STRUCTURAL prefix — `## CHANGES-REQUESTED-AT:
# claude <sha>` — matches neither the verdict grammar nor the suspect pattern,
# so it is silently invisible. Measured against the reference on 2026-08-13: for
# an approval followed by a `##`-prefixed rejection at the SAME head, the
# reference returns the approval with an empty malformed list, i.e. the lane
# reads as currently approved while its newest verdict is a rejection. That is a
# false PASS, and it is what happened on PR #2176.
#
# `undecorate` above cannot absorb these because it only removes markdown that
# WRAPS a line (matching opener and closer); a heading, blockquote, or list
# marker is a prefix with no closer.
#
# This gate therefore refuses such a line instead of ignoring it. The divergence
# can only ever turn a reference PASS into a refusal here, never the reverse —
# a property the differential in the PR description measures rather than
# asserts. The correct end state is for the reference to adopt the same class so
# both consumers agree again; that belongs in a dev-hermit change, not here.
readonly SUSPECT_RE="^[[:space:]#>*+-]*(APPROV|CHANGES-REQUESTED|REQUEST[[:space:]]+CHANGES|REJECT|LGTM|SIGN(ED)?[-[:space:]]?OFF|ACK).*${SHA40_RE}"

# Scan every comment for LANE and emit one tagged row per line of stdout:
#
#   S <sha>    a SHA this lane bound itself to, oldest first
#   M <line>   a verdict-ish line matching no known shape
#
# Everything travels out through stdout ON PURPOSE. The caller reads this with
# `mapfile < <(...)`, which runs the function in a subshell, so a global
# assigned in here would be silently discarded in the parent — an earlier draft
# collected malformed lines that way and the malformed check was inert while its
# tests still passed, because those cases also failed the binding check for an
# unrelated reason. Tagged stdout is what makes both results actually observable.
scan_lane() {
    local lane=$1 encoded body line undecorated login assoc
    local -a found=()
    local idx=-1
    # The reference grammar is case-insensitive throughout, so `APPROVED-AT:
    # CODEX <SHA>` binds exactly as `approved-at: codex <sha>` does. Scoped to
    # this function and restored on return.
    local had_nocasematch=0
    shopt -q nocasematch && had_nocasematch=1
    shopt -s nocasematch
    while IFS=$'\x1f' read -r login assoc encoded; do
        [ -n "$encoded" ] || continue
        idx=$((idx + 1))
        body=$(printf '%s' "$encoded" | base64 -d)
        # Decided per comment, before any line of it is read, so the same
        # verdict applies to every APPROVED-AT line the comment carries.
        # Scanned over the WHOLE comment, not just its first line: the relayed
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
        done <<< "$body"
        # A rejection contributes NOTHING from this comment, even if the same
        # comment also carries an APPROVED-AT-shaped line: a comment quoting the
        # approval it supersedes must not bind as a positive. Clearing rather
        # than skipping is load-bearing, or APPROVED-then-CHANGES-REQUESTED
        # would read as approved forever.
        local rejected=0
        while IFS= read -r line; do
            undecorated=$(undecorate "$line")
            if [[ $undecorated =~ $REJECT_LEGACY_RE ]]; then
                rejected=1
                break
            fi
            if [[ $undecorated =~ $REJECT_RE ]]; then
                local rejected_lane=${BASH_REMATCH[1]}
                if [ "${rejected_lane,,}" = "$lane" ]; then
                    rejected=1
                    break
                fi
            fi
        done <<< "$body"
        if [ "$rejected" -eq 1 ]; then
            found=()
            continue
        fi
        while IFS= read -r line; do
            undecorated=$(undecorate "$line")
            if [[ $undecorated =~ $APPROVE_RE ]]; then
                local matched_lane=${BASH_REMATCH[1]} matched_sha=${BASH_REMATCH[2]}
                if [ "${matched_lane,,}" = "$lane" ] && [ -n "$approver_refusal" ]; then
                    # Reported, not silently dropped. A refused approval that
                    # vanished would surface only as the generic "no approval
                    # from <lane>", which reads as "the reviewer never got to
                    # it" rather than "someone tried to mint this".
                    printf 'U %s %s (%s)\n' "$idx" "${login:-<unidentified>}" "$approver_refusal"
                elif [ "${matched_lane,,}" = "$lane" ]; then
                    # Recorded EXACTLY as written, deliberately not lowercased.
                    # The reference verifier compares the captured text against
                    # the API's lowercase head, so an upper-case SHA does not
                    # bind there. Normalising here would make this gate accept
                    # an approval the reference rejects, and two consumers
                    # disagreeing about what approval means is the whole defect.
                    found+=("$idx $matched_sha")
                fi
            elif [[ $undecorated =~ $SUSPECT_RE ]] \
                 && ! [[ $undecorated =~ $REJECT_RE ]] \
                 && ! [[ $undecorated =~ $REJECT_LEGACY_RE ]]; then
                # A well-formed rejection for the OTHER lane reaches here (it is
                # not this lane's rejection, and it is not an approval) and it
                # opens with a verdict keyword, so it matches SUSPECT_RE. It is
                # perfectly parseable and must not be reported as malformed.
                printf 'M %s %s\n' "$idx" "${undecorated:0:120}"
            fi
        done <<< "$body"
    # Carries the commenter out alongside the text, for the audit trail. `.body`
    # alone was the whole defect: it is the one field of a comment that its
    # author fully controls. @tsv over a fixed 3-field row keeps the split
    # unambiguous, and the body stays base64 so an embedded tab or newline
    # cannot shift the columns.
    #
    # TOTAL BY CONSTRUCTION. Every accessor is guarded, so no element can make
    # jq raise and truncate the stream mid-way. The input validation above
    # already refuses a non-object element; this is the second line of defence,
    # because the failure it prevents is silent and reads as "fewer comments"
    # rather than as an error.
    done < <(printf '%s' "$comments_json" \
        | jq -r '.[]? | if type == "object" then
                            [((.user // {}) | if type == "object" then (.login // "") else "" end),
                             (.author_association // ""),
                             (.body // "" | tostring | @base64)]
                        else ["", "", ""] end | join("\u001f")')
    # Truncation is still checked rather than assumed: if the loop saw fewer
    # comments than the payload holds, something dropped rows and the lane's
    # verdict was computed from a partial history.
    local seen=$((idx + 1)) declared
    declared=$(printf '%s' "$comments_json" | jq -r 'length' 2>/dev/null || echo -1)
    if [ "$declared" -ge 0 ] && [ "$seen" -ne "$declared" ]; then
        printf 'X read %s of %s comments; the stream was truncated\n' "$seen" "$declared"
    fi
    [ "$had_nocasematch" -eq 1 ] || shopt -u nocasematch
    local row
    for row in ${found[@]+"${found[@]}"}; do
        printf 'S %s\n' "$row"
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
        lane_truncated=()
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
                'X '*) lane_truncated+=("${row#X }") ;;
            esac
        done < <(scan_lane "$lane")

        # Before any verdict: a partial read cannot support one in either
        # direction, because the rows that went missing could be the rejection.
        for entry in ${lane_truncated[@]+"${lane_truncated[@]}"}; do
            fail "cannot verify ${lane}: ${entry}. A verdict computed from part of the comment history is not a verdict."
        done

        # Said out loud even when the lane also has a genuine approval: an
        # attempt to mint one is worth seeing in the log either way.
        for entry in ${lane_unauthorized[@]+"${lane_unauthorized[@]}"}; do
            echo "PR #${pr}: ignoring an unauthenticated ${lane} approval at comment ${entry% (*}: ${entry#* }"
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

        if [ "${#lane_bound[@]}" -eq 0 ] && [ "${#lane_unauthorized[@]}" -gt 0 ]; then
            fail "no AUTHENTICATED exact-head approval from ${lane}: ${#lane_unauthorized[@]} \`APPROVED-AT: ${lane}\` line(s) were present but none came from an authorized approver (${lane_unauthorized[0]}). A comment body is not authority; the approver must have write access and must not be the pull request's author."
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
