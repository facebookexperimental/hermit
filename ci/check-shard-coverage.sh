#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# check-shard-coverage.sh — fail-closed correspondence guard for the parallel
# portable fan-out. Asserts that ci/portable-shards.json assigns EVERY step in
# the committed DAG's hosted-portable label selection to exactly one job, with no overlap
# and no unknown step names:
#
#   union(preflight, builds, test shards, e2e, final)
#     == { steps selected from ci/dag/validate.json by the hosted-portable label }
#
# The immutable E2E artifact and the LiteInst producer are deliberately assigned
# to one completed-build job after the debug and release producers. Keeping that
# internal edge preserves the constructed ordering while later test jobs fetch
# the resulting artifact instead of rerunning its command.
#
# Every hosted group must also preserve each constructed predecessor either in
# the same selected group or in an earlier job whose artifacts/results it uses.
# Exact set coverage alone cannot catch an edge that was reversed or dropped.
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

shards="ci/portable-shards.json"
workflow=".github/workflows/ci-portable.yml"
command -v jq >/dev/null 2>&1 || { echo "check-shard-coverage.sh: jq is required" >&2; exit 2; }
[[ -f $shards ]] || { echo "check-shard-coverage.sh: missing $shards" >&2; exit 2; }
[[ -f $workflow ]] || { echo "check-shard-coverage.sh: missing $workflow" >&2; exit 2; }

# Ask the same plan constructor the runner uses. The command is inert, may run
# inside validate, and emits its JSON as the first stdout line.
plan_out=$(mktemp)
trap 'rm -f "$plan_out"' EXIT
./scripts/validate.rs --hosted-portable-only --show-plan-json \
    --skip-inner-dirty-working-tree-and-rebase-freshness-checks >"$plan_out"
