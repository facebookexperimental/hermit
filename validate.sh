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
# Usage: ./validate.sh [quick|full|super] [options]
# Default (no level): run the full validation suite, which also prints the
# working-envelope vector at the end.
#   quick  Core ptrace run/verify/record smoke tests; no alternate backends.
#   full   Everything in quick plus the complete suite and DBI/KVM gates.
#   super  Repeat stress probes (20x by default) under moderate oversubscription
#          and report a pass rate for every probe.
#
# The envelope path is factored out so CI
# can call the *identical* measurement code and produce matching numbers:
#   ./validate.sh --envelope-only            # measure + emit vector (JSON+human)
#   ./validate.sh --envelope-compare FILE    # measure, then fail if any count
#                                            # regressed below FILE's baseline
#   ./validate.sh --strict-compat-only        # run the nonblocking L2 app matrix
#   ./validate.sh --rr-compat-only            # gate the known-passing R/R matrix
#   ./validate.sh --sabre-compat-only         # gate the measured SaBRe matrix
#   ./validate.sh --qemu-l2-only              # run the heavyweight QEMU L2 boot
#   ./validate.sh --verbose                  # stream each gate's command, PID,
#                                            # elapsed time, and subprocess output
# A fully-green full run labels the current PR `locally-validated` by default.
# PR_NUMBER=N overrides branch-based PR detection. Use --no-label-pr or
# VALIDATE_LABEL_PR=0 to disable the non-fatal GitHub update.
ENVELOPE_MODE="full"          # full | only
ENVELOPE_BASELINE=""
VALIDATION_LEVEL="full"       # quick | full | super
VALIDATION_LEVEL_EXPLICIT=0
STRICT_COMPAT_ONLY=0
RR_COMPAT_ONLY=0
SABRE_COMPAT_ONLY=0
QEMU_L2_ONLY=0
LABEL_PR=1
[[ ${VALIDATE_LABEL_PR:-1} == 0 ]] && LABEL_PR=0
VERBOSE=0
[[ ${VALIDATE_VERBOSE:-0} == 1 ]] && VERBOSE=1
PR_NUMBER=${PR_NUMBER:-}
while [[ $# -gt 0 ]]; do
    case "$1" in
        quick|full|super)
            if ((VALIDATION_LEVEL_EXPLICIT == 1)); then
                echo "validate.sh: choose only one validation level" >&2
                exit 2
            fi
            VALIDATION_LEVEL=$1
            VALIDATION_LEVEL_EXPLICIT=1
            shift ;;
        --envelope-only) ENVELOPE_MODE="only"; shift ;;
        --envelope-compare)
            ENVELOPE_MODE="only"; ENVELOPE_BASELINE=${2:-}
            [[ -n $ENVELOPE_BASELINE ]] || { echo "validate.sh: --envelope-compare needs a FILE" >&2; exit 2; }
            shift 2 ;;
        --strict-compat-only) STRICT_COMPAT_ONLY=1; shift ;;
        --rr-compat-only) RR_COMPAT_ONLY=1; shift ;;
        # AUTONOMOUS-BOT-IMPLEMENTED
        # TODO-HUMAN-REVIEW(#589): Review the focused SaBRe compatibility CLI.
        --sabre-compat-only) SABRE_COMPAT_ONLY=1; shift ;;
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
((RR_COMPAT_ONLY == 1)) && ((only_modes += 1))
((SABRE_COMPAT_ONLY == 1)) && ((only_modes += 1))
((QEMU_L2_ONLY == 1)) && ((only_modes += 1))
if ((only_modes > 1)); then
    echo "validate.sh: choose only one focused validation mode" >&2
    exit 2
fi
if ((VALIDATION_LEVEL_EXPLICIT == 1 && only_modes > 0)); then
    echo "validate.sh: validation levels cannot be combined with focused validation modes" >&2
    exit 2
fi
VALIDATION_PROFILE=$VALIDATION_LEVEL
[[ $ENVELOPE_MODE == only ]] && VALIDATION_PROFILE="envelope-only"
((STRICT_COMPAT_ONLY == 1)) && VALIDATION_PROFILE="strict-compat-only"
((RR_COMPAT_ONLY == 1)) && VALIDATION_PROFILE="rr-compat-only"
((SABRE_COMPAT_ONLY == 1)) && VALIDATION_PROFILE="sabre-compat-only"
((QEMU_L2_ONLY == 1)) && VALIDATION_PROFILE="qemu-l2-only"

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
readonly STRICT_COMPAT_ONLY RR_COMPAT_ONLY SABRE_COMPAT_ONLY QEMU_L2_ONLY
readonly VALIDATION_LEVEL VALIDATION_PROFILE

SUPER_REPETITIONS=${SUPER_REPETITIONS:-20}
if [[ ! $SUPER_REPETITIONS =~ ^[1-9][0-9]*$ ]]; then
    echo "validate.sh: SUPER_REPETITIONS must be a positive integer" >&2
    exit 2
