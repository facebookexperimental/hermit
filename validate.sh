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
#   ./validate.sh --strict-compat-only        # run the nonblocking L2 app matrix
#   ./validate.sh --qemu-l2-only              # run the heavyweight QEMU L2 boot
#   ./validate.sh --verbose                  # stream each gate's command, PID,
#                                            # elapsed time, and subprocess output
# A fully-green full run labels the current PR `locally-validated` by default.
# PR_NUMBER=N overrides branch-based PR detection. Use --no-label-pr or
# VALIDATE_LABEL_PR=0 to disable the non-fatal GitHub update.
ENVELOPE_MODE="full"          # full | only
ENVELOPE_BASELINE=""
STRICT_COMPAT_ONLY=0
QEMU_L2_ONLY=0
LABEL_PR=1
[[ ${VALIDATE_LABEL_PR:-1} == 0 ]] && LABEL_PR=0
VERBOSE=0
[[ ${VALIDATE_VERBOSE:-0} == 1 ]] && VERBOSE=1
PR_NUMBER=${PR_NUMBER:-}
while [[ $# -gt 0 ]]; do
    case "$1" in
        --envelope-only) ENVELOPE_MODE="only"; shift ;;
        --envelope-compare)
            ENVELOPE_MODE="only"; ENVELOPE_BASELINE=${2:-}
            [[ -n $ENVELOPE_BASELINE ]] || { echo "validate.sh: --envelope-compare needs a FILE" >&2; exit 2; }
            shift 2 ;;
        --strict-compat-only) STRICT_COMPAT_ONLY=1; shift ;;
        --qemu-l2-only) QEMU_L2_ONLY=1; shift ;;
        --label-pr) LABEL_PR=1; shift ;;
        --verbose) VERBOSE=1; shift ;;
        --no-label-pr) LABEL_PR=0; shift ;;
        -h|--help)
            grep -E '^#( |$)' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "validate.sh: unknown argument: $1 (try --help)" >&2; exit 2 ;;
    esac
done

# AUTONOMOUS-BOT-IMPLEMENTED
# TODO-HUMAN-REVIEW(#553)
only_modes=0
[[ $ENVELOPE_MODE == only ]] && ((only_modes += 1))
((STRICT_COMPAT_ONLY == 1)) && ((only_modes += 1))
((QEMU_L2_ONLY == 1)) && ((only_modes += 1))
if ((only_modes > 1)); then
    echo "validate.sh: choose only one of --envelope-only/--envelope-compare, --strict-compat-only, or --qemu-l2-only" >&2
    exit 2
fi

default_gate_timeout_seconds=600
if ((QEMU_L2_ONLY == 1)); then
    qemu_phase_timeout_seconds=${QEMU_L2_PHASE_TIMEOUT_SECONDS:-300}
    if [[ ! $qemu_phase_timeout_seconds =~ ^[1-9][0-9]*$ ]]; then
        echo "validate.sh: QEMU_L2_PHASE_TIMEOUT_SECONDS must be a positive integer" >&2
        exit 2
    fi
    # One boot-oracle phase plus run1/run2/compare, with five minutes for
    # process startup, teardown, and reporting outside those phase budgets.
    default_gate_timeout_seconds=$((4 * qemu_phase_timeout_seconds + 300))
fi
GATE_TIMEOUT_SECONDS=${VALIDATE_GATE_TIMEOUT_SECONDS:-$default_gate_timeout_seconds}
TIMEOUT_KILL_GRACE_SECONDS=${VALIDATE_TIMEOUT_KILL_GRACE_SECONDS:-5}
VERBOSE_INTERVAL_SECONDS=${VALIDATE_VERBOSE_INTERVAL_SECONDS:-10}
if [[ ! $GATE_TIMEOUT_SECONDS =~ ^[1-9][0-9]*$ ]]; then
    echo "validate.sh: VALIDATE_GATE_TIMEOUT_SECONDS must be a positive integer" >&2
    exit 2
