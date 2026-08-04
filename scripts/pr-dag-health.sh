#!/usr/bin/env bash
#
# pr-dag-health.sh -- health assessment for the floating PR DAG.
#
# Development model this tool serves:
#
#   main (always green)  +  floating DAG (human-review PRs)  =  frontier (derived)
#
# New work targets frontier and rebases back to main when it commutes. Review
# effort is the scarce resource, so this tool finds where a single human review
# unblocks the most work.
#
# What it does:
#   1. Enumerates all open PRs and classifies each (human-review,
#      blocked-on-human-review, blocked-on-dependency, landable-now, conflicted,
#      ci-red, detached-base, pending).
#   2. Builds the dependency DAG from branch stacking: PR X depends on PR Y when
#      X's base branch is Y's head branch.
#   3. Checks per-PR health: can it merge onto its base (no conflicts), and did
#      the repository's authoritative merge-gate context pass. Portable and
#      privileged jobs remain visible as diagnostics; Hermit's hosted leg
#      requires both unless an exact-head local receipt supplies its alternate.
#   4. Ranks review priority: "Review PR #X -> releases N PRs from floating to
#      landed", where N is the size of X's dependent subtree.
#   5. For floating (non-main-based) PRs, checks whether the PR's own commits
#      commute cleanly onto main (git merge-tree with the PR base as merge-base)
#      and recommends retargeting the ones that do (skip the float).
#   6. Reports main's HEAD and its CI (green check).
#   7. Emits a human-readable summary and a machine-readable JSON document.
#
# All GitHub access goes through the `with-proxy` wrapper (Meta devserver egress
# requirement); override with PR_DAG_PROXY="" to call gh directly.
#
# Usage:
#   scripts/pr-dag-health.sh [--repo OWNER/NAME] [--format human|json|both]
#                            [--out FILE] [--no-commute] [--limit N]
#
# Exit status: 0 on success, 1 on a hard failure (gh/jq missing, gh error).

set -uo pipefail

REPO="${PR_DAG_REPO:-rrnewton/hermit}"
MAIN_BRANCH="${PR_DAG_MAIN:-main}"
FORMAT="both"                 # human | json | both
OUT="pr-dag-health.json"
DO_COMMUTE=1
LIMIT=200

die() { echo "pr-dag-health: $*" >&2; exit 1; }
log() { echo "$*" >&2; }

while [ $# -gt 0 ]; do
    case "$1" in
        --repo) REPO="$2"; shift 2 ;;
        --format) FORMAT="$2"; shift 2 ;;
        --out) OUT="$2"; shift 2 ;;
        --no-commute) DO_COMMUTE=0; shift ;;
        --limit) LIMIT="$2"; shift 2 ;;
        -h|--help) sed -n '2,40p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) die "unknown argument: $1 (try --help)" ;;
    esac
done

case "$FORMAT" in human|json|both) ;; *) die "--format must be human, json, or both" ;; esac
command -v gh >/dev/null 2>&1 || die "gh not found on PATH"
command -v jq >/dev/null 2>&1 || die "jq not found on PATH"

# Proxy wrapper: default to with-proxy when present, unless PR_DAG_PROXY overrides.
PROXY="${PR_DAG_PROXY-with-proxy}"
if [ -n "$PROXY" ] && ! command -v "$PROXY" >/dev/null 2>&1; then PROXY=""; fi
gh_()  { if [ -n "$PROXY" ]; then "$PROXY" gh  "$@"; else gh  "$@"; fi; }
git_() { if [ -n "$PROXY" ]; then "$PROXY" git "$@"; else git "$@"; fi; }

log "pr-dag-health: querying open PRs for $REPO ..."
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RAW="$(gh_ pr list -R "$REPO" --state open --limit "$LIMIT" \
    --json number,title,headRefName,headRefOid,baseRefName,labels,isDraft,author,mergeable,mergeStateStatus,statusCheckRollup 2>/dev/null)" \
    || die "gh pr list failed (is the proxy/auth configured?)"
