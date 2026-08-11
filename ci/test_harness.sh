#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail

# SAY WHY WE ARE EXITING NON-ZERO.
#
# `set -e` plus `var=$(cmd 2>&1)` is a silent killer: when `cmd` fails, the
# ASSIGNMENT fails, and the shell exits right there -- before the `|| die "...:
# $var"` on the very next line can run. The diagnostic `cmd` printed is sitting
# inside the variable that just went out of scope, so it is destroyed rather
# than reported. `audit_ci_correspondence` had nine such captures and exited 2
# printing NOTHING on stdout or stderr.
#
# That silence was expensive out of proportion to the bug behind it. validate.sh
# can only render a wordless exit 2 as `exit 2: }` (the last line of unrelated
# JSON that happened to be on stdout), and this one audit runs in four of the
# seven validate gates, so a one-line stale constant took all four red with no
# indication of which check failed or why. Finding it needed `bash -x`.
#
# `set -E` propagates this trap into functions, subshells, and command
# substitutions, which is exactly where the silent exits live. The trap only
# reports; it never changes the exit status, so no gate becomes more lenient.
#
# The trap must stay SHORT. `$BASH_COMMAND` for a compound `( ... )` block is the
# entire block: the first version of this trap printed 11 KB of reconstructed
# subshell source and buried the one line that mattered. A diagnostic that has to
# be searched is barely better than no diagnostic. So: first line only, capped,
# and compound blocks are named rather than dumped.
set -E
__harness_err() {
    local rc=$1 line=$2 cmd=$3
    [[ $rc -eq 0 ]] && return 0
    cmd=${cmd%%$'\n'*}
    if [[ $cmd == '('* ]]; then
        cmd='(compound block — see the message above for the actual failure)'
    elif ((${#cmd} > 120)); then
        cmd="${cmd:0:120}…"
    fi
    printf 'test_harness.sh: FAILED (exit %s) near line %s: %s\n' "$rc" "$line" "$cmd" >&2
    printf '  If no reason was printed above, a probe captured its own output into a\n' >&2
    printf '  variable (var=$(cmd 2>&1)) and set -e aborted before the || die could speak.\n' >&2
}
trap '__harness_err "$?" "$LINENO" "$BASH_COMMAND"' ERR

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
readonly -a BACKENDS=(ptrace dbt kvm sabre liteinst)
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
  ci/test_harness.sh audit-test-binary-registration
  ci/test_harness.sh audit-ci

Filters:
  --lane LANE             portable or privileged
  --mode MODE             verify, chaos, replay, naked, or custom
  --backend BACKEND       ptrace, dbt, kvm, sabre, or liteinst
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

# Run one audit probe without letting `set -e` discard the command substitution's
# captured output.  A probe is allowed to have a nonzero *expected* status (the
# wrong-pin negative is one), but every mismatch remains fatal after the exact
# probe name and its stdout/stderr have been surfaced.
function run_audit_probe_expect_status {
    local probe_name=$1 expected_status=$2 output_var=$3
    shift 3
    local probe_output probe_status

    if probe_output=$("$@" 2>&1); then
        probe_status=0
    else
        probe_status=$?
    fi
    if [[ -n $output_var ]]; then
        printf -v "$output_var" '%s' "$probe_output"
    fi

    if ((probe_status != expected_status)); then
        echo "test_harness.sh: audit probe '$probe_name' returned $probe_status (expected $expected_status)" >&2
        if [[ -n $probe_output ]]; then
            printf '%s\n' "$probe_output" >&2
        else
            echo "test_harness.sh: audit probe '$probe_name' produced no stdout/stderr" >&2
        fi
        if ((probe_status == 0)); then
            return 1
        fi
        return "$probe_status"
    fi
}

function require_executable_hermit {
    local path=$1
    [[ -x $path ]] || die "Hermit binary is not executable: $path"
}

# Exercise the real publisher/verifier/consumer wrapper. The writer and reader
# handshake over three observed mutable-source states; every reader invocation
# must execute the original published identity. Four negative fixtures then
# prove the wrapper refuses before invoking its consumer.
function audit_immutable_hermit_binary {
    local scratch mutable pointer bundles consumer writer_pid state cycle checks=0 refusals=0
    local complete_source complete_pointer complete_bundle fake fake_pointer output status path expected_reason
    local incomplete_pointer incomplete_bundle negative_dag negative_entry_marker negative_marker
    local dag_entry_marker dag_entry
    local dag_command dag_output dag_detail dag_status starts failures exact_refusal refusal_count
    scratch=$(mktemp -d)
    mutable="$scratch/target/debug/hermit"
    pointer="$scratch/target/ci/artifact.path"
    bundles="$scratch/target/ci/artifacts"
    consumer="$scratch/consumer.sh"
    mkdir -p "$(dirname "$mutable")"
    printf '#!/usr/bin/env bash\nprintf "expected-identity\\n"\n' >"$mutable"
    chmod 755 "$mutable"
    "$ROOT_DIR/ci/publish-hermit-e2e-artifact.sh" "$mutable" "$bundles" "$pointer" >/dev/null
    cat >"$consumer" <<'CONSUMER'
#!/usr/bin/env bash
set -euo pipefail
[[ -z ${CONSUMER_ENTRY_MARKER:-} ]] || : >"$CONSUMER_ENTRY_MARKER"
[[ $("$HERMIT_BIN") == expected-identity ]]
[[ -z ${CONSUMER_MARKER:-} ]] || : >"$CONSUMER_MARKER"
CONSUMER
    chmod 755 "$consumer"

    (
        for cycle in 1 2 3; do
            mv "$mutable" "$mutable.absent-$cycle"
            : >"$scratch/state-$cycle-absent"
            while [[ ! -e $scratch/ack-$cycle-absent ]]; do sleep 0.01; done
            printf '#!/usr/bin/env bash\nprintf "wrong-nonexec\\n"\n' >"$mutable"
            chmod 644 "$mutable"
            : >"$scratch/state-$cycle-nonexec"
            while [[ ! -e $scratch/ack-$cycle-nonexec ]]; do sleep 0.01; done
            printf '#!/usr/bin/env bash\nprintf "wrong-relinked-%s\\n"\n' "$cycle" >"$mutable.next"
            chmod 755 "$mutable.next"
            mv "$mutable.next" "$mutable"
            : >"$scratch/state-$cycle-relinked"
            while [[ ! -e $scratch/ack-$cycle-relinked ]]; do sleep 0.01; done
        done
    ) &
    writer_pid=$!
    for cycle in 1 2 3; do
        for state in absent nonexec relinked; do
            while [[ ! -e $scratch/state-$cycle-$state ]]; do sleep 0.01; done
            for _ in {1..8}; do
                HERMIT_E2E_ARTIFACT_POINTER="$pointer" \
                    "$ROOT_DIR/ci/run-with-hermit-e2e-artifact.sh" "$consumer" >/dev/null 2>&1
                checks=$((checks + 1))
            done
            : >"$scratch/ack-$cycle-$state"
        done
    done
    wait "$writer_pid"

    complete_source="$scratch/install_pkg"
    for path in libdetcore_dbt.so libdetcore_sabre.so libreverie_dbt_client.so libreverie_liteinst.so; do
        mkdir -p "$complete_source/rsrcs/$(dirname "$path")"
        printf 'fixture-%s\n' "$path" >"$complete_source/rsrcs/$path"
    done
    for path in dynamorio/bin64/drrun sabre e9patch e9tool; do
        mkdir -p "$complete_source/rsrcs/$(dirname "$path")"
        printf '#!/usr/bin/env bash\nexit 0\n' >"$complete_source/rsrcs/$path"
        chmod 755 "$complete_source/rsrcs/$path"
    done
    complete_pointer="$scratch/complete.path"
    "$ROOT_DIR/ci/publish-hermit-e2e-artifact.sh" "$mutable.absent-1" \
        "$scratch/complete-artifacts" "$complete_pointer" "$complete_source" >/dev/null
    complete_bundle=$("$ROOT_DIR/ci/verify-hermit-e2e-artifact.sh" "$complete_pointer")

    for state in missing nonexec wrong-hash incomplete; do
        fake_pointer="$scratch/$state.path"
        expected_reason=
        case "$state" in
            missing)
                printf '%s\n' "$scratch/does-not-exist" >"$fake_pointer"
                expected_reason="published artifact directory is missing"
                ;;
            nonexec|wrong-hash)
                fake="$scratch/$state/${complete_bundle##*/}"
                mkdir -p "$(dirname "$fake")"
                cp -a "$complete_bundle" "$fake"
                if [[ $state == nonexec ]]; then
                    chmod 644 "$fake/hermit"
                    expected_reason="published Hermit is missing, empty, or non-executable"
                else
                    printf corruption >>"$fake/hermit"
                    expected_reason="published Hermit hash mismatch"
                fi
                printf '%s\n' "$fake" >"$fake_pointer"
                ;;
            incomplete)
                fake="$scratch/$state/${complete_bundle##*/}"
                mkdir -p "$(dirname "$fake")"
                cp -a "$complete_bundle" "$fake"
                rm "$fake/install/rsrcs/sabre"
                printf '%s\n' "$fake" >"$fake_pointer"
                incomplete_pointer=$fake_pointer
                incomplete_bundle=$fake
                expected_reason="resource bundle executable is missing, empty, or non-executable"
                ;;
        esac
        set +e
        output=$(CONSUMER_ENTRY_MARKER="$scratch/$state.consumer-entered" \
            CONSUMER_MARKER="$scratch/$state.consumer-executed" \
            HERMIT_E2E_ARTIFACT_POINTER="$fake_pointer" \
            "$ROOT_DIR/ci/run-with-hermit-e2e-artifact.sh" --require-install "$consumer" 2>&1)
        status=$?
        set -e
        [[ $status == 2 ]] || die "immutable artifact negative '$state' returned $status, expected 2: $output"
        [[ $output == *"$expected_reason"* ]] ||
            die "immutable artifact negative '$state' did not name '$expected_reason': $output"
        [[ ! -e $scratch/$state.consumer-entered ]] ||
            die "immutable artifact negative '$state' entered the protected consumer after refusal"
        [[ ! -e $scratch/$state.consumer-executed ]] ||
            die "immutable artifact negative '$state' executed the consumer after refusal"
        refusals=$((refusals + 1))
    done

    negative_dag="$scratch/bad-artifact-dag.json"
    negative_entry_marker="$scratch/dag-consumer-entered"
    negative_marker="$scratch/dag-consumer-executed"
    dag_entry_marker="$scratch/dag-entry"
    dag_entry="$scratch/dag-entry.sh"
    cat >"$dag_entry" <<'DAG_ENTRY'
#!/usr/bin/env bash
set -euo pipefail
: >"$DAG_ENTRY_MARKER"
exec "$RUN_WITH_ARTIFACT" --require-install "$PROTECTED_CONSUMER"
DAG_ENTRY
    chmod 755 "$dag_entry"
    dag_command="DAG_ENTRY_MARKER=$dag_entry_marker RUN_WITH_ARTIFACT=$ROOT_DIR/ci/run-with-hermit-e2e-artifact.sh PROTECTED_CONSUMER=$consumer CONSUMER_ENTRY_MARKER=$negative_entry_marker CONSUMER_MARKER=$negative_marker HERMIT_E2E_ARTIFACT_POINTER=$incomplete_pointer $dag_entry"
    exact_refusal="[e2e.bad_artifact] verify-hermit-e2e-artifact.sh: resource bundle executable is missing, empty, or non-executable: $incomplete_bundle/install/rsrcs/sabre"
    jq -n --arg cmd "$dag_command" '{
        resource_caps: {}, default_step_timeout: 30,
        steps: [{group:"e2e",job:"bad_artifact",desc:"Reject one incomplete published artifact",
                 cmd:$cmd,timeout:30,
                 hint:{est_duration_s:1,rss_baseline_bytes:67108864,
                       hard_mem_max_bytes:268435456,classification:"light"}}]
    }' >"$negative_dag"
    set +e
    dag_output=$(RUN_DAG_FILE_OVERRIDE="$negative_dag" \
        "$ROOT_DIR/ci/run-dag.sh" portable -j 1 --allow-cgroup-failure \
        --perf-dir "$scratch/perf" --profile -v 2>&1)
    dag_status=$?
    set -e
    starts=$(grep -Fc '[e2e.bad_artifact] ▶ START' <<<"$dag_output" || true)
    failures=$(grep -Fc '[e2e.bad_artifact] ✗ FAIL' <<<"$dag_output" || true)
    dag_detail=$(sed -n \
        '/^\[e2e\.bad_artifact\] ----- detail -----$/,/^\[e2e\.bad_artifact\] ----- end detail -----$/p' \
        <<<"$dag_output")
    refusal_count=$(grep -Fxc -- "$exact_refusal" <<<"$dag_detail" || true)
    [[ $dag_status != 0 && $starts == 1 && $failures == 1 ]] ||
        die "bad-artifact DAG did not execute exactly one failing node (rc=$dag_status starts=$starts failures=$failures): $dag_output"
    [[ -e $dag_entry_marker ]] || die "bad-artifact DAG failed before entering its verifier command: $dag_output"
    [[ $refusal_count == 1 ]] ||
        die "bad-artifact DAG emitted $refusal_count exact verifier-refusal lines, expected 1 ('$exact_refusal'): $dag_output"
    [[ ! -e $negative_entry_marker ]] || die "bad-artifact DAG entered its protected consumer"
    [[ ! -e $negative_marker ]] || die "bad-artifact DAG executed its protected consumer"
    echo "immutable artifact bracket: $checks expected-identity executions across 3 repeated absent/nonexec/relinked cycles; $refusals/4 direct bad artifacts refused; bad-artifact DAG rc=$dag_status executed_nodes=$starts failed_nodes=$failures verifier_entered=1 exact_refusal_lines=$refusal_count consumer_entered=0 consumer_executed=0"
    rm -rf "$scratch"
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
    # Enumerate through GIT, not a bare filesystem walk.
    #
    # `find` reported every file ON DISK, so any ignored build output under
    # tests/ failed this gate: __pycache__, .pytest_cache, coverage data, editor
    # swap files, core dumps. That conflates "exists on disk" with "must be
    # inventoried", and it made the gate depend on checkout state rather than on
    # repository content -- the same commit passed in a fresh worktree and failed
    # in a checkout where a tool had run.
    #
    # It is a RECURRING self-inflicted red, not a one-off: `make validate-kvm`
    # and `make validate-dbt` both run `python3 tests/backend-parity/run_matrix.py`,
    # which creates tests/backend-parity/__pycache__. Running a per-backend
    # validate therefore reds the next full validate's metadata gate, via a file
    # that .gitignore hides so `git status` still reads clean.
    #
    # `--cached --others --exclude-standard` is tracked files PLUS genuinely new
    # untracked ones, MINUS ignored output. This does not relax the check: a new
    # undispositioned test file is still caught by `--others`. Verified three
    # ways -- on a clean tree the two enumerations are byte-identical (518 files);
    # with a planted `__pycache__/*.pyc` only `find` reports it; with a planted
    # new `tests/*.c` both still report it.
    git -C "$ROOT_DIR" ls-files --cached --others --exclude-standard -- tests \
        | LC_ALL=C sort >"$expected"
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

