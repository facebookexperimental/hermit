#!/usr/bin/env bash
# Migrate Hermit's personal-repository ruleset to a versioned Merge Gate status
# context and bind that context to one server-side expected workflow blob.
#
# GitHub's stronger required-workflow rule is organization/enterprise-only.
# For this user-owned repository, a versioned context prevents an unmodified old
# branch from satisfying a tightened gate. MERGE_GATE_V2_BLOB catches accidental
# v2 drift that retains the guard; it is not a trusted-workflow signature.

set -euo pipefail

readonly DEFAULT_REPO="rrnewton/hermit"
readonly DEFAULT_RULESET_NAME="main check gating (admin-bypassable)"
readonly GATE_PATH=".github/workflows/merge-gate.yml"
readonly REQUIRED_CONTEXT="merge-gate-v2"
readonly LEGACY_CONTEXT="merge-gate"
readonly EXPECTED_BLOB_VARIABLE="MERGE_GATE_V2_BLOB"
readonly LEGACY_SHIM_VARIABLE="MERGE_GATE_LEGACY_CONTEXT"
readonly GITHUB_ACTIONS_INTEGRATION_ID=15368

repo=$DEFAULT_REPO
ruleset_name=$DEFAULT_RULESET_NAME
mode=check
prepare_ref=""

usage() {
    cat <<'EOF'
Usage: scripts/configure-merge-gate-ruleset.sh MODE [options]

Modes:
  --check          Verify the live v2 context, main-workflow blob, and disabled
                   transition shim without changing GitHub (default).
  --prepare REF    Before landing v2, bind its branch workflow blob and enable
                   the temporary legacy context shim. Does not change ruleset.
  --apply          After v2 lands on main, bind main's blob, replace the legacy
                   required context with merge-gate-v2, and disable the shim.

Options:
  --repo R         Repository (default: rrnewton/hermit).
  --ruleset-name N Ruleset to reconcile.
  -h, --help       Show this help.
EOF
}

while (($# > 0)); do
    case "$1" in
        --check) mode=check; shift ;;
        --prepare) mode=prepare; prepare_ref=${2:?--prepare requires a ref}; shift 2 ;;
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

read_variable() {
    "${gh_cmd[@]}" variable get "$1" --repo "$repo" --json value -q .value 2>/dev/null || true
}

set_variable() {
    "${gh_cmd[@]}" variable set "$1" --repo "$repo" --body "$2" >/dev/null
}

gate_blob() {
    "${gh_cmd[@]}" api --method GET "repos/$repo/contents/$GATE_PATH" \
        -f ref="$1" --jq .sha
}

gate_source() {
    "${gh_cmd[@]}" api --method GET "repos/$repo/contents/$GATE_PATH" \
        -f ref="$1" --jq .content | tr -d '\n' | base64 -d
}

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

required_context_count() {
    local context=$1
    jq --arg context "$context" '[.rules[]
      | select(.type == "required_status_checks")
      | .parameters.required_status_checks[]?
      | select(.context == $context)] | length'
}

required_context_integration() {
    local context=$1
    jq -r --arg context "$context" '[.rules[]
      | select(.type == "required_status_checks")
      | .parameters.required_status_checks[]?
      | select(.context == $context)
      | .integration_id] | if length == 1 then .[0] else empty end'
}

normalized_policy() {
    jq -S '{name, target, enforcement, conditions, rules, bypass_actors}'
}

if [[ $mode == prepare ]]; then
    blob=$(gate_blob "$prepare_ref")
    source=$(gate_source "$prepare_ref")
    if ! grep -Fq "name: $REQUIRED_CONTEXT" <<<"$source" ||
       ! grep -Fq "$EXPECTED_BLOB_VARIABLE" <<<"$source"; then
        printf 'configure-merge-gate-ruleset: %s does not define the bound %s context\n' \
            "$prepare_ref" "$REQUIRED_CONTEXT" >&2
        exit 1
    fi
    set_variable "$EXPECTED_BLOB_VARIABLE" "$blob"
    set_variable "$LEGACY_SHIM_VARIABLE" true
    if [[ $(read_variable "$EXPECTED_BLOB_VARIABLE") != "$blob" ]] ||
       [[ $(read_variable "$LEGACY_SHIM_VARIABLE") != true ]]; then
        printf 'configure-merge-gate-ruleset: transition variable verification failed\n' >&2
        exit 1
    fi
    printf 'PREPARED: %s=%s for %s; legacy context shim enabled.\n' \
        "$EXPECTED_BLOB_VARIABLE" "$blob" "$prepare_ref"
    exit 0
fi

current=$(read_ruleset)
main_blob=$(gate_blob refs/heads/main)
expected_blob=$(read_variable "$EXPECTED_BLOB_VARIABLE")
legacy_shim=$(read_variable "$LEGACY_SHIM_VARIABLE")
v2_count=$(required_context_count "$REQUIRED_CONTEXT" <<<"$current")
legacy_count=$(required_context_count "$LEGACY_CONTEXT" <<<"$current")
v2_integration=$(required_context_integration "$REQUIRED_CONTEXT" <<<"$current")
legacy_integration=$(required_context_integration "$LEGACY_CONTEXT" <<<"$current")