fi
if [[ ! $TIMEOUT_KILL_GRACE_SECONDS =~ ^[0-9]+$ ]]; then
    echo "validate.sh: VALIDATE_TIMEOUT_KILL_GRACE_SECONDS must be a non-negative integer" >&2
    exit 2
fi
if [[ ! $VERBOSE_INTERVAL_SECONDS =~ ^[1-9][0-9]*$ ]]; then
    echo "validate.sh: VALIDATE_VERBOSE_INTERVAL_SECONDS must be a positive integer" >&2
    exit 2
fi
readonly VERBOSE GATE_TIMEOUT_SECONDS TIMEOUT_KILL_GRACE_SECONDS VERBOSE_INTERVAL_SECONDS
readonly STRICT_COMPAT_ONLY QEMU_L2_ONLY

checks=0
failures=0
active_check_pid=""
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
if ((VERBOSE == 1)); then
    printf "Verbose validation enabled\n"
    printf "  root: %s\n" "$ROOT_DIR"
    printf "  log: %s\n" "$LOG_FILE"
    printf "  gate timeout: %ss (kill grace: %ss; heartbeat: %ss)\n" \
        "$GATE_TIMEOUT_SECONDS" "$TIMEOUT_KILL_GRACE_SECONDS" "$VERBOSE_INTERVAL_SECONDS"
fi

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
readonly STRICT_COMPAT_HERMIT_BIN="$ROOT_DIR/target/release/hermit"
readonly STRICT_COMPAT_TIMEOUT=60
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

function kill_process_tree {
    local pid=$1
    local signal=$2
    local child

    while read -r child; do
        [[ -n $child ]] || continue
        kill_process_tree "$child" "$signal"
    done < <(ps -o pid= --ppid "$pid" 2>/dev/null)
    kill "-$signal" "$pid" 2>/dev/null || true
}

