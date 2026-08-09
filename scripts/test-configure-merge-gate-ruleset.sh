#!/usr/bin/env bash
# Exercise the real ruleset reconciler against an inert fake GitHub transport.
# The fixture can neither change repository settings nor publish a check.
set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
configure="$root/scripts/configure-merge-gate-ruleset.sh"
tmp=$(mktemp -d)
trap 'rm -rf -- "$tmp"' EXIT

mkdir -p "$tmp/bin" "$tmp/state"

write_gated_ruleset() {
    cat >"$tmp/state/ruleset.json" <<'JSON'
{
  "id": 42,
  "name": "main check gating (admin-bypassable)",
  "target": "branch",
  "enforcement": "active",
  "conditions": {"ref_name": {"exclude": [], "include": ["~DEFAULT_BRANCH"]}},
  "rules": [
    {
      "type": "pull_request",
      "parameters": {
        "required_approving_review_count": 0,
        "allowed_merge_methods": ["squash", "rebase"]
      }
    },
    {
      "type": "required_status_checks",
      "parameters": {
        "strict_required_status_checks_policy": false,
        "required_status_checks": [
          {"context": "merge-gate-v4", "integration_id": 15368}
        ]
      }
    }
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
    jq '[{id: .id, name: .name}]' "$state/ruleset.json"
    exit 0
fi
if [[ ${1:-} == api && ${2:-} == repos/rrnewton/hermit/rulesets/42 ]]; then
    reads=$(<"$state/read-count")
    reads=$((reads + 1))
    printf '%s\n' "$reads" >"$state/read-count"
    if [[ ${FAKE_MUTATE_ON_SECOND_READ:-0} == 1 && $reads == 2 ]]; then
        jq '.enforcement = "evaluate"' "$state/ruleset.json" >"$state/ruleset.next"
        mv "$state/ruleset.next" "$state/ruleset.json"
    fi
    cat "$state/ruleset.json"
    exit 0
fi
if [[ ${1:-} == api && ${2:-} == --method && ${3:-} == PUT ]]; then
    jq '. + {id: 42}' >"$state/ruleset.next"
    mv "$state/ruleset.next" "$state/ruleset.json"
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
write_gated_ruleset

if run_configure --check >"$tmp/check-gated.out" 2>&1; then
    echo "FAIL: pull-request and hosted-check landing rules were accepted" >&2
    exit 1
elif grep -Fq 'types=pull_request,required_status_checks' "$tmp/check-gated.out"; then
    negative=$((negative + 1))
else
    cat "$tmp/check-gated.out" >&2
    echo "FAIL: refusal did not name both prohibited landing rules" >&2
    exit 1
fi

write_gated_ruleset
if FAKE_MUTATE_ON_SECOND_READ=1 run_configure --apply >"$tmp/concurrent.out" 2>&1; then
    echo "FAIL: stale full-object PUT was accepted" >&2
    exit 1
elif grep -Fq 'changed concurrently' "$tmp/concurrent.out" &&
     [[ $(<"$tmp/state/put-count") == 0 ]]; then
    negative=$((negative + 1))
else
    cat "$tmp/concurrent.out" >&2
    echo "FAIL: concurrent change was not refused before PUT" >&2
    exit 1
fi

write_gated_ruleset
run_configure --apply >"$tmp/apply.out"
[[ $(jq '.rules == []' "$tmp/state/ruleset.json") == true ]]
[[ $(jq '.bypass_actors == [{"actor_id":5,"actor_type":"RepositoryRole","bypass_mode":"always"}]' \
    "$tmp/state/ruleset.json") == true ]]
[[ $(<"$tmp/state/put-count") == 1 ]]
run_configure --check >"$tmp/check-advisory.out"
positive=$((positive + 1))

# Reconciliation is idempotent and must not rewrite an already-correct policy.
printf '0\n' >"$tmp/state/read-count"
run_configure --apply >"$tmp/apply-idempotent.out"
[[ $(<"$tmp/state/put-count") == 1 ]]
positive=$((positive + 1))

printf 'NEGATIVE refusals: %d/2   POSITIVE acceptances: %d/2\n' \
    "$negative" "$positive"
[[ $negative == 2 && $positive == 2 ]]
echo "PASS: the check-gating ruleset is inert and its envelope is preserved"
