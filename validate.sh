#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -uo pipefail

# Deny warnings for every compiler and rustdoc invocation while preserving any
# caller-provided flags.
export RUSTFLAGS="${RUSTFLAGS:+${RUSTFLAGS} }-D warnings"
export RUSTDOCFLAGS="${RUSTDOCFLAGS:+${RUSTDOCFLAGS} }-D warnings"
ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"

readonly ROOT_DIR
cd "$ROOT_DIR" || exit 1

# --- Argument parsing -------------------------------------------------------
# Default (no args): run the full validation suite, which also prints the
# working-envelope vector at the end. The envelope path is factored out so CI
# can call the *identical* measurement code and produce matching numbers:
#   ./validate.sh --envelope-only            # measure + emit vector (JSON+human)
#   ./validate.sh --envelope-compare FILE    # measure, then fail if any count
#                                            # regressed below FILE's baseline
#   ./validate.sh --label-pr                 # on a fully-green full run, tag the
#                                            # current branch's PR
#                                            # 'locally-validated' (opt-in; env
#                                            # VALIDATE_LABEL_PR=1 is equivalent,
#                                            # PR_NUMBER=N overrides detection)
ENVELOPE_MODE="full"          # full | only
ENVELOPE_BASELINE=""
# Opt-in PR auto-labeling. Off unless --label-pr or VALIDATE_LABEL_PR is set;
# never mutate a live PR as a silent side effect of a routine local run.
LABEL_PR=0
[[ -n ${VALIDATE_LABEL_PR:-} ]] && LABEL_PR=1
PR_NUMBER=${PR_NUMBER:-}
while [[ $# -gt 0 ]]; do
    case "$1" in
        --envelope-only) ENVELOPE_MODE="only"; shift ;;
        --envelope-compare)
            ENVELOPE_MODE="only"; ENVELOPE_BASELINE=${2:-}
            [[ -n $ENVELOPE_BASELINE ]] || { echo "validate.sh: --envelope-compare needs a FILE" >&2; exit 2; }
            shift 2 ;;
        --label-pr) LABEL_PR=1; shift ;;
        -h|--help)
            grep -E '^#( |$)' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "validate.sh: unknown argument: $1 (try --help)" >&2; exit 2 ;;
    esac
done

checks=0
failures=0
declare -a background_pids=()
declare -a background_names=()
declare -a background_logs=()
declare -a background_duration_files=()

VALIDATION_TMP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/hermit-validate.XXXXXX")
if [[ -z $VALIDATION_TMP_DIR ]]; then
    echo "Unable to create validation workspace." >&2
    exit 1
fi
readonly VALIDATION_TMP_DIR

LOG_FILE=$(mktemp "${TMPDIR:-/tmp}/hermit-validate.XXXXXX.log")
if [[ -z $LOG_FILE ]]; then
    echo "Unable to create validation log." >&2
    exit 1
fi
readonly LOG_FILE
printf "Hermit validation log\nRoot: %s\n\n" "$ROOT_DIR" >"$LOG_FILE"

readonly NEXTEST_VERSION=0.9.100
NEXTEST_PROFILE_NAME=${NEXTEST_PROFILE:-}
if [[ -z $NEXTEST_PROFILE_NAME && -n ${CI:-} ]]; then
    NEXTEST_PROFILE_NAME=ci
fi
declare -a NEXTEST_RUN=(cargo nextest run)
if [[ -n $NEXTEST_PROFILE_NAME ]]; then
    NEXTEST_RUN+=(--profile "$NEXTEST_PROFILE_NAME")
fi
readonly NEXTEST_PROFILE_NAME NEXTEST_RUN

readonly HERMIT_BIN="$ROOT_DIR/target/debug/hermit"
readonly HERMIT_SMOKE_TIMEOUT="30s"
readonly SMOKE_MARKER="hermit-validation-smoke"
declare -ar HERMIT_RUN_ARGS=(
    run
    --base-env=minimal
    --no-virtualize-cpuid
    --preemption-timeout=disabled
)