# READ the inner DAG-launcher budget out of the workflow instead of keeping a
# second copy of the number here.
#
# A hand-copied constant IS a derived identity, and a silent duplicate of one is
# how a raised budget keeps a guard that still validates the value nobody
# deploys. This function existed as `local privileged_inner_timeout_seconds=360`
# until 2026-08-08; raising the workflow to 600s while leaving that line at 360
# would have left the launcher/critical-path bound checking a number absent from
# the repository, and it would have passed. Derive, never transcribe.
function workflow_dag_launcher_timeout_seconds {
    local workflow=$1 lane=$2
    sed -n \
        "s|.*timeout --foreground --kill-after=[0-9]*s \([0-9]*\)s env SAFE_CI_DAG_RUNNER=.*ci/run-dag\.sh $lane .*|\1|p" \
        "$workflow" | head -1
}

# --------------------------------------------------------------------------
# A DECLARED BUDGET THAT CANNOT FIRE IS NOT A BUDGET; IT IS DOCUMENTATION.
#
# A node whose wall budget is >= the kill of the job that runs it can NEVER time
# out on its own terms. The job dies first, every time, and the node is never
# blamed -- by its own declaration it had not overrun. That is not a cosmetic
# mis-ordering: it is a structural reason a slow node cannot be identified. Hours
# were spent trying to pin an overrun in the portable lane before anyone noticed
# that seventeen of its twenty-two sharded nodes were in exactly this state, and
# that the only timeout able to fire there was GitHub's, which names nothing.
#
# The predicate is `>=`, not `>`. A node whose budget EQUALS the job kill still
# cannot fire first, because the node starts only after checkout, build and setup
# -- measured at 108s into one such job -- so the job's clock always wins.
#
# DERIVED, NEVER TRANSCRIBED, like every other bound in this file: the node set
# comes from the audited shard map, the budgets from the audited DAG, and the
# kills from the workflow's own timeout-minutes and launcher wrappers. Nothing
# here holds a second copy of a number that lives somewhere else.
# --------------------------------------------------------------------------

# "<node>\t<effective wall seconds>" for every step in a lane DAG.
#
# AN UNKNOWN BUDGET IS NOT A SAFE BUDGET. This defaulted a missing `.timeout`
# AND a missing lane `.default_step_timeout` to 0, and 0 compares as comfortably
# below every job kill -- so a node whose budget nobody could derive scored as a
# node that provably fits. That is the "nothing to check became check passed"
# shape this whole section exists to refuse, one level up from the inversion it
# was written to catch. Both DAGs currently DO set `default_step_timeout`, so
# nothing hits it today; that is exactly why it would have rotted unnoticed.
# Emit a typed marker instead and let the caller refuse.
function dag_node_budgets {
    local dag=$1
    jq -r '(.default_step_timeout) as $d
           | .steps[]
           | "\(.group).\(.job)\t\(.timeout // $d // "UNDERIVABLE")"' "$dag"
}

# "<node>\t<job>" for the portable lane, from the audited shard map. Portable
# fans its DAG across jobs, so a node's enclosing kill is its SHARD's job, not
# one lane-wide number.
function portable_node_jobs {
    local shards=$1
    jq -r '(.debug_shards[]?   | .nodes[] | . + "\ttest-debug"),
           (.release_shards[]? | .nodes[] | . + "\ttest-release")' "$shards"
}

# Emit "<node> <declared>s >= <bound>s (<what bounds it>)" for every inversion.
#
# TAKES THE TREE AS AN ARGUMENT rather than reading $ROOT_DIR, so the bracket below can point it
# at a temp copy. $ROOT_DIR and $DAG_ROOT are `readonly` (see the declaration near the top of this
# file), and bash refuses to reassign a readonly variable even inside a subshell -- so a bracket
# that tried to rebind them could never run at all, and its CONTROL case would die before
# exercising anything. An untestable guard is the thing this whole section exists to prevent.
function budget_inversions {
    local root=${1:-$ROOT_DIR}
    local dags=${2:-$DAG_ROOT}
    local workflow_portable="$root/.github/workflows/ci-portable.yml"
    local workflow_privileged="$root/.github/workflows/ci-privileged.yml"
    local shards="$root/ci/portable-shards.json"

    local debug_kill release_kill
    debug_kill=$(( $(workflow_job_timeout_minutes "$workflow_portable" test-debug) * 60 ))
    release_kill=$(( $(workflow_job_timeout_minutes "$workflow_portable" test-release) * 60 ))
    (( debug_kill > 0 && release_kill > 0 )) ||
        die "could not read test-debug/test-release timeout-minutes from ci-portable.yml"

    join -t $'\t' \
        <(portable_node_jobs "$shards" | sort) \
        <(dag_node_budgets "$dags/portable.json" | sort) |
        awk -F'\t' -v dk="$debug_kill" -v rk="$release_kill" '
            { bound = ($2 == "test-debug") ? dk : rk }
            $3 >= bound { printf "%s %ss >= %ss (job %s timeout-minutes)\n", $1, $3, bound, $2 }
        '

    # Privileged runs its whole DAG inside one launcher wrapper, which is a
    # TIGHTER bound than the job -- so the wrapper is what must be beaten.
    local launcher
    launcher=$(workflow_dag_launcher_timeout_seconds "$workflow_privileged" privileged)
    [[ -n $launcher ]] ||
        die "could not read the privileged DAG launcher timeout from ci-privileged.yml"
    dag_node_budgets "$dags/privileged.json" |
        awk -F'\t' -v bound="$launcher" '
            $2 >= bound { printf "%s %ss >= %ss (privileged launcher wrapper)\n", $1, $2, bound }
        '
}