[ -n "$RAW" ] || die "gh returned no data"
RAW="$(printf '%s' "$RAW" | python3 "$SCRIPT_DIR/../agent-utils/py/ci_hub_check_outcome.py" --annotate-rollups)" \
    || die "check-status annotation failed"
GATE_CONTEXT=merge-gate
[[ $REPO == rrnewton/hermit ]] && GATE_CONTEXT=merge-gate-v4

# ---------------------------------------------------------------------------
# Enrich each PR and build the dependency DAG (jq). Emits an array of PRs, each
# with: ci{regular,hostdep,overall}, is_human_review, parent (PR number|null),
# base_is_main, conflicts, blocked_on_human_review, released_prs[], releases,
# class.
# ---------------------------------------------------------------------------
ENRICH_JQ='
def result: (.conclusion // .state // "NONE");
def latest_named($rollup; $name):
  ($rollup
   | map(select((.name // .context)==$name))
   | first);
def ci_of($rollup):
  ($rollup // []) as $r
  | latest_named($r; $gate_context) as $gate
  | latest_named($r; "Regular tests (GitHub-managed portable)") as $regular
  | latest_named($r; "Privileged capability and E2E tests") as $hostdep
  | { checks: ($r|length),
      gate: (if $gate==null then "NONE" else ($gate|result) end),
      regular: (if $regular==null then "NONE" else ($regular|result) end),
      hostdep: (if $hostdep==null then "NONE" else ($hostdep|result) end),
      overall: (if $gate==null then "NO_RESULT" else ($gate._checkOutcome // "NO_RESULT") end) };

. as $prs
| ($prs | map({(.headRefName): .number}) | add // {}) as $byhead
| ($prs | map(. + {
      ci: ci_of(.statusCheckRollup),
      is_human_review: (any(.labels[]?; .name=="human-review")),
      parent: ($byhead[.baseRefName] // null),
      base_is_main: (.baseRefName==$main),
      conflicts: ((.mergeStateStatus=="DIRTY") or (.mergeable=="CONFLICTING"))
    })) as $enr
| ($enr | map({(.number|tostring): .parent}) | add) as $pmap
| ($enr | map({(.number|tostring): .is_human_review}) | add) as $hrmap
| ($enr | map(select(.parent!=null) | {p:(.parent|tostring), c:.number})
        | group_by(.p) | map({key:.[0].p, value:(map(.c))}) | from_entries) as $children
| def anc_hr($n):
    ($pmap[$n|tostring]) as $p
    | if $p==null then false elif ($hrmap[$p|tostring]) then true else anc_hr($p) end;
  def descs($n):
    ($children[$n|tostring] // []) as $k
    | $k + ($k | map(descs(.)) | add // []);
  $enr
  | map(. + {
      blocked_on_human_review: anc_hr(.number),
      released_prs: (descs(.number) | sort),
      releases: (descs(.number) | length),
      class:
        (if .is_human_review then "human-review"
         elif .parent != null then (if anc_hr(.number) then "blocked-on-human-review" else "blocked-on-dependency" end)
         elif (.base_is_main | not) then "detached-base"
         elif .conflicts then "conflicted"
         elif .ci.overall=="FAILED" then "ci-red"
         elif .ci.overall=="PASSED" then "landable-now"
         else "pending" end) })
  | map({number, title, headRefName, baseRefName,
         author: (.author.login // "?"),
         isDraft, mergeable, mergeStateStatus,
         ci, is_human_review, parent, base_is_main, conflicts,
         blocked_on_human_review, releases, released_prs, class})
'

ENRICHED="$(printf '%s' "$RAW" | jq -L "$SCRIPT_DIR" \
    --arg main "$MAIN_BRANCH" --arg gate_context "$GATE_CONTEXT" "$ENRICH_JQ")" \
    || die "jq enrichment failed"

# ---------------------------------------------------------------------------
# Commute check: for each floating PR (base != main), does the PR's own delta
# apply cleanly onto main? We 3-way merge main and the PR head using the PR's
# current base as the merge base, which isolates the PR's own commits. Requires
# a local checkout with origin refs; degrades to "unknown" otherwise.
# ---------------------------------------------------------------------------
COMMUTE_JSON='{}'
if [ "$DO_COMMUTE" -eq 1 ]; then
    REPO_DIR="$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel 2>/dev/null || true)"
    if [ -z "$REPO_DIR" ]; then
        log "pr-dag-health: no local checkout found; skipping commute check"
    else
        log "pr-dag-health: fetching origin refs for commute check ..."
        git_ -C "$REPO_DIR" fetch --quiet origin 2>/dev/null || \
            log "pr-dag-health: warning: git fetch failed; commute results may be stale"
        tmp_commute="$(mktemp)"; echo '{}' > "$tmp_commute"
        while IFS=$'\t' read -r num head base; do
            [ -n "$num" ] || continue
            if ! git -C "$REPO_DIR" rev-parse -q --verify "origin/$head" >/dev/null 2>&1 \
               || ! git -C "$REPO_DIR" rev-parse -q --verify "origin/$base" >/dev/null 2>&1 \
               || ! git -C "$REPO_DIR" rev-parse -q --verify "origin/$MAIN_BRANCH" >/dev/null 2>&1; then
                st="unknown"
            elif git -C "$REPO_DIR" merge-tree --write-tree \
                    --merge-base="origin/$base" "origin/$MAIN_BRANCH" "origin/$head" >/dev/null 2>&1; then
                st="clean"
            else
                st="conflicts"
            fi
            jq --arg n "$num" --arg s "$st" '. + {($n): $s}' "$tmp_commute" > "$tmp_commute.new" \
                && mv "$tmp_commute.new" "$tmp_commute"
        done < <(printf '%s' "$ENRICHED" | jq -r '.[] | select(.base_is_main|not) | [.number, .headRefName, .baseRefName] | @tsv')
        COMMUTE_JSON="$(cat "$tmp_commute")"; rm -f "$tmp_commute"
    fi
fi

# ---------------------------------------------------------------------------
# main HEAD + its CI (green check).
# ---------------------------------------------------------------------------
MAIN_FULL_SHA="$(gh_ api "repos/$REPO/commits/$MAIN_BRANCH" --jq '.sha' 2>/dev/null)"
MAIN_SHA="${MAIN_FULL_SHA:0:12}"
MAIN_CHECKS="$(gh_ api "repos/$REPO/commits/$MAIN_BRANCH/check-runs" 2>/dev/null)"
MAIN_CI="$(printf '%s' "$MAIN_CHECKS" \
    | python3 "$SCRIPT_DIR/../agent-utils/py/ci_hub_check_outcome.py" \
        --select-latest-rollup --head-sha "$MAIN_FULL_SHA" \
    | jq -r '[.[] | select(.name=="Regular tests (GitHub-managed portable)")] | (.[0].conclusion // "none")')"
[ -n "$MAIN_SHA" ] || MAIN_SHA="unknown"
[ -n "$MAIN_CI" ]  || MAIN_CI="unknown"
MAIN_OUTCOME="$(python3 "$SCRIPT_DIR/../agent-utils/py/ci_hub_check_outcome.py" \
    --status COMPLETED --conclusion "$MAIN_CI")" || die "main check-status classification failed"

# ---------------------------------------------------------------------------
# Assemble the final JSON model.
# ---------------------------------------------------------------------------
MODEL="$(printf '%s' "$ENRICHED" | jq \
    --arg repo "$REPO" --arg main "$MAIN_BRANCH" \
    --arg mainsha "$MAIN_SHA" --arg mainci "$MAIN_CI" \
    --arg mainoutcome "$MAIN_OUTCOME" --argjson commute "$COMMUTE_JSON" '
  map(. + {commutes_to_main: ($commute[(.number|tostring)] // "n/a")}) as $prs
  | {
      generated_by: "pr-dag-health.sh",
      repo: $repo,
      main: {branch: $main, head: $mainsha, ci_regular: $mainci,
             outcome: $mainoutcome, green: ($mainoutcome=="PASSED")},
      summary: {
        total: ($prs | length),
        by_class: ($prs | group_by(.class) | map({key: .[0].class, value: length}) | from_entries),
        conflicted: ($prs | map(select(.conflicts)) | length),
        ci_red: ($prs | map(select(.ci.overall=="FAILED")) | length),
        ci_no_result: ($prs | map(select(.ci.overall=="NO_RESULT")) | length),
        floating: ($prs | map(select(.base_is_main|not)) | length)
      },
      review_priority: ($prs
        | map(select(.is_human_review))
        | sort_by(-.releases, .number)
        | map({number, title, releases, released_prs,
               conflicts, ci: .ci.overall, mergeStateStatus})),
      commute_candidates: ($prs
        | map(select(.commutes_to_main=="clean"))
        | sort_by(.number)
        | map({number, title, base: .baseRefName, releases})),
      prs: $prs
    }')" || die "jq model assembly failed"

# ---------------------------------------------------------------------------
# Emit JSON (stdout for --format json, else a file).
# ---------------------------------------------------------------------------
if [ "$FORMAT" = json ]; then
    printf '%s\n' "$MODEL"
else
    printf '%s\n' "$MODEL" > "$OUT" || die "could not write $OUT"
    log "pr-dag-health: wrote machine-readable JSON to $OUT"
fi

# ---------------------------------------------------------------------------
# Human-readable report.
# ---------------------------------------------------------------------------
if [ "$FORMAT" != json ]; then
    printf '%s' "$MODEL" | jq -r '
      def col($n): (tostring | (. + (" " * $n))[:$n]);
      "==================================================================",
      "PR DAG health -- \(.repo)",
      "==================================================================",
      "main \(.main.branch) @ \(.main.head)  |  Regular-tests CI: \(.main.ci_regular)  |  green: \(.main.green)",
      "",
      "Open PRs: \(.summary.total)   floating(non-main base): \(.summary.floating)   conflicted: \(.summary.conflicted)   ci-red: \(.summary.ci_red)   ci-no-result: \(.summary.ci_no_result)",
      "By class:",
      (.summary.by_class | to_entries[] | "  \(.key|col(26)) \(.value)"),
      "",
      "------------------------------------------------------------------",
      "MAXIMUM-UNBLOCK REVIEW PRIORITY (human-review PRs, most leverage first)",
      "------------------------------------------------------------------",
      (if (.review_priority|length)==0 then "  (no open human-review PRs)"
       else (.review_priority[]
         | "  Review PR #\(.number) -> releases \(.releases) PR(s) from floating to landed"
           + (if (.released_prs|length)>0 then "  [#" + (.released_prs|map(tostring)|join(", #")) + "]" else "" end)
           + "\n      \(.title)"
           + "\n      health: conflicts=\(.conflicts)  CI=\(.ci)  merge=\(.mergeStateStatus)")
       end),
      "",
      "------------------------------------------------------------------",
      "COMMUTE-TO-MAIN CANDIDATES (floating PRs whose delta applies cleanly to main)",
      "------------------------------------------------------------------",
      (if (.commute_candidates|length)==0 then "  (none detected / commute check unavailable)"
       else (.commute_candidates[]
         | "  PR #\(.number) (base \(.base)) commutes cleanly -> consider: gh pr edit \(.number) --base main   (skip the float)"
           + "\n      \(.title)")
       end),
      "",
      "------------------------------------------------------------------",
      "PER-PR DETAIL (grouped by class)",
      "------------------------------------------------------------------",
      (.prs | group_by(.class)[]
        | "[\(.[0].class)]  (\(length))",
          (sort_by(.number)[]
            | "  #\(.number|col(4))"
              + " base=\(.baseRefName|col(22))"
              + " conflicts=\(.conflicts|col(5))"
              + " CI=\(.ci.overall|col(8))"
              + " commute=\(.commutes_to_main|col(9))"
              + " releases=\(.releases)"
              + "  \(.title)"),
          "")
    '
fi

exit 0