# --- Working-envelope measurement -------------------------------------------
# The "working envelope" is the set of end-to-end guest scenarios that Hermit
# runs deterministically, counted at each assurance level (see AGENTS.md):
#   L1 = hermit run --strict                                   (deterministic)
#   L2 = hermit run --strict --verify                          (bitwise-identical)
#   L3 = hermit run --strict --verify --detlog-heap --detlog-stack (memory det.)
#   L4 = L2 repeated $L4_REPS times with no divergence         (stress-hardened)
#   rr = hermit record start --verify ...                      (record/replay e2e)
# The vector {l1_pass,l2_pass,l3_pass,l4_pass,rr_pass,total} must increase
# monotonically main -> PR -> frontier; --envelope-compare enforces that.
#
# ENVELOPE_PROBES is the shared, extensible e2e probe list. Each entry is
# "label|command-with-space-separated-args". Add new guest scenarios here; CI
# and validate.sh both measure this exact list via the same code path.
declare -ar ENVELOPE_PROBES=(
    "true|/bin/true"
    "echo|/bin/echo hermit-envelope"
    "date|/bin/date -u +%Y"
)
readonly L4_REPS=${L4_REPS:-20}
ENVELOPE_JSON=${ENVELOPE_JSON:-"$ROOT_DIR/envelope.json"}
ENVELOPE_LAST_JSON=""

function cleanup {
    local pid
    for pid in "${background_pids[@]}"; do
        kill "$pid" 2>/dev/null || true
    done
    wait 2>/dev/null || true
    rm -rf "$VALIDATION_TMP_DIR"
}

function interrupted {
    trap - INT TERM
    printf "❌ Validation interrupted (full log: %s)\n" "$LOG_FILE"
    exit 130
}
trap cleanup EXIT
trap interrupted INT TERM

