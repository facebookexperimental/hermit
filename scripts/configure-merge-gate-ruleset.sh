#!/usr/bin/env bash
# Reconcile Hermit's main-branch gate with GitHub's pinned required-workflow
# primitive. A required status context named `merge-gate` is insufficient:
# workflow_dispatch can emit that context from an old PR branch using old YAML.

set -euo pipefail

readonly DEFAULT_REPO="rrnewton/hermit"
readonly DEFAULT_RULESET_NAME="main check gating (admin-bypassable)"
readonly GATE_PATH=".github/workflows/merge-gate.yml"
readonly GATE_REF="refs/heads/main"

repo=$DEFAULT_REPO
ruleset_name=$DEFAULT_RULESET_NAME
mode=check

usage() {
    cat <<'EOF'
Usage: scripts/configure-merge-gate-ruleset.sh [--check|--apply] [options]

Replace the spoofable `merge-gate` required-status context with a required
workflow pinned to `.github/workflows/merge-gate.yml@refs/heads/main`.

Options:
  --check          Verify the pinned workflow rule without changing GitHub
                   (default).
  --apply          Reconcile the named ruleset, then verify it.
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

for command in gh jq; do
    if ! command -v "$command" >/dev/null 2>&1; then
        printf 'configure-merge-gate-ruleset: required command not found: %s\n' "$command" >&2
        exit 2
    fi
done

gh_cmd=(gh)
if command -v with-proxy >/dev/null 2>&1; then
    gh_cmd=(with-proxy gh)
fi

repo_id=$("${gh_cmd[@]}" api "repos/$repo" --jq .id)
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

is_reconciled() {
    jq -e \
        --arg path "$GATE_PATH" \
        --arg ref "$GATE_REF" \
        --argjson repo_id "$repo_id" '
          ([.rules[]
            | select(.type == "workflows")
            | .parameters.workflows[]?
            | select(.path == $path and .ref == $ref and .repository_id == $repo_id)
           ] | length) == 1
          and
          ([.rules[]
            | select(.type == "required_status_checks")
            | .parameters.required_status_checks[]?
            | select(.context == "merge-gate")
           ] | length) == 0
        ' >/dev/null
}

current=$(read_ruleset)
if is_reconciled <<<"$current"; then
    printf 'PASS: ruleset %s requires %s@%s from repository %s (%s).\n' \
        "$ruleset_id" "$GATE_PATH" "$GATE_REF" "$repo" "$repo_id"
    exit 0
fi

if [[ $mode == check ]]; then
    printf 'FAIL: ruleset %s does not require %s@%s, or still trusts the bare merge-gate status context.\n' \
        "$ruleset_id" "$GATE_PATH" "$GATE_REF" >&2
    exit 1
fi

# Do not silently discard a second required context. This migration knows how
# to replace only the legacy `merge-gate` context; anything else needs a human
# policy decision.
unexpected_contexts=$(jq -r '
  [.rules[]
   | select(.type == "required_status_checks")
   | .parameters.required_status_checks[]?
   | select(.context != "merge-gate")
   | .context] | unique | join(",")
' <<<"$current")
if [[ -n $unexpected_contexts ]]; then
    printf 'configure-merge-gate-ruleset: refusing to drop unexpected required contexts: %s\n' \
        "$unexpected_contexts" >&2
    exit 1
fi

desired=$(jq \
    --arg path "$GATE_PATH" \
    --arg ref "$GATE_REF" \
    --argjson repo_id "$repo_id" '
      {
        name,
        target,
        enforcement,
        conditions,
        rules: (
          [.rules[] | select(.type != "required_status_checks" and .type != "workflows")]
          + [{
              type: "workflows",
              parameters: {
                workflows: [{path: $path, ref: $ref, repository_id: $repo_id}]
              }
            }]
        ),
        bypass_actors
      }
    ' <<<"$current")

printf '%s\n' "$desired" | "${gh_cmd[@]}" api \
    --method PUT "repos/$repo/rulesets/$ruleset_id" --input - >/dev/null

updated=$(read_ruleset)
if ! is_reconciled <<<"$updated"; then
    printf 'configure-merge-gate-ruleset: GitHub accepted the update but verification failed\n' >&2
    exit 1
fi

printf 'APPLIED: ruleset %s now requires %s@%s from repository %s (%s).\n' \
    "$ruleset_id" "$GATE_PATH" "$GATE_REF" "$repo" "$repo_id"
