#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
TEST_ROOT="$ROOT_DIR/tests/e2e"
MANIFEST_ROOT="$TEST_ROOT/manifests"
INVENTORY="$MANIFEST_ROOT/inventory/test-files.json"
EXPECTED_PLAN="$ROOT_DIR/ci/expected-e2e-plan.json"
HERMIT_BIN=${HERMIT_BIN:-$ROOT_DIR/target/debug/hermit}
RESULT_ROOT=${E2E_RESULT_ROOT:-$ROOT_DIR/target/e2e}
RUN_ID=${E2E_RUN_ID:-"local-$(date +%s)-$$"}
SOURCE_TREE_SHA=$(git -C "$ROOT_DIR" rev-parse HEAD)
if [[ -n $(git -C "$ROOT_DIR" status --porcelain --untracked-files=no) ]]; then
    SOURCE_TREE_DIRTY=true
else
    SOURCE_TREE_DIRTY=false
fi

readonly ROOT_DIR TEST_ROOT MANIFEST_ROOT INVENTORY EXPECTED_PLAN HERMIT_BIN RESULT_ROOT RUN_ID SOURCE_TREE_SHA SOURCE_TREE_DIRTY
readonly -a MODES=(verify chaos replay naked custom)
readonly -a BACKENDS=(ptrace dbi kvm sabre liteinst)
readonly -a LANES=(portable privileged)
DAG_DIR="$ROOT_DIR/ci/dag"
readonly DAG_DIR

function usage {
    cat <<'USAGE'
Usage:
  ci/test_harness.sh validate
  ci/test_harness.sh plan [--lane portable|privileged] [--format text|json]
  ci/test_harness.sh run [filters] [--results PATH] [--junit PATH]
  ci/test_harness.sh audit-gaps [--lane portable|privileged] [--format text|json]
  ci/test_harness.sh audit-inventory
  ci/test_harness.sh audit-ci

Filters:
  --lane LANE             portable or privileged
  --mode MODE             verify, chaos, replay, naked, or custom
  --backend BACKEND       ptrace, dbi, kvm, sabre, or liteinst
  --category CATEGORY     manifest category
  --test ID               exact category/test ID
  --include-occasional    include tests marked occasional
  --include-manual        include a ci=false cell; requires exact --test and --mode

The run command defaults to all CI-enabled, non-occasional cells in both lanes.
Naked controls are meta-CI checks and run only when explicitly selected.
Every selected required cell emits one JSONL record. Verification runs use the
required INFO level, but diagnostics stay outside the guest-observation hash.
USAGE
}

function die {
    echo "test_harness.sh: $*" >&2
    exit 2
}

function contains {
    local needle=$1
    shift
    local item
    for item in "$@"; do
        [[ $item == "$needle" ]] && return 0
    done
    return 1
}