fi
host_cpus=$(getconf _NPROCESSORS_ONLN 2>/dev/null || nproc 2>/dev/null || echo 1)
if [[ ! $host_cpus =~ ^[1-9][0-9]*$ ]]; then
    host_cpus=1
fi
SUPER_JOBS=${SUPER_JOBS:-$(((host_cpus * 3 + 1) / 2))}
if [[ ! $SUPER_JOBS =~ ^[1-9][0-9]*$ ]]; then
    echo "validate.sh: SUPER_JOBS must be a positive integer" >&2
    exit 2
fi
readonly SUPER_REPETITIONS SUPER_JOBS host_cpus

HOST_OS=$(sed -n 's/^PRETTY_NAME=//p' /etc/os-release 2>/dev/null | head -n 1)
HOST_OS=${HOST_OS#\"}
HOST_OS=${HOST_OS%\"}
[[ -n $HOST_OS ]] || HOST_OS="unknown Linux"
readonly HOST_OS

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
printf "Hermit validation log\nRoot: %s\nLevel: %s\nHost OS: %s\n\n" \
    "$ROOT_DIR" "$VALIDATION_PROFILE" "$HOST_OS" >"$LOG_FILE"
printf "Validation level: %s (host OS: %s)\n" "$VALIDATION_PROFILE" "$HOST_OS"
if [[ $VALIDATION_LEVEL == super ]]; then
    printf "Super stress: %s repetitions/probe, up to %s concurrent jobs (%s online CPUs)\n" \
        "$SUPER_REPETITIONS" "$SUPER_JOBS" "$host_cpus"
fi
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
readonly REAL_COMPAT_FIXTURES="$ROOT_DIR/target/real-compat-fixtures-$$"
readonly REAL_COMPAT_WORKLOAD="$ROOT_DIR/tests/compat/real_compat_workload.sh"
RR_COMPAT_PHASE_TIMEOUT_SECONDS=${RR_COMPAT_PHASE_TIMEOUT_SECONDS:-60}
if [[ ! $RR_COMPAT_PHASE_TIMEOUT_SECONDS =~ ^[1-9][0-9]*$ ]]; then
    echo "validate.sh: RR_COMPAT_PHASE_TIMEOUT_SECONDS must be a positive integer" >&2
    exit 2
fi
readonly RR_COMPAT_PHASE_TIMEOUT_SECONDS
readonly RR_COMPAT_EXPECTED=128
# The prior 115/121 floor plus 25 passing additions in the current corpus.
# This is a compatibility floor, not a Detcore determinism claim.
readonly SABRE_COMPAT_EXPECTED=140
readonly SABRE_COMPAT_TOTAL=147
COMPATIBILITY_MODE=strict

# Exact label ratchet measured at Hermit a919cce. Commands remain owned by the
# strict corpus below; this set only selects the rows known to pass R/R.
declare -Ar RR_COMPAT_PASSING_LABELS=(
    [echo]=1 [seq]=1 [cat]=1 [wc]=1 [head]=1 [base64]=1 [id]=1
    [lua]=1 [perl]=1 [awk]=1 [bc]=1 [sqlite3]=1 [bash]=1
    [gcc]=1 [g++]=1 [make]=1 [bzip2]=1 [gzip]=1 [xz]=1 [zstd]=1
    [openssl]=1 [sort]=1 [uniq]=1 [tr]=1 [cut]=1 [tee]=1
    [paste]=1 [comm]=1 [join]=1 [find]=1 [stat]=1 [file]=1
    [basename]=1 [dirname]=1 [env]=1 [printenv]=1 [uname]=1
    [factor]=1 [expr]=1 [dd]=1 [df]=1 [du]=1 [hostname]=1
    [whoami]=1 [groups]=1 [tty]=1 [nproc]=1 [arch]=1 [realpath]=1
    [readlink]=1 [sha256sum]=1 [sha1sum]=1 [md5sum]=1 [wc-lines]=1
    [nl]=1 [expand]=1 [unexpand]=1 [test]=1 [bracket]=1 [printf]=1
    [sleep]=1 [stdbuf]=1 [nohup]=1 [nice]=1 [ionice]=1 [taskset]=1
    [chrt]=1 [flock]=1 [logger]=1 [getopt]=1 [column]=1 [hexdump]=1
    [xxd]=1 [strings]=1 [od]=1 [sum]=1 [cksum]=1 [b2sum]=1
    [tsort]=1 [ptx]=1 [pinky]=1 [logname]=1 [users]=1 [uptime]=1
    [grep]=1 [egrep]=1 [fgrep]=1 [sed]=1 [date]=1 [cal]=1 [yes]=1
    [tac]=1 [rev]=1 [fold]=1 [fmt]=1 [shuf]=1 [numfmt]=1
    [split]=1 [cmp]=1
    [java]=1 [python3]=1 [git]=1 [true]=1 [pwd]=1 [base32]=1
    [sha224sum]=1 [sha384sum]=1 [sha512sum]=1 [pr]=1 [ls]=1
    [xargs]=1 [iconv]=1 [ar]=1 [as]=1 [ld]=1 [nm]=1 [objcopy]=1
    [objdump]=1 [ranlib]=1 [readelf]=1 [size]=1 [strip]=1 [addr2line]=1
    [c++filt]=1 [elfedit]=1 [gprof]=1 [cpp]=1 [gcov]=1
)
if ((${#RR_COMPAT_PASSING_LABELS[@]} != RR_COMPAT_EXPECTED)); then
    echo "validate.sh: R/R compatibility label set must contain exactly $RR_COMPAT_EXPECTED rows" >&2
    exit 2
fi
RR_COMPAT_PASSED=0
RR_COMPAT_FAILED=0
RR_COMPAT_TOTAL=0
RR_COMPAT_SKIPPED=0
declare -ar HERMIT_RUN_ARGS=(
    run
    --base-env=minimal
    --no-virtualize-cpuid
    --max-timeslice=disabled
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
    rm -rf "$REAL_COMPAT_FIXTURES"
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

function hermit_record_replay_smoke {
    local case_dir="$VALIDATION_TMP_DIR/record-smoke"
    local data_dir="$case_dir/recording"
    local record_stdout="$case_dir/record.stdout"
    local replay_stdout="$case_dir/replay.stdout"

    rm -rf "$case_dir"
    mkdir -p "$case_dir"
    timeout "$HERMIT_SMOKE_TIMEOUT" \
        "$HERMIT_BIN" record start --data-dir "$data_dir" -- \
        /bin/echo "$SMOKE_MARKER" >"$record_stdout" || return
    timeout "$HERMIT_SMOKE_TIMEOUT" \
        "$HERMIT_BIN" replay --autopilot --data-dir "$data_dir" \
        >"$replay_stdout" || return
    grep -Fxq "$SMOKE_MARKER" "$record_stdout" &&
        cmp -s "$record_stdout" "$replay_stdout"
}

function backend_selector_supported {
    "$HERMIT_BIN" run --help 2>&1 | grep -q -- '--backend'
}

function kvm_backend_available {
    [[ -r /dev/kvm && -w /dev/kvm ]]
}

function dbi_backend_available {
    timeout "$HERMIT_SMOKE_TIMEOUT" \
        "$HERMIT_BIN" run --backend dbi -- /bin/true \
        </dev/null >/dev/null 2>&1
}

function note_backend_skip {
    local backend=$1
    local reason=$2
    printf "SKIP: %s backend gate (%s)\n" "$backend" "$reason"
    printf "SKIP: %s backend gate (%s)\n" "$backend" "$reason" >>"$LOG_FILE"
}

function run_full_backend_gates {
    if ! backend_selector_supported; then
        note_backend_skip "DBI/KVM" "backend selector is unavailable"
        return
    fi

    if kvm_backend_available; then
        run_check "KVM backend parity ratchet" \
            python3 experiments/backend-parity_20260722/run_matrix.py \
            --backend kvm --require-backend
    else
        note_backend_skip "KVM" "/dev/kvm is not readable and writable"
    fi

    if dbi_backend_available; then
        run_check "DBI backend parity ratchet" \
            python3 experiments/backend-parity_20260722/run_matrix.py \
            --backend dbi --require-backend
    else
        note_backend_skip "DBI" "backend smoke did not complete successfully"
    fi
}

function super_probe_command {
    local probe=$1
    local iteration=$2
    local data_dir
    local status

    case "$probe" in
        ptrace-strict-verify)
            timeout "$STRICT_COMPAT_TIMEOUT" \
                "$STRICT_COMPAT_HERMIT_BIN" run --strict --verify -- \
                /bin/echo "hermit-super-$iteration" ;;
        ptrace-pipeline)
            timeout "$STRICT_COMPAT_TIMEOUT" \
                "$STRICT_COMPAT_HERMIT_BIN" run --strict --verify -- \
                bash -c 'yes hermit | head -n 64 | sha256sum' ;;
        ptrace-record-replay)
            data_dir="$VALIDATION_TMP_DIR/super-record-$iteration"
            rm -rf "$data_dir"
            timeout "$STRICT_COMPAT_TIMEOUT" \
                "$STRICT_COMPAT_HERMIT_BIN" record start --verify \
                --data-dir "$data_dir" -- /bin/echo "hermit-super-record-$iteration"
            status=$?
            rm -rf "$data_dir"
            return "$status" ;;
        kvm-verify)
            timeout "$STRICT_COMPAT_TIMEOUT" \
                "$HERMIT_BIN" run --backend kvm --verify -- \
                /bin/echo "hermit-super-kvm-$iteration" ;;
        dbi-verify)
            timeout "$STRICT_COMPAT_TIMEOUT" \
                "$HERMIT_BIN" run --backend dbi --verify -- \
                /bin/echo "hermit-super-dbi-$iteration" ;;
        *)
            echo "validate.sh: unknown super probe: $probe" >&2
            return 2 ;;
    esac
}

