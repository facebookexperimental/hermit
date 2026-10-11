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
# The immutable E2E artifact publisher and the inert Cargo-mode Buck branch it
# depends on are deliberately assigned to one completed-build job after the
# debug producer, the only job that compiles Hermit. There has been no
# release-profile producer since d44bbbb79ac
# (https://github.com/rrnewton/hermit/issues/3458). Keeping that internal edge
# preserves the constructed ordering while later test jobs fetch the resulting
# artifact instead of rerunning its command.
#
# Every hosted group must also preserve each constructed predecessor either in
# the same selected group or in an earlier job whose artifacts/results it uses.
# Exact set coverage alone cannot catch an edge that was reversed or dropped.
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

shards="ci/portable-shards.json"
# The hosted-portable E2E population: portable cells minus the backends that
# HOSTED_PORTABLE_EXCLUDED_BACKENDS (ci/manifest-plan/src/validation_dag.rs)
# omits because GitHub-hosted runners have no PMU for KVM guests and no CPUID
# faulting for the in-guest LiteInst runtime (liteinst, in-guest-trap). The workflow
# reducer must apply this exact filter; a generator test keeps it in sync.
hosted_e2e_cell_filter='select(.lane == "portable" and .backend != "kvm" and .backend != "liteinst" and .backend != "in-guest-trap")'
workflow=".github/workflows/ci-portable.yml"
hosted_runner="ci/run-hosted-node.sh"
command -v jq >/dev/null 2>&1 || { echo "check-shard-coverage.sh: jq is required" >&2; exit 2; }
[[ -f $shards ]] || { echo "check-shard-coverage.sh: missing $shards" >&2; exit 2; }
[[ -f $workflow ]] || { echo "check-shard-coverage.sh: missing $workflow" >&2; exit 2; }
[[ -f $hosted_runner ]] || { echo "check-shard-coverage.sh: missing $hosted_runner" >&2; exit 2; }

# Every hosted test job enters a network namespace. The outer validation driver
# must therefore resolve to the artifact built before fan-out, never to Cargo.
hosted_runner_text=$(<"$hosted_runner")
for required in \
    'export PATH="$ROOT_DIR/ci/rust-script-bin:$PATH"' \
    'export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$ROOT_DIR/target/ci/rust-scripts"' \
    'export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1' \
    'mkdir -p "$ROOT_DIR/target/tmp"' \
    '"$ROOT_DIR/ci/delegate-hosted-cgroup.sh" "$$"' \
    'exec unshare --user --map-root-user --uts --net --mount' \
    'ip link set lo up' \
    'mount -t tmpfs -o mode=1777 tmpfs /tmp' \
    'export TMPDIR=/tmp'
do
    if ! grep -Fq "$required" <<<"$hosted_runner_text"; then
        echo "check-shard-coverage.sh: FAIL — hosted namespace wrapper omitted required bootstrap: $required" >&2
        exit 1
    fi
done
if grep -Eq -- '--pid|--fork' <<<"$hosted_runner_text"; then
    echo "check-shard-coverage.sh: FAIL — hosted wrapper reintroduced an outer PID namespace" >&2
    exit 1
fi

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

