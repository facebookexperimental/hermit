#!/usr/bin/env bash
# Reconcile Hermit's landing-branch rulesets with the owner policy: green
# branches may land by fast-forward; branch deletion and non-fast-forward
# updates are prohibited. Hosted checks and PR state remain advisory.
#
# There is deliberately no linear-history rule (none on the live history
# ruleset since 2026-11-09). Imports from facebookexperimental land on the
# landing branch as exact ancestry merges: two-parent merge commits that keep
# the imported history as an ancestor, which a linear-history rule refuses.
# Examples: https://github.com/rrnewton/hermit/pull/5377 (29da3587),
# https://github.com/rrnewton/hermit/pull/5385 (7c04b4bc) and
# https://github.com/rrnewton/hermit/pull/5391 (f8b2162f). --check therefore
# refuses a history ruleset that carries required_linear_history, and --apply
# removes it.
#
# The two landing rulesets name the repository's default branch
# (~DEFAULT_BRANCH), so they follow it when the landing branch is renamed.
#
# One additional ruleset may protect the upstream branch, the fast-forward-only
# inbox that mirrors facebookexperimental (HERMIT_UPSTREAM_BRANCH from the
# environment or ci/landing-branches.env, or --upstream-branch; main today). It
# is recognized by naming exactly refs/heads/<upstream> and nothing else.
# --check accepts it without judging its rules, which are the owner's, and
# --apply never writes it. --check fails when two rulesets each protect the
# upstream branch, or when a ruleset names it among other refs; the JSON
# booleans keep describing only the two landing rulesets, so such a failure
# reads state=fail with the reason on stderr. --apply refuses to rewrite a
# landing ruleset that names the upstream branch, because rewriting it to
# ~DEFAULT_BRANCH would silently drop that branch's protection.

set -euo pipefail

readonly DEFAULT_REPO="rrnewton/hermit"
readonly DEFAULT_GATE_RULESET_NAME="main check gating (admin-bypassable)"
readonly DEFAULT_HISTORY_RULESET_NAME="main history protection"

# shellcheck source=scripts/lib/landing_branch.sh
source "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/lib/landing_branch.sh"

repo=$DEFAULT_REPO
gate_ruleset_name=$DEFAULT_GATE_RULESET_NAME
history_ruleset_name=$DEFAULT_HISTORY_RULESET_NAME
upstream_branch=
mode=check
json_output=0

usage() {
    cat <<'EOF'
Usage: scripts/configure-merge-gate-ruleset.sh MODE [options]

Modes:
  --check          Verify the exact owner policy without changing GitHub.
  --apply          Empty the check-gating ruleset and enforce zero-bypass
                   deletion + non-fast-forward rules (no linear-history rule).

Options:
  --repo R                  Repository (default: rrnewton/hermit).
  --ruleset-name N          Legacy check-gating ruleset name.
  --history-ruleset-name N  History-protection ruleset name.
  --upstream-branch B       Upstream (inbox) branch whose one additional
                            ruleset is tolerated (default: HERMIT_UPSTREAM_BRANCH
                            from the environment or ci/landing-branches.env).
  --json                     Emit the check result as one JSON object.
  -h, --help                Show this help.
EOF
}

while (($# > 0)); do
    case "$1" in
        --check) mode=check; shift ;;
        --apply) mode=apply; shift ;;
        --repo) repo=${2:?--repo requires OWNER/NAME}; shift 2 ;;
        --ruleset-name) gate_ruleset_name=${2:?--ruleset-name requires a value}; shift 2 ;;
        --history-ruleset-name)
            history_ruleset_name=${2:?--history-ruleset-name requires a value}
            shift 2
            ;;
        --upstream-branch)
            upstream_branch=${2:?--upstream-branch requires a branch}
            shift 2
            ;;
        --json) json_output=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) printf 'configure-merge-gate-ruleset: unknown argument: %s\n' "$1" >&2; exit 2 ;;
    esac
done

if [[ $mode != check && $json_output == 1 ]]; then
    printf 'configure-merge-gate-ruleset: --json is valid only with --check\n' >&2
    exit 2
fi

if [[ -z $upstream_branch ]]; then
    upstream_branch=$(landing_branch HERMIT_UPSTREAM_BRANCH) || exit 2
elif ! landing_branch_valid "$upstream_branch"; then
    printf 'configure-merge-gate-ruleset: --upstream-branch %q is not a branch name\n' \
        "$upstream_branch" >&2
    exit 2
fi
readonly upstream_ref="refs/heads/$upstream_branch"

for command in gh jq sha256sum; do
    if ! command -v "$command" >/dev/null 2>&1; then
        printf 'configure-merge-gate-ruleset: required command not found: %s\n' "$command" >&2
        exit 2
    fi
done

gh_cmd=(gh)
if command -v with-proxy >/dev/null 2>&1; then
    gh_cmd=(with-proxy gh)
fi

rulesets=$("${gh_cmd[@]}" api --paginate "repos/$repo/rulesets")