function run_super_probe {
    local probe=$1
    local passed=0
    local iteration=1
    local batch_size
    local i
    local status
    local log_file
    local -a pids=()
    local -a iterations=()
    local -a logs=()

    while ((iteration <= SUPER_REPETITIONS)); do
        batch_size=$SUPER_JOBS
        if ((batch_size > SUPER_REPETITIONS - iteration + 1)); then
            batch_size=$((SUPER_REPETITIONS - iteration + 1))
        fi
        pids=()
        iterations=()
        logs=()
        for ((i = 0; i < batch_size; i += 1)); do
            log_file="$VALIDATION_TMP_DIR/super-${probe}-$iteration.log"
            super_probe_command "$probe" "$iteration" >"$log_file" 2>&1 &
            pids+=("$!")
            iterations+=("$iteration")
            logs+=("$log_file")
            iteration=$((iteration + 1))
        done

        for i in "${!pids[@]}"; do
            if wait "${pids[$i]}"; then
                passed=$((passed + 1))
            else
                status=$?
                {
                    printf '%s\n' \
                        "--- super $probe iteration ${iterations[$i]} failed (exit $status) ---"
                    tail -n 120 "${logs[$i]}"
                } >>"$LOG_FILE"
            fi
        done
    done

    if ((passed == SUPER_REPETITIONS)); then
        printf "  ✅ %-24s %s/%s (100%%)\n" \
            "$probe" "$passed" "$SUPER_REPETITIONS" |
            tee -a "$VALIDATION_TMP_DIR/super-report"
        return 0
    fi

    printf "  ⚠️  %-24s %s/%s (%s%%) FLAKY/FAILING\n" \
        "$probe" "$passed" "$SUPER_REPETITIONS" \
        "$((100 * passed / SUPER_REPETITIONS))" |
        tee -a "$VALIDATION_TMP_DIR/super-report"
    return 1
}

