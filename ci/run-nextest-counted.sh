#!/usr/bin/env bash

set -uo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
readonly SCRIPT_DIR
readonly RESULT_WRITER="$SCRIPT_DIR/nextest-test-results.rs"
readonly TIMEOUT_CONFIG_WRITER="$SCRIPT_DIR/nextest-timeout-config.rs"
readonly PREBUILT_RUST_SCRIPT="$SCRIPT_DIR/rust-script-bin/rust-script"
# shellcheck source=scripts/lib/landing_branch.sh
source "$SCRIPT_DIR/../scripts/lib/landing_branch.sh"

nextest_config=
cpu_measurement_dir=
calibration_launch=
calibration_host=false
nextest_budget_map=
nextest_budget_sha=

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
    local -a context_args=()
    shift
    if [[ ${HERMIT_PREPARED_NEXTEST_REQUIRED:-0} == 1 ]]; then
        [[ -z $calibration_launch ]] || context_args+=(--launch-proof "$calibration_launch")
        if [[ $operation == run && -n $nextest_budget_map ]]; then
            context_args+=(--budget-map "$nextest_budget_map" "$nextest_budget_sha")
        fi
        run_rust_script "$SCRIPT_DIR/nextest-binaries.rs" "$operation" "${context_args[@]}" --config-file "$nextest_config" "$@"
    else
        # Nextest builds with `cargo test --no-run` here, so a stale committed
        # Cargo.lock must stop the build, never be rewritten
        # (ci/manifest-plan/src/locked_cargo.rs). Nextest refuses a repeated
        # --locked, so a caller's own before any `--` is dropped.
        local -a forwarded=()
        local arg separated=0
        for arg in "$@"; do
            if [[ $arg == -- ]]; then
                separated=1
            fi
            if ((separated)) || [[ $arg != --locked ]]; then
                forwarded+=("$arg")
            fi
        done
        cargo nextest --config-file "$nextest_config" "$operation" --locked "${forwarded[@]}"
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

