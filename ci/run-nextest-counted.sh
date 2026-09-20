#!/usr/bin/env bash

set -uo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
readonly SCRIPT_DIR
readonly RESULT_WRITER="$SCRIPT_DIR/nextest-test-results.rs"
readonly TIMEOUT_CONFIG_WRITER="$SCRIPT_DIR/nextest-timeout-config.rs"
readonly PREBUILT_RUST_SCRIPT="$SCRIPT_DIR/rust-script-bin/rust-script"

nextest_config=
cpu_measurement_dir=

# Do not rely on a nested `/usr/bin/env rust-script` shebang resolving the
# repository wrapper after validation has entered its hosted namespace.  The
# top-level driver was already resolved before that boundary, but these helper
# invocations happen inside it; run 35335623037 resolved the installed compiler
# instead, tried to rebuild nextest-binaries.rs without network access, and all
# prepared Nextest shards refused before executing a test.  Official consumers
# name the prepared runner directly.  Standalone use retains the normal shebang
# path and may compile as before.
function run_rust_script {
    local source=$1
    shift
    if [[ ${HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED:-0} == 1 ]]; then
        "$PREBUILT_RUST_SCRIPT" --force "$source" "$@"
    else
        "$source" "$@"
    fi
}

function cleanup_nextest_config {
    if [[ -n $nextest_config ]]; then
        rm -f -- "$nextest_config"
        nextest_config=
    fi
}

function cleanup_cpu_measurement_dir {
    if [[ -n $cpu_measurement_dir ]]; then
        rm -rf -- "$cpu_measurement_dir"
        cpu_measurement_dir=
    fi
}

# Only standalone use can build. Official consumers require the recorded
# normal binary, independently of the Nextest test-harness binary of this name.
function build_cpu_wrapper {
    if [[ ${HERMIT_PREPARED_NEXTEST_REQUIRED:-0} == 1 ]]; then
        run_rust_script "$SCRIPT_DIR/nextest-binaries.rs" cpu-wrapper
    else
        run_rust_script "$SCRIPT_DIR/nextest-binaries.rs" build-cpu-wrapper
    fi
}

function invoke_nextest {
    local operation=$1
    shift
    if [[ ${HERMIT_PREPARED_NEXTEST_REQUIRED:-0} == 1 ]]; then
        run_rust_script "$SCRIPT_DIR/nextest-binaries.rs" "$operation" --config-file "$nextest_config" "$@"
    else
        cargo nextest --config-file "$nextest_config" "$operation" "$@"
    fi
}

function configured_cpu_report_path {
    local count_path=${DAGRUN_TEST_COUNTS_PATH:--}
    if [[ ${HERMIT_NEXTEST_CPU_REPORT_PATH+x} ]]; then
        if [[ -z $HERMIT_NEXTEST_CPU_REPORT_PATH ]]; then
            printf 'run-nextest-counted: HERMIT_NEXTEST_CPU_REPORT_PATH must not be empty\n' >&2
            return 2
        fi
        printf '%s\n' "$HERMIT_NEXTEST_CPU_REPORT_PATH"
    elif [[ $count_path != - && -n $count_path ]]; then
        printf '%s.cpu.json\n' "$count_path"
    else
        printf '%s\n' -
    fi
}

function configured_wall_timeout_multiplier {
    printf '%s\n' "${HERMIT_TEST_WALL_TIMEOUT_MULTIPLIER-1}"
}

function nextest_inventory_args {
    local -n output=$1
    shift
    output=()
    while (($#)); do
        case $1 in
            --)
                output+=("$@")
                return 0
                ;;
            -j|--jobs|--test-threads|--retries|--max-fail|--no-tests|\
                --failure-output|--success-output|--status-level|\
                --final-status-level|--message-format|--message-format-version)
                if (($# < 2)); then
                    printf 'run-nextest-counted: %s requires a value\n' "$1" >&2
                    return 2
                fi
                shift 2
                ;;
            -j?*|--jobs=*|--test-threads=*|--retries=*|--max-fail=*|--no-tests=*|\
                --failure-output=*|--success-output=*|--status-level=*|\
                --final-status-level=*|--message-format=*|--message-format-version=*)
                shift
                ;;
            --no-run|--fail-fast|--ff|--no-fail-fast|--nff|--no-capture|\
                --hide-progress-bar|--no-output-indent|--no-input-handler)
                shift
                ;;
            *)
                output+=("$1")
                shift
                ;;
        esac
    done
}