function run_super_stress_suite {
    local failed=0
    local -a probes=(
        ptrace-strict-verify
        ptrace-pipeline
        ptrace-record-replay
    )
    local probe

    : >"$VALIDATION_TMP_DIR/super-report"
    if backend_selector_supported && kvm_backend_available; then
        probes+=(kvm-verify)
    else
        note_backend_skip "KVM super stress" "backend unavailable"
        printf "  SKIP KVM super stress (backend unavailable)\n" \
            >>"$VALIDATION_TMP_DIR/super-report"
    fi
    if backend_selector_supported && dbi_backend_available; then
        probes+=(dbi-verify)
    else
        note_backend_skip "DBI super stress" "backend unavailable"
        printf "  SKIP DBI super stress (backend unavailable)\n" \
            >>"$VALIDATION_TMP_DIR/super-report"
    fi

    printf "\n== Super stress pass rates ==\n"
    printf "Repetitions: %s; concurrency: %s; online CPUs: %s\n" \
        "$SUPER_REPETITIONS" "$SUPER_JOBS" "$host_cpus"
    for probe in "${probes[@]}"; do
        run_super_probe "$probe" || failed=$((failed + 1))
    done
    ((failed == 0))
}

# AUTONOMOUS-BOT-IMPLEMENTED
# TODO-HUMAN-REVIEW(#567): Review the blocking R/R compatibility ratchet.
# Run one record or replay phase with a private process group so a regression
# cannot leave tracees behind after the per-phase deadline.
function run_rr_compatibility_phase {
    local stdout_file=$1
    local stderr_file=$2
    shift 2

    local started_at=$SECONDS
    local pid
    local status

    setsid "$@" </dev/null >"$stdout_file" 2>"$stderr_file" &
    pid=$!
    while kill -0 "$pid" 2>/dev/null; do
        if ((SECONDS - started_at >= RR_COMPAT_PHASE_TIMEOUT_SECONDS)); then
            kill -TERM -- "-$pid" 2>/dev/null || true
            if ((TIMEOUT_KILL_GRACE_SECONDS > 0)); then
                sleep "$TIMEOUT_KILL_GRACE_SECONDS"
            fi
            kill -KILL -- "-$pid" 2>/dev/null || true
            wait "$pid" 2>/dev/null || true
            return 124
        fi
        sleep 0.2
    done

    if wait "$pid"; then
        status=0
    else
        status=$?
    fi
    return "$status"
}

