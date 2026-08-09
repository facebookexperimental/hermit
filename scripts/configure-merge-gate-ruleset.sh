#!/usr/bin/env bash
# Keep Hermit's main-branch check ruleset advisory.  Owner policy permits an
# exact-head green branch to land by fast-forward without waiting for hosted
# GitHub checks; deletion and non-fast-forward updates remain prohibited by the
# separate "main history protection" ruleset.

set -euo pipefail

readonly DEFAULT_REPO="rrnewton/hermit"
readonly DEFAULT_RULESET_NAME="main check gating (admin-bypassable)"

repo=$DEFAULT_REPO
ruleset_name=$DEFAULT_RULESET_NAME
mode=check

usage() {
    cat <<'EOF'
Usage: scripts/configure-merge-gate-ruleset.sh MODE [options]

Modes:
  --check          Verify that hosted status checks are advisory (default).
  --apply          Remove every required-status-check rule while preserving all
                   other ruleset fields.

Options:
  --repo R         Repository (default: rrnewton/hermit).
  --ruleset-name N Ruleset to reconcile.
  -h, --help       Show this help.
EOF
}

while (($# > 0)); do
    case "$1" in
        --check) mode=check; shift ;;
        --apply) mode=apply; shift ;;
        --repo) repo=${2:?--repo requires OWNER/NAME}; shift 2 ;;
        --ruleset-name) ruleset_name=${2:?--ruleset-name requires a value}; shift 2 ;;
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
matches=$(jq --arg name "$ruleset_name" '[.[] | select(.name == $name)]' <<<"$rulesets")
match_count=$(jq 'length' <<<"$matches")
if [[ $match_count != 1 ]]; then
    printf 'configure-merge-gate-ruleset: expected one ruleset named %q, found %s\n' \
        "$ruleset_name" "$match_count" >&2
    exit 1
fi
ruleset_id=$(jq -r '.[0].id' <<<"$matches")

read_ruleset() {
    "${gh_cmd[@]}" api "repos/$repo/rulesets/$ruleset_id"
}

normalized_policy() {
    jq -S '{name, target, enforcement, conditions, rules, bypass_actors}'
}

policy_fingerprint() {
    normalized_policy | sha256sum | awk '{print $1}'
}

required_status_rule_count() {
    jq '[.rules[] | select(.type == "required_status_checks")] | length'
}

current=$(read_ruleset)
required_count=$(required_status_rule_count <<<"$current")

if [[ $mode == check ]]; then
    if [[ $required_count != 0 ]]; then
        contexts=$(jq -r '[.rules[]
          | select(.type == "required_status_checks")
          | .parameters.required_status_checks[]?.context] | unique | join(",")' \
          <<<"$current")
        printf 'FAIL: ruleset %s has %s required-status-check rule(s) (contexts=%s); hosted checks must remain advisory.\n' \
            "$ruleset_id" "$required_count" "${contexts:-none}" >&2
        exit 1
    fi
    printf 'PASS: ruleset %s has no required status checks; hosted checks are advisory.\n' \
        "$ruleset_id"
    exit 0
fi

desired=$(jq '
  {
    name,
    target,
    enforcement,
    conditions,
    rules: [.rules[] | select(.type != "required_status_checks")],
    bypass_actors
  }
' <<<"$current")

# GitHub updates the full ruleset object.  Refuse a stale PUT if any field
# changed after the snapshot, and verify the complete normalized post-state.
latest=$(read_ruleset)
if [[ $(policy_fingerprint <<<"$latest") != $(policy_fingerprint <<<"$current") ]]; then
    printf 'configure-merge-gate-ruleset: ruleset changed concurrently; refusing stale full-object PUT\n' >&2
    exit 1
fi

if [[ $(policy_fingerprint <<<"$current") != $(policy_fingerprint <<<"$desired") ]]; then
    printf '%s\n' "$desired" | "${gh_cmd[@]}" api \
        --method PUT "repos/$repo/rulesets/$ruleset_id" --input - >/dev/null
fi

updated=$(read_ruleset)
updated_required_count=$(required_status_rule_count <<<"$updated")
if [[ $(policy_fingerprint <<<"$updated") != $(policy_fingerprint <<<"$desired") ]] ||
   [[ $updated_required_count != 0 ]]; then
    printf 'configure-merge-gate-ruleset: GitHub accepted reconciliation but verification failed\n' >&2
    exit 1
fi

printf 'APPLIED: ruleset %s has no required status checks; all unrelated policy is unchanged.\n' \
    "$ruleset_id"