function emit_libtest_count {
    local events=$1 status=${2:-0} path=${DAGRUN_TEST_COUNTS_PATH:--}
    local attempts=${3-} binary_map=${4-} cpu_report=${5-}
    local writer_status=0
    local -a cpu_args=()
    if [[ ! -s $events ]]; then
        printf 'run-nextest-counted: typed nextest event stream is empty\n' >&2
        return 2
    fi
    if [[ -n $attempts || -n $binary_map || -n $cpu_report ]]; then
        if [[ -z $attempts || -z $binary_map || -z $cpu_report ]]; then
            printf 'run-nextest-counted: CPU attempt directory, binary map, and report path must be supplied together\n' >&2
            return 2
        fi
        cpu_args=("$attempts" "$binary_map" "$cpu_report")
    fi
    run_rust_script "$RESULT_WRITER" "$events" "$status" "$path" "${cpu_args[@]}" || writer_status=$?
    if ((writer_status != 0)); then
        printf 'run-nextest-counted: cannot derive typed test results from %s\n' "$events" >&2
        return "$writer_status"
    fi
}

function run_nextest {
    local events_log=$1 wall_multiplier=$2 cpu_wrapper=$3 attempts=$4 binary_map=$5
    local inventory=$6 cpu_report=$7
    local status count_status=0
    local -a inventory_arguments=()
    shift 7

    if [[ $cpu_report != - && $cpu_report == "${DAGRUN_TEST_COUNTS_PATH:--}" ]]; then
        printf 'run-nextest-counted: CPU report path must differ from DAGRUN_TEST_COUNTS_PATH\n' >&2
        return 2
    fi

    cleanup_nextest_config
    nextest_config=$(mktemp "${TMPDIR:-/tmp}/hermit-nextest-config.XXXXXX.toml") || return $?
    if ! HERMIT_NEXTEST_CPU_WRAPPER_BIN="$cpu_wrapper" run_rust_script "$TIMEOUT_CONFIG_WRITER" \
        "$SCRIPT_DIR/../.config/nextest.toml" "$wall_multiplier" "$nextest_config"; then
        cleanup_nextest_config
        return 2
    fi

    nextest_inventory_args inventory_arguments "$@" || return $?
    if ! HERMIT_NEXTEST_CPU_WRAPPER_BIN="$cpu_wrapper" invoke_nextest list --color never \
        --message-format json "${inventory_arguments[@]}" >"$inventory"; then
        printf 'run-nextest-counted: cannot obtain typed nextest binary inventory\n' >&2
        cleanup_nextest_config
        return 2
    fi
    if ! run_rust_script "$RESULT_WRITER" --write-binary-map "$inventory" "$binary_map"; then
        printf 'run-nextest-counted: cannot validate typed nextest binary inventory\n' >&2
        cleanup_nextest_config
        return 2
    fi

    set +e
    # nextest's stderr remains presentation. The versioned stdout event stream
    # is the only input to the aggregate count and per-test result adapter.
    HERMIT_NEXTEST_CPU_BINARY_MAP="$binary_map" \
        HERMIT_NEXTEST_CPU_RECORD_DIR="$attempts" \
        HERMIT_NEXTEST_CPU_WRAPPER_BIN="$cpu_wrapper" \
        NEXTEST_EXPERIMENTAL_LIBTEST_JSON=1 invoke_nextest run \
        --color never --message-format libtest-json-plus --message-format-version 0.1 \
        "$@" >"$events_log"
    status=$?
    set -e
    cleanup_nextest_config

    emit_libtest_count "$events_log" "$status" "$attempts" "$binary_map" \
        "$cpu_report" || count_status=$?
    # Publication cannot erase a failure that the actual test runner observed.
    if ((status != 0)); then
        return "$status"
    fi
    return "$count_status"
}