plan_json=$(sed -n '1p' "$plan_out")
jq -e '.profile == "hosted-portable" and .selection_mode == "label"' \
    <<<"$plan_json" >/dev/null || {
    echo "check-shard-coverage.sh: validate did not return the committed hosted-portable plan" >&2
    exit 2
}
mapfile -t expected < <(jq -r '.dags[].steps[].tag' <<<"$plan_json" | sort -u)
mapfile -t strict_compat_expansion < <(
    jq -r '
        .dags[].steps[].tag
        | select(. == "compatprep.fixtures" or . == "compatprep.fixtures_on_host" or startswith("compat."))
    ' <<<"$plan_json" | sort -u
)
if ((${#strict_compat_expansion[@]} == 0)); then
    echo "check-shard-coverage.sh: constructed plan has no direct strict compatibility nodes" >&2
    exit 2
fi

# Match validate's exact-name-first hosted selector resolution before checking
# either coverage or predecessor supply. The source shard map keeps its public
# selectors; unknown names and duplicate resolutions remain failures below.
shards_json=$(jq --argjson available "$(jq '[.dags[].steps[].tag]' <<<"$plan_json")" '
    def resolve:
        . as $tag | ($tag + "_on_host") as $hosted
        | if ($available | index($tag)) == null and ($available | index($hosted)) != null
          then $hosted else $tag end;
    (.preflight_nodes[], .check_nodes[], .build_debug_nodes[],
     .build_dbt_nodes[], .build_aux_nodes[], .strict_compat_nodes[],
     .e2e_nodes[], .final_nodes[], .debug_shards[].nodes[],
     .release_shards[].nodes[]) |= resolve
' "$shards")

# Every selection alias assigned by the shard map, across all job buckets.
mapfile -t assigned_aliases < <(
    jq -r '
        (.preflight_nodes // [])
      + (.check_nodes // [])
      + (.build_debug_nodes // [])
      + (.build_dbt_nodes // [])
      + (.build_aux_nodes // [])
      + (.strict_compat_nodes // [])
      + (.e2e_nodes // [])
      + (.final_nodes // [])
      + ([ (.debug_shards // [])[]   | .nodes[] ])
      + ([ (.release_shards // [])[] | .nodes[] ])
        | .[]
    ' <<<"$shards_json" | sort
)
strict_alias_count=$(printf '%s\n' "${assigned_aliases[@]}" |
    grep -Fxc 'test.strict_compat' || true)
if [[ $strict_alias_count -ne 1 ]]; then
    echo "check-shard-coverage.sh: FAIL — shard map assigns test.strict_compat $strict_alias_count times; expected exactly one stable alias" >&2
    exit 1
fi
mapfile -t assigned < <(
    {
        printf '%s\n' "${assigned_aliases[@]}" | grep -Fvx 'test.strict_compat'
        printf '%s\n' "${strict_compat_expansion[@]}"
    } | sort
)

# Duplicate assignment (a node in two buckets) is a defect.
dupes=$(printf '%s\n' "${assigned[@]}" | uniq -d || true)
if [[ -n $dupes ]]; then
    echo "check-shard-coverage.sh: FAIL — node(s) assigned to more than one job:" >&2
    printf '  %s\n' $dupes >&2
    exit 1
fi

assigned_unique=$(printf '%s\n' "${assigned[@]}" | sort -u)
expected_list=$(printf '%s\n' "${expected[@]}")

missing=$(comm -23 <(printf '%s\n' "$expected_list") <(printf '%s\n' "$assigned_unique") || true)
extra=$(comm -13 <(printf '%s\n' "$expected_list") <(printf '%s\n' "$assigned_unique") || true)

status=0
if [[ -n $missing ]]; then
    echo "check-shard-coverage.sh: FAIL — portable nodes NOT assigned to any job:" >&2
    printf '  %s\n' $missing >&2
    status=1
fi
if [[ -n $extra ]]; then
    echo "check-shard-coverage.sh: FAIL — shard map names steps absent from the committed hosted-portable plan:" >&2
    printf '  %s\n' $extra >&2
    status=1
fi

dependency_misses() {
    local selected_json=$1
    local supplied_json=$2
    local source_plan=${3:-$plan_json}
    jq -r --argjson selected "$selected_json" --argjson supplied "$supplied_json" '
    [
      .dags[].steps[]
      | select(.tag as $tag | $selected | index($tag))
      | .deps[]
      | select(. as $dependency | ($supplied | index($dependency)) == null)
    ]
    | unique[]
' <<<"$source_plan"
}

# Pin both directions of the dependency guard with a synthetic hosted group.
# The live plan below caught a real omission after build.workspace became a
# predecessor of a check assigned to the pre-build checks job. A guard that only
# happens to reject today's map can silently decay when its jq selection changes;
# this fixture requires the missing edge to be named and the supplied edge to
# clear without relying on any current node identity.
dependency_fixture='{"dags":[{"steps":[{"tag":"check.fixture","deps":["build.fixture"]}]}]}'
fixture_missing=$(dependency_misses '["check.fixture"]' '["check.fixture"]' "$dependency_fixture")
if [[ $fixture_missing != build.fixture ]]; then
    echo "check-shard-coverage.sh: FAIL — dependency guard did not name a planted missing predecessor" >&2
    status=1
fi
fixture_clear=$(dependency_misses \
    '["check.fixture"]' '["check.fixture","build.fixture"]' "$dependency_fixture")
if [[ -n $fixture_clear ]]; then
    echo "check-shard-coverage.sh: FAIL — dependency guard rejected a planted supplied predecessor" >&2
    status=1
fi

workflow_step_body() {
    local step_name=$1 workflow_text=$2
    awk -v marker="      - name: $step_name" '
        $0 == marker { in_step = 1; next }
        in_step && /^      - name:/ { exit }
        in_step { print }
    ' <<<"$workflow_text"
}

debug_artifact_contract() {
    local workflow_text=$1 pack_step unpack_step
    local archive_member='            target/debug/verification-report \'
    pack_step=$(workflow_step_body "Pack debug prebuilt tree" "$workflow_text")
    unpack_step=$(workflow_step_body "Unpack debug tree" "$workflow_text")
    grep -Fqx '          test -x target/debug/verification-report' <<<"$pack_step" &&
        grep -Fqx "$archive_member" <<<"$pack_step" &&
        grep -Fqx '          test -x target/debug/verification-report' <<<"$unpack_step"
}

workflow_job_body() {
    local job=$1 workflow_text=$2
    awk -v marker="  $job:" '
        $0 == marker { in_job = 1; found = 1; next }
        in_job && /^  [A-Za-z0-9_-]+:$/ { exit }
        in_job { print }
        END { if (!found) exit 1 }
    ' <<<"$workflow_text"
}

workflow_job_needs() {
    local job=$1 workflow_text=$2 body
    body=$(workflow_job_body "$job" "$workflow_text") || return 1
    awk '
        /^    needs: \[/ {
            line = $0
            sub(/^    needs: \[/, "", line)
            sub(/\][[:space:]]*$/, "", line)
            count = split(line, values, /,[[:space:]]*/)
            for (i = 1; i <= count; i++) print values[i]
            found = 1
            next
        }
        /^    needs: [A-Za-z0-9_-]+[[:space:]]*$/ {
            line = $0
            sub(/^    needs: /, "", line)
            sub(/[[:space:]]*$/, "", line)
            print line
            found = 1
            next
        }
        /^    needs:[[:space:]]*$/ { in_needs = 1; found = 1; next }
        in_needs && /^      - [A-Za-z0-9_-]+[[:space:]]*$/ {
            line = $0
            sub(/^      - /, "", line)
            sub(/[[:space:]]*$/, "", line)
            print line
            next
        }
        in_needs { in_needs = 0 }
        END { if (!found) exit 1 }
    ' <<<"$body"
}

workflow_job_action_values() {
    local job=$1 direction=$2 field=$3 workflow_text=$4 body
    body=$(workflow_job_body "$job" "$workflow_text") || return 1
    awk -v wanted_action="actions/${direction}-artifact@" -v wanted_field="$field" '
        /^      - / { action = "" }
        index($0, "uses: " wanted_action) { action = wanted_action; next }
        action == wanted_action && $0 ~ ("^          " wanted_field ":[[:space:]]") {
            line = $0
            sub("^          " wanted_field ":[[:space:]]*", "", line)
            print line
            action = ""
        }
    ' <<<"$body"
}

workflow_global_env_value() {
    local name=$1 workflow_text=$2
    awk -v marker="  ${name}:" -v env_name="$name" '
        /^env:$/ { in_env = 1; next }
        in_env && /^[^ ]/ { exit }
        in_env && index($0, marker) == 1 {
            line = $0
            sub("^  " env_name ":[[:space:]]*", "", line)
            print line
            count += 1
        }
        END { if (count != 1) exit 1 }
    ' <<<"$workflow_text"
}

workflow_job_prepares_isolated_workdir() {
    local job=$1 workflow_text=$2 body
    body=$(workflow_job_body "$job" "$workflow_text") || return 1
    grep -Fqx '          sudo install -d -o "$(id -u)" -g "$(id -g)" /test' <<<"$body"
}

workflow_e2e_uses_pinned_result_root() {
    local workflow_text=$1 body
    body=$(workflow_job_body e2e "$workflow_text") || return 1
    grep -Fqx '      E2E_RESULT_ROOT: /results/${{ matrix.slug }}' <<<"$body" &&
        grep -Fqx '          sudo install -d -o "$(id -u)" -g "$(id -g)" /results' <<<"$body"
}

workflow_e2e_prepares_btrfs() {
    local workflow_text=$1 body
    body=$(workflow_job_body e2e "$workflow_text") || return 1
    grep -Eq '^          sudo apt-get install -y .* btrfs-progs( |$)' <<<"$body" &&
        grep -Fqx '      - name: Provide Btrfs sysfs state for system-utils' <<<"$body" &&
        grep -Fqx "        if: matrix.slug == 'system_utils'" <<<"$body" &&
        grep -Fqx '          sudo truncate -s 128M /tmp/hermit-ci-btrfs.img' <<<"$body" &&
        grep -Fqx '          sudo mkfs.btrfs -q -f /tmp/hermit-ci-btrfs.img' <<<"$body" &&
        grep -Fqx '          sudo install -d /mnt/hermit-ci-btrfs' <<<"$body" &&
        grep -Fqx '          sudo mount -o loop /tmp/hermit-ci-btrfs.img /mnt/hermit-ci-btrfs' <<<"$body" &&
        grep -Fqx "          compgen -G '/sys/fs/btrfs/*/commit_stats' >/dev/null" <<<"$body"
}

workflow_job_needs_exactly() {
    local job=$1 expected_csv=$2 workflow_text=$3 actual expected
    actual=$(workflow_job_needs "$job" "$workflow_text" | sort) || return 1
    expected=$(tr ',' '\n' <<<"$expected_csv" | sed '/^$/d' | sort)
    [[ $actual == "$expected" ]]
}

workflow_artifact_edge() {
    local producer=$1 upload_field=$2 upload_value=$3
    local consumer=$4 download_field=$5 download_value=$6 workflow_text=$7
    workflow_job_action_values "$producer" upload "$upload_field" "$workflow_text" |
        grep -Fqx -- "$upload_value" &&
        workflow_job_action_values "$consumer" download "$download_field" "$workflow_text" |
            grep -Fqx -- "$download_value"
}

workflow_wiring_contract() {
    local workflow_text=$1

    # Keep these exact. The dependency checks below treat predecessor groups as
    # supplied only because this job graph orders them and these artifact edges
    # move the build products across runner boundaries.
    [[ $(workflow_global_env_value HERMIT_E2E_EMPTY_WORKDIR "$workflow_text") == /test ]] &&
        workflow_job_prepares_isolated_workdir test-debug "$workflow_text" &&
        workflow_job_prepares_isolated_workdir strict-compat "$workflow_text" &&
        workflow_job_prepares_isolated_workdir test-release "$workflow_text" &&
        workflow_job_prepares_isolated_workdir e2e "$workflow_text" &&
        workflow_job_prepares_isolated_workdir sabre_non_gated_parity "$workflow_text" &&
        workflow_e2e_uses_pinned_result_root "$workflow_text" &&
        workflow_e2e_prepares_btrfs "$workflow_text" &&
        workflow_job_needs_exactly preflight 'select' "$workflow_text" &&
        workflow_job_needs_exactly checks 'select,preflight' "$workflow_text" &&
        workflow_job_needs_exactly build-debug 'select,preflight' "$workflow_text" &&
        workflow_job_needs_exactly build-release 'select,preflight' "$workflow_text" &&
        workflow_job_needs_exactly build-complete 'select,build-debug,build-release' "$workflow_text" &&
        workflow_job_needs_exactly test-debug 'select,build-debug,build-release,build-complete' "$workflow_text" &&
        workflow_job_needs_exactly strict-compat 'select,build-debug,build-complete,test-debug' "$workflow_text" &&
        workflow_job_needs_exactly test-release 'select,build-complete' "$workflow_text" &&
        workflow_job_needs_exactly e2e 'select,build-debug,build-complete' "$workflow_text" &&
        workflow_job_needs_exactly regular \
            'select,plan,preflight,checks,build-debug,build-release,build-complete,test-debug,strict-compat,test-release,e2e' \
            "$workflow_text" &&
        workflow_artifact_edge preflight name '${{ env.MANIFEST_PLAN_ARTIFACT }}' \
            build-debug name '${{ env.MANIFEST_PLAN_ARTIFACT }}' "$workflow_text" &&
        workflow_artifact_edge build-debug name '${{ env.DEBUG_ARTIFACT }}' \
            build-complete name '${{ env.DEBUG_ARTIFACT }}' "$workflow_text" &&
        workflow_artifact_edge build-release name '${{ env.RELEASE_DBT_ARTIFACT }}' \
            build-complete name '${{ env.RELEASE_DBT_ARTIFACT }}' "$workflow_text" &&
        workflow_artifact_edge build-debug name '${{ env.DEBUG_ARTIFACT }}' \
            test-debug name '${{ env.DEBUG_ARTIFACT }}' "$workflow_text" &&
        workflow_artifact_edge build-complete name '${{ env.RELEASE_ARTIFACT }}' \
            test-debug name '${{ env.RELEASE_ARTIFACT }}' "$workflow_text" &&
        workflow_artifact_edge build-debug name '${{ env.DEBUG_ARTIFACT }}' \
            strict-compat name '${{ env.DEBUG_ARTIFACT }}' "$workflow_text" &&
        workflow_artifact_edge build-complete name '${{ env.RELEASE_ARTIFACT }}' \
            strict-compat name '${{ env.RELEASE_ARTIFACT }}' "$workflow_text" &&
        workflow_artifact_edge build-complete name '${{ env.RELEASE_ARTIFACT }}' \
            test-release name '${{ env.RELEASE_ARTIFACT }}' "$workflow_text" &&
        workflow_artifact_edge build-debug name '${{ env.DEBUG_ARTIFACT }}' \
            e2e name '${{ env.DEBUG_ARTIFACT }}' "$workflow_text" &&
        workflow_artifact_edge build-complete name '${{ env.RELEASE_ARTIFACT }}' \
            e2e name '${{ env.RELEASE_ARTIFACT }}' "$workflow_text" &&
        workflow_artifact_edge e2e name \
            'parity-v1-${{ github.run_id }}-${{ github.run_attempt }}-portable-${{ matrix.slug }}' \
            regular pattern 'parity-v1-${{ github.run_id }}-${{ github.run_attempt }}-*' "$workflow_text"
}

# check.backend_parity_suites runs target/debug/verification-report after the
# debug tree crosses a job boundary. Guard all three parts of that contract:
# producer existence, archive membership, and executable consumer assertion.
# The mutation bracket proves the guard rejects the original omission instead
# of passing merely because the binary is mentioned somewhere in the workflow.
workflow_text=$(<"$workflow")
if ! debug_artifact_contract "$workflow_text"; then
    echo "check-shard-coverage.sh: FAIL — debug artifact must transport executable target/debug/verification-report" >&2
    status=1
fi
omitted_artifact=${workflow_text/$'            target/debug/verification-report \\\n'/}
if [[ $omitted_artifact == "$workflow_text" ]]; then
    echo "check-shard-coverage.sh: FAIL — artifact omission fixture did not remove verification-report" >&2
    status=1
elif debug_artifact_contract "$omitted_artifact"; then
    echo "check-shard-coverage.sh: FAIL — artifact guard accepted a planted missing verification-report member" >&2
    status=1
fi
if ! workflow_wiring_contract "$workflow_text"; then
    echo "check-shard-coverage.sh: FAIL — workflow job needs/artifact transfers do not match the constructed dependency supply contract" >&2
    status=1
fi

# Mutation brackets prove the workflow contract is reading the checked-in job
# graph and artifact actions rather than accepting the shard-map-derived sets by
# themselves. Remove one real needs edge and one real download independently;
# each broken workflow must be refused.
missing_need=${workflow_text/$'    needs: [select, build-complete]\n'/$'    needs: [select]\n'}
if [[ $missing_need == "$workflow_text" ]]; then
    echo "check-shard-coverage.sh: FAIL — needs-edge mutation did not change the workflow fixture" >&2
    status=1
elif workflow_wiring_contract "$missing_need"; then
    echo "check-shard-coverage.sh: FAIL — workflow guard accepted a planted missing needs edge" >&2
    status=1
fi
release_download=$'      - name: Download full release prebuilt tree\n        uses: actions/download-artifact@v4\n        with:\n          name: ${{ env.RELEASE_ARTIFACT }}'
missing_artifact=${workflow_text/"$release_download"/${release_download%$'\n'*}}
if [[ $missing_artifact == "$workflow_text" ]]; then
    echo "check-shard-coverage.sh: FAIL — artifact-edge mutation did not change the workflow fixture" >&2
    status=1
elif workflow_wiring_contract "$missing_artifact"; then
    echo "check-shard-coverage.sh: FAIL — workflow guard accepted a planted missing artifact download" >&2
    status=1
fi
missing_workdir_env=${workflow_text/$'  HERMIT_E2E_EMPTY_WORKDIR: /test\n'/}
if [[ $missing_workdir_env == "$workflow_text" ]]; then
    echo "check-shard-coverage.sh: FAIL — isolated-workdir mutation did not change the workflow fixture" >&2
    status=1
elif workflow_wiring_contract "$missing_workdir_env"; then
    echo "check-shard-coverage.sh: FAIL — workflow guard accepted a missing hosted isolated workdir" >&2
    status=1
fi
workdir_setup=$'          sudo install -d -o "$(id -u)" -g "$(id -g)" /test\n'
missing_workdir_setup=${workflow_text/"$workdir_setup"/}
if [[ $missing_workdir_setup == "$workflow_text" ]]; then
    echo "check-shard-coverage.sh: FAIL — isolated-workdir setup mutation did not change the workflow fixture" >&2
    status=1
elif workflow_wiring_contract "$missing_workdir_setup"; then
    echo "check-shard-coverage.sh: FAIL — workflow guard accepted a test job without the hosted isolated-workdir setup" >&2
    status=1
fi
result_root=$'      E2E_RESULT_ROOT: /results/${{ matrix.slug }}'
wrong_result_root=${workflow_text/"$result_root"/$'      E2E_RESULT_ROOT: ignored/e2e/${{ matrix.slug }}'}
if [[ $wrong_result_root == "$workflow_text" ]]; then
    echo "check-shard-coverage.sh: FAIL — result-root mutation did not change the workflow fixture" >&2
    status=1
elif workflow_wiring_contract "$wrong_result_root"; then
    echo "check-shard-coverage.sh: FAIL — workflow guard accepted a hosted result root outside /results" >&2
    status=1
fi
btrfs_setup_name="      - name: Provide Btrfs sysfs state for system-utils"
missing_btrfs_setup=${workflow_text/"$btrfs_setup_name"/}
if [[ $missing_btrfs_setup == "$workflow_text" ]]; then
    echo "check-shard-coverage.sh: FAIL — Btrfs setup mutation did not change the workflow fixture" >&2
    status=1
elif workflow_wiring_contract "$missing_btrfs_setup"; then
    echo "check-shard-coverage.sh: FAIL — workflow guard accepted missing Btrfs setup" >&2
    status=1
fi
btrfs_slug="        if: matrix.slug == 'system_utils'"
wrong_btrfs_slug=${workflow_text/"$btrfs_slug"/"        if: matrix.slug == 'applications'"}
if [[ $wrong_btrfs_slug == "$workflow_text" ]]; then
    echo "check-shard-coverage.sh: FAIL — Btrfs slug mutation did not change the workflow fixture" >&2
    status=1
elif workflow_wiring_contract "$wrong_btrfs_slug"; then
    echo "check-shard-coverage.sh: FAIL — workflow guard accepted Btrfs setup on the wrong E2E shard" >&2
    status=1
fi

check_dependencies() {
    local label=$1 selected_json=$2 supplied_json=$3 missing
    missing=$(dependency_misses "$selected_json" "$supplied_json")
    if [[ -n $missing ]]; then
        echo "check-shard-coverage.sh: FAIL — $label drops constructed predecessor(s) that no earlier job supplies:" >&2
        printf '  %s\n' $missing >&2
        status=1
    fi
}

preflight_json=$(jq -c '.preflight_nodes // []' <<<"$shards_json")
check_json=$(jq -c '.check_nodes // []' <<<"$shards_json")
build_debug_json=$(jq -c '.build_debug_nodes // []' <<<"$shards_json")
build_dbt_json=$(jq -c '.build_dbt_nodes // []' <<<"$shards_json")
build_aux_json=$(jq -c '.build_aux_nodes // []' <<<"$shards_json")
strict_compat_json=$(printf '%s\n' "${strict_compat_expansion[@]}" |
    jq -Rsc 'split("\n") | map(select(length > 0))')
through_preflight=$(jq -cn --argjson preflight "$preflight_json" '$preflight')
through_checks=$(jq -cn --argjson preflight "$preflight_json" --argjson checks "$check_json" '$preflight + $checks')
through_debug=$(jq -cn --argjson preflight "$preflight_json" --argjson debug "$build_debug_json" '$preflight + $debug')
through_release=$(jq -cn --argjson preflight "$preflight_json" --argjson release "$build_dbt_json" '$preflight + $release')
through_builds=$(jq -cn \
    --argjson preflight "$preflight_json" \
    --argjson debug "$build_debug_json" \
    --argjson release "$build_dbt_json" \
    --argjson aux "$build_aux_json" \
    '$preflight + $debug + $release + $aux')

check_dependencies "preflight" "$preflight_json" "$through_preflight"
check_dependencies "check job" "$check_json" "$through_checks"
check_dependencies "debug build job" "$build_debug_json" "$through_debug"
check_dependencies "release build job" "$build_dbt_json" "$through_release"
check_dependencies "completed build job" "$build_aux_json" "$through_builds"

debug_test_json=$(jq -c '[.debug_shards[].nodes[]]' <<<"$shards_json")
strict_compat_supplied=$(jq -cn \
    --argjson prior "$through_builds" \
    --argjson tests "$debug_test_json" \
    --argjson selected "$strict_compat_json" \
    '$prior + $tests + $selected')
check_dependencies "strict compatibility job" "$strict_compat_json" "$strict_compat_supplied"

while IFS= read -r shard; do
    slug=$(jq -r '.slug' <<<"$shard")
    nodes=$(jq -c '.nodes' <<<"$shard")
    supplied=$(jq -cn --argjson prior "$through_builds" --argjson selected "$nodes" '$prior + $selected')
    check_dependencies "debug shard $slug" "$nodes" "$supplied"
done < <(jq -c '.debug_shards[]' <<<"$shards_json")

while IFS= read -r shard; do
    slug=$(jq -r '.slug' <<<"$shard")
    nodes=$(jq -c '.nodes' <<<"$shard")
    supplied=$(jq -cn --argjson prior "$through_builds" --argjson selected "$nodes" '$prior + $selected')
    check_dependencies "release shard $slug" "$nodes" "$supplied"
done < <(jq -c '.release_shards[]' <<<"$shards_json")

while IFS= read -r node; do
    selected=$(jq -cn --arg node "$node" '[$node]')
    supplied=$(jq -cn --argjson prior "$through_builds" --argjson selected "$selected" '$prior + $selected')
    check_dependencies "E2E job $node" "$selected" "$supplied"
done < <(jq -r '.e2e_nodes[]' <<<"$shards_json")

final_json=$(jq -c '.final_nodes // []' <<<"$shards_json")
all_supplied_json=$(printf '%s\n' "${assigned[@]}" | jq -Rsc 'split("\n") | map(select(length > 0))')
check_dependencies "final job" "$final_json" "$all_supplied_json"

if ! jq -e '
    (.build_aux_nodes // []) as $completed_build
    | ($completed_build | index("build.e2e_artifact") != null)
      and ($completed_build | index("build.liteinst_runtime_release") != null)
' <<<"$shards_json" >/dev/null; then
    echo "check-shard-coverage.sh: FAIL — completed build job must preserve build.e2e_artifact -> build.liteinst_runtime_release" >&2
    status=1
fi

if ((status == 0)); then
    n=$(printf '%s\n' "$assigned_unique" | grep -c . || true)
    cell_count=$(jq '[.cells[] | select(.lane == "portable")] | length' ci/expected-e2e-plan.json)
    ((cell_count > 0)) || {
        echo "check-shard-coverage.sh: FAIL — committed hosted-portable cell population is empty" >&2
        exit 1
    }
    if [[ -n ${GITHUB_OUTPUT:-} ]]; then
        printf 'constructed_step_count=%s\n' "$n" >>"$GITHUB_OUTPUT"
        printf 'selected_cell_count=%s\n' "$cell_count" >>"$GITHUB_OUTPUT"
    fi
    echo "check-shard-coverage.sh: OK — $n committed hosted-portable steps each assigned to exactly one hosted job; $cell_count selected portable cells."
fi
exit "$status"