function rr_compatibility_probe {
    local label=$1
    shift

    if [[ -z ${RR_COMPAT_PASSING_LABELS[$label]+selected} ]]; then
        RR_COMPAT_SKIPPED=$((RR_COMPAT_SKIPPED + 1))
        return 0
    fi

    local case_dir="$VALIDATION_TMP_DIR/rr-$label"
    local data_dir="$case_dir/recording"
    local started_at=$SECONDS
    local output_start
    local record_status
    local replay_status=125
    local stdout_equal=no
    local summary

    RR_COMPAT_TOTAL=$((RR_COMPAT_TOTAL + 1))
    mkdir -p "$case_dir"
    {
        printf "=== R/R compatibility: %s ===\n" "$label"
        printf "Record:"
        printf " %q" "$STRICT_COMPAT_HERMIT_BIN" record start --data-dir "$data_dir" -- "$@"
        printf "\nReplay: %q replay --autopilot --data-dir %q\n" \
            "$STRICT_COMPAT_HERMIT_BIN" "$data_dir"
    } >>"$LOG_FILE"
    output_start=$(($(wc -l <"$LOG_FILE") + 1))

    if ((VERBOSE == 1)); then
        printf "  R/R compatibility probe: %s\n" "$label"
    fi

    run_rr_compatibility_phase "$case_dir/record.stdout" "$case_dir/record.stderr" \
        "$STRICT_COMPAT_HERMIT_BIN" record start --data-dir "$data_dir" -- "$@"
    record_status=$?
    if ((record_status == 0)); then
        run_rr_compatibility_phase "$case_dir/replay.stdout" "$case_dir/replay.stderr" \
            "$STRICT_COMPAT_HERMIT_BIN" replay --autopilot --data-dir "$data_dir"
        replay_status=$?
        if cmp -s "$case_dir/record.stdout" "$case_dir/replay.stdout"; then
            stdout_equal=yes
        else
            diff -u "$case_dir/record.stdout" "$case_dir/replay.stdout" \
                >"$case_dir/stdout.diff" || true
        fi
    fi

    if ((record_status == 0 && replay_status == 0)) && [[ $stdout_equal == yes ]]; then
        RR_COMPAT_PASSED=$((RR_COMPAT_PASSED + 1))
        printf "  ✅ %-12s PASS R/R (%ss)\n" "$label" "$((SECONDS - started_at))"
        printf "Record exit: 0\nReplay exit: 0\nStdout equal: yes\n\n" >>"$LOG_FILE"
        rm -rf "$case_dir"
        return 0
    fi

    RR_COMPAT_FAILED=$((RR_COMPAT_FAILED + 1))
    {
        printf "Record exit: %s\nReplay exit: %s\nStdout equal: %s\n" \
            "$record_status" "$replay_status" "$stdout_equal"
        if [[ -s $case_dir/record.stderr ]]; then
            printf '%s\n' "--- record stderr ---"
            tail -n 120 "$case_dir/record.stderr"
        fi
        if [[ -s $case_dir/replay.stderr ]]; then
            printf '%s\n' "--- replay stderr ---"
            tail -n 120 "$case_dir/replay.stderr"
        fi
        if [[ -s $case_dir/stdout.diff ]]; then
            printf '%s\n' "--- stdout diff ---"
            sed -n '1,120p' "$case_dir/stdout.diff"
        fi
        printf "\n"
    } >>"$LOG_FILE"
    summary=$(failure_summary "$output_start")
    printf "  ❌ %-12s FAIL R/R (record %s, replay %s, stdout %s: %s)\n" \
        "$label" "$record_status" "$replay_status" "$stdout_equal" "$summary"
    return 0
}

# AUTONOMOUS-BOT-IMPLEMENTED
# TODO-HUMAN-REVIEW(#521): Review the initial nonblocking compatibility policy.
# Run one application through strict L2 or the SaBRe compatibility path. Each
# row has its own hard timeout so a regression cannot stall the rest of the matrix.
function strict_compatibility_probe {
    local label=$1
    shift

    if [[ $COMPATIBILITY_MODE == rr ]]; then
        rr_compatibility_probe "$label" "$@"
        return 0
    fi

    local started_at=$SECONDS
    local output_start
    local status
    local summary
    local assurance=L2
    local -a run_args=(run --strict --verify --)
    if [[ $COMPATIBILITY_MODE == sabre ]]; then
        assurance=SaBRe
        run_args=(run --backend sabre --strict --verify --)
    fi

    {
        printf "=== %s compatibility: %s ===\n" "$assurance" "$label"
        printf "Command: timeout %s %q" \
            "$STRICT_COMPAT_TIMEOUT" "$STRICT_COMPAT_HERMIT_BIN"
        printf " %q" "${run_args[@]}"
        printf " %q" "$@"
        printf "\n"
    } >>"$LOG_FILE"
    output_start=$(($(wc -l <"$LOG_FILE") + 1))

    if ((VERBOSE == 1)); then
        printf "  %s compatibility probe: %s\n" "$assurance" "$label"
    fi

    if timeout "$STRICT_COMPAT_TIMEOUT" \
        "$STRICT_COMPAT_HERMIT_BIN" "${run_args[@]}" "$@" \
        </dev/null >>"$LOG_FILE" 2>&1; then
        status=0
        printf "  ✅ %-12s PASS %s (%ss)\n" \
            "$label" "$assurance" "$((SECONDS - started_at))"
    else
        status=$?
        summary=$(failure_summary "$output_start")
        printf "  ❌ %-12s FAIL %s (exit %s: %s)\n" \
            "$label" "$assurance" "$status" "$summary"
    fi

    {
        printf "Exit: %s\n" "$status"
        printf "Duration: %ss\n\n" "$((SECONDS - started_at))"
    } >>"$LOG_FILE"
    return "$status"
}