function self_test {
    local scratch got expected status=0 wall_multiplier cpu_wrapper fixture_binary attempts
    local binary_map inventory launch_failure=0 ordinary_attempts
    scratch=$(mktemp -d)
    trap 'rm -rf "$scratch"' RETURN

    cpu_wrapper=$(build_cpu_wrapper) || return $?
    "$cpu_wrapper" --self-test || return $?

    printf '%s\n' \
        '{"type":"suite","event":"started","test_count":2,"nextest":{"crate":"suite","test_binary":"suite","kind":"lib"}}' \
        '{"type":"test","event":"started","name":"suite::suite$passes"}' \
        '{"type":"test","event":"ok","name":"suite::suite$passes","exec_time":0.1}' \
        '{"type":"test","event":"started","name":"suite::suite$recovers"}' \
        '{"type":"test","event":"ok","name":"suite::suite$recovers#2","exec_time":0.2}' \
        '{"type":"suite","event":"ok","passed":2,"failed":0,"ignored":0,"measured":0,"filtered_out":7,"exec_time":0.3,"nextest":{"crate":"suite","test_binary":"suite","kind":"lib"}}' \
        >"$scratch/events.jsonl"
    got=$(DAGRUN_TEST_COUNTS_PATH="$scratch/counts.json" \
        emit_libtest_count "$scratch/events.jsonl" 0)
    expected=$'running 2 tests\ntest result: ok. 2 passed; 0 failed; 0 ignored; 7 filtered out'
    [[ $got == "$expected" ]] || return 1
    python3 - "$scratch/counts.json" <<'PYEOF' || return 1
import json
import sys

with open(sys.argv[1], encoding="utf-8") as source:
    report = json.load(source)
    assert report == {
    "schema": 2,
    "executed_tests": 2,
    "filtered_tests": 7,
    "results": [
        {"id": "suite$passes", "result": "pass", "attempts": 1},
        {"id": "suite$recovers", "result": "pass", "attempts": 2},
    ],
    }
PYEOF

    sed 's/"filtered_out":7/"filtered_out":11/' \
        "$scratch/events.jsonl" >"$scratch/mutated-events.jsonl"
    got=$(DAGRUN_TEST_COUNTS_PATH="$scratch/mutated-counts.json" \
        emit_libtest_count "$scratch/mutated-events.jsonl" 0)
    expected=$'running 2 tests\ntest result: ok. 2 passed; 0 failed; 0 ignored; 11 filtered out'
    [[ $got == "$expected" ]] || return 1
    python3 - "$scratch/mutated-counts.json" <<'PYEOF' || return 1
import json
import sys

with open(sys.argv[1], encoding="utf-8") as source:
    report = json.load(source)
assert report["executed_tests"] == 2
assert report["filtered_tests"] == 11
PYEOF

    got=$(DAGRUN_TEST_COUNTS_PATH="$scratch/expected-counts.json" \
        NEXTEST_EXPECTED_EXECUTED=2 emit_libtest_count "$scratch/events.jsonl" 0)
    expected=$'running 2 tests\ntest result: ok. 2 passed; 0 failed; 0 ignored; 7 filtered out'
    [[ $got == "$expected" ]] || return 1
    cmp -s "$scratch/expected-counts.json" "$scratch/counts.json" || return 1

    status=0
    DAGRUN_TEST_COUNTS_PATH="$scratch/wrong-counts.json" \
        NEXTEST_EXPECTED_EXECUTED=3 emit_libtest_count "$scratch/events.jsonl" 0 \
        >"$scratch/wrong-count.stdout" 2>"$scratch/wrong-count.stderr" || status=$?
    [[ $status == 2 ]] || return 1
    grep -q 'expected 3 tests to execute, saw 2' "$scratch/wrong-count.stderr" || return 1
    [[ ! -e $scratch/wrong-counts.json ]] || return 1

    status=0
    NEXTEST_EXPECTED_EXECUTED=unknown emit_libtest_count "$scratch/events.jsonl" 0 \
        >/dev/null 2>"$scratch/invalid-count.stderr" || status=$?
    [[ $status == 2 ]] || return 1
    grep -q 'NEXTEST_EXPECTED_EXECUTED' "$scratch/invalid-count.stderr" || return 1

    [[ $(configured_wall_timeout_multiplier) == 1 ]] || return 1
    wall_multiplier=$(HERMIT_TEST_WALL_TIMEOUT_MULTIPLIER=1.5 \
        configured_wall_timeout_multiplier)
    [[ $wall_multiplier == 1.5 ]] || return 1

    printf '%s\n' \
        '{"type":"suite","event":"started","test_count":1,"nextest":{"crate":"suite","test_binary":"suite","kind":"lib"}}' \
        '{"type":"test","event":"started","name":"suite::suite$fails"}' \
        '{"type":"test","event":"failed","name":"suite::suite$fails","exec_time":0.2}' \
        '{"type":"suite","event":"failed","passed":0,"failed":1,"ignored":0,"measured":0,"filtered_out":3,"exec_time":0.2,"nextest":{"crate":"suite","test_binary":"suite","kind":"lib"}}' \
        >"$scratch/failed-events.jsonl"
    DAGRUN_TEST_COUNTS_PATH="$scratch/failed-counts.json" \
        emit_libtest_count "$scratch/failed-events.jsonl" 100 >/dev/null || return 1
    python3 - "$scratch/failed-counts.json" <<'PYEOF' || return 1
import json
import sys

with open(sys.argv[1], encoding="utf-8") as source:
    report = json.load(source)
assert report["results"] == [
    {"id": "suite$fails", "result": "fail", "attempts": 1},
]
PYEOF

    # Exercise the generated config, the real per-execution wrapper, and the
    # typed reconciliation path. A valid failed run must still return nextest's
    # original nonzero status.
    fixture_binary="$scratch/suite-0123456789abcdef"
    ln -s -- "$cpu_wrapper" "$fixture_binary"
    attempts="$scratch/wrapper-attempts"
    mkdir "$attempts"
    function cargo {
        [[ $1 == nextest && $2 == --config-file && -f $3 ]] || return 99
        cp -- "$3" "$scratch/scaled-nextest.toml"
        local operation=$4
        shift 4
        if [[ $operation == list ]]; then
            printf '%s\n' "$@" >"$scratch/list-arguments"
            jq -nc --arg executable "$fixture_binary" '{
                "rust-suites": {
                    "suite": {
                        "package-name": "suite",
                        "binary-id": "suite",
                        "binary-name": "suite",
                        "kind": "lib",
                        "binary-path": $executable
                    }
                }
            }'
            return
        fi
        [[ $operation == run ]] || return 99
        if ((launch_failure)); then
            printf '%s\n' \
                '{"type":"suite","event":"started","test_count":0,"nextest":{"crate":"suite","test_binary":"suite","kind":"lib"}}' \
                '{"type":"suite","event":"failed","passed":0,"failed":0,"ignored":0,"measured":0,"filtered_out":0,"exec_time":0.0,"nextest":{"crate":"suite","test_binary":"suite","kind":"lib"}}'
            printf 'Summary [   0.010s] 12 tests run: 0 passed, 12 exec failed, 0 skipped\n' >&2
            return 100
        fi
        local wrapper_status=0
        HERMIT_NEXTEST_CPU_CONTROL=1 CARGO_PKG_NAME=suite \
            NEXTEST_RUN_ID=self-test-nextest __NEXTEST_ATTEMPT=1 \
            HERMIT_NEXTEST_CPU_CONTROL_CWD="$PWD" \
            HERMIT_NEXTEST_CPU_CONTROL_SENTINEL=preserved \
            setsid "$cpu_wrapper" "$fixture_binary" --exact failure --nocapture \
            >/dev/null 2>/dev/null || wrapper_status=$?
        [[ $wrapper_status == 23 ]] || return 99
        printf '%s\n' \
            '{"type":"suite","event":"started","test_count":1,"nextest":{"crate":"suite","test_binary":"suite","kind":"lib"}}' \
            '{"type":"test","event":"failed","name":"suite::suite$failure","exec_time":0.01}' \
            '{"type":"suite","event":"failed","passed":0,"failed":1,"ignored":0,"measured":0,"filtered_out":0,"exec_time":0.01,"nextest":{"crate":"suite","test_binary":"suite","kind":"lib"}}'
        printf 'Summary [   0.010s] 1 test run: 0 passed, 1 failed, 0 skipped\n' >&2
        return 100
    }
    set +e
    inventory="$scratch/wrapper-inventory.json"
    binary_map="$scratch/wrapper-binary-map.json"
    got=$(TMPDIR="$scratch" DAGRUN_TEST_COUNTS_PATH="$scratch/wrapper-counts.json" \
        run_nextest "$scratch/wrapper-events" "$wall_multiplier" "$cpu_wrapper" \
        "$attempts" "$binary_map" "$inventory" "$scratch/wrapper-cpu.json" \
        --profile ci -p suite -j 1 --retries 2 -- --skip skipme)
    status=$?
    set -e
    [[ $status == 100 ]] || return 1
    [[ $got == *$'running 1 tests\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 filtered out' ]] || return 1
    [[ $got != *'test result: ok.'* ]] || return 1
    printf '%s\n' --color never --message-format json --profile ci -p suite \
        -- --skip skipme >"$scratch/expected-list-arguments"
    cmp -s "$scratch/expected-list-arguments" "$scratch/list-arguments" || return 1
    [[ $(<"$scratch/wrapper-counts.json") == \
        '{"executed_tests":1,"filtered_tests":0,"results":[{"attempts":1,"id":"suite$failure","result":"fail"}],"schema":2}' ]] || return 1
    jq -e '
        .schema == 3 and .run_id == "self-test-nextest" and
        (.attempts | length) == 1 and
        .attempts[0].identity == {package:"suite", binary:"suite", test:"failure", attempt:1} and
        .attempts[0].completion == {kind:"exit", code:23} and
        .attempts[0].cpu_source == "wait4-subtree"
    ' "$scratch/wrapper-cpu.json" >/dev/null || return 1
    grep -q 'period = "86s"' "$scratch/scaled-nextest.toml" || return 1
    [[ $(grep -c '^run-wrapper = "hermit-per-test-cpu"$' \
        "$scratch/scaled-nextest.toml") == 1 ]] || return 1
    grep -q '^experimental = \["wrapper-scripts"\]$' \
        "$scratch/scaled-nextest.toml" || return 1
    grep -q '^target-runner = "within-wrapper"$' \
        "$scratch/scaled-nextest.toml" || return 1
    if compgen -G "$scratch/hermit-nextest-config.*.toml" >/dev/null; then
        printf 'run-nextest-counted: temporary nextest config leaked\n' >&2
        return 1
    fi

    # Preserve the original zero-terminal launch failure separately from the
    # executed exit23 control above. Stderr's twelve failures are not test rows.
    launch_failure=1
    ordinary_attempts=$attempts
    attempts="$scratch/launch-attempts"
    mkdir "$attempts"
    status=0
    got=$(TMPDIR="$scratch" DAGRUN_TEST_COUNTS_PATH="$scratch/launch-counts.json" \
        run_nextest "$scratch/launch-events" "$wall_multiplier" "$cpu_wrapper" \
        "$attempts" "$binary_map" "$inventory" "$scratch/launch-cpu.json" \
        --profile ci -p suite -j 1 --retries 2 -- --skip skipme) || status=$?
    [[ $status == 100 ]] || return 1
    [[ $got == *$'running 0 tests\ntest result: FAILED. 0 passed; 0 failed; 0 ignored; 0 filtered out' ]] || return 1
    [[ $got != *'test result: ok.'* ]] || return 1
    [[ $(<"$scratch/launch-counts.json") == \
        '{"executed_tests":0,"filtered_tests":0,"results":[],"schema":2}' ]] || return 1
    jq -e '. == {schema:3, run_id:null, attempts:[]}' "$scratch/launch-cpu.json" >/dev/null || return 1
    cmp -s "$scratch/expected-list-arguments" "$scratch/list-arguments" || return 1
    if compgen -G "$scratch/hermit-nextest-config.*.toml" >/dev/null; then return 1; fi

    status=0
    DAGRUN_TEST_COUNTS_PATH="$scratch/launch-wrong-count.json" NEXTEST_EXPECTED_EXECUTED=1 \
        emit_libtest_count "$scratch/launch-events" 100 "$attempts" "$binary_map" \
        "$scratch/launch-wrong-cpu.json" >"$scratch/launch-wrong.stdout" 2>"$scratch/launch-wrong.stderr" || status=$?
    [[ $status == 2 ]] || return 1
    grep -q 'expected 1 tests to execute, saw 0' "$scratch/launch-wrong.stderr" || return 1
    # The refusal must carry the outcome it already knows. Withholding the typed
    # report is deliberate and asserted on the next line, so this message is the
    # only place the run can say whether the tests it DID run passed. Without it
    # an all-green population drift and a failing one read identically.
    grep -q 'of which 0 passed and 0 failed' "$scratch/launch-wrong.stderr" || return 1
    [[ ! -e $scratch/launch-wrong-count.json && ! -e $scratch/launch-wrong-cpu.json ]] || return 1
    status=0
    DAGRUN_TEST_COUNTS_PATH="$scratch/failed-wrong-count.json" NEXTEST_EXPECTED_EXECUTED=2 \
        emit_libtest_count "$scratch/wrapper-events" 100 "$ordinary_attempts" "$binary_map" \
        "$scratch/failed-wrong-cpu.json" >"$scratch/failed-wrong.stdout" 2>"$scratch/failed-wrong.stderr" || status=$?
    [[ $status == 2 ]] || return 1
    grep -q 'expected 2 tests to execute, saw 1' "$scratch/failed-wrong.stderr" || return 1
    grep -q 'of which 0 passed and 1 failed' "$scratch/failed-wrong.stderr" || return 1
    [[ ! -e $scratch/failed-wrong-count.json && ! -e $scratch/failed-wrong-cpu.json ]] || return 1
    status=0
    DAGRUN_TEST_COUNTS_PATH="$scratch/launch-stray-count.json" \
        emit_libtest_count "$scratch/launch-events" 100 "$ordinary_attempts" "$binary_map" \
        "$scratch/launch-stray-cpu.json" >"$scratch/launch-stray.stdout" 2>"$scratch/launch-stray.stderr" || status=$?
    [[ $status == 2 ]] || return 1
    grep -q 'unexpected=' "$scratch/launch-stray.stderr" || return 1
    [[ ! -e $scratch/launch-stray-count.json && ! -e $scratch/launch-stray-cpu.json ]] || return 1
    unset -f cargo

    printf '%s\n' \
        '{"type":"test","event":"future","name":"suite::suite$case"}' \
        >"$scratch/unknown-event.jsonl"
    status=0
    DAGRUN_TEST_COUNTS_PATH="$scratch/refused.json" \
        emit_libtest_count "$scratch/unknown-event.jsonl" 0 \
        >/dev/null 2>"$scratch/refusal" || status=$?
    [[ $status == 2 ]] || return 1
    grep -q 'unsupported test event "future"' "$scratch/refusal" || return 1
    [[ ! -e $scratch/refused.json ]] || return 1

    printf 'run-nextest-counted: self-test PASS (typed counts, executed failure, zero-terminal launch failure, CPU reconciliation, refusal controls)\n'
}

if [[ ${1:-} == --self-test ]]; then
    self_test
    exit
fi

events_log=$(mktemp) || exit $?
cpu_measurement_dir=$(mktemp -d "${TMPDIR:-/tmp}/hermit-nextest-cpu.XXXXXX") || exit $?
cpu_attempt_records="$cpu_measurement_dir/attempts"
cpu_inventory="$cpu_measurement_dir/inventory.json"
cpu_binary_map="$cpu_measurement_dir/binary-map.json"
mkdir "$cpu_attempt_records" || exit $?
trap 'cleanup_nextest_config; cleanup_cpu_measurement_dir; rm -f "$events_log"' EXIT
cpu_wrapper=$(build_cpu_wrapper) || exit $?
cpu_report=$(configured_cpu_report_path) || exit $?
run_nextest "$events_log" "$(configured_wall_timeout_multiplier)" "$cpu_wrapper" \
    "$cpu_attempt_records" "$cpu_binary_map" "$cpu_inventory" "$cpu_report" "$@"