# Where to keep the typed event stream:
# - the path a caller asked for (HERMIT_NEXTEST_EVENTS_PATH);
# - else, for a counted DAG step of a validation with a durable result root,
#   $E2E_RESULT_ROOT/nextest-events/<step>.<attempt>.jsonl, so that two runs can be compared
#   test by test (each test's typed outcome, in both directions) and a lost test is named.
#   <step> is the dagrun step tag (DAGRUN_TEST_COUNTS_PATH's basename without `.json`, for
#   example `test.detcore_unit`). The census check in run_nextest derives its suite name from
#   the same basename but also strips a leading `test.` (`detcore_unit`), so a reader that joins
#   streams to census suites strips it too. <attempt> is DAGRUN_STEP_STARTED_MONOTONIC_NS, so a
#   retried step keeps each attempt's stream instead of being refused for an existing file;
# - else `-`, none. A counts path of `-` is not a counted step.
# Beside a stream kept by that default go the attempt's start marker, <step>.<attempt>.started,
# written before nextest runs, and its typed per-test results, <step>.<attempt>.results.json: a
# copy of the result writer's DAGRUN_TEST_COUNTS_PATH file (nextest-test-results.rs, dagrun's
# structured test results, with runtime skips and SKID-INFRA already typed), which dagrun does
# not keep. ci/buck-build-compare.py compares runs by those results; the marker makes an attempt
# that was killed before it published results visible as one without a result.
function configured_events_path {
    if [[ ${HERMIT_NEXTEST_EVENTS_PATH+x} ]]; then
        if [[ -z $HERMIT_NEXTEST_EVENTS_PATH ]]; then
            printf 'run-nextest-counted: HERMIT_NEXTEST_EVENTS_PATH must not be empty\n' >&2
            return 2
        fi
        printf '%s\n' "$HERMIT_NEXTEST_EVENTS_PATH"
    elif [[ -n ${E2E_RESULT_ROOT:-} && -n ${DAGRUN_TEST_COUNTS_PATH:-} && $DAGRUN_TEST_COUNTS_PATH != - ]]; then
        local step=${DAGRUN_TEST_COUNTS_PATH##*/} attempt=${DAGRUN_STEP_STARTED_MONOTONIC_NS:-}
        step=${step##*:}
        step=${step%.json}
        [[ -n $step ]] || step=nextest
        [[ $attempt =~ ^[0-9]+$ ]] || attempt=$(date +%s%N)-$$
        if ! mkdir -p -- "$E2E_RESULT_ROOT/nextest-events"; then
            printf 'run-nextest-counted: cannot create %s to keep the typed event stream\n' "$E2E_RESULT_ROOT/nextest-events" >&2
            return 2
        fi
        printf '%s\n' "$E2E_RESULT_ROOT/nextest-events/$step.$attempt.jsonl"
    else
        printf '%s\n' -
    fi
}

# Keep a copy of the typed event stream (libtest-json-plus, message format
# 0.1) for a caller that asked for one: its failed events carry each failing
# test's captured output, which ci/kvm-gap-ledger.rs matches against a gap
# row's expected-failure signature. It never replaces an existing file.
function retain_events {
    local events=$1 destination=$2
    [[ $destination == - ]] && return 0
    if ! (set -o noclobber; cat -- "$events" >"$destination"); then
        printf 'run-nextest-counted: cannot keep the typed event stream at %s\n' "$destination" >&2
        return 2
    fi
}

# The node's status once its records are kept: a failed keep (2) fails only a run that would
# otherwise pass.
function kept_status { # $1: the run's status; $2: the keeping's
    if (($1 == 0 && $2 != 0)); then
        printf '%s\n' "$2"
    else
        printf '%s\n' "$1"
    fi
}

# The start marker and the kept typed results that go beside a default-kept stream (above);
# `-` for a caller-chosen stream path or none.
function configured_results_prefix {
    local events_destination=$1
    if [[ $events_destination == - || ${HERMIT_NEXTEST_EVENTS_PATH+x} ]]; then
        printf '%s\n' -
    else
        printf '%s\n' "${events_destination%.jsonl}"
    fi
}

function mark_attempt_started { # $1: the results prefix, or -
    [[ $1 == - ]] && return 0
    if ! (set -o noclobber; : >"$1.started"); then
        printf 'run-nextest-counted: cannot write the start marker %s.started\n' "$1" >&2
        return 2
    fi
}

function retain_results { # $1: the result writer's file; $2: the results prefix, or -. A writer
    # that published nothing this attempt leaves nothing to keep: the marker says so.
    local results=$1 prefix=$2
    [[ $prefix == - || $results == - || ! -e $results ]] && return 0
    if ! (set -o noclobber; cat -- "$results" >"$prefix.results.json"); then
        printf 'run-nextest-counted: cannot keep the typed test results at %s.results.json\n' "$prefix" >&2
        return 2
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

function report_in_flight { # $1: the typed event stream. Name the tests that started but
    # never finished, on stderr. A CPU-cap kill cuts the stream before nextest prints any
    # captured output, so without this the node log names neither the in-flight tests nor
    # that they were cut (dh-for17). A truncated last line is ignored, never fatal.
    python3 - "$1" <<'PYEOF'
import json, re, sys
started = {}
finished = set()
try:
    lines = open(sys.argv[1], encoding="utf-8", errors="replace")
except OSError:
    lines = []
for line in lines:
    try:
        event = json.loads(line)
    except ValueError:
        continue
    if event.get("type") != "test" or not event.get("name"):
        continue
    name = re.sub(r"#\d+$", "", event["name"])
    if event.get("event") == "started":
        started.setdefault(name, 0)
        started[name] += 1
    elif event.get("event") in ("ok", "failed", "ignored"):
        finished.add(name)
in_flight = sorted(name for name in started if name not in finished)
if in_flight:
    print("run-nextest-counted: %d test(s) still in flight when the run ended: %s"
          % (len(in_flight), ", ".join(in_flight)), file=sys.stderr)
PYEOF
}

function check_selected_cases_listed { # $1: this selection's `nextest list` JSON.
    # validate --tests names its cases in HERMIT_NEXTEST_SELECTED_CASES:
    # whitespace-separated result IDs as run records print them,
    # <package>$<test name> for a library unit test and
    # <package>::<binary name>$<test name> for every other target. Every
    # named case must be in this listing: one that is not cannot run, and
    # since the run executes exactly what is listed, it would otherwise pass
    # unrun (a nonexistent name lists nothing and passes with zero tests).
    # Refuse before any test runs, naming each unlisted case. Returns 0 when
    # every named case is listed (or none is named), 1 when one is not, and
    # 2 when the listing cannot be read. The split covers every whitespace
    # character, newlines included, so no later line goes unchecked.
    local inventory=$1 output
    local -a selected=() unlisted=()
    read -r -d '' -a selected <<<"${HERMIT_NEXTEST_SELECTED_CASES:-}" || true
    ((${#selected[@]})) || return 0
    if ! output=$(run_rust_script "$RESULT_WRITER" --unlisted-cases "$inventory" "${selected[@]}"); then
        printf 'run-nextest-counted: cannot check the selected cases against the nextest inventory\n' >&2
        return 2
    fi
    [[ -n $output ]] || return 0
    mapfile -t unlisted <<<"$output"
    printf 'run-nextest-counted: case-unmatched: validate --tests named %s of %s case(s) that this node does not list, so they cannot run; refusing before any test runs. Unlisted:\n' \
        "${#unlisted[@]}" "${#selected[@]}" >&2
    printf '  %s\n' "${unlisted[@]}" >&2
    printf 'run-nextest-counted: a case ID is a run record test_cases entry: <package>$<test name> for a library unit test, <package>::<binary name>$<test name> for every other target; it is unlisted when that test does not exist, when the node does not build its binary, or when the node'"'"'s own selection (its filters, --skip, #[ignore]) leaves it out. A package or binary name nextest does not know never gets here: nextest'"'"'s filter parser refuses it first (exit 2), naming it\n' >&2
    return 1
}

function merge_case_filter { # $1: output array name; $2: the case filter; the rest: nextest run arguments.
    # nextest ORs separate filtersets, so the case filter passed as one
    # more -E would widen a node that passes its own: the node's whole
    # population would run, plus any named case its filter excludes. So
    # every filterset before the first `--` comes out and one goes in, also
    # before `--`: ((<node filterset>) | ...) & (<case filter>), or the case
    # filter alone when the node has none. A named case outside the node's
    # population is then unlisted, and check_selected_cases_listed refuses
    # it by name. The grammar is nextest 0.9.100's: -E and --filterset (with
    # its alias --filter-expr) take a separate or `=` value, and -E also an
    # attached one; the values of its other value-taking options are
    # skipped, so a value is never read as a filterset. A short cluster
    # holding E after other flags (-rE) is refused rather than guessed at.
    local -n merged=$1
    local case_filter=$2 arg
    local -a node_filters=() rest=()
    shift 2
    merged=()
    while (($#)); do
        arg=$1
        if [[ $arg == -- ]]; then
            rest=("$@")
            break
        fi
        # -v, -h and -r are nextest's only short flags without a value, so
        # only they can come before an E in one cluster.
        if [[ $arg =~ ^-[vhr]+E ]]; then
            printf 'run-nextest-counted: cannot merge the case filter with %s: pass -E on its own\n' "$arg" >&2
            return 2
        fi
        case $arg in
            -E|--filterset|--filter-expr)
                if (($# < 2)); then
                    printf 'run-nextest-counted: %s requires a value\n' "$arg" >&2
                    return 2
                fi
                node_filters+=("$2")
                shift 2
                continue
                ;;
            --filterset=*|--filter-expr=*) node_filters+=("${arg#*=}") ;;
            -E=*) node_filters+=("${arg#-E=}") ;;
            -E?*) node_filters+=("${arg#-E}") ;;
            -p|--package|--exclude|--bin|--example|--test|--bench|-F|--features|\
                --build-jobs|--cargo-profile|--target|--target-dir|--manifest-path|\
                --config|-Z|--run-ignored|--partition|--platform-filter|-j|\
                --test-threads|--retries|--max-fail|--no-tests|--failure-output|\
                --success-output|--status-level|--final-status-level|\
                --message-format|--message-format-version|--archive-file|\
                --archive-format|--extract-to|--cargo-metadata|--workspace-remap|\
                --binaries-metadata|--target-dir-remap|--config-file|\
                --tool-config-file|-P|--profile|--color)
                if (($# < 2)); then
                    printf 'run-nextest-counted: %s requires a value\n' "$arg" >&2
                    return 2
                fi
                merged+=("$1" "$2")
                shift 2
                continue
                ;;
            *) merged+=("$arg") ;;
        esac
        shift
    done
    if ((${#node_filters[@]})); then
        local joined filter
        joined=
        for filter in "${node_filters[@]}"; do
            joined+="${joined:+ | }($filter)"
        done
        case_filter="($joined) & ($case_filter)"
    fi
    merged+=(--filter-expr "$case_filter" "${rest[@]}")
}

function emit_libtest_count {
    local events=$1 status=${2:-0} path=${DAGRUN_TEST_COUNTS_PATH:--}
    local attempts=${3-} binary_map=${4-} cpu_report=${5-} inventory=${6-}
    local writer_status=0
    local -a cpu_args=() inventory_args=()
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
    if [[ -n $inventory ]]; then
        inventory_args=(--inventory "$inventory")
    fi
    run_rust_script "$RESULT_WRITER" "${inventory_args[@]}" "$events" "$status" "$path" "${cpu_args[@]}" || writer_status=$?
    if ((writer_status != 0)); then
        printf 'run-nextest-counted: cannot derive typed test results from %s\n' "$events" >&2
        return "$writer_status"
    fi
    # Zero-skips gate (opt-in per node, HERMIT_FORBID_SKIPS=1): a node that
    # must really run its population (the cli_kvm nodes, which already
    # refuse a host without /dev/kvm) fails when any test reported a
    # runtime skip, naming each skipped test and its reason. The typed
    # report stays published; only the node's status changes.
    if [[ ${HERMIT_FORBID_SKIPS:-} == 1 && $path != - && -n $path ]]; then
        if ! python3 - "$path" <<'PYEOF'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as source:
    report = json.load(source)
skips = [
    row for row in report.get("results", []) if row.get("result") == "skip"
]
for row in skips:
    print(
        f"run-nextest-counted: FORBID-SKIPS: {row.get('id')}: "
        f"{row.get('skip_reason') or 'no reason recorded'}",
        file=sys.stderr,
    )
if skips:
    print(
        f"run-nextest-counted: FORBID-SKIPS: {len(skips)} skipped test(s) "
        "in a zero-skips node",
        file=sys.stderr,
    )
sys.exit(1 if skips else 0)
PYEOF
        then
            return 1
        fi
    fi
}

function run_nextest {
    local events_log=$1 wall_multiplier=$2 cpu_wrapper=$3 attempts=$4 binary_map=$5
    local inventory=$6 cpu_report=$7
    local budget_map="$4/../resolved-budgets.json" budget_context="$4/../budget-context.json"
    local status count_status=0
    local -a inventory_arguments=()
    local -a launch_arguments=()
    shift 7
    # Case selection (validate --tests): HERMIT_NEXTEST_FILTER_EXPR holds a
    # nextest filter expression selecting exactly the recorded cases, ANDed
    # with the node's own filtersets (merge_case_filter). The inventory and
    # run invocations below share "$@" and stay consistent.
    if [[ -n ${HERMIT_NEXTEST_FILTER_EXPR:-} ]]; then
        local -a filtered_args=()
        merge_case_filter filtered_args "$HERMIT_NEXTEST_FILTER_EXPR" "$@" || return $?
        set -- "${filtered_args[@]}"
    fi

    if [[ $cpu_report != - && $cpu_report == "${DAGRUN_TEST_COUNTS_PATH:--}" ]]; then
        printf 'run-nextest-counted: CPU report path must differ from DAGRUN_TEST_COUNTS_PATH\n' >&2
        return 2
    fi

    cleanup_nextest_config
    nextest_budget_map=
    nextest_budget_sha=
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
    # The expected executed count is derived from this same inventory (the
    # will-run testcases of `nextest list` for the identical
    # selection), never from a hand-maintained pin: a successful run must
    # execute exactly the listed population (executed == listed). Any
    # NEXTEST_EXPECTED_EXECUTED inherited from the DAG is superseded below.
    # HERMIT_NEXTEST_EXPECTED_EXECUTED_OVERRIDE exists only so the
    # structured-results fixture can still manufacture a count mismatch
    # (a listed test that never executes) against the derived population;
    # no DAG node sets it.
    local listed_count
    if [[ -n ${HERMIT_NEXTEST_EXPECTED_EXECUTED_OVERRIDE:-} ]]; then
        listed_count=$HERMIT_NEXTEST_EXPECTED_EXECUTED_OVERRIDE
    elif ! listed_count=$(run_rust_script "$RESULT_WRITER" --listed-count "$inventory"); then
        printf 'run-nextest-counted: cannot derive listed test count from nextest inventory\n' >&2
        cleanup_nextest_config
        return 2
    fi
    # validate --tests: every named case must be in this listing, checked
    # here, before any test runs. Executed == listed (below) then makes every
    # named case run.
    local selection_status=0
    check_selected_cases_listed "$inventory" || selection_status=$?
    if ((selection_status != 0)); then
        cleanup_nextest_config
        return "$selection_status"
    fi
    # Retain the census identities beside the counts report: the landing
    # census gate diffs these against the base run's, so a test that stops
    # being listed is named instead of hidden inside a drifting count.
    # Derived from this same inventory; a failure here is the same class
    # as failing to derive the count.
    if [[ -n ${DAGRUN_TEST_COUNTS_PATH:-} && ${DAGRUN_TEST_COUNTS_PATH} != - ]]; then
        if ! run_rust_script "$RESULT_WRITER" --listed-ids "$inventory" "${DAGRUN_TEST_COUNTS_PATH}.listed-ids"; then
            printf 'run-nextest-counted: cannot derive listed test identities from nextest inventory\n' >&2
            cleanup_nextest_config
            return 2
        fi
        # Census gate: every run refuses a node that lists zero tests; a run
        # given a base also diffs its listed population against the base (last
        # landed main) population the queue adapter cached, so a vanished test
        # fails. Only candidate-versus-main comparisons need that base, and
        # ci-hub supplies it to the validates it runs; a plain run has none,
        # needs none, and its ledger row says census_checked=false.
        # HERMIT_CENSUS_BASE_LISTED_IDS names that base .listed-ids
        # file (single-suite selections); HERMIT_CENSUS_BASE_DIR names the
        # adapter's per-suite supply directory (one stamped
        # <suite>.listed-ids per suite) for multi-suite/full-profile nodes,
        # resolved per node below; HERMIT_CENSUS_DROPPED_FILE names
        # Test-Dropped records dumped
        # from `git log base..head`. Base contract (adapter-owned): cached
        # from a full counted run at the last landed main commit, keyed by
        # that commit, never mutated in place, and stamped by its producer
        # with a "# census-base: commit=<sha> count=<n>" first line.
        # Freshness is checked hermit-side in ci/census-check.sh: the
        # expected base commit is HERMIT_CENSUS_BASE_COMMIT when the
        # adapter declares one, else this checkout's merge-base with
        # origin/<landing branch> (HERMIT_LANDING_BRANCH,
        # ci/landing-branches.env); a base stamped with another commit, a short/
        # truncated base, or an unstamped base is refused by name. A
        # supplied but stale/truncated/unstamped base, a vanished ID and a
        # zero-test population are product failures.
        # HERMIT_CENSUS_REPORT_ONLY is retired in counted nodes (phase-2
        # rollout lever): it is ignored here so a stale adapter setting
        # cannot silently downgrade enforcement; --report-only remains a
        # manual ci/census-check.sh debugging aid only.
        # This node's suite is the one its counts report is named for
        # (test.<suite>.json, the census cache's suite key); the per-suite
        # base file carries the same name under HERMIT_CENSUS_BASE_DIR.
        local census_suite counts_name=${DAGRUN_TEST_COUNTS_PATH##*/}
        counts_name=${counts_name##*:}
        counts_name=${counts_name%.json}
        census_suite=${counts_name#test.}
        [[ -n $census_suite && $census_suite != "$counts_name" ]] || census_suite=${counts_name:-nextest}
        local -a census_args=(--candidate-listed "${DAGRUN_TEST_COUNTS_PATH}.listed-ids" --suite "$census_suite")
        [[ -n ${HERMIT_CENSUS_BASE_LISTED_IDS:-} ]] && census_args+=(--base-listed "$HERMIT_CENSUS_BASE_LISTED_IDS")
        [[ -n ${HERMIT_CENSUS_BASE_DIR:-} ]] && census_args+=(--base-dir "$HERMIT_CENSUS_BASE_DIR")
        # Case-selected runs list a subset, never the suite population:
        # census-check names that treatment instead of comparing the
        # full base, and still refuses a selection that lists nothing.
        [[ -n ${HERMIT_NEXTEST_FILTER_EXPR:-} ]] && census_args+=(--case-selected)
        [[ -n ${HERMIT_CENSUS_DROPPED_FILE:-} ]] && census_args+=(--dropped "$HERMIT_CENSUS_DROPPED_FILE")
        local expected_base_commit=${HERMIT_CENSUS_EXPECTED_BASE_COMMIT:-${HERMIT_CENSUS_BASE_COMMIT:-}}
        if [[ -z $expected_base_commit ]]; then
            local landing_branch_name
            if ! landing_branch_name=$(landing_branch HERMIT_LANDING_BRANCH); then
                printf 'run-nextest-counted: the census base defaults to the landing branch, whose setting cannot be read\n' >&2
                cleanup_nextest_config
                return 1
            fi
            expected_base_commit=$(git -C "$SCRIPT_DIR/.." merge-base HEAD "origin/$landing_branch_name" 2>/dev/null || true)
            [[ -n $expected_base_commit ]] || \
                expected_base_commit=$(git -C "$SCRIPT_DIR/.." rev-parse "origin/$landing_branch_name" 2>/dev/null || true)
        fi
        [[ -n $expected_base_commit ]] && census_args+=(--expected-base-commit "$expected_base_commit")
        if [[ ${HERMIT_CENSUS_REPORT_ONLY:-} == 1 ]]; then
            printf 'run-nextest-counted: HERMIT_CENSUS_REPORT_ONLY=1 is ignored in counted nodes since the count pins were deleted (census stays enforced)\n' >&2
        fi
        local census_status=0
        "$SCRIPT_DIR/census-check.sh" "${census_args[@]}" || census_status=$?
        if (( census_status != 0 )); then
            printf 'run-nextest-counted: census gate refused this listed population (see census FAIL lines above)\n' >&2
            cleanup_nextest_config
            # Every census refusal (zero tests, a stale/truncated/unstamped
            # base, a vanished ID) is a product failure.
            return 1
        fi
    fi

    # Prepared consumers can bind calibration to the exact checked build and
    # selected inventory. Standalone/unsupported populations keep the existing
    # path. Never infer a calibration context from unchecked executable names.
    # The context is recomputed at run time and compared exactly, so a field
    # that moves between resolution and run (a cpuset change, a dirtied tree,
    # a refreshed prepared record) refuses before any test executes. That
    # refusal is safe to retry exactly once after re-resolving: counted,
    # bounded at one re-resolution, and the second refusal stands.
    # The stderr capture sits beside the attempts directory, never inside
    # it: HERMIT_NEXTEST_CPU_RECORD_DIR is read back as attempt records
    # only, and one plain-text file there refuses the whole typed report.
    local budget_attempt=0 run_stderr="$attempts/../budget-run-stderr.txt"
    while :; do
        budget_attempt=$((budget_attempt + 1))
        if [[ ${HERMIT_PREPARED_NEXTEST_REQUIRED:-0} == 1 && $cpu_report != - ]]; then
            if (( budget_attempt == 1 )); then
                [[ -z $calibration_launch ]] || launch_arguments+=(--launch-proof "$calibration_launch")
            fi
            if (( budget_attempt > 1 )); then
                # The resolver creates its map with create_new; the first
                # attempt's map and context are retained beside the CPU
                # report, so the invocation-local ones can go.
                rm -f -- "$budget_map" "$budget_context"
            fi
            if ! run_rust_script "$SCRIPT_DIR/nextest-binaries.rs" budget-context \
                "${launch_arguments[@]}" "${inventory_arguments[@]}" >"$budget_context"; then
                cleanup_nextest_config
                return 2
            fi
            if ! HERMIT_NEXTEST_CPU_WRAPPER_BIN="$cpu_wrapper" run_rust_script "$TIMEOUT_CONFIG_WRITER" \
                --resolve "$SCRIPT_DIR/../.config/nextest.toml" "$wall_multiplier" "$nextest_config" \
                "$inventory" "$budget_context" "$budget_map" "$SCRIPT_DIR/../.config/nextest-budgets.json" \
                -- "$@"; then
                cleanup_nextest_config
                return 2
            fi
            if [[ -f $budget_map ]]; then
                local digest_line retained="$cpu_report.budgets.json"
                (( budget_attempt == 1 )) || retained="$cpu_report.budgets.reresolved.json"
                digest_line=$(sha256sum -- "$budget_map") || return $?
                nextest_budget_map=$budget_map
                nextest_budget_sha=${digest_line%% *}
                # Keep the exact digest-bound CPU/wall input on both success and
                # failure, beside the existing attempt report. Never replace an
                # earlier invocation's retained calibration evidence.
                if ! (set -o noclobber; cat -- "$budget_map" >"$retained"); then
                    cleanup_nextest_config
                    return 2
                fi
            fi
        fi

    # A schema-5 node gets a fresh skip sidecar for this run: test helpers
    # append one HERMIT-TEST-SKIP line per runtime skip to it, and the
    # result writer classifies those rows from it after the run. A budget
    # re-resolution retry ran no tests, so it keeps the first sidecar.
    local skip_file=
    if [[ ${HERMIT_NEXTEST_RESULTS_SCHEMA:-} == 5 && -z ${HERMIT_TEST_SKIP_FILE:-} ]]; then
        skip_file=$(mktemp "${TMPDIR:-/tmp}/hermit-test-skips.XXXXXX") || return $?
        export HERMIT_TEST_SKIP_FILE="$skip_file"
    fi

        set +e
        # nextest's stderr remains presentation. On the prepared path it is
        # captured only so the context-changed refusal can be told from a
        # test failure, then replayed verbatim; other paths keep it live.
        # The versioned stdout event stream is the only input to the
        # aggregate count and per-test result adapter.
        if [[ -n $nextest_budget_map ]]; then
            HERMIT_NEXTEST_CPU_BINARY_MAP="$binary_map" \
                HERMIT_NEXTEST_CPU_RECORD_DIR="$attempts" \
                HERMIT_NEXTEST_CPU_WRAPPER_BIN="$cpu_wrapper" \
                NEXTEST_EXPERIMENTAL_LIBTEST_JSON=1 invoke_nextest run \
                --color never --message-format libtest-json-plus --message-format-version 0.1 \
                "$@" >"$events_log" 2>"$run_stderr"
            status=$?
            cat "$run_stderr" >&2
        else
            HERMIT_NEXTEST_CPU_BINARY_MAP="$binary_map" \
                HERMIT_NEXTEST_CPU_RECORD_DIR="$attempts" \
                HERMIT_NEXTEST_CPU_WRAPPER_BIN="$cpu_wrapper" \
                NEXTEST_EXPERIMENTAL_LIBTEST_JSON=1 invoke_nextest run \
                --color never --message-format libtest-json-plus --message-format-version 0.1 \
                "$@" >"$events_log"
            status=$?
        fi
        set -e
        if (( status == 0 || budget_attempt > 1 )) || [[ -z $nextest_budget_map ]] \
            || ! grep -q "context changed after budget resolution" "$run_stderr"; then
            break
        fi
        printf 'run-nextest-counted: budget context changed after resolution; re-resolving once (attempt %s of 2)\n' \
            "$((budget_attempt + 1))" >&2
    done
    cleanup_nextest_config

    # Gate only a successful run on executed == listed: a failed run already
    # fails the node by its status, and its typed report must still publish
    # for diagnosis. An inherited pin is never consulted on either path.
    if ((status == 0)); then
        NEXTEST_EXPECTED_EXECUTED="$listed_count" \
            emit_libtest_count "$events_log" "$status" "$attempts" "$binary_map" \
            "$cpu_report" "$inventory" || count_status=$?
    else
        (unset NEXTEST_EXPECTED_EXECUTED; \
            emit_libtest_count "$events_log" "$status" "$attempts" "$binary_map" \
            "$cpu_report" "$inventory") || count_status=$?
    fi
    if ((status != 0)); then
        report_in_flight "$events_log"
    fi
    if [[ -n $skip_file ]]; then
        rm -f "$skip_file"
        unset HERMIT_TEST_SKIP_FILE
    fi
    # Publication cannot erase a failure that the actual test runner observed.
    if ((status != 0)); then
        return "$status"
    fi
    return "$count_status"
}

function self_test_failed { # $1: the line ($LINENO) of the assertion that failed.
    # A failed assertion names itself: the line and its text, joined across
    # backslash continuations, so a red lint-checks run says what broke.
    printf 'run-nextest-counted: self-test FAIL at line %s: %s\n' "$1" "$(awk -v last="$1" '
        { line[NR] = $0 }
        END {
            first = last
            while (first > 1 && line[first - 1] ~ /\\$/) first--
            for (i = first; i <= last; i++) {
                text = line[i]; sub(/^[[:space:]]+/, "", text); sub(/[[:space:]]*\\$/, "", text)
                out = out (i == first ? "" : " ") text
            }
            print out
        }' "${BASH_SOURCE[0]}")" >&2
    return 1
}

function self_test {
    local scratch got expected status=0 wall_multiplier cpu_wrapper fixture_binary attempts
    local binary_map inventory launch_failure=0 empty_listing=0 inventory_wrong_name=0
    local ordinary_attempts ordinary_inventory inherited
    # The verdict depends on this tree alone. Each case sets every input it
    # exercises, so an inherited one can only change what a case tests: a
    # validation's census base (HERMIT_CENSUS_BASE_DIR) arms the census gate
    # against the fixture's own suite and refuses it. So clear, on entry, every
    # inherited variable in the namespaces this wrapper and its helpers read
    # (HERMIT_, NEXTEST_, DAGRUN_, E2E_), except the three that choose how a
    # rust-script helper is started, never what it decides: a validation runs
    # this self-test with prebuilt helpers.
    while IFS= read -r inherited; do
        case $inherited in
            HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED|HERMIT_RUST_SCRIPT_ARTIFACT_ROOT|HERMIT_REAL_RUST_SCRIPT) ;;
            HERMIT_*|NEXTEST_*|DAGRUN_*|E2E_*) unset "$inherited" ;;
        esac
    done < <(compgen -e)
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
    [[ $got == "$expected" ]] || { self_test_failed "$LINENO"; return 1; }
    python3 - "$scratch/counts.json" <<'PYEOF' || { self_test_failed "$LINENO"; return 1; }
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

    # A cut stream names its in-flight tests (dh-for17): one finished, one
    # started and never finished, and a truncated last line is ignored.
    printf '%s\n' \
        '{"type":"test","event":"started","name":"suite::suite$passes"}' \
        '{"type":"test","event":"ok","name":"suite::suite$passes","exec_time":0.1}' \
        '{"type":"test","event":"started","name":"suite::suite$cut"}' \
        '{"type":"test","event":"started","name":"suite::suite$cut' \
        >"$scratch/cut-events.jsonl"
    got=$(report_in_flight "$scratch/cut-events.jsonl" 2>&1 >/dev/null)
    [[ $got == 'run-nextest-counted: 1 test(s) still in flight when the run ended: suite::suite$cut' ]] || { self_test_failed "$LINENO"; return 1; }
    got=$(report_in_flight "$scratch/events.jsonl" 2>&1 >/dev/null)
    [[ -z $got ]] || { self_test_failed "$LINENO"; return 1; }

    printf 'HERMIT-TEST-SKIP: passes\tno /dev/kvm on this host\n' \
        >"$scratch/skips.txt"
    got=$(HERMIT_NEXTEST_RESULTS_SCHEMA=5 HERMIT_TEST_SKIP_FILE="$scratch/skips.txt" \
        DAGRUN_TEST_COUNTS_PATH="$scratch/skip-counts.json" \
        emit_libtest_count "$scratch/events.jsonl" 0)
    expected=$'running 2 tests\ntest result: ok. 1 passed; 0 failed; 1 skipped; 0 ignored; 7 filtered out'
    [[ $got == "$expected" ]] || { self_test_failed "$LINENO"; return 1; }
    python3 - "$scratch/skip-counts.json" <<'PYEOF' || { self_test_failed "$LINENO"; return 1; }
import json
import sys

with open(sys.argv[1], encoding="utf-8") as source:
    report = json.load(source)
assert report["schema"] == 5
assert report["executed_tests"] == 2
rows = {row["id"]: row for row in report["results"]}
assert rows["suite$passes"]["result"] == "skip"
assert rows["suite$passes"]["skip_reason"] == "no /dev/kvm on this host"
assert rows["suite$recovers"]["result"] == "pass"
PYEOF

    status=0
    HERMIT_TEST_SKIP_FILE="$scratch/skips.txt" \
        DAGRUN_TEST_COUNTS_PATH="$scratch/no-optin-counts.json" \
        emit_libtest_count "$scratch/events.jsonl" 0 \
        >/dev/null 2>"$scratch/no-optin.stderr" || status=$?
    [[ $status == 2 ]] || { self_test_failed "$LINENO"; return 1; }
    grep -q 'not opted into schema 5' "$scratch/no-optin.stderr" || { self_test_failed "$LINENO"; return 1; }
    [[ ! -e $scratch/no-optin-counts.json ]] || { self_test_failed "$LINENO"; return 1; }

    # A zero-skips node fails on the same skip population, naming the
    # skipped test, while the typed report still publishes.
    status=0
    HERMIT_NEXTEST_RESULTS_SCHEMA=5 HERMIT_TEST_SKIP_FILE="$scratch/skips.txt" \
        HERMIT_FORBID_SKIPS=1 \
        DAGRUN_TEST_COUNTS_PATH="$scratch/forbid-skip-counts.json" \
        emit_libtest_count "$scratch/events.jsonl" 0 \
        >/dev/null 2>"$scratch/forbid-skip.stderr" || status=$?
    [[ $status == 1 ]] || { self_test_failed "$LINENO"; return 1; }
    grep -q 'FORBID-SKIPS: suite\$passes: no /dev/kvm on this host' \
        "$scratch/forbid-skip.stderr" || { self_test_failed "$LINENO"; return 1; }
    [[ -e $scratch/forbid-skip-counts.json ]] || { self_test_failed "$LINENO"; return 1; }

    sed 's/"filtered_out":7/"filtered_out":11/' \
        "$scratch/events.jsonl" >"$scratch/mutated-events.jsonl"
    got=$(DAGRUN_TEST_COUNTS_PATH="$scratch/mutated-counts.json" \
        emit_libtest_count "$scratch/mutated-events.jsonl" 0)
    expected=$'running 2 tests\ntest result: ok. 2 passed; 0 failed; 0 ignored; 11 filtered out'
    [[ $got == "$expected" ]] || { self_test_failed "$LINENO"; return 1; }
    python3 - "$scratch/mutated-counts.json" <<'PYEOF' || { self_test_failed "$LINENO"; return 1; }
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
    [[ $got == "$expected" ]] || { self_test_failed "$LINENO"; return 1; }
    cmp -s "$scratch/expected-counts.json" "$scratch/counts.json" || { self_test_failed "$LINENO"; return 1; }

    status=0
    DAGRUN_TEST_COUNTS_PATH="$scratch/wrong-counts.json" \
        NEXTEST_EXPECTED_EXECUTED=3 emit_libtest_count "$scratch/events.jsonl" 0 \
        >"$scratch/wrong-count.stdout" 2>"$scratch/wrong-count.stderr" || status=$?
    [[ $status == 2 ]] || { self_test_failed "$LINENO"; return 1; }
    grep -q 'expected 3 tests to execute, saw 2' "$scratch/wrong-count.stderr" || { self_test_failed "$LINENO"; return 1; }
    [[ ! -e $scratch/wrong-counts.json ]] || { self_test_failed "$LINENO"; return 1; }

    status=0
    NEXTEST_EXPECTED_EXECUTED=unknown emit_libtest_count "$scratch/events.jsonl" 0 \
        >/dev/null 2>"$scratch/invalid-count.stderr" || status=$?
    [[ $status == 2 ]] || { self_test_failed "$LINENO"; return 1; }
    grep -q 'NEXTEST_EXPECTED_EXECUTED' "$scratch/invalid-count.stderr" || { self_test_failed "$LINENO"; return 1; }

    [[ $(configured_wall_timeout_multiplier) == 1 ]] || { self_test_failed "$LINENO"; return 1; }
    wall_multiplier=$(HERMIT_TEST_WALL_TIMEOUT_MULTIPLIER=1.5 \
        configured_wall_timeout_multiplier)
    [[ $wall_multiplier == 1.5 ]] || { self_test_failed "$LINENO"; return 1; }

    printf '%s\n' \
        '{"type":"suite","event":"started","test_count":1,"nextest":{"crate":"suite","test_binary":"suite","kind":"lib"}}' \
        '{"type":"test","event":"started","name":"suite::suite$fails"}' \
        '{"type":"test","event":"failed","name":"suite::suite$fails","exec_time":0.2}' \
        '{"type":"suite","event":"failed","passed":0,"failed":1,"ignored":0,"measured":0,"filtered_out":3,"exec_time":0.2,"nextest":{"crate":"suite","test_binary":"suite","kind":"lib"}}' \
        >"$scratch/failed-events.jsonl"
    DAGRUN_TEST_COUNTS_PATH="$scratch/failed-counts.json" \
        emit_libtest_count "$scratch/failed-events.jsonl" 100 >/dev/null || { self_test_failed "$LINENO"; return 1; }
    python3 - "$scratch/failed-counts.json" <<'PYEOF' || { self_test_failed "$LINENO"; return 1; }
import json
import sys

with open(sys.argv[1], encoding="utf-8") as source:
    report = json.load(source)
assert report["results"] == [
    {"id": "suite$fails", "result": "fail", "attempts": 1},
]
PYEOF

    # A skid-infra marker claiming the shared 5-attempt budget reclassifies
    # the failed row's terminal attempt as an infrastructure error; the row
    # stays a failure. Without the schema-5 opt-in the writer refuses.
    printf 'HERMIT-SKID-INFRA: fails\tall 5 attempt(s) exited 122 with `HERMIT_POLICY_REFUSAL class=policy-refusal cause=skid-overshoot count=2`; verdict SKID-INFRA (not a test failure)\n' \
        >"$scratch/skid-infra.txt"
    got=$(HERMIT_NEXTEST_RESULTS_SCHEMA=5 HERMIT_SKID_INFRA_FILE="$scratch/skid-infra.txt" \
        DAGRUN_TEST_COUNTS_PATH="$scratch/skid-infra-counts.json" \
        emit_libtest_count "$scratch/failed-events.jsonl" 100)
    expected=$'running 1 tests\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 3 filtered out'
    [[ $got == "$expected" ]] || { self_test_failed "$LINENO"; return 1; }
    python3 - "$scratch/skid-infra-counts.json" <<'PYEOF' || { self_test_failed "$LINENO"; return 1; }
import json
import sys

with open(sys.argv[1], encoding="utf-8") as source:
    report = json.load(source)
assert report["schema"] == 5
rows = {row["id"]: row for row in report["results"]}
terminal = rows["suite$fails"]["attempt_results"][-1]
assert rows["suite$fails"]["result"] == "fail"
assert terminal["outcome"] == "infrastructure_error"
assert terminal["detail"].startswith("SKID-INFRA: ")
PYEOF

    status=0
    HERMIT_SKID_INFRA_FILE="$scratch/skid-infra.txt" \
        DAGRUN_TEST_COUNTS_PATH="$scratch/skid-infra-no-optin.json" \
        emit_libtest_count "$scratch/failed-events.jsonl" 100 \
        >/dev/null 2>"$scratch/skid-infra-no-optin.stderr" || status=$?
    [[ $status == 2 ]] || { self_test_failed "$LINENO"; return 1; }
    grep -q 'skid-infra markers were recorded' \
        "$scratch/skid-infra-no-optin.stderr" || { self_test_failed "$LINENO"; return 1; }
    [[ ! -e $scratch/skid-infra-no-optin.json ]] || { self_test_failed "$LINENO"; return 1; }

    # The listed count is derived from the inventory: filter-matched
    # testcases only (executed == listed for a successful run).
    jq -nc '{
        "rust-suites": {
            "suite": {
                "package-name": "suite",
                "binary-id": "suite",
                "binary-name": "suite",
                "kind": "lib",
                "binary-path": "/tmp/suite-binary",
                "testcases": {
                    "runs": {"ignored": false, "filter-match": {"status": "matches"}},
                    "also_runs": {"ignored": false, "filter-match": {"status": "matches"}},
                    "ignored_one": {"ignored": true, "filter-match": {"status": "matches"}},
                    "filtered_out": {"ignored": false, "filter-match": {"status": "mismatch"}}
                }
            }
        }
    }' >"$scratch/listed-inventory.json"
    [[ $(run_rust_script "$RESULT_WRITER" --listed-count "$scratch/listed-inventory.json") == 3 ]] || { self_test_failed "$LINENO"; return 1; }
    run_rust_script "$RESULT_WRITER" --listed-ids "$scratch/listed-inventory.json" "$scratch/listed-ids.txt" || { self_test_failed "$LINENO"; return 1; }
    [[ $(cat "$scratch/listed-ids.txt") == $'suite/suite/also_runs\nsuite/suite/ignored_one\nsuite/suite/runs' ]] || { self_test_failed "$LINENO"; return 1; }

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
            jq -nc --arg executable "$fixture_binary" --argjson empty_listing "$empty_listing" \
                --argjson inventory_wrong_name "$inventory_wrong_name" '{
                "rust-suites": {
                    "suite": {
                        "package-name": "suite",
                        "binary-id": "suite",
                        "binary-name": "suite",
                        "kind": "lib",
                        "binary-path": $executable,
                        "testcases": (if $empty_listing == 1 then {} else {
                            (if $inventory_wrong_name == 1 then "substituted" else "failure" end):
                                {"ignored":false,"filter-match":{"status":"matches"}}
                        } end)
                    }
                }
            }'
            return
        fi
        [[ $operation == run ]] || return 99
        if ((launch_failure)); then
            printf '%s\n' \
                '{"type":"suite","event":"started","test_count":1,"nextest":{"crate":"suite","test_binary":"suite","kind":"lib"}}' \
                '{"type":"suite","event":"failed","passed":0,"failed":0,"ignored":0,"measured":0,"filtered_out":0,"exec_time":0.0,"nextest":{"crate":"suite","test_binary":"suite","kind":"lib"}}'
            printf 'Summary [   0.010s] 12 tests run: 0 passed, 12 exec failed, 0 skipped\n' >&2
            return 100
        fi
        local wrapper_status=0
        HERMIT_NEXTEST_CPU_CONTROL=1 CARGO_PKG_NAME=suite \
            NEXTEST_RUN_ID=self-test-nextest NEXTEST_ATTEMPT=1 \
            HERMIT_NEXTEST_CPU_CONTROL_CWD="$PWD" \
            HERMIT_NEXTEST_CPU_CONTROL_SENTINEL=preserved \
            LD_LIBRARY_PATH="$PWD/deps" \
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
    [[ $status == 100 ]] || { self_test_failed "$LINENO"; return 1; }
    ordinary_inventory="$scratch/ordinary-inventory.json"
    cp -- "$inventory" "$ordinary_inventory" || { self_test_failed "$LINENO"; return 1; }
    [[ $got == *$'running 1 tests\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 filtered out' ]] || { self_test_failed "$LINENO"; return 1; }
    [[ $got != *'test result: ok.'* ]] || { self_test_failed "$LINENO"; return 1; }
    printf '%s\n' --locked --color never --message-format json --profile ci -p suite \
        -- --skip skipme >"$scratch/expected-list-arguments"
    cmp -s "$scratch/expected-list-arguments" "$scratch/list-arguments" || { self_test_failed "$LINENO"; return 1; }
    [[ $(<"$scratch/wrapper-counts.json") == \
        '{"executed_tests":1,"filtered_tests":0,"results":[{"attempts":1,"id":"suite$failure","result":"fail"}],"schema":2}' ]] || { self_test_failed "$LINENO"; return 1; }
    jq -e '
        .schema == 3 and .run_id == "self-test-nextest" and
        (.attempts | length) == 1 and
        .attempts[0].identity == {package:"suite", binary:"suite", test:"failure", attempt:1} and
        .attempts[0].completion == {kind:"exit", code:23} and
        .attempts[0].cpu_source == "wait4-subtree"
    ' "$scratch/wrapper-cpu.json" >/dev/null || { self_test_failed "$LINENO"; return 1; }
    # validate --tests (dh-71wz9z): a named case the listing lacks fails the
    # node before any test runs, naming only that case; a listed one runs.
    mkdir "$scratch/selected-attempts" "$scratch/listed-attempts"
    status=0
    HERMIT_NEXTEST_SELECTED_CASES='suite$failure suite$absent' TMPDIR="$scratch" \
        DAGRUN_TEST_COUNTS_PATH="$scratch/selected-counts.json" \
        run_nextest "$scratch/selected-events" "$wall_multiplier" "$cpu_wrapper" \
        "$scratch/selected-attempts" "$scratch/selected-binary-map.json" \
        "$scratch/selected-inventory.json" "$scratch/selected-cpu.json" \
        --profile ci -p suite -j 1 --retries 2 >/dev/null 2>"$scratch/selected.stderr" || status=$?
    local listed_status=0
    HERMIT_NEXTEST_SELECTED_CASES='suite$failure' TMPDIR="$scratch" \
        DAGRUN_TEST_COUNTS_PATH="$scratch/listed-counts.json" \
        run_nextest "$scratch/listed-events" "$wall_multiplier" "$cpu_wrapper" \
        "$scratch/listed-attempts" "$scratch/listed-binary-map.json" \
        "$scratch/listed-inventory-run.json" "$scratch/listed-cpu.json" \
        --profile ci -p suite -j 1 --retries 2 >/dev/null 2>"$scratch/listed.stderr" || listed_status=$?
    [[ $status == 1 && ! -e $scratch/selected-events && ! -e $scratch/selected-counts.json ]] || { self_test_failed "$LINENO"; return 1; }
    grep -qx 'run-nextest-counted: case-unmatched: validate --tests named 1 of 2 case(s) that this node does not list, so they cannot run; refusing before any test runs. Unlisted:' \
        "$scratch/selected.stderr" || { self_test_failed "$LINENO"; return 1; }
    grep -qx '  suite\$absent' "$scratch/selected.stderr" || { self_test_failed "$LINENO"; return 1; }
    ! grep -qx '  suite\$failure' "$scratch/selected.stderr" || { self_test_failed "$LINENO"; return 1; }
    [[ $listed_status == 100 && -e $scratch/listed-events ]] || { self_test_failed "$LINENO"; return 1; }
    ! grep -q 'case-unmatched' "$scratch/listed.stderr" || { self_test_failed "$LINENO"; return 1; }
    # The named cases split on every whitespace character: a second line is
    # checked too, and a leading newline does not skip the check.
    local cases_value lines=0
    for cases_value in $'suite$failure\nsuite$absent' $'\nsuite$absent'; do
        lines=$((lines + 1))
        mkdir "$scratch/lines$lines-attempts"
        status=0
        HERMIT_NEXTEST_SELECTED_CASES=$cases_value TMPDIR="$scratch" \
            DAGRUN_TEST_COUNTS_PATH="$scratch/lines$lines-counts.json" \
            run_nextest "$scratch/lines$lines-events" "$wall_multiplier" "$cpu_wrapper" \
            "$scratch/lines$lines-attempts" "$scratch/lines$lines-binary-map.json" \
            "$scratch/lines$lines-inventory.json" "$scratch/lines$lines-cpu.json" \
            --profile ci -p suite -j 1 --retries 2 >/dev/null 2>"$scratch/lines.stderr" || status=$?
        [[ $status == 1 && ! -e $scratch/lines$lines-events && ! -e $scratch/lines$lines-counts.json ]] || { self_test_failed "$LINENO"; return 1; }
        grep -q "case-unmatched: validate --tests named 1 of $(wc -w <<<"$cases_value") case(s)" \
            "$scratch/lines.stderr" || { self_test_failed "$LINENO"; return 1; }
        grep -qx '  suite\$absent' "$scratch/lines.stderr" || { self_test_failed "$LINENO"; return 1; }
        ! grep -qx '  suite\$failure' "$scratch/lines.stderr" || { self_test_failed "$LINENO"; return 1; }
    done
    # The case filter narrows the node's own filtersets, which nextest would
    # OR with it: every spelling before `--` merges into one AND. Option
    # values (a value is never read as a filterset) and everything after
    # `--` stay as given; -rE is refused by name.
    local -a merge_out=() merge_expected=()
    merge_case_filter merge_out '(C)' --profile ci -E 'test(a)' -p -E \
        '--filterset=kind(lib)' --filter-expr 'test(b)' '-E=test(c)' '-Etest(d)' -j 1 \
        -- -E 'test(z)' --skip x || { self_test_failed "$LINENO"; return 1; }
    merge_expected=(--profile ci -p -E -j 1 --filter-expr
        '((test(a)) | (kind(lib)) | (test(b)) | (test(c)) | (test(d))) & ((C))'
        -- -E 'test(z)' --skip x)
    [[ $(printf '%s\n' "${merge_out[@]}") == "$(printf '%s\n' "${merge_expected[@]}")" ]] || { self_test_failed "$LINENO"; return 1; }
    merge_case_filter merge_out '(C)' -p suite || { self_test_failed "$LINENO"; return 1; }
    [[ $(printf '%s\n' "${merge_out[@]}") == $'-p\nsuite\n--filter-expr\n(C)' ]] || { self_test_failed "$LINENO"; return 1; }
    status=0
    merge_case_filter merge_out '(C)' -p suite -rE 'test(a)' 2>"$scratch/merge.stderr" || status=$?
    [[ $status == 2 ]] || { self_test_failed "$LINENO"; return 1; }
    grep -qx 'run-nextest-counted: cannot merge the case filter with -rE: pass -E on its own' \
        "$scratch/merge.stderr" || { self_test_failed "$LINENO"; return 1; }
    status=0
    merge_case_filter merge_out '(C)' -p suite -E 2>/dev/null || status=$?
    [[ $status == 2 ]] || { self_test_failed "$LINENO"; return 1; }
    # End to end, the listing receives the one merged filter.
    mkdir "$scratch/merged-attempts"
    status=0
    HERMIT_NEXTEST_FILTER_EXPR='test(=failure)' HERMIT_NEXTEST_SELECTED_CASES='suite$failure' \
        TMPDIR="$scratch" DAGRUN_TEST_COUNTS_PATH="$scratch/merged-counts.json" \
        run_nextest "$scratch/merged-events" "$wall_multiplier" "$cpu_wrapper" \
        "$scratch/merged-attempts" "$scratch/merged-binary-map.json" \
        "$scratch/merged-inventory.json" "$scratch/merged-cpu.json" \
        --profile ci -p suite -E 'kind(lib)' -j 1 --retries 2 -- --skip skipme \
        >/dev/null 2>"$scratch/merged.stderr" || status=$?
    [[ $status == 100 ]] || { self_test_failed "$LINENO"; return 1; }
    [[ $(<"$scratch/list-arguments") == "$(printf '%s\n' --locked --color never \
        --message-format json --profile ci -p suite --filter-expr \
        '((kind(lib))) & (test(=failure))' -- --skip skipme)" ]] || { self_test_failed "$LINENO"; return 1; }
    # Budget re-resolution: the first prepared run refuses on a moved
    # context (before any test executes), the counted runner re-resolves
    # once and the run proceeds. The captured run stderr must sit beside
    # the attempt-record directory, never inside it: the typed-results
    # reader parses every file there as an attempt record, and one
    # plain-text file refuses the whole report.
    eval "real_run_rust_script() $(declare -f run_rust_script | sed -n '/^{/,$p')"
    function run_rust_script {
        local source=$1
        shift
        if [[ $source == *nextest-binaries.rs ]]; then
            case $1 in
                budget-context)
                    printf '%s\n' '{"source_sha256":"0","source_clean":true,"build_sha256":"0","machine":"x86_64:self-test","available_cpus":1}'
                    ;;
                list)
                    jq -nc --arg executable "$fixture_binary" '{
                        "rust-suites": {
                            "suite": {
                                "package-name": "suite",
                                "binary-id": "suite",
                                "binary-name": "suite",
                                "kind": "lib",
                                "binary-path": $executable,
                                "testcases": {
                                    "failure": {"ignored":false,"filter-match":{"status":"matches"}}
                                }
                            }
                        }
                    }'
                    ;;
                run)
                    if [[ ! -e $scratch/reresolve-refused ]]; then
                        : >"$scratch/reresolve-refused"
                        printf '%s\n' 'prepared-nextest: Nextest source/build/launch context changed after budget resolution (available_cpus); resolve again before running' >&2
                        return 2
                    fi
                    cp -- "$scratch/wrapper-attempts"/*.json "$HERMIT_NEXTEST_CPU_RECORD_DIR/"
                    printf '%s\n' \
                        '{"type":"suite","event":"started","test_count":1,"nextest":{"crate":"suite","test_binary":"suite","kind":"lib"}}' \
                        '{"type":"test","event":"failed","name":"suite::suite$failure","exec_time":0.01}' \
                        '{"type":"suite","event":"failed","passed":0,"failed":1,"ignored":0,"measured":0,"filtered_out":0,"exec_time":0.01,"nextest":{"crate":"suite","test_binary":"suite","kind":"lib"}}'
                    return 100
                    ;;
                *) return 99 ;;
            esac
            return
        fi
        "$source" "$@"
    }
    attempts="$scratch/reresolve-attempts"
    mkdir "$attempts"
    status=0
    got=$(TMPDIR="$scratch" DAGRUN_TEST_COUNTS_PATH="$scratch/reresolve-counts.json" \
        HERMIT_PREPARED_NEXTEST_REQUIRED=1 \
        run_nextest "$scratch/reresolve-events" "$wall_multiplier" "$cpu_wrapper" \
        "$attempts" "$binary_map" "$inventory" "$scratch/reresolve-cpu.json" \
        --profile ci --workspace --exclude hermit-detcore --exclude hermit-detcore-tests \
        --exclude hermit-run --exclude hermetic_infra_hermit_flaky-tests \
        -j 1 --retries 2 \
        2>"$scratch/reresolve.stderr") || status=$?
    eval "run_rust_script() $(declare -f real_run_rust_script | sed -n '/^{/,$p')"
    unset -f real_run_rust_script
    [[ $status == 100 ]] || { self_test_failed "$LINENO"; return 1; }
    grep -q 're-resolving once (attempt 2 of 2)' "$scratch/reresolve.stderr" || { self_test_failed "$LINENO"; return 1; }
    [[ -e $scratch/budget-run-stderr.txt && ! -e $attempts/budget-run-stderr.txt ]] || { self_test_failed "$LINENO"; return 1; }
    # Only attempt records in the record directory, each one JSON the
    # typed-results reader would accept as a record.
    local reresolve_entry
    for reresolve_entry in "$attempts"/*; do
        [[ -f $reresolve_entry ]] || { self_test_failed "$LINENO"; return 1; }
        python3 -c 'import json,sys; record=json.load(open(sys.argv[1])); assert record["identity"] and record["completion"]' \
            "$reresolve_entry" || { self_test_failed "$LINENO"; return 1; }
    done
    [[ $(<"$scratch/reresolve-counts.json") == \
        '{"executed_tests":1,"filtered_tests":0,"results":[{"attempts":1,"id":"suite$failure","result":"fail"}],"schema":2}' ]] || { self_test_failed "$LINENO"; return 1; }
    attempts="$scratch/wrapper-attempts"
    grep -q 'period = "270s"' "$scratch/scaled-nextest.toml" || { self_test_failed "$LINENO"; return 1; }
    [[ $(grep -c '^run-wrapper = "hermit-per-test-cpu"$' \
        "$scratch/scaled-nextest.toml") == 1 ]] || { self_test_failed "$LINENO"; return 1; }
    grep -q '^experimental = \["wrapper-scripts"\]$' \
        "$scratch/scaled-nextest.toml" || { self_test_failed "$LINENO"; return 1; }
    grep -q '^target-runner = "within-wrapper"$' \
        "$scratch/scaled-nextest.toml" || { self_test_failed "$LINENO"; return 1; }
    # This is the command Nextest receives, not the fixture's direct wrapper call.
    grep -Fq '"--cpu-timeout-usec", "22000000", "--termination-grace-ms", "2000", "--"' \
        "$scratch/scaled-nextest.toml" || { self_test_failed "$LINENO"; return 1; }
    if compgen -G "$scratch/hermit-nextest-config.*.toml" >/dev/null; then
        printf 'run-nextest-counted: temporary nextest config leaked\n' >&2
        return 1
    fi

    # Synthetic mutation: the map and counts still agree, but inventory names
    # do not. This must reach the result writer through run_nextest and refuse;
    # omitting --inventory would wrongly publish the ordinary failed result.
    inventory_wrong_name=1
    mkdir "$scratch/attribution-attempts"
    status=0
    TMPDIR="$scratch" DAGRUN_TEST_COUNTS_PATH="$scratch/attribution-counts.json" \
        run_nextest "$scratch/attribution-events" "$wall_multiplier" "$cpu_wrapper" \
        "$scratch/attribution-attempts" "$binary_map" "$inventory" "$scratch/attribution-cpu.json" \
        --profile ci -p suite -j 1 --retries 2 -- --skip skipme \
        >"$scratch/attribution.stdout" 2>"$scratch/attribution.stderr" || status=$?
    [[ $status == 100 ]] || { self_test_failed "$LINENO"; return 1; }
    [[ ! -e $scratch/attribution-counts.json && ! -e $scratch/attribution-cpu.json ]] || { self_test_failed "$LINENO"; return 1; }
    grep -q '0 eligible matches' "$scratch/attribution.stderr" || { self_test_failed "$LINENO"; return 1; }
    inventory_wrong_name=0

    # Preserve the original zero-terminal launch failure separately from the
    # executed exit23 control above, end to end through run_nextest's failure
    # branch. Its listing names the one test, as a real launch failure's
    # does; its stream ends with no terminal test event, and stderr's twelve
    # exec failures are not test rows: the one listed test never ran, so it
    # counts as not executed (filtered_tests 1), never as a row. This caller
    # passes its own --locked, which the list below must not repeat.
    launch_failure=1
    ordinary_attempts=$attempts
    attempts="$scratch/launch-attempts"
    mkdir "$attempts"
    status=0
    got=$(TMPDIR="$scratch" DAGRUN_TEST_COUNTS_PATH="$scratch/launch-counts.json" \
        run_nextest "$scratch/launch-events" "$wall_multiplier" "$cpu_wrapper" \
        "$attempts" "$binary_map" "$inventory" "$scratch/launch-cpu.json" \
        --profile ci --locked -p suite -j 1 --retries 2 -- --skip skipme) || status=$?
    [[ $status == 100 ]] || { self_test_failed "$LINENO"; return 1; }
    [[ $got == *$'running 0 tests\ntest result: FAILED. 0 passed; 0 failed; 0 ignored; 1 filtered out' ]] || { self_test_failed "$LINENO"; return 1; }
    [[ $got != *'test result: ok.'* ]] || { self_test_failed "$LINENO"; return 1; }
    [[ $(<"$scratch/launch-counts.json") == \
        '{"executed_tests":0,"filtered_tests":1,"results":[],"schema":2}' ]] || { self_test_failed "$LINENO"; return 1; }
    jq -e '. == {schema:3, run_id:null, attempts:[]}' "$scratch/launch-cpu.json" >/dev/null || { self_test_failed "$LINENO"; return 1; }
    cmp -s "$scratch/expected-list-arguments" "$scratch/list-arguments" || { self_test_failed "$LINENO"; return 1; }
    if compgen -G "$scratch/hermit-nextest-config.*.toml" >/dev/null; then self_test_failed "$LINENO"; return 1; fi
    launch_failure=0

    # A listing that names no test: the census gate refuses it as zero-tests
    # before the run, so nothing runs and nothing publishes.
    empty_listing=1
    mkdir "$scratch/empty-attempts"
    status=0
    TMPDIR="$scratch" DAGRUN_TEST_COUNTS_PATH="$scratch/empty-counts.json" \
        run_nextest "$scratch/empty-events" "$wall_multiplier" "$cpu_wrapper" \
        "$scratch/empty-attempts" "$scratch/empty-binary-map.json" \
        "$scratch/empty-inventory.json" "$scratch/empty-cpu.json" \
        --profile ci --locked -p suite -j 1 --retries 2 -- --skip skipme \
        >/dev/null 2>"$scratch/empty.stderr" || status=$?
    [[ $status == 1 ]] || { self_test_failed "$LINENO"; return 1; }
    grep -q 'census-check: zero-tests' "$scratch/empty.stderr" || { self_test_failed "$LINENO"; return 1; }
    [[ ! -e $scratch/empty-events && ! -e $scratch/empty-counts.json && ! -e $scratch/empty-cpu.json ]] || { self_test_failed "$LINENO"; return 1; }
    cmp -s "$scratch/expected-list-arguments" "$scratch/list-arguments" || { self_test_failed "$LINENO"; return 1; }
    if compgen -G "$scratch/hermit-nextest-config.*.toml" >/dev/null; then self_test_failed "$LINENO"; return 1; fi
    empty_listing=0

    status=0
    DAGRUN_TEST_COUNTS_PATH="$scratch/launch-wrong-count.json" NEXTEST_EXPECTED_EXECUTED=1 \
        emit_libtest_count "$scratch/launch-events" 100 "$attempts" "$binary_map" \
        "$scratch/launch-wrong-cpu.json" "$inventory" >"$scratch/launch-wrong.stdout" 2>"$scratch/launch-wrong.stderr" || status=$?
    [[ $status == 2 ]] || { self_test_failed "$LINENO"; return 1; }
    grep -q 'expected 1 tests to execute, saw 0' "$scratch/launch-wrong.stderr" || { self_test_failed "$LINENO"; return 1; }
    # The refusal must carry the outcome it already knows. Withholding the typed
    # report is deliberate and asserted on the next line, so this message is the
    # only place the run can say whether the tests it DID run passed. Without it
    # an all-green population drift and a failing one read identically.
    grep -q 'of which 0 passed and 0 failed' "$scratch/launch-wrong.stderr" || { self_test_failed "$LINENO"; return 1; }
    [[ ! -e $scratch/launch-wrong-count.json && ! -e $scratch/launch-wrong-cpu.json ]] || { self_test_failed "$LINENO"; return 1; }
    status=0
    DAGRUN_TEST_COUNTS_PATH="$scratch/failed-wrong-count.json" NEXTEST_EXPECTED_EXECUTED=2 \
        emit_libtest_count "$scratch/wrapper-events" 100 "$ordinary_attempts" "$binary_map" \
        "$scratch/failed-wrong-cpu.json" "$ordinary_inventory" >"$scratch/failed-wrong.stdout" 2>"$scratch/failed-wrong.stderr" || status=$?
    [[ $status == 2 ]] || { self_test_failed "$LINENO"; return 1; }
    grep -q 'expected 2 tests to execute, saw 1' "$scratch/failed-wrong.stderr" || { self_test_failed "$LINENO"; return 1; }
    grep -q 'of which 0 passed and 1 failed' "$scratch/failed-wrong.stderr" || { self_test_failed "$LINENO"; return 1; }
    [[ ! -e $scratch/failed-wrong-count.json && ! -e $scratch/failed-wrong-cpu.json ]] || { self_test_failed "$LINENO"; return 1; }
    status=0
    DAGRUN_TEST_COUNTS_PATH="$scratch/launch-stray-count.json" \
        emit_libtest_count "$scratch/launch-events" 100 "$ordinary_attempts" "$binary_map" \
        "$scratch/launch-stray-cpu.json" "$inventory" >"$scratch/launch-stray.stdout" 2>"$scratch/launch-stray.stderr" || status=$?
    [[ $status == 2 ]] || { self_test_failed "$LINENO"; return 1; }
    grep -q 'unexpected=' "$scratch/launch-stray.stderr" || { self_test_failed "$LINENO"; return 1; }
    [[ ! -e $scratch/launch-stray-count.json && ! -e $scratch/launch-stray-cpu.json ]] || { self_test_failed "$LINENO"; return 1; }

    unset -f cargo

    printf '%s\n' \
        '{"type":"test","event":"future","name":"suite::suite$case"}' \
        >"$scratch/unknown-event.jsonl"
    status=0
    DAGRUN_TEST_COUNTS_PATH="$scratch/refused.json" \
        emit_libtest_count "$scratch/unknown-event.jsonl" 0 \
        >/dev/null 2>"$scratch/refusal" || status=$?
    [[ $status == 2 ]] || { self_test_failed "$LINENO"; return 1; }
    grep -q 'unsupported test event "future"' "$scratch/refusal" || { self_test_failed "$LINENO"; return 1; }
    [[ ! -e $scratch/refused.json ]] || { self_test_failed "$LINENO"; return 1; }

    retain_events "$scratch/events.jsonl" - || { self_test_failed "$LINENO"; return 1; }
    [[ ! -e $scratch/kept-events.jsonl ]] || { self_test_failed "$LINENO"; return 1; }
    retain_events "$scratch/events.jsonl" "$scratch/kept-events.jsonl" || { self_test_failed "$LINENO"; return 1; }
    cmp -s "$scratch/events.jsonl" "$scratch/kept-events.jsonl" || { self_test_failed "$LINENO"; return 1; }
    status=0
    retain_events "$scratch/failed-events.jsonl" "$scratch/kept-events.jsonl" \
        2>"$scratch/retain-refusal" || status=$?
    [[ $status == 2 ]] || { self_test_failed "$LINENO"; return 1; }
    grep -q 'cannot keep the typed event stream' "$scratch/retain-refusal" || { self_test_failed "$LINENO"; return 1; }
    cmp -s "$scratch/events.jsonl" "$scratch/kept-events.jsonl" || { self_test_failed "$LINENO"; return 1; }
    [[ $(HERMIT_NEXTEST_EVENTS_PATH="$scratch/asked" configured_events_path) == "$scratch/asked" ]] || { self_test_failed "$LINENO"; return 1; }
    [[ $(unset HERMIT_NEXTEST_EVENTS_PATH E2E_RESULT_ROOT; configured_events_path) == - ]] || { self_test_failed "$LINENO"; return 1; }
    status=0
    HERMIT_NEXTEST_EVENTS_PATH='' configured_events_path >/dev/null 2>&1 || status=$?
    [[ $status == 2 ]] || { self_test_failed "$LINENO"; return 1; }
    # A counted step of a validation keeps its stream under the durable result root, named by
    # its dagrun step tag, one file per attempt; an explicit path still wins and makes no
    # directory; without a result root or a counted step (a counts path of `-` included)
    # nothing is kept.
    local kept_root=$scratch/result-root counts=$scratch/counts/run-1:test.detcore_unit.json got
    [[ $(E2E_RESULT_ROOT=$kept_root DAGRUN_TEST_COUNTS_PATH=$counts HERMIT_NEXTEST_EVENTS_PATH="$scratch/asked" \
        configured_events_path) == "$scratch/asked" ]] || { self_test_failed "$LINENO"; return 1; }
    [[ ! -e $kept_root ]] || { self_test_failed "$LINENO"; return 1; }
    [[ $(unset HERMIT_NEXTEST_EVENTS_PATH; E2E_RESULT_ROOT=$kept_root DAGRUN_TEST_COUNTS_PATH=- \
        configured_events_path) == - ]] || { self_test_failed "$LINENO"; return 1; }
    [[ ! -e $kept_root ]] || { self_test_failed "$LINENO"; return 1; }
    [[ $(unset HERMIT_NEXTEST_EVENTS_PATH; E2E_RESULT_ROOT=$kept_root DAGRUN_TEST_COUNTS_PATH=$counts \
        DAGRUN_STEP_STARTED_MONOTONIC_NS=41 configured_events_path) == "$kept_root/nextest-events/test.detcore_unit.41.jsonl" ]] || { self_test_failed "$LINENO"; return 1; }
    [[ -d $kept_root/nextest-events ]] || { self_test_failed "$LINENO"; return 1; }
    [[ $(unset HERMIT_NEXTEST_EVENTS_PATH E2E_RESULT_ROOT; DAGRUN_TEST_COUNTS_PATH=$counts configured_events_path) == - ]] || { self_test_failed "$LINENO"; return 1; }
    [[ $(unset HERMIT_NEXTEST_EVENTS_PATH DAGRUN_TEST_COUNTS_PATH; E2E_RESULT_ROOT=$kept_root configured_events_path) == - ]] || { self_test_failed "$LINENO"; return 1; }
    got=$(unset HERMIT_NEXTEST_EVENTS_PATH; E2E_RESULT_ROOT=$kept_root DAGRUN_TEST_COUNTS_PATH=$counts \
        DAGRUN_STEP_STARTED_MONOTONIC_NS=not-a-number configured_events_path)
    [[ $got == "$kept_root/nextest-events/"* && ${got#"$kept_root/nextest-events/"} =~ ^test\.detcore_unit\.[0-9]+-[0-9]+\.jsonl$ ]] || { self_test_failed "$LINENO"; return 1; }
    # Two attempts of one step keep two streams; neither is refused as an existing file.
    local attempt
    for attempt in 41 42; do
        retain_events "$scratch/events.jsonl" "$(unset HERMIT_NEXTEST_EVENTS_PATH; E2E_RESULT_ROOT=$kept_root \
            DAGRUN_TEST_COUNTS_PATH=$counts DAGRUN_STEP_STARTED_MONOTONIC_NS=$attempt configured_events_path)" || { self_test_failed "$LINENO"; return 1; }
    done
    cmp -s "$scratch/events.jsonl" "$kept_root/nextest-events/test.detcore_unit.41.jsonl" || { self_test_failed "$LINENO"; return 1; }
    cmp -s "$scratch/events.jsonl" "$kept_root/nextest-events/test.detcore_unit.42.jsonl" || { self_test_failed "$LINENO"; return 1; }
    # A result root where the directory cannot be made refuses (exit 2), never keeps nothing.
    : >"$scratch/not-a-directory"
    status=0
    (unset HERMIT_NEXTEST_EVENTS_PATH; E2E_RESULT_ROOT=$scratch/not-a-directory DAGRUN_TEST_COUNTS_PATH=$counts \
        configured_events_path) >/dev/null 2>"$scratch/events-dir-refusal" || status=$?
    [[ $status == 2 ]] || { self_test_failed "$LINENO"; return 1; }
    grep -q 'cannot create .* to keep the typed event stream' "$scratch/events-dir-refusal" || { self_test_failed "$LINENO"; return 1; }
    # Beside a default-kept stream: the attempt's start marker (one per attempt, never replaced)
    # and a copy of the typed results the writer published; nothing for a caller-chosen path.
    local prefix=$kept_root/nextest-events/test.detcore_unit.43
    [[ $(unset HERMIT_NEXTEST_EVENTS_PATH; configured_results_prefix "$prefix.jsonl") == "$prefix" ]] || { self_test_failed "$LINENO"; return 1; }
    [[ $(unset HERMIT_NEXTEST_EVENTS_PATH; configured_results_prefix -) == - ]] || { self_test_failed "$LINENO"; return 1; }
    [[ $(HERMIT_NEXTEST_EVENTS_PATH="$scratch/asked" configured_results_prefix "$scratch/asked") == - ]] || { self_test_failed "$LINENO"; return 1; }
    mark_attempt_started - || { self_test_failed "$LINENO"; return 1; }
    mark_attempt_started "$prefix" || { self_test_failed "$LINENO"; return 1; }
    [[ -f $prefix.started && ! -s $prefix.started ]] || { self_test_failed "$LINENO"; return 1; }
    status=0
    mark_attempt_started "$prefix" 2>"$scratch/marker-refusal" || status=$?
    [[ $status == 2 ]] || { self_test_failed "$LINENO"; return 1; }
    grep -q 'cannot write the start marker' "$scratch/marker-refusal" || { self_test_failed "$LINENO"; return 1; }
    retain_results "$scratch/no-such-results.json" "$prefix" || { self_test_failed "$LINENO"; return 1; }
    [[ ! -e $prefix.results.json ]] || { self_test_failed "$LINENO"; return 1; }
    retain_results - "$prefix" || { self_test_failed "$LINENO"; return 1; }
    printf '%s' '{"executed_tests":0,"filtered_tests":0,"results":[],"schema":5}' >"$scratch/results.json"
    retain_results "$scratch/results.json" - || { self_test_failed "$LINENO"; return 1; }
    [[ ! -e $prefix.results.json ]] || { self_test_failed "$LINENO"; return 1; }
    retain_results "$scratch/results.json" "$prefix" || { self_test_failed "$LINENO"; return 1; }
    cmp -s "$scratch/results.json" "$prefix.results.json" || { self_test_failed "$LINENO"; return 1; }
    status=0
    retain_results "$scratch/results.json" "$prefix" 2>"$scratch/results-refusal" || status=$?
    [[ $status == 2 ]] || { self_test_failed "$LINENO"; return 1; }
    grep -q 'cannot keep the typed test results' "$scratch/results-refusal" || { self_test_failed "$LINENO"; return 1; }
    # A failed keep fails only a passing run: a census no-result (75) or a test failure stands.
    [[ $(kept_status 0 0) == 0 && $(kept_status 0 2) == 2 ]] || { self_test_failed "$LINENO"; return 1; }
    [[ $(kept_status 75 2) == 75 && $(kept_status 100 2) == 100 && $(kept_status 1 0) == 1 ]] || { self_test_failed "$LINENO"; return 1; }

    printf 'run-nextest-counted: self-test PASS (typed counts, executed failure, zero-terminal launch failure, zero-tests census refusal, case selection (named cases checked against the listing over all whitespace, the case filter ANDed with the node filtersets), CPU reconciliation, refusal controls, event retention, typed-result retention, skip classification, skid-infra classification)\n'
}

if [[ ${1:-} == --help || ${1:-} == -h ]]; then
    printf '%s\n' 'usage: ci/run-nextest-counted.sh [--calibration-host | --calibration-launch-proof PATH] NEXTEST_ARGS...' \
        'The maintained native host boundary may capture actual resources with --calibration-host.' \
        'Pinned callers use the existing wrapper-provided read-only launch proof.' \
        'Missing evidence keeps defaults; malformed or changed evidence refuses. Capture never compiles.'
    exit 0
fi
if [[ ${1:-} == --calibration-host ]]; then
    calibration_host=true
    shift
elif [[ ${1:-} == --calibration-launch-proof ]]; then
    [[ $# -ge 2 && $2 == /* ]] || { printf 'run-nextest-counted: --calibration-launch-proof requires an absolute path; use --help\n' >&2; exit 2; }
    calibration_launch=$2
    shift 2
fi
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
events_destination=$(configured_events_path) || exit $?
if [[ $events_destination != - && ( $events_destination == "${DAGRUN_TEST_COUNTS_PATH:--}" || $events_destination == "$cpu_report" ) ]]; then
    printf 'run-nextest-counted: HERMIT_NEXTEST_EVENTS_PATH must differ from DAGRUN_TEST_COUNTS_PATH and the CPU report path\n' >&2
    exit 2
fi
if "$calibration_host"; then
    capture_manifest=${HERMIT_RUST_SCRIPT_ARTIFACT_ROOT:-$SCRIPT_DIR/../target/ci/rust-scripts}/manifest.tsv
    if [[ -e $capture_manifest || -L $capture_manifest ]]; then
        resolve_status=0
        capture_binary=$("$PREBUILT_RUST_SCRIPT" --resolve-optional --force "$SCRIPT_DIR/nextest-binaries.rs") || resolve_status=$?
        if ((resolve_status != 0 && resolve_status != 3)); then exit "$resolve_status"; fi
        capture_status=2
        if ((resolve_status == 0)); then
            capture_status=0
            capture_version=$("$capture_binary" capture-launch --probe) || capture_status=$?
        fi
        case $capture_status in
            0)
                [[ $capture_version == nextest-launch-observation-v2 ]] || { printf 'nextest budgets: malformed prepared capture capability; refresh build.rust_scripts\n' >&2; exit 2; }
                calibration_launch="$cpu_measurement_dir/launch.json"
                "$capture_binary" capture-launch --host --output "$calibration_launch" || exit $?
                ;;
            2) printf 'nextest budgets: prepared capture capability unavailable; unchanged defaults\n' >&2 ;;
            *) exit "$capture_status" ;;
        esac
    else
        printf 'nextest budgets: prepared host capture unavailable; unchanged defaults\n' >&2
    fi
fi
if [[ -n $calibration_launch && ( -e $calibration_launch || -L $calibration_launch ) && $cpu_report != - ]]; then
    [[ -f $calibration_launch && ! -L $calibration_launch ]] || { printf 'nextest budgets: launch proof must be a regular non-symlink file\n' >&2; exit 2; }
    (set -o noclobber; head -c 262145 -- "$calibration_launch" >"$cpu_report.launch.json") || exit $?
    # Preserve the wrapper mount as authority; this copy only retains evidence.
fi
# Hermit-running tests retry a typed PMU skid-overshoot refusal and append
# one line per retry here (hermit-cli/tests/common/skid_overshoot.rs). Report
# the count even when every test passed, so a rising skid rate is visible in
# the validate log rather than hidden by the retries.
skid_retry_record="$cpu_measurement_dir/skid-retries.log"
: >"$skid_retry_record" || exit $?
export HERMIT_SKID_RETRY_RECORD=$skid_retry_record
# One RetryRecordV1 JSON line per retried test (ledger retry_rollup schema).
export HERMIT_RETRY_RECORD_FILE="$cpu_measurement_dir/retry-records.jsonl"
: >"$HERMIT_RETRY_RECORD_FILE" || exit $?
# A test whose whole skid budget ended in typed skid-overshoot refusals
# appends one HERMIT-SKID-INFRA line here; the result writer classifies that
# test's terminal attempt as an infrastructure error (verdict SKID-INFRA,
# not a test failure) and this count keeps it visible in the log. Wired
# only for a schema-5 node, like the skip sidecar: the writer refuses a
# marker it cannot classify, so an unwired helper must not write one.
HERMIT_SKID_INFRA_FILE="$cpu_measurement_dir/skid-infra.log"
: >"$HERMIT_SKID_INFRA_FILE" || exit $?
if [[ ${HERMIT_NEXTEST_RESULTS_SCHEMA:-} == 5 ]]; then
    export HERMIT_SKID_INFRA_FILE
fi
results_prefix=$(configured_results_prefix "$events_destination")
keep_status=0
mark_attempt_started "$results_prefix" || keep_status=2
nextest_status=0
run_nextest "$events_log" "$(configured_wall_timeout_multiplier)" "$cpu_wrapper" \
    "$cpu_attempt_records" "$cpu_binary_map" "$cpu_inventory" "$cpu_report" "$@" \
    || nextest_status=$?
# A record that cannot be kept fails a run that would otherwise pass (2). It never replaces a
# status that already says something else: a census no-result (75), which validate requeues,
# or a test failure must stay what it is.
retain_events "$events_log" "$events_destination" || keep_status=2
retain_results "${DAGRUN_TEST_COUNTS_PATH:--}" "$results_prefix" || keep_status=2
nextest_status=$(kept_status "$nextest_status" "$keep_status")
skid_retries=$(wc -l <"$skid_retry_record") || exit $?
printf 'run-nextest-counted: skid-overshoot retries: %d\n' "$skid_retries" >&2
retry_records=$(wc -l <"$HERMIT_RETRY_RECORD_FILE") || exit $?
printf 'run-nextest-counted: retry records: %d (in %s)\n' "$retry_records" "$HERMIT_RETRY_RECORD_FILE" >&2
if ((retry_records > 0)); then
    # The measurement dir is removed on exit; keep the records in the log.
    sed 's/^/run-nextest-counted: retry record: /' "$HERMIT_RETRY_RECORD_FILE" >&2
fi
if ((skid_retries > 0)); then
    sed 's/^/run-nextest-counted: skid-overshoot retry: /' "$skid_retry_record" >&2
fi
skid_infra=$(wc -l <"$HERMIT_SKID_INFRA_FILE") || exit $?
printf 'run-nextest-counted: SKID-INFRA count %d\n' "$skid_infra" >&2
if ((skid_infra > 0)); then
    sed 's/^HERMIT-SKID-INFRA: /run-nextest-counted: SKID-INFRA: /' "$HERMIT_SKID_INFRA_FILE" >&2
fi
exit "$nextest_status"