ruleset_id_by_name() {
    local name=$1 matches count
    matches=$(jq --arg name "$name" '[.[] | select(.name == $name)]' <<<"$rulesets")
    count=$(jq 'length' <<<"$matches")
    if [[ $count != 1 ]]; then
        printf 'configure-merge-gate-ruleset: expected one ruleset named %q, found %s\n' \
            "$name" "$count" >&2
        return 1
    fi
    jq -r '.[0].id' <<<"$matches"
}

gate_ruleset_id=$(ruleset_id_by_name "$gate_ruleset_name")
history_ruleset_id=$(ruleset_id_by_name "$history_ruleset_name")

read_ruleset() {
    "${gh_cmd[@]}" api "repos/$repo/rulesets/$1"
}

write_ruleset() {
    local id=$1 policy=$2
    printf '%s\n' "$policy" | "${gh_cmd[@]}" api \
        --method PUT "repos/$repo/rulesets/$id" --input - >/dev/null
}

normalized_policy() {
    jq -S '{name, target, enforcement, conditions, rules, bypass_actors}'
}

policy_fingerprint() {
    normalized_policy | sha256sum | awk '{print $1}'
}

rule_types() {
    jq -r '[.rules[].type] | sort | join(",")'
}

scope_state() {
    jq -r '[.target, (.conditions | tojson)] | join(" / ")'
}

scope_is_default_branch() {
    jq -e '
      .target == "branch" and
      .conditions == {
        ref_name: {exclude: [], include: ["~DEFAULT_BRANCH"]}
      }
    ' >/dev/null
}

# Whether the ruleset on stdin names the upstream branch among its includes.
names_upstream() {
    jq -e --arg ref "$upstream_ref" \
        '(.conditions.ref_name.include // []) | any(.[]; . == $ref)' >/dev/null
}

# Whether the ruleset on stdin has the upstream ruleset's scope: a branch
# ruleset that names refs/heads/<upstream> and nothing else.
scope_is_upstream_only() {
    jq -e --arg ref "$upstream_ref" '
      .target == "branch" and
      .conditions == {ref_name: {exclude: [], include: [$ref]}}
    ' >/dev/null
}

current_gate=$(read_ruleset "$gate_ruleset_id")
current_history=$(read_ruleset "$history_ruleset_id")
gate_rule_count=$(jq '.rules | length' <<<"$current_gate")
gate_bypass_count=$(jq '.bypass_actors | length' <<<"$current_gate")
gate_enforcement=$(jq -r '.enforcement' <<<"$current_gate")
history_types=$(rule_types <<<"$current_history")
history_bypass_count=$(jq '.bypass_actors | length' <<<"$current_history")
history_enforcement=$(jq -r '.enforcement' <<<"$current_history")