# Fail on any inversion that is not in the recorded baseline, and on any baseline
# entry that has been fixed.
#
# WHY A BASELINE AND NOT A FLAT REFUSAL. The inversions are real and present, and
# whether to fix them by raising a job's timeout-minutes or by lowering node
# budgets is a CI policy decision with real consequences that this guard does not
# get to make. Refusing outright would block every PR on someone else's decision;
# recording the inventory adopts the gate now, keeps each entry visible with both
# numbers, and makes the count monotonically DECREASE -- a fixed node that stays
# listed fails too, so the list cannot rot into permanent permission.
function assert_node_budgets_fit_their_job_kill {
    local root=${1:-$ROOT_DIR}
    local dags=${2:-$DAG_ROOT}
    local baseline="$root/ci/budget-inversions-baseline.txt"
    [[ -f $baseline ]] || die "missing budget-inversion baseline: ${baseline#"$root/"}"

    # REFUSE AN UNDERIVABLE BUDGET BEFORE COMPARING ANYTHING. A node with no
    # `.timeout` and no lane `.default_step_timeout` cannot be shown to fit its
    # job kill, and must not be silently counted among the nodes that do.
    local -a underivable=()
    mapfile -t underivable < <(
        { dag_node_budgets "$dags/portable.json"
          dag_node_budgets "$dags/privileged.json"
        } | awk -F'\t' '$2 == "UNDERIVABLE" { print $1 }' | sort -u
    )
    if ((${#underivable[@]})); then
        printf 'node(s) with NO derivable wall budget (neither .timeout nor the lane default):\n' >&2
        printf '  %s\n' "${underivable[@]}" >&2
        die "an underivable node budget cannot be proven to fit its job kill; set .timeout on the node or .default_step_timeout on the lane"
    fi

    local -a current=() expected=() unlisted=() fixed=()
    mapfile -t current < <(budget_inversions "$root" "$dags" | sort)
    mapfile -t expected < <(grep -Ev '^[[:space:]]*(#|$)' "$baseline" | sort)

    mapfile -t unlisted < <(comm -23 <(printf '%s\n' "${current[@]}") <(printf '%s\n' "${expected[@]}"))
    mapfile -t fixed    < <(comm -13 <(printf '%s\n' "${current[@]}") <(printf '%s\n' "${expected[@]}"))

    if ((${#unlisted[@]})); then
        printf 'NEW budget inversion (a node that can never fire its own timeout):\n' >&2
        printf '  %s\n' "${unlisted[@]}" >&2
        die "declared node budget is >= the kill of the job that runs it; lower the node budget, or raise that job's timeout-minutes and record the decision"
    fi
    if ((${#fixed[@]})); then
        printf 'budget inversion(s) FIXED but still listed in the baseline:\n' >&2
        printf '  %s\n' "${fixed[@]}" >&2
        die "remove them from ci/budget-inversions-baseline.txt so the list can only shrink"
    fi
    # NO SILENT COVERAGE. The join can only judge nodes whose enclosing job is knowable from a
    # tracked file: the portable TEST shards and the whole privileged lane. Nodes dispatched by a
    # matrix computed at run time (the e2e fan-out) have no static job mapping, so they are NOT
    # checked -- say how many, rather than let a partial audit read as a complete one.
    local checked total
    checked=$(( $(portable_node_jobs "$root/ci/portable-shards.json" | wc -l) \
                + $(dag_node_budgets "$dags/privileged.json" | wc -l) ))
    total=$(( $(dag_node_budgets "$dags/portable.json" | wc -l) \
              + $(dag_node_budgets "$dags/privileged.json" | wc -l) ))
    printf 'budget ordering: %d node(s) still declare a budget >= their job kill (baseline; each can never be blamed for its own overrun); %d of %d DAG nodes have a statically knowable job kill and were checked\n' \
        "${#current[@]}" "$checked" "$total"
}

# TWO-SIDED BRACKET FOR THE GUARD ABOVE, run inert against COPIES in a temp tree.
#
# A refusal that fires on everything is as useless as one that fires on nothing, so both
# directions are exercised: a planted inversion must be REFUSED with its numbers, a
# correctly-ordered stack must PASS, and a baseline entry that no longer inverts must also be
# refused so the list cannot rot into permanent permission. Nothing here touches the real DAGs,
# workflows or baseline.
function assert_budget_guard_brackets {
    local tmp
    tmp=$(mktemp -d) || die "budget guard bracket: mktemp failed"
    mkdir -p "$tmp/ci/dag" "$tmp/.github/workflows"
    cp "$ROOT_DIR/ci/portable-shards.json" "$tmp/ci/portable-shards.json"
    cp "$DAG_ROOT/portable.json" "$tmp/ci/dag/portable.json"
    cp "$DAG_ROOT/privileged.json" "$tmp/ci/dag/privileged.json"
    cp "$ROOT_DIR/.github/workflows/ci-portable.yml" "$tmp/.github/workflows/ci-portable.yml"
    cp "$ROOT_DIR/.github/workflows/ci-privileged.yml" "$tmp/.github/workflows/ci-privileged.yml"
    cp "$ROOT_DIR/ci/budget-inversions-baseline.txt" "$tmp/ci/budget-inversions-baseline.txt"

    # Run the guard against the temp tree, reporting only its exit status.
    #
    # The tree is PASSED as arguments, not rebound: $ROOT_DIR/$DAG_ROOT are readonly and bash
    # refuses to reassign a readonly variable even inside a subshell.
    #
    # The SUBSHELL is still load-bearing and must stay: `die` is `exit 2`, so a refusal invoked
    # directly would terminate the whole harness instead of returning a status this bracket can
    # assert on. Subshell for containment, arguments for redirection -- both, not either.
    _budget_guard_in() (
        assert_node_budgets_fit_their_job_kill "$tmp" "$tmp/ci/dag" >/dev/null 2>&1
    )

    # CONTROL: the copies are consistent with their own baseline, so this must PASS. Without it,
    # the two refusals below are satisfiable by a guard that rejects everything.
    _budget_guard_in || die "budget guard bracket: an unmodified copy must pass"

    # POSITIVE: plant ONE new inversion by raising a node that currently fits. The guard must
    # refuse, because that node could no longer be blamed for its own overrun.
    local planted="$tmp/ci/dag/portable.json"
    jq '(.steps[] | select(.group == "test" and .job == "detcore_misc") | .timeout) = 5400' \
        "$planted" > "$planted.new" && mv "$planted.new" "$planted"
    ! _budget_guard_in || die "budget guard bracket: a planted 5400s node under a 900s job must be REFUSED"
    # ...and it must NAME it, with both numbers, or the refusal is unactionable.
    local report
    report=$( (assert_node_budgets_fit_their_job_kill "$tmp" "$tmp/ci/dag") 2>&1 || true)
    [[ $report == *"test.detcore_misc 5400s >= 900s"* ]] ||
        die "budget guard bracket: the refusal must name the node and both numbers, got: $report"
    cp "$DAG_ROOT/portable.json" "$planted"

    # NEGATIVE, the other rot direction: a baseline entry that no longer inverts must ALSO be
    # refused, so a fix must delete its line and the inventory can only shrink.
    printf 'fictional.node 1s >= 900s (job test-debug timeout-minutes)\n' \
        >> "$tmp/ci/budget-inversions-baseline.txt"
    ! _budget_guard_in || die "budget guard bracket: a stale baseline entry must be REFUSED"
    cp "$ROOT_DIR/ci/budget-inversions-baseline.txt" "$tmp/ci/budget-inversions-baseline.txt"

    # NEGATIVE, the UNKNOWN-IS-NOT-SAFE direction: strip a node's own timeout AND the lane
    # default so no budget can be derived for it. The old code scored that node 0s -- safely
    # under every job kill -- and passed. It must now be REFUSED and must NAME the node,
    # otherwise "nobody could work out this node's budget" reads as "this node is fine".
    jq 'del(.default_step_timeout) | (.steps[] | select(.group == "test" and .job == "detcore_parallel") | .timeout) |= empty' \
        "$DAG_ROOT/portable.json" > "$tmp/ci/dag/portable.json"
    ! _budget_guard_in || die "budget guard bracket: a node with NO derivable budget must be REFUSED, not scored 0"
    report=$( (assert_node_budgets_fit_their_job_kill "$tmp" "$tmp/ci/dag") 2>&1 || true)
    [[ $report == *"test.detcore_parallel"* && $report == *"UNDERIVABLE"* || $report == *"test.detcore_parallel"* ]] ||
        die "budget guard bracket: the underivable-budget refusal must name the node, got: $report"
    cp "$DAG_ROOT/portable.json" "$tmp/ci/dag/portable.json"

    # CONTROL again: with the real files restored the guard must pass, proving the two
    # refusals above came from the planted conditions and not from a guard stuck refusing.
    _budget_guard_in || die "budget guard bracket: restored copies must pass again"

    rm -rf "$tmp"
    unset -f _budget_guard_in
}

# Everything ELSE the job can spend, summed from the workflow's own step
# budgets, so the outer wall is checked against what this job may actually
# consume rather than a hand-picked overhead constant that nobody rechecks.
# Adding another budgeted step automatically raises the required job timeout.
function workflow_non_dag_step_budget_seconds {
    local workflow=$1
    awk '
        /SAFE_CI_DAG_RUNNER/ { next }
        match($0, /timeout --foreground --kill-after=[0-9]+s [0-9]+s/) {
            segment = substr($0, RSTART, RLENGTH)
            fields = split(segment, parts, " ")
            budget = parts[fields]
            sub(/s$/, "", budget)
            total += budget
        }
        END { print total + 0 }
    ' "$workflow"
}

# The strict-compat lane used to permit 1800s internally inside a 900s hosted
# job. The external kill necessarily won, leaving neither a named probe nor its
# per-probe rows. Assert the deployed values, not hand-copied policy numbers.
function assert_strict_compat_budget_ladder {
    local workflow=$1
    local cmd gate_seconds run_seconds node_seconds job_seconds
    cmd=$(jq -r '.steps[] | select(.group == "test" and .job == "strict_compat") | .cmd' \
        "$DAG_ROOT/portable.json")
    gate_seconds=$(sed -n 's/.*VALIDATE_GATE_TIMEOUT_SECONDS=\([0-9][0-9]*\).*/\1/p' <<<"$cmd")
    run_seconds=$(sed -n 's/.*HERMIT_VALIDATE_RUN_TIMEOUT_SECONDS=\([0-9][0-9]*\).*/\1/p' <<<"$cmd")
    node_seconds=$(jq -r '.steps[] | select(.group == "test" and .job == "strict_compat") | .timeout' \
        "$DAG_ROOT/portable.json")
    job_seconds=$(( $(workflow_job_timeout_minutes "$workflow" test-debug) * 60 ))

    [[ $gate_seconds =~ ^[1-9][0-9]*$ && $run_seconds =~ ^[1-9][0-9]*$ && \
       $node_seconds =~ ^[1-9][0-9]*$ && $job_seconds =~ ^[1-9][0-9]*$ ]] ||
        die "strict-compat must expose numeric gate/run/node/job budgets"
    ((gate_seconds < run_seconds && run_seconds < node_seconds && node_seconds < job_seconds)) ||
        die "strict-compat budget ladder must satisfy gate < whole-run < node < hosted job; got ${gate_seconds} < ${run_seconds} < ${node_seconds} < ${job_seconds}"

    # The scope backstop is derived as run + max(60, run/10), so the deployed
    # 600s run yields 660s. Keep that strictly below the 720s node boundary.
    ((run_seconds + 60 < node_seconds)) ||
        die "strict-compat scope backstop must fit between whole-run and node budgets"
    [[ $(grep -Fxc 'const COMPAT_DIAGNOSTIC_WALL_S: i64 = 420;' "$ROOT_DIR/scripts/validate.rs") == 1 ]] ||
        die "strict-compat heavy prep must retain its measured 420s inner bound"
    grep -Fq 'run_dag_boxed_deadline' "$ROOT_DIR/scripts/validate.rs" ||
        die "strict-compat whole-run budget has no in-process deadline consumer"
    grep -Fq 'STEP_STARTED_MONOTONIC_NS_ENV' "$ROOT_DIR/scripts/validate.rs" ||
        die "strict-compat starts a fresh inner clock instead of inheriting the outer node epoch"
    grep -Fq 'expected_scope_runtime_max_s' "$ROOT_DIR/scripts/validate.rs" &&
        grep -Fq 'verify_scope_runtime_max' "$ROOT_DIR/scripts/validate.rs" ||
        die "strict-compat requests a scope RuntimeMaxSec without reading the live value back"
    grep -Fq 'forward_step_profiles(&first, jobs)' "$ROOT_DIR/scripts/validate.rs" ||
        die "strict-compat inner rows are not forwarded to the hosted artifact directory"
    [[ $cmd == *'SAFE_CI_DAG_RUNNER_LOG_DIR="${RUN_NODE_PERF_DIR:-$PWD/ignored/ci/perf/strict-compat}/logs"'* ]] ||
        die "strict-compat per-probe logs are not routed into the always-uploaded shard artifact"
    [[ $(grep -Fxc '        if: always()' "$workflow") -ge 1 ]] ||
        die "portable workflow must upload diagnostic artifacts on failed shards"
    grep -Fq 'pub fn run_dag_boxed_deadline' \
        "$ROOT_DIR/agent-utils/rs/safe-ci-dag-runner/src/scheduler.rs" ||
        die "the pinned safe-ci runner does not implement the whole-run deadline"
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

    assert_strict_compat_budget_ladder "$workflow"

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
        ($shards[0].build_dbt_nodes + $shards[0].build_aux_nodes) as $selected
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
    [[ $(grep -Fxc '            target/install_pkg/rsrcs/libdetcore_dbt.so \' "$workflow") == 1 ]] ||
        die "GitHub portable debug artifact must preserve the installed DBT runtime"
    [[ $(grep -Fxc '          test -f target/install_pkg/rsrcs/libdetcore_dbt.so' "$workflow") == 1 ]] ||
        die "GitHub portable debug shards must verify the installed DBT runtime"
    [[ $(grep -Fxc '          test -f target/debug/deps/libdetcore_dbt.so' "$workflow") == 2 ]] ||
        die "GitHub portable workflow must package and verify the debug DBT cdylib"
    [[ $(grep -Fxc '            target/ci \' "$workflow") == 1 ]] ||
        die "GitHub portable release artifact must transport the strict-compat Hermit"
    [[ $(grep -Fxc '          test -x target/ci/hermit-strict' "$workflow") == 1 ]] ||
        die "GitHub portable debug shards must verify the strict-compat Hermit"
    # Both the debug test shards (run_dbt_* CLI tests) and the e2e backend cells
    # consume the DBT install package built by build-release, so both must wait on
    # [select, build-debug, build-release]. (select gates the affected-test matrix;
    # dropping build-release from either would race the DBT runtime.)
    [[ $(grep -Fxc '    needs: [select, build-debug, build-release]' "$workflow") == 2 ]] ||
        die "GitHub portable debug and e2e shards must wait for the complete DBT install package"
    [[ $(grep -Fxc '          test -x target/install_pkg/rsrcs/dynamorio/bin64/drrun' "$workflow") == 1 ]] ||
        die "GitHub portable debug shards must verify the DynamoRIO launcher"
    [[ $(grep -Fxc '          test -f target/install_pkg/rsrcs/libreverie_dbt_client.so' "$workflow") == 1 ]] ||
        die "GitHub portable debug shards must verify the DynamoRIO client"
    [[ $(grep -Fxc '        run: ./ci/publish-hermit-e2e-artifact.sh target/debug/hermit target/ci/hermit-e2e-artifacts target/ci/hermit-e2e-artifact.path target/install_pkg' "$workflow") == 1 ]] ||
        die "GitHub portable e2e cells must publish one verified immutable Hermit artifact after unpack"
    [[ $(grep -Fxc '          ./ci/run-with-hermit-e2e-artifact.sh --require-install "${args[@]}" \' "$workflow") == 1 ]] ||
        die "GitHub portable e2e cells must consume the verified immutable Hermit artifact"
    [[ $(grep -Fxc '      HERMIT_BIN: ${{ github.workspace }}/target/release/hermit' "$workflow") == 1 ]] ||
        die "only the non-gating SaBRe diagnostic may use the mutable release Hermit directly"
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
        | .nodes
        | index("test.cli") != null
          and index("build.e2e_artifact") != null
          and index("test.applications_e2e") != null
    ' "$ROOT_DIR/ci/portable-shards.json" >/dev/null ||
        die "GitHub portable integration shard must retain CLI tests and execute the immutable artifact producer with its applications consumer"
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

# The production validation driver is scripts/validate.rs. The retired shell
# implementation must stay absent; the root validate.sh is an exact, behaviorless
# reminder alias, while Make, workflows, and DAGs call Rust directly.
#
# These assertions replace the former `assert_validate_entrypoint` audits over
# bash function bodies. The property audited is unchanged and is the one that
# matters: a validate profile's node set comes from the AUDITED DAG FILES and
# from nowhere else, so no local path can quietly run a different or smaller
# suite than the one CI runs.
function validate_reminder_shim_is_exact {
    local shim=$1

    [[ -f $shim && ! -L $shim && -x $shim ]] || return 1
    cmp -s "$shim" <(printf '%s\n' \
        '#!/usr/bin/env bash' \
        '# The local validation ledger is the landing authority.' \
        'exec ./scripts/validate.rs "$@"')
}

function assert_validate_driver_entrypoint {
    local shim="$ROOT_DIR/validate.sh"
    local mutated_shim
    local plan_src="$ROOT_DIR/scripts/lib/validate_plan.rs"
    local driver="$ROOT_DIR/scripts/validate.rs"

    validate_reminder_shim_is_exact "$shim" ||
        die "validate.sh must remain the exact executable reminder alias for scripts/validate.rs"
    mutated_shim=$(mktemp)
    cp "$shim" "$mutated_shim"
    printf '%s\n' 'echo unexpected-wrapper-behavior' >>"$mutated_shim"
    chmod +x "$mutated_shim"
    if validate_reminder_shim_is_exact "$mutated_shim"; then
        rm -f "$mutated_shim"
        die "the validate.sh audit must reject behavior beyond direct Rust-driver delegation"
    fi
    rm -f "$mutated_shim"
    [[ -x $driver ]] ||
        die "scripts/validate.rs must be the executable validation entrypoint"
    ! jq -r '.steps[].cmd' "$ROOT_DIR/ci/dag/portable.json" | grep -Fq './validate.sh' ||
        die "the portable DAG must invoke the Rust validation driver directly"
    [[ $(grep -Fc 'run: ./scripts/validate.rs' "$ROOT_DIR/.github/workflows/validation-levels.yml") == 3 ]] ||
        die "every validation-level workflow route must invoke the Rust driver directly"
    [[ $(grep -Fxc $'\t./scripts/validate.rs $(ARGS)' "$ROOT_DIR/Makefile") == 1 ]] ||
        die "make validate must invoke the Rust driver directly"
    # ONE place resolves a lane's node set, and it is the audited DAG file.
    [[ $(grep -Fc 'root.join("ci").join("dag").join(format!("{lane}.json"))' "$plan_src") == 1 ]] ||
        die "the validate driver must resolve every CI lane from exactly one place: ci/dag/<lane>.json"
    # And the profile->lane table names both audited lanes, once each, with the
    # full profile delegating to BOTH.
    [[ $(grep -Fxc '        (Some(Focused::PrivilegedOnly), _) => vec!["privileged"],' "$driver") == 1 ]] ||
        die "validate --privileged-only must delegate to the audited privileged DAG"
    [[ $(grep -Fxc '        (None, Level::PortableOnly) => vec!["portable"],' "$driver") == 1 ]] ||
        die "validate --portable-only must delegate to the audited portable DAG"
    [[ $(grep -Fxc '        (None, Level::Full) => vec!["portable", "privileged"],' "$driver") == 1 ]] ||
        die "the default full validation must delegate to BOTH audited DAGs"
    # No substitute profile: an unplannable request refuses rather than silently
    # running a different gate set under the requested name.
    grep -Fq 'refusing to substitute another profile' "$driver" ||
        die "the validate driver must refuse an unplannable profile, never substitute one"
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
    # Both hook invocations must bind the launcher to the exact repository
    # rather than relying on cwd -- that is the original intent of this
    # assertion, preserved across the 2026-08-08 relaxation.
    [[ $(grep -Fc -- '--repo "$root"' "$ROOT_DIR/.githooks/pre-commit") == 2 ]] ||
        die "pre-commit hook must bind the launcher to the exact repository (both invocations)"
    # The OFFLINE leg is the only hard blocker: local incoherence never needs
    # the network and is never fixed by waiting.
    [[ $(grep -Fxc '"$checker" --repo "$root" --offline || exit 1' "$ROOT_DIR/.githooks/pre-commit") == 1 ]] ||
        die "pre-commit hook must block on the offline local-coherence leg"
    # The ADVISORY leg must NOT be a hard refusal. Owner ruling 2026-08-08: this
    # surface is awareness only; enforcement is CI's check.reverie_pin. If this
    # ever regains `|| exit 1` it is a hard blocker again and the CI-config
    # commit that touched zero Cargo files starts being refused once more.
    [[ $(grep -Fxc '"${proxy[@]}" "$checker" --repo "$root" --staged-pin-advisory || advisory_status=$?' "$ROOT_DIR/.githooks/pre-commit") == 1 ]] ||
        die "pre-commit advisory leg must capture its status, never propagate it as a refusal"
    # The validate driver's Reverie-pin preflight node. It builds the command
    # string, so the audited literal is the format template — which still pins
    # both halves of the property: the canonical launcher, bound to an explicit
    # `--repo` rather than to whatever directory the node inherits.
    [[ $(grep -Fc '{proxy}{root}/ci/run-reverie-pin-check.sh --repo {root}' \
        "$ROOT_DIR/scripts/lib/validate_plan.rs") == 1 ]] ||
        die "the validate Reverie-pin gate must use the exact-repository launcher"
    ! grep -F 'run-reverie-pin-check.sh' "$ROOT_DIR/scripts/lib/validate_plan.rs" |
        grep -Fqv -- '--repo' ||
        die "every validate Reverie-pin invocation must bind --repo; found an unbound one"
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
        local fake_cargo staged_runtime direct_status reverie_fixture old_pin
        local allowed_cpu parallel_index parallel_failures shared_failures
        local -a checker_pids
        scratch=$(mktemp -d)
        trap 'rm -rf -- "$scratch"' EXIT
        isolated_path="$scratch/bin"
        fixture="$scratch/hermit"
        real_git=$(command -v git)
        # A real local Reverie history: `old` then `current` on main, so the
        # ancestry bracket has genuine reachability to judge. `stale` stays a
        # synthetic SHA that is on NO history -- the off-history negative.
        reverie_fixture="$scratch/reverie"
        stale=89abcdef0123456789abcdef0123456789abcdef
        mkdir -p "$isolated_path" "$fixture" "$reverie_fixture"
        "$real_git" -C "$reverie_fixture" init -q
        "$real_git" -C "$reverie_fixture" config user.email pin@harness.local
        "$real_git" -C "$reverie_fixture" config user.name "Pin Harness"
        printf 'old\n' >"$reverie_fixture/revision"
        "$real_git" -C "$reverie_fixture" add revision
        "$real_git" -C "$reverie_fixture" commit -qm old
        old_pin=$("$real_git" -C "$reverie_fixture" rev-parse HEAD)
        printf 'current\n' >"$reverie_fixture/revision"
        "$real_git" -C "$reverie_fixture" add revision
        "$real_git" -C "$reverie_fixture" commit -qm current
        "$real_git" -C "$reverie_fixture" branch -M main
        current=$("$real_git" -C "$reverie_fixture" rev-parse HEAD)
        ln -s "$(command -v rustc)" "$isolated_path/rustc"
        if PATH="$isolated_path:/usr/bin:/bin" command -v rust-script >/dev/null; then
            die "rust-script unexpectedly present in isolated checker PATH"
        fi
        PATH="$isolated_path:/usr/bin:/bin" "$runner" --self-test >/dev/null

        # The pin gate and both DBT build children may compile the checker at
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
# ls-remote fakes the authority tip, as before. fetch is ALSO redirected now:
# ancestry and monotonicity need Reverie's COMMIT GRAPH, not just a tip, so the
# checker fetches one. Redirecting it to a local fixture keeps this bracket
# hermetic -- it must not reach the network to decide a pin question.
if [[ \${1:-} == ls-remote ]]; then
    printf '%s\trefs/heads/main\n' '$current' 
    exit 0
fi
# "git -C DIR fetch ..." puts the subcommand in \$3, not \$1 -- a positional
# check silently passes the fetch through to the REAL remote, which both breaks
# hermeticity and makes the graph disagree with the stubbed ls-remote tip.
is_fetch=0
for arg in "\$@"; do [[ \$arg == fetch ]] && { is_fetch=1; break; }; done
if ((is_fetch)); then
    rewritten=()
    for arg in "\$@"; do
        if [[ \$arg == https://github.com/*/reverie.git ]]; then
            rewritten+=('$reverie_fixture')
        else
            rewritten+=("\$arg")
        fi
    done
    exec '$real_git' "\${rewritten[@]}"
fi
exec '$real_git' "\$@"
EOF
        chmod +x "$isolated_path/git"
        "$real_git" -C "$fixture" init -q
        printf '[dependencies]\nreverie = { git = "https://github.com/rrnewton/reverie.git", rev = "%s" }\n' \
            "$current" >"$fixture/Cargo.toml"
        "$real_git" -C "$fixture" add Cargo.toml
        PATH="$isolated_path:/usr/bin:/bin" "$runner" --repo "$fixture" --no-base >/dev/null

        printf '[dependencies]\nreverie = { git = "https://github.com/rrnewton/reverie.git", rev = "%s" }\n' \
            "$stale" >"$fixture/Cargo.toml"
        if PATH="$isolated_path:/usr/bin:/bin" "$runner" --repo "$fixture" --no-base \
            >/dev/null 2>&1; then
            status=0
        else
            status=$?
        fi
        [[ $status == 1 ]] ||
            die "rustc checker off-history-pin bracket returned $status instead of 1"

        # ANCESTRY: a pin LAGGING behind main must now PASS. This assertion is
        # deliberately the inverse of the pre-2026-08-08 rule -- lagging is what
        # a pin is FOR, and requiring equality made the verdict depend on when
        # you looked rather than on the tree.
        printf '[dependencies]\nreverie = { git = "https://github.com/rrnewton/reverie.git", rev = "%s" }\n' \
            "$old_pin" >"$fixture/Cargo.toml"
        PATH="$isolated_path:/usr/bin:/bin" "$runner" --repo "$fixture" --no-base >/dev/null ||
            die "rustc checker lagging-pin bracket must PASS under ancestry"

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

    # Exactly one Reverie-pin gate in the validate driver's plan, and every lane
    # node waits on it: the archival pin is proved current BEFORE anything is
    # built or tested, on every profile.
    [[ $(grep -Fc '"Reverie pin consistency",' "$ROOT_DIR/scripts/lib/validate_plan.rs") == 1 ]] ||
        die "the validate driver must plan the latest-Reverie gate exactly once"
    [[ $(grep -Fc 'vec!["pre.reverie_pin".to_string()]' "$ROOT_DIR/scripts/lib/validate_plan.rs") == 1 ]] ||
        die "the validate manifest gate must depend on the latest-Reverie gate"
    [[ $(grep -Fc 'reverie_pin_current: pin_gate_passed' "$ROOT_DIR/scripts/validate.rs") == 1 ]] ||
        die "the Rust validate receipt must derive pin currency from the observed gate"
    [[ $(grep -Fc '"reverie_pin_current": ctx.reverie_pin_current' "$ROOT_DIR/scripts/validate.rs") == 1 ]] ||
        die "the Rust validate receipt must state whether the latest-Reverie gate passed"

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

# A tracked hermit-cli test binary absent from both the explicit CI DAG and the
# omission ledger is unaccounted, not passing. The audit derives existence from
# the tracked tree, so its ledger cannot certify its own completeness.
function audit_test_binary_registration {
    python3 "$ROOT_DIR/ci/audit-test-binary-registration.py" ||
        die "undeclared hermit-cli test binaries (see above)"
}

function audit_ci_correspondence {
    local lane dag

    # Both DAG launch surfaces use the explicit ordinary-launcher mode; the DBT
    # wrapper is the sole child-budget caller.
    # shellcheck disable=SC2016
    [[ $(grep -Fxc 'source "$ROOT_DIR/ci/configure-build-jobs.sh" launcher || exit $?' "$ROOT_DIR/ci/run-dag.sh") == 1 ]] ||
        die "run-dag.sh must source ordinary build-job configuration exactly once"
    # shellcheck disable=SC2016
    [[ $(grep -Fxc 'source "$ROOT_DIR/ci/configure-build-jobs.sh" launcher || exit $?' "$ROOT_DIR/ci/run-node.sh") == 1 ]] ||
        die "run-node.sh must source ordinary build-job configuration exactly once"
    local budget_config="$ROOT_DIR/ci/configure-build-jobs.sh"
    local budget_wrapper="$ROOT_DIR/ci/run-with-reverie-dbt-budget.sh"
    [[ -x $budget_wrapper ]] || die "DBT child-budget wrapper must be executable"
    [[ $(grep -Fc 'reverie-dbt-budget=portable-build-child-only' "$ROOT_DIR/ci/run-dag.sh") == 1 ]] ||
        die "run-dag.sh must identify the DBT budget as portable-child-only"
    [[ $(grep -Fc 'reverie-dbt-budget=portable-build-child-only' "$ROOT_DIR/ci/run-node.sh") == 1 ]] ||
        die "run-node.sh must identify the DBT budget as portable-child-only"
    [[ $(grep -Fxc 'source "$ROOT_DIR/ci/configure-build-jobs.sh" reverie-dbt-budget-child' "$budget_wrapper") == 1 ]] ||
        die "DBT wrapper must select the explicit portable child-budget mode"
    [[ $(grep -Fxc '    "$ROOT_DIR/ci/run-reverie-pin-check.sh" --repo "$ROOT_DIR" --print-pin' "$budget_wrapper") == 1 ]] ||
        die "DBT wrapper must bind its calibration through the canonical local-pin verifier"
    [[ $(grep -Fc 'c261050cfd41bec67e31bfd0cf6f56be008d0ebb' "$budget_wrapper") == 1 ]] ||
        die "DBT wrapper must name exactly one calibrated Reverie pin"
    [[ $(grep -Fc 'c261050cfd41bec67e31bfd0cf6f56be008d0ebb' "$budget_config") == 2 ]] ||
        die "DBT derivation must independently require and diagnose the calibrated Reverie pin"
    # shellcheck disable=SC2016
    local budget_record='reverie-dbt-budget={pin:$REVERIE_DBT_BUDGET_BOUND_PIN,source:$REVERIE_DBT_BUILD_JOBS_SOURCE,raw-build-jobs:$REVERIE_DBT_RAW_BUILD_JOBS,effective-cpus-source:$REVERIE_DBT_EFFECTIVE_CPUS_SOURCE,effective-cpus:$REVERIE_DBT_EFFECTIVE_CPUS,reverie-max-jobs:$REVERIE_DBT_MAX_PARALLEL_JOBS,effective-native-jobs:$REVERIE_DBT_EFFECTIVE_BUILD_JOBS,effective-job-seconds:$REVERIE_DBT_MAX_BUILD_EFFECTIVE_JOB_SECONDS,max-elapsed-seconds:$REVERIE_DBT_MAX_BUILD_SECONDS,basis:github-portable-cold-miss-n3-affinity4,carried-to-pin-on-dynamorio-recipe-key:63e29544455c901f05e37224b52e7f9734480d7c05914083bdcbd335968e6429}'
    [[ $(grep -Fc "$budget_record" "$budget_wrapper") == 1 ]] ||
        die "DBT child wrapper must log the pin and every derivation condition"

    jq -e '
        [.steps[] | select(
            .group == "build"
            and (.job == "workspace" or .job == "runtime_release")
            and (.cmd | contains("./ci/run-with-reverie-dbt-budget.sh cargo build"))
            and .timeout >= 1200
        )] | length == 2
    ' "$DAG_ROOT/portable.json" >/dev/null ||
        die "portable DBT builds must derive inside the child and allow 1050s DBT + 150s overhead"
    # A leading `CARGO_BUILD_JOBS=<N>` prefix is the step's REQUESTED build width, but it
    # reaches only Cargo -- it declares nothing to safe-ci. Since agent-utils ada564d the
    # runner boxes an UNDECLARED step at DEFAULT_SMALL_CPU_COUNT=1 core, so such a step asks
    # for N workers and is granted one, while ci/run-with-reverie-dbt-budget.sh still derives
    # its DynamoRIO ratchet from N. That mismatch is what reddened doc.rustdoc on 2026-08-10.
    #
    # Enforce the pairing as a RULE over every step with that prefix, not as per-step cases,
    # so a step added tomorrow is covered without editing this audit. Two halves:
    #   preferred_inner_jobs == N  -- grant the cores the command asks for.
    #   jobs_flag == ""            -- keep the declaration cgroup-only. The width is already
    #                                 in the environment prefix, and the runner would
    #                                 otherwise APPEND `-j N` to the command; on three of
    #                                 these steps that lands after `--` and reaches libtest
    #                                 or rustc, and on the nextest step `-j` means
    #                                 test-execution threads rather than build jobs.
    local width_rule width_contract width_population width_steps plant
    width_rule='[ .steps[]
        | select((.cmd // "") | test("^CARGO_BUILD_JOBS=[0-9]+ "))
        | . as $s
        | (($s.cmd | capture("^CARGO_BUILD_JOBS=(?<n>[0-9]+) ").n | tonumber) as $n
           | select($s.hint.preferred_inner_jobs != $n or $s.jobs_flag != "")
           | "\($s.group).\($s.job)")
    ]'
    width_contract="($width_rule | length) == 0"
    width_population='[.steps[] | select((.cmd // "") | test("^CARGO_BUILD_JOBS=[0-9]+ "))] | length'
    # Non-vacuity floor. This is the half of the bracket that stops a step ESCAPING the rule:
    # dropping the CARGO_BUILD_JOBS prefix would silently remove a step from the rule's
    # population and return it to the one-core default, so the count may grow but not shrink.
    width_steps=$(jq "$width_population" "$DAG_ROOT/portable.json")
    [[ $width_steps -ge 7 ]] ||
        die "CARGO_BUILD_JOBS width rule inspected only $width_steps step(s); expected at least 7"
    jq -e "$width_contract" "$DAG_ROOT/portable.json" >/dev/null ||
        die "every step whose command starts with CARGO_BUILD_JOBS=<N> must declare hint.preferred_inner_jobs == N and jobs_flag \"\"; offenders: $(jq -c "$width_rule" "$DAG_ROOT/portable.json")"
    # Negative brackets: each plant is a well-shaped violation the rule must refuse.
    for plant in \
        '(.steps[] | select((.cmd // "") | test("^CARGO_BUILD_JOBS=[0-9]+ ")) | .hint) |= del(.preferred_inner_jobs)' \
        '(.steps[] | select((.cmd // "") | test("^CARGO_BUILD_JOBS=[0-9]+ ")) | .hint.preferred_inner_jobs) |= 4' \
        '(.steps[] | select((.cmd // "") | test("^CARGO_BUILD_JOBS=[0-9]+ ")) | .jobs_flag) |= "-j"'
    do
        if jq "$plant" "$DAG_ROOT/portable.json" | jq -e "$width_contract" >/dev/null; then
            die "CARGO_BUILD_JOBS width rule accepted a planted violation: $plant"
        fi
    done
    jq -e '
        [.steps[] | select(
            .group == "build" and .job == "privileged_tests"
            and .timeout == 120
            and .cmd == "CARGO_BUILD_JOBS=8 cargo build -p hermit --features third-party-backends --bin hermit && ./ci/publish-hermit-e2e-artifact.sh target/debug/hermit target/ci/hermit-e2e-artifacts target/ci/hermit-e2e-artifact.path && CARGO_BUILD_JOBS=8 cargo test -p hermit-detcore --test tests_misc --no-run"
        )] | length == 1
    ' "$DAG_ROOT/privileged.json" >/dev/null ||
        die "portable-only DBT override must not alter the privileged command or timeout"

    (
        local scratch fake_runner privileged_env privileged_node_env name
        local budget_probe budget_tuple clamp_boundaries cpu_boundaries
        local hosted_wrapper_log boxed_wrapper_log wrong_pin_log
        local planted_probe_diagnostic planted_probe_status
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
            REVERIE_DBT_BUDGET_BOUND_PIN
            REVERIE_DBT_BUILD_JOBS_SOURCE
            REVERIE_DBT_RAW_BUILD_JOBS
            REVERIE_DBT_EFFECTIVE_CPUS_SOURCE
            REVERIE_DBT_EFFECTIVE_CPUS
            REVERIE_DBT_MAX_PARALLEL_JOBS
            REVERIE_DBT_EFFECTIVE_BUILD_JOBS
            REVERIE_DBT_MAX_BUILD_EFFECTIVE_JOB_SECONDS
            REVERIE_DBT_MAX_BUILD_SECONDS
            CI_DAG_LAUNCH_WIDTH_BOUND
            CI_DAG_LAUNCH_BUILD_JOBS_SOURCE
            CI_DAG_LAUNCH_RAW_BUILD_JOBS
            CI_DAG_EFFECTIVE_CPUS
            CI_DAG_REVERIE_DBT_MAX_PARALLEL_JOBS
            CI_DAG_REVERIE_DBT_MAX_BUILD_JOB_SECONDS
            CI_DAG_REVERIE_DBT_MAX_BUILD_EFFECTIVE_JOB_SECONDS
            REVERIE_DBT_PINNED_MAX_PARALLEL_JOBS
            REVERIE_DBT_BUDGET_CHILD
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
        ) || privileged_env="[probe exited $?] $privileged_env"
        for name in "${budget_names[@]}"; do
            ! grep -q "^${name}=" <<<"$privileged_env" ||
                die "privileged DAG runner inherited portable DBT variable $name"
        done
        grep -Fxq 'CARGO_BUILD_JOBS=8' <<<"$privileged_env" ||
            die "privileged DAG runner lost the historical Cargo width"
        grep -Fxq 'THIRD_PARTY_BUILD_JOBS=8' <<<"$privileged_env" ||
            die "privileged DAG runner lost the historical native-build width"
        privileged_node_env=$(
            env "${planted_budget_env[@]}" CI_DAG_BUILD_JOBS=8 \
                SAFE_CI_DAG_RUNNER="$fake_runner" RUN_NODE_PERF_DIR="$scratch/perf" \
                "$ROOT_DIR/ci/run-node.sh" privileged build.privileged_tests 2>/dev/null
        ) || privileged_node_env="[probe exited $?] $privileged_node_env"
        for name in "${budget_names[@]}"; do
            ! grep -q "^${name}=" <<<"$privileged_node_env" ||
                die "privileged node runner inherited portable DBT variable $name"
        done
        grep -Fxq 'CARGO_BUILD_JOBS=8' <<<"$privileged_node_env" ||
            die "privileged node runner lost the historical Cargo width"
        grep -Fxq 'THIRD_PARTY_BUILD_JOBS=8' <<<"$privileged_node_env" ||
            die "privileged node runner lost the historical native-build width"

        configured_jobs=$(
            "${clean_budget_env[@]}" CI_DAG_BUILD_JOBS=5 \
                bash -c 'source "$1" launcher; printf "%s %s\n" "$CARGO_BUILD_JOBS" "$THIRD_PARTY_BUILD_JOBS"' \
                _ "$budget_config"
        ) || configured_jobs="[probe exited $?] $configured_jobs"
        [[ $configured_jobs == '5 5' ]] ||
            die "ordinary launcher did not propagate mutation K=5: $configured_jobs"

        # All budget arithmetic uses a fake `nproc`, not an input variable. This
        # brackets the production observation point at the child process itself.
        # shellcheck disable=SC2016
        budget_probe='source "$1" reverie-dbt-budget-child; printf "%s %s %s %s %s %s %s %s %s %s\n" "$REVERIE_DBT_BUILD_JOBS_SOURCE" "$REVERIE_DBT_RAW_BUILD_JOBS" "$CARGO_BUILD_JOBS" "$THIRD_PARTY_BUILD_JOBS" "$REVERIE_DBT_EFFECTIVE_CPUS_SOURCE" "$REVERIE_DBT_EFFECTIVE_CPUS" "$REVERIE_DBT_MAX_PARALLEL_JOBS" "$REVERIE_DBT_EFFECTIVE_BUILD_JOBS" "$REVERIE_DBT_MAX_BUILD_EFFECTIVE_JOB_SECONDS" "$REVERIE_DBT_MAX_BUILD_SECONDS"'
        budget_tuple=$(
            PATH="$scratch/nproc-4:$PATH" "${clean_budget_env[@]}" \
                REVERIE_DBT_BUDGET_BOUND_PIN=c261050cfd41bec67e31bfd0cf6f56be008d0ebb \
                CARGO_BUILD_JOBS=8 bash -c "$budget_probe" _ "$budget_config"
        ) || budget_tuple="[probe exited $?] $budget_tuple"
        [[ $budget_tuple == 'inherited-launch-cargo-build-jobs 8 8 8 child-nproc 4 16 4 1050 263' ]] ||
            die "hosted j8/child-CPU4 budget tuple drifted: $budget_tuple"
        budget_tuple=$(
            PATH="$scratch/nproc-64:$PATH" "${clean_budget_env[@]}" \
                REVERIE_DBT_BUDGET_BOUND_PIN=c261050cfd41bec67e31bfd0cf6f56be008d0ebb \
                SAFE_CI_IN_SCOPE=1 CARGO_BUILD_JOBS=32 \
                bash -c "$budget_probe" _ "$budget_config"
        ) || budget_tuple="[probe exited $?] $budget_tuple"
        [[ $budget_tuple == 'runner-child-cargo-build-jobs 32 32 32 child-nproc 64 16 16 1050 66' ]] ||
            die "boxed j32/child-CPU64 budget tuple drifted: $budget_tuple"

        run_audit_probe_expect_status hosted-budget-wrapper 0 hosted_wrapper_log \
            "${clean_budget_env[@]}" PATH="$scratch/nproc-4:$PATH" \
            CARGO_BUILD_JOBS=8 "$budget_wrapper" true
        [[ $hosted_wrapper_log == *'pin:c261050cfd41bec67e31bfd0cf6f56be008d0ebb,source:inherited-launch-cargo-build-jobs,raw-build-jobs:8,effective-cpus-source:child-nproc,effective-cpus:4,reverie-max-jobs:16,effective-native-jobs:4,effective-job-seconds:1050,max-elapsed-seconds:263'* ]] ||
            die "production wrapper did not log the bound hosted tuple: $hosted_wrapper_log"
        run_audit_probe_expect_status boxed-budget-wrapper 0 boxed_wrapper_log \
            "${clean_budget_env[@]}" PATH="$scratch/nproc-64:$PATH" \
            SAFE_CI_IN_SCOPE=1 CARGO_BUILD_JOBS=32 "$budget_wrapper" true
        [[ $boxed_wrapper_log == *'pin:c261050cfd41bec67e31bfd0cf6f56be008d0ebb,source:runner-child-cargo-build-jobs,raw-build-jobs:32,effective-cpus-source:child-nproc,effective-cpus:64,reverie-max-jobs:16,effective-native-jobs:16,effective-job-seconds:1050,max-elapsed-seconds:66'* ]] ||
            die "production wrapper did not log the bound boxed tuple: $boxed_wrapper_log"

        # Mutation bracket for the evidence path itself: a Rust-style panic
        # status must remain fatal while preserving both the sibling identity
        # and the captured diagnostic.  The two production probes above are the
        # positive control that expected status 0 still proceeds normally.
        cat >"$scratch/panicking-audit-probe" <<'EOF'
#!/usr/bin/env bash
echo 'planted rust panic detail' >&2
exit 101
EOF
        chmod +x "$scratch/panicking-audit-probe"
        if planted_probe_diagnostic=$(
            run_audit_probe_expect_status planted-reverie-pin-panic 0 '' \
                "$scratch/panicking-audit-probe" 2>&1
        ); then
            die "audit probe evidence bracket accepted planted exit 101"
        else
            planted_probe_status=$?
        fi
        [[ $planted_probe_status == 101 ]] ||
            die "audit probe evidence bracket returned $planted_probe_status instead of 101"
        [[ $planted_probe_diagnostic == *"audit probe 'planted-reverie-pin-panic' returned 101 (expected 0)"* ]] ||
            die "audit probe evidence bracket lost the exact sibling name: $planted_probe_diagnostic"
        [[ $planted_probe_diagnostic == *'planted rust panic detail'* ]] ||
            die "audit probe evidence bracket lost captured stderr: $planted_probe_diagnostic"

        clamp_boundaries=$(
            for requested in 15 16 17 64; do
                PATH="$scratch/nproc-64:$PATH" "${clean_budget_env[@]}" \
                    REVERIE_DBT_BUDGET_BOUND_PIN=c261050cfd41bec67e31bfd0cf6f56be008d0ebb \
                    CARGO_BUILD_JOBS=$requested bash -c "$budget_probe" _ "$budget_config"
            done
        ) || clamp_boundaries="[probe exited $?] $clamp_boundaries"
        [[ $clamp_boundaries == $'inherited-launch-cargo-build-jobs 15 15 15 child-nproc 64 16 15 1050 70\ninherited-launch-cargo-build-jobs 16 16 16 child-nproc 64 16 16 1050 66\ninherited-launch-cargo-build-jobs 17 17 17 child-nproc 64 16 16 1050 66\ninherited-launch-cargo-build-jobs 64 64 64 child-nproc 64 16 16 1050 66' ]] ||
            die "Reverie clamp boundary did not hold W at 16: $clamp_boundaries"
        cpu_boundaries=$(
            PATH="$scratch/nproc-4:$PATH" "${clean_budget_env[@]}" \
                REVERIE_DBT_BUDGET_BOUND_PIN=c261050cfd41bec67e31bfd0cf6f56be008d0ebb \
                CARGO_BUILD_JOBS=17 bash -c "$budget_probe" _ "$budget_config"
            PATH="$scratch/nproc-2:$PATH" "${clean_budget_env[@]}" \
                REVERIE_DBT_BUDGET_BOUND_PIN=c261050cfd41bec67e31bfd0cf6f56be008d0ebb \
                CARGO_BUILD_JOBS=8 bash -c "$budget_probe" _ "$budget_config"
        ) || cpu_boundaries="[probe exited $?] $cpu_boundaries"
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
        run_audit_probe_expect_status wrong-pin-budget-wrapper 2 wrong_pin_log \
            env PATH="$scratch/nproc-4:$PATH" CARGO_BUILD_JOBS=8 \
            "$fixture/ci/run-with-reverie-dbt-budget.sh" true
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
            REVERIE_DBT_BUDGET_BOUND_PIN=wrong CARGO_BUILD_JOBS=8 \
            bash -c 'source "$1" reverie-dbt-budget-child' _ "$budget_config" 2>/dev/null; then
            die "child derivation accepted an uncalibrated Reverie pin"
        fi
        if PATH="$scratch/nproc-4:$PATH" "${clean_budget_env[@]}" \
            REVERIE_DBT_BUDGET_BOUND_PIN=c261050cfd41bec67e31bfd0cf6f56be008d0ebb \
            CI_DAG_REVERIE_DBT_MAX_BUILD_JOB_SECONDS=1050 CARGO_BUILD_JOBS=8 \
            bash -c 'source "$1" reverie-dbt-budget-child' _ "$budget_config" 2>/dev/null; then
            die "child derivation accepted a retired unconditioned DBT threshold"
        fi
        if PATH="$scratch/nproc-zero:$PATH" "${clean_budget_env[@]}" \
            REVERIE_DBT_BUDGET_BOUND_PIN=c261050cfd41bec67e31bfd0cf6f56be008d0ebb CARGO_BUILD_JOBS=8 \
            bash -c 'source "$1" reverie-dbt-budget-child' _ "$budget_config" 2>/dev/null; then
            die "child derivation accepted nproc=0"
        fi
        if PATH="$scratch/nproc-invalid:$PATH" "${clean_budget_env[@]}" \
            REVERIE_DBT_BUDGET_BOUND_PIN=c261050cfd41bec67e31bfd0cf6f56be008d0ebb CARGO_BUILD_JOBS=8 \
            bash -c 'source "$1" reverie-dbt-budget-child' _ "$budget_config" 2>/dev/null; then
            die "child derivation accepted a noninteger nproc observation"
        fi
        if PATH="$scratch/nproc-4:$PATH" "${clean_budget_env[@]}" \
            REVERIE_DBT_BUDGET_BOUND_PIN=c261050cfd41bec67e31bfd0cf6f56be008d0ebb CARGO_BUILD_JOBS=0 \
            bash -c 'source "$1" reverie-dbt-budget-child' _ "$budget_config" 2>/dev/null; then
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
        'timeout --foreground --kill-after=10s 720s env SAFE_CI_DAG_RUNNER=agent-utils/py/bin/safe-ci-dag-runner ci/run-dag.sh privileged -j 2 --allow-cgroup-failure --perf-dir "$RUNNER_TEMP/hermit-privileged-dag-perf" -v'
    assert_privileged_diagnostics "$ROOT_DIR/.github/workflows/ci-privileged.yml"
    assert_validate_driver_entrypoint
    assert_node_budgets_fit_their_job_kill
    assert_budget_guard_brackets

    # This validation command contains real concurrent rustc probes. Three warm
    # samples measured 71.44-73.84s wall, with 58.36-60.38s CPU. Keep both lane
    # copies on the measured 75s workload class and the same 180s cap so a shorter
    # privileged proxy cannot reject work that passed the portable gate.
    for lane in portable privileged; do
        jq -e '
            [.steps[] | select(
                .group == "e2e"
                and .job == "metadata"
                and .cmd == "./ci/test_harness.sh validate"
                and .timeout == 180
                and .hint.est_duration_s == 75
                and .hint.hard_mem_max_bytes == 1073741824
            )] | length == 1
        ' "$DAG_ROOT/$lane.json" >/dev/null ||
            die "$lane e2e.metadata must carry the measured validation workload and 180s/1GiB bounds"
    done

    # Three budgets are nested here, and every term is DERIVED from the file
    # that authors it -- the DAG for the critical path, the workflow for both
    # timeouts. Nothing below is a transcription of a number kept elsewhere.
    local privileged_workflow="$ROOT_DIR/.github/workflows/ci-privileged.yml"
    local privileged_critical_path privileged_job_timeout_minutes
    local privileged_inner_timeout_seconds privileged_non_dag_step_budget
    local privileged_runner_overhead_seconds=30
    # Job setup, checkout, and the artifact uploads carry no `timeout` of their
    # own; they measured 7s total on a warm green run (31282467953, 2026-08-08).
    # A cold checkout and a large artifact upload are much slower, so allow 120s.
    local privileged_unbudgeted_step_allowance_seconds=120
    privileged_critical_path=$(dag_critical_path_seconds "$DAG_ROOT/privileged.json")
    privileged_inner_timeout_seconds=$(workflow_dag_launcher_timeout_seconds \
        "$privileged_workflow" privileged)
    privileged_non_dag_step_budget=$(workflow_non_dag_step_budget_seconds \
        "$privileged_workflow")
    privileged_job_timeout_minutes=$(workflow_job_timeout_minutes \
        "$privileged_workflow" privileged)
    [[ $privileged_critical_path =~ ^[0-9]+$ ]] ||
        die "privileged DAG critical path is not an integer: $privileged_critical_path"
    [[ $privileged_inner_timeout_seconds =~ ^[1-9][0-9]*$ ]] ||
        die "privileged workflow has no numeric inner DAG launcher timeout"
    [[ $privileged_non_dag_step_budget =~ ^[0-9]+$ ]] ||
        die "privileged workflow non-DAG step budget is not an integer: $privileged_non_dag_step_budget"
    [[ $privileged_job_timeout_minutes =~ ^[1-9][0-9]*$ ]] ||
        die "privileged workflow has no numeric outer job timeout"
    ((privileged_inner_timeout_seconds > privileged_critical_path + privileged_runner_overhead_seconds)) ||
        die "privileged DAG launcher (${privileged_inner_timeout_seconds}s) must exceed ${privileged_critical_path}s critical path plus ${privileged_runner_overhead_seconds}s runner overhead"
    local privileged_job_floor_seconds=$((privileged_inner_timeout_seconds
        + privileged_non_dag_step_budget
        + privileged_unbudgeted_step_allowance_seconds))
    ((privileged_job_timeout_minutes * 60 > privileged_job_floor_seconds)) ||
        die "privileged job timeout (${privileged_job_timeout_minutes}m) must exceed ${privileged_inner_timeout_seconds}s DAG launcher plus ${privileged_non_dag_step_budget}s of other budgeted steps plus ${privileged_unbudgeted_step_allowance_seconds}s for unbudgeted steps (${privileged_job_floor_seconds}s)"

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
                (if $m.lane == "portable"
                 then "./ci/run-with-hermit-e2e-artifact.sh --require-install "
                 else "./ci/run-with-hermit-e2e-artifact.sh " end)
                + "./ci/test_harness.sh run --lane \($m.lane) --category \($m.category) --ci-only --allow-empty --prebuilt --results ignored/e2e/\($m.lane)/\($m.category)/results.jsonl --junit ignored/e2e/\($m.lane)/\($m.category)/junit.xml";
            def artifact_producer($lane):
                if $lane == "portable" then "build.e2e_artifact" else "build.privileged_tests" end;
            ([.steps[] | select(.cmd | contains("./ci/test_harness.sh run "))] | all(has("manifest")))
            and ([.steps[] | select(has("manifest"))] | all(
                . as $step
                | ($step.manifest | (keys | sort) == ["category","lane"] and .lane == $lane and (.category | length > 0))
                and ($step.cmd == expected_command($step.manifest))
                and ($step.deps | index("e2e.metadata") != null)
                and ($step.deps | index("build.manifest_guests") != null)
                and ($step.deps | index(artifact_producer($lane)) != null)))
            and ([.steps[]
                    | select((.group + "." + .job) == artifact_producer($lane))
                    | select(.cmd | contains("ci/publish-hermit-e2e-artifact.sh target/debug/hermit"))
                    | select(if $lane == "portable"
                             then (.deps | index("build.workspace") != null)
                                  and (.deps | index("build.runtime_release") != null)
                                  and (.cmd | endswith(" target/install_pkg"))
                             else true end)]
                | length) == 1
            and (if $lane == "portable" then
                    ([.steps[]
                        | select(.cmd | contains("cargo "))
                        | select((.group + "." + .job) as $tag
                            | ["setup.nextest","setup.manifest_plan","build.workspace","build.runtime_release","lint.rustfmt"]
                            | index($tag) == null)]
                     | all(.deps | index("build.e2e_artifact") != null))
                 else true end)
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

# Give the timeout's owner, not the enclosing DAG node, a durable identity.
# GNU timeout's 124/137 statuses are otherwise indistinguishable from an
# arbitrary child exit once execute_attempt returns its compact TSV row.
function individual_test_timeout_reason {
    local test_id=$1 mode=$2 backend=$3 attempt=$4 timeout_seconds=$5 status=$6
    local disposition
    case $status in
        124) disposition="deadline reached (exit 124)" ;;
        137) disposition="SIGKILL after 10 s grace (exit 137)" ;;
        *) return 1 ;;
    esac
    [[ -n $backend ]] || backend=native
    printf 'test %s/%s/%s exceeded %s s in attempt %s (innermost E2E timeout: %s)' \
        "$test_id" "$mode" "$backend" "$timeout_seconds" "$attempt" "$disposition"
}

# Bracket the actual process-group wrapper and the exact message constructor.
# This runs inside the metadata audit, far below safe-ci-dag-runner's node
# timeout: the planted 2 s sleeper must be named after 1 s, while a healthy
# command under the same wrapper must remain unaffected.
function audit_innermost_e2e_timeout {
    local scratch status reason expected
    scratch=$(mktemp -d)

    status=0
    run_capture "$scratch/hang.stdout" "$scratch/hang.stderr" 1 sleep 2 || status=$?
    [[ $status == 124 ]] || {
        rm -rf "$scratch"
        die "innermost E2E negative bracket returned $status, expected timeout exit 124"
    }
    reason=$(individual_test_timeout_reason \
        timeout-probe/deliberate-hang verify ptrace 1 1 "$status")
    expected='test timeout-probe/deliberate-hang/verify/ptrace exceeded 1 s in attempt 1 (innermost E2E timeout: deadline reached (exit 124))'
    [[ $reason == "$expected" ]] || {
        rm -rf "$scratch"
        die "innermost E2E negative bracket lost test identity: $reason"
    }

    status=0
    run_capture "$scratch/pass.stdout" "$scratch/pass.stderr" 5 true || status=$?
    rm -rf "$scratch"
    [[ $status == 0 ]] || die "innermost E2E positive bracket rejected a healthy command: exit $status"
    printf 'INNERMOST-TIMEOUT negative=1 named: %s\n' "$reason"
    printf 'INNERMOST-TIMEOUT positive=1 passed within 5 s\n'
}

# Refuse a future regression from a named individual timeout back to an opaque
# safe-ci-dag-runner node kill. The 630 s floor is the 600 s slow-test override
# plus its 30 s grace; ordinary tests retain the much wider 300 s limit.
function audit_innermost_timeout_coverage {
    local config="$ROOT_DIR/.config/nextest.toml"
    "$ROOT_DIR/ci/run-nextest-counted.sh" --self-test ||
        die "counted nextest wrapper failed its two-sided parser bracket"
    grep -Fqx 'slow-timeout = { period = "300s", terminate-after = 1, grace-period = "30s" }' "$config" ||
        die "nextest default must terminate an individual test after 300 s plus 30 s grace"
    grep -Fqx 'slow-timeout = { period = "600s", terminate-after = 1, grace-period = "30s" }' "$config" ||
        die "nextest slow-test override must terminate after 600 s plus 30 s grace"

    # cargo-nextest 0.9.100 does not execute rustdoc tests. Keep `--doc` as the
    # explicit upstream limitation; `--no-run` is compilation, and the single
    # privileged `--exact` case owns an equivalent named timeout wrapper.
    jq -s -e '
        all(.[]; all(.steps[];
            if ((.cmd // "") | contains("./ci/run-nextest-counted.sh")) then
                (((.deps // []) | index("setup.nextest")) != null)
                and (.timeout > 630)
            elif ((.cmd // "") | contains("cargo nextest run")) then
                false
            elif ((.cmd // "") | contains("cargo test")) then
                ((.cmd | contains("--doc"))
                 or (.cmd | contains("--no-run"))
                 or ((.cmd | contains("--exact"))
                     and (.cmd | contains("innermost exact Cargo timeout"))))
            else true end))
    ' "$DAG_ROOT/portable.json" "$DAG_ROOT/privileged.json" >/dev/null ||
        die "an executing Cargo test lacks a named inner timeout or its DAG node is not outside the 630 s hard-kill bound"

    local id metadata lane category timeout key job outer hard_floor
    declare -A category_max=()
    for id in "${!METADATA_BY_ID[@]}"; do
        metadata=${METADATA_BY_ID[$id]}
        jq -e '[.modes[] | select(.ci == true)] | length > 0' <<<"$metadata" >/dev/null || continue
        lane=$(jq -r .lane <<<"$metadata")
        category=$(jq -r .category <<<"$metadata")
        timeout=$(jq -r .timeout_seconds <<<"$metadata")
        key="$lane/$category"
        if ((timeout > ${category_max[$key]:-0})); then
            category_max[$key]=$timeout
        fi
    done
    for key in "${!category_max[@]}"; do
        lane=${key%%/*}
        category=${key#*/}
        job="manifest_${category//-/_}"
        outer=$(jq -r --arg job "$job" \
            '.steps[] | select(.group == "e2e" and .job == $job) | .timeout' \
            "$DAG_ROOT/$lane.json")
        [[ $outer =~ ^[1-9][0-9]*$ ]] ||
            die "E2E category $key has no numeric DAG-node timeout"
        hard_floor=$((${category_max[$key]} + 10))
        ((outer > hard_floor)) ||
            die "E2E category $key node ${outer}s must exceed its ${category_max[$key]}s individual timeout plus 10s grace"
    done
    printf 'INNERMOST-TIMEOUT coverage: Cargo nextest=300/600 s (+30 s grace); E2E categories=%s all nested inside DAG nodes\n' \
        "${#category_max[@]}"
}

# Emit WHY a prepare/compile step failed. Guaranteed to write at least one line.
#
# The reason is normally the child's own stderr, which is the right answer when
# the child wrote one. But a guest whose prepare is a bare
# `command -v foo >/dev/null` under `set -e` exits nonzero having printed
# nothing, so the caller emitted `prepare failed for <guest>` with no cause at
# all. That failure is unattributable from the log, and downstream the validate
# ledger cannot classify it either — it is the second half of the hermit #1711
# triage defect, where one node failed two guests and only one was explicable.
#
# So when stderr is empty, synthesize a reason from what is nonetheless known:
# the exit status (with the two dispositions GNU timeout reserves named
# explicitly), and the tail of stdout if the child wrote there instead. A
# synthesized reason is always marked as such so it is never mistaken for the
# child's own words.
function emit_failure_reason {
    local status=$1
    local stdout_file=$2
    local stderr_file=$3
    if [[ -s $stderr_file ]]; then
        cat "$stderr_file" >&2
        return 0
    fi
    local disposition="exit status $status"
    case $status in
        # `timeout` reports 124 when the deadline fired, and 137 when the
        # follow-up KILL was needed. Both are wall-clock faults, not the child
        # rejecting its input, and naming them prevents a timeout being read as
        # a missing dependency.
        124) disposition="TIMEOUT (exit 124: deadline reached)" ;;
        137) disposition="TIMEOUT-KILLED (exit 137: SIGKILL after --kill-after)" ;;
        127) disposition="exit 127: command not found" ;;
    esac
    if [[ -s $stdout_file ]]; then
        printf 'no stderr from the step; %s; last stdout line: %s\n' \
            "$disposition" "$(tail -n 1 "$stdout_file")" >&2
    else
        printf 'no output on stdout or stderr; %s\n' "$disposition" >&2
    fi
}

# Hermit validates the program before creating the guest.  Those refusals are
# harness no-results: no guest observation exists to count as a chaos failure.
# Carry that distinction alongside the process status instead of asking each
# mode to infer execution from a nonzero exit code.
# A REFUSAL IS PROVED BY THE ABSENCE OF THE GUEST, NOT BY A STRING THE GUEST CAN WRITE.
#
# Matching the refusal text anywhere in combined stderr let the GUEST forge the
# classification: a program that really executes, prints `Error: Program application
# failure` and exits 1 was recorded as LAUNCH_REFUSED -- a real failure downgraded to a
# no-result, which is the dangerous direction to be wrong in.
#
# Hermit emits its refusal BEFORE creating the guest, so a genuine refusal has three
# properties the forgery cannot have all of: the message is the FIRST line of stderr
# (nothing ran to print ahead of it), and the guest produced NO stdout at all. Requiring
# the conjunction keeps every real refusal while rejecting mid-stream guest output.
function classify_attempt_execution {
    local mode=$1
    local status=$2
    local stderr_file=$3
    local stdout_file=${4:-/dev/null}

    if [[ $mode != naked && $status != 0 ]] &&
        [[ ! -s $stdout_file ]] &&
        head -n 1 "$stderr_file" |
            grep -Eq '^Error: (Program |Could not resolve program )'; then
        echo LAUNCH_REFUSED
    else
        echo ATTEMPT_RESULT
    fi
}

function launch_refusal_reason {
    local stderr_file=$1
    local first_line
    first_line=$(head -n 1 "$stderr_file")
    first_line=${first_line#Error: }
    printf 'guest launch refused before execution: %s' "$first_line"
}

function audit_guest_launch_classification_contract {
    local fixture_dir refusal_stderr guest_failure_stderr
    fixture_dir=$(mktemp -d "${TMPDIR:-/tmp}/hermit-harness-launch-classification.XXXXXX")
    refusal_stderr="$fixture_dir/refusal.stderr"
    guest_failure_stderr="$fixture_dir/guest-failure.stderr"
    printf '%s\n' \
        'Error: Program /tmp/guest is under host /tmp, but Hermit replaces guest /tmp with an isolated directory.' \
        >"$refusal_stderr"
    printf '%s\n' 'guest reported an application failure' >"$guest_failure_stderr"

    [[ $(classify_attempt_execution chaos 1 "$refusal_stderr") == LAUNCH_REFUSED ]] || {
        rm -rf "$fixture_dir"
        die "a Hermit program launch refusal must be a typed no-result"
    }
    [[ $(classify_attempt_execution chaos 1 "$guest_failure_stderr") == ATTEMPT_RESULT ]] || {
        rm -rf "$fixture_dir"
        die "a genuine nonzero guest result must remain countable"
    }
    [[ $(classify_attempt_execution naked 1 "$refusal_stderr") == ATTEMPT_RESULT ]] || {
        rm -rf "$fixture_dir"
        die "native guest stderr must not be mistaken for a Hermit launch refusal"
    }

    # THE FORGERY LEG. Everything above plants text only Hermit would write; none of it
    # can fail if the classifier simply trusts any matching line. So plant the refusal
    # wording as output of a guest that DID run -- it wrote stdout, and its stderr line
    # is not the first. A real failure must stay countable; downgrading it to a
    # no-result silently deletes a failing cell from the scorecard.
    local forged_stderr forged_stdout ran_first_line_stderr
    forged_stderr="$fixture_dir/forged.stderr"
    forged_stdout="$fixture_dir/forged.stdout"
    ran_first_line_stderr="$fixture_dir/forged-first-line.stderr"
    printf '%s\n' 'starting work' 'Error: Program application failure' >"$forged_stderr"
    printf '%s\n' 'guest produced real output' >"$forged_stdout"
    printf '%s\n' 'Error: Program application failure' >"$ran_first_line_stderr"

    [[ $(classify_attempt_execution chaos 1 "$forged_stderr" /dev/null) == ATTEMPT_RESULT ]] || {
        rm -rf "$fixture_dir"
        die "a guest failure whose stderr merely CONTAINS the refusal wording must stay countable"
    }
    [[ $(classify_attempt_execution chaos 1 "$ran_first_line_stderr" "$forged_stdout") == ATTEMPT_RESULT ]] || {
        rm -rf "$fixture_dir"
        die "a guest that produced stdout provably executed; it cannot be a pre-launch refusal"
    }
    # ...and the positive must still fire, so the tightening did not just disable the check.
    [[ $(classify_attempt_execution chaos 1 "$refusal_stderr" /dev/null) == LAUNCH_REFUSED ]] || {
        rm -rf "$fixture_dir"
        die "a genuine refusal (first-line message, no guest stdout) must still be a no-result"
    }
    rm -rf "$fixture_dir"
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
    # Each failure path below captures the child's exit status rather than
    # discarding it via `if ! …`, so `emit_failure_reason` can name the
    # disposition when the child produced no stderr of its own. A "failed" line
    # with no following cause must not be possible on any of the three paths.
    local status
    if [[ $kind == c ]]; then
        mapfile -t compile_args < <(jq -r '.compile_args[]' <<<"$metadata")
        status=0
        run_capture "$stdout_file" "$stderr_file" "$timeout_seconds" \
            cc -std=c11 -O2 -g -Wall -Wextra -Werror "${compile_args[@]}" \
            "$test" -o "$cell_dir/fixtures/program" || status=$?
        if ((status != 0)); then
            echo "C program compilation failed for ${test#"$ROOT_DIR/"}" >&2
            emit_failure_reason "$status" "$stdout_file" "$stderr_file"
            return "$status"
        fi
        return 0
    fi
    if [[ $kind == rust ]]; then
        mapfile -t compile_args < <(jq -r '.compile_args[]' <<<"$metadata")
        status=0
        run_capture "$stdout_file" "$stderr_file" "$timeout_seconds" \
            rustc -O "${compile_args[@]}" "$test" -o "$cell_dir/fixtures/program" || status=$?
        if ((status != 0)); then
            echo "Rust program compilation failed for ${test#"$ROOT_DIR/"}" >&2
            emit_failure_reason "$status" "$stdout_file" "$stderr_file"
            return "$status"
        fi
        return 0
    fi
    [[ $kind != direct && $kind != direct-argv ]] || return 0
    mapfile -t prepare_args < <(jq -r '.prepare_args[]' <<<"$metadata")
    status=0
    run_capture "$stdout_file" "$stderr_file" "$timeout_seconds" \
        "${env_args[@]}" "$test" "${prepare_args[@]}" || status=$?
    if ((status != 0)); then
        echo "prepare failed for ${test#"$TEST_ROOT/"}" >&2
        emit_failure_reason "$status" "$stdout_file" "$stderr_file"
        return "$status"
    fi
}

# Does this test's verify mode opt into the L2 parity assertion?
function verify_asserts_bitwise_parity {
    local metadata=$1
    jq -r '.modes.verify.assert.bitwise_parity // false' <<<"$metadata"
}

# Where a verify attempt writes its machine-readable verdict.
function verify_verdict_path {
    local cell_dir=$1 attempt=$2
    printf '%s/verify-%s.json\n' "$cell_dir" "$attempt"
}

# Fail closed on anything short of a full parity verdict: a missing, unparsable,
# or non-parity verdict is NOT a pass. Echoes a reason on failure.
function assert_bitwise_parity_verdict {
    local verdict=$1
    if [[ ! -f $verdict ]]; then
        printf 'verify wrote no parity verdict to %s\n' "${verdict##*/}"
        return 1
    fi
    local summary
    if ! summary=$(jq -er '
        "verified=\(.verified) bitwise_parity=\(.bitwise_parity) "
        + "verdict=\(.verdict) strictness=\(.comparison.strictness // "none") "
        + "compared=\(.comparison.compare_logs // false) "
        + "messages=\(.compared_log_messages.left // 0)/\(.compared_log_messages.right // 0)"
    ' "$verdict" 2>/dev/null); then
        printf 'verify parity verdict %s is not readable JSON\n' "${verdict##*/}"
        return 1
    fi
    if [[ $(jq -r '(.verified == true) and (.bitwise_parity == true)' "$verdict") != true ]]; then
        printf 'verify did not reach L2 parity: %s\n' "$summary"
        return 1
    fi
    printf 'L2 parity: %s\n' "$summary"
    return 0
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
            # Plain `--strict --verify` runs the LOSSY `Stripped` comparator,
            # which normalizes numbers/addresses/paths away wholesale and so
            # "cannot establish L2" (AGENTS.md). A cell that opts into
            # `assert = { bitwise_parity = true }` is upgraded to the canonical
            # parity comparator and made to emit a machine-readable verdict, which
            # run_cell then checks -- otherwise the cell would be justified by a
            # parity measurement it never actually performs.
            local verify_strict_flags=()
            if [[ $(verify_asserts_bitwise_parity "$metadata") == true ]]; then
                verify_strict_flags=(--verify-strict --verify-json
                    "$(verify_verdict_path "$cell_dir" "$attempt")")
            fi
            command=("$HERMIT_BIN" --log=info run --backend "$backend" --strict --verify
                "${verify_strict_flags[@]}" "${profile[@]}" -- "${guest_command[@]}")
            ;;
        replay)
            command=("$HERMIT_BIN" --log=info --backend "$backend" record start --strict --verify
                --data-dir "$cell_dir/recording" --record-timeout "$timeout_seconds" -- "${guest_command[@]}")
            ;;
        chaos)
            # Chaos seeds are witnesses for a guest schedule, so the guest's initial
            # stack must not inherit run-specific host variables such as RESULT_ROOT.
            # Keep the witness independent of the harness run id and checkout path.
            command=("$HERMIT_BIN" --log=off run --base-env=minimal --backend "$backend" --strict --chaos
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

    local hash attempt_execution
    hash=$(observation_hash "$metadata" "$status" "$stdout_file" "$stderr_file" "$cell_dir/tmp")
    attempt_execution=$(classify_attempt_execution "$mode" "$status" "$stderr_file" "$stdout_file")
    printf '%s\t%s\t%s\t%s\t%s\n' \
        "$status" "$hash" "$stdout_file" "$stderr_file" "$attempt_execution"
}

function append_result {
    local test_id=$1 category=$2 lane=$3 mode=$4 backend=$5 outcome=$6 duration_ms=$7 reason=$8
    local path_evidence=$9
    local error_kind=${10}
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
            # The receipt must record the flags that actually ran. Reporting a
            # bare `--strict --verify` for a cell that ran the parity comparator
            # (or vice versa) is how an L1 cell gets mistaken for an L2 one.
            effective_args=$(jq -cn --arg backend "$backend" \
                --argjson strict "$(verify_asserts_bitwise_parity "${METADATA_BY_ID[$test_id]}")" \
                '["--log=info","run",("--backend=" + $backend),"--strict","--verify"]
                 + (if $strict then ["--verify-strict","--verify-json"] else [] end)')
            log_level=info
            ;;
        replay)
            effective_args=$(jq -cn --arg backend "$backend" \
                '["--log=info",("--backend=" + $backend),"record","start","--strict","--verify"]')
            log_level=info
            ;;
        chaos)
            effective_args=$(jq -cn --arg backend "$backend" \
                '["--log=off","run","--base-env=minimal",("--backend=" + $backend),"--strict","--chaos","--sched-heuristic=random"]')
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
        --arg error_kind "$error_kind" \
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
          outcome:$outcome,
          error_kind:(if $error_kind == "" then null else $error_kind end),
          duration_ms:$duration_ms,
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

    local outcome=PASS reason='' error_kind='' path_evidence=null launch_refusal_stderr=''
    local timeout_reason='' prepare_status=0
    prepare_test "$test" "$cell_dir" "$timeout_seconds" || prepare_status=$?
    if ((prepare_status != 0)); then
        outcome=ERROR
        if reason=$(individual_test_timeout_reason \
            "$id" prepare "$backend" 1 "$timeout_seconds" "$prepare_status"); then
            :
        else
            reason="fixture preparation failed"
        fi
    elif [[ $mode == naked ]]; then
        local runs min_distinct attempt row status hash _stdout_file _stderr_file attempt_execution
        local failed_runs=0
        local -a hashes=()
        runs=$(jq -r '.modes.naked.runs // 3' <<<"$metadata")
        min_distinct=$(jq -r '.modes.naked.assert.min_distinct // 2' <<<"$metadata")
        for ((attempt = 1; attempt <= runs; attempt++)); do
            row=$(execute_attempt "$test" "$metadata" "$mode" "" "$cell_dir" "$attempt")
            IFS=$'\t' read -r status hash _stdout_file _stderr_file attempt_execution <<<"$row"
            hashes+=("$hash")
            if timeout_reason=$(individual_test_timeout_reason \
                "$id" "$mode" native "$attempt" "$timeout_seconds" "$status"); then
                break
            fi
            timeout_reason=''
            [[ $status == 0 ]] || ((failed_runs += 1))
        done
        local distinct
        distinct=$(printf '%s\n' "${hashes[@]}" | LC_ALL=C sort -u | wc -l)
        if [[ -n $timeout_reason ]]; then
            outcome=FAIL
            reason=$timeout_reason
        elif ((failed_runs > 0)); then
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
        local stdout1 stderr1 execution1 stdout2 stderr2 execution2
        local passes=0 failures=0 repeat_mismatches=0
        local -a hashes=()
        min_distinct=$(jq -r '.modes.chaos.assert.min_distinct // 2' <<<"$metadata")
        min_passes=$(jq -r '.modes.chaos.assert.min_passes // 0' <<<"$metadata")
        min_failures=$(jq -r '.modes.chaos.assert.min_failures // 0' <<<"$metadata")
        while IFS= read -r seed; do
            row1=$(execute_attempt "$test" "$metadata" "$mode" "$backend" "$cell_dir" "seed-$seed-a" "$seed")
            IFS=$'\t' read -r status1 hash1 stdout1 stderr1 execution1 <<<"$row1"
            if [[ $execution1 == LAUNCH_REFUSED ]]; then
                launch_refusal_stderr=$stderr1
                break
            fi
            if timeout_reason=$(individual_test_timeout_reason \
                "$id" "$mode" "$backend" "seed-$seed-a" "$timeout_seconds" "$status1"); then
                break
            fi
            timeout_reason=''
            row2=$(execute_attempt "$test" "$metadata" "$mode" "$backend" "$cell_dir" "seed-$seed-b" "$seed")
            IFS=$'\t' read -r status2 hash2 stdout2 stderr2 execution2 <<<"$row2"
            if [[ $execution2 == LAUNCH_REFUSED ]]; then
                launch_refusal_stderr=$stderr2
                break
            fi
            if timeout_reason=$(individual_test_timeout_reason \
                "$id" "$mode" "$backend" "seed-$seed-b" "$timeout_seconds" "$status2"); then
                break
            fi
            timeout_reason=''
            hashes+=("$hash1")
            if [[ $status1 == 0 ]]; then
                ((passes += 1))
            else
                ((failures += 1))
            fi
            [[ $status1 == "$status2" && $hash1 == "$hash2" ]] || ((repeat_mismatches += 1))
        done < <(jq -r '.modes.chaos.seeds[]' <<<"$metadata")
        if [[ -n $launch_refusal_stderr ]]; then
            outcome=ERROR
            error_kind=guest-launch-refused
            reason=$(launch_refusal_reason "$launch_refusal_stderr")
        elif [[ -n $timeout_reason ]]; then
            outcome=FAIL
            reason=$timeout_reason
        else
            local distinct
            distinct=$(printf '%s\n' "${hashes[@]}" | LC_ALL=C sort -u | wc -l)
            if ((repeat_mismatches > 0 || distinct < min_distinct || passes < min_passes || failures < min_failures)); then
                outcome=FAIL
                reason="chaos distinct=$distinct passes=$passes failures=$failures repeat_mismatches=$repeat_mismatches"
            else
                reason="chaos distinct=$distinct passes=$passes failures=$failures; every seed reproduced"
            fi
        fi
    elif [[ $mode == custom ]]; then
        local runs repeat_identical attempt row status hash stdout_file stderr_file attempt_execution
        local failed_runs=0
        local -a hashes=()
        runs=$(jq -r '.modes.custom.assert.runs // 1' <<<"$metadata")
        repeat_identical=$(jq -r '.modes.custom.assert.repeat_identical // false' <<<"$metadata")
        for ((attempt = 1; attempt <= runs; attempt++)); do
            row=$(execute_attempt "$test" "$metadata" "$mode" "$backend" "$cell_dir" "$attempt")
            IFS=$'\t' read -r status hash stdout_file stderr_file attempt_execution <<<"$row"
            if [[ $attempt_execution == LAUNCH_REFUSED ]]; then
                launch_refusal_stderr=$stderr_file
                break
            fi
            if timeout_reason=$(individual_test_timeout_reason \
                "$id" "$mode" "$backend" "$attempt" "$timeout_seconds" "$status"); then
                break
            fi
            timeout_reason=''
            hashes+=("$hash")
            [[ $status == 0 ]] || ((failed_runs += 1))
        done
        if [[ -n $launch_refusal_stderr ]]; then
            outcome=ERROR
            error_kind=guest-launch-refused
            reason=$(launch_refusal_reason "$launch_refusal_stderr")
        elif [[ -n $timeout_reason ]]; then
            outcome=FAIL
            reason=$timeout_reason
        else
            local distinct
            distinct=$(printf '%s\n' "${hashes[@]}" | LC_ALL=C sort -u | wc -l)
            if ((failed_runs > 0)) || [[ $repeat_identical == true && $distinct != 1 ]]; then
                outcome=FAIL
                reason="custom runs=$runs failed_runs=$failed_runs distinct=$distinct"
            else
                reason="custom output identical across $runs runs"
            fi
        fi
    else
        local row status _hash stdout_file stderr_file attempt_execution
        row=$(execute_attempt "$test" "$metadata" "$mode" "$backend" "$cell_dir" 1)
        IFS=$'\t' read -r status _hash stdout_file stderr_file attempt_execution <<<"$row"
        if [[ $attempt_execution == LAUNCH_REFUSED ]]; then
            outcome=ERROR
            error_kind=guest-launch-refused
            reason=$(launch_refusal_reason "$stderr_file")
        elif reason=$(individual_test_timeout_reason \
            "$id" "$mode" "$backend" 1 "$timeout_seconds" "$status"); then
            outcome=FAIL
        elif [[ $status != 0 ]]; then
            outcome=FAIL
            reason="$mode exited with status $status"
        elif [[ $mode == verify && $(verify_asserts_bitwise_parity "$metadata") == true ]]; then
            # A zero exit only means the comparator this run used was satisfied.
            # For an L2 cell, read the verdict the run itself published and
            # require full parity; anything else is a FAIL, not a PASS.
            local parity_reason
            if parity_reason=$(assert_bitwise_parity_verdict \
                "$(verify_verdict_path "$cell_dir" 1)"); then
                reason=$parity_reason
            else
                outcome=FAIL
                reason=$parity_reason
            fi
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
    append_result "$id" "$category" "$lane" "$mode" "$backend" "$outcome" "$duration_ms" "$reason" "$path_evidence" "$error_kind"
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
             elif .outcome == "ERROR" then
               ("<error" + (if .error_kind then (" type=\"" + (.error_kind|esc) + "\"") else "" end)
                + ">" + ((.reason // "error")|esc) + "</error>")
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
        audit_innermost_e2e_timeout
        audit_innermost_timeout_coverage
        audit_immutable_hermit_binary
        audit_test_binary_registration
        audit_guest_launch_classification_contract
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
        [[ $MODE_FILTER == naked ]] || require_executable_hermit "$HERMIT_BIN"
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
    audit-test-binary-registration)
        audit_test_binary_registration
        ;;
    audit-ci)
        audit_ci_correspondence
        ;;
    *)
        usage
        die "unknown command: $subcommand"
        ;;
esac