function normalize_metadata {
    jq -c '
        . + {
          program_kind:
            (if has("direct") then "direct"
             elif (.program | endswith(".sh")) then "shell"
             elif (.program | endswith(".c")) then "c"
             elif (.program | endswith(".rs")) then "rust"
             else error("unsupported program kind") end),
          program_path: (.program // ""),
          direct_command: (.direct // ""),
          prepare_args: (if ((.program // "") | endswith(".sh")) then ["--prepare"] else [] end),
          compile_args: (.build.cflags // .build.rustflags // []),
          run_args: (if ((.program // "") | endswith(".sh")) then ["--run"] else [] end),
          modes: (.modes | with_entries(
            .value |= (. + {
              backends: (.backends_enabled // []),
              disabled: (.backends_disabled // {}),
              args: (.args // []),
              guest_args: (.guest_args // {}),
              assert: (.assert // {})
            } | del(.backends_enabled, .backends_disabled))))
        }
        | del(.program, .direct, .build)
    '
}

declare -A TEST_BY_ID=()
declare -A ID_BY_TEST=()
declare -A METADATA_BY_ID=()
declare -a TESTS=()

function metadata_json {
    local test=$1
    local id=${ID_BY_TEST[$test]:-}
    [[ -n $id ]] || die "internal error: no manifest entry for $test"
    printf '%s\n' "${METADATA_BY_ID[$id]}"
}

function load_tests {
    TESTS=()
    TEST_BY_ID=()
    ID_BY_TEST=()
    METADATA_BY_ID=()
    local documents raw metadata id test relative kind
    documents=$(cargo run --quiet -p hermit-manifest-plan -- --format harness-json) ||
        die "TOML manifest validation failed"
    while IFS= read -r raw; do
        id=$(jq -r .id <<<"$raw")
        relative=$(jq -r '.program // ""' <<<"$raw")
        if [[ -n $relative ]]; then
            test="$ROOT_DIR/$relative"
        else
            test="direct:$id"
        fi
        [[ -z ${TEST_BY_ID[$id]+x} ]] || die "duplicate test id: $id"
        [[ -z ${ID_BY_TEST[$test]+x} ]] || die "program appears in multiple tests: $relative"
        metadata=$(normalize_metadata <<<"$raw")
        kind=$(jq -r .program_kind <<<"$metadata")
        if [[ $kind == shell ]]; then
            [[ -x $test ]] || die "program is not executable: $relative"
            bash -n "$test" || die "bash syntax check failed: $relative"
        fi
        TEST_BY_ID[$id]=$test
        ID_BY_TEST[$test]=$id
        METADATA_BY_ID[$id]=$metadata
        TESTS+=("$test")
    done < <(jq -c '.[] as $manifest | $manifest.test[] | . + {category:$manifest.bucket}' <<<"$documents")
    ((${#TESTS[@]} > 0)) || die "no tests discovered below $MANIFEST_ROOT"
}

function audit_inventory {
    [[ -f $INVENTORY ]] || die "missing test inventory: ${INVENTORY#"$ROOT_DIR/"}"
    jq -e '
        .schema == 2
        and (.files | type == "array" and length > 0)
        and (.files | all(
            type == "object"
            and ((keys | sort) == ["disposition", "path", "runner", "why"])
            and (.path | type == "string" and startswith("tests/") and (contains("..") | not))
            and (.disposition | type == "string" and length > 0)
            and (.runner | type == "string" and length > 0)
            and (.why | type == "string" and length > 0)
            and (. as $entry | ($entry.why | startswith($entry.path + " is owned by " + $entry.runner + ": ")))))
        and ((.files | map(.path) | unique | length) == (.files | length))
        and ([.files[] | select(.disposition != "manifest-test")
              | . as $entry
              | ($entry.why | ltrimstr($entry.path + " is owned by " + $entry.runner + ": "))]
             | length == (unique | length))
        and all(.files[] | select(.disposition != "manifest-test");
            (. as $entry
             | ($entry.why
                | ltrimstr($entry.path + " is owned by " + $entry.runner + ": ")
                | length >= 120)))
    ' "$INVENTORY" >/dev/null || die "test inventory schema violation"

    local scratch expected actual
    scratch=$(mktemp -d)
    expected="$scratch/expected"
    actual="$scratch/actual"
    find "$ROOT_DIR/tests" \( -type f -o -type l \) -printf 'tests/%P\n' | LC_ALL=C sort >"$expected"
    jq -r '.files[].path' "$INVENTORY" | LC_ALL=C sort >"$actual"
    if ! diff -u "$expected" "$actual"; then
        rm -rf "$scratch"
        die "test inventory is stale; every file in tests/ must have an explicit disposition"
    fi

    local manifest_programs="$scratch/manifest-programs"
    local inventory_manifest_tests="$scratch/inventory-manifest-tests"
    local test
    for test in "${TESTS[@]}"; do
        [[ $test != direct:* ]] || continue
        printf '%s\n' "${test#"$ROOT_DIR/"}"
    done | LC_ALL=C sort >"$manifest_programs"
    jq -r '.files[] | select(.disposition == "manifest-test") | .path' "$INVENTORY" |
        LC_ALL=C sort >"$inventory_manifest_tests"
    if ! diff -u "$manifest_programs" "$inventory_manifest_tests"; then
        rm -rf "$scratch"
        die "manifest programs and disposition=manifest-test inventory entries differ"
    fi
    rm -rf "$scratch"

    jq '{files:(.files|length),by_disposition:(.files|group_by(.disposition)|map({key:.[0].disposition,value:length})|from_entries)}' \
        "$INVENTORY"
}

function function_body {
    local name=$1 file=$2
    awk -v signature="function $name {" '
        $0 == signature { inside = 1 }
        inside { print }
        inside && $0 == "}" { exit }
    ' "$file"
}

function assert_workflow_entrypoint {
    local lane=$1 workflow=$2 expected=$3
    local -a commands=()
    mapfile -t commands < <(
        sed -n -E "s/^[[:space:]]+run: (.*ci\/run-dag\.sh $lane([[:space:]].*)?)$/\1/p" "$workflow"
    )
    ((${#commands[@]} == 1)) ||
        die "GitHub $lane workflow must have exactly one executable ci/run-dag.sh $lane command"
    [[ ${commands[0]} == "$expected" ]] ||
        die "GitHub $lane workflow command diverged: ${commands[0]}"
}

function assert_validate_entrypoint {
    local lane=$1 function_name=$2 expected=$3
    local body
    body=$(function_body "$function_name" "$ROOT_DIR/validate.sh")
    [[ -n $body ]] || die "validate.sh function is missing: $function_name"
    [[ $(grep -Ec "^[[:space:]]*run_ci_manifest_lane $lane([[:space:]]|$)" <<<"$body") == 1 ]] ||
        die "validate.sh $function_name must call run_ci_manifest_lane $lane exactly once"
    grep -Fqx "$expected" <<<"$body" ||
        die "validate.sh $function_name command diverged from the audited entrypoint"
}

function dag_critical_path_seconds {
    local dag=$1
    jq -r '
        (.steps | map({key:(.group + "." + .job), value:.}) | from_entries) as $steps
        | def critical($id):
            $steps[$id] as $step
            | ($step.timeout + ([($step.deps // [])[] | critical(.)] | max // 0));
          [.steps[] | critical(.group + "." + .job)] | max
    ' "$dag"
}

function audit_ci_correspondence {
    local lane dag
    for lane in portable privileged; do
        dag="$ROOT_DIR/ci/dag/$lane.json"
        jq -e '
            .steps | type == "array" and length > 0
            and (map(.group + "." + .job) | unique | length) == length
            and all(.[]; (.cmd | type == "string" and length > 0))
        ' "$dag" >/dev/null || die "invalid or duplicate CI DAG steps: ci/dag/$lane.json"
    done

    # These are literal workflow/validate expressions, not local expansions.
    # shellcheck disable=SC2016
    assert_workflow_entrypoint portable "$ROOT_DIR/.github/workflows/ci-portable.yml" \
        'env SAFE_CI_DAG_RUNNER=agent-utils/py/bin/safe-ci-dag-runner ci/run-dag.sh portable --max-mem 14G --perf-dir "$RUNNER_TEMP/hermit-dag-perf" -v'
    # shellcheck disable=SC2016
    assert_workflow_entrypoint privileged "$ROOT_DIR/.github/workflows/ci-privileged.yml" \
        'timeout --foreground --kill-after=10s 270s env SAFE_CI_DAG_RUNNER=agent-utils/py/bin/safe-ci-dag-runner flock /tmp/hermit-privileged-pmu.lock ci/run-dag.sh privileged -j 2 --perf-dir "$RUNNER_TEMP/hermit-privileged-dag-perf" -v'
    # shellcheck disable=SC2016
    assert_validate_entrypoint portable run_portable_only_suite \
        '    run_ci_manifest_lane portable "${CI_PORTABLE_DAG_TIMEOUT_SECONDS:-7200}"'
    # shellcheck disable=SC2016
    assert_validate_entrypoint privileged run_privileged_validation \
        '    run_ci_manifest_lane privileged "${CI_PRIVILEGED_DAG_TIMEOUT_SECONDS:-7200}"'
    # The default full validation must delegate to both audited DAGs too.
    # shellcheck disable=SC2016
    assert_validate_entrypoint portable run_full_suite \
        '    run_ci_manifest_lane portable "${CI_PORTABLE_DAG_TIMEOUT_SECONDS:-7200}"'
    # shellcheck disable=SC2016
    assert_validate_entrypoint privileged run_full_suite \
        '    run_ci_manifest_lane privileged "${CI_PRIVILEGED_DAG_TIMEOUT_SECONDS:-7200}"'
    local runner_body
    runner_body=$(function_body run_ci_manifest_lane "$ROOT_DIR/validate.sh")
    # shellcheck disable=SC2016
    [[ $(grep -Fxc '        ./ci/run-dag.sh "$lane" -j "$jobs" -v' <<<"$runner_body") == 1 ]] ||
        die "validate.sh run_ci_manifest_lane must execute exactly one audited DAG"

    local privileged_critical_path
    privileged_critical_path=$(dag_critical_path_seconds "$ROOT_DIR/ci/dag/privileged.json")
    [[ $privileged_critical_path =~ ^[0-9]+$ ]] ||
        die "privileged DAG critical path is not an integer: $privileged_critical_path"
    ((privileged_critical_path <= 270)) ||
        die "privileged DAG timeout path ${privileged_critical_path}s exceeds workflow bound 270s"

    [[ -f $EXPECTED_PLAN ]] || die "missing E2E denominator ratchet: ${EXPECTED_PLAN#"$ROOT_DIR/"}"
    jq -e '.schema == 1 and (.cells | type == "array" and length > 0)' "$EXPECTED_PLAN" >/dev/null ||
        die "invalid E2E denominator ratchet"
    local scratch current_plan expected_plan
    scratch=$(mktemp -d)
    current_plan="$scratch/current-plan.json"
    expected_plan="$scratch/expected-plan.json"
    emit_required_plan | jq -sS 'sort_by(.category,.test,.mode,.backend)' >"$current_plan"
    jq -S '.cells | sort_by(.category,.test,.mode,.backend)' "$EXPECTED_PLAN" >"$expected_plan"
    if ! diff -u "$expected_plan" "$current_plan"; then
        rm -rf "$scratch"
        die "required E2E plan changed; update ci/expected-e2e-plan.json in the same review"
    fi

    local portable_fingerprint privileged_fingerprint e2e_cells
    portable_fingerprint=$(jq -Sc '.steps | map({id:(.group + "." + .job),cmd})' \
        "$ROOT_DIR/ci/dag/portable.json" | sha256sum | cut -d' ' -f1)
    privileged_fingerprint=$(jq -Sc '.steps | map({id:(.group + "." + .job),cmd})' \
        "$ROOT_DIR/ci/dag/privileged.json" | sha256sum | cut -d' ' -f1)
    e2e_cells=$(jq length "$current_plan")
    rm -rf "$scratch"
    jq -n \
        --arg portable_fingerprint "$portable_fingerprint" \
        --arg privileged_fingerprint "$privileged_fingerprint" \
        --argjson portable_steps "$(jq '.steps | length' "$ROOT_DIR/ci/dag/portable.json")" \
        --argjson privileged_steps "$(jq '.steps | length' "$ROOT_DIR/ci/dag/privileged.json")" \
        --argjson privileged_critical_path_seconds "$privileged_critical_path" \
        --argjson e2e_cells "$e2e_cells" \
        '{portable_steps:$portable_steps,privileged_steps:$privileged_steps,
          privileged_critical_path_seconds:$privileged_critical_path_seconds,
          e2e_cells:$e2e_cells,portable_fingerprint:$portable_fingerprint,
          privileged_fingerprint:$privileged_fingerprint,
          correspondence:"validated exact workflow and validate.sh entrypoints plus both DAG fingerprints"}'
}

# Enforce that the committed CI DAG stays in correspondence with the e2e test
# plan. `ci/run-dag.sh` executes ci/dag/<lane>.json VERBATIM, so a node that is
# deleted, renamed, or divorced from the plan silently changes what CI runs.
# This check reads the COMMITTED DAG (not a freshly re-rendered copy) and fails
# closed on any drift, so removing a node from portable.json makes `validate`
# exit non-zero. Two invariants are enforced per lane:
#   1. Referential integrity: every `deps` entry names a node that exists in the
#      same file (removing any depended-upon node leaves a dangling edge).
#   2. e2e <-> plan correspondence: the set of e2e run-nodes equals, exactly,
#      the (lane, category) cells the harness plans from the e2e metadata, and
#      the `e2e.metadata` gate node is present (removing a leaf e2e node, or
#      adding one for a category with no tests, is a mismatch).
function validate_dag_correspondence {
    local lane dag
    for lane in "${LANES[@]}"; do
        dag="$DAG_DIR/$lane.json"
        [[ -f $dag ]] || die "missing committed DAG: ci/dag/$lane.json"
        jq -e . >/dev/null <"$dag" || die "invalid DAG JSON: ci/dag/$lane.json"

        # --- (1) Referential integrity of the committed DAG. ---
        local node_ids dup dep_id
        node_ids=$(jq -r '.steps[] | .group + "." + .job' "$dag" | LC_ALL=C sort)
        dup=$(printf '%s\n' "$node_ids" | uniq -d)
        [[ -z $dup ]] || die "ci/dag/$lane.json: duplicate node id(s): $(echo $dup)"
        while IFS= read -r dep_id; do
            [[ -z $dep_id ]] && continue
            printf '%s\n' "$node_ids" | grep -Fxq -- "$dep_id" ||
                die "ci/dag/$lane.json: dependency '$dep_id' names no node (node removed or renamed?)"
        done < <(jq -r '.steps[] | (.deps // [])[]' "$dag" | LC_ALL=C sort -u)

        # --- (2) e2e run-nodes must correspond to the planned cells. ---
        # The e2e.metadata gate node (which runs this very `validate`) must exist.
        jq -e '[.steps[]
                | select(.group == "e2e" and .job == "metadata"
                         and (.cmd | test("test_harness\\.sh validate")))]
               | length == 1' >/dev/null <"$dag" ||
            die "ci/dag/$lane.json: missing e2e.metadata node running 'test_harness.sh validate'"

        # Every e2e run-node must target this lane.
        local wrong_lane
        wrong_lane=$(jq -r --arg lane "$lane" '
            .steps[] | select(.group == "e2e")
            | select(.cmd | test("--category "))
            | (.cmd | capture("--lane (?<l>\\S+) --category (?<c>\\S+)"))
            | select(.l != $lane) | .l + ":" + .c' "$dag")
        [[ -z $wrong_lane ]] ||
            die "ci/dag/$lane.json: e2e run-node targets wrong lane: $(echo $wrong_lane)"

        local expected actual
        expected=$(emit_required_plan | jq -r --arg lane "$lane" \
            'select(.lane == $lane) | .category' | LC_ALL=C sort -u)
        actual=$(jq -r '
            .steps[] | select(.group == "e2e")
            | select(.cmd | test("--category "))
            | (.cmd | capture("--category (?<c>\\S+)")) | .c' "$dag" |
            LC_ALL=C sort -u)

        if [[ $expected != "$actual" ]]; then
            {
                echo "ci/dag/$lane.json: e2e DAG nodes do not correspond to the $lane test plan."
                echo "  planned categories : $(echo $expected)"
                echo "  DAG run-node cats  : $(echo $actual)"
                comm -23 <(printf '%s\n' "$expected") <(printf '%s\n' "$actual") |
                    sed 's/^/  MISSING from DAG (node deleted or renamed?): /'
                comm -13 <(printf '%s\n' "$expected") <(printf '%s\n' "$actual") |
                    sed 's/^/  EXTRA in DAG (no such planned test category): /'
            } >&2
            die "DAG/plan correspondence mismatch for lane $lane"
        fi
    done
    echo "PASS: committed CI DAG (ci/dag/${LANES[0]}.json, ci/dag/${LANES[1]}.json) corresponds to the e2e plan with no dangling deps"
}

LANE_FILTER=
MODE_FILTER=
BACKEND_FILTER=
CATEGORY_FILTER=
TEST_FILTER=
FORMAT=text
RESULTS=
JUNIT=
INCLUDE_OCCASIONAL=0
INCLUDE_MANUAL=0

function parse_options {
    while (($#)); do
        case "$1" in
            --lane) LANE_FILTER=${2:?missing lane}; shift 2 ;;
            --mode) MODE_FILTER=${2:?missing mode}; shift 2 ;;
            --backend) BACKEND_FILTER=${2:?missing backend}; shift 2 ;;
            --category) CATEGORY_FILTER=${2:?missing category}; shift 2 ;;
            --test) TEST_FILTER=${2:?missing test id}; shift 2 ;;
            --format) FORMAT=${2:?missing format}; shift 2 ;;
            --results) RESULTS=${2:?missing result path}; shift 2 ;;
            --junit) JUNIT=${2:?missing JUnit path}; shift 2 ;;
            --include-occasional) INCLUDE_OCCASIONAL=1; shift ;;
            --include-manual) INCLUDE_MANUAL=1; shift ;;
            -h|--help) usage; exit 0 ;;
            *) die "unknown option: $1" ;;
        esac
    done

    [[ -z $LANE_FILTER ]] || contains "$LANE_FILTER" portable privileged || die "invalid lane: $LANE_FILTER"
    [[ -z $MODE_FILTER ]] || contains "$MODE_FILTER" "${MODES[@]}" || die "invalid mode: $MODE_FILTER"
    [[ -z $BACKEND_FILTER ]] || contains "$BACKEND_FILTER" "${BACKENDS[@]}" || die "invalid backend: $BACKEND_FILTER"
    [[ $FORMAT == text || $FORMAT == json ]] || die "invalid format: $FORMAT"
    if ((INCLUDE_MANUAL)); then
        [[ -n $TEST_FILTER && -n $MODE_FILTER ]] ||
            die "--include-manual requires exact --test and --mode filters"
    fi
}

function emit_required_plan {
    local test
    for test in "${TESTS[@]}"; do
        metadata_json "$test"
    done | jq -c \
        --arg lane_filter "$LANE_FILTER" \
        --arg mode_filter "$MODE_FILTER" \
        --arg backend_filter "$BACKEND_FILTER" \
        --arg category_filter "$CATEGORY_FILTER" \
        --arg test_filter "$TEST_FILTER" \
        --argjson include_manual "$INCLUDE_MANUAL" \
        --argjson include_occasional "$INCLUDE_OCCASIONAL" '
        select($lane_filter == "" or .lane == $lane_filter)
        | select($category_filter == "" or .category == $category_filter)
        | select($test_filter == "" or .id == $test_filter)
        | select($include_occasional == 1 or (.occasional // false) == false)
        | . as $test
        | .modes | to_entries[]
        | select($mode_filter == "" or .key == $mode_filter)
        | select(.value.ci == true or $mode_filter == "naked" or $include_manual == 1)
        | . as $mode
        | if .key == "naked" then
            select($backend_filter == "")
            | select(.value.backends | index("native") != null)
            | {test:$test.id,category:$test.category,lane:$test.lane,mode:$mode.key,backend:null}
          else
            .value.backends[] as $backend
            | select($backend_filter == "" or $backend == $backend_filter)
            | {test:$test.id,category:$test.category,lane:$test.lane,mode:$mode.key,backend:$backend}
          end
    '
}

function emit_gap_plan {
    local test
    for test in "${TESTS[@]}"; do
        metadata_json "$test"
    done | jq -c \
        --arg lane_filter "$LANE_FILTER" \
        --arg mode_filter "$MODE_FILTER" \
        --arg backend_filter "$BACKEND_FILTER" \
        --arg category_filter "$CATEGORY_FILTER" \
        --arg test_filter "$TEST_FILTER" \
        --argjson include_occasional "$INCLUDE_OCCASIONAL" '
        select($lane_filter == "" or .lane == $lane_filter)
        | select($category_filter == "" or .category == $category_filter)
        | select($test_filter == "" or .id == $test_filter)
        | select($include_occasional == 1 or (.occasional // false) == false)
        | . as $test
        | .modes | to_entries[]
        | select($mode_filter == "" or .key == $mode_filter)
        | . as $mode
        | if .key == "naked" then
            select($backend_filter == "")
            | .value.disabled.native as $why
            | select($why != null)
            | {test:$test.id,category:$test.category,lane:$test.lane,mode:$mode.key,
               backend:"native",classification:"disabled",why:$why}
          else
            .value.disabled | to_entries[]
            | select($backend_filter == "" or .key == $backend_filter)
            | {test:$test.id,category:$test.category,lane:$test.lane,mode:$mode.key,
               backend:.key,classification:"disabled",why:.value}
          end
    '
}

function print_plan {
    local kind=$1
    local plan
    if [[ $kind == required ]]; then
        plan=$(emit_required_plan)
    else
        plan=$(emit_gap_plan)
    fi
    if [[ $FORMAT == json ]]; then
        jq -s 'sort_by(.category,.test,.mode,.backend)' <<<"$plan"
    else
        jq -r '[.lane,.category,.test,.mode,(.backend // "-")] | @tsv' <<<"$plan" |
            LC_ALL=C sort
    fi
}

function prepare_cell_dirs {
    local cell_dir=$1
    mkdir -p "$cell_dir"/{home,xdg-config,tmp,fixtures,recording,captures}
    if [[ -d $TEST_ROOT/xdg-config ]]; then
        cp -a "$TEST_ROOT/xdg-config/." "$cell_dir/xdg-config/"
    fi
}

function run_capture {
    local stdout_file=$1
    local stderr_file=$2
    local timeout_seconds=$3
    shift 3

    # Without --foreground, GNU timeout owns a fresh process group and its
    # TERM/KILL sequence reaches the complete Hermit guest subtree.
    timeout --kill-after=10s "${timeout_seconds}s" "$@" \
        </dev/null >"$stdout_file" 2>"$stderr_file"
}

function observation_hash {
    local metadata=$1
    local status=$2
    local stdout_file=$3
    local stderr_file=$4
    local tmpdir=$5
    local include_status include_stdout include_stderr artifact
    include_status=$(jq -r .observation.status <<<"$metadata")
    include_stdout=$(jq -r .observation.stdout <<<"$metadata")
    include_stderr=$(jq -r .observation.stderr <<<"$metadata")
    {
        [[ $include_status == true ]] && printf 'status\0%s\0' "$status"
        [[ $include_stdout == true ]] && { printf 'stdout\0'; cat "$stdout_file"; printf '\0'; }
        [[ $include_stderr == true ]] && { printf 'stderr\0'; cat "$stderr_file"; printf '\0'; }
        while IFS= read -r artifact; do
            [[ -z $artifact ]] && continue
            printf 'artifact\0%s\0' "$artifact"
            if [[ -f $tmpdir/$artifact ]]; then
                cat "$tmpdir/$artifact"
            else
                printf 'MISSING'
            fi
            printf '\0'
        done < <(jq -r '.observation.artifacts[]' <<<"$metadata")
    } | sha256sum | cut -d' ' -f1
}

function prepare_test {
    local test=$1
    local cell_dir=$2
    local timeout_seconds=$3
    local stdout_file="$cell_dir/captures/prepare.stdout"
    local stderr_file="$cell_dir/captures/prepare.stderr"
    local -a env_args=(
        env
        LC_ALL=C
        TZ=UTC
        HOME="$cell_dir/home"
        XDG_CONFIG_HOME="$cell_dir/xdg-config"
        E2E_TMPDIR="$cell_dir/tmp"
        E2E_FIXTURE_DIR="$cell_dir/fixtures"
    )
    local metadata kind
    metadata=$(metadata_json "$test")
    kind=$(jq -r .program_kind <<<"$metadata")
    local -a prepare_args=() compile_args=()
    if [[ $kind == c ]]; then
        mapfile -t compile_args < <(jq -r '.compile_args[]' <<<"$metadata")
        if ! run_capture "$stdout_file" "$stderr_file" "$timeout_seconds" \
            cc -std=c11 -O2 -g -Wall -Wextra -Werror "${compile_args[@]}" \
            "$test" -o "$cell_dir/fixtures/program"; then
            echo "C program compilation failed for ${test#"$ROOT_DIR/"}" >&2
            cat "$stderr_file" >&2
            return 1
        fi
        return 0
    fi
    if [[ $kind == rust ]]; then
        mapfile -t compile_args < <(jq -r '.compile_args[]' <<<"$metadata")
        if ! run_capture "$stdout_file" "$stderr_file" "$timeout_seconds" \
            rustc -O "${compile_args[@]}" "$test" -o "$cell_dir/fixtures/program"; then
            echo "Rust program compilation failed for ${test#"$ROOT_DIR/"}" >&2
            cat "$stderr_file" >&2
            return 1
        fi
        return 0
    fi
    [[ $kind != direct ]] || return 0
    mapfile -t prepare_args < <(jq -r '.prepare_args[]' <<<"$metadata")
    if ! run_capture "$stdout_file" "$stderr_file" "$timeout_seconds" \
        "${env_args[@]}" "$test" "${prepare_args[@]}"; then
        echo "prepare failed for ${test#"$TEST_ROOT/"}" >&2
        cat "$stderr_file" >&2
        return 1
    fi
}

function execute_attempt {
    local test=$1
    local metadata=$2
    local mode=$3
    local backend=$4
    local cell_dir=$5
    local attempt=$6
    local seed=${7:-}
    local timeout_seconds stdout_file stderr_file guest_tmpdir kind guest_backend
    timeout_seconds=$(jq -r .timeout_seconds <<<"$metadata")
    stdout_file="$cell_dir/captures/${mode}-${attempt}.stdout"
    stderr_file="$cell_dir/captures/${mode}-${attempt}.stderr"
    kind=$(jq -r .program_kind <<<"$metadata")
    guest_backend=${backend:-native}

    rm -rf "$cell_dir/tmp"
    mkdir -p "$cell_dir/tmp"
    guest_tmpdir="$cell_dir/tmp"
    if [[ $mode != naked ]]; then
        # Hermit's built-in verification executes the guest twice. Its isolated
        # /tmp is fresh for each repeat, while a host-backed target path would
        # leak run-one mutations into run two.
        guest_tmpdir=/tmp/hermit-e2e
    fi

    local -a env_args=(
        env
        LC_ALL=C
        TZ=UTC
        HOME="$cell_dir/home"
        XDG_CONFIG_HOME="$cell_dir/xdg-config"
        E2E_TMPDIR="$guest_tmpdir"
        E2E_FIXTURE_DIR="$cell_dir/fixtures"
    )
    local -a command guest_command profile run_args guest_args custom_args
    mapfile -t run_args < <(jq -r '.run_args[]' <<<"$metadata")
    mapfile -t guest_args < <(
        jq -r --arg mode "$mode" --arg backend "$guest_backend" \
            '.modes[$mode].guest_args[$backend][]?' <<<"$metadata"
    )
    case "$kind" in
        c|rust)
            guest_command=("$cell_dir/fixtures/program" "${run_args[@]}" "${guest_args[@]}")
            ;;
        shell)
            guest_command=("$test" "${run_args[@]}" "${guest_args[@]}")
            ;;
        direct)
            guest_command=(bash -c "$(jq -r .direct_command <<<"$metadata")")
            if ((${#guest_args[@]})); then
                guest_command+=(-- "${guest_args[@]}")
            fi
            ;;
        *) die "internal error: unsupported program kind $kind" ;;
    esac
    profile=()
    if [[ $(jq -r .lane <<<"$metadata") == portable && $mode != naked ]]; then
        profile=(--no-virtualize-cpuid --max-timeslice=disabled)
    fi

    case "$mode" in
        naked)
            command=("${guest_command[@]}")
            ;;
        verify)
            command=("$HERMIT_BIN" --log=info run --backend "$backend" --strict --verify
                "${profile[@]}" -- "${guest_command[@]}")
            ;;
        replay)
            command=("$HERMIT_BIN" --log=info --backend "$backend" record start --strict --verify
                --data-dir "$cell_dir/recording" --record-timeout "$timeout_seconds" -- "${guest_command[@]}")
            ;;
        chaos)
            command=("$HERMIT_BIN" --log=off run --backend "$backend" --strict --chaos
                --sched-heuristic=random "--seed=$seed" "${profile[@]}" -- "${guest_command[@]}")
            ;;
        custom)
            mapfile -t custom_args < <(jq -r '.modes.custom.args[]' <<<"$metadata")
            command=("$HERMIT_BIN" --log=info run --backend "$backend"
                "${custom_args[@]}" -- "${guest_command[@]}")
            ;;
        *) die "internal error: unsupported mode $mode" ;;
    esac

    local status
    set +e
    run_capture "$stdout_file" "$stderr_file" "$timeout_seconds" \
        "${env_args[@]}" "${command[@]}"
    status=$?
    set -e

    local hash
    hash=$(observation_hash "$metadata" "$status" "$stdout_file" "$stderr_file" "$cell_dir/tmp")
    printf '%s\t%s\t%s\t%s\n' "$status" "$hash" "$stdout_file" "$stderr_file"
}

function append_result {
    local test_id=$1 category=$2 lane=$3 mode=$4 backend=$5 outcome=$6 duration_ms=$7 reason=$8
    local test_file test_sha256 binary_sha256 effective_args guest_args guest_backend relaxations log_level
    test_file=${TEST_BY_ID[$test_id]}
    if [[ -f $test_file ]]; then
        test_sha256=$(sha256sum "$test_file" | cut -d' ' -f1)
    else
        test_sha256=$(jq -r .direct_command <<<"${METADATA_BY_ID[$test_id]}" | sha256sum | cut -d' ' -f1)
    fi
    if [[ -x $HERMIT_BIN ]]; then
        binary_sha256=$(sha256sum "$HERMIT_BIN" | cut -d' ' -f1)
    else
        binary_sha256=
    fi
    guest_backend=${backend:-native}
    guest_args=$(jq -c --arg mode "$mode" --arg backend "$guest_backend" \
        '.modes[$mode].guest_args[$backend] // []' <<<"${METADATA_BY_ID[$test_id]}")
    relaxations='[]'
    case "$mode" in
        naked)
            effective_args='[]'
            log_level=
            ;;
        verify)
            effective_args=$(jq -cn --arg backend "$backend" \
                '["--log=info","run",("--backend=" + $backend),"--strict","--verify"]')
            log_level=info
            ;;
        replay)
            effective_args=$(jq -cn --arg backend "$backend" \
                '["--log=info",("--backend=" + $backend),"record","start","--strict","--verify"]')
            log_level=info
            ;;
        chaos)
            effective_args=$(jq -cn --arg backend "$backend" \
                '["--log=off","run",("--backend=" + $backend),"--strict","--chaos","--sched-heuristic=random"]')
            log_level=off
            ;;
        custom)
            effective_args=$(jq -cn --arg backend "$backend" \
                '["--log=info","run",("--backend=" + $backend),"<manifest-custom-args>"]')
            log_level=info
            ;;
    esac
    if [[ $lane == portable && $mode != naked ]]; then
        relaxations='["no-virtualize-cpuid","max-timeslice=disabled"]'
    fi
    jq -cn \
        --arg run_id "$RUN_ID" \
        --arg hermit_sha "$SOURCE_TREE_SHA" \
        --argjson source_tree_dirty "$SOURCE_TREE_DIRTY" \
        --arg binary_sha256 "$binary_sha256" \
        --arg test_sha256 "$test_sha256" \
        --arg test "$test_id" \
        --arg category "$category" \
        --arg lane "$lane" \
        --arg mode "$mode" \
        --arg backend "$backend" \
        --arg outcome "$outcome" \
        --arg reason "$reason" \
        --arg log_level "$log_level" \
        --argjson duration_ms "$duration_ms" \
        --argjson effective_args "$effective_args" \
        --argjson guest_args "$guest_args" \
        --argjson relaxations "$relaxations" \
        '{schema:1,run_id:$run_id,hermit_sha:$hermit_sha,source_tree_dirty:$source_tree_dirty,
          binary_sha256:(if $binary_sha256 == "" then null else $binary_sha256 end),
          test_sha256:$test_sha256,test:$test,category:$category,lane:$lane,mode:$mode,
          backend:(if $backend == "" then null else $backend end),classification:"required",
          outcome:$outcome,duration_ms:$duration_ms,
          log_level:(if $log_level == "" then null else $log_level end),
          effective_args:$effective_args,guest_args:$guest_args,
          relaxations:$relaxations,preprocessor:null,
          reason:(if $reason == "" then null else $reason end)}' >>"$RESULTS"
}

function run_cell {
    local test=$1 metadata=$2 mode=$3 backend=$4
    local id category lane slug cell_dir timeout_seconds start_ms end_ms duration_ms
    id=$(jq -r .id <<<"$metadata")
    category=$(jq -r .category <<<"$metadata")
    lane=$(jq -r .lane <<<"$metadata")
    slug=${id//\//-}-$mode-${backend:-none}
    cell_dir="$RESULT_ROOT/runs/$RUN_ID/$slug"
    timeout_seconds=$(jq -r .timeout_seconds <<<"$metadata")
    prepare_cell_dirs "$cell_dir"
    start_ms=$(date +%s%3N)

    local outcome=PASS reason=
    if ! prepare_test "$test" "$cell_dir" "$timeout_seconds"; then
        outcome=ERROR
        reason="fixture preparation failed"
    elif [[ $mode == naked ]]; then
        local runs min_distinct attempt row status hash
        local failed_runs=0
        local -a hashes=()
        runs=$(jq -r '.modes.naked.runs // 3' <<<"$metadata")
        min_distinct=$(jq -r '.modes.naked.assert.min_distinct // 2' <<<"$metadata")
        for ((attempt = 1; attempt <= runs; attempt++)); do
            row=$(execute_attempt "$test" "$metadata" "$mode" "" "$cell_dir" "$attempt")
            IFS=$'\t' read -r status hash _ _ <<<"$row"
            hashes+=("$hash")
            [[ $status == 0 ]] || ((failed_runs += 1))
        done
        local distinct
        distinct=$(printf '%s\n' "${hashes[@]}" | LC_ALL=C sort -u | wc -l)
        if ((failed_runs > 0)); then
            outcome=FAIL
            reason="naked control had $failed_runs failed native run(s) across $runs attempts"
        elif ((distinct < min_distinct)); then
            outcome=FAIL
            reason="naked control observed $distinct distinct outcome(s), need $min_distinct"
        else
            reason="naked control observed $distinct distinct outcomes across $runs runs"
        fi
    elif [[ $mode == chaos ]]; then
        local min_distinct min_passes min_failures seed row1 row2 status1 hash1 status2 hash2
        local passes=0 failures=0 repeat_mismatches=0
        local -a hashes=()
        min_distinct=$(jq -r '.modes.chaos.assert.min_distinct // 2' <<<"$metadata")
        min_passes=$(jq -r '.modes.chaos.assert.min_passes // 0' <<<"$metadata")
        min_failures=$(jq -r '.modes.chaos.assert.min_failures // 0' <<<"$metadata")
        while IFS= read -r seed; do
            row1=$(execute_attempt "$test" "$metadata" "$mode" "$backend" "$cell_dir" "seed-$seed-a" "$seed")
            row2=$(execute_attempt "$test" "$metadata" "$mode" "$backend" "$cell_dir" "seed-$seed-b" "$seed")
            IFS=$'\t' read -r status1 hash1 _ _ <<<"$row1"
            IFS=$'\t' read -r status2 hash2 _ _ <<<"$row2"
            hashes+=("$hash1")
            if [[ $status1 == 0 ]]; then
                ((passes += 1))
            else
                ((failures += 1))
            fi
            [[ $status1 == "$status2" && $hash1 == "$hash2" ]] || ((repeat_mismatches += 1))
        done < <(jq -r '.modes.chaos.seeds[]' <<<"$metadata")
        local distinct
        distinct=$(printf '%s\n' "${hashes[@]}" | LC_ALL=C sort -u | wc -l)
        if ((repeat_mismatches > 0 || distinct < min_distinct || passes < min_passes || failures < min_failures)); then
            outcome=FAIL
            reason="chaos distinct=$distinct passes=$passes failures=$failures repeat_mismatches=$repeat_mismatches"
        else
            reason="chaos distinct=$distinct passes=$passes failures=$failures; every seed reproduced"
        fi
    elif [[ $mode == custom ]]; then
        local runs repeat_identical attempt row status hash
        local failed_runs=0
        local -a hashes=()
        runs=$(jq -r '.modes.custom.assert.runs // 1' <<<"$metadata")
        repeat_identical=$(jq -r '.modes.custom.assert.repeat_identical // false' <<<"$metadata")
        for ((attempt = 1; attempt <= runs; attempt++)); do
            row=$(execute_attempt "$test" "$metadata" "$mode" "$backend" "$cell_dir" "$attempt")
            IFS=$'\t' read -r status hash _ _ <<<"$row"
            hashes+=("$hash")
            [[ $status == 0 ]] || ((failed_runs += 1))
        done
        local distinct
        distinct=$(printf '%s\n' "${hashes[@]}" | LC_ALL=C sort -u | wc -l)
        if ((failed_runs > 0)) || [[ $repeat_identical == true && $distinct != 1 ]]; then
            outcome=FAIL
            reason="custom runs=$runs failed_runs=$failed_runs distinct=$distinct"
        else
            reason="custom output identical across $runs runs"
        fi
    else
        local row status
        row=$(execute_attempt "$test" "$metadata" "$mode" "$backend" "$cell_dir" 1)
        IFS=$'\t' read -r status _ _ _ <<<"$row"
        if [[ $status != 0 ]]; then
            outcome=FAIL
            reason="$mode exited with status $status"
        fi
    fi

    end_ms=$(date +%s%3N)
    duration_ms=$((end_ms - start_ms))
    append_result "$id" "$category" "$lane" "$mode" "$backend" "$outcome" "$duration_ms" "$reason"
    printf '%-5s %-10s %-11s %-9s %s%s\n' "$outcome" "$lane" "$mode" "${backend:--}" "$id" \
        "${reason:+ - $reason}"
    [[ $outcome == PASS ]]
}

function write_junit {
    local tests failures errors
    tests=$(wc -l <"$RESULTS")
    failures=$(jq -s '[.[] | select(.outcome == "FAIL")] | length' "$RESULTS")
    errors=$(jq -s '[.[] | select(.outcome == "ERROR")] | length' "$RESULTS")
    mkdir -p "$(dirname "$JUNIT")"
    {
        printf '<?xml version="1.0" encoding="UTF-8"?>\n'
        printf '<testsuite name="hermit-e2e" tests="%s" failures="%s" errors="%s">\n' "$tests" "$failures" "$errors"
        jq -r '
            def esc: @html;
            "  <testcase classname=\"" + (.category|esc) + "\" name=\"" + ((.test + "/" + .mode + "/" + (.backend // "none"))|esc) + "\" time=\"" + ((.duration_ms / 1000)|tostring) + "\">" +
            (if .outcome == "FAIL" then ("<failure>" + ((.reason // "failed")|esc) + "</failure>")
             elif .outcome == "ERROR" then ("<error>" + ((.reason // "error")|esc) + "</error>")
             else "" end) + "</testcase>"
        ' "$RESULTS"
        printf '</testsuite>\n'
    } >"$JUNIT"
}

function run_required {
    RESULTS=${RESULTS:-$RESULT_ROOT/$RUN_ID/results.jsonl}
    JUNIT=${JUNIT:-$RESULT_ROOT/$RUN_ID/junit.xml}
    mkdir -p "$(dirname "$RESULTS")"
    : >"$RESULTS"

    local planned test_id mode backend test metadata failures=0 selected=0
    planned=$(emit_required_plan)
    while IFS=$'\t' read -r test_id mode backend; do
        [[ -n $test_id ]] || continue
        selected=$((selected + 1))
        test=${TEST_BY_ID[$test_id]}
        metadata=$(metadata_json "$test")
        run_cell "$test" "$metadata" "$mode" "$backend" || failures=$((failures + 1))
    done < <(jq -r '[.test,.mode,(.backend // "")] | @tsv' <<<"$planned")

    ((selected > 0)) || die "filters selected no required test cells"
    write_junit
    jq -s '{schema:1,tests:(map(.test)|unique|length),cells:length,
        passed:(map(select(.outcome=="PASS"))|length),
        failed:(map(select(.outcome=="FAIL"))|length),
        errors:(map(select(.outcome=="ERROR"))|length),
        by_mode:(group_by(.mode)|map({key:.[0].mode,value:{cells:length,passed:(map(select(.outcome=="PASS"))|length)}})|from_entries)}' \
        "$RESULTS" >"$(dirname "$RESULTS")/summary.json"
    echo "Results: $RESULTS"
    echo "JUnit:  $JUNIT"
    ((failures == 0))
}

subcommand=${1:-}
[[ -n $subcommand ]] || { usage; exit 2; }
shift
parse_options "$@"
load_tests

case "$subcommand" in
    validate)
        (($# == 0)) || true
        audit_inventory
        audit_ci_correspondence
        echo "PASS: ${#TESTS[@]} E2E tests have valid syntax and centralized schema-v2 manifests"
        emit_required_plan | jq -s '{tests:(map(.test)|unique|length),required_cells:length,by_mode:(group_by(.mode)|map({key:.[0].mode,value:length})|from_entries)}'
        validate_dag_correspondence
        ;;
    plan)
        print_plan required
        ;;
    run)
        [[ -x $HERMIT_BIN || $MODE_FILTER == naked ]] || die "Hermit binary is not executable: $HERMIT_BIN"
        run_required
        ;;
    audit-gaps)
        print_plan gaps
        ;;
    audit-inventory)
        audit_inventory
        ;;
    audit-ci)
        audit_ci_correspondence
        ;;
    *)
        usage
        die "unknown command: $subcommand"
        ;;
esac
