#!/usr/bin/env bash
# Exercise the real two-ruleset reconciler against an inert fake GitHub
# transport. The fixture can neither change repository settings nor publish a
# check.
set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
configure="$root/scripts/configure-merge-gate-ruleset.sh"
tmp=$(mktemp -d)
trap 'rm -rf -- "$tmp"' EXIT

mkdir -p "$tmp/bin" "$tmp/state"

write_drifted_policy() {
    cat >"$tmp/state/gate.json" <<'JSON'
{
  "id": 42,
  "name": "main check gating (admin-bypassable)",
  "target": "branch",
  "enforcement": "active",
  "conditions": {"ref_name": {"exclude": [], "include": ["~DEFAULT_BRANCH"]}},
  "rules": [
    {"type": "pull_request", "parameters": {"required_approving_review_count": 0}},
    {"type": "required_status_checks", "parameters": {"required_status_checks": [
      {"context": "merge-gate-v4", "integration_id": 15368}
    ]}}
  ],
  "bypass_actors": [
    {"actor_id": 5, "actor_type": "RepositoryRole", "bypass_mode": "always"}
  ]
}
JSON
    cat >"$tmp/state/history.json" <<'JSON'
{
  "id": 43,
  "name": "main history protection",
  "target": "branch",
  "enforcement": "evaluate",
  "conditions": {"ref_name": {"exclude": [], "include": ["~DEFAULT_BRANCH"]}},
  "rules": [
    {"type": "deletion"},
    {"type": "non_fast_forward"},
    {"type": "required_linear_history"}
  ],
  "bypass_actors": [
    {"actor_id": 5, "actor_type": "RepositoryRole", "bypass_mode": "always"}
  ]
}
JSON
    printf '0\n' >"$tmp/state/read-count"
    printf '0\n' >"$tmp/state/put-count"
}

cat >"$tmp/bin/with-proxy" <<'STUB'
#!/usr/bin/env bash
exec "$@"
STUB

cat >"$tmp/bin/gh" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
state=${FAKE_GH_STATE:?}

if [[ ${1:-} == api && ${2:-} == --paginate ]]; then
    jq -s '[.[] | {id, name}]' "$state/gate.json" "$state/history.json"
    exit 0
fi

if [[ ${1:-} == api && ${2:-} == repos/rrnewton/hermit/rulesets/42 ]]; then
    file="$state/gate.json"
elif [[ ${1:-} == api && ${2:-} == repos/rrnewton/hermit/rulesets/43 ]]; then
    file="$state/history.json"
else
    file=""
fi
if [[ -n $file ]]; then
    reads=$(<"$state/read-count")
    reads=$((reads + 1))
    printf '%s\n' "$reads" >"$state/read-count"
    if [[ ${FAKE_MUTATE_ON_THIRD_READ:-0} == 1 && $reads == 3 ]]; then
        jq '.enforcement = "disabled"' "$file" >"$state/ruleset.next"
        mv "$state/ruleset.next" "$file"
    fi
    cat "$file"
    exit 0
fi

if [[ ${1:-} == api && ${2:-} == --method && ${3:-} == PUT ]]; then
    case ${4:-} in
        repos/rrnewton/hermit/rulesets/42) file="$state/gate.json" ;;
        repos/rrnewton/hermit/rulesets/43) file="$state/history.json" ;;
        *) printf 'unsupported fake PUT target: %s\n' "${4:-unset}" >&2; exit 2 ;;
    esac
    jq ". + {id: $(jq .id "$file")}" >"$state/ruleset.next"
    mv "$state/ruleset.next" "$file"
    puts=$(<"$state/put-count")
    printf '%s\n' "$((puts + 1))" >"$state/put-count"
    exit 0
fi

printf 'unsupported fake gh invocation: %q ' "$@" >&2
printf '\n' >&2
exit 2
STUB
chmod +x "$tmp/bin/with-proxy" "$tmp/bin/gh"

run_configure() {
    PATH="$tmp/bin:/usr/bin:/bin" FAKE_GH_STATE="$tmp/state" \
        "$configure" "$@"
}

negative=0
positive=0
write_drifted_policy

if run_configure --check >"$tmp/check-drifted.out" 2>&1; then
    echo "FAIL: extra landing rules and bypassable history protection were accepted" >&2
    exit 1
elif grep -Fq 'check-gating ruleset' "$tmp/check-drifted.out" &&
     grep -Fq 'history ruleset' "$tmp/check-drifted.out"; then
    negative=$((negative + 1))
else
    cat "$tmp/check-drifted.out" >&2
    echo "FAIL: refusal did not identify both policy violations" >&2
    exit 1
fi

write_drifted_policy
if FAKE_MUTATE_ON_THIRD_READ=1 run_configure --apply >"$tmp/concurrent.out" 2>&1; then
    echo "FAIL: stale two-ruleset PUT was accepted" >&2
    exit 1
elif grep -Fq 'changed concurrently' "$tmp/concurrent.out" &&
     [[ $(<"$tmp/state/put-count") == 0 ]]; then
    negative=$((negative + 1))
else
    cat "$tmp/concurrent.out" >&2
    echo "FAIL: concurrent change was not refused before PUT" >&2
    exit 1
fi

write_drifted_policy
run_configure --apply >"$tmp/apply.out"
[[ $(jq '.rules == []' "$tmp/state/gate.json") == true ]]
[[ $(jq '[.rules[].type] | sort == ["deletion","non_fast_forward"]' \
    "$tmp/state/history.json") == true ]]
[[ $(jq '.enforcement == "active" and (.bypass_actors == [])' \
    "$tmp/state/history.json") == true ]]
[[ $(<"$tmp/state/put-count") == 2 ]]
run_configure --check >"$tmp/check-correct.out"
positive=$((positive + 1))

# Reconciliation is idempotent and must not rewrite an already-correct policy.
printf '0\n' >"$tmp/state/read-count"
run_configure --apply >"$tmp/apply-idempotent.out"
[[ $(<"$tmp/state/put-count") == 2 ]]
positive=$((positive + 1))

printf 'NEGATIVE refusals: %d/2   POSITIVE acceptances: %d/2\n' \
    "$negative" "$positive"
[[ $negative == 2 && $positive == 2 ]]
echo "PASS: only zero-bypass deletion and non-fast-forward rules remain active"