function failure_summary {
    local output_start=$1
    local output
    local summary

    output=$(
        tail -n "+$output_start" "$LOG_FILE" |
            sed $'s/\033\\[[0-9;]*[[:alpha:]]//g; s/^[[:space:]]*//; s/[[:space:]][[:space:]]*/ /g'
    )
    summary=$(
        printf "%s\n" "$output" |
            grep -E '(^error(\[[^]]+\])?:|^FAIL:|^test result: FAILED|^failures:|panicked at|Unexpected .*:|differed between|timed out|command not found|No such file)' |
            tail -n 1
    ) || true

    if [[ -z $summary ]]; then
        summary=$(printf "%s\n" "$output" | sed '/^[[:space:]]*$/d' | tail -n 1)
    fi
    if [[ -z $summary ]]; then
        summary="command exited without an error message"
    elif ((${#summary} > 180)); then
        summary="${summary:0:177}..."
    fi
    printf "%s" "$summary"
}

function run_check {
    local name=$1
    shift

    local started_at=$SECONDS
    local output_start
    local status
    local summary

    {
        printf "=== %s ===\n" "$name"
        printf "Command:"
        printf " %q" "$@"
        printf "\n"
    } >>"$LOG_FILE"
    output_start=$(($(wc -l <"$LOG_FILE") + 1))

    if "$@" >>"$LOG_FILE" 2>&1; then
        status=0
        printf "✅ %s (1 passed, 0 failed, %ss)\n" \
            "$name" "$((SECONDS - started_at))"
    else
        status=$?
        failures=$((failures + 1))
        summary=$(failure_summary "$output_start")
        printf "❌ %s (0 passed, 1 failed, exit %s: %s; full log: %s)\n" \
            "$name" "$status" "$summary" "$LOG_FILE"
    fi

    {
        printf "Exit: %s\n" "$status"
        printf "Duration: %ss\n\n" "$((SECONDS - started_at))"
    } >>"$LOG_FILE"
    checks=$((checks + 1))
}

function start_check {
    local name=$1
    shift

    local index=${#background_pids[@]}
    local log_file="$VALIDATION_TMP_DIR/check-$index.log"
    local duration_file="$VALIDATION_TMP_DIR/check-$index.duration"

    (
        local started_at=$SECONDS
        local status

        printf "Command:"
        printf " %q" "$@"
        printf "\n"
        "$@"
        status=$?
        printf "%s\n" "$((SECONDS - started_at))" >"$duration_file"
        exit "$status"
    ) >"$log_file" 2>&1 &

    background_pids+=("$!")
    background_names+=("$name")
    background_logs+=("$log_file")
    background_duration_files+=("$duration_file")
    checks=$((checks + 1))
}

function wait_for_background_checks {
    local i
    for i in "${!background_pids[@]}"; do
        local pid=${background_pids[$i]}
        local name=${background_names[$i]}
        local log_file=${background_logs[$i]}
        local duration_file=${background_duration_files[$i]}
        local output_start
        local status
        local duration
        local summary

        if wait "$pid"; then
            status=0
        else
            status=$?
            failures=$((failures + 1))
        fi

        printf "=== %s ===\n" "$name" >>"$LOG_FILE"
        output_start=$(($(wc -l <"$LOG_FILE") + 1))
        cat "$log_file" >>"$LOG_FILE"
        if [[ -r $duration_file ]]; then
            duration=$(<"$duration_file")
        else
            duration=0
        fi

        if ((status == 0)); then
            printf "✅ %s (1 passed, 0 failed, %ss)\n" "$name" "$duration"
        else
            summary=$(failure_summary "$output_start")
            printf "❌ %s (0 passed, 1 failed, exit %s: %s; full log: %s)\n" \
                "$name" "$status" "$summary" "$LOG_FILE"
        fi
        {
            printf "Exit: %s\n" "$status"
            printf "Duration: %ss\n\n" "$duration"
        } >>"$LOG_FILE"
    done

    background_pids=()
    background_names=()
    background_logs=()
    background_duration_files=()
}

function ensure_cargo_nextest {
    if cargo nextest show-config version >/dev/null 2>&1; then
        return 0
    fi

    local -ar install_command=(
        cargo install cargo-nextest --locked --version "$NEXTEST_VERSION"
    )
    if command -v with-proxy >/dev/null 2>&1; then
        with-proxy "${install_command[@]}"
    else
        "${install_command[@]}"
    fi

    cargo nextest show-config version
}
function hermit_echo {
    timeout "$HERMIT_SMOKE_TIMEOUT" \
        "$HERMIT_BIN" "${HERMIT_RUN_ARGS[@]}" -- \
        /bin/echo "$SMOKE_MARKER"
}

function hermit_run_smoke {
    local output
    local status

    output=$(hermit_echo)
    status=$?
    if ((status != 0)); then
        return "$status"
    fi

    if [[ "$output" != "$SMOKE_MARKER" ]]; then
        printf "Unexpected Hermit stdout: %q\n" "$output" >&2
        return 1
    fi
}

function hermit_determinism_check {
    local first_output
    local second_output
    local status

    first_output=$(hermit_echo)
    status=$?
    if ((status != 0)); then
        return "$status"
    fi

    second_output=$(hermit_echo)
    status=$?
    if ((status != 0)); then
        return "$status"
    fi

    if [[ "$first_output" != "$second_output" ]]; then
        echo "Hermit stdout differed between identical runs:" >&2
        diff -u \
            <(printf "%s\n" "$first_output") \
            <(printf "%s\n" "$second_output") >&2 || true
        return 1
    fi
}

function hermit_verify_smoke {
    timeout "$HERMIT_SMOKE_TIMEOUT" \
        "$HERMIT_BIN" "${HERMIT_RUN_ARGS[@]}" --verify -- \
        /bin/echo "$SMOKE_MARKER"
}

# Run one probe at one assurance level. $1 = extra run flags (space-split on
# purpose); remaining args are the guest argv. Returns the guest/hermit status.
function _envelope_level {
    local flags=$1
    shift
    # shellcheck disable=SC2086
    timeout "$HERMIT_SMOKE_TIMEOUT" \
        "$HERMIT_BIN" "${HERMIT_RUN_ARGS[@]}" $flags -- "$@" </dev/null >>"$LOG_FILE" 2>&1
}

# Measure the working envelope over ENVELOPE_PROBES, write JSON to
# $ENVELOPE_JSON, cache it in $ENVELOPE_LAST_JSON, and print a human summary.
# This is a measurement, not a gate: known failures (e.g. an unsupported
# syscall on this host) lower a count but never abort validation.
function run_envelope {
    local l1=0 l2=0 l3=0 l4=0 rr=0 total=0
    local probe label cmd i ok
    local -a cmdarr detail=()

    printf "=== Working-envelope measurement (L4 stress reps=%s) ===\n" "$L4_REPS" >>"$LOG_FILE"
    for probe in "${ENVELOPE_PROBES[@]}"; do
        label=${probe%%|*}
        cmd=${probe#*|}
        read -r -a cmdarr <<<"$cmd"
        total=$((total + 1))
        local p1=0 p2=0 p3=0 p4=0 prr=0

        _envelope_level "--strict" "${cmdarr[@]}" && { l1=$((l1 + 1)); p1=1; }
        _envelope_level "--strict --verify" "${cmdarr[@]}" && { l2=$((l2 + 1)); p2=1; }
        _envelope_level "--strict --verify --detlog-heap --detlog-stack" "${cmdarr[@]}" \
            && { l3=$((l3 + 1)); p3=1; }

        if ((p2 == 1)); then
            ok=1
            for ((i = 0; i < L4_REPS; i++)); do
                _envelope_level "--strict --verify" "${cmdarr[@]}" || { ok=0; break; }
            done
            ((ok == 1)) && { l4=$((l4 + 1)); p4=1; }
        fi

        # Record then replay end-to-end. `record start --verify` records the
        # run, immediately replays it non-interactively, diffs the two logs, and
        # deletes the recording on success -- a self-contained rr probe that
        # returns a clean exit status. (Plain `hermit replay` launches an
        # interactive gdbserver and hangs/answers prompts under redirection, so
        # it is unsuitable for an unattended gate.) stdin is closed so no probe
        # can ever block waiting for input.
        if timeout "${HERMIT_RR_TIMEOUT:-$HERMIT_SMOKE_TIMEOUT}" "$HERMIT_BIN" record start --verify -- "${cmdarr[@]}" \
            </dev/null >>"$LOG_FILE" 2>&1; then
            rr=$((rr + 1))
            prr=1
        fi

        detail+=("{\"probe\":\"$label\",\"l1\":$p1,\"l2\":$p2,\"l3\":$p3,\"l4\":$p4,\"rr\":$prr}")
    done

    local commit
    commit=$(git -C "$ROOT_DIR" rev-parse --short HEAD 2>/dev/null || echo unknown)
    ENVELOPE_LAST_JSON=$(printf \
        '{"l1_pass":%d,"l2_pass":%d,"l3_pass":%d,"l4_pass":%d,"rr_pass":%d,"total":%d,"commit":"%s","l4_reps":%d,"probes":[%s]}' \
        "$l1" "$l2" "$l3" "$l4" "$rr" "$total" "$commit" "$L4_REPS" \
        "$(IFS=,; echo "${detail[*]}")")
    printf "%s\n" "$ENVELOPE_LAST_JSON" >"$ENVELOPE_JSON"

    printf "\n== Working-envelope vector (commit %s) ==\n" "$commit"
    printf "  L1  hermit run --strict                          : %d/%d\n" "$l1" "$total"
    printf "  L2  --strict --verify (bitwise identical)        : %d/%d\n" "$l2" "$total"
    printf "  L3  --verify --detlog-heap --detlog-stack        : %d/%d\n" "$l3" "$total"
    printf "  L4  L2 stress x%-3d (no divergence)               : %d/%d\n" "$L4_REPS" "$l4" "$total"
    printf "  rr  record/replay end-to-end                     : %d/%d\n" "$rr" "$total"
    printf "  total e2e probes                                 : %d\n" "$total"
    printf "  JSON: %s\n" "$ENVELOPE_JSON"
    printf "  %s\n" "$ENVELOPE_LAST_JSON"
}

# Compare the just-measured envelope against a baseline JSON. Any count that
# decreased is a regression -> nonzero exit. Requires jq.
function envelope_compare {
    local baseline=$1
    [[ -r $baseline ]] || { echo "envelope-compare: cannot read baseline $baseline" >&2; return 2; }
    command -v jq >/dev/null 2>&1 || { echo "envelope-compare: jq not found; cannot compare" >&2; return 2; }

    local regressed=0 key cur base
    printf "\n== Envelope monotonicity vs %s ==\n" "$baseline"
    for key in l1_pass l2_pass l3_pass l4_pass rr_pass total; do
        base=$(jq -r ".$key // 0" "$baseline" 2>/dev/null)
        cur=$(printf "%s" "$ENVELOPE_LAST_JSON" | jq -r ".$key // 0" 2>/dev/null)
        if ((cur < base)); then
            printf "  ❌ REGRESSION %-8s %d < baseline %d\n" "$key" "$cur" "$base"
            regressed=1
        else
            printf "  ✅ %-8s %d >= baseline %d\n" "$key" "$cur" "$base"
        fi
    done
    return "$regressed"
}

# Auto-apply the `locally-validated` PR label after a fully-green full run.
# Landing gate policy is: validate.sh passes locally -> PR carries the
# `locally-validated` label. Agents kept forgetting the manual `gh pr edit`, so
# this automates it -- but only when opted in via --label-pr / VALIDATE_LABEL_PR.
# The PR is taken from $PR_NUMBER when set, else detected from the current branch
# via `gh pr view`. Missing gh, no PR, or a failed edit is a warning only and
# never changes validation's exit status.
readonly LOCALLY_VALIDATED_LABEL="locally-validated"

function apply_locally_validated_label {
    local pr=$PR_NUMBER
    local -a gh_cmd=(gh)

    if ! command -v gh >/dev/null 2>&1; then
        printf "⚠️  --label-pr: gh CLI not found; skipping '%s' label\n" \
            "$LOCALLY_VALIDATED_LABEL" >&2
        return 0
    fi
    # gh on Meta devservers needs the forward proxy; mirror ensure_cargo_nextest.
    if command -v with-proxy >/dev/null 2>&1; then
        gh_cmd=(with-proxy gh)
    fi

    if [[ -z $pr ]]; then
        pr=$("${gh_cmd[@]}" pr view --json number -q .number 2>/dev/null) || true
    fi
    if [[ -z $pr ]]; then
        printf "⚠️  --label-pr: no PR found for the current branch; skipping '%s' label\n" \
            "$LOCALLY_VALIDATED_LABEL" >&2
        return 0
    fi

    if "${gh_cmd[@]}" pr edit "$pr" --add-label "$LOCALLY_VALIDATED_LABEL" \
        >>"$LOG_FILE" 2>&1; then
        printf "🏷️  Applied '%s' label to PR #%s\n" "$LOCALLY_VALIDATED_LABEL" "$pr"
    else
        printf "⚠️  --label-pr: failed to add '%s' label to PR #%s (full log: %s)\n" \
            "$LOCALLY_VALIDATED_LABEL" "$pr" "$LOG_FILE" >&2
    fi
}

function print_summary {
    local passed=$((checks - failures))
    if ((failures == 0)); then
        printf "✅ Validation summary (%s passed, 0 failed; full log: %s)\n" \
            "$passed" "$LOG_FILE"
    else
        printf "❌ Validation summary (%s passed, %s failed; full log: %s)\n" \
            "$passed" "$failures" "$LOG_FILE"
    fi
}

# Envelope-only fast path: build the binary, measure the envelope, optionally
# enforce monotonicity, and exit. CI uses this so its numbers match validate.sh.
if [[ $ENVELOPE_MODE == only ]]; then
    printf "Building workspace for envelope measurement...\n"
    if ! cargo build --workspace >>"$LOG_FILE" 2>&1; then
        printf "❌ build failed (full log: %s)\n" "$LOG_FILE" >&2
        exit 1
    fi
    run_envelope
    if [[ -n $ENVELOPE_BASELINE ]]; then
        envelope_compare "$ENVELOPE_BASELINE"
        exit $?
    fi
    exit 0
fi

run_check "cargo-nextest available" ensure_cargo_nextest
run_check "Build workspace" cargo build --workspace

# Cargo supports concurrent commands in one target directory. Run checks that
# do not execute Hermit guests alongside the ordered runtime and PMU gates.
start_check "Test workspace documentation" cargo test --workspace --doc
start_check "Clippy" cargo clippy --workspace --all-targets -- -D warnings
start_check "Rustfmt" cargo fmt --all -- --check
start_check "Documentation" cargo doc --workspace --no-deps

run_check "Hermit run smoke test" hermit_run_smoke
run_check "Hermit output determinism" hermit_determinism_check
run_check "Hermit verify-mode smoke test" hermit_verify_smoke
# Nextest runs most package unit and Cargo integration targets in parallel.
# Detcore's PMU tests depend on same-binary coordination; nextest would launch
# them as separate processes. Keep detcore and rustdoc tests as Cargo phases.
run_check "Test workspace and integrations" \
    "${NEXTEST_RUN[@]}" --workspace --exclude detcore \
    --exclude hermetic_infra_hermit_flaky-tests
run_check "Test detcore package" cargo test -p detcore
run_check "Fast concurrency stress suite" \
    "${NEXTEST_RUN[@]}" -p hermit --test stress_suite \
    --run-ignored only -E 'test(=fast_chaos_matrix)'
# rr's syscall edge-case programs (third-party/rr submodule) run under Hermit.
if [[ -f "$ROOT_DIR/third-party/rr/src/test/util.h" ]]; then
    run_check "rr syscall suite" \
        cargo test -p hermit --test rr_suite -- --ignored
else
    echo "SKIP: rr syscall suite (run 'git submodule update --init third-party/rr' to enable)"
fi
# `hermit analyze` root-cause search over chaotic schedules (Buck analyze_* targets).
run_check "Hermit analyze scenarios" \
    cargo test -p hermit --test analyze -- --ignored
run_check "Schedule search E2E (requires PMU)" \
    ./tests/util/hermit_analyze_e2e.sh

wait_for_background_checks

# Measure and report the working-envelope vector (informational; does not gate).
run_envelope

print_summary

# On a fully-green full run, optionally tag the PR as locally validated. Guarded
# so it runs only when opted in; it must not affect the final exit status below.
if ((failures == 0)) && ((LABEL_PR == 1)); then
    apply_locally_validated_label
fi

((failures == 0))