# Every node assigned by the shard map, across all job buckets.
mapfile -t assigned < <(
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

# A one-node hosted validation that selects an empty manifest bucket is
# correctly refused as a zero-test pass. Keep such constructed nodes assigned
# exactly once, but require them to share a nonempty shard instead of becoming
# standalone E2E matrix jobs.
while IFS= read -r node; do
    category=${node#e2e.manifest_}
    category=${category%_on_host}
    category=${category//_/-}
    cells=$(jq --arg category "$category" '[.cells[] | select(.category == $category)] | length' \
        ci/expected-e2e-plan.json)
    if ((cells == 0)); then
        echo "check-shard-coverage.sh: FAIL — standalone E2E node $node selects zero committed cells; co-schedule it with a nonempty shard" >&2
        status=1
    fi
done < <(jq -r '.e2e_nodes[]' <<<"$shards_json")

# The converse of the rule above. Only e2e jobs, and the strict-compat job for
# its own buckets (workflow_strict_compat_parity_contract), pack the parity-v1
# transport archive that the reducer in the `regular` job reads, and the reducer compares
# its results against every portable cell in ci/expected-e2e-plan.json. A
# manifest node that selects portable cells but is co-scheduled in a test shard
# still runs and passes there, yet its results never reach the reducer, which
# then reports those cells as missing. Run 36485831200 failed that way after
# https://github.com/rrnewton/hermit/pull/3213 gave shared-futex-c and util-c
# their first portable cells while both nodes still sat in the integration
# shard, where they had been placed while their buckets were empty.
reducer_orphan_nodes() {
    local map_json=$1 plan_file=$2
    # A node in both e2e_nodes and a test shard is already refused above as a
    # duplicate assignment, so every manifest node found in a shard is unpacked.
    jq -r --slurpfile plan "$plan_file" '
        [ (.debug_shards // [])[], (.release_shards // [])[] | .nodes[] ]
        | map(select(startswith("e2e.manifest_")))
        | map(. as $node
            | ($node | sub("^e2e\\.manifest_"; "") | sub("_on_host$"; "") | gsub("_"; "-"))
                as $category
            | select([$plan[0].cells[]
                | select(.lane == "portable" and .category == $category)] | length > 0)
            | $node)
        | unique[]
    ' <<<"$map_json"
}

orphan_fixture_dir=$(mktemp -d)
printf '%s\n' '{"cells":[{"lane":"portable","category":"fixture-c"},{"lane":"privileged","category":"privileged-c"}]}' \
    >"$orphan_fixture_dir/plan.json"
fixture_orphans=$(reducer_orphan_nodes \
    '{"e2e_nodes":[],"debug_shards":[{"slug":"x","nodes":["e2e.manifest_fixture_c_on_host","e2e.manifest_empty_c_on_host","e2e.manifest_privileged_c_on_host","test.fixture"]}],"release_shards":[]}' \
    "$orphan_fixture_dir/plan.json")
if [[ $fixture_orphans != e2e.manifest_fixture_c_on_host ]]; then
    echo "check-shard-coverage.sh: FAIL — reducer-orphan guard did not name exactly the planted nonempty co-scheduled node (got: ${fixture_orphans:-nothing})" >&2
    status=1
fi
fixture_orphans=$(reducer_orphan_nodes \
    '{"e2e_nodes":["e2e.manifest_fixture_c_on_host"],"debug_shards":[{"slug":"x","nodes":["e2e.manifest_empty_c_on_host"]}],"release_shards":[]}' \
    "$orphan_fixture_dir/plan.json")
rm -rf "$orphan_fixture_dir"
if [[ -n $fixture_orphans ]]; then
    echo "check-shard-coverage.sh: FAIL — reducer-orphan guard named a planted co-scheduled node that selects no portable cells" >&2
    status=1
fi

orphans=$(reducer_orphan_nodes "$shards_json" ci/expected-e2e-plan.json)
if [[ -n $orphans ]]; then
    echo "check-shard-coverage.sh: FAIL — E2E node(s) select portable cells but run in a test shard, which packs no parity-v1 archive for the reducer; move them to e2e_nodes:" >&2
    printf '  %s\n' $orphans >&2
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

# A step name is not unique across jobs (strict-compat and e2e both have
# "Unpack prebuilt trees"), so consumer checks read the step inside its job.
workflow_job_step_body() {
    local job=$1 step_name=$2 workflow_text=$3 body step
    body=$(workflow_job_body "$job" "$workflow_text") || return 1
    step=$(workflow_step_body "$step_name" "$body")
    [[ -n $step ]] || return 1
    printf '%s\n' "$step"
}

# Every step that checks its inputs with `require` uses one counted gate:
#
#   missing=0
#   require() { if ! test "$1" "$2"; then <name it>; missing+1; fi; }
#   require -x <path>          (one or more, comments allowed between)
#   if ((missing > 0)); then <report>; exit 1; fi
#
# Pinning the `require` lines alone does not hold that gate. Deleting the
# gate's `exit 1`, turning it into `exit 0`, or weakening the helper to
# `if false` or `test -e "$2"` keeps every pinned line while nothing is
# enforced. This reads the whole block in order, and refuses a `require` call
# or a `missing` use anywhere else in the step, so a `require` placed after
# the gate cannot report a missing input without failing the step.
require_gate_contract() {
    local step_text=$1
    awk '
        { line[++n] = $0 }
        END {
            for (i = 1; i <= n; i++) {
                if (line[i] == "          missing=0") {
                    if (start) exit 1
                    start = i
                }
            }
            if (!start) exit 1
            i = start + 1
            if (line[i++] != "          require() {") exit 1
            if (line[i++] != "            if ! test \"$1\" \"$2\"; then") exit 1
            if (line[i++] !~ /^              echo "::error::[^"$`\\]*: missing: \$2 \(test \$1 failed\)" >&2$/) exit 1
            if (line[i++] != "              missing=$((missing + 1))") exit 1
            if (line[i++] != "            fi") exit 1
            if (line[i++] != "          }") exit 1
            calls = 0
            while (i <= n && (line[i] ~ /^          require -[efsx] [^ ;&|$`]+$/ || line[i] ~ /^          #/)) {
                if (line[i] !~ /^          #/) calls++
                i++
            }
            if (!calls) exit 1
            if (line[i++] != "          if ((missing > 0)); then") exit 1
            if (line[i++] !~ /^            echo "[^"$`\\]*: \$missing required input\(s\) missing[^"$`\\]*" >&2$/) exit 1
            if (line[i++] != "            exit 1") exit 1
            if (line[i++] != "          fi") exit 1
            for (j = 1; j <= n; j++) {
                if (j >= start && j < i) continue
                if (line[j] ~ /^[[:space:]]*#/) continue
                if (line[j] ~ /(^|[^[:alnum:]_])(require|missing)([^[:alnum:]_]|$)/) exit 1
            }
        }
    ' <<<"$step_text"
}

# build.workspace_on_host builds Hermit in the validate profile (d44bbbb79ac),
# so hermit, verification-report and the DBT runtime must be packed from
# target/validate. Run 36836745615 failed when this step still named the
# target/debug copies, while this contract, which pinned the same stale path,
# stayed green. The pack step checks each input with `require`, which names a
# missing path and counts it, then exits 1 once every input has been checked.
# The four `require`/`missing` lines that end the first grep chain are that
# fail-closed gate: without them a missing input is reported and the tree is
# packed anyway.
debug_artifact_contract() {
    local workflow_text=$1 pack_step unpack_step
    local archive_member='            target/validate/verification-report \'
    local cpu_wrapper_member='            target/debug/nextest-cpu-wrapper \'
    local nextest_member='            target/ci/nextest-binaries \'
    pack_step=$(workflow_step_body "Pack debug prebuilt tree" "$workflow_text")
    unpack_step=$(workflow_step_body "Unpack debug tree" "$workflow_text")
    grep -Fqx '          require -x target/validate/verification-report' <<<"$pack_step" &&
        grep -Fqx "$archive_member" <<<"$pack_step" &&
        grep -Fqx '          test -x target/validate/verification-report' <<<"$unpack_step" &&
        grep -Fqx '          require -x target/debug/nextest-cpu-wrapper' <<<"$pack_step" &&
        grep -Fqx "$cpu_wrapper_member" <<<"$pack_step" &&
        grep -Fqx '          test -x target/debug/nextest-cpu-wrapper' <<<"$unpack_step" &&
        grep -Fqx '          require -f target/ci/nextest-binaries/current.json' <<<"$pack_step" &&
        grep -Fqx "$nextest_member" <<<"$pack_step" &&
        grep -Fqx '          test -f target/ci/nextest-binaries/current.json' <<<"$unpack_step" &&
        grep -Fqx '          require -x target/validate/hermit' <<<"$pack_step" &&
        grep -Fqx '            target/validate/hermit \' <<<"$pack_step" &&
        grep -Fqx '          test -x target/validate/hermit' <<<"$unpack_step" &&
        grep -Fqx '          require -f target/validate/deps/libdetcore_dbt.so' <<<"$pack_step" &&
        grep -Fqx '            target/validate/deps/libdetcore_dbt.so \' <<<"$pack_step" &&
        grep -Fqx '          test -f target/validate/deps/libdetcore_dbt.so' <<<"$unpack_step" &&
        grep -Fqx '            if ! test "$1" "$2"; then' <<<"$pack_step" &&
        grep -Fqx '              missing=$((missing + 1))' <<<"$pack_step" &&
        grep -Fqx '          if ((missing > 0)); then' <<<"$pack_step" &&
        grep -Fqx '            exit 1' <<<"$pack_step" || return 1
    require_gate_contract "$pack_step" &&
        debug_install_resources_contract "$pack_step"
}

# No release-profile producer exists since d44bbbb79ac
# (https://github.com/rrnewton/hermit/issues/3458), so the debug tree is the
# only carrier of the install resources that build.e2e_artifact publishes and
# the release shards read, and of the profile-staged Reverie LiteInst runtime
# that hermit-install writes beside the validate-profile Hermit. hermit-install
# links three of those resources
# into target/validate, which the tree does not carry whole, so the pack step
# must replace every link with a regular copy and refuse a leftover one. The
# replacement loop is pinned whole and must precede the tar: its dangling-link
# refusal and the leftover refusal are each the part that acts, and a line
# pinned without its consequence (`leftover=` without the `exit 1` after it)
# accepted a tree that still linked into target/validate.
debug_install_resources_contract() {
    local pack_step=$1 required symlink_block before_tar
    symlink_block=$(cat <<'EOF'
          while IFS= read -r -d '' link; do
            if ! resolved=$(readlink -e -- "$link") || ! test -f "$resolved"; then
              echo "::error::Pack debug prebuilt tree: dangling or non-file symlink: $link" >&2
              exit 1
            fi
            rm -f -- "$link"
            cp -p -- "$resolved" "$link"
          done < <(find target/install_pkg -type l -print0)
          leftover=$(find target/install_pkg -type l -print -quit)
          if [[ -n $leftover ]]; then echo "::error::Pack debug prebuilt tree: symlink left in target/install_pkg: $leftover" >&2; exit 1; fi
EOF
)
    before_tar=${pack_step%%'          tar --zstd -cf "$DEBUG_TARBALL" \'*}
    [[ $before_tar != "$pack_step" && $before_tar == *"$symlink_block"* ]] || return 1
    for required in \
        '          require -x target/install_pkg/rsrcs/dynamorio/bin64/drrun' \
        '          require -x target/install_pkg/rsrcs/sabre' \
        '          require -s target/install_pkg/rsrcs/sabre.revision' \
        '          require -x target/install_pkg/rsrcs/e9patch' \
        '          require -x target/install_pkg/rsrcs/e9tool' \
        '          require -s target/install_pkg/rsrcs/libdetcore_sabre.so' \
        '          require -s target/install_pkg/rsrcs/libreverie_dbt_client.so' \
        '          require -s target/install_pkg/rsrcs/libreverie_liteinst.so' \
        '          require -s target/validate/libreverie_liteinst.so' \
        '          require -s target/validate/libreverie_liteinst.so.revision' \
        '            target/install_pkg \' \
        '            target/validate/libreverie_liteinst.so \' \
        '            target/validate/libreverie_liteinst.so.revision \'
    do
        grep -Fqx -- "$required" <<<"$pack_step" || return 1
    done
}

prepared_nextest_artifact_contract() {
    local workflow_text=$1 pack_step job body
    pack_step=$(workflow_step_body "Pack prepared Nextest inputs" "$workflow_text")
    grep -Fq '.selections[].binaries[].executable.path' <<<"$pack_step" &&
        grep -Fq '.selections[].runtime_files[].path' <<<"$pack_step" &&
        grep -Fq '.guests[].path' <<<"$pack_step" &&
        grep -Fqx '            target/debug/nextest-cpu-wrapper \' <<<"$pack_step" &&
        grep -Fqx '            target/ci/nextest-binaries' <<<"$pack_step" || return 1

    for job in test-debug strict-compat test-release; do
        body=$(workflow_job_body "$job" "$workflow_text") || return 1
        grep -Fqx '          name: ${{ env.NEXTEST_ARTIFACT }}' <<<"$body" &&
            grep -Fqx '          tar --zstd -xf "$NEXTEST_TARBALL"' <<<"$body" &&
            grep -Fqx '          test -x target/debug/nextest-cpu-wrapper' <<<"$body" &&
            grep -Fqx '          test -f target/ci/nextest-binaries/current.json' <<<"$body" || return 1
    done

    # build-complete runs build.e2e_artifact_on_host, whose first command,
    # `nextest-binaries.rs assert hosted-portable`, re-hashes every recorded
    # test executable and runtime file. Only this artifact carries them.
    body=$(workflow_job_body build-complete "$workflow_text") || return 1
    grep -Fqx '          name: ${{ env.NEXTEST_ARTIFACT }}' <<<"$body" &&
        grep -Fqx '          tar --zstd -xf "$NEXTEST_TARBALL"' <<<"$body" &&
        grep -Fqx '          require -x target/debug/nextest-cpu-wrapper' <<<"$body" &&
        grep -Fqx '          require -f target/ci/nextest-binaries/current.json' <<<"$body"
}

# Every node of test-debug and test-release runs inside ci/run-hosted-node.sh's
# network namespace, where Cargo cannot download anything. lint.clippy compiles
# against the locked registry crates and git checkouts, and every
# prepared-Nextest consumer re-hashes each git dependency's Cargo checkout
# (sources() in ci/manifest-plan/src/nextest_binaries.rs). A rust-cache restore
# does not supply them: its key hashes the runner's installed toolchains, so it
# misses whenever a runner image's default stable rustc differs from
# build-debug's. Run 37469056816 failed three shards that way. Each job must
# therefore run an unconditional `cargo fetch --locked` step before the step
# that enters the namespace. strict-compat is not listed because neither of its
# nodes runs Cargo or verifies the prepared-Nextest record.
locked_source_jobs=(test-debug test-release)
locked_sources_fetched_contract() {
    local workflow_text=$1 job body step fetch_line enter_line
    for job in "${locked_source_jobs[@]}"; do
        body=$(workflow_job_body "$job" "$workflow_text") || return 1
        step=$(workflow_step_body "Fetch locked workspace dependencies" "$body" |
            grep -Ev '^[[:space:]]*(#|$)') || return 1
        [[ $step == '        run: cargo fetch --locked' ]] || return 1
        fetch_line=$(grep -nFx '      - name: Fetch locked workspace dependencies' <<<"$body" | cut -d: -f1)
        enter_line=$(grep -m1 -nE '^        run: \./ci/run-hosted-node\.sh ' <<<"$body" | cut -d: -f1)
        [[ $fetch_line =~ ^[0-9]+$ && $enter_line =~ ^[0-9]+$ ]] &&
            ((fetch_line < enter_line)) || return 1
    done
}

# Remove one named step, and the lines that belong to it, from one job only.
drop_job_step() {
    local job=$1 step_name=$2 workflow_text=$3 body mutated
    body=$(workflow_job_body "$job" "$workflow_text") || return 1
    mutated=$(awk -v marker="      - name: $step_name" '
        $0 == marker { skip = 1; next }
        skip && /^      (- |# )/ { skip = 0 }
        !skip { print }
    ' <<<"$body")
    [[ $mutated != "$body" ]] || return 1
    printf '%s' "${workflow_text/"$body"/"$mutated"}"
}

# The completed-build job is the literal contraction
#   build.buck_release_artifact + build.workspace -> build.e2e_artifact
# with build.workspace supplied by build-debug's tree. It must run both shard
# buckets, fail closed on a missing predecessor input, and hand target/ci,
# target/install_pkg and the LiteInst runtime to the release-tree consumers.
completed_build_contract() {
    local workflow_text=$1 body unpack_step pack_step required
    body=$(workflow_job_body build-complete "$workflow_text") || return 1
    unpack_step=$(workflow_step_body "Unpack prerequisite trees" "$workflow_text")
    pack_step=$(workflow_step_body "Pack full release prebuilt tree (artifact + resources + liteinst)" "$workflow_text")
    grep -Fqx -- "        run: ./ci/run-node.sh portable \"\$(jq -r '(.build_dbt_nodes + .build_aux_nodes)|join(\",\")' ci/portable-shards.json)\"" <<<"$body" &&
        grep -Fqx '        run: cargo fetch --locked' <<<"$body" &&
        require_gate_contract "$unpack_step" &&
        require_gate_contract "$pack_step" || return 1
    for required in \
        '          tar --zstd -xf "$DEBUG_TARBALL"' \
        '          require -x target/validate/hermit' \
        '          require -x target/install_pkg/rsrcs/dynamorio/bin64/drrun' \
        '          require -x target/install_pkg/rsrcs/sabre' \
        '          require -s target/install_pkg/rsrcs/sabre.revision' \
        '          require -s target/install_pkg/rsrcs/libdetcore_sabre.so' \
        '          require -s target/validate/libreverie_liteinst.so' \
        '          require -s target/validate/libreverie_liteinst.so.revision' \
        '              missing=$((missing + 1))' \
        '          if ((missing > 0)); then' \
        '            exit 1'
    do
        grep -Fqx -- "$required" <<<"$unpack_step" || return 1
    done
    for required in \
        '          require -x target/ci/hermit' \
        '          require -s target/ci/libdetcore_sabre.so' \
        '          require -s target/ci/hermit-e2e-artifact.path' \
        '          require -s target/install_pkg/rsrcs/libdetcore_dbt.so' \
        '          require -s target/install_pkg/rsrcs/libreverie_dbt_client.so' \
        '          require -s target/validate/libreverie_liteinst.so' \
        '          require -s target/validate/libreverie_liteinst.so.revision' \
        '              missing=$((missing + 1))' \
        '          if ((missing > 0)); then' \
        '            exit 1' \
        '            target/ci \' \
        '            target/install_pkg \' \
        '            target/validate/libreverie_liteinst.so \' \
        '            target/validate/libreverie_liteinst.so.revision'
    do
        grep -Fqx -- "$required" <<<"$pack_step" || return 1
    done
}

# Nothing builds target/release since d44bbbb79ac, and target/ci/hermit-strict
# exists only in Buck mode, so every executable line that named either was a
# dangling input (https://github.com/rrnewton/hermit/issues/3458). Each
# release-tree consumer must instead require target/ci/hermit, the one path
# build.e2e_artifact installs, behind the counted gate that exits 1 when it is
# absent.
release_tree_consumers=(
    'build-complete|Pack full release prebuilt tree (artifact + resources + liteinst)'
    'test-debug|Unpack completed release tree'
    'strict-compat|Unpack prebuilt trees'
    'test-release|Unpack full release prebuilt tree'
    'e2e|Unpack prebuilt trees'
    'sabre_non_gated_parity|Unpack product and runner trees'
)
release_tree_consumer_contract() {
    local workflow_text=$1 consumer step body required
    if grep -v '^[[:space:]]*#' <<<"$workflow_text" | grep -Eq 'target/release|hermit-strict'; then
        return 1
    fi
    for consumer in "${release_tree_consumers[@]}"; do
        step=$(workflow_job_step_body "${consumer%%|*}" "${consumer#*|}" "$workflow_text") || return 1
        require_gate_contract "$step" &&
            grep -Fqx '          require -x target/ci/hermit' <<<"$step" || return 1
    done
    step=$(workflow_job_step_body test-release 'Unpack full release prebuilt tree' "$workflow_text") || return 1
    for required in \
        '          require -x target/install_pkg/rsrcs/sabre' \
        '          require -f target/install_pkg/rsrcs/libreverie_dbt_client.so' \
        '          require -f target/install_pkg/rsrcs/libdetcore_dbt.so' \
        '          require -s target/validate/libreverie_liteinst.so' \
        '          require -s target/validate/libreverie_liteinst.so.revision'
    do
        grep -Fqx -- "$required" <<<"$step" || return 1
    done
    body=$(workflow_job_body sabre_non_gated_parity "$workflow_text") || return 1
    grep -Fqx '      HERMIT_BIN: ${{ github.workspace }}/target/ci/hermit' <<<"$body"
}

# build-debug publishes the prepared-Nextest record inside build.workspace_on_host.
# Anything that rebuilds afterward rewrites recorded executables or runtime
# files (run 36485831200: a DBT staging `cargo build -p detcore-dbt` rewrote
# target/debug/deps/libdetcore_dbt.{so,rlib}), and every consumer then refuses
# its inventory. The producer must re-assert the complete record after its last
# Cargo invocation and before either pack step.
prepared_nextest_producer_contract() {
    local workflow_text=$1 body assert_line last_cargo pack_debug pack_nextest
    body=$(workflow_job_body build-debug "$workflow_text") || return 1
    assert_line=$(grep -nFx '          ./ci/nextest-binaries.rs assert hosted-portable' <<<"$body" | cut -d: -f1)
    [[ $assert_line =~ ^[0-9]+$ ]] || return 1
    last_cargo=$(grep -nE '(^|[^-[:alnum:]_/])cargo[[:space:]]+(build|test|rustc|run|nextest|check|clippy|doc)([[:space:]]|$)' <<<"$body" |
        grep -vE '^[0-9]+:[[:space:]]*#' | tail -n 1 | cut -d: -f1)
    pack_debug=$(grep -nFx '      - name: Pack debug prebuilt tree' <<<"$body" | cut -d: -f1)
    pack_nextest=$(grep -nFx '      - name: Pack prepared Nextest inputs' <<<"$body" | cut -d: -f1)
    [[ $pack_debug =~ ^[0-9]+$ && $pack_nextest =~ ^[0-9]+$ ]] || return 1
    ((assert_line < pack_debug && assert_line < pack_nextest)) || return 1
    [[ -z $last_cargo ]] || ((last_cargo < assert_line))
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

workflow_job_uses_hosted_namespace_wrapper() {
    local job=$1 workflow_text=$2 body
    body=$(workflow_job_body "$job" "$workflow_text") || return 1
    grep -Fq './ci/run-hosted-node.sh portable ' <<<"$body"
}

workflow_e2e_uses_pinned_result_root() {
    local workflow_text=$1 body
    body=$(workflow_job_body e2e "$workflow_text") || return 1
    grep -Fqx '      E2E_RESULT_ROOT: /results/${{ matrix.slug }}' <<<"$body" &&
        grep -Fqx '          sudo install -d -o "$(id -u)" -g "$(id -g)" /results' <<<"$body" &&
        grep -Fqx '            sudo chmod a+rw /dev/kvm' <<<"$body" &&
        grep -Fqx '            sudo sysctl -w kernel.perf_event_paranoid=-1' <<<"$body"
}

# The `packages:` input of every step in JOB that uses the cached apt-install
# action (.github/actions/apt-install), one line per step. Only a `packages:`
# line inside such a step counts, so a package named in a comment or in another
# step's input does not satisfy a caller. A trailing ` # ...` is a YAML comment
# that the action never receives, so it is stripped before the caller matches.
workflow_job_apt_packages() {
    local job=$1 workflow_text=$2 body
    body=$(workflow_job_body "$job" "$workflow_text") || return 1
    awk '
        /^      - / { action = "" }
        $0 == "        uses: ./.github/actions/apt-install" { action = "apt"; next }
        action == "apt" && /^          packages:[[:space:]]/ {
            line = $0
            sub(/^          packages:[[:space:]]*/, "", line)
            sub(/[[:space:]]+#.*$/, "", line)
            print line
            action = ""
        }
    ' <<<"$body"
}

workflow_e2e_prepares_btrfs() {
    local workflow_text=$1 body packages
    body=$(workflow_job_body e2e "$workflow_text") || return 1
    packages=$(workflow_job_apt_packages e2e "$workflow_text") || return 1
    grep -Eq '(^| )btrfs-progs( |$)' <<<"$packages" &&
        grep -Fqx '      - name: Provide Btrfs sysfs state for system-utils' <<<"$body" &&
        grep -Fqx "        if: matrix.slug == 'system_utils'" <<<"$body" &&
        grep -Fqx '          sudo truncate -s 128M /tmp/hermit-ci-btrfs.img' <<<"$body" &&
        grep -Fqx '          sudo mkfs.btrfs -q -f /tmp/hermit-ci-btrfs.img' <<<"$body" &&
        grep -Fqx '          sudo install -d /mnt/hermit-ci-btrfs' <<<"$body" &&
        grep -Fqx '          sudo mount -o loop /tmp/hermit-ci-btrfs.img /mnt/hermit-ci-btrfs' <<<"$body" &&
        grep -Fqx "          compgen -G '/sys/fs/btrfs/*/commit_stats' >/dev/null" <<<"$body"
}

# Product and evidence failures must both remain visible in the workflow result.
workflow_e2e_verdict_contract() {
    local workflow_text=$1 e2e_body regular_body run_step verdict_step reducer_step
    e2e_body=$(workflow_job_body e2e "$workflow_text") || return 1
    regular_body=$(workflow_job_body regular "$workflow_text") || return 1
    run_step=$(workflow_step_body 'Run constructed E2E node (${{ matrix.node }})' "$workflow_text") || return 1
    verdict_step=$(workflow_step_body 'Run constructed E2E verdict' "$workflow_text") || return 1
    reducer_step=$(workflow_step_body 'Verify completeness, checksums, and archive paths' "$workflow_text") || return 1

    ! grep -Eq '^[[:space:]]+continue-on-error:' <<<"$e2e_body" &&
        ! grep -Eq '^[[:space:]]+continue-on-error:' <<<"$regular_body" &&
        grep -Fq './ci/run-hosted-node.sh portable ' <<<"$run_step" &&
        grep -Fq './ci/run-node.sh portable ' <<<"$verdict_step" &&
        grep -Fqx '          EXPECTED_CELLS: ${{ needs.plan.outputs.selected_cell_count }}' <<<"$reducer_step" &&
        grep -Fq '.repository_sha == $sha' <<<"$reducer_step" &&
        grep -Fq '.hermit_sha == $sha' <<<"$reducer_step" &&
        grep -Fq '.lane == "portable"' <<<"$reducer_step" &&
        grep -Fq "$hosted_e2e_cell_filter" <<<"$reducer_step" &&
        grep -Fq '.source_tree_dirty == false' <<<"$reducer_step" &&
        grep -Fq '[.lane, .category, .test, .mode, .backend]' <<<"$reducer_step" &&
        grep -Fq 'ci/expected-e2e-plan.json > ignored/reduced/expected-identities.json' <<<"$reducer_step" &&
        grep -Fq 'ignored/reduced/results.jsonl > ignored/reduced/actual-identities.json' <<<"$reducer_step" &&
        grep -Fq 'cmp -s ignored/reduced/expected-identities.json ignored/reduced/actual-identities.json' <<<"$reducer_step"
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

# The strict-compat job runs manifest buckets with portable cells
# (e2e.manifest_compat_on_host), so, like an e2e matrix job, it packs each one's
# parity-v1 archive for the reducer through ci/pack-parity-transport.sh, the
# reducer's expected archive count includes them, and selection runs the job
# whenever e2e runs, because the reducer requires every portable cell.
workflow_strict_compat_parity_contract() {
    local workflow_text=$1 map_json=$2 node slug body archive_line
    body=$(workflow_job_body strict-compat "$workflow_text") || return 1
    while IFS= read -r node; do
        slug=${node#e2e.manifest_}
        slug=${slug%_on_host}
        grep -Fqx "        run: ./ci/pack-parity-transport.sh $slug $node" <<<"$body" &&
            workflow_artifact_edge strict-compat name \
                "parity-v1-\${{ github.run_id }}-\${{ github.run_attempt }}-portable-$slug" \
                regular pattern 'parity-v1-${{ github.run_id }}-${{ github.run_attempt }}-*' \
                "$workflow_text" || return 1
    done < <(jq -r '.strict_compat_nodes[] | select(startswith("e2e.manifest_"))' <<<"$map_json")
    IFS= read -r archive_line <<'LINE'
          archive_count=$(jq '[.e2e_nodes[], (.strict_compat_nodes[] | select(startswith("e2e.manifest_")))] | length' ci/portable-shards.json)
LINE
    grep -Fqx -- "$archive_line" <<<"$workflow_text" &&
        grep -Fqx '          EXPECTED: ${{ needs.build-debug.outputs.parity_archive_count }}' <<<"$workflow_text" &&
        grep -Fqx '          if [[ "$run_e2e" == "true" && "$run_strict" != "true" ]]; then' <<<"$workflow_text"
}

# GitHub reports one result for the whole test-debug matrix, so a strict-compat
# job left on the default success() condition is skipped when any leg fails,
# also one that runs none of its predecessors, and its parity archives never
# reach the reducer (runs 38106386550 and 38110887257). Each leg records its
# success and strict-compat, run under !cancelled() once both builds succeeded,
# requires the records of exactly the legs its dependency closure needs
# (ci/debug-shard-success.sh).
workflow_strict_compat_dependency_contract() {
    local workflow_text=$1 debug_body strict_body
    debug_body=$(workflow_job_body test-debug "$workflow_text") || return 1
    strict_body=$(workflow_job_body strict-compat "$workflow_text") || return 1
    grep -Fqx '        run: ./ci/debug-shard-success.sh record "${{ matrix.slug }}" "${{ matrix.nodes }}"' \
        <<<"$debug_body" &&
        workflow_artifact_edge test-debug name \
            'debug-shard-ok-${{ github.run_id }}-${{ github.run_attempt }}-${{ matrix.slug }}' \
            strict-compat pattern 'debug-shard-ok-${{ github.run_id }}-${{ github.run_attempt }}-*' \
            "$workflow_text" &&
        grep -Fqx '      !cancelled()' <<<"$strict_body" &&
        grep -Fqx "      && needs.build-debug.result == 'success'" <<<"$strict_body" &&
        grep -Fqx "      && needs.build-complete.result == 'success'" <<<"$strict_body" &&
        grep -Fqx "        run: ./ci/debug-shard-success.sh require ignored/debug-shard-ok \"\$(jq -r '.strict_compat_nodes|join(\",\")' ci/portable-shards.json)\"" \
            <<<"$strict_body"
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
        workflow_job_uses_hosted_namespace_wrapper test-debug "$workflow_text" &&
        workflow_job_uses_hosted_namespace_wrapper strict-compat "$workflow_text" &&
        workflow_job_uses_hosted_namespace_wrapper test-release "$workflow_text" &&
        workflow_job_uses_hosted_namespace_wrapper e2e "$workflow_text" &&
        workflow_e2e_uses_pinned_result_root "$workflow_text" &&
        workflow_e2e_prepares_btrfs "$workflow_text" &&
        workflow_e2e_verdict_contract "$workflow_text" &&
        workflow_job_needs_exactly preflight 'select' "$workflow_text" &&
        workflow_job_needs_exactly checks 'select,preflight' "$workflow_text" &&
        workflow_job_needs_exactly build-debug 'select,preflight' "$workflow_text" &&
        workflow_job_needs_exactly build-complete 'select,build-debug' "$workflow_text" &&
        workflow_job_needs_exactly test-debug 'select,build-debug,build-complete' "$workflow_text" &&
        workflow_job_needs_exactly strict-compat 'select,build-debug,build-complete,test-debug' "$workflow_text" &&
        workflow_job_needs_exactly test-release 'select,build-complete' "$workflow_text" &&
        workflow_job_needs_exactly e2e 'select,build-debug,build-complete' "$workflow_text" &&
        workflow_job_needs_exactly regular \
            'select,plan,preflight,checks,build-debug,build-complete,test-debug,strict-compat,test-release,e2e' \
            "$workflow_text" &&
        workflow_artifact_edge preflight name '${{ env.MANIFEST_PLAN_ARTIFACT }}' \
            build-debug name '${{ env.MANIFEST_PLAN_ARTIFACT }}' "$workflow_text" &&
        workflow_artifact_edge build-debug name '${{ env.DEBUG_ARTIFACT }}' \
            build-complete name '${{ env.DEBUG_ARTIFACT }}' "$workflow_text" &&
        workflow_artifact_edge build-debug name '${{ env.NEXTEST_ARTIFACT }}' \
            build-complete name '${{ env.NEXTEST_ARTIFACT }}' "$workflow_text" &&
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

# test.hermit_integration_on_host (the debug "integration" shard) runs the
# verification_report_cli and verification_report_consumers tests, which execute
# target/validate/verification-report (build.workspace_on_host builds in the
# validate profile) through env!("CARGO_BIN_EXE_verification-report") after the
# debug tree crosses the build-debug -> test-debug job boundary. Guard all three
# parts of that contract:
# producer existence, archive membership, and executable consumer assertion.
# The mutation bracket proves the guard rejects the original omission instead
# of passing merely because the binary is mentioned somewhere in the workflow.
workflow_text=$(<"$workflow")
if ! debug_artifact_contract "$workflow_text"; then
    echo "check-shard-coverage.sh: FAIL — debug artifact must transport target/validate/{hermit,verification-report,deps/libdetcore_dbt.so}, the symlink-free target/install_pkg resources and the target/validate LiteInst runtime, and fail closed on a missing input" >&2
    status=1
fi
if ! prepared_nextest_artifact_contract "$workflow_text"; then
    echo "check-shard-coverage.sh: FAIL — prepared Nextest artifact must transport every identity-bound input to all Nextest consumers" >&2
    status=1
fi
omitted_artifact=${workflow_text/$'            target/validate/verification-report \\\n'/}
if [[ $omitted_artifact == "$workflow_text" ]]; then
    echo "check-shard-coverage.sh: FAIL — artifact omission fixture did not remove verification-report" >&2
    status=1
elif debug_artifact_contract "$omitted_artifact"; then
    echo "check-shard-coverage.sh: FAIL — artifact guard accepted a planted missing verification-report member" >&2
    status=1
fi
# Delete only the pack step's `exit 1`: the step would then report a missing
# input and pack the tree anyway.
pack_step_text=$(workflow_step_body "Pack debug prebuilt tree" "$workflow_text")
open_pack_step=${pack_step_text/$'            exit 1\n'/}
open_pack_gate=${workflow_text/"$pack_step_text"/"$open_pack_step"}
if [[ $open_pack_step == "$pack_step_text" || $open_pack_gate == "$workflow_text" ]]; then
    echo "check-shard-coverage.sh: FAIL — pack-gate mutation did not remove the pack step's exit 1" >&2
    status=1
elif debug_artifact_contract "$open_pack_gate"; then
    echo "check-shard-coverage.sh: FAIL — artifact guard accepted a pack step that reports a missing input and packs anyway" >&2
    status=1
fi
omitted_cpu_wrapper=${workflow_text/$'            target/debug/nextest-cpu-wrapper \\\n'/}
if [[ $omitted_cpu_wrapper == "$workflow_text" ]]; then
    echo "check-shard-coverage.sh: FAIL — nextest CPU-wrapper artifact omission fixture did not change the workflow" >&2
    status=1
elif debug_artifact_contract "$omitted_cpu_wrapper"; then
    echo "check-shard-coverage.sh: FAIL — artifact guard accepted a planted missing nextest CPU wrapper" >&2
    status=1
fi
omitted_nextest=${workflow_text/$'            target/ci/nextest-binaries \\\n'/}
if [[ $omitted_nextest == "$workflow_text" ]]; then
    echo "check-shard-coverage.sh: FAIL — prepared-nextest artifact omission fixture did not change the workflow" >&2
    status=1
elif debug_artifact_contract "$omitted_nextest"; then
    echo "check-shard-coverage.sh: FAIL — artifact guard accepted a planted missing prepared-nextest identity" >&2
    status=1
fi
missing_prepared_download=${workflow_text/$'      - name: Download prepared Nextest inputs\n        uses: actions/download-artifact@v4\n        with:\n          name: ${{ env.NEXTEST_ARTIFACT }}\n          path: .\n'/}
if [[ $missing_prepared_download == "$workflow_text" ]]; then
    echo "check-shard-coverage.sh: FAIL — prepared-nextest download mutation did not change the workflow" >&2
    status=1
elif prepared_nextest_artifact_contract "$missing_prepared_download"; then
    echo "check-shard-coverage.sh: FAIL — prepared-nextest guard accepted a consumer without its artifact download" >&2
    status=1
fi
# Debug-tree install resources (https://github.com/rrnewton/hermit/issues/3458).
# Each mutation edits only the named step and must be refused: dropping the
# install tree, keeping a symlink into the uncarried target/validate, or
# dropping the LiteInst runtime check would each leave build-complete or a
# release shard reading a missing file.
mutate_step() {
    local step_name=$1 from=$2 to=$3 workflow_text=$4 step mutated
    step=$(workflow_step_body "$step_name" "$workflow_text")
    mutated=${step/"$from"/"$to"}
    [[ -n $step && $mutated != "$step" ]] || return 1
    printf '%s' "${workflow_text/"$step"/"$mutated"}"
}
for omitted_resource_line in \
    $'            target/install_pkg \\\n' \
    $'            cp -p -- "$resolved" "$link"\n' \
    $'          require -s target/validate/libreverie_liteinst.so\n' \
    $'          if [[ -n $leftover ]]; then echo "::error::Pack debug prebuilt tree: symlink left in target/install_pkg: $leftover" >&2; exit 1; fi\n' \
    $'              exit 1\n'
do
    if ! omitted_resource=$(mutate_step "Pack debug prebuilt tree" "$omitted_resource_line" '' "$workflow_text") ||
        [[ $omitted_resource == "$workflow_text" ]]; then
        echo "check-shard-coverage.sh: FAIL — debug install-resource mutation did not change the pack step: ${omitted_resource_line%$'\n'}" >&2
        status=1
    elif debug_artifact_contract "$omitted_resource"; then
        echo "check-shard-coverage.sh: FAIL — artifact guard accepted a debug tree without: ${omitted_resource_line%$'\n'}" >&2
        status=1
    fi
done
# Keeping every pinned line while defeating the loop must also be refused:
# relinking after the copy, or a dangling-link test that can never fire.
symlink_loop_from=(
    $'            cp -p -- "$resolved" "$link"\n'
    '            if ! resolved=$(readlink -e -- "$link") || ! test -f "$resolved"; then'
)
symlink_loop_to=(
    $'            cp -p -- "$resolved" "$link"\n            ln -sfr -- "$resolved" "$link"\n'
    '            if false; then'
)
for index in "${!symlink_loop_from[@]}"; do
    if ! defeated_loop=$(mutate_step "Pack debug prebuilt tree" \
        "${symlink_loop_from[index]}" "${symlink_loop_to[index]}" "$workflow_text") ||
        [[ $defeated_loop == "$workflow_text" ]]; then
        echo "check-shard-coverage.sh: FAIL — debug symlink-loop mutation $index did not change the pack step" >&2
        status=1
    elif debug_artifact_contract "$defeated_loop"; then
        echo "check-shard-coverage.sh: FAIL — artifact guard accepted a debug tree whose symlink loop was defeated (mutation $index)" >&2
        status=1
    fi
done
if ! completed_build_contract "$workflow_text"; then
    echo "check-shard-coverage.sh: FAIL — build-complete must run build_dbt_nodes and build_aux_nodes from the build-debug tree, fail closed on a missing input, and pack target/ci, target/install_pkg and the LiteInst runtime" >&2
    status=1
fi
dbt_bucket_dropped=${workflow_text/'(.build_dbt_nodes + .build_aux_nodes)|join'/'(.build_aux_nodes)|join'}
if [[ $dbt_bucket_dropped == "$workflow_text" ]]; then
    echo "check-shard-coverage.sh: FAIL — completed-build node-bucket mutation did not change the workflow" >&2
    status=1
elif completed_build_contract "$dbt_bucket_dropped"; then
    echo "check-shard-coverage.sh: FAIL — completed-build guard accepted a job that skips build_dbt_nodes" >&2
    status=1
fi
unfetched_sources=${workflow_text/$'      - name: Fetch locked workspace dependencies\n        run: cargo fetch --locked\n'/}
if [[ $unfetched_sources == "$workflow_text" ]]; then
    echo "check-shard-coverage.sh: FAIL — completed-build fetch mutation did not change the workflow" >&2
    status=1
elif completed_build_contract "$unfetched_sources"; then
    echo "check-shard-coverage.sh: FAIL — completed-build guard accepted a job that cannot hash its git dependency checkouts" >&2
    status=1
fi
if ! release_tree_consumer_contract "$workflow_text"; then
    echo "check-shard-coverage.sh: FAIL — no executable workflow line may name target/release or target/ci/hermit-strict, and every release-tree consumer must require target/ci/hermit behind a counted gate that exits 1" >&2
    status=1
fi
stale_release_bin=${workflow_text/'      HERMIT_BIN: ${{ github.workspace }}/target/ci/hermit'/'      HERMIT_BIN: ${{ github.workspace }}/target/release/hermit'}
if [[ $stale_release_bin == "$workflow_text" ]]; then
    echo "check-shard-coverage.sh: FAIL — release-binary mutation did not change the workflow" >&2
    status=1
elif release_tree_consumer_contract "$stale_release_bin"; then
    echo "check-shard-coverage.sh: FAIL — release-tree guard accepted a consumer naming target/release/hermit" >&2
    status=1
fi
if ! unchecked_consumer=$(mutate_step "Unpack completed release tree" $'          require -x target/ci/hermit\n' '' "$workflow_text") ||
    [[ $unchecked_consumer == "$workflow_text" ]]; then
    echo "check-shard-coverage.sh: FAIL — release-tree consumer mutation did not change test-debug" >&2
    status=1
elif release_tree_consumer_contract "$unchecked_consumer"; then
    echo "check-shard-coverage.sh: FAIL — release-tree guard accepted test-debug without its target/ci/hermit check" >&2
    status=1
fi
# Every counted input gate (https://github.com/rrnewton/hermit/issues/3458).
# Each mutation keeps every pinned `require` line, so only the gate structure
# can refuse it: the gate's `exit 1` deleted or made `exit 0`, the helper
# weakened to `if false` or to `test -e`, and a `require` after the gate.
mutate_job_step() {
    local job=$1 step_name=$2 from=$3 to=$4 workflow_text=$5 body step mutated
    body=$(workflow_job_body "$job" "$workflow_text") || return 1
    step=$(workflow_step_body "$step_name" "$body")
    mutated=${step/"$from"/"$to"}
    [[ -n $step && $mutated != "$step" ]] || return 1
    mutated=${body/"$step"/"$mutated"}
    printf '%s' "${workflow_text/"$body"/"$mutated"}"
}
gate_from=(
    $'\n            exit 1\n'
    $'\n            exit 1\n'
    '            if ! test "$1" "$2"; then'
    '            if ! test "$1" "$2"; then'
    $'\n            exit 1\n          fi'
)
gate_to=(
    $'\n'
    $'\n            exit 0\n'
    '            if false; then'
    '            if ! test -e "$2"; then'
    $'\n            exit 1\n          fi\n          require -x target/ci/late'
)
for gate in \
    'build-debug|Pack debug prebuilt tree|debug_artifact_contract' \
    'build-complete|Unpack prerequisite trees|completed_build_contract' \
    "${release_tree_consumers[@]/%/|release_tree_consumer_contract}"
do
    IFS='|' read -r gate_job gate_step gate_contract <<<"$gate"
    for index in "${!gate_from[@]}"; do
        if ! open_gate=$(mutate_job_step "$gate_job" "$gate_step" \
            "${gate_from[index]}" "${gate_to[index]}" "$workflow_text") ||
            [[ $open_gate == "$workflow_text" ]]; then
            echo "check-shard-coverage.sh: FAIL — input-gate mutation $index did not change $gate_job: $gate_step" >&2
            status=1
        elif "$gate_contract" "$open_gate"; then
            echo "check-shard-coverage.sh: FAIL — $gate_contract accepted an open input gate (mutation $index) in $gate_job: $gate_step" >&2
            status=1
        fi
    done
done
if ! locked_sources_fetched_contract "$workflow_text"; then
    echo "check-shard-coverage.sh: FAIL — ${locked_source_jobs[*]} must each run an unconditional 'cargo fetch --locked' step before ci/run-hosted-node.sh enters the network namespace (run 37469056816)" >&2
    status=1
fi
# Each mutation must be refused: the fetch step deleted from one job, made
# conditional, run without --locked, or moved after the namespace entry.
for job in "${locked_source_jobs[@]}"; do
    if ! unfetched_job=$(drop_job_step "$job" "Fetch locked workspace dependencies" "$workflow_text") ||
        [[ $unfetched_job == "$workflow_text" ]]; then
        echo "check-shard-coverage.sh: FAIL — locked-source fetch deletion did not change $job" >&2
        status=1
    elif locked_sources_fetched_contract "$unfetched_job"; then
        echo "check-shard-coverage.sh: FAIL — locked-source guard accepted $job without its fetch step" >&2
        status=1
    fi
done
fetch_from=(
    '        run: cargo fetch --locked'
    '        run: cargo fetch --locked'
)
fetch_to=(
    $'        if: false\n        run: cargo fetch --locked'
    '        run: cargo fetch'
)
for index in "${!fetch_from[@]}"; do
    if ! weakened_fetch=$(mutate_job_step test-debug "Fetch locked workspace dependencies" \
        "${fetch_from[index]}" "${fetch_to[index]}" "$workflow_text") ||
        [[ $weakened_fetch == "$workflow_text" ]]; then
        echo "check-shard-coverage.sh: FAIL — locked-source fetch mutation $index did not change test-debug" >&2
        status=1
    elif locked_sources_fetched_contract "$weakened_fetch"; then
        echo "check-shard-coverage.sh: FAIL — locked-source guard accepted a weakened test-debug fetch (mutation $index)" >&2
        status=1
    fi
done
late_run_line='        run: ./ci/run-hosted-node.sh portable "${{ matrix.nodes }}"'
if ! late_fetch=$(drop_job_step test-release "Fetch locked workspace dependencies" "$workflow_text") ||
    ! late_fetch=$(mutate_job_step test-release 'Run shard nodes (${{ matrix.slug }})' "$late_run_line" \
        "$late_run_line"$'\n      - name: Fetch locked workspace dependencies\n        run: cargo fetch --locked' "$late_fetch") ||
    [[ $late_fetch == "$workflow_text" ]]; then
    echo "check-shard-coverage.sh: FAIL — locked-source late-fetch mutation did not change test-release" >&2
    status=1
elif locked_sources_fetched_contract "$late_fetch"; then
    echo "check-shard-coverage.sh: FAIL — locked-source guard accepted a test-release fetch after the namespace entry" >&2
    status=1
fi
if ! prepared_nextest_producer_contract "$workflow_text"; then
    echo "check-shard-coverage.sh: FAIL — build-debug must re-assert the prepared Nextest record after its last Cargo build and before packing" >&2
    status=1
fi
unverified_producer=${workflow_text/$'          ./ci/nextest-binaries.rs assert hosted-portable\n'/}
if [[ $unverified_producer == "$workflow_text" ]]; then
    echo "check-shard-coverage.sh: FAIL — prepared-nextest producer assertion mutation did not change the workflow" >&2
    status=1
elif prepared_nextest_producer_contract "$unverified_producer"; then
    echo "check-shard-coverage.sh: FAIL — prepared-nextest producer guard accepted a build-debug job without its record assertion" >&2
    status=1
fi
restaged_after_record=${workflow_text/$'      - name: Pack debug prebuilt tree\n'/$'      - name: Stage debug DBT runtime\n        run: cargo build -p detcore-dbt --message-format=json-render-diagnostics\n      - name: Pack debug prebuilt tree\n'}
if [[ $restaged_after_record == "$workflow_text" ]]; then
    echo "check-shard-coverage.sh: FAIL — prepared-nextest restage mutation did not change the workflow" >&2
    status=1
elif prepared_nextest_producer_contract "$restaged_after_record"; then
    echo "check-shard-coverage.sh: FAIL — prepared-nextest producer guard accepted a Cargo rebuild after the record assertion" >&2
    status=1
fi
if ! workflow_wiring_contract "$workflow_text"; then
    echo "check-shard-coverage.sh: FAIL — workflow job needs/artifact transfers do not match the constructed dependency supply contract" >&2
    status=1
fi
if ! workflow_strict_compat_parity_contract "$workflow_text" "$shards_json"; then
    echo "check-shard-coverage.sh: FAIL — the strict-compat job's manifest buckets do not reach the reducer: each must be packed by ci/pack-parity-transport.sh and uploaded, counted in parity_archive_count, and run whenever e2e runs" >&2
    status=1
fi
for mutation in pack count; do
    case $mutation in
    pack) planted=${workflow_text/$'        run: ./ci/pack-parity-transport.sh compat e2e.manifest_compat_on_host\n'/} ;;
    count) planted=${workflow_text/'${{ needs.build-debug.outputs.parity_archive_count }}'/'${{ needs.build-debug.outputs.e2e_count }}'} ;;
    esac
    if [[ $planted == "$workflow_text" ]]; then
        echo "check-shard-coverage.sh: FAIL — strict-compat parity $mutation mutation did not change the workflow fixture" >&2
        status=1
    elif workflow_strict_compat_parity_contract "$planted" "$shards_json"; then
        echo "check-shard-coverage.sh: FAIL — strict-compat parity guard accepted a planted $mutation defect" >&2
        status=1
    fi
done

if ! workflow_strict_compat_dependency_contract "$workflow_text"; then
    echo "check-shard-coverage.sh: FAIL — strict-compat must run whenever its own debug predecessors succeeded: each test-debug leg records its success, and strict-compat runs under !cancelled() and requires the records of its predecessors' legs (ci/debug-shard-success.sh)" >&2
    status=1
fi
for mutation in record require condition; do
    case $mutation in
    record) planted=${workflow_text/$'        run: ./ci/debug-shard-success.sh record "${{ matrix.slug }}" "${{ matrix.nodes }}"\n'/} ;;
    require) planted=${workflow_text/$'        run: ./ci/debug-shard-success.sh require ignored/debug-shard-ok'/$'        run: true ignored/debug-shard-ok'} ;;
    condition) planted=${workflow_text/$'      !cancelled()\n      && needs.select.outputs.run_strict_compat != \'false\''/$'      needs.select.outputs.run_strict_compat != \'false\''} ;;
    esac
    if [[ $planted == "$workflow_text" ]]; then
        echo "check-shard-coverage.sh: FAIL — strict-compat dependency $mutation mutation did not change the workflow fixture" >&2
        status=1
    elif workflow_strict_compat_dependency_contract "$planted"; then
        echo "check-shard-coverage.sh: FAIL — strict-compat dependency guard accepted a planted $mutation defect" >&2
        status=1
    fi
done
if ! "$ROOT_DIR/ci/debug-shard-success.sh" self-test; then
    echo "check-shard-coverage.sh: FAIL — ci/debug-shard-success.sh self-test" >&2
    status=1
fi

for step_name in 'Run constructed E2E node (${{ matrix.node }})' 'Run constructed E2E verdict'; do
    original="      - name: $step_name"
    non_gating_e2e=${workflow_text/"$original"/"$original"$'\n        continue-on-error: true'}
    if [[ $non_gating_e2e == "$workflow_text" ]]; then
        echo "check-shard-coverage.sh: FAIL — E2E verdict mutation did not change $step_name" >&2
        status=1
    elif workflow_e2e_verdict_contract "$non_gating_e2e"; then
        echo "check-shard-coverage.sh: FAIL — workflow guard accepted a non-gating E2E verdict in $step_name" >&2
        status=1
    fi
done
missing_e2e_identity=${workflow_text/$'              and .hermit_sha == $sha\n'/}
if [[ $missing_e2e_identity == "$workflow_text" ]]; then
    echo "check-shard-coverage.sh: FAIL — E2E identity mutation did not change the workflow fixture" >&2
    status=1
elif workflow_e2e_verdict_contract "$missing_e2e_identity"; then
    echo "check-shard-coverage.sh: FAIL — workflow guard accepted E2E evidence without source identity" >&2
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
hosted_wrapper='./ci/run-hosted-node.sh portable '
missing_hosted_wrapper=${workflow_text/"$hosted_wrapper"/'./ci/run-node.sh portable '}
if [[ $missing_hosted_wrapper == "$workflow_text" ]]; then
    echo "check-shard-coverage.sh: FAIL — hosted namespace-wrapper mutation did not change the workflow fixture" >&2
    status=1
elif workflow_wiring_contract "$missing_hosted_wrapper"; then
    echo "check-shard-coverage.sh: FAIL — workflow guard accepted a hosted test job outside its user namespace" >&2
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
kvm_access='            sudo chmod a+rw /dev/kvm'
missing_kvm_access=${workflow_text/"$kvm_access"/}
if [[ $missing_kvm_access == "$workflow_text" ]]; then
    echo "check-shard-coverage.sh: FAIL — KVM-access mutation did not change the workflow fixture" >&2
    status=1
elif workflow_wiring_contract "$missing_kvm_access"; then
    echo "check-shard-coverage.sh: FAIL — workflow guard accepted an E2E job that cannot open /dev/kvm" >&2
    status=1
fi
kvm_perf_access='            sudo sysctl -w kernel.perf_event_paranoid=-1'
missing_kvm_perf_access=${workflow_text/"$kvm_perf_access"/}
if [[ $missing_kvm_perf_access == "$workflow_text" ]]; then
    echo "check-shard-coverage.sh: FAIL — KVM-perf-access mutation did not change the workflow fixture" >&2
    status=1
elif workflow_wiring_contract "$missing_kvm_perf_access"; then
    echo "check-shard-coverage.sh: FAIL — workflow guard accepted an E2E job whose KVM guests cannot open perf events" >&2
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
btrfs_package_line=$(grep -E '^          packages: (.* )?btrfs-progs( |$)' <<<"$workflow_text" || true)
if [[ $(grep -c . <<<"$btrfs_package_line") != 1 ]]; then
    echo "check-shard-coverage.sh: FAIL — expected exactly one apt-install packages line naming btrfs-progs" >&2
    status=1
else
    without_btrfs_package=${workflow_text/"$btrfs_package_line"/"${btrfs_package_line/ btrfs-progs/}"}
    if [[ $without_btrfs_package == "$workflow_text" ]]; then
        echo "check-shard-coverage.sh: FAIL — btrfs-progs package mutation did not change the workflow fixture" >&2
        status=1
    elif workflow_wiring_contract "$without_btrfs_package"; then
        echo "check-shard-coverage.sh: FAIL — workflow guard accepted an E2E job that does not install btrfs-progs" >&2
        status=1
    fi
    btrfs_package_step=$'        uses: ./.github/actions/apt-install\n        with:\n'"$btrfs_package_line"
    btrfs_package_elsewhere=${workflow_text/"$btrfs_package_step"/$'        uses: ./.github/actions/not-apt-install\n        with:\n'"$btrfs_package_line"}
    if [[ $btrfs_package_elsewhere == "$workflow_text" ]]; then
        echo "check-shard-coverage.sh: FAIL — btrfs-progs action mutation did not change the workflow fixture" >&2
        status=1
    elif workflow_wiring_contract "$btrfs_package_elsewhere"; then
        echo "check-shard-coverage.sh: FAIL — workflow guard accepted btrfs-progs outside the apt-install action" >&2
        status=1
    fi
    # A trailing ` # ...` is a YAML comment the action never receives, so a
    # btrfs-progs that survives only there is not installed. This exercises
    # the comment stripping in workflow_job_apt_packages.
    btrfs_package_commented=${workflow_text/"$btrfs_package_line"/"${btrfs_package_line/ btrfs-progs/} # btrfs-progs"}
    if [[ $btrfs_package_commented == "$workflow_text" ]]; then
        echo "check-shard-coverage.sh: FAIL — btrfs-progs comment mutation did not change the workflow fixture" >&2
        status=1
    elif workflow_wiring_contract "$btrfs_package_commented"; then
        echo "check-shard-coverage.sh: FAIL — workflow guard accepted btrfs-progs named only in a trailing YAML comment" >&2
        status=1
    fi
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
strict_compat_json=$(jq -c '.strict_compat_nodes // []' <<<"$shards_json")
through_preflight=$(jq -cn --argjson preflight "$preflight_json" '$preflight')
through_checks=$(jq -cn --argjson preflight "$preflight_json" --argjson checks "$check_json" '$preflight + $checks')
through_debug=$(jq -cn --argjson preflight "$preflight_json" --argjson debug "$build_debug_json" '$preflight + $debug')
through_builds=$(jq -cn \
    --argjson preflight "$preflight_json" \
    --argjson debug "$build_debug_json" \
    --argjson dbt "$build_dbt_json" \
    --argjson aux "$build_aux_json" \
    '$preflight + $debug + $dbt + $aux')
# build-complete runs both the Buck-branch bucket and the publisher bucket after
# build-debug; no separate release producer exists since d44bbbb79ac.
build_complete_json=$(jq -cn --argjson dbt "$build_dbt_json" --argjson aux "$build_aux_json" '$dbt + $aux')

check_dependencies "preflight" "$preflight_json" "$through_preflight"
check_dependencies "check job" "$check_json" "$through_checks"
check_dependencies "debug build job" "$build_debug_json" "$through_debug"
check_dependencies "completed build job" "$build_complete_json" "$through_builds"

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

# shards_json has already resolved public aliases to the exact hosted twins.
# Keep the completed-build contraction bound to the committed hosted publisher.
# It also carried build.liteinst_runtime_release_on_host until 2026-09-30, when
# the one validate-profile workspace build began staging the LiteInst runtime.
if ! jq -e '
    (.build_aux_nodes // []) as $completed_build
    | ($completed_build | index("build.e2e_artifact_on_host") != null)
' <<<"$shards_json" >/dev/null; then
    echo "check-shard-coverage.sh: FAIL — completed build job must carry the hosted publisher build.e2e_artifact_on_host" >&2
    status=1
fi

if ((status == 0)); then
    n=$(printf '%s\n' "$assigned_unique" | grep -c . || true)
    cell_count=$(jq "[.cells[] | $hosted_e2e_cell_filter] | length" ci/expected-e2e-plan.json)
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