function cleanup {
    local pid

    if [[ -n $active_check_pid ]]; then
        kill_process_tree "$active_check_pid" TERM
    fi
    for pid in "${background_pids[@]}"; do
        kill_process_tree "$pid" TERM
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

function run_timed_command {
    local name=$1
    local log_file=$2
    shift 2

    local started_at=$SECONDS
    local next_report=$VERBOSE_INTERVAL_SECONDS
    local pid
    local status
    local elapsed
    local grace_deadline

    (
        if ((VERBOSE == 1)); then
            "$@" 2>&1 |
                tee -a "$log_file" |
                sed -u "s|^|[$name] |"
        else
            "$@" >>"$log_file" 2>&1
        fi
    ) &
    pid=$!
    active_check_pid=$pid

    if ((VERBOSE == 1)); then
        printf "  subprocess PID: %s\n" "$pid"
    fi

    while kill -0 "$pid" 2>/dev/null; do
        elapsed=$((SECONDS - started_at))
        if ((elapsed >= GATE_TIMEOUT_SECONDS)); then
            kill_process_tree "$pid" TERM
            grace_deadline=$((SECONDS + TIMEOUT_KILL_GRACE_SECONDS))
            while kill -0 "$pid" 2>/dev/null && ((SECONDS < grace_deadline)); do
                sleep 0.2
            done
            if kill -0 "$pid" 2>/dev/null; then
                kill_process_tree "$pid" KILL
            fi
            wait "$pid" 2>/dev/null || true
            active_check_pid=""
            printf "Gate timed out after %ss (subprocess PID %s)\n" \
                "$GATE_TIMEOUT_SECONDS" "$pid" >>"$log_file"
            printf "⏱️  %s timed out after %ss (subprocess PID %s)\n" \
                "$name" "$GATE_TIMEOUT_SECONDS" "$pid"
            return 124
        fi

        if ((VERBOSE == 1 && elapsed >= next_report)); then
            printf "  still running: %s (PID %s, elapsed %ss/%ss)\n" \
                "$name" "$pid" "$elapsed" "$GATE_TIMEOUT_SECONDS"
            next_report=$((next_report + VERBOSE_INTERVAL_SECONDS))
        fi
        sleep 0.2
    done

    if wait "$pid"; then
        status=0
    else
        status=$?
    fi
    active_check_pid=""
    if ((VERBOSE == 1)); then
        printf "  subprocess PID %s finished after %ss\n" "$pid" "$((SECONDS - started_at))"
    fi
    return "$status"
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

    if ((VERBOSE == 1)); then
        printf "\n▶ %s\n" "$name"
        printf "  command:"
        printf " %q" "$@"
        printf "\n  timeout: %ss\n" "$GATE_TIMEOUT_SECONDS"
    fi

    if run_timed_command "$name" "$LOG_FILE" "$@"; then
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

    {
        printf "Command:"
        printf " %q" "$@"
        printf "\n"
    } >"$log_file"
    if ((VERBOSE == 1)); then
        printf "\n▶ %s (background)\n" "$name"
        printf "  command:"
        printf " %q" "$@"
        printf "\n  timeout: %ss\n" "$GATE_TIMEOUT_SECONDS"
    fi

    (
        local started_at=$SECONDS
        local status

        if run_timed_command "$name" "$log_file" "$@"; then
            status=0
        else
            status=$?
        fi
        printf "%s\n" "$((SECONDS - started_at))" >"$duration_file"
        exit "$status"
    ) &

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

        if ((VERBOSE == 1)); then
            printf "\n▶ Collecting background gate: %s (manager PID %s)\n" "$name" "$pid"
        fi

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

# AUTONOMOUS-BOT-IMPLEMENTED
# TODO-HUMAN-REVIEW(#521): Review the initial nonblocking compatibility policy.
# Run one known-compatible application at L2. Each row has its own hard timeout
# so a regression cannot stall the rest of the matrix.
function strict_compatibility_probe {
    local label=$1
    shift

    local started_at=$SECONDS
    local output_start
    local status
    local summary

    {
        printf "=== Strict compatibility: %s ===\n" "$label"
        printf "Command: timeout %s %q run --strict --verify --" \
            "$STRICT_COMPAT_TIMEOUT" "$STRICT_COMPAT_HERMIT_BIN"
        printf " %q" "$@"
        printf "\n"
    } >>"$LOG_FILE"
    output_start=$(($(wc -l <"$LOG_FILE") + 1))

    if ((VERBOSE == 1)); then
        printf "  compatibility probe: %s\n" "$label"
    fi

    if timeout "$STRICT_COMPAT_TIMEOUT" \
        "$STRICT_COMPAT_HERMIT_BIN" run --strict --verify -- "$@" \
        </dev/null >>"$LOG_FILE" 2>&1; then
        status=0
        printf "  ✅ %-12s PASS L2 (%ss)\n" "$label" "$((SECONDS - started_at))"
    else
        status=$?
        summary=$(failure_summary "$output_start")
        printf "  ❌ %-12s FAIL (exit %s: %s)\n" "$label" "$status" "$summary"
    fi

    {
        printf "Exit: %s\n" "$status"
        printf "Duration: %ss\n\n" "$((SECONDS - started_at))"
    } >>"$LOG_FILE"
    return "$status"
}

# This is an observation gate for now: full validation prints every regression
# but does not add it to the fatal failure count. --strict-compat-only exposes
# the real aggregate status so CI can mark the step while continue-on-error
# keeps the lane nonblocking until the matrix is ratcheted.
function run_strict_compatibility_envelope {
    local passed=0
    local failed=0
    local total=0

    printf "\n== Strict compatibility envelope (L2, nonblocking) ==\n"
    printf "=== Strict compatibility envelope (L2, nonblocking) ===\n" >>"$LOG_FILE"

    strict_compatibility_probe echo /bin/echo hermit-compat \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe seq /usr/bin/seq 10 \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe cat /bin/cat README.md \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe wc /usr/bin/wc -c README.md \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe head /usr/bin/head -n 3 README.md \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe base64 /usr/bin/base64 README.md \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe id /usr/bin/id -u \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe lua lua -e 'print(42)' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe perl perl -e 'print 42, chr(10)' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe awk awk 'BEGIN { print 42 }' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe bc bash -c 'printf "6*7\n" | bc' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe sqlite3 sqlite3 :memory: 'SELECT 1+1;' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    # Expand $i inside the guest shell, not here.
    # shellcheck disable=SC2016
    strict_compatibility_probe bash bash -c \
        'for i in 1 2 3; do echo "$i"; done' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe cargo cargo --version \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe rustc rustc --version \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe java java -version \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe node node -e 'console.log(42)' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe gcc gcc --version \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe g++ g++ --version \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe make make --version \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe bzip2 bash -c \
        'bzip2 -c README.md | sha256sum' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe gzip bash -c \
        'gzip -cn README.md | sha256sum' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe xz bash -c \
        'xz -c README.md | sha256sum' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe zstd bash -c \
        'zstd -q -c README.md | sha256sum' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe openssl openssl dgst -sha256 /etc/hostname \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe sort bash -c \
        'printf "beta\nalpha\nalpha\n" | sort' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe uniq bash -c \
        'printf "alpha\nalpha\nbeta\n" | uniq -c' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe tr bash -c \
        'printf "Hermit\n" | tr "[:upper:]" "[:lower:]"' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe cut bash -c \
        'printf "alpha:beta\n" | cut -d: -f2' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe tee bash -c \
        'printf "tee-through-hermit\n" | tee /dev/null' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe paste bash -c \
        'paste -d: <(printf "alpha\nbeta\n") <(printf "1\n2\n")' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe comm bash -c \
        'comm <(printf "alpha\nbeta\n") <(printf "beta\ngamma\n")' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe join bash -c \
        'join <(printf "1 alpha\n2 beta\n") <(printf "1 one\n2 two\n")' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe find find /etc -maxdepth 1 \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    # Numeric output avoids nondeterministic host NSS owner/group lookups.
    strict_compatibility_probe stat stat -c '%n %s %f' /etc/hostname \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe file file /bin/sh \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe basename /usr/bin/basename /usr/local/bin/hermit \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe dirname /usr/bin/dirname /usr/local/bin/hermit \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe env /usr/bin/env -i HERMIT_COMPAT=env /usr/bin/env \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe printenv /usr/bin/env -i HERMIT_COMPAT=printenv \
        /usr/bin/printenv HERMIT_COMPAT \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe uname /usr/bin/uname -sr \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe factor factor 42 \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe expr expr 2 + 2 \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe dd bash -c \
        'printf "hermit-dd\n" | dd bs=1 count=10 status=none' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe df /usr/bin/df -P / \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe du /usr/bin/du -sk README.md \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe hostname /usr/bin/hostname \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe whoami /usr/bin/whoami \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    # An explicit user avoids host-specific supplementary GIDs without names.
    strict_compatibility_probe groups /usr/bin/groups root \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    # The compatibility harness supplies /dev/null, so tty should report the
    # expected non-terminal result while the wrapper preserves a zero exit.
    # shellcheck disable=SC2016
    strict_compatibility_probe tty bash -c \
        'output=$(tty 2>&1); status=$?; printf "%s\n" "$output"; test "$status" -eq 1' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe nproc /usr/bin/nproc \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe arch /usr/bin/arch \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe realpath /usr/bin/realpath README.md \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe readlink /usr/bin/readlink -f README.md \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    # shellcheck disable=SC2016
    strict_compatibility_probe mktemp bash -c \
        'd=$(mktemp -d /tmp/hermit-compat.XXXXXX) && basename "$d" && rmdir "$d"' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe sha256sum /usr/bin/sha256sum README.md \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe sha1sum /usr/bin/sha1sum README.md \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe md5sum /usr/bin/md5sum README.md \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe wc-lines /usr/bin/wc -l README.md \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe nl bash -c \
        'printf "alpha\nbeta\n" | nl -ba' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe expand bash -c \
        'printf "a\tb\n" | expand -t 4' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe unexpand bash -c \
        'printf "a   b\n" | unexpand -a -t 4' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe test /usr/bin/test 42 -eq 42 \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe bracket /usr/bin/[ 42 -eq 42 ']' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe printf /usr/bin/printf '%s=%d\n' hermit 42 \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe sleep /usr/bin/sleep 0 \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe stdbuf /usr/bin/stdbuf -o0 \
        /usr/bin/printf 'stdbuf-ok\n' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe nohup /usr/bin/nohup /bin/echo nohup-ok \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe nice /usr/bin/nice -n 1 /bin/echo nice-ok \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe ionice /usr/bin/ionice -c 3 /bin/echo ionice-ok \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    # Query the virtualized guest PID rather than setting a host CPU/policy.
    # shellcheck disable=SC2016
    strict_compatibility_probe taskset bash -c 'taskset -p $$' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    # shellcheck disable=SC2016
    strict_compatibility_probe chrt bash -c 'chrt -p $$' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe flock bash -c \
        'set -euo pipefail; rm -f /tmp/hermit-compat-flock; flock -x /tmp/hermit-compat-flock -c "printf \"flock-ok\\n\""; rm -f /tmp/hermit-compat-flock' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    # Exercise logger formatting without writing to a host logging service.
    strict_compatibility_probe logger /usr/bin/logger --stderr --no-act \
        -t hermit-compat logger-ok \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe getopt /usr/bin/getopt -o ab: -- -a -b value \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe column bash -c \
        'set -euo pipefail; printf "alpha:1\nbeta:22\n" | column -t -s :' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe hexdump bash -c \
        'set -euo pipefail; printf "Hermit\n" | hexdump -C' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe xxd bash -c \
        'set -euo pipefail; printf "Hermit\n" | xxd' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe strings bash -c \
        'set -euo pipefail; printf "\0Hermit\0" | strings -n 5' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe od bash -c \
        'set -euo pipefail; printf "Hermit\n" | od -An -tx1' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe sum /usr/bin/sum README.md \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe cksum /usr/bin/cksum README.md \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe b2sum /usr/bin/b2sum README.md \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe tsort bash -c \
        'set -euo pipefail; printf "alpha beta\nbeta gamma\n" | tsort' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe ptx bash -c \
        'set -euo pipefail; printf "alpha beta\n" | ptx -f' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe pinky /usr/bin/pinky -l root \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe logname /usr/bin/logname \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe users /usr/bin/users \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe uptime /usr/bin/uptime -p \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    # timeout is intentionally absent: "timeout 1 true" hangs in Run1 while
    # the parent waits in rt_sigsuspend for its delayed child.
    # Filesystem fixtures use distinct fixed paths and clean them before and
    # after each run so both sides of --verify begin from equivalent state.
    strict_compatibility_probe diff bash -c \
        'set -euo pipefail; rm -rf /tmp/hermit-compat-diff; mkdir /tmp/hermit-compat-diff; printf "alpha\nbeta\n" >/tmp/hermit-compat-diff/a; cp /tmp/hermit-compat-diff/a /tmp/hermit-compat-diff/b; diff -u /tmp/hermit-compat-diff/a /tmp/hermit-compat-diff/b; rm -rf /tmp/hermit-compat-diff; printf "diff-ok\n"' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe patch bash -c \
        'set -euo pipefail; rm -rf /tmp/hermit-compat-patch; mkdir /tmp/hermit-compat-patch; printf "old\n" >/tmp/hermit-compat-patch/file; printf "%s\n" "--- file" "+++ file" "@@ -1 +1 @@" "-old" "+new" | (cd /tmp/hermit-compat-patch && patch -s file); cat /tmp/hermit-compat-patch/file; rm -rf /tmp/hermit-compat-patch' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe grep bash -c \
        'set -euo pipefail; printf "alpha\nbeta\ngamma\n" | grep -x alpha' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe egrep bash -c \
        'set -euo pipefail; printf "alpha\nbeta\ngamma\n" | egrep "alpha|gamma"' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe fgrep bash -c \
        'set -euo pipefail; printf "alpha.beta\nalphaXbeta\n" | fgrep "alpha.beta"' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe sed bash -c \
        'set -euo pipefail; printf "alpha beta\n" | sed "s/alpha/omega/"' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe tar bash -c \
        'set -euo pipefail; rm -rf /tmp/hermit-compat-tar; mkdir /tmp/hermit-compat-tar; printf "archive-data\n" >/tmp/hermit-compat-tar/input; touch -t 200001010000 /tmp/hermit-compat-tar/input; tar -cf /tmp/hermit-compat-tar/archive.tar -C /tmp/hermit-compat-tar input; tar -tf /tmp/hermit-compat-tar/archive.tar; rm -rf /tmp/hermit-compat-tar' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe cp bash -c \
        'set -euo pipefail; rm -rf /tmp/hermit-compat-cp; mkdir /tmp/hermit-compat-cp; printf "copy-data\n" >/tmp/hermit-compat-cp/source; cp /tmp/hermit-compat-cp/source /tmp/hermit-compat-cp/copy; cmp /tmp/hermit-compat-cp/source /tmp/hermit-compat-cp/copy; cat /tmp/hermit-compat-cp/copy; rm -rf /tmp/hermit-compat-cp' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe mv bash -c \
        'set -euo pipefail; rm -rf /tmp/hermit-compat-mv; mkdir /tmp/hermit-compat-mv; printf "move-data\n" >/tmp/hermit-compat-mv/source; mv /tmp/hermit-compat-mv/source /tmp/hermit-compat-mv/moved; test ! -e /tmp/hermit-compat-mv/source; cat /tmp/hermit-compat-mv/moved; rm -rf /tmp/hermit-compat-mv' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe rm bash -c \
        'set -euo pipefail; rm -rf /tmp/hermit-compat-rm; mkdir /tmp/hermit-compat-rm; printf "remove-data\n" >/tmp/hermit-compat-rm/file; rm /tmp/hermit-compat-rm/file; test ! -e /tmp/hermit-compat-rm/file; rmdir /tmp/hermit-compat-rm; printf "rm-ok\n"' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe mkdir bash -c \
        'set -euo pipefail; rm -rf /tmp/hermit-compat-mkdir; mkdir -p /tmp/hermit-compat-mkdir/a/b; test -d /tmp/hermit-compat-mkdir/a/b; printf "mkdir-ok\n"; rm -rf /tmp/hermit-compat-mkdir' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe rmdir bash -c \
        'set -euo pipefail; rm -rf /tmp/hermit-compat-rmdir; mkdir /tmp/hermit-compat-rmdir; rmdir /tmp/hermit-compat-rmdir; test ! -e /tmp/hermit-compat-rmdir; printf "rmdir-ok\n"' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe touch bash -c \
        'set -euo pipefail; rm -f /tmp/hermit-compat-touch; touch -t 200001010000 /tmp/hermit-compat-touch; stat -c "%Y %s" /tmp/hermit-compat-touch; rm -f /tmp/hermit-compat-touch' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe chmod bash -c \
        'set -euo pipefail; rm -f /tmp/hermit-compat-chmod; printf "mode\n" >/tmp/hermit-compat-chmod; chmod 640 /tmp/hermit-compat-chmod; stat -c "%a" /tmp/hermit-compat-chmod; rm -f /tmp/hermit-compat-chmod' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe chown bash -c \
        'set -euo pipefail; rm -f /tmp/hermit-compat-chown; printf "owner\n" >/tmp/hermit-compat-chown; chown --reference=README.md /tmp/hermit-compat-chown; stat -c "%u:%g" /tmp/hermit-compat-chown; rm -f /tmp/hermit-compat-chown' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe ln bash -c \
        'set -euo pipefail; rm -rf /tmp/hermit-compat-ln; mkdir /tmp/hermit-compat-ln; printf "link-data\n" >/tmp/hermit-compat-ln/source; ln /tmp/hermit-compat-ln/source /tmp/hermit-compat-ln/hard; ln -s source /tmp/hermit-compat-ln/sym; stat -c "%h" /tmp/hermit-compat-ln/source; cat /tmp/hermit-compat-ln/hard /tmp/hermit-compat-ln/sym; rm -rf /tmp/hermit-compat-ln' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe date /usr/bin/date -u +'%Y-%m-%dT%H:%M:%SZ' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe cal /usr/bin/cal 1 2000 \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe yes bash -c \
        'set -eu; yes hermit | head -n 3' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe tac bash -c \
        'set -euo pipefail; printf "first\nsecond\nthird\n" | tac' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe rev bash -c \
        'set -euo pipefail; printf "Hermit\ndeterminism\n" | rev' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe fold bash -c \
        'set -euo pipefail; printf "abcdefghijklmnopqrstuvwxyz\n" | fold -w 8' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe fmt bash -c \
        'set -euo pipefail; printf "Hermit formats this deterministic paragraph into narrow lines for validation.\n" | fmt -w 24' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe shuf bash -c \
        'set -euo pipefail; printf "alpha\nbeta\ngamma\ndelta\n" | shuf' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe numfmt /usr/bin/numfmt --to=iec 1048576 \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe csplit bash -c \
        'set -euo pipefail; rm -rf /tmp/hermit-compat-csplit; mkdir /tmp/hermit-compat-csplit; printf "alpha\nbeta\ngamma\n" >/tmp/hermit-compat-csplit/input; (cd /tmp/hermit-compat-csplit && csplit -s input "/^beta$/" && cat xx00 xx01); rm -rf /tmp/hermit-compat-csplit' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe split bash -c \
        'set -euo pipefail; rm -rf /tmp/hermit-compat-split; mkdir /tmp/hermit-compat-split; printf "one\ntwo\nthree\nfour\n" >/tmp/hermit-compat-split/input; split -l 2 /tmp/hermit-compat-split/input /tmp/hermit-compat-split/part-; cat /tmp/hermit-compat-split/part-*; rm -rf /tmp/hermit-compat-split' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe install bash -c \
        'set -euo pipefail; rm -rf /tmp/hermit-compat-install; mkdir /tmp/hermit-compat-install; install -m 640 README.md /tmp/hermit-compat-install/copied; stat -c "%a %s" /tmp/hermit-compat-install/copied; rm -rf /tmp/hermit-compat-install' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe mkfifo bash -c \
        'set -euo pipefail; rm -f /tmp/hermit-compat-fifo; mkfifo /tmp/hermit-compat-fifo; stat -c "%F" /tmp/hermit-compat-fifo; rm -f /tmp/hermit-compat-fifo' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    # The task named 29 utilities; cmp completes the requested 30-row push.
    strict_compatibility_probe cmp bash -c \
        'set -euo pipefail; rm -rf /tmp/hermit-compat-cmp; mkdir /tmp/hermit-compat-cmp; printf "same\n" >/tmp/hermit-compat-cmp/a; printf "same\n" >/tmp/hermit-compat-cmp/b; cmp -s /tmp/hermit-compat-cmp/a /tmp/hermit-compat-cmp/b; printf "cmp-ok\n"; rm -rf /tmp/hermit-compat-cmp' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    # free is intentionally absent: its live /proc/meminfo values differ
    # between otherwise identical strict runs.

    total=$((passed + failed))
    if ((failed == 0)); then
        printf "✅ Strict compatibility envelope (%s/%s passed L2)\n" "$passed" "$total"
        return 0
    fi

    printf "❌ Strict compatibility envelope (%s/%s passed L2, %s regressed; nonblocking)\n" \
        "$passed" "$total" "$failed"
    return 1
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

    if ((VERBOSE == 1)); then
        printf "\n▶ Working-envelope measurement (L4 stress reps=%s)\n" "$L4_REPS"
    fi
    printf "=== Working-envelope measurement (L4 stress reps=%s) ===\n" "$L4_REPS" >>"$LOG_FILE"
    for probe in "${ENVELOPE_PROBES[@]}"; do
        label=${probe%%|*}
        cmd=${probe#*|}
        read -r -a cmdarr <<<"$cmd"
        if ((VERBOSE == 1)); then
            printf "  envelope probe: %s (%s)\n" "$label" "$cmd"
        fi
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
# `locally-validated` label. Label creation and application are best-effort so
# GitHub or proxy failures never change the validation result.
# The PR is taken from $PR_NUMBER when set, else detected from the current branch
# via `gh pr view`. Missing gh, no PR, or a failed edit is a warning only and
# never changes validation's exit status.
readonly LOCALLY_VALIDATED_LABEL="locally-validated"

function apply_locally_validated_label {
    local pr=$PR_NUMBER
    local pr_head=""
    local local_head
    local -a gh_cmd=(gh)

    if ! command -v gh >/dev/null 2>&1; then
        printf "⚠️  gh CLI not found; skipping '%s' label\n" \
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
        printf "⚠️  no PR found for the current branch; skipping '%s' label\n" \
            "$LOCALLY_VALIDATED_LABEL" >&2
        return 0
    fi
    pr_head=$("${gh_cmd[@]}" pr view "$pr" --json headRefOid \
        -q .headRefOid 2>/dev/null) || true
    if [[ -z $pr_head ]]; then
        printf "⚠️  could not read PR #%s head; skipping '%s' label\n" \
            "$pr" "$LOCALLY_VALIDATED_LABEL" >&2
        return 0
    fi
    local_head=$(git rev-parse HEAD)
    if [[ $pr_head != "$local_head" ]]; then
        printf "⚠️  PR #%s advanced from %s to %s; skipping '%s' label\n" \
            "$pr" "$local_head" "$pr_head" "$LOCALLY_VALIDATED_LABEL" >&2
        return 0
    fi

    # Ensure a fresh repository can accept the label. Failure is harmless here:
    # the edit below reports the actionable warning and validation remains green.
    "${gh_cmd[@]}" label create "$LOCALLY_VALIDATED_LABEL" \
        --color 1d76db \
        --description "Full local validation passed for the current PR head" \
        --force >>"$LOG_FILE" 2>&1 || true

    if "${gh_cmd[@]}" pr edit "$pr" --add-label "$LOCALLY_VALIDATED_LABEL" \
        >>"$LOG_FILE" 2>&1; then
        printf "🏷️  Applied '%s' label to PR #%s\n" "$LOCALLY_VALIDATED_LABEL" "$pr"
    else
        printf "⚠️  failed to add '%s' label to PR #%s (full log: %s)\n" \
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
if ((STRICT_COMPAT_ONLY == 1)); then
    run_check "Build release Hermit for strict compatibility" \
        cargo build --release -p hermit
    if ((failures != 0)); then
        exit 1
    fi
    run_strict_compatibility_envelope
    exit $?
fi

# AUTONOMOUS-BOT-IMPLEMENTED
# TODO-HUMAN-REVIEW(#553)
if ((QEMU_L2_ONLY == 1)); then
    run_check "Build release Hermit for QEMU L2" \
        cargo build --release -p hermit
    if ((failures == 0)); then
        run_check "QEMU strict L2 boot (heavyweight)" \
            ./experiments/qemu-boot-debug/strict_l2_test.sh
    fi
    print_summary
    ((failures == 0))
    exit $?
fi

if [[ $ENVELOPE_MODE == only ]]; then
    run_check "Build workspace for envelope measurement" cargo build --workspace
    if ((failures != 0)); then
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
run_check "Build release Hermit for strict compatibility" \
    cargo build --release -p hermit

# Cargo supports concurrent commands in one target directory. Run checks that
# do not execute Hermit guests alongside the ordered runtime and PMU gates.
start_check "Test workspace documentation" cargo test --workspace --doc
start_check "Clippy" cargo clippy --workspace --all-targets -- -D warnings
start_check "Rustfmt" cargo fmt --all -- --check
start_check "Documentation" cargo doc --workspace --no-deps

run_check "Hermit run smoke test" hermit_run_smoke
run_check "Hermit output determinism" hermit_determinism_check
run_check "Hermit verify-mode smoke test" hermit_verify_smoke
if ! run_strict_compatibility_envelope; then
    printf "⚠️  Strict compatibility regressions are informational and do not fail full validation yet.\n"
fi
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

# On a fully-green full run, tag the PR unless explicitly disabled. GitHub
# failures are warnings and never affect the final validation exit status.
if ((failures == 0)) && ((LABEL_PR == 1)); then
    apply_locally_validated_label
fi

((failures == 0))