# Strict compatibility remains an observation gate in full validation. The
# focused SaBRe and record/replay modes enforce their measured blocking floors.
# Strict mode replaces banner probes with functional workloads so an
# executable that merely starts cannot be counted as compatible at L2.
function functional_compatibility_probe {
    local label=$1
    shift

    if [[ $COMPATIBILITY_MODE != strict ]]; then
        strict_compatibility_probe "$label" "$@"
        return $?
    fi

    strict_compatibility_probe "$label" env \
        REAL_COMPAT_FIXTURES="$REAL_COMPAT_FIXTURES" \
        bash "$REAL_COMPAT_WORKLOAD" "$label"
}
function run_compatibility_corpus {
    local passed=0
    local failed=0
    local total=0

    if [[ $COMPATIBILITY_MODE == rr ]]; then
        printf "\n== Record/replay compatibility baseline (blocking gate) ==\n"
        printf "=== Record/replay compatibility baseline (blocking gate) ===\n" >>"$LOG_FILE"
    elif [[ $COMPATIBILITY_MODE == sabre ]]; then
        printf "\n== SaBRe compatibility ratchet (blocking floor) ==\n"
        printf "=== SaBRe compatibility ratchet (blocking floor) ===\n" >>"$LOG_FILE"
    else
        printf "\n== Strict compatibility envelope (L2, nonblocking) ==\n"
        printf "=== Strict compatibility envelope (L2, nonblocking) ===\n" >>"$LOG_FILE"
    fi

    strict_compatibility_probe echo /bin/echo hermit-compat \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe true /usr/bin/true \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe pwd /usr/bin/pwd \
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
    strict_compatibility_probe base32 /usr/bin/base32 README.md \
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
    functional_compatibility_probe cargo cargo --version \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    functional_compatibility_probe rustc rustc --version \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    functional_compatibility_probe java java -version \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe node /bin/node -e 'console.log(42)' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    # Avoid the PATH fbpython wrapper and exercise the system CPython ELF.
    strict_compatibility_probe python3 /usr/bin/python3 -c 'print(42)' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    # Avoid the PATH Git wrapper: its telemetry sidecar pipes are nondeterministic.
    functional_compatibility_probe git /usr/local/bin/git.meta.real --version \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    functional_compatibility_probe gcc gcc --version \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    functional_compatibility_probe g++ g++ --version \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    functional_compatibility_probe make make --version \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    functional_compatibility_probe ar /usr/bin/ar --version \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    functional_compatibility_probe as /usr/bin/as --version \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    functional_compatibility_probe ld /usr/bin/ld --version \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    functional_compatibility_probe nm /usr/bin/nm --version \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    functional_compatibility_probe objcopy /usr/bin/objcopy --version \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    functional_compatibility_probe objdump /usr/bin/objdump --version \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    functional_compatibility_probe ranlib /usr/bin/ranlib --version \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    functional_compatibility_probe readelf /usr/bin/readelf --version \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    functional_compatibility_probe size /usr/bin/size --version \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    functional_compatibility_probe strip /usr/bin/strip --version \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    functional_compatibility_probe addr2line /usr/bin/addr2line --version \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    functional_compatibility_probe c++filt /usr/bin/c++filt --version \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    functional_compatibility_probe elfedit /usr/bin/elfedit --version \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    functional_compatibility_probe gprof /usr/bin/gprof --version \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    functional_compatibility_probe cpp /usr/bin/cpp --version \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    functional_compatibility_probe gcov /usr/bin/gcov --version \
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
    strict_compatibility_probe sha224sum /usr/bin/sha224sum README.md \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe sha384sum /usr/bin/sha384sum README.md \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe sha512sum /usr/bin/sha512sum README.md \
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
    strict_compatibility_probe pr /usr/bin/pr -t README.md \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe ls /usr/bin/ls -1 README.md \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe xargs bash -c \
        'printf "one\ntwo\n" | /usr/bin/xargs -n1 /bin/echo' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe iconv bash -c \
        'printf "hermit\n" | /usr/bin/iconv -f UTF-8 -t UTF-8' \
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

    if [[ $COMPATIBILITY_MODE == rr ]]; then
        if ((RR_COMPAT_TOTAL != RR_COMPAT_EXPECTED)); then
            printf "❌ Record/replay compatibility baseline selected %s rows; expected %s (%s skipped)\n" \
                "$RR_COMPAT_TOTAL" "$RR_COMPAT_EXPECTED" "$RR_COMPAT_SKIPPED"
            return 1
        fi
        if ((RR_COMPAT_FAILED == 0)); then
            printf "✅ Record/replay compatibility baseline (%s/%s passed R/R; %s unselected)\n" \
                "$RR_COMPAT_PASSED" "$RR_COMPAT_TOTAL" "$RR_COMPAT_SKIPPED"
            return 0
        fi
        printf "❌ Record/replay compatibility baseline (%s/%s passed R/R, %s regressed; %s unselected)\n" \
            "$RR_COMPAT_PASSED" "$RR_COMPAT_TOTAL" "$RR_COMPAT_FAILED" \
            "$RR_COMPAT_SKIPPED"
        return 1
    fi

    total=$((passed + failed))
    if [[ $COMPATIBILITY_MODE == sabre ]]; then
        if ((total != SABRE_COMPAT_TOTAL)); then
            printf "❌ SaBRe compatibility corpus selected %s rows; expected %s\n" \
                "$total" "$SABRE_COMPAT_TOTAL"
            return 1
        fi
        if ((passed < SABRE_COMPAT_EXPECTED)); then
            printf "❌ SaBRe compatibility ratchet regressed (%s/%s passed; floor %s)\n" \
                "$passed" "$total" "$SABRE_COMPAT_EXPECTED"
            return 1
        fi
        printf "✅ SaBRe compatibility ratchet (%s/%s passed; floor %s)\n" \
            "$passed" "$total" "$SABRE_COMPAT_EXPECTED"
        return 0
    fi

    if ((failed == 0)); then
        printf "✅ Strict compatibility envelope (%s/%s passed L2)\n" "$passed" "$total"
        return 0
    fi

    printf "❌ Strict compatibility envelope (%s/%s passed L2, %s regressed; nonblocking)\n" \
        "$passed" "$total" "$failed"
    return 1
}

