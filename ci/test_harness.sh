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
RESULT_ROOT=${E2E_RESULT_ROOT:-$ROOT_DIR/ignored/e2e}
RUN_ID=${E2E_RUN_ID:-"local-$(date +%s)-$$"}
SOURCE_TREE_SHA=$(git -C "$ROOT_DIR" rev-parse HEAD)
BUILD_ROOT=${E2E_BUILD_ROOT:-$RESULT_ROOT/build/$SOURCE_TREE_SHA}
DAG_ROOT=${E2E_DAG_ROOT:-$ROOT_DIR/ci/dag}
if [[ -n $(git -C "$ROOT_DIR" status --porcelain --untracked-files=no) ]]; then
    SOURCE_TREE_DIRTY=true
else
    SOURCE_TREE_DIRTY=false
fi

readonly ROOT_DIR TEST_ROOT MANIFEST_ROOT INVENTORY EXPECTED_PLAN HERMIT_BIN RESULT_ROOT RUN_ID SOURCE_TREE_SHA SOURCE_TREE_DIRTY BUILD_ROOT DAG_ROOT
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
  ci/test_harness.sh build [filters]
  ci/test_harness.sh run [filters] [--results PATH] [--junit PATH]
  ci/test_harness.sh audit-gaps [--lane portable|privileged] [--format text|json]
  ci/test_harness.sh audit-inventory
  ci/test_harness.sh audit-test-footprints
  ci/test_harness.sh audit-ci

