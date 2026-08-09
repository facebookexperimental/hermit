#!/usr/bin/env bash
# Reconcile Hermit's main rulesets with the owner policy: green branches may
# land by fast-forward; only branch deletion and non-fast-forward updates are
# prohibited. Hosted checks, PR state, and linear-history shape are advisory.

set -euo pipefail

readonly DEFAULT_REPO="rrnewton/hermit"
readonly DEFAULT_GATE_RULESET_NAME="main check gating (admin-bypassable)"
readonly DEFAULT_HISTORY_RULESET_NAME="main history protection"

repo=$DEFAULT_REPO
gate_ruleset_name=$DEFAULT_GATE_RULESET_NAME
history_ruleset_name=$DEFAULT_HISTORY_RULESET_NAME
mode=check

usage() {
    cat <<'EOF'
Usage: scripts/configure-merge-gate-ruleset.sh MODE [options]

Modes:
  --check          Verify the exact owner policy without changing GitHub.
  --apply          Empty the check-gating ruleset and retain only zero-bypass
                   deletion + non-fast-forward history rules.

Options:
  --repo R                  Repository (default: rrnewton/hermit).
  --ruleset-name N          Legacy check-gating ruleset name.
  --history-ruleset-name N  History-protection ruleset name.
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
        -h|--help) usage; exit 0 ;;
        *) printf 'configure-merge-gate-ruleset: unknown argument: %s\n' "$1" >&2; exit 2 ;;
    esac
done

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

current_gate=$(read_ruleset "$gate_ruleset_id")
current_history=$(read_ruleset "$history_ruleset_id")
gate_rule_count=$(jq '.rules | length' <<<"$current_gate")
history_types=$(rule_types <<<"$current_history")
history_bypass_count=$(jq '.bypass_actors | length' <<<"$current_history")
history_enforcement=$(jq -r '.enforcement' <<<"$current_history")

if [[ $mode == check ]]; then
    failed=0
    if [[ $gate_rule_count != 0 ]]; then
        printf 'FAIL: check-gating ruleset %s has %s rule(s) (types=%s); it must be inert.\n' \
            "$gate_ruleset_id" "$gate_rule_count" "$(rule_types <<<"$current_gate")" >&2
        failed=1
    fi
    if [[ $history_types != deletion,non_fast_forward ||
          $history_bypass_count != 0 || $history_enforcement != active ]]; then
        printf 'FAIL: history ruleset %s has types=%s enforcement=%s bypass_count=%s; expected deletion,non_fast_forward / active / 0.\n' \
            "$history_ruleset_id" "${history_types:-none}" "$history_enforcement" \
            "$history_bypass_count" >&2
        failed=1
    fi
    if ((failed != 0)); then
        exit 1
    fi
    printf 'PASS: check-gating ruleset %s is inert; history ruleset %s enforces only zero-bypass deletion + non-fast-forward.\n' \
        "$gate_ruleset_id" "$history_ruleset_id"
    exit 0
fi

desired_gate=$(jq '
  {name, target, enforcement, conditions, rules: [], bypass_actors}
' <<<"$current_gate")
desired_history=$(jq '
  {
    name,
    target,
    enforcement: "active",
    conditions,
    rules: [.rules[] | select(.type == "deletion" or .type == "non_fast_forward")],
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

printf 'APPLIED: check-gating ruleset %s is inert; history ruleset %s enforces only zero-bypass deletion + non-fast-forward.\n' \
    "$gate_ruleset_id" "$history_ruleset_id"