if [[ $mode == check ]]; then
    failed=0
    if [[ $v2_count != 1 || $legacy_count != 0 ||
          $v2_integration != "$GITHUB_ACTIONS_INTEGRATION_ID" ]]; then
        printf 'FAIL: ruleset %s has v2 count/integration %s/%s and %s legacy contexts.\n' \
            "$ruleset_id" "$v2_count" "${v2_integration:-unset}" "$legacy_count" >&2
        failed=1
    fi
    if [[ $expected_blob != "$main_blob" ]]; then
        printf 'FAIL: %s=%s, main workflow blob=%s.\n' \
            "$EXPECTED_BLOB_VARIABLE" "${expected_blob:-unset}" "$main_blob" >&2
        failed=1
    fi
    if [[ $legacy_shim != false ]]; then
        printf 'FAIL: %s=%s, expected false.\n' \
            "$LEGACY_SHIM_VARIABLE" "${legacy_shim:-unset}" >&2
        failed=1
    fi
    if ((failed != 0)); then
        exit 1
    fi
    printf 'PASS: ruleset %s requires %s; main blob %s is bound; legacy shim is disabled.\n' \
        "$ruleset_id" "$REQUIRED_CONTEXT" "$main_blob"
    exit 0
fi

source=$(gate_source refs/heads/main)
if ! grep -Fq "name: $REQUIRED_CONTEXT" <<<"$source" ||
   ! grep -Fq "$EXPECTED_BLOB_VARIABLE" <<<"$source"; then
    printf 'configure-merge-gate-ruleset: main does not yet define the bound %s context\n' \
        "$REQUIRED_CONTEXT" >&2
    exit 1
fi
if ! { [[ $legacy_count == 1 && $v2_count == 0 &&
          $legacy_integration == "$GITHUB_ACTIONS_INTEGRATION_ID" ]] ||
       [[ $legacy_count == 0 && $v2_count == 1 &&
          $v2_integration == "$GITHUB_ACTIONS_INTEGRATION_ID" ]]; }; then
    printf 'configure-merge-gate-ruleset: expected exactly one %s or one %s context from Actions app %s\n' \
        "$LEGACY_CONTEXT" "$REQUIRED_CONTEXT" "$GITHUB_ACTIONS_INTEGRATION_ID" >&2
    exit 1
fi

# Bind main before switching the ruleset so the new context can never start in
# an unbound state. Keep every unrelated required context and rule unchanged.
set_variable "$EXPECTED_BLOB_VARIABLE" "$main_blob"
desired=$(jq \
    --arg old "$LEGACY_CONTEXT" \
    --arg new "$REQUIRED_CONTEXT" '
      {
        name,
        target,
        enforcement,
        conditions,
        rules: [.rules[] |
          if .type == "required_status_checks" then
            .parameters.required_status_checks |= map(
              if .context == $old then .context = $new else . end
            )
          else . end],
        bypass_actors
      }
    ' <<<"$current")

# The ruleset API updates the full object. Re-read immediately before PUT and
# abort if another writer changed any policy field after our snapshot.
latest=$(read_ruleset)
if [[ $(normalized_policy <<<"$latest") != $(normalized_policy <<<"$current") ]]; then
    printf 'configure-merge-gate-ruleset: ruleset changed concurrently; refusing stale full-object PUT\n' >&2
    exit 1
fi

printf '%s\n' "$desired" | "${gh_cmd[@]}" api \
    --method PUT "repos/$repo/rulesets/$ruleset_id" --input - >/dev/null
set_variable "$LEGACY_SHIM_VARIABLE" false

# Re-read every server-side input rather than trusting the PUT response.
updated=$(read_ruleset)
updated_v2_count=$(required_context_count "$REQUIRED_CONTEXT" <<<"$updated")
updated_legacy_count=$(required_context_count "$LEGACY_CONTEXT" <<<"$updated")
updated_v2_integration=$(required_context_integration "$REQUIRED_CONTEXT" <<<"$updated")
if [[ $(normalized_policy <<<"$updated") != $(normalized_policy <<<"$desired") ]] ||
   [[ $updated_v2_count != 1 || $updated_legacy_count != 0 ]] ||
   [[ $updated_v2_integration != "$GITHUB_ACTIONS_INTEGRATION_ID" ]] ||
   [[ $(read_variable "$EXPECTED_BLOB_VARIABLE") != "$main_blob" ]] ||
   [[ $(read_variable "$LEGACY_SHIM_VARIABLE") != false ]]; then
    printf 'configure-merge-gate-ruleset: GitHub accepted migration but verification failed\n' >&2
    exit 1
fi

printf 'APPLIED: ruleset %s now requires %s; main blob %s is bound; legacy shim disabled.\n' \
    "$ruleset_id" "$REQUIRED_CONTEXT" "$main_blob"