Filters:
  --lane LANE             portable or privileged
  --mode MODE             verify, chaos, replay, naked, or custom
  --backend BACKEND       ptrace, dbi, kvm, sabre, or liteinst
  --category CATEGORY     manifest category
  --test ID               exact category/test ID
  --ci-only               select only cells explicitly marked ci=true
  --prebuilt              require artifacts produced by the build command
  --allow-empty           permit an empty selection (DAG build/bucket nodes only)
  --include-occasional    include tests marked occasional
  --include-manual        include a ci=false cell; requires exact --test and --mode
  --probe-disabled        run one explicitly disabled backend cell; requires exact
                          --test, --mode, and --backend filters (run only)

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
            (if (has("direct") and (.direct | type == "array")) then "direct-argv"
             elif has("direct") then "direct"
             elif (.program | endswith(".sh")) then "shell"
             elif (.program | endswith(".c")) then "c"
             elif (.program | endswith(".rs")) then "rust"
             else error("unsupported program kind") end),
          program_path: (.program // ""),
          direct_command: (if ((.direct // null) | type) == "string" then .direct else "" end),
          direct_argv: (if ((.direct // null) | type) == "array" then .direct else [] end),
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

function audit_test_footprints {
    cargo run --quiet -p hermit-manifest-plan \
        --bin generate-test-footprints -- --check ||
        die "ci/test-footprints.json is stale relative to Cargo metadata, the portable DAG, or footprint policy"
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

function workflow_job_timeout_minutes {
    local workflow=$1 job=$2
    awk -v start="  $job:" '
        $0 == start { inside = 1; next }
        inside && /^  [a-zA-Z0-9_-]+:/ { exit }
        inside && /^    timeout-minutes: [0-9]+$/ { print $2; exit }
    ' "$workflow"
}

function assert_parallel_portable_workflow {
    local workflow=$1
    local run_dag_count run_node_count debug_inner_path release_inner_path
    local debug_outer_minutes release_outer_minutes
    local hosted_job_overhead_seconds=300
    run_dag_count=$(grep -Ec '^[[:space:]]+run: .*ci/run-dag[.]sh portable([[:space:]]|$)' "$workflow" || true)
    run_node_count=$(grep -Ec '^[[:space:]]+run: .*ci/run-node[.]sh portable([[:space:]]|$)' "$workflow" || true)

    ((run_dag_count == 0)) ||
        die "GitHub portable workflow must not retain the serial ci/run-dag.sh entrypoint"
    ((run_node_count == 5)) ||
        die "GitHub portable workflow must have five audited ci/run-node.sh entrypoints"

    # The hosted job is the outer kill boundary. Compute the critical path for
    # each exact run-node selection, retaining dependencies among selected nodes
    # just as safe-ci --only does. A max-single-node proxy misses release's
    # runtime_release -> liteinst_runtime_release chain.
    debug_inner_path=$(jq --slurpfile shards "$ROOT_DIR/ci/portable-shards.json" '
        ($shards[0].build_debug_nodes) as $selected
        | (.steps | map({key:(.group + "." + .job), value:.}) | from_entries) as $steps
        | def critical($id):
            $steps[$id] as $step
            | ($step.timeout + ([($step.deps // [])[] as $dep
                | select($selected | index($dep) != null)
                | critical($dep)] | max // 0));
          [$selected[] | critical(.)] | max
    ' "$DAG_ROOT/portable.json")
    release_inner_path=$(jq --slurpfile shards "$ROOT_DIR/ci/portable-shards.json" '
        ($shards[0].build_dbi_nodes + $shards[0].build_aux_nodes) as $selected
        | (.steps | map({key:(.group + "." + .job), value:.}) | from_entries) as $steps
        | def critical($id):
            $steps[$id] as $step
            | ($step.timeout + ([($step.deps // [])[] as $dep
                | select($selected | index($dep) != null)
                | critical($dep)] | max // 0));
          [$selected[] | critical(.)] | max
    ' "$DAG_ROOT/portable.json")
    debug_outer_minutes=$(workflow_job_timeout_minutes "$workflow" build-debug)
    release_outer_minutes=$(workflow_job_timeout_minutes "$workflow" build-release)
    [[ $debug_inner_path =~ ^[1-9][0-9]*$ ]] ||
        die "portable debug selection has no numeric critical path"
    [[ $release_inner_path =~ ^[1-9][0-9]*$ ]] ||
        die "portable release selection has no numeric critical path"
    [[ $debug_outer_minutes =~ ^[1-9][0-9]*$ ]] ||
        die "GitHub debug build has no numeric outer timeout"
    [[ $release_outer_minutes =~ ^[1-9][0-9]*$ ]] ||
        die "GitHub release build has no numeric outer timeout"
    ((debug_outer_minutes * 60 > debug_inner_path + hosted_job_overhead_seconds)) ||
        die "GitHub debug outer timeout must exceed ${debug_inner_path}s selected path plus ${hosted_job_overhead_seconds}s overhead"
    ((release_outer_minutes * 60 > release_inner_path + hosted_job_overhead_seconds)) ||
        die "GitHub release outer timeout must exceed ${release_inner_path}s selected path plus ${hosted_job_overhead_seconds}s overhead"
    [[ $(grep -Fxc '        run: ./ci/check-shard-coverage.sh' "$workflow") == 1 ]] ||
        die "GitHub portable workflow must run the shard-coverage guard exactly once"
    # Match the literal command embedded in workflow YAML.
    # shellcheck disable=SC2016
    [[ $(grep -Fxc '          plan=$(./ci/test_harness.sh plan --lane portable --ci-only --format json)' "$workflow") == 1 ]] ||
        die "GitHub portable workflow must derive one e2e matrix from the audited plan"
    [[ $(grep -Fxc '    name: Regular tests (GitHub-managed portable)' "$workflow") == 1 ]] ||
        die "GitHub portable workflow must expose exactly one stable aggregate gate"
    [[ $(grep -Fxc '  merge_group:' "$workflow") == 1 ]] ||
        die "GitHub portable workflow must run against merge-queue commits"
    [[ $(grep -Fxc '            target/install_pkg/rsrcs/libdetcore_dbi.so \' "$workflow") == 1 ]] ||
        die "GitHub portable debug artifact must preserve the installed DBI runtime"
    [[ $(grep -Fxc '          test -f target/install_pkg/rsrcs/libdetcore_dbi.so' "$workflow") == 1 ]] ||
        die "GitHub portable debug shards must verify the installed DBI runtime"
    [[ $(grep -Fxc '          test -f target/debug/deps/libdetcore_dbi.so' "$workflow") == 2 ]] ||
        die "GitHub portable workflow must package and verify the debug DBI cdylib"
    [[ $(grep -Fxc '            target/ci \' "$workflow") == 1 ]] ||
        die "GitHub portable release artifact must transport the strict-compat Hermit"
    [[ $(grep -Fxc '          test -x target/ci/hermit-strict' "$workflow") == 1 ]] ||
        die "GitHub portable debug shards must verify the strict-compat Hermit"
    # Both the debug test shards (run_dbi_* CLI tests) and the e2e backend cells
    # consume the DBI install package built by build-release, so both must wait on
    # [select, build-debug, build-release]. (select gates the affected-test matrix;
    # dropping build-release from either would race the DBI runtime.)
    [[ $(grep -Fxc '    needs: [select, build-debug, build-release]' "$workflow") == 2 ]] ||
        die "GitHub portable debug and e2e shards must wait for the complete DBI install package"
    [[ $(grep -Fxc '          test -x target/install_pkg/rsrcs/dynamorio/bin64/drrun' "$workflow") == 1 ]] ||
        die "GitHub portable debug shards must verify the DynamoRIO launcher"
    [[ $(grep -Fxc '          test -f target/install_pkg/rsrcs/libreverie_dbi_client.so' "$workflow") == 1 ]] ||
        die "GitHub portable debug shards must verify the DynamoRIO client"
    [[ $(grep -Fxc '      - name: Enable unprivileged user and mount namespaces' "$workflow") == 4 ]] ||
        die "GitHub portable debug, release, e2e, and SaBRe diagnostics must enable user namespaces"
    [[ $(grep -Fxc '            sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0' "$workflow") == 4 ]] ||
        die "GitHub portable test shards must lift AppArmor's user-namespace restriction"
    [[ $(grep -Fxc '    needs: [test-debug, test-release, e2e, sabre_non_gated_parity, regular]' "$workflow") == 1 ]] ||
        die "GitHub portable artifact cleanup must wait for every test consumer"
    [[ $(grep -Fxc '  sabre_non_gated_parity:' "$workflow") == 1 ]] ||
        die "GitHub portable workflow must retain the SaBRe non-gating diagnostic job"
    [[ $(grep -Fxc '    continue-on-error: true' "$workflow") == 1 ]] ||
        die "SaBRe exec diagnostics must remain explicitly non-gating"
    [[ $(grep -Fxc '          probe data-handling/archive-roundtrip' "$workflow") == 1 ]] ||
        die "SaBRe diagnostics must probe archive-roundtrip"
    [[ $(grep -Fxc '          probe system-utils/date-nanoseconds' "$workflow") == 1 ]] ||
        die "SaBRe diagnostics must probe date-nanoseconds"
    [[ $(grep -Fxc '          probe system-utils/random-device' "$workflow") == 1 ]] ||
        die "SaBRe diagnostics must probe random-device"
    [[ $(grep -Fc -- '--mode verify --backend sabre --probe-disabled' "$workflow") == 1 ]] ||
        die "SaBRe diagnostics must execute disabled cells through the harness"
    jq -e '
        .debug_shards[]
        | select(.slug == "integration")
        | .nodes | index("test.cli") != null
    ' "$ROOT_DIR/ci/portable-shards.json" >/dev/null ||
        die "GitHub portable integration shard must retain the run_dbi_* CLI tests"
}

function assert_privileged_diagnostics {
    local workflow=$1
    [[ $(grep -Fxc '    - name: Run non-gating occasional KVM probes' "$workflow") == 1 ]] ||
        die "GitHub privileged workflow must run the occasional KVM diagnostics"
    [[ $(grep -Fc -- '--ci-only --include-occasional' "$workflow") == 2 ]] ||
        die "GitHub privileged workflow must build and run occasional KVM cells"
    [[ $(grep -Fxc '      continue-on-error: true' "$workflow") == 1 ]] ||
        die "GitHub occasional KVM diagnostics must remain explicitly non-gating"
    [[ $(grep -Fxc '              --results ignored/e2e/privileged/occasional/results.jsonl \' "$workflow") == 1 ]] ||
        die "GitHub occasional KVM diagnostics must publish structured results"
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

# Keep the latest-Reverie invariant attached to every testing evidence path.
# The checker unit tests plant stale/current pins; these structural assertions
# prove that those same fail-closed semantics cannot be bypassed by selecting a
# different local, DAG, hosted-CI, merge-gate, or receipt path.
function assert_reverie_pin_enforcement {
    local checker="$ROOT_DIR/scripts/check-reverie-pin.rs"
    local runner="$ROOT_DIR/ci/run-reverie-pin-check.sh"
    local liteinst_stage="$ROOT_DIR/scripts/stage-liteinst-runtime.sh"
    grep -Fq '.args(["ls-remote", "--exit-code", remote, MAIN_REF])' "$checker" ||
        die "latest-Reverie checker must dereference refs/heads/main with git ls-remote"
    ! grep -Fq 'main_sha' "$checker" ||
        die "latest-Reverie checker must not accept a pre-recorded main SHA"
    ! grep -Fq -- '--reverie-remote' "$checker" ||
        die "production callers must not redirect the latest-Reverie authority"
    [[ -x $runner ]] ||
        die "latest-Reverie CI runner must be executable"
    [[ $(grep -Fxc '"$checker" "$@"' "$runner") == 1 ]] ||
        die "latest-Reverie runner must forward every verifier argument exactly"

    # Exhaustive tracked-reference audit. Any new direct source reference fails
    # until it is classified in this explicit trusted allowlist; checking a few
    # known callers would let a new bypass escape unnoticed.
    local direct_references expected_direct_references
    direct_references=$(
        git -C "$ROOT_DIR" grep -Il -F 'scripts/check-reverie-pin.rs' -- . |
            LC_ALL=C sort
    )
    expected_direct_references=$'.github/workflows/merge-gate.yml\nci/run-reverie-pin-check.sh\nci/test_harness.sh\ndocs/updating-reverie.md\nscripts/check-nested-lockfiles.rs\nscripts/check-reverie-pin.rs'
    [[ $direct_references == "$expected_direct_references" ]] ||
        die "direct Reverie-pin source references differ from the trusted allowlist:
$direct_references"

    # Executable source consumers are limited to the portable rustc launcher
    # and merge-gate's trusted-main compiler. The harness is this audit; the
    # remaining allowlisted references are documentation or checker source.
    [[ $(grep -Fxc '        scripts/check-reverie-pin.rs -o "$checker"' "$runner") == 2 ]] ||
        die "latest-Reverie runner must compile the canonical source in both modes"
    [[ $(grep -Fxc $'\t$(SUBMODULE_PROXY) ./ci/run-reverie-pin-check.sh' "$ROOT_DIR/Makefile") == 1 ]] ||
        die "Makefile lint must use the canonical Reverie-pin launcher"
    [[ $(grep -Fxc 'checker="$root/ci/run-reverie-pin-check.sh"' "$ROOT_DIR/.githooks/pre-commit") == 1 ]] ||
        die "pre-commit hook must use the canonical Reverie-pin launcher"
    [[ $(grep -Fxc '"${proxy[@]}" "$checker" --repo "$root" || exit 1' "$ROOT_DIR/.githooks/pre-commit") == 1 ]] ||
        die "pre-commit hook must bind the launcher to the exact repository"
    [[ $(grep -Fc '"$ROOT_DIR/ci/run-reverie-pin-check.sh" --repo "$ROOT_DIR"' "$ROOT_DIR/validate.sh") == 2 ]] ||
        die "both validate Reverie-pin gates must use the exact-repository launcher"
    [[ $(grep -Fxc '    "$root_dir/ci/run-reverie-pin-check.sh" --repo "$root_dir" --print-pin' "$liteinst_stage") == 1 ]] ||
        die "LiteInst staging must obtain its cache pin through the exact-repository launcher"

    local dag
    for dag in "$DAG_ROOT/portable.json" "$DAG_ROOT/privileged.json"; do
        jq -e '
            [.steps[] | select(
                .group == "check"
                and .job == "reverie_pin"
                and .cmd == "./ci/run-reverie-pin-check.sh"
            )] | length == 1
        ' "$dag" >/dev/null ||
            die "${dag#"$ROOT_DIR/"} must contain exactly one latest-Reverie pin gate"
    done

    # Execute the same rustc wrapper with a PATH that deliberately excludes
    # rust-script. A fake git transport keeps both brackets hermetic while the
    # production checker still dereferences refs/heads/main and scans a real Git
    # fixture. The positive arm proves the path fires; the planted stale pin
    # must return the checker's typed policy refusal (1), not a compile/no-result
    # error (2).
    (
        local scratch isolated_path race_path shared_compile_dir fixture
        local real_git current stale status
        local fake_cargo staged_runtime direct_status
        local allowed_cpu parallel_index parallel_failures shared_failures
        local -a checker_pids
        scratch=$(mktemp -d)
        trap 'rm -rf -- "$scratch"' EXIT
        isolated_path="$scratch/bin"
        fixture="$scratch/hermit"
        real_git=$(command -v git)
        current=0123456789abcdef0123456789abcdef01234567
        stale=89abcdef0123456789abcdef0123456789abcdef
        mkdir -p "$isolated_path" "$fixture"
        ln -s "$(command -v rustc)" "$isolated_path/rustc"
        if PATH="$isolated_path:/usr/bin:/bin" command -v rust-script >/dev/null; then
            die "rust-script unexpectedly present in isolated checker PATH"
        fi
        PATH="$isolated_path:/usr/bin:/bin" "$runner" --self-test >/dev/null

        # The pin gate and both DBI build children may compile the checker at
        # once in one worktree. Exercise real rustc concurrently within the
        # e2e.metadata node's 1 GiB cap. Pin each compiler to one allowed CPU so
        # rustc cannot infer the 316-CPU host and create hundreds of codegen
        # threads; filesystem concurrency remains real and deterministic.
        allowed_cpu=$(taskset -pc "$$" | sed -E 's/.*: ([0-9]+).*/\1/')
        [[ $allowed_cpu =~ ^[0-9]+$ ]] ||
            die "could not determine one allowed CPU for the checker race bracket"
        checker_pids=()
        for parallel_index in 1 2 3 4; do
            taskset -c "$allowed_cpu" "$runner" --self-test \
                >"$scratch/parallel-checker-$parallel_index.log" 2>&1 &
            checker_pids+=("$!")
        done
        parallel_failures=0
        for parallel_index in "${!checker_pids[@]}"; do
            if ! wait "${checker_pids[$parallel_index]}"; then
                cat "$scratch/parallel-checker-$((parallel_index + 1)).log" >&2
                parallel_failures=$((parallel_failures + 1))
            fi
        done
        ((parallel_failures == 0)) ||
            die "$parallel_failures of 4 concurrent real Reverie-pin checker builds failed"

        # Bracket the shared-intermediate failure deterministically at higher
        # concurrency without making twelve rustc processes exceed that cap. The
        # stand-in writes one crate-named intermediate beside `-o`, exactly the
        # path class that collides when outputs share a directory. First prove a
        # planted shared directory fails, then send twelve invocations through
        # the production launcher and require its private directories to pass.
        race_path="$scratch/race-bin"
        shared_compile_dir="$scratch/shared-compile-dir"
        mkdir -p "$race_path" "$shared_compile_dir"
        cat >"$race_path/rustc" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
output=
while (($# > 0)); do
    if [[ $1 == -o ]]; then
        output=$2
        shift 2
    else
        shift
    fi
done
[[ -n $output ]]
intermediate="$(dirname -- "$output")/check_reverie_pin.rcgu.o"
if ! (set -o noclobber; : >"$intermediate") 2>/dev/null; then
    echo "planted shared rustc intermediate collision: $intermediate" >&2
    exit 97
fi
sleep 0.2
printf '#!/usr/bin/env bash\nexit 0\n' >"$output"
chmod +x "$output"
rm -f -- "$intermediate"
EOF
        chmod +x "$race_path/rustc"
        checker_pids=()
        for parallel_index in $(seq 1 12); do
            PATH="$race_path:/usr/bin:/bin" rustc --edition=2021 --test \
                scripts/check-reverie-pin.rs \
                -o "$shared_compile_dir/checker-$parallel_index" \
                >"$scratch/shared-checker-$parallel_index.log" 2>&1 &
            checker_pids+=("$!")
        done
        shared_failures=0
        for parallel_index in "${!checker_pids[@]}"; do
            if ! wait "${checker_pids[$parallel_index]}"; then
                shared_failures=$((shared_failures + 1))
            fi
        done
        ((shared_failures > 0)) ||
            die "planted shared-directory compiler race was inert"

        checker_pids=()
        for parallel_index in $(seq 1 12); do
            PATH="$race_path:/usr/bin:/bin" "$runner" --self-test \
                >"$scratch/isolated-checker-$parallel_index.log" 2>&1 &
            checker_pids+=("$!")
        done
        parallel_failures=0
        for parallel_index in "${!checker_pids[@]}"; do
            if ! wait "${checker_pids[$parallel_index]}"; then
                cat "$scratch/isolated-checker-$((parallel_index + 1)).log" >&2
                parallel_failures=$((parallel_failures + 1))
            fi
        done
        ((parallel_failures == 0)) ||
            die "$parallel_failures of 12 private-directory compiler probes failed"

        cat >"$isolated_path/git" <<EOF
#!/usr/bin/env bash
if [[ \${1:-} == ls-remote ]]; then
    printf '%s\trefs/heads/main\n' '$current'
    exit 0
fi
exec '$real_git' "\$@"
EOF
        chmod +x "$isolated_path/git"
        "$real_git" -C "$fixture" init -q
        printf '[dependencies]\nreverie = { git = "https://github.com/rrnewton/reverie.git", rev = "%s" }\n' \
            "$current" >"$fixture/Cargo.toml"
        "$real_git" -C "$fixture" add Cargo.toml
        PATH="$isolated_path:/usr/bin:/bin" "$runner" --repo "$fixture" >/dev/null

        printf '[dependencies]\nreverie = { git = "https://github.com/rrnewton/reverie.git", rev = "%s" }\n' \
            "$stale" >"$fixture/Cargo.toml"
        if PATH="$isolated_path:/usr/bin:/bin" "$runner" --repo "$fixture" \
            >/dev/null 2>&1; then
            status=0
        else
            status=$?
        fi
        [[ $status == 1 ]] ||
            die "rustc checker stale-pin bracket returned $status instead of 1"

        # Reproduce the hosted release-build failure signature, then exercise
        # the real LiteInst staging script through the canonical launcher. A
        # tiny Cargo stand-in isolates this regression to staging/launcher
        # behavior while still requiring the production script to atomically
        # install a non-empty runtime under a PATH with no rust-script.
        if PATH="$isolated_path:/usr/local/bin:/usr/bin:/bin" \
            "$checker" --repo "$ROOT_DIR" --print-pin >/dev/null 2>&1; then
            direct_status=0
        else
            direct_status=$?
        fi
        [[ $direct_status == 127 ]] ||
            die "direct LiteInst pin source negative returned $direct_status instead of hosted rc=127"
        fake_cargo="$scratch/fake-cargo"
        staged_runtime="$scratch/staged/libreverie_liteinst.so"
        cat >"$fake_cargo" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
[[ ${1:-} == build ]]
[[ -n ${HERMIT_LITEINST_STAGE:-} ]]
printf 'hermetic-liteinst-runtime\n' >"$HERMIT_LITEINST_STAGE"
EOF
        chmod +x "$fake_cargo"
        PATH="$isolated_path:/usr/local/bin:/usr/bin:/bin" CARGO="$fake_cargo" \
            "$liteinst_stage" dev "$staged_runtime" "$scratch/runtime-target"
        [[ -s $staged_runtime && $(<"$staged_runtime") == hermetic-liteinst-runtime ]] ||
            die "rust-script-free LiteInst staging did not install the fixture runtime"
    )

    [[ $(grep -Fc 'run_check "Reverie dependency pin equals latest main"' "$ROOT_DIR/validate.sh") == 1 ]] ||
        die "validate.sh must execute the latest-Reverie gate exactly once"
    [[ $(grep -Fc 'REVERIE_PIN_GATE_PASSED != 1' "$ROOT_DIR/validate.sh") == 1 ]] ||
        die "validate.sh receipt cleanup must fail closed when the pin gate was bypassed"
    [[ $(grep -Fc '\"reverie_pin_current\"' "$ROOT_DIR/validate.sh") == 1 ]] ||
        die "validate.sh receipts must state whether the latest-Reverie gate passed"

    local portable_workflow="$ROOT_DIR/.github/workflows/ci-portable.yml"
    [[ $(grep -Fxc '    name: Reverie pin is latest main' "$portable_workflow") == 1 ]] ||
        die "portable CI must expose exactly one latest-Reverie job"
    [[ $(grep -Fxc '      - reverie-pin' "$portable_workflow") == 1 ]] ||
        die "the authoritative portable aggregate must depend on the Reverie pin job"
    [[ $(grep -Fxc '          ./ci/run-reverie-pin-check.sh --self-test' "$portable_workflow") == 1 ]] ||
        die "portable CI must execute the canonical checker self-tests through rustc"
    [[ $(grep -Fxc '          ./ci/run-reverie-pin-check.sh' "$portable_workflow") == 1 ]] ||
        die "portable CI must execute the canonical live-query checker through rustc"
    ! grep -Fq 'Stale-Reverie-Pin-Reason' "$portable_workflow" ||
        die "portable CI must not retain a stale-Reverie override"

    local merge_workflow="$ROOT_DIR/.github/workflows/merge-gate.yml"
    [[ $(grep -Fxc '    name: reverie-pin-is-latest-main' "$merge_workflow") == 1 ]] ||
        die "merge-gate must check exact PR heads with the trusted pin checker"
    [[ $(grep -Fxc '    needs: [invalidate-local-validation, core-review-protocol, reverie-pin]' "$merge_workflow") == 1 ]] ||
        die "merge-gate must depend on its exact-head Reverie pin job"
    grep -Fq 'trusted/scripts/check-reverie-pin.rs -o "$checker"' "$merge_workflow" ||
        die "merge-gate must compile the checker from trusted main"
    grep -Fq 'git -C trusted worktree add --quiet --detach "$checkout" "$head_sha"' "$merge_workflow" ||
        die "merge-gate must inspect the exact PR head"
    grep -Fq 'with-proxy "$checker" --repo "$checkout"' "$merge_workflow" ||
        die "merge-gate must run the canonical live-query checker on the exact PR head"
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

function emit_manifest_buckets {
    local test
    for test in "${TESTS[@]}"; do
        metadata_json "$test" | jq -c '{lane,category}'
    done | jq -sS 'unique | sort_by(.lane,.category)'
}

function audit_ci_correspondence {
    local lane dag

    # Both DAG launch surfaces use the explicit ordinary-launcher mode; the DBI
    # wrapper is the sole child-budget caller.
    # shellcheck disable=SC2016
    [[ $(grep -Fxc 'source "$ROOT_DIR/ci/configure-build-jobs.sh" launcher || exit $?' "$ROOT_DIR/ci/run-dag.sh") == 1 ]] ||
        die "run-dag.sh must source ordinary build-job configuration exactly once"
    # shellcheck disable=SC2016
    [[ $(grep -Fxc 'source "$ROOT_DIR/ci/configure-build-jobs.sh" launcher || exit $?' "$ROOT_DIR/ci/run-node.sh") == 1 ]] ||
        die "run-node.sh must source ordinary build-job configuration exactly once"
    local budget_config="$ROOT_DIR/ci/configure-build-jobs.sh"
    local budget_wrapper="$ROOT_DIR/ci/run-with-reverie-dbi-budget.sh"
    [[ -x $budget_wrapper ]] || die "DBI child-budget wrapper must be executable"
    [[ $(grep -Fc 'reverie-dbi-budget=portable-build-child-only' "$ROOT_DIR/ci/run-dag.sh") == 1 ]] ||
        die "run-dag.sh must identify the DBI budget as portable-child-only"
    [[ $(grep -Fc 'reverie-dbi-budget=portable-build-child-only' "$ROOT_DIR/ci/run-node.sh") == 1 ]] ||
        die "run-node.sh must identify the DBI budget as portable-child-only"
    [[ $(grep -Fxc 'source "$ROOT_DIR/ci/configure-build-jobs.sh" reverie-dbi-budget-child' "$budget_wrapper") == 1 ]] ||
        die "DBI wrapper must select the explicit portable child-budget mode"
    [[ $(grep -Fxc '    "$ROOT_DIR/ci/run-reverie-pin-check.sh" --repo "$ROOT_DIR" --print-pin' "$budget_wrapper") == 1 ]] ||
        die "DBI wrapper must bind its calibration through the canonical local-pin verifier"
    [[ $(grep -Fc '0ae0c01b5e4c9fbf85c97adc66c2740f280727df' "$budget_wrapper") == 1 ]] ||
        die "DBI wrapper must name exactly one calibrated Reverie pin"
    [[ $(grep -Fc '0ae0c01b5e4c9fbf85c97adc66c2740f280727df' "$budget_config") == 2 ]] ||
        die "DBI derivation must independently require and diagnose the calibrated Reverie pin"
    # shellcheck disable=SC2016
    local budget_record='reverie-dbi-budget={pin:$REVERIE_DBI_BUDGET_BOUND_PIN,source:$REVERIE_DBI_BUILD_JOBS_SOURCE,raw-build-jobs:$REVERIE_DBI_RAW_BUILD_JOBS,effective-cpus-source:$REVERIE_DBI_EFFECTIVE_CPUS_SOURCE,effective-cpus:$REVERIE_DBI_EFFECTIVE_CPUS,reverie-max-jobs:$REVERIE_DBI_MAX_PARALLEL_JOBS,effective-native-jobs:$REVERIE_DBI_EFFECTIVE_BUILD_JOBS,effective-job-seconds:$REVERIE_DBI_MAX_BUILD_EFFECTIVE_JOB_SECONDS,max-elapsed-seconds:$REVERIE_DBI_MAX_BUILD_SECONDS,basis:github-portable-cold-miss-n3-affinity4,carried-to-pin-on-dynamorio-recipe-key:76403e8e76b128119be4a7192893b7ec3084aeb85f4bd0377198a538d94b2a1d}'
    [[ $(grep -Fc "$budget_record" "$budget_wrapper") == 1 ]] ||
        die "DBI child wrapper must log the pin and every derivation condition"

    jq -e '
        [.steps[] | select(
            .group == "build"
            and (.job == "workspace" or .job == "runtime_release")
            and (.cmd | contains("./ci/run-with-reverie-dbi-budget.sh cargo build"))
            and .timeout >= 1200
        )] | length == 2
    ' "$DAG_ROOT/portable.json" >/dev/null ||
        die "portable DBI builds must derive inside the child and allow 1050s DBI + 150s overhead"
    jq -e '
        [.steps[] | select(
            .group == "build" and .job == "privileged_tests"
            and .timeout == 120
            and .cmd == "CARGO_BUILD_JOBS=8 cargo build -p hermit --features third-party-backends --bin hermit && CARGO_BUILD_JOBS=8 cargo test -p hermit-detcore --test tests_misc --no-run"
        )] | length == 1
    ' "$DAG_ROOT/privileged.json" >/dev/null ||
        die "portable-only DBI override must not alter the privileged command or timeout"

    (
        local scratch fake_runner privileged_env privileged_node_env name
        local budget_probe budget_tuple clamp_boundaries cpu_boundaries
        local hosted_wrapper_log boxed_wrapper_log wrong_pin_log wrong_pin_status
        local configured_jobs fixture wrong_pin
        local -a clean_budget_env budget_names planted_budget_env
        scratch=$(mktemp -d)
        trap 'rm -rf -- "$scratch"' EXIT

        for cpu_count in 2 4 64; do
            mkdir -p "$scratch/nproc-$cpu_count"
            printf '#!/usr/bin/env bash\nprintf "%%s\\n" "%s"\n' "$cpu_count" >"$scratch/nproc-$cpu_count/nproc"
            chmod +x "$scratch/nproc-$cpu_count/nproc"
        done
        mkdir -p "$scratch/nproc-zero" "$scratch/nproc-invalid"
        printf '#!/usr/bin/env bash\nprintf "0\\n"\n' >"$scratch/nproc-zero/nproc"
        printf '#!/usr/bin/env bash\nprintf "invalid\\n"\n' >"$scratch/nproc-invalid/nproc"
        chmod +x "$scratch/nproc-zero/nproc" "$scratch/nproc-invalid/nproc"

        budget_names=(
            REVERIE_DBI_BUDGET_BOUND_PIN
            REVERIE_DBI_BUILD_JOBS_SOURCE
            REVERIE_DBI_RAW_BUILD_JOBS
            REVERIE_DBI_EFFECTIVE_CPUS_SOURCE
            REVERIE_DBI_EFFECTIVE_CPUS
            REVERIE_DBI_MAX_PARALLEL_JOBS
            REVERIE_DBI_EFFECTIVE_BUILD_JOBS
            REVERIE_DBI_MAX_BUILD_EFFECTIVE_JOB_SECONDS
            REVERIE_DBI_MAX_BUILD_SECONDS
            CI_DAG_LAUNCH_WIDTH_BOUND
            CI_DAG_LAUNCH_BUILD_JOBS_SOURCE
            CI_DAG_LAUNCH_RAW_BUILD_JOBS
            CI_DAG_EFFECTIVE_CPUS
            CI_DAG_REVERIE_DBI_MAX_PARALLEL_JOBS
            CI_DAG_REVERIE_DBI_MAX_BUILD_JOB_SECONDS
            CI_DAG_REVERIE_DBI_MAX_BUILD_EFFECTIVE_JOB_SECONDS
            REVERIE_DBI_PINNED_MAX_PARALLEL_JOBS
            REVERIE_DBI_BUDGET_CHILD
        )
        clean_budget_env=(env -u CARGO_BUILD_JOBS -u THIRD_PARTY_BUILD_JOBS -u SAFE_CI_IN_SCOPE)
        for name in "${budget_names[@]}"; do
            clean_budget_env+=(-u "$name")
            planted_budget_env+=("$name=planted")
        done

        # Exercise the real privileged launcher with every current and retired
        # budget variable planted. The fake runner observes exactly what the DAG
        # engine would inherit; none of the portable provenance may survive.
        fake_runner="$scratch/observe-runner"
        printf '#!/usr/bin/env bash\nenv | LC_ALL=C sort\n' >"$fake_runner"
        chmod +x "$fake_runner"
        privileged_env=$(
            env "${planted_budget_env[@]}" CI_DAG_BUILD_JOBS=8 \
                SAFE_CI_DAG_RUNNER="$fake_runner" \
                "$ROOT_DIR/ci/run-dag.sh" privileged ascii 2>/dev/null
        )
        for name in "${budget_names[@]}"; do
            ! grep -q "^${name}=" <<<"$privileged_env" ||
                die "privileged DAG runner inherited portable DBI variable $name"
        done
        grep -Fxq 'CARGO_BUILD_JOBS=8' <<<"$privileged_env" ||
            die "privileged DAG runner lost the historical Cargo width"
        grep -Fxq 'THIRD_PARTY_BUILD_JOBS=8' <<<"$privileged_env" ||
            die "privileged DAG runner lost the historical native-build width"
        privileged_node_env=$(
            env "${planted_budget_env[@]}" CI_DAG_BUILD_JOBS=8 \
                SAFE_CI_DAG_RUNNER="$fake_runner" RUN_NODE_PERF_DIR="$scratch/perf" \
                "$ROOT_DIR/ci/run-node.sh" privileged build.privileged_tests 2>/dev/null
        )
        for name in "${budget_names[@]}"; do
            ! grep -q "^${name}=" <<<"$privileged_node_env" ||
                die "privileged node runner inherited portable DBI variable $name"
        done
        grep -Fxq 'CARGO_BUILD_JOBS=8' <<<"$privileged_node_env" ||
            die "privileged node runner lost the historical Cargo width"
        grep -Fxq 'THIRD_PARTY_BUILD_JOBS=8' <<<"$privileged_node_env" ||
            die "privileged node runner lost the historical native-build width"

        configured_jobs=$(
            "${clean_budget_env[@]}" CI_DAG_BUILD_JOBS=5 \
                bash -c 'source "$1" launcher; printf "%s %s\n" "$CARGO_BUILD_JOBS" "$THIRD_PARTY_BUILD_JOBS"' \
                _ "$budget_config"
        )
        [[ $configured_jobs == '5 5' ]] ||
            die "ordinary launcher did not propagate mutation K=5: $configured_jobs"

        # All budget arithmetic uses a fake `nproc`, not an input variable. This
        # brackets the production observation point at the child process itself.
        # shellcheck disable=SC2016
        budget_probe='source "$1" reverie-dbi-budget-child; printf "%s %s %s %s %s %s %s %s %s %s\n" "$REVERIE_DBI_BUILD_JOBS_SOURCE" "$REVERIE_DBI_RAW_BUILD_JOBS" "$CARGO_BUILD_JOBS" "$THIRD_PARTY_BUILD_JOBS" "$REVERIE_DBI_EFFECTIVE_CPUS_SOURCE" "$REVERIE_DBI_EFFECTIVE_CPUS" "$REVERIE_DBI_MAX_PARALLEL_JOBS" "$REVERIE_DBI_EFFECTIVE_BUILD_JOBS" "$REVERIE_DBI_MAX_BUILD_EFFECTIVE_JOB_SECONDS" "$REVERIE_DBI_MAX_BUILD_SECONDS"'
        budget_tuple=$(
            PATH="$scratch/nproc-4:$PATH" "${clean_budget_env[@]}" \
                REVERIE_DBI_BUDGET_BOUND_PIN=0ae0c01b5e4c9fbf85c97adc66c2740f280727df \
                CARGO_BUILD_JOBS=8 bash -c "$budget_probe" _ "$budget_config"
        )
        [[ $budget_tuple == 'inherited-launch-cargo-build-jobs 8 8 8 child-nproc 4 16 4 1050 263' ]] ||
            die "hosted j8/child-CPU4 budget tuple drifted: $budget_tuple"
        budget_tuple=$(
            PATH="$scratch/nproc-64:$PATH" "${clean_budget_env[@]}" \
                REVERIE_DBI_BUDGET_BOUND_PIN=0ae0c01b5e4c9fbf85c97adc66c2740f280727df \
                SAFE_CI_IN_SCOPE=1 CARGO_BUILD_JOBS=32 \
                bash -c "$budget_probe" _ "$budget_config"
        )
        [[ $budget_tuple == 'runner-child-cargo-build-jobs 32 32 32 child-nproc 64 16 16 1050 66' ]] ||
            die "boxed j32/child-CPU64 budget tuple drifted: $budget_tuple"

        hosted_wrapper_log=$(
            PATH="$scratch/nproc-4:$PATH" "${clean_budget_env[@]}" \
                CARGO_BUILD_JOBS=8 "$budget_wrapper" true 2>&1
        )
        [[ $hosted_wrapper_log == *'pin:0ae0c01b5e4c9fbf85c97adc66c2740f280727df,source:inherited-launch-cargo-build-jobs,raw-build-jobs:8,effective-cpus-source:child-nproc,effective-cpus:4,reverie-max-jobs:16,effective-native-jobs:4,effective-job-seconds:1050,max-elapsed-seconds:263'* ]] ||
            die "production wrapper did not log the bound hosted tuple: $hosted_wrapper_log"
        boxed_wrapper_log=$(
            PATH="$scratch/nproc-64:$PATH" "${clean_budget_env[@]}" \
                SAFE_CI_IN_SCOPE=1 CARGO_BUILD_JOBS=32 "$budget_wrapper" true 2>&1
        )
        [[ $boxed_wrapper_log == *'pin:0ae0c01b5e4c9fbf85c97adc66c2740f280727df,source:runner-child-cargo-build-jobs,raw-build-jobs:32,effective-cpus-source:child-nproc,effective-cpus:64,reverie-max-jobs:16,effective-native-jobs:16,effective-job-seconds:1050,max-elapsed-seconds:66'* ]] ||
            die "production wrapper did not log the bound boxed tuple: $boxed_wrapper_log"

        clamp_boundaries=$(
            for requested in 15 16 17 64; do
                PATH="$scratch/nproc-64:$PATH" "${clean_budget_env[@]}" \
                    REVERIE_DBI_BUDGET_BOUND_PIN=0ae0c01b5e4c9fbf85c97adc66c2740f280727df \
                    CARGO_BUILD_JOBS=$requested bash -c "$budget_probe" _ "$budget_config"
            done
        )
        [[ $clamp_boundaries == $'inherited-launch-cargo-build-jobs 15 15 15 child-nproc 64 16 15 1050 70\ninherited-launch-cargo-build-jobs 16 16 16 child-nproc 64 16 16 1050 66\ninherited-launch-cargo-build-jobs 17 17 17 child-nproc 64 16 16 1050 66\ninherited-launch-cargo-build-jobs 64 64 64 child-nproc 64 16 16 1050 66' ]] ||
            die "Reverie clamp boundary did not hold W at 16: $clamp_boundaries"
        cpu_boundaries=$(
            PATH="$scratch/nproc-4:$PATH" "${clean_budget_env[@]}" \
                REVERIE_DBI_BUDGET_BOUND_PIN=0ae0c01b5e4c9fbf85c97adc66c2740f280727df \
                CARGO_BUILD_JOBS=17 bash -c "$budget_probe" _ "$budget_config"
            PATH="$scratch/nproc-2:$PATH" "${clean_budget_env[@]}" \
                REVERIE_DBI_BUDGET_BOUND_PIN=0ae0c01b5e4c9fbf85c97adc66c2740f280727df \
                CARGO_BUILD_JOBS=8 bash -c "$budget_probe" _ "$budget_config"
        )
        [[ $cpu_boundaries == $'inherited-launch-cargo-build-jobs 17 17 17 child-nproc 4 16 4 1050 263\ninherited-launch-cargo-build-jobs 8 8 8 child-nproc 2 16 2 1050 525' ]] ||
            die "child nproc boundary did not cap the budget width: $cpu_boundaries"

        # Plant a well-formed but uncalibrated pin in a real Git fixture. The
        # production wrapper must refuse it through the canonical --print-pin
        # path before executing the requested command.
        fixture="$scratch/wrong-pin-hermit"
        wrong_pin=89abcdef0123456789abcdef0123456789abcdef
        mkdir -p "$fixture/ci" "$fixture/scripts/lib"
        cp "$budget_config" "$budget_wrapper" "$ROOT_DIR/ci/run-reverie-pin-check.sh" "$fixture/ci/"
        cp "$ROOT_DIR/scripts/check-reverie-pin.rs" "$fixture/scripts/"
        cp "$ROOT_DIR/scripts/lib/rust_script_prelude.rs" "$fixture/scripts/lib/"
        printf '[dependencies]\nreverie = { git = "https://github.com/rrnewton/reverie.git", rev = "%s" }\n' \
            "$wrong_pin" >"$fixture/Cargo.toml"
        git -C "$fixture" init -q
        git -C "$fixture" add Cargo.toml ci scripts
        if wrong_pin_log=$(
            PATH="$scratch/nproc-4:$PATH" CARGO_BUILD_JOBS=8 \
                "$fixture/ci/run-with-reverie-dbi-budget.sh" true 2>&1
        ); then
            wrong_pin_status=0
        else
            wrong_pin_status=$?
        fi
        [[ $wrong_pin_status == 2 ]] ||
            die "uncalibrated Reverie pin returned $wrong_pin_status instead of 2: $wrong_pin_log"
        [[ $wrong_pin_log == *"no calibrated budget for Reverie pin $wrong_pin"* ]] ||
            die "uncalibrated Reverie pin refusal lost its binding: $wrong_pin_log"

        if "${clean_budget_env[@]}" CI_DAG_BUILD_JOBS=0 \
            bash -c 'source "$1" launcher' _ "$budget_config" 2>/dev/null; then
            die "ordinary launcher accepted a zero build width"
        fi
        if "${clean_budget_env[@]}" CI_DAG_BUILD_JOBS=not-a-number \
            bash -c 'source "$1" launcher' _ "$budget_config" 2>/dev/null; then
            die "ordinary launcher accepted a noninteger build width"
        fi
        if PATH="$scratch/nproc-4:$PATH" "${clean_budget_env[@]}" \
            REVERIE_DBI_BUDGET_BOUND_PIN=wrong CARGO_BUILD_JOBS=8 \
            bash -c 'source "$1" reverie-dbi-budget-child' _ "$budget_config" 2>/dev/null; then
            die "child derivation accepted an uncalibrated Reverie pin"
        fi
        if PATH="$scratch/nproc-4:$PATH" "${clean_budget_env[@]}" \
            REVERIE_DBI_BUDGET_BOUND_PIN=0ae0c01b5e4c9fbf85c97adc66c2740f280727df \
            CI_DAG_REVERIE_DBI_MAX_BUILD_JOB_SECONDS=1050 CARGO_BUILD_JOBS=8 \
            bash -c 'source "$1" reverie-dbi-budget-child' _ "$budget_config" 2>/dev/null; then
            die "child derivation accepted a retired unconditioned DBI threshold"
        fi
        if PATH="$scratch/nproc-zero:$PATH" "${clean_budget_env[@]}" \
            REVERIE_DBI_BUDGET_BOUND_PIN=0ae0c01b5e4c9fbf85c97adc66c2740f280727df CARGO_BUILD_JOBS=8 \
            bash -c 'source "$1" reverie-dbi-budget-child' _ "$budget_config" 2>/dev/null; then
            die "child derivation accepted nproc=0"
        fi
        if PATH="$scratch/nproc-invalid:$PATH" "${clean_budget_env[@]}" \
            REVERIE_DBI_BUDGET_BOUND_PIN=0ae0c01b5e4c9fbf85c97adc66c2740f280727df CARGO_BUILD_JOBS=8 \
            bash -c 'source "$1" reverie-dbi-budget-child' _ "$budget_config" 2>/dev/null; then
            die "child derivation accepted a noninteger nproc observation"
        fi
        if PATH="$scratch/nproc-4:$PATH" "${clean_budget_env[@]}" \
            REVERIE_DBI_BUDGET_BOUND_PIN=0ae0c01b5e4c9fbf85c97adc66c2740f280727df CARGO_BUILD_JOBS=0 \
            bash -c 'source "$1" reverie-dbi-budget-child' _ "$budget_config" 2>/dev/null; then
            die "child derivation accepted a zero Cargo width"
        fi
        if "${clean_budget_env[@]}" bash -c 'source "$1"' _ "$budget_config" 2>/dev/null; then
            die "build-job configuration accepted an implicit source mode"
        fi
    )

    for lane in portable privileged; do
        dag="$DAG_ROOT/$lane.json"
        jq -e '
            .steps | type == "array" and length > 0
            and (map(.group + "." + .job) | unique | length) == length
            and all(.[]; (.cmd | type == "string" and length > 0))
        ' "$dag" >/dev/null || die "invalid or duplicate CI DAG steps: ${dag#"$ROOT_DIR/"}"
    done

    assert_reverie_pin_enforcement

    # Portable CI fans the audited DAG out across jobs; privileged CI still runs
    # its small hardware DAG within one job.
    assert_parallel_portable_workflow "$ROOT_DIR/.github/workflows/ci-portable.yml"
    # This is a literal workflow expression, not a local expansion.
    # shellcheck disable=SC2016
    assert_workflow_entrypoint privileged "$ROOT_DIR/.github/workflows/ci-privileged.yml" \
        'timeout --foreground --kill-after=10s 360s env SAFE_CI_DAG_RUNNER=agent-utils/py/bin/safe-ci-dag-runner ci/run-dag.sh privileged -j 2 --allow-cgroup-failure --perf-dir "$RUNNER_TEMP/hermit-privileged-dag-perf" -v'
    assert_privileged_diagnostics "$ROOT_DIR/.github/workflows/ci-privileged.yml"
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
    [[ $(grep -Fxc '        ./ci/run-dag.sh "$lane" -j "$VALIDATION_DAG_JOBS" -v' <<<"$runner_body") == 1 ]] ||
        die "validate.sh run_ci_manifest_lane must execute exactly one audited DAG"

    # This validation command contains real concurrent rustc probes. Keep both
    # lane copies on the measured 30s workload class and the same 60s cap so a
    # shorter privileged proxy cannot reject work that passed the portable gate.
    for lane in portable privileged; do
        jq -e '
            [.steps[] | select(
                .group == "e2e"
                and .job == "metadata"
                and .cmd == "./ci/test_harness.sh validate"
                and .timeout == 60
                and .hint.est_duration_s == 30
                and .hint.hard_mem_max_bytes == 1073741824
            )] | length == 1
        ' "$DAG_ROOT/$lane.json" >/dev/null ||
            die "$lane e2e.metadata must carry the measured validation workload and 60s/1GiB bounds"
    done

    local privileged_critical_path privileged_job_timeout_minutes
    local privileged_inner_timeout_seconds=360
    local privileged_runner_overhead_seconds=30
    privileged_critical_path=$(dag_critical_path_seconds "$DAG_ROOT/privileged.json")
    privileged_job_timeout_minutes=$(workflow_job_timeout_minutes \
        "$ROOT_DIR/.github/workflows/ci-privileged.yml" privileged)
    [[ $privileged_critical_path =~ ^[0-9]+$ ]] ||
        die "privileged DAG critical path is not an integer: $privileged_critical_path"
    [[ $privileged_job_timeout_minutes =~ ^[1-9][0-9]*$ ]] ||
        die "privileged workflow has no numeric outer job timeout"
    ((privileged_inner_timeout_seconds > privileged_critical_path + privileged_runner_overhead_seconds)) ||
        die "privileged DAG launcher must exceed ${privileged_critical_path}s critical path plus ${privileged_runner_overhead_seconds}s runner overhead"
    ((privileged_job_timeout_minutes * 60 > privileged_inner_timeout_seconds + 180)) ||
        die "privileged job timeout must exceed ${privileged_inner_timeout_seconds}s DAG launcher plus 180s workflow overhead"

    [[ -f $EXPECTED_PLAN ]] || die "missing E2E denominator ratchet: ${EXPECTED_PLAN#"$ROOT_DIR/"}"
    jq -e '.schema == 1 and (.cells | type == "array" and length > 0)' "$EXPECTED_PLAN" >/dev/null ||
        die "invalid E2E denominator ratchet"
    local scratch current_plan expected_plan all_buckets
    scratch=$(mktemp -d)
    current_plan="$scratch/current-plan.json"
    expected_plan="$scratch/expected-plan.json"
    all_buckets="$scratch/manifest-buckets.json"
    emit_required_plan | jq -sS 'sort_by(.category,.test,.mode,.backend)' >"$current_plan"
    jq -S '.cells | sort_by(.category,.test,.mode,.backend)' "$EXPECTED_PLAN" >"$expected_plan"
    if ! diff -u "$expected_plan" "$current_plan"; then
        rm -rf "$scratch"
        die "required E2E plan changed; update ci/expected-e2e-plan.json in the same review"
    fi
    emit_manifest_buckets >"$all_buckets"

    local selectors expected_buckets dag_buckets selected_cells lane_cells
    for lane in portable privileged; do
        dag="$DAG_ROOT/$lane.json"
        jq -e --arg lane "$lane" '
            def expected_command($m):
                "./ci/test_harness.sh run --lane \($m.lane) --category \($m.category) --ci-only --allow-empty --prebuilt --results ignored/e2e/\($m.lane)/\($m.category)/results.jsonl --junit ignored/e2e/\($m.lane)/\($m.category)/junit.xml";
            ([.steps[] | select(.cmd | startswith("./ci/test_harness.sh run "))] | all(has("manifest")))
            and ([.steps[] | select(has("manifest"))] | all(
                . as $step
                | ($step.manifest | (keys | sort) == ["category","lane"] and .lane == $lane and (.category | length > 0))
                and ($step.cmd == expected_command($step.manifest))
                and ($step.deps | index("e2e.metadata") != null)
                and ($step.deps | index("build.manifest_guests") != null)))
            and ([.steps[] | select(has("manifest")) | .manifest.category] | unique | length)
                == ([.steps[] | select(has("manifest"))] | length)
            and ([.steps[] | select(.group == "build" and .job == "manifest_guests"
                    and .cmd == ("./ci/test_harness.sh build --lane " + $lane + " --ci-only --allow-empty"))] | length) == 1
        ' "$dag" >/dev/null || {
            rm -rf "$scratch"
            die "$lane DAG manifest nodes do not match the fail-closed build/run contract"
        }

        selectors="$scratch/$lane-selectors.json"
        expected_buckets="$scratch/$lane-expected-buckets.json"
        dag_buckets="$scratch/$lane-dag-buckets.json"
        selected_cells="$scratch/$lane-selected-cells.json"
        lane_cells="$scratch/$lane-cells.json"
        jq -S '[.steps[] | select(has("manifest")) | .manifest] | sort_by(.lane,.category)' \
            "$dag" >"$selectors"
        jq -S --arg lane "$lane" '[.[] | select(.lane == $lane)]' \
            "$all_buckets" >"$expected_buckets"
        cp "$selectors" "$dag_buckets"
        if ! diff -u "$expected_buckets" "$dag_buckets"; then
            rm -rf "$scratch"
            die "$lane DAG must contain exactly one run node per manifest bucket"
        fi

        jq -S --arg lane "$lane" --slurpfile selectors "$selectors" '
            [.[] as $cell
             | select($cell.lane == $lane)
             | select(any($selectors[0][]; .category == $cell.category))
             | $cell]
            | sort_by(.category,.test,.mode,.backend)
        ' "$current_plan" >"$selected_cells"
        jq -S --arg lane "$lane" '[.[] | select(.lane == $lane)]
            | sort_by(.category,.test,.mode,.backend)' "$current_plan" >"$lane_cells"
        if ! diff -u "$lane_cells" "$selected_cells"; then
            rm -rf "$scratch"
            die "$lane DAG manifest nodes do not select the exact ratcheted cells"
        fi
    done

    local portable_fingerprint privileged_fingerprint e2e_cells
    portable_fingerprint=$(jq -Sc '.steps | map({id:(.group + "." + .job),cmd})' \
        "$DAG_ROOT/portable.json" | sha256sum | cut -d' ' -f1)
    privileged_fingerprint=$(jq -Sc '.steps | map({id:(.group + "." + .job),cmd})' \
        "$DAG_ROOT/privileged.json" | sha256sum | cut -d' ' -f1)
    e2e_cells=$(jq length "$current_plan")
    rm -rf "$scratch"
    jq -n \
        --arg portable_fingerprint "$portable_fingerprint" \
        --arg privileged_fingerprint "$privileged_fingerprint" \
        --argjson portable_steps "$(jq '.steps | length' "$DAG_ROOT/portable.json")" \
        --argjson privileged_steps "$(jq '.steps | length' "$DAG_ROOT/privileged.json")" \
        --argjson privileged_critical_path_seconds "$privileged_critical_path" \
        --argjson e2e_cells "$e2e_cells" \
        '{portable_steps:$portable_steps,privileged_steps:$privileged_steps,
          privileged_critical_path_seconds:$privileged_critical_path_seconds,
          e2e_cells:$e2e_cells,portable_fingerprint:$portable_fingerprint,
          privileged_fingerprint:$privileged_fingerprint,
          correspondence:"validated exact workflow/validate entrypoints, one DAG node per manifest bucket, and exact aggregate cells"}'
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
        [[ -z $dup ]] || die "ci/dag/$lane.json: duplicate node id(s): $dup"
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
            die "ci/dag/$lane.json: e2e run-node targets wrong lane: $wrong_lane"

        local expected actual
        expected=$(emit_manifest_buckets | jq -r --arg lane "$lane" \
            '.[] | select(.lane == $lane) | .category' | LC_ALL=C sort -u)
        actual=$(jq -r '
            .steps[] | select(.group == "e2e")
            | select(.cmd | test("--category "))
            | (.cmd | capture("--category (?<c>\\S+)")) | .c' "$dag" |
            LC_ALL=C sort -u)

        if [[ $expected != "$actual" ]]; then
            {
                echo "ci/dag/$lane.json: e2e DAG nodes do not correspond to the $lane manifest buckets."
                echo "  manifest categories: $expected"
                echo "  DAG run-node cats  : $actual"
                comm -23 <(printf '%s\n' "$expected") <(printf '%s\n' "$actual") |
                    sed 's/^/  MISSING from DAG (node deleted or renamed?): /'
                comm -13 <(printf '%s\n' "$expected") <(printf '%s\n' "$actual") |
                    sed 's/^/  EXTRA in DAG (no such manifest bucket): /'
            } >&2
            die "DAG/manifest-bucket correspondence mismatch for lane $lane"
        fi
    done
    echo "PASS: committed CI DAG (ci/dag/${LANES[0]}.json, ci/dag/${LANES[1]}.json) corresponds to the manifest buckets with no dangling deps"
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
PROBE_DISABLED=0
CI_ONLY=0
PREBUILT=0
ALLOW_EMPTY=0

function parse_options {
    while (($#)); do
        case "$1" in
            --lane) LANE_FILTER=${2:?missing lane}; shift 2 ;;
            --mode) MODE_FILTER=${2:?missing mode}; shift 2 ;;
            --backend) BACKEND_FILTER=${2:?missing backend}; shift 2 ;;
            --category) CATEGORY_FILTER=${2:?missing category}; shift 2 ;;
            --test) TEST_FILTER=${2:?missing test id}; shift 2 ;;
            --ci-only) CI_ONLY=1; shift ;;
            --prebuilt) PREBUILT=1; shift ;;
            --allow-empty) ALLOW_EMPTY=1; shift ;;
            --format) FORMAT=${2:?missing format}; shift 2 ;;
            --results) RESULTS=${2:?missing result path}; shift 2 ;;
            --junit) JUNIT=${2:?missing JUnit path}; shift 2 ;;
            --include-occasional) INCLUDE_OCCASIONAL=1; shift ;;
            --include-manual) INCLUDE_MANUAL=1; shift ;;
            --probe-disabled) PROBE_DISABLED=1; shift ;;
            -h|--help) usage; exit 0 ;;
            *) die "unknown option: $1" ;;
        esac
    done

    [[ -z $LANE_FILTER ]] || contains "$LANE_FILTER" portable privileged || die "invalid lane: $LANE_FILTER"
    [[ -z $MODE_FILTER ]] || contains "$MODE_FILTER" "${MODES[@]}" || die "invalid mode: $MODE_FILTER"
    [[ -z $BACKEND_FILTER ]] || contains "$BACKEND_FILTER" "${BACKENDS[@]}" || die "invalid backend: $BACKEND_FILTER"
    if ((ALLOW_EMPTY == 1)); then
        ((CI_ONLY == 1)) || die "--allow-empty requires --ci-only"
        case $subcommand in
            build) [[ -n $LANE_FILTER || -n $CATEGORY_FILTER ]] ||
                die "build --allow-empty requires an explicit --lane or --category" ;;
            run) [[ -n $CATEGORY_FILTER ]] ||
                die "run --allow-empty requires an explicit --category" ;;
            *) die "--allow-empty is accepted by build and run only" ;;
        esac
    fi
    [[ $FORMAT == text || $FORMAT == json ]] || die "invalid format: $FORMAT"
    if ((INCLUDE_MANUAL)); then
        [[ -n $TEST_FILTER && -n $MODE_FILTER ]] ||
            die "--include-manual requires exact --test and --mode filters"
    fi
    if ((PROBE_DISABLED)); then
        [[ $subcommand == run ]] || die "--probe-disabled is accepted by run only"
        [[ -n $TEST_FILTER && -n $MODE_FILTER && -n $BACKEND_FILTER ]] ||
            die "--probe-disabled requires exact --test, --mode, and --backend filters"
        ((INCLUDE_MANUAL == 0)) ||
            die "--probe-disabled and --include-manual are mutually exclusive"
        ((CI_ONLY == 0)) ||
            die "--probe-disabled and --ci-only are mutually exclusive"
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
        --argjson ci_only "$CI_ONLY" \
        --argjson include_occasional "$INCLUDE_OCCASIONAL" '
        select($lane_filter == "" or .lane == $lane_filter)
        | select($category_filter == "" or .category == $category_filter)
        | select($test_filter == "" or .id == $test_filter)
        | select($include_occasional == 1 or (.occasional // false) == false)
        | . as $test
        | .modes | to_entries[]
        | select($mode_filter == "" or .key == $mode_filter)
        | select(.value.ci == true or $mode_filter == "naked" or $include_manual == 1)
        | select($ci_only == 0 or .value.ci == true)
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

# $1 is the number of guest executions the mode is contracted to perform.
# `complete` and `eligible` are bound to that obligation: a run that produced
# fewer records than executions is a SHORTFALL, not a pass. Reporting the count
# without enforcing it lets one surviving record authorize a two-execution cell.
function summarize_sabre_path_evidence {
    local expected=$1
    shift
    jq -s --argjson expected "$expected" '
        if all(.[];
            .schema == 1
            and (.guest_rpc_observed | type == "boolean")
            and (.ptrace_fallback_sites | type == "number")
            and (.trusted_shared_object_sites | type == "number")
            and (.trusted_shared_objects | type == "array"))
        then {
            schema: 1,
            expected_execution_count: $expected,
            complete: (length == $expected),
            execution_count: length,
            guest_rpc_observed: (length > 0 and all(.[]; .guest_rpc_observed)),
            ptrace_fallback_sites: (map(.ptrace_fallback_sites) | add // 0),
            trusted_shared_object_sites: (map(.trusted_shared_object_sites) | add // 0),
            trusted_shared_objects: (map(.trusted_shared_objects) | add // [] | unique),
            eligible: (length == $expected and length > 0 and all(.[];
                .guest_rpc_observed
                and .ptrace_fallback_sites == 0
                and .trusted_shared_object_sites == 0)),
            executions: .
        }
        else error("invalid SaBRe path-evidence record")
        end
    ' "$@"
}

function collect_sabre_path_evidence {
    local cell_dir=$1
    local expected=$2
    local -a files=()
    shopt -s nullglob
    files=("$cell_dir"/captures/*.sabre-path.jsonl)
    shopt -u nullglob
    if ((${#files[@]} == 0)); then
        jq -cn --argjson expected "$expected" '{schema:1,
            expected_execution_count:$expected,complete:false,execution_count:0,
            guest_rpc_observed:false,
            ptrace_fallback_sites:0,trusted_shared_object_sites:0,
            trusted_shared_objects:[],eligible:false,executions:[]}'
        return
    fi
    summarize_sabre_path_evidence "$expected" "${files[@]}"
}

# Hermit's built-in verification executes the guest twice (see the guest_tmpdir
# comment in prepare_test); every other mode executes it once. The decision
# point must know this number, not merely report whatever arrived.
function expected_sabre_execution_count {
    case $1 in
    verify | replay) echo 2 ;;
    *) echo 1 ;;
    esac
}

function audit_sabre_path_evidence_contract {
    local eligible fallback trusted shortfall
    eligible=$(printf '%s\n' \
        '{"schema":1,"guest_rpc_observed":true,"ptrace_fallback_sites":0,"trusted_shared_object_sites":0,"trusted_shared_objects":[]}' \
        '{"schema":1,"guest_rpc_observed":true,"ptrace_fallback_sites":0,"trusted_shared_object_sites":0,"trusted_shared_objects":[]}' |
        summarize_sabre_path_evidence 2)
    fallback=$(printf '%s\n' \
        '{"schema":1,"guest_rpc_observed":true,"ptrace_fallback_sites":1,"trusted_shared_object_sites":0,"trusted_shared_objects":[]}' |
        summarize_sabre_path_evidence 1)
    trusted=$(printf '%s\n' \
        '{"schema":1,"guest_rpc_observed":true,"ptrace_fallback_sites":0,"trusted_shared_object_sites":1,"trusted_shared_objects":["/usr/lib/libc.so.6"]}' |
        summarize_sabre_path_evidence 1)
    # PLANTED NEGATIVE for the execution-count obligation: one otherwise-clean
    # record where the mode contracts for two. Every per-record predicate here
    # passes, so this is refused only if the count itself is enforced.
    shortfall=$(printf '%s\n' \
        '{"schema":1,"guest_rpc_observed":true,"ptrace_fallback_sites":0,"trusted_shared_object_sites":0,"trusted_shared_objects":[]}' |
        summarize_sabre_path_evidence 2)
    jq -e '.eligible and .complete and .execution_count == 2' <<<"$eligible" >/dev/null ||
        die "legitimate SaBRe path evidence must remain eligible"
    jq -e '(.eligible | not) and .ptrace_fallback_sites == 1' <<<"$fallback" >/dev/null ||
        die "ptrace-installed SaBRe markers must be classified as fallback"
    jq -e '(.eligible | not) and .trusted_shared_object_sites == 1' <<<"$trusted" >/dev/null ||
        die "trusted shared-object native execution must be ineligible"
    jq -e '(.eligible | not) and (.complete | not)
        and .execution_count == 1 and .expected_execution_count == 2' <<<"$shortfall" >/dev/null ||
        die "an execution-count shortfall must be ineligible even when every record is clean"
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
    local metadata kind id prebuilt_fixtures
    metadata=$(metadata_json "$test")
    kind=$(jq -r .program_kind <<<"$metadata")
    id=$(jq -r .id <<<"$metadata")
    if ((PREBUILT == 1)) && [[ $kind != direct && $kind != direct-argv ]]; then
        prebuilt_fixtures="$BUILD_ROOT/${id//\//-}/fixtures"
        [[ -d $prebuilt_fixtures ]] || {
            echo "prebuilt fixture is missing for $id: $prebuilt_fixtures" >&2
            return 1
        }
        cp -a "$prebuilt_fixtures/." "$cell_dir/fixtures/"
        return 0
    fi
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
    [[ $kind != direct && $kind != direct-argv ]] || return 0
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
    local timeout_seconds stdout_file stderr_file path_evidence_file guest_tmpdir kind guest_backend
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
    if [[ $backend == sabre ]]; then
        path_evidence_file="$cell_dir/captures/${mode}-${attempt}.sabre-path.jsonl"
        : >"$path_evidence_file"
        env_args+=(HERMIT_SABRE_PATH_EVIDENCE="$path_evidence_file")
    fi
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
        direct-argv)
            mapfile -t guest_command < <(jq -r '.direct_argv[]' <<<"$metadata")
            guest_command+=("${guest_args[@]}")
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
    local path_evidence=$9
    local test_file test_sha256 binary_sha256 effective_args guest_args guest_backend relaxations log_level classification kind
    test_file=${TEST_BY_ID[$test_id]}
    if [[ -f $test_file ]]; then
        test_sha256=$(sha256sum "$test_file" | cut -d' ' -f1)
    else
        kind=$(jq -r .program_kind <<<"${METADATA_BY_ID[$test_id]}")
        if [[ $kind == direct-argv ]]; then
            test_sha256=$(jq -c .direct_argv <<<"${METADATA_BY_ID[$test_id]}" | sha256sum | cut -d' ' -f1)
        else
            test_sha256=$(jq -r .direct_command <<<"${METADATA_BY_ID[$test_id]}" | sha256sum | cut -d' ' -f1)
        fi
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
    classification=required
    ((PROBE_DISABLED)) && classification=disabled
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
        --arg classification "$classification" \
        --arg outcome "$outcome" \
        --arg reason "$reason" \
        --arg log_level "$log_level" \
        --argjson duration_ms "$duration_ms" \
        --argjson effective_args "$effective_args" \
        --argjson guest_args "$guest_args" \
        --argjson relaxations "$relaxations" \
        --argjson path_evidence "$path_evidence" \
        '{schema:2,run_id:$run_id,hermit_sha:$hermit_sha,source_tree_dirty:$source_tree_dirty,
          binary_sha256:(if $binary_sha256 == "" then null else $binary_sha256 end),
          test_sha256:$test_sha256,test:$test,category:$category,lane:$lane,mode:$mode,
          backend:(if $backend == "" then null else $backend end),classification:$classification,
          outcome:$outcome,duration_ms:$duration_ms,
          log_level:(if $log_level == "" then null else $log_level end),
          effective_args:$effective_args,guest_args:$guest_args,
          relaxations:$relaxations,preprocessor:null,execution_path:$path_evidence,
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

    local outcome=PASS reason='' path_evidence=null
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

    if [[ $backend == sabre ]]; then
        path_evidence=$(collect_sabre_path_evidence "$cell_dir" \
            "$(expected_sabre_execution_count "$mode")") || {
            outcome=ERROR
            reason="invalid SaBRe path evidence"
            path_evidence=null
        }
        if [[ $outcome == PASS && $(jq -r '.eligible // false' <<<"$path_evidence") != true ]]; then
            outcome=FAIL
            reason=$(jq -r '
                "SaBRe path ineligible: executions=\(.execution_count)/\(.expected_execution_count), "
                + "guest_rpc_observed=\(.guest_rpc_observed), "
                + "ptrace_fallback_sites=\(.ptrace_fallback_sites), "
                + "trusted_shared_object_sites=\(.trusted_shared_object_sites), "
                + "trusted_shared_objects=\(.trusted_shared_objects | join(","))"
            ' <<<"$path_evidence")
        fi
    fi

    end_ms=$(date +%s%3N)
    duration_ms=$((end_ms - start_ms))
    append_result "$id" "$category" "$lane" "$mode" "$backend" "$outcome" "$duration_ms" "$reason" "$path_evidence"
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

function build_required {
    local planned test_id test build_dir metadata timeout_seconds kind failures=0 selected=0
    planned=$(emit_required_plan)
    while IFS= read -r test_id; do
        [[ -n $test_id ]] || continue
        selected=$((selected + 1))
        test=${TEST_BY_ID[$test_id]}
        build_dir="$BUILD_ROOT/${test_id//\//-}"
        rm -rf "$build_dir"
        prepare_cell_dirs "$build_dir"
        metadata=$(metadata_json "$test")
        timeout_seconds=$(jq -r .timeout_seconds <<<"$metadata")
        kind=$(jq -r .program_kind <<<"$metadata")
        if prepare_test "$test" "$build_dir" "$timeout_seconds"; then
            printf 'BUILT %-11s %s\n' "$kind" "$test_id"
        else
            failures=$((failures + 1))
        fi
    done < <(jq -r '.test' <<<"$planned" | LC_ALL=C sort -u)

    ((selected > 0)) || ((ALLOW_EMPTY == 1)) || die "filters selected no required test cells"
    echo "Build root: $BUILD_ROOT"
    ((failures == 0))
}

function run_required {
    RESULTS=${RESULTS:-$RESULT_ROOT/$RUN_ID/results.jsonl}
    JUNIT=${JUNIT:-$RESULT_ROOT/$RUN_ID/junit.xml}
    mkdir -p "$(dirname "$RESULTS")"
    : >"$RESULTS"

    local planned test_id mode backend test metadata failures=0 selected=0
    if ((PROBE_DISABLED)); then
        planned=$(emit_gap_plan)
    else
        planned=$(emit_required_plan)
    fi
    while IFS=$'\t' read -r test_id mode backend; do
        [[ -n $test_id ]] || continue
        selected=$((selected + 1))
        test=${TEST_BY_ID[$test_id]}
        metadata=$(metadata_json "$test")
        run_cell "$test" "$metadata" "$mode" "$backend" || failures=$((failures + 1))
    done < <(jq -r '[.test,.mode,(.backend // "")] | @tsv' <<<"$planned")

    ((selected > 0)) || ((ALLOW_EMPTY == 1)) || die "filters selected no required test cells"
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
        audit_sabre_path_evidence_contract
        audit_test_footprints
        python3 "$ROOT_DIR/tests/backend-parity/split_asymmetric_pr.py" --self-test
        audit_inventory
        audit_ci_correspondence
        echo "PASS: ${#TESTS[@]} E2E tests have valid syntax and centralized schema-v2 manifests"
        emit_required_plan | jq -s '{tests:(map(.test)|unique|length),required_cells:length,by_mode:(group_by(.mode)|map({key:.[0].mode,value:length})|from_entries)}'
        validate_dag_correspondence
        ;;
    plan)
        print_plan required
        ;;
    build)
        ((PREBUILT == 0)) || die "build does not accept --prebuilt"
        build_required
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
    audit-test-footprints)
        audit_test_footprints
        ;;
    audit-ci)
        audit_ci_correspondence
        ;;
    *)
        usage
        die "unknown command: $subcommand"
        ;;
esac