function run_strict_compatibility_envelope {
    if ! "$ROOT_DIR/tests/compat/prepare_real_compat_fixtures.sh" \
        "$REAL_COMPAT_FIXTURES" >>"$LOG_FILE" 2>&1; then
        printf "❌ Unable to prepare functional compatibility fixtures (log: %s)\n" \
            "$LOG_FILE"
        return 1
    fi

    COMPATIBILITY_MODE=strict
    run_compatibility_corpus
}

function run_sabre_compatibility_envelope {
    local status=0

    COMPATIBILITY_MODE=sabre
    run_compatibility_corpus || status=$?
    COMPATIBILITY_MODE=strict
    return "$status"
}

function require_sabre_artifacts {
    local variable
    for variable in HERMIT_SABRE_RUNNER HERMIT_SABRE_BINARY HERMIT_SABRE_PLUGIN; do
        if [[ -z ${!variable:-} || ! -f ${!variable} ]]; then
            printf "validate.sh: %s must name a regular file for SaBRe compatibility\n" \
                "$variable" >&2
            return 1
        fi
    done
}

function run_rr_compatibility_envelope {
    local status=0

    RR_COMPAT_PASSED=0
    RR_COMPAT_FAILED=0
    RR_COMPAT_TOTAL=0
    RR_COMPAT_SKIPPED=0
    COMPATIBILITY_MODE=rr
    run_compatibility_corpus || status=$?
    COMPATIBILITY_MODE=strict
    return "$status"
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

# Auto-apply the `locally-validated` PR label after a fully-green full run, then
# cancel the redundant in-flight CI run for the exact validated commit.
# Landing gate policy is: validate.sh passes locally -> PR carries the
# `locally-validated` label. Label application and CI cancellation are
# best-effort so GitHub or proxy failures never change the validation result.
# The PR is taken from $PR_NUMBER when set, else detected from the current branch
# via `gh pr view`. Missing gh, no PR, or a failed edit is a warning only and
# never changes validation's exit status.
readonly LOCALLY_VALIDATED_REPOSITORY="rrnewton/hermit"
readonly LOCALLY_VALIDATED_LABEL="locally-validated"

function apply_locally_validated_label {
    local pr=$PR_NUMBER
    local pr_head=""
    local local_head
    local run_id=""
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
        pr=$("${gh_cmd[@]}" pr view --repo "$LOCALLY_VALIDATED_REPOSITORY" \
            --json number -q .number 2>/dev/null) || true
    fi
    if [[ -z $pr ]]; then
        printf "⚠️  no PR found for the current branch; skipping '%s' label\n" \
            "$LOCALLY_VALIDATED_LABEL" >&2
        return 0
    fi
    pr_head=$("${gh_cmd[@]}" pr view "$pr" \
        --repo "$LOCALLY_VALIDATED_REPOSITORY" \
        --json headRefOid -q .headRefOid 2>/dev/null) || true
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
        --repo "$LOCALLY_VALIDATED_REPOSITORY" \
        --color 1d76db \
        --description "Full local validation passed for the current PR head" \
        --force >>"$LOG_FILE" 2>&1 || true

    if "${gh_cmd[@]}" pr edit "$pr" --add-label "$LOCALLY_VALIDATED_LABEL" \
        --repo "$LOCALLY_VALIDATED_REPOSITORY" \
        >>"$LOG_FILE" 2>&1; then
        printf "🏷️  Applied '%s' label to PR #%s\n" "$LOCALLY_VALIDATED_LABEL" "$pr"

        if ! run_id=$("${gh_cmd[@]}" api \
            "repos/${LOCALLY_VALIDATED_REPOSITORY}/actions/workflows/ci.yml/runs?head_sha=${local_head}&per_page=100" \
            --jq '.workflow_runs | map(select(.status != "completed")) | first | .id // empty' \
            2>>"$LOG_FILE"); then
            printf "⚠️  failed to query CI runs for %s (full log: %s)\n" \
                "$local_head" "$LOG_FILE" >&2
            return 0
        fi
        if [[ -z $run_id ]]; then
            printf "ℹ️  No in-flight CI run found for %s\n" "$local_head"
        elif "${gh_cmd[@]}" api --method POST \
            "repos/${LOCALLY_VALIDATED_REPOSITORY}/actions/runs/${run_id}/cancel" \
            >>"$LOG_FILE" 2>&1; then
            printf "🛑 Cancelled CI run %s for %s\n" "$run_id" "$local_head"
        else
            printf "⚠️  failed to cancel CI run %s for %s (full log: %s)\n" \
                "$run_id" "$local_head" "$LOG_FILE" >&2
        fi
    else
        printf "⚠️  failed to add '%s' label to PR #%s (full log: %s)\n" \
            "$LOCALLY_VALIDATED_LABEL" "$pr" "$LOG_FILE" >&2
    fi
}

function print_summary {
    local passed=$((checks - failures))
    if ((failures == 0)); then
        printf "✅ Validation summary [%s] (%s passed, 0 failed; full log: %s)\n" \
            "$VALIDATION_PROFILE" "$passed" "$LOG_FILE"
    else
        printf "❌ Validation summary [%s] (%s passed, %s failed; full log: %s)\n" \
            "$VALIDATION_PROFILE" "$passed" "$failures" "$LOG_FILE"
    fi
}

function run_quick_suite {
    run_check "Build workspace" cargo build --workspace
    run_check "Detcore core unit tests" cargo test -p detcore --lib
    run_check "Hermit run smoke test" hermit_run_smoke
    run_check "Hermit output determinism" hermit_determinism_check
    run_check "Hermit verify-mode smoke test" hermit_verify_smoke
    run_check "Hermit record/replay smoke test" hermit_record_replay_smoke
}

function run_full_suite {
    run_check "cargo-nextest available" ensure_cargo_nextest
    run_quick_suite
    run_check "Build release Hermit" cargo build --release -p hermit

    # Cargo supports concurrent commands in one target directory. Run checks that
    # do not execute Hermit guests alongside the ordered runtime and PMU gates.
    start_check "Test workspace documentation" cargo test --workspace --doc
    start_check "Clippy" cargo clippy --workspace --all-targets -- -D warnings
    start_check "Rustfmt" cargo fmt --all -- --check
    start_check "Documentation" cargo doc --workspace --no-deps

    if ! run_strict_compatibility_envelope; then
        printf "⚠️  Strict compatibility regressions are informational and do not fail full validation yet.\n"
    fi
    run_check "Record/replay compatibility baseline (128 programs)" \
        run_rr_compatibility_envelope
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

    run_full_backend_gates
    wait_for_background_checks

    # Measure and report the working-envelope vector (informational; does not gate).
    run_envelope
}

function run_super_suite {
    run_check "Build workspace" cargo build --workspace
    run_check "Build release Hermit" cargo build --release -p hermit
    run_check "Super repeated determinism probes" run_super_stress_suite
    if [[ -s $VALIDATION_TMP_DIR/super-report ]]; then
        printf "\n== Super stress pass rates ==\n"
        cat "$VALIDATION_TMP_DIR/super-report"
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

if ((SABRE_COMPAT_ONLY == 1)); then
    run_check "SaBRe artifacts configured" require_sabre_artifacts
    if ((failures == 0)); then
        run_check "Build release Hermit for SaBRe compatibility" \
            cargo build --release -p hermit
    fi
    if ((failures == 0)); then
        run_check "SaBRe compatibility ratchet (147 programs)" \
            run_sabre_compatibility_envelope
    fi
    print_summary
    ((failures == 0))
    exit $?
fi

if ((RR_COMPAT_ONLY == 1)); then
    run_check "Build release Hermit for record/replay compatibility" \
        cargo build --release -p hermit
    if ((failures == 0)); then
        run_check "Record/replay compatibility baseline (128 programs)" \
            run_rr_compatibility_envelope
    fi
    print_summary
    ((failures == 0))
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

case "$VALIDATION_LEVEL" in
    quick) run_quick_suite ;;
    full) run_full_suite ;;
    super) run_super_suite ;;
esac

print_summary

# On a fully-green full run, tag the PR unless explicitly disabled. GitHub
# failures are warnings and never affect the final validation exit status.
if [[ $VALIDATION_LEVEL == full ]] && ((failures == 0)) && ((LABEL_PR == 1)); then
    apply_locally_validated_label
fi

((failures == 0))