if [[ $mode == check ]]; then
    failed=0
    gate_matches=true
    history_matches=true
    if [[ $gate_rule_count != 0 || $gate_bypass_count != 0 ||
          $gate_enforcement != active ]] || ! scope_is_default_branch <<<"$current_gate"; then
        printf 'FAIL: check-gating ruleset %s has types=%s enforcement=%s bypass_count=%s scope=%s; expected no rules / active / 0 / branch default-only.\n' \
            "$gate_ruleset_id" "$(rule_types <<<"$current_gate")" "$gate_enforcement" \
            "$gate_bypass_count" "$(scope_state <<<"$current_gate")" >&2
        failed=1
        gate_matches=false
    fi
    # Exactly these two rules: required_linear_history would refuse the
    # ancestry merges that facebookexperimental imports land as (see the
    # header).
    if [[ $history_types != deletion,non_fast_forward ||
          $history_bypass_count != 0 || $history_enforcement != active ]] ||
       ! scope_is_default_branch <<<"$current_history"; then
        printf 'FAIL: history ruleset %s has types=%s enforcement=%s bypass_count=%s scope=%s; expected deletion,non_fast_forward / active / 0 / branch default-only.\n' \
            "$history_ruleset_id" "${history_types:-none}" "$history_enforcement" \
            "$history_bypass_count" "$(scope_state <<<"$current_history")" >&2
        failed=1
        history_matches=false
    fi
    # Every other ruleset that names the upstream branch. One with exactly the
    # upstream scope is tolerated, whatever its rules; it is not a landing
    # ruleset, so it changes neither JSON boolean.
    mapfile -t other_ids < <(
        jq -r --argjson gate "$gate_ruleset_id" --argjson history "$history_ruleset_id" \
            '.[] | select(.id != $gate and .id != $history) | .id' <<<"$rulesets"
    )
    upstream_ids=()
    for id in "${other_ids[@]}"; do
        candidate=$(read_ruleset "$id" </dev/null)
        names_upstream <<<"$candidate" || continue
        if scope_is_upstream_only <<<"$candidate"; then
            upstream_ids+=("$id")
        else
            printf 'FAIL: ruleset %s names the upstream branch %s among other refs (scope=%s); the upstream ruleset must name only %s.\n' \
                "$id" "$upstream_branch" "$(scope_state <<<"$candidate")" "$upstream_ref" >&2
            failed=1
        fi
    done
    if ((${#upstream_ids[@]} > 1)); then
        printf 'FAIL: rulesets %s each protect the upstream branch %s; at most one additional ruleset may.\n' \
            "${upstream_ids[*]}" "$upstream_ref" >&2
        failed=1
    fi
    if ((failed != 0)); then
        if ((json_output == 1)); then
            jq -cn \
                --arg state fail \
                --argjson gate_matches "$gate_matches" \
                --argjson history_matches "$history_matches" \
                '{schema:1,state:$state,gate_policy_matches:$gate_matches,history_policy_matches:$history_matches}'
        fi
        exit 1
    fi
    if ((json_output == 1)); then
        jq -cn \
            --arg state pass \
            '{schema:1,state:$state,gate_policy_matches:true,history_policy_matches:true}'
    else
        upstream_note=
        if ((${#upstream_ids[@]} == 1)); then
            upstream_note="; upstream ruleset ${upstream_ids[0]} protects $upstream_ref (its rules are not judged here)"
        fi
        printf 'PASS: check-gating ruleset %s is inert; history ruleset %s enforces zero-bypass deletion + non-fast-forward, with no linear-history rule%s.\n' \
            "$gate_ruleset_id" "$history_ruleset_id" "$upstream_note"
    fi
    exit 0
fi

# A landing ruleset that names the upstream branch is not rewritten: rewriting
# it to ~DEFAULT_BRANCH would drop that branch's protection without a word.
for landing_ruleset in "$current_gate" "$current_history"; do
    if names_upstream <<<"$landing_ruleset"; then
        printf 'configure-merge-gate-ruleset: ruleset %s names the upstream branch %s; rewriting it to ~DEFAULT_BRANCH would drop that protection. Give the upstream branch its own ruleset first.\n' \
            "$(jq -r '.id' <<<"$landing_ruleset")" "$upstream_ref" >&2
        exit 1
    fi
done

desired_gate=$(jq '
  {
    name,
    target: "branch",
    enforcement: "active",
    conditions: {ref_name: {exclude: [], include: ["~DEFAULT_BRANCH"]}},
    rules: [],
    bypass_actors: []
  }
' <<<"$current_gate")
desired_history=$(jq '
  {
    name,
    target: "branch",
    enforcement: "active",
    conditions: {ref_name: {exclude: [], include: ["~DEFAULT_BRANCH"]}},
    rules: [
      {type: "deletion"},
      {type: "non_fast_forward"}
    ],
    bypass_actors: []
  }
' <<<"$current_history")

# GitHub updates full ruleset objects and exposes no conditional PUT. Re-read
# both snapshots before the first write; after that, update history protection
# first so a partial failure cannot remove landing gates before the two durable
# history restrictions are established.
latest_gate=$(read_ruleset "$gate_ruleset_id")
latest_history=$(read_ruleset "$history_ruleset_id")
if [[ $(policy_fingerprint <<<"$latest_gate") != $(policy_fingerprint <<<"$current_gate") ]] ||
   [[ $(policy_fingerprint <<<"$latest_history") != $(policy_fingerprint <<<"$current_history") ]]; then
    printf 'configure-merge-gate-ruleset: ruleset changed concurrently; refusing stale full-object PUT\n' >&2
    exit 1
fi

if [[ $(policy_fingerprint <<<"$current_history") != $(policy_fingerprint <<<"$desired_history") ]]; then
    write_ruleset "$history_ruleset_id" "$desired_history"
fi
updated_history=$(read_ruleset "$history_ruleset_id")
if [[ $(policy_fingerprint <<<"$updated_history") != $(policy_fingerprint <<<"$desired_history") ]]; then
    printf 'configure-merge-gate-ruleset: history-policy reconciliation verification failed\n' >&2
    exit 1
fi

if [[ $(policy_fingerprint <<<"$current_gate") != $(policy_fingerprint <<<"$desired_gate") ]]; then
    write_ruleset "$gate_ruleset_id" "$desired_gate"
fi
updated_gate=$(read_ruleset "$gate_ruleset_id")
updated_history=$(read_ruleset "$history_ruleset_id")
if [[ $(policy_fingerprint <<<"$updated_gate") != $(policy_fingerprint <<<"$desired_gate") ]] ||
   [[ $(policy_fingerprint <<<"$updated_history") != $(policy_fingerprint <<<"$desired_history") ]]; then
    printf 'configure-merge-gate-ruleset: final two-ruleset verification failed\n' >&2
    exit 1
fi

printf 'APPLIED: check-gating ruleset %s is inert; history ruleset %s enforces zero-bypass deletion + non-fast-forward, with no linear-history rule.\n' \
    "$gate_ruleset_id" "$history_ruleset_id"
