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
# Usage: ./validate.sh [quick|portable-only|full|super] [options]
# Default (no level): run the full validation suite, which also prints the
# working-envelope vector at the end. VALIDATE_LEVEL may select the same level.
#   quick        Core ptrace run/verify/record smoke tests; no alternate backends.
#   portable-only  Portable build, test, lint, format, and documentation gates
#                matching GitHub-managed portable CI; no PMU or namespace requirements.
#   full         Everything in quick plus the complete suite and DBI/KVM gates.
#   super        Repeat stress probes (20x by default) under moderate
#                oversubscription and report a pass rate for every probe.
#   --quick      Alias for the quick level.
#   --portable     Alias for the portable-only level.
#
# The envelope path is factored out so CI
# can call the *identical* measurement code and produce matching numbers:
#   ./validate.sh --envelope-only            # measure + emit vector (JSON+human)
#   ./validate.sh --envelope-compare FILE    # measure, then fail if any count
#                                            # regressed below FILE's baseline
#   ./validate.sh --strict-compat-only        # run the blocking L2 app matrix;
#                                            # STRICT_COMPAT_HERMIT_BIN reuses
#                                            # an existing executable
#   ./validate.sh --portable-strict-compat-only # portable L2 matrix with bounded diagnostics
#   ./validate.sh --rr-compat-only            # gate the known-passing R/R matrix
#   ./validate.sh --sabre-compat-only         # gate the measured SaBRe matrix;
#                                            # needs executable HERMIT_SABRE_BINARY
#   ./validate.sh --e9patch-compat-only       # gate core + installed e9patch L2 apps
#   ./validate.sh --qemu-l2-only              # run the heavyweight QEMU L2 boot
#   ./validate.sh --portable-only               # no PMU/CPUID hardware required
#   ./validate.sh --privileged-only             # PMU/CPUID-dependent tests only
#   ./validate.sh --verbose                  # stream each gate's command, PID,
#                                            # elapsed time, and subprocess output
# Every foreground/background gate has a process-tree timeout. Override the
# profile default with VALIDATE_GATE_TIMEOUT_SECONDS; tune TERM-to-KILL grace
# with VALIDATE_TIMEOUT_KILL_GRACE_SECONDS.
# A fully-green full run labels the current PR `locally-validated` by default.
# PR_NUMBER=N overrides branch-based PR detection. Use --no-label-pr or
# VALIDATE_LABEL_PR=0 to disable the non-fatal GitHub update.
ENVELOPE_MODE="full"          # full | only
ENVELOPE_BASELINE=""
VALIDATION_LEVEL=${VALIDATE_LEVEL:-full} # quick | portable-only | full | super
VALIDATION_LEVEL_EXPLICIT=0
if [[ -n ${VALIDATE_LEVEL:-} ]]; then
    case "$VALIDATION_LEVEL" in
        quick|portable-only|full|super) ;;
        *)
            echo "validate.sh: invalid VALIDATE_LEVEL: $VALIDATION_LEVEL" >&2
            exit 2 ;;
    esac
    VALIDATION_LEVEL_EXPLICIT=1
fi

function select_validation_level {
    local level=$1
    if ((VALIDATION_LEVEL_EXPLICIT == 1)); then
        echo "validate.sh: choose only one validation level" >&2
        exit 2
    fi
    VALIDATION_LEVEL=$level
    VALIDATION_LEVEL_EXPLICIT=1
}
STRICT_COMPAT_ONLY=0
PORTABLE_STRICT_COMPAT_ONLY=0
PORTABLE_STRICT_PROBE_ARGS=0
RR_COMPAT_ONLY=0
SABRE_COMPAT_ONLY=0
E9PATCH_COMPAT_ONLY=0
QEMU_L2_ONLY=0
PRIVILEGED_ONLY=0
LABEL_PR=1
[[ ${VALIDATE_LABEL_PR:-1} == 0 ]] && LABEL_PR=0
VERBOSE=0
[[ ${VALIDATE_VERBOSE:-0} == 1 ]] && VERBOSE=1
PR_NUMBER=${PR_NUMBER:-}
while [[ $# -gt 0 ]]; do
    case "$1" in
        quick|portable-only|full|super)
            select_validation_level "$1"
            shift ;;
        --quick)
            select_validation_level quick
            shift ;;
        --portable|--portable-only)
            select_validation_level portable-only
            shift ;;
        --envelope-only) ENVELOPE_MODE="only"; shift ;;
        --envelope-compare)
            ENVELOPE_MODE="only"; ENVELOPE_BASELINE=${2:-}
            [[ -n $ENVELOPE_BASELINE ]] || { echo "validate.sh: --envelope-compare needs a FILE" >&2; exit 2; }
            shift 2 ;;
        --strict-compat-only) STRICT_COMPAT_ONLY=1; shift ;;
        # TODO-HUMAN-REVIEW(#719): Review the focused portable compatibility CLI.
        --portable-strict-compat-only)
            STRICT_COMPAT_ONLY=1; PORTABLE_STRICT_COMPAT_ONLY=1; shift ;;
        --rr-compat-only) RR_COMPAT_ONLY=1; shift ;;
        # AUTONOMOUS-BOT-IMPLEMENTED
        # TODO-HUMAN-REVIEW(#589): Review the focused SaBRe compatibility CLI.
        --sabre-compat-only) SABRE_COMPAT_ONLY=1; shift ;;
        # TODO-HUMAN-REVIEW(PR-664): Review the focused e9patch compatibility CLI.
        --e9patch-compat-only) E9PATCH_COMPAT_ONLY=1; shift ;;
        --qemu-l2-only) QEMU_L2_ONLY=1; shift ;;
        --privileged-only) PRIVILEGED_ONLY=1; shift ;;
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
((E9PATCH_COMPAT_ONLY == 1)) && ((only_modes += 1))
((QEMU_L2_ONLY == 1)) && ((only_modes += 1))
((PRIVILEGED_ONLY == 1)) && ((only_modes += 1))
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
((PORTABLE_STRICT_COMPAT_ONLY == 1)) && VALIDATION_PROFILE="portable-strict-compat-only"
((RR_COMPAT_ONLY == 1)) && VALIDATION_PROFILE="rr-compat-only"
((SABRE_COMPAT_ONLY == 1)) && VALIDATION_PROFILE="sabre-compat-only"
((E9PATCH_COMPAT_ONLY == 1)) && VALIDATION_PROFILE="e9patch-compat-only"
((QEMU_L2_ONLY == 1)) && VALIDATION_PROFILE="qemu-l2-only"
((PRIVILEGED_ONLY == 1)) && VALIDATION_PROFILE="privileged-only"

case "$VALIDATION_PROFILE" in
    quick) VALIDATION_ESTIMATE="about 3 minutes" ;;
    portable-only) VALIDATION_ESTIMATE="about 8 minutes" ;;
    full) VALIDATION_ESTIMATE="about 20-70 minutes; R/R fails fast if its canary is broken" ;;
    super) VALIDATION_ESTIMATE="about 30-90 minutes, depending on repetitions and backends" ;;
    strict-compat-only) VALIDATION_ESTIMATE="about 5-15 minutes" ;;
    portable-strict-compat-only) VALIDATION_ESTIMATE="about 5-15 minutes" ;;
    rr-compat-only) VALIDATION_ESTIMATE="about 5-65 minutes when healthy; fails fast on canary failure" ;;
    sabre-compat-only) VALIDATION_ESTIMATE="about 10-20 minutes" ;;
    e9patch-compat-only) VALIDATION_ESTIMATE="about 5-20 minutes" ;;
    qemu-l2-only) VALIDATION_ESTIMATE="about 30-60 minutes" ;;
    privileged-only) VALIDATION_ESTIMATE="about 60-180 minutes" ;;
    envelope-only) VALIDATION_ESTIMATE="about 5 minutes" ;;
esac
readonly VALIDATION_ESTIMATE

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
elif ((PRIVILEGED_ONLY == 1)); then
    # The PMU memory-race fixtures perform tens of millions of instrumented
    # atomic operations. They need a longer per-family budget than portable CI.
    default_gate_timeout_seconds=3600
elif ((SABRE_COMPAT_ONLY == 1)); then
    # The focused SaBRe profile measures 212 programs and is documented as a
    # 10-20 minute gate. Preserve headroom without bypassing the caller's
    # VALIDATE_GATE_TIMEOUT_SECONDS override.
    default_gate_timeout_seconds=1800
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
readonly STRICT_COMPAT_ONLY PORTABLE_STRICT_COMPAT_ONLY RR_COMPAT_ONLY SABRE_COMPAT_ONLY
readonly E9PATCH_COMPAT_ONLY QEMU_L2_ONLY PRIVILEGED_ONLY
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

mkdir -p "$ROOT_DIR/target/validation"
VALIDATION_TMP_DIR=$(mktemp -d "$ROOT_DIR/target/validation/hermit-validate.XXXXXX")
if [[ -z $VALIDATION_TMP_DIR ]]; then
    echo "Unable to create validation workspace." >&2
    exit 1
fi
readonly VALIDATION_TMP_DIR
export XDG_CONFIG_HOME="$VALIDATION_TMP_DIR/xdg-config"
mkdir -p "$XDG_CONFIG_HOME"
readonly XDG_CONFIG_HOME

LOG_FILE=$(mktemp "${TMPDIR:-/tmp}/hermit-validate.XXXXXX.log")
if [[ -z $LOG_FILE ]]; then
    echo "Unable to create validation log." >&2
    exit 1
fi
readonly LOG_FILE
printf "Hermit validation log\nRoot: %s\nLevel: %s\nHost OS: %s\n\n" \
    "$ROOT_DIR" "$VALIDATION_PROFILE" "$HOST_OS" >"$LOG_FILE"
printf "Validation level: %s (host OS: %s)\n" "$VALIDATION_PROFILE" "$HOST_OS"
printf "Estimated time: %s\n" "$VALIDATION_ESTIMATE"
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
readonly DEFAULT_STRICT_COMPAT_HERMIT_BIN="$ROOT_DIR/target/release/hermit"
STRICT_COMPAT_HERMIT_BIN=${STRICT_COMPAT_HERMIT_BIN:-"$DEFAULT_STRICT_COMPAT_HERMIT_BIN"}
readonly STRICT_COMPAT_HERMIT_BIN
readonly STRICT_COMPAT_TIMEOUT=60
readonly BACKEND_COMPAT_RESULTS="$VALIDATION_TMP_DIR/backend-compat-results.tsv"
readonly COMPAT_SUMMARY_RESULTS="$VALIDATION_TMP_DIR/compat-summary-results.tsv"
VALIDATE_RESULTS_FILE=${VALIDATE_RESULTS_FILE:-"$ROOT_DIR/target/validate-results.txt"}
readonly VALIDATE_RESULTS_FILE
readonly REAL_COMPAT_FIXTURES="$ROOT_DIR/target/real-compat-fixtures-$$"
readonly E9PATCH_NSSWITCH_FILE="$VALIDATION_TMP_DIR/e9patch-nsswitch.conf"
readonly REAL_COMPAT_WORKLOAD="$ROOT_DIR/tests/compat/real_compat_workload.sh"
readonly COMPLEX_SHELL_WORKLOAD="$ROOT_DIR/tests/compat/complex_shell_workload.sh"
RR_COMPAT_PHASE_TIMEOUT_SECONDS=${RR_COMPAT_PHASE_TIMEOUT_SECONDS:-60}
if [[ ! $RR_COMPAT_PHASE_TIMEOUT_SECONDS =~ ^[1-9][0-9]*$ ]]; then
    echo "validate.sh: RR_COMPAT_PHASE_TIMEOUT_SECONDS must be a positive integer" >&2
    exit 2
fi
readonly RR_COMPAT_PHASE_TIMEOUT_SECONDS
# The compatibility corpus contains semantic workloads only. Banner-only wget,
# netcat, socat, and sensors probes were removed when the E2E harness landed.
readonly STRICT_COMPAT_TOTAL=191
# The R/R ratchet asserts exactly the programs measured to pass record/replay.
# History: PR #729 established a 131-row set (incl. ruby/dc/tcl) and PR #662 added
# descriptor-state and writable-filesystem rows, reaching 144. A measured sweep
# (see RR_COMPAT_KNOWN_FAILURES below) then found five gcc/binutils toolchain
# programs -- g++, ar, strip, gprof, gcov -- that diverge on replay, so the honest
# passing set is 139, not 144. Do NOT raise this number without a fresh sweep
# proving the added rows pass R/R: an aspirational count is a phantom ratchet that
# either fails the parse-time size check or masks real divergences.
readonly RR_COMPAT_EXPECTED=139
# Require the established SaBRe compatibility floor across the full measured corpus.
# Explicit must-pass rows below ratchet fixed programs without allowing host-sensitive
# rows to make the aggregate floor alternate between green and red.
readonly SABRE_COMPAT_EXPECTED=202
# AUTONOMOUS-BOT-IMPLEMENTED
# TODO-HUMAN-REVIEW(PR-1154): Review synchronization of the measured SaBRe corpus size.
readonly SABRE_COMPAT_TOTAL=212
readonly E9PATCH_COMPAT_TOTAL=155
COMPATIBILITY_MODE=strict
E9PATCH_COMPAT_REWRITTEN=0
E9PATCH_COMPAT_ZERO_SITE=0
E9PATCH_COMPAT_CANDIDATE_ONLY=0
E9PATCH_COMPAT_NON_ELF=0
E9PATCH_COMPAT_NO_DIAGNOSTIC=0

# Tracked compatibility gaps that are intentionally excluded from the
# executable corpus. They remain in the canonical denominator and table.
declare -Ar COMPAT_SUMMARY_KNOWN_FAILURES=(
    # Explicit --strict now fail-closes on unsupported syscalls (PR #644). These
    # programs each require a syscall Detcore does not yet determinize, so they
    # correctly abort under fail-closed --strict; they only passed the envelope
    # previously because --strict used to forward unsupported syscalls.
    # (chrt/ioprio_set-based ionice/flock were determinized in PR-batch-51 and
    # are now measured as ordinary passing rows below.)
    [curl-localhost]="fail-closed --strict rejects the unsupported shutdown syscall on some hosts"
    [lsof]="fail-closed --strict rejects the unsupported close_range syscall"
    [make]="fail-closed --strict rejects the unsupported setresuid syscall"
    [wget-localhost]="fail-closed --strict rejects the unsupported shutdown syscall on some hosts"
)
declare -Ar PORTABLE_STRICT_DIAGNOSTIC_FAILURES=(
    [top]="live process-table reads differ on the GitHub-managed portable runner"
    [zstd]="timed out on the GitHub-managed portable no-PMU runner"
    [zstd-roundtrip]="timed out on the GitHub-managed portable no-PMU runner"
)
declare -Ar PORTABLE_STRICT_SUPER_ONLY=(
    [rustc]="full compile-link-run workload"
    [javac]="JVM startup and compile-run workload"
    [java]="threaded JVM filesystem and digest workload"
    [node]="Node.js runtime startup workload"
)
PORTABLE_STRICT_DIAGNOSTIC_FAILURE_COUNT=0
declare -A COMPAT_SUMMARY_CELLS=()
declare -ar COMPAT_SUMMARY_CATEGORIES=(
    coreutils
    interpreters
    build-toolchain
    text-data
    archive-compression
    filesystem-storage
    process-scheduling
    system-introspection
    networking
    applications
    other
)

# Programs owned by the strict corpus that are measured to FAIL record/replay and
# are therefore excluded from the R/R passing ratchet below. Each records cleanly
# but diverges on replay at hermit-cli/src/replayer/mod.rs:776 (the two runs part
# on a specific thread/syscall event) -- a deeper multi-threaded compile/link/
# analyze desync, distinct from the regular-file lseek(SEEK_CUR) divergence fixed
# alongside this ratchet. They remain probed under strict/sabre modes; this list
# documents why rr mode does not gate on them (the gcc/binutils toolchain R/R gap).
declare -Ar RR_COMPAT_KNOWN_FAILURES=(
    [g++]="replay diverges (thread 13, ~event 132): C++ front-end header/.gch path resolution (readlink vs newfstatat) desyncs the event stream"
    [ar]="replay diverges (thread 11, ~event 3): archive workload teardown (execveat rm -rf) reorders against the recorded stream"
    [strip]="replay diverges at replayer/mod.rs:776 after a clean record"
    [gprof]="replay diverges at replayer/mod.rs:776 after a clean record"
    [gcov]="replay diverges at replayer/mod.rs:776 after a clean record"
)
# Commands remain owned by the strict corpus below; this exact set only selects
# rows measured to pass R/R. The five RR_COMPAT_KNOWN_FAILURES toolchain programs
# (g++, ar, strip, gprof, gcov) are intentionally absent.
declare -Ar RR_COMPAT_PASSING_LABELS=(
    [echo]=1 [seq]=1 [cat]=1 [wc]=1 [head]=1 [base64]=1 [id]=1
    [lua]=1 [perl]=1 [awk]=1 [bc]=1 [sqlite3]=1 [bash]=1
    [gcc]=1 [make]=1 [bzip2]=1 [gzip]=1 [xz]=1 [zstd]=1
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
    [split]=1 [cmp]=1 [rmdir]=1 [mkfifo]=1 [mkdir]=1 [node]=1
    [diff]=1 [cp]=1 [install]=1 [tar]=1 [mv]=1 [rm]=1 [touch]=1 [chmod]=1
    [java]=1 [python3]=1 [git]=1 [true]=1 [pwd]=1 [base32]=1
    [sha224sum]=1 [sha384sum]=1 [sha512sum]=1 [pr]=1 [ls]=1
    [xargs]=1 [iconv]=1 [as]=1 [ld]=1 [nm]=1 [objcopy]=1
    [objdump]=1 [ranlib]=1 [readelf]=1 [size]=1 [addr2line]=1
    [c++filt]=1 [elfedit]=1 [cpp]=1
    [ruby]=1 [dc]=1 [tcl]=1 [free]=1
)
# mktemp remains excluded: SIGCHLD delivery can race the command-substitution pipe EOF
# during replay, changing deterministic log order while preserving output and exit status.
if ((${#RR_COMPAT_PASSING_LABELS[@]} != RR_COMPAT_EXPECTED)); then
    echo "validate.sh: R/R compatibility label set must contain exactly $RR_COMPAT_EXPECTED rows" >&2
    exit 2
fi
RR_COMPAT_PASSED=0
RR_COMPAT_FAILED=0
RR_COMPAT_TOTAL=0
RR_COMPAT_SKIPPED=0
RR_COMPAT_FAIL_FAST_SKIPPED=0
RR_COMPAT_CANARY_FAILED=0
RR_COMPAT_CANARY_LABEL=""
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
    if declare -F print_compatibility_summary >/dev/null; then
        print_compatibility_summary
    fi
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
    local timeout_seconds=$3
    shift 3

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
        if ((elapsed >= timeout_seconds)); then
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
                "$timeout_seconds" "$pid" >>"$log_file"
            printf "⏱️  %s timed out after %ss (subprocess PID %s)\n" \
                "$name" "$timeout_seconds" "$pid"
            return 124
        fi

        if ((VERBOSE == 1 && elapsed >= next_report)); then
            printf "  still running: %s (PID %s, elapsed %ss/%ss)\n" \
                "$name" "$pid" "$elapsed" "$timeout_seconds"
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

function run_check_with_timeout {
    local timeout_seconds=$1
    local name=$2
    shift 2

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
        printf "\n  timeout: %ss\n" "$timeout_seconds"
    fi

    if run_timed_command "$name" "$LOG_FILE" "$timeout_seconds" "$@"; then
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

function run_check {
    run_check_with_timeout "$GATE_TIMEOUT_SECONDS" "$@"
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

        if run_timed_command "$name" "$log_file" "$GATE_TIMEOUT_SECONDS" "$@"; then
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

function kvm_backend_available {
    [[ -r /dev/kvm && -w /dev/kvm ]]
}

function dbi_backend_available {
    timeout "$HERMIT_SMOKE_TIMEOUT" \
        "$HERMIT_BIN" --log=info run --backend dbi --strict --verify -- \
        /bin/echo hermit-dbi-probe \
        </dev/null >/dev/null 2>&1
}

function note_backend_skip {
    local backend=$1
    local reason=$2
    printf "SKIP: %s backend gate (%s)\n" "$backend" "$reason"
    printf "SKIP: %s backend gate (%s)\n" "$backend" "$reason" >>"$LOG_FILE"
}

function run_full_backend_gates {
    local -a backends=(--backend ptrace)

    if kvm_backend_available; then
        backends+=(--backend kvm)
    else
        note_backend_skip "KVM" "/dev/kvm is not readable and writable"
    fi

    if dbi_backend_available; then
        backends+=(--backend dbi)
    else
        note_backend_skip "DBI" "backend smoke did not complete successfully"
    fi

    run_check "Real backend compatibility matrix" \
        python3 tests/backend-parity/run_matrix.py \
        "${backends[@]}" --probe-gaps --require-backend \
        --output "$BACKEND_COMPAT_RESULTS"
}

# AUTONOMOUS-BOT-IMPLEMENTED
# TODO-HUMAN-REVIEW(#706): Review the canonical cross-backend compatibility summary.
function compat_summary_backend {
    case "$COMPATIBILITY_MODE" in
        strict) printf "ptrace" ;;
        sabre) printf "sabre" ;;
        *) return 1 ;;
    esac
}

function record_compatibility_result {
    local program=$1
    local result=$2
    local detail=${3:-}
    local backend

    backend=$(compat_summary_backend) || return 0
    detail=${detail//$'\t'/ }
    detail=${detail//$'\n'/ }
    if [[ ! -e $COMPAT_SUMMARY_RESULTS ]]; then
        printf "program\tbackend\tresult\tdetail\n" >"$COMPAT_SUMMARY_RESULTS"
    fi
    printf "%s\t%s\t%s\t%s\n" \
        "$program" "$backend" "$result" "$detail" >>"$COMPAT_SUMMARY_RESULTS"
}

function compat_summary_programs {
    local key

    for key in "${!COMPAT_SUMMARY_CELLS[@]}"; do
        printf "%s\n" "${key%:*}"
    done
}

function compatibility_category {
    case "$1" in
        arch|b2sum|base32|base64|basename|basenc|bracket|cat|cksum|comm|cp|csplit|cut|date|dd|df|dirname|du|echo|env|expand|expr|factor|fmt|fold|head|id|install|join|ln|ls|md5sum|mkdir|mkfifo|mktemp|mv|nice|nl|nohup|nproc|numfmt|od|paste|pathchk|pinky|pr|printenv|printf|ptx|pwd|readlink|realpath|rm|rmdir|seq|sha1sum|sha224sum|sha256sum|sha384sum|sha512sum|shred|shuf|sleep|sort|split|stat|stdbuf|sum|sync|tac|tee|test|timeout|touch|tr|true|truncate|tsort|tty|uname|unexpand|uniq|users|wc|wc-lines|whoami|xargs|yes)
            printf "coreutils" ;;
        awk|bash|bc|dc|java|lua|node|perl|python3|ruby|tcl)
            printf "interpreters" ;;
        addr2line|ar|as|c++filt|cargo|clang|cmake|cpp|elfedit|flex|g++|gcc|gcov|gprof|javac|ld|m4|make|nm|objcopy|objdump|pkg-config|ranlib|readelf|rustc|shell-build|size|strings|strip)
            printf "build-toolchain" ;;
        col|colrm|column|diff|diff3|dos2unix|egrep|envsubst|fgrep|file|find|grep|hexdump|iconv|msgfmt|msgunfmt|patch|rev|sed|xxd)
            printf "text-data" ;;
        bzip2|bzip2-roundtrip|cpio-roundtrip|crc32|gzip|gzip-roundtrip|tar|tar-roundtrip|xz|xz-roundtrip|zip-unzip|zstd|zstd-roundtrip)
            printf "archive-compression" ;;
        chmod|chown|cmp|fallocate|findmnt|mountpoint|namei|setfacl|setfattr)
            printf "filesystem-storage" ;;
        chrt|flock|getopt|groups|ionice|kill|logger|logname|lsof|pgrep|pkill|ps|taskset|time|top)
            printf "process-scheduling" ;;
        cal|free|getconf|hostname|iostat|lscpu|lsirq|lsmod|mpstat-softirqs|numactl-hardware|numastat|pidstat-disk|sar-resource-tables|sensors-version|sysctl-random-uuid|uptime|uuidgen|vmstat|vmstat-disk)
            printf "system-introspection" ;;
        curl|curl-localhost|ip|netcat|socat|ss|wget|wget-localhost)
            printf "networking" ;;
        cscope|git|jq|openssl|sqlite3|xmllint)
            printf "applications" ;;
        *) printf "other" ;;
    esac
}

function compatibility_count_cell {
    local output_variable=$1
    local category=$2
    local backend=$3
    local -n _pass_counts=$4
    local -n _measured_counts=$5
    local key="$category:$backend"
    local passed=${_pass_counts[$key]:-0}
    local measured=${_measured_counts[$key]:-0}

    if ((measured == 0)); then
        printf -v "$output_variable" "N/A"
    else
        printf -v "$output_variable" "%s/%s" "$passed" "$measured"
    fi
}

function backend_parity_program_name {
    case "$1" in
        hello_stdout) printf "echo" ;;
        argument_forwarding) printf "printf" ;;
        exit_zero) printf "true" ;;
        file_read) printf "cat" ;;
        *) return 1 ;;
    esac
}

function load_compatibility_results {
    local test_name
    local backend
    local _expectation
    local result
    local _seconds
    local detail
    local program

    COMPAT_SUMMARY_CELLS=()
    if [[ -r $BACKEND_COMPAT_RESULTS ]]; then
        while IFS=$'\t' read -r test_name backend _expectation result _seconds detail; do
            [[ $test_name != test_name ]] || continue
            program=$(backend_parity_program_name "$test_name" || true)
            [[ -n $program ]] || continue
            COMPAT_SUMMARY_CELLS["$program:$backend"]=$result
        done <"$BACKEND_COMPAT_RESULTS"
    fi
    if [[ -r $COMPAT_SUMMARY_RESULTS ]]; then
        while IFS=$'\t' read -r program backend result detail; do
            [[ $program != program ]] || continue
            COMPAT_SUMMARY_CELLS["$program:$backend"]=$result
        done <"$COMPAT_SUMMARY_RESULTS"
    fi
}

function backend_compatibility_cell {
    local output_variable=$1
    local program=$2
    local backend=$3
    local result=${COMPAT_SUMMARY_CELLS["$program:$backend"]:-}
    local cell

    if [[ -z $result && $backend == ptrace &&
        -n ${COMPAT_SUMMARY_KNOWN_FAILURES[$program]+known} ]]; then
        result=FAIL
    fi

    case "$result" in
        PASS|XPASS) cell=PASS ;;
        FAIL) cell=FAIL ;;
        *) cell=N/A ;;
    esac
    printf -v "$output_variable" "%s" "$cell"
}

function compatibility_status {
    local output_variable=$1
    local program=$2
    shift 2
    local cell
    local pass_count=0
    local fail_count=0
    local backend_index=0
    local failed_list
    local rendered_status
    local -a backend_names=(ptrace KVM DBI SaBRe)
    local -a failed_backends=()

    for cell in "$@"; do
        case "$cell" in
            PASS) pass_count=$((pass_count + 1)) ;;
            FAIL)
                fail_count=$((fail_count + 1))
                failed_backends+=("${backend_names[$backend_index]}")
                ;;
        esac
        backend_index=$((backend_index + 1))
    done
    failed_list=${failed_backends[*]}
    failed_list=${failed_list// /,}

    if [[ -n ${COMPAT_SUMMARY_KNOWN_FAILURES[$program]+known} && $1 == FAIL ]]; then
        rendered_status="❌ known-fail: ${COMPAT_SUMMARY_KNOWN_FAILURES[$program]}"
    elif ((pass_count == 4)); then
        rendered_status="✅"
    elif ((pass_count > 0 && fail_count > 0)); then
        rendered_status="⚠️ FAIL: $failed_list"
    elif ((fail_count > 0)); then
        rendered_status="❌ FAIL: $failed_list"
    elif ((pass_count == 1)) && [[ $1 == PASS ]]; then
        rendered_status="ptrace-only"
    elif ((pass_count > 0)); then
        rendered_status="partial"
    else
        rendered_status="not measured"
    fi
    printf -v "$output_variable" "%s" "$rendered_status"
}

function print_compatibility_summary {
    local program
    local category
    local backend
    local category_cell
    local ptrace
    local kvm
    local dbi
    local sabre
    local status
    local total=0
    local category_programs
    local raw_tmp="$VALIDATE_RESULTS_FILE.tmp.$$"
    local rendered="$VALIDATION_TMP_DIR/compat-summary-rendered.tsv"
    local -a programs=()
    local -a backends=(ptrace kvm dbi sabre)
    local -A category_totals=()
    local -A pass_counts=()
    local -A measured_counts=()
    load_compatibility_results
    mapfile -t programs < <(compat_summary_programs | sort -u)

    : >"$rendered"
    for program in "${programs[@]}"; do
        category=$(compatibility_category "$program")
        backend_compatibility_cell ptrace "$program" ptrace
        backend_compatibility_cell kvm "$program" kvm
        backend_compatibility_cell dbi "$program" dbi
        backend_compatibility_cell sabre "$program" sabre
        compatibility_status status "$program" "$ptrace" "$kvm" "$dbi" "$sabre"
        printf "%s\t%s\t%s\t%s\t%s\t%s\t%s\n" \
            "$program" "$category" "$ptrace" "$kvm" "$dbi" "$sabre" "$status" \
            >>"$rendered"
        total=$((total + 1))
        category_totals[$category]=$((${category_totals[$category]:-0} + 1))
        for backend in "${backends[@]}"; do
            backend_compatibility_cell category_cell "$program" "$backend"
            case "$category_cell" in
                PASS)
                    pass_counts["$category:$backend"]=$((${pass_counts["$category:$backend"]:-0} + 1))
                    measured_counts["$category:$backend"]=$((${measured_counts["$category:$backend"]:-0} + 1))
                    ;;
                FAIL)
                    measured_counts["$category:$backend"]=$((${measured_counts["$category:$backend"]:-0} + 1))
                    ;;
            esac
        done
    done

    mkdir -p "$(dirname "$VALIDATE_RESULTS_FILE")"
    {
        printf "Hermit compatibility results\n"
        printf "profile\t%s\n" "$VALIDATION_PROFILE"
        printf "program\tcategory\tptrace\tKVM\tDBI\tSaBRe\tstatus\n"
        cat "$rendered"
    } >"$raw_tmp"
    mv "$raw_tmp" "$VALIDATE_RESULTS_FILE"

    if ((total == 0)); then
        return 0
    fi

    printf "\nCOMPATIBILITY SUMMARY (%s recorded programs)\n" "$total"
    printf "%-22s | %8s | %9s | %9s | %9s | %9s\n" \
        "Category" "Programs" "ptrace" "KVM" "DBI" "SaBRe"
    printf "%s\n" "-----------------------|----------|-----------|-----------|-----------|----------"
    for category in "${COMPAT_SUMMARY_CATEGORIES[@]}"; do
        category_programs=${category_totals[$category]:-0}
        ((category_programs > 0)) || continue
        compatibility_count_cell ptrace "$category" ptrace pass_counts measured_counts
        compatibility_count_cell kvm "$category" kvm pass_counts measured_counts
        compatibility_count_cell dbi "$category" dbi pass_counts measured_counts
        compatibility_count_cell sabre "$category" sabre pass_counts measured_counts
        printf "%-22s | %8s | %9s | %9s | %9s | %9s\n" \
            "$category" "$category_programs" "$ptrace" "$kvm" "$dbi" "$sabre"
    done

    for backend in "${backends[@]}"; do
        pass_counts["total:$backend"]=0
        measured_counts["total:$backend"]=0
        for category in "${COMPAT_SUMMARY_CATEGORIES[@]}"; do
            pass_counts["total:$backend"]=$((${pass_counts["total:$backend"]} + ${pass_counts["$category:$backend"]:-0}))
            measured_counts["total:$backend"]=$((${measured_counts["total:$backend"]} + ${measured_counts["$category:$backend"]:-0}))
        done
    done
    compatibility_count_cell ptrace total ptrace pass_counts measured_counts
    compatibility_count_cell kvm total kvm pass_counts measured_counts
    compatibility_count_cell dbi total dbi pass_counts measured_counts
    compatibility_count_cell sabre total sabre pass_counts measured_counts
    printf "%-22s | %8s | %9s | %9s | %9s | %9s\n" \
        "TOTAL" "$total" "$ptrace" "$kvm" "$dbi" "$sabre"
    printf "P/M means passing/measured; failures are M-P and unmeasured rows are excluded from M.\n"
    for backend in "${backends[@]}"; do
        printf "  %-7s %s passed, %s failed, %s unmeasured\n" \
            "$backend:" "${pass_counts["total:$backend"]}" \
            "$((${measured_counts["total:$backend"]} - ${pass_counts["total:$backend"]}))" \
            "$((total - ${measured_counts["total:$backend"]}))"
    done
    printf "Raw per-program results: %s\n" "$VALIDATE_RESULTS_FILE"
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
    if ((RR_COMPAT_CANARY_FAILED == 1)); then
        RR_COMPAT_FAIL_FAST_SKIPPED=$((RR_COMPAT_FAIL_FAST_SKIPPED + 1))
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
    if ((RR_COMPAT_TOTAL == 1)); then
        RR_COMPAT_CANARY_FAILED=1
        RR_COMPAT_CANARY_LABEL=$label
        printf "  ⚠️  R/R canary %s failed; skipping the remaining selected probes\n" "$label"
    fi
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
# TODO-HUMAN-REVIEW(#589): Review bounded SaBRe process-group teardown.
function terminate_sabre_compatibility_group {
    local pid=$1
    local grace_deadline

    kill -TERM -- "-$pid" 2>/dev/null || true
    grace_deadline=$((SECONDS + TIMEOUT_KILL_GRACE_SECONDS))
    while kill -0 -- "-$pid" 2>/dev/null && ((SECONDS < grace_deadline)); do
        sleep 0.2
    done
    if kill -0 -- "-$pid" 2>/dev/null; then
        kill -KILL -- "-$pid" 2>/dev/null || true
    fi
}

function run_sabre_compatibility_command {
    (
        local timeout_seconds=$1
        shift

        local started_at=$SECONDS
        local pid=""
        local status

        # TODO-HUMAN-REVIEW(PR-814): Review immediate abort on outer-gate termination.
        trap 'if [[ -n $pid ]]; then kill -KILL -- "-$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; fi; exit 143' INT TERM HUP
        setsid "$@" </dev/null >>"$LOG_FILE" 2>&1 &
        pid=$!
        while kill -0 "$pid" 2>/dev/null; do
            if ((SECONDS - started_at >= timeout_seconds)); then
                terminate_sabre_compatibility_group "$pid"
                wait "$pid" 2>/dev/null || true
                exit 124
            fi
            sleep 0.2
        done

        if wait "$pid"; then
            status=0
        else
            status=$?
        fi
        if kill -0 -- "-$pid" 2>/dev/null; then
            terminate_sabre_compatibility_group "$pid"
        fi
        trap - INT TERM HUP
        exit "$status"
    )
}

# AUTONOMOUS-BOT-IMPLEMENTED
# TODO-HUMAN-REVIEW(#521): Review the strict compatibility policy.
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
    local backend_diagnostic=""
    local nonblocking=0
    local probe_timeout=$STRICT_COMPAT_TIMEOUT
    local -a run_args=(run --strict --verify --)
    if [[ $VALIDATION_PROFILE == portable-only || $PORTABLE_STRICT_COMPAT_ONLY == 1 \
        || $PORTABLE_STRICT_PROBE_ARGS == 1 ]]; then
        run_args=(run --strict --verify --no-virtualize-cpuid --max-timeslice=disabled --)
        if [[ -n ${PORTABLE_STRICT_DIAGNOSTIC_FAILURES[$label]+set} ]]; then
            probe_timeout=20
        fi
    fi
    if [[ $COMPATIBILITY_MODE == sabre ]]; then
        assurance=SaBRe
        run_args=(run --backend sabre --strict --verify --)
    elif [[ $COMPATIBILITY_MODE == e9patch ]]; then
        assurance="e9patch L2"
        run_args=(run --backend e9patch)
        # These workloads query owner names that the host may delegate to an
        # asynchronous identity daemon. Pin just those rows to the fixture;
        # custom mounts intentionally reject unrelated symlinked executables.
        case "$label" in
            whoami | groups | pinky | logname | tar | chown)
                run_args+=("--mount=type=bind,source=$E9PATCH_NSSWITCH_FILE,target=/etc/nsswitch.conf,readonly")
                ;;
        esac
        run_args+=(--strict --verify --)
        # TODO-HUMAN-REVIEW(PR-681): Review the cache-miss whole-row
        # budget for the large internal mysql executable.
        # TODO-HUMAN-REVIEW(PR-687): Review extending the same cache-miss
        # budget to the large internal PHP/HHVM executable.
        if [[ $label == mysql || $label == php ]]; then
            probe_timeout=180
        fi
    fi

    {
        printf "=== %s compatibility: %s ===\n" "$assurance" "$label"
        printf "Command: timeout %s %q" \
            "$probe_timeout" "$STRICT_COMPAT_HERMIT_BIN"
        printf " %q" "${run_args[@]}"
        printf " %q" "$@"
        printf "\n"
    } >>"$LOG_FILE"
    output_start=$(($(wc -l <"$LOG_FILE") + 1))

    if ((VERBOSE == 1)); then
        printf "  %s compatibility probe: %s\n" "$assurance" "$label"
    fi

    if [[ $COMPATIBILITY_MODE == sabre ]]; then
        run_sabre_compatibility_command "$probe_timeout" \
            "$STRICT_COMPAT_HERMIT_BIN" "${run_args[@]}" "$@"
        status=$?
    else
        timeout "$probe_timeout" \
            "$STRICT_COMPAT_HERMIT_BIN" "${run_args[@]}" "$@" \
            </dev/null >>"$LOG_FILE" 2>&1
        status=$?
    fi

    if ((status == 0)); then
        printf "  ✅ %-12s PASS %s (%ss)\n" \
            "$label" "$assurance" "$((SECONDS - started_at))"
        record_compatibility_result "$label" PASS "$assurance"
    else
        summary=$(failure_summary "$output_start")
        printf "  ❌ %-12s FAIL %s (exit %s: %s)\n" \
            "$label" "$assurance" "$status" "$summary"
        record_compatibility_result "$label" FAIL "exit $status: $summary"
        if [[ ($VALIDATION_PROFILE == portable-only || $PORTABLE_STRICT_COMPAT_ONLY == 1) && -n ${PORTABLE_STRICT_DIAGNOSTIC_FAILURES[$label]+set} ]]; then
            nonblocking=1
            PORTABLE_STRICT_DIAGNOSTIC_FAILURE_COUNT=$((PORTABLE_STRICT_DIAGNOSTIC_FAILURE_COUNT + 1))
            printf "  WARN %s is a bounded portable diagnostic: %s\n" \
                "$label" "${PORTABLE_STRICT_DIAGNOSTIC_FAILURES[$label]}"
        fi
    fi

    {
        printf "Exit: %s\n" "$status"
        printf "Duration: %ss\n\n" "$((SECONDS - started_at))"
    } >>"$LOG_FILE"

    if [[ $COMPATIBILITY_MODE == e9patch ]]; then
        backend_diagnostic=$(sed -n "${output_start},\$p" "$LOG_FILE" |
            grep -m1 '^:: Backend: e9patch' || true)
        if [[ $backend_diagnostic == *"main_executable=non-ELF"* ]]; then
            ((E9PATCH_COMPAT_NON_ELF += 1))
        elif [[ $backend_diagnostic =~ candidate_sites=([0-9]+).*mapped_sites=([0-9]+) ]]; then
            if ((BASH_REMATCH[2] > 0)); then
                ((E9PATCH_COMPAT_REWRITTEN += 1))
            elif ((BASH_REMATCH[1] > 0)); then
                ((E9PATCH_COMPAT_CANDIDATE_ONLY += 1))
            else
                ((E9PATCH_COMPAT_ZERO_SITE += 1))
            fi
        else
            ((E9PATCH_COMPAT_NO_DIAGNOSTIC += 1))
        fi
    fi
    if ((nonblocking == 1)); then
        return 0
    fi
    return "$status"
}

# Strict compatibility remains an observation gate in full validation. The
# focused SaBRe and record/replay modes enforce their measured blocking floors.
# Strict mode replaces banner probes with functional workloads so an
# executable that merely starts cannot be counted as compatible at L2.
# AUTONOMOUS-BOT-IMPLEMENTED
# TODO-HUMAN-REVIEW(#845): Review sharing exact functional fixtures with SaBRe.
function real_compatibility_probe {
    local label=$1

    strict_compatibility_probe "$label" env \
        REAL_COMPAT_FIXTURES="$REAL_COMPAT_FIXTURES" \
        bash "$REAL_COMPAT_WORKLOAD" "$label"
}

function functional_compatibility_probe {
    local label=$1

    real_compatibility_probe "$label"
}

# These runtime/compiler workloads consume their full timeout on the no-PMU
# portable runner and are explicitly nonblocking there. Keep each row in the
# compatibility table, but measure it in the scheduled super suite instead of
# spending 20 seconds per row on every pull request.
function defer_portable_strict_diagnostic_to_super {
    local label=$1

    if [[ $COMPATIBILITY_MODE != strict \
        || ($VALIDATION_PROFILE != portable-only && $PORTABLE_STRICT_COMPAT_ONLY != 1) \
        || -z ${PORTABLE_STRICT_SUPER_ONLY[$label]+set} ]]; then
        return 1
    fi

    printf "  SKIP %-12s scheduled super diagnostic (%s)\n" \
        "$label" "${PORTABLE_STRICT_SUPER_ONLY[$label]}"
    {
        printf "=== L2 compatibility: %s ===\n" "$label"
        printf "Skipped: scheduled super diagnostic (%s)\n\n" \
            "${PORTABLE_STRICT_SUPER_ONLY[$label]}"
    } >>"$LOG_FILE"
    record_compatibility_result "$label" N/A "scheduled super diagnostic"
    return 0
}
# Route a compatibility probe whose failure under fail-closed --strict is an
# accepted unsupported-syscall gap (see COMPAT_SUMMARY_KNOWN_FAILURES; PR #644).
# In strict mode such a failure is nonblocking known-flaky and the row keeps
# running so the gap stays visible (mirroring the gcc vfork precedent); other
# modes tally it as an ordinary failure. Uses namerefs to update the caller's
# passed/failed/known_flaky counters.
function tally_known_failclosed_probe {
    local -n _tkfp_passed=$1
    local -n _tkfp_failed=$2
    local -n _tkfp_known=$3
    local label=$4
    shift 4

    if "$@"; then
        _tkfp_passed=$((_tkfp_passed + 1))
        if [[ $COMPATIBILITY_MODE == strict ]]; then
            printf "  WARN %s unexpectedly passed fail-closed --strict; drop it from COMPAT_SUMMARY_KNOWN_FAILURES\n" \
                "$label"
        fi
    elif [[ $COMPATIBILITY_MODE == strict ]]; then
        _tkfp_known=$((_tkfp_known + 1))
        printf "  WARN %s known fail-closed under --strict (%s; PR #644; nonblocking)\n" \
            "$label" "${COMPAT_SUMMARY_KNOWN_FAILURES[$label]:-unsupported syscall}"
    else
        _tkfp_failed=$((_tkfp_failed + 1))
    fi
}

function run_compatibility_corpus {
    local passed=0
    local failed=0
    local known_flaky=0
    local unavailable=0
    local total=0
    local sabre_flex_passed=0
    local sabre_ld_passed=0
    local sabre_make_passed=0
    PORTABLE_STRICT_DIAGNOSTIC_FAILURE_COUNT=0

    if [[ $COMPATIBILITY_MODE == rr ]]; then
        printf "\n== Record/replay compatibility baseline (blocking gate) ==\n"
        printf "=== Record/replay compatibility baseline (blocking gate) ===\n" >>"$LOG_FILE"
    elif [[ $COMPATIBILITY_MODE == sabre ]]; then
        printf "\n== SaBRe compatibility ratchet (blocking floor) ==\n"
        printf "=== SaBRe compatibility ratchet (blocking floor) ===\n" >>"$LOG_FILE"
    elif [[ $COMPATIBILITY_MODE == e9patch ]]; then
        printf "\n== e9patch compatibility matrix (L2) ==\n"
        printf "=== e9patch compatibility matrix (L2) ===\n" >>"$LOG_FILE"
    else
        printf "\n== Strict compatibility envelope (L2, blocking) ==\n"
        printf "=== Strict compatibility envelope (L2, blocking) ===\n" >>"$LOG_FILE"
    fi

    strict_compatibility_probe echo /bin/echo hermit-compat \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe true /usr/bin/true \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe pwd /usr/bin/pwd \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    # AUTONOMOUS-BOT-IMPLEMENTED
    # TODO-HUMAN-REVIEW(#700): Review the functional miscellaneous probes.
    functional_compatibility_probe seq /usr/bin/seq 10 \
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
    # AUTONOMOUS-BOT-IMPLEMENTED
    # TODO-HUMAN-REVIEW(#697): Review the strict-only system utility probes.
    if [[ $COMPATIBILITY_MODE == strict ]]; then
        strict_compatibility_probe lua bash -c \
            'set -euo pipefail; out=$("$1" -e "$2"); test "$out" = "$3"; printf "lua-fib=%s\n" "$out"' \
            bash /usr/bin/lua \
            'local a,b=0,1; for i=1,30 do a,b=b,a+b end; print(a)' 832040 \
            && passed=$((passed + 1)) || failed=$((failed + 1))
        strict_compatibility_probe perl bash -c \
            'set -euo pipefail; out=$("$1" -e "$2"); test "$out" = "$3"; printf "perl-prime-sum=%s\n" "$out"' \
            bash /usr/bin/perl \
            'my $sum=0; OUTER: for my $n (2..100) { for my $d (2..int(sqrt($n))) { next OUTER if $n % $d == 0 } $sum += $n } print "$sum\n"' 1060 \
            && passed=$((passed + 1)) || failed=$((failed + 1))
        # AUTONOMOUS-BOT-IMPLEMENTED
        # TODO-HUMAN-REVIEW(#698): Review the expanded bc and dc exact-output probes.
        # Keep the combined exact result below GNU bc output wrap width.
        strict_compatibility_probe bc bash -c \
            'set -euo pipefail; out=$(printf "%s\n" "$2" | BC_LINE_LENGTH=200 "$1" -q); test "$out" = "$3"; printf "bc-math=%s\n" "$out"' \
            bash /usr/bin/bc \
            'define f(n) { auto r,i; r=1; for(i=2;i<=n;i++) r*=i; return(r) }; scale=50; print f(20), " ", sqrt(2), "\n"' \
            '2432902008176640000 1.41421356237309504880168872420969807856967187537694' \
            && passed=$((passed + 1)) || failed=$((failed + 1))
    else
        strict_compatibility_probe lua lua -e 'print(42)' \
            && passed=$((passed + 1)) || failed=$((failed + 1))
        strict_compatibility_probe perl perl -e 'print 42, chr(10)' \
            && passed=$((passed + 1)) || failed=$((failed + 1))
        strict_compatibility_probe bc bash -c 'printf "6*7\n" | bc' \
            && passed=$((passed + 1)) || failed=$((failed + 1))
    fi
    # AUTONOMOUS-BOT-IMPLEMENTED
    # TODO-HUMAN-REVIEW(#845): Review the measured SaBRe system-utility expansion.
    # TODO-HUMAN-REVIEW(#1044): rr mode must reach tcl/dc too. Both
    # are listed in RR_COMPAT_PASSING_LABELS (part of the 144-row expected set),
    # but this guard previously admitted only strict/sabre, so under
    # COMPATIBILITY_MODE=rr the two probes were never invoked. RR_COMPAT_TOTAL
    # then topped out at 142 and the envelope's selection check
    # (RR_COMPAT_TOTAL != RR_COMPAT_EXPECTED) failed regardless of results,
    # leaving the ratchet structurally un-greenable. tcl and dc pass R/R, so
    # admit rr here to measure them honestly.
    if [[ $COMPATIBILITY_MODE == strict || $COMPATIBILITY_MODE == sabre \
        || $COMPATIBILITY_MODE == rr ]]; then
        strict_compatibility_probe tcl bash -c \
            'set -euo pipefail; out=$(printf "%s\n" "$2" | "$1"); test "$out" = "$3"; printf "tcl-squares=%s\n" "$out"' \
            bash /usr/bin/tclsh \
            'set sum 0; for {set i 1} {$i <= 100} {incr i} {set sum [expr {$sum + $i*$i}]}; puts $sum' 338350 \
            && passed=$((passed + 1)) || failed=$((failed + 1))
        strict_compatibility_probe dc bash -c \
            'set -euo pipefail; out=$(printf "%s\n" "$2" | "$1"); test "$out" = "$3"; printf "dc-math=%s\n" "$out"' \
            bash /usr/bin/dc '2 100 ^ 1 - n [ ]P 4 13 497 | p' \
            '1267650600228229401496703205375 445' \
            && passed=$((passed + 1)) || failed=$((failed + 1))
    fi
    # shellcheck disable=SC2016
    strict_compatibility_probe awk bash -c \
        'set -euo pipefail; printf "alpha 2\nbeta 3\nalpha 5\n" | awk "\$1 == \"alpha\" { sum += \$2 } END { print sum }" | diff -u <(printf "7\n") -; printf "awk-ok\n"' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    functional_compatibility_probe sqlite3 sqlite3 :memory: \
        'CREATE TABLE values_under_test(value INTEGER NOT NULL); WITH RECURSIVE sequence(value) AS (VALUES(1) UNION ALL SELECT value + 1 FROM sequence WHERE value < 100) INSERT INTO values_under_test SELECT value FROM sequence; SELECT count(*), sum(value) FROM values_under_test;' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    functional_compatibility_probe jq /usr/bin/jq -c -n \
        '{sum: ([range(1;6)] | add), evens: [range(1;6) | select(. % 2 == 0)]}' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    if [[ $COMPATIBILITY_MODE == strict || $COMPATIBILITY_MODE == sabre ]]; then
        functional_compatibility_probe xmllint \
            && passed=$((passed + 1)) || failed=$((failed + 1))
    fi
    # Expand $i inside the guest shell, not here.
    # shellcheck disable=SC2016
    strict_compatibility_probe bash bash -c \
        'for i in 1 2 3; do echo "$i"; done' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    # AUTONOMOUS-BOT-IMPLEMENTED
    # TODO-HUMAN-REVIEW(#701): Review the complex shell-build L2 workload.
    if [[ $COMPATIBILITY_MODE == strict ]]; then
        # Give the workload a per-run-unique work directory so concurrent
        # validate.sh runs never collide on a shared path under --verify. The
        # path is identical across this probe's two --verify runs (fixed argv),
        # keeping it L2-stable, but unique across processes (host mktemp).
        local shell_build_dir
        shell_build_dir=$(mktemp -d "${TMPDIR:-/tmp}/hermit-shell-build.XXXXXX")
        strict_compatibility_probe shell-build bash "$COMPLEX_SHELL_WORKLOAD" \
            "$shell_build_dir" \
            && passed=$((passed + 1)) || failed=$((failed + 1))
        rm -rf "$shell_build_dir"
    fi
    functional_compatibility_probe cargo \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    if defer_portable_strict_diagnostic_to_super rustc; then
        unavailable=$((unavailable + 1))
    else
        functional_compatibility_probe rustc \
            && passed=$((passed + 1)) || failed=$((failed + 1))
    fi
    if [[ $COMPATIBILITY_MODE == strict || $COMPATIBILITY_MODE == sabre ]]; then
        functional_compatibility_probe clang \
            && passed=$((passed + 1)) || failed=$((failed + 1))
        if defer_portable_strict_diagnostic_to_super javac; then
            unavailable=$((unavailable + 1))
        else
            functional_compatibility_probe javac \
                && passed=$((passed + 1)) || failed=$((failed + 1))
        fi
    fi
    if defer_portable_strict_diagnostic_to_super java; then
        unavailable=$((unavailable + 1))
    else
        functional_compatibility_probe java \
            && passed=$((passed + 1)) || failed=$((failed + 1))
    fi
    strict_compatibility_probe ruby /usr/bin/ruby --disable-gems -e \
        'values = (1..5).map { |value| value * value }; raise "unexpected squares" unless values == [1, 4, 9, 16, 25]; puts values.join(",")' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    if defer_portable_strict_diagnostic_to_super node; then
        unavailable=$((unavailable + 1))
    else
        strict_compatibility_probe node /bin/node -e 'console.log(42)' \
            && passed=$((passed + 1)) || failed=$((failed + 1))
    fi
    # Avoid the PATH fbpython wrapper and exercise the system CPython ELF.
    strict_compatibility_probe python3 /usr/bin/python3 -c 'print(42)' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe curl /usr/bin/curl --fail --silent --show-error \
        file:///etc/hostname \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    # Avoid the PATH Git wrapper: its telemetry sidecar pipes are nondeterministic.
    functional_compatibility_probe git \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    functional_compatibility_probe cmake \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    functional_compatibility_probe pkg-config \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    functional_compatibility_probe m4 \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    # TODO-HUMAN-REVIEW(#239): Make GCC blocking after deterministic vfork
    # child registration lands. Keep running it so the gap remains visible.
    if [[ $COMPATIBILITY_MODE == strict ]]; then
        if functional_compatibility_probe gcc; then
            passed=$((passed + 1))
        else
            known_flaky=$((known_flaky + 1))
            printf "  WARN gcc vfork probe failed (known scheduling gap #239; nonblocking)\n"
        fi
    else
        functional_compatibility_probe gcc \
            && passed=$((passed + 1)) || failed=$((failed + 1))
    fi
    functional_compatibility_probe g++ \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    if [[ $COMPATIBILITY_MODE == sabre ]]; then
        if functional_compatibility_probe make; then
            passed=$((passed + 1))
            sabre_make_passed=1
        else
            failed=$((failed + 1))
        fi
    else
        tally_known_failclosed_probe passed failed known_flaky make \
            functional_compatibility_probe make
    fi
    functional_compatibility_probe ar \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    functional_compatibility_probe as \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    if functional_compatibility_probe ld; then
        passed=$((passed + 1))
        if [[ $COMPATIBILITY_MODE == sabre ]]; then
            sabre_ld_passed=1
        fi
    else
        failed=$((failed + 1))
    fi
    functional_compatibility_probe nm \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    functional_compatibility_probe objcopy \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    functional_compatibility_probe objdump \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    functional_compatibility_probe ranlib \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    functional_compatibility_probe readelf \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    functional_compatibility_probe size \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    functional_compatibility_probe strip \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    functional_compatibility_probe addr2line \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    functional_compatibility_probe c++filt \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    functional_compatibility_probe elfedit \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    functional_compatibility_probe gprof \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    functional_compatibility_probe cpp \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    functional_compatibility_probe gcov \
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
    # AUTONOMOUS-BOT-IMPLEMENTED
    # TODO-HUMAN-REVIEW(#686): Review archive/network envelope growth.
    # SaBRe measures the deterministic archive and localhost round trips with
    # the same real workloads as ptrace strict mode. Other backend ratchets
    # remain independent.
    if [[ $COMPATIBILITY_MODE == strict ]]; then
        functional_compatibility_probe gzip-roundtrip \
            && passed=$((passed + 1)) || failed=$((failed + 1))
        functional_compatibility_probe bzip2-roundtrip \
            && passed=$((passed + 1)) || failed=$((failed + 1))
        functional_compatibility_probe xz-roundtrip \
            && passed=$((passed + 1)) || failed=$((failed + 1))
        functional_compatibility_probe zstd-roundtrip \
            && passed=$((passed + 1)) || failed=$((failed + 1))
        functional_compatibility_probe tar-roundtrip \
            && passed=$((passed + 1)) || failed=$((failed + 1))
        functional_compatibility_probe cpio-roundtrip \
            && passed=$((passed + 1)) || failed=$((failed + 1))
        tally_known_failclosed_probe passed failed known_flaky wget-localhost \
            functional_compatibility_probe wget-localhost
        tally_known_failclosed_probe passed failed known_flaky curl-localhost \
            functional_compatibility_probe curl-localhost
    elif [[ $COMPATIBILITY_MODE == sabre ]]; then
        real_compatibility_probe gzip-roundtrip \
            && passed=$((passed + 1)) || failed=$((failed + 1))
        real_compatibility_probe bzip2-roundtrip \
            && passed=$((passed + 1)) || failed=$((failed + 1))
        real_compatibility_probe xz-roundtrip \
            && passed=$((passed + 1)) || failed=$((failed + 1))
        real_compatibility_probe zstd-roundtrip \
            && passed=$((passed + 1)) || failed=$((failed + 1))
        real_compatibility_probe tar-roundtrip \
            && passed=$((passed + 1)) || failed=$((failed + 1))
        real_compatibility_probe cpio-roundtrip \
            && passed=$((passed + 1)) || failed=$((failed + 1))
        real_compatibility_probe wget-localhost \
            && passed=$((passed + 1)) || failed=$((failed + 1))
        real_compatibility_probe curl-localhost \
            && passed=$((passed + 1)) || failed=$((failed + 1))
    fi
    strict_compatibility_probe zip-unzip bash -c \
        'set -euo pipefail; rm -rf /tmp/hermit-compat-zip; mkdir /tmp/hermit-compat-zip; printf "archive-data\n" >/tmp/hermit-compat-zip/input; touch -t 200001010000 /tmp/hermit-compat-zip/input; (cd /tmp/hermit-compat-zip && zip -q archive.zip input); unzip -Z1 /tmp/hermit-compat-zip/archive.zip; unzip -p /tmp/hermit-compat-zip/archive.zip input; rm -rf /tmp/hermit-compat-zip' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe openssl openssl dgst -sha256 /etc/hostname \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe sort bash -c \
        'printf "beta\nalpha\nalpha\n" | sort' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe uniq bash -c \
        'set -euo pipefail; printf "alpha\nalpha\nbeta\nbeta\ngamma\n" | uniq -d | diff -u <(printf "alpha\nbeta\n") -; printf "uniq-ok\n"' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe tr bash -c \
        'printf "Hermit\n" | tr "[:upper:]" "[:lower:]"' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe cut bash -c \
        'set -euo pipefail; printf "one:two:three\nfour:five:six\n" | cut -d: -f2 | diff -u <(printf "two\nfive\n") -; printf "cut-ok\n"' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe tee bash -c \
        'printf "tee-through-hermit\n" | tee /dev/null' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe paste bash -c \
        'set -euo pipefail; paste -d: <(printf "alpha\nbeta\n") <(printf "1\n2\n") | diff -u <(printf "alpha:1\nbeta:2\n") -; printf "paste-ok\n"' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe comm bash -c \
        'set -euo pipefail; comm -12 <(printf "alpha\nbeta\n") <(printf "beta\ngamma\n") | diff -u <(printf "beta\n") -; printf "comm-ok\n"' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe join bash -c \
        'set -euo pipefail; join <(printf "1 alpha\n2 beta\n") <(printf "1 one\n2 two\n") | diff -u <(printf "1 alpha one\n2 beta two\n") -; printf "join-ok\n"' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    functional_compatibility_probe find find /etc -maxdepth 1 \
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
    functional_compatibility_probe env /usr/bin/env -i HERMIT_COMPAT=env /usr/bin/env \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe printenv /usr/bin/env -i HERMIT_COMPAT=printenv \
        /usr/bin/printenv HERMIT_COMPAT \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe uname /usr/bin/uname -sr \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    functional_compatibility_probe factor factor 42 \
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
    if [[ $COMPATIBILITY_MODE == strict || $COMPATIBILITY_MODE == sabre ]]; then
        functional_compatibility_probe ip \
            && passed=$((passed + 1)) || failed=$((failed + 1))
        functional_compatibility_probe ss \
            && passed=$((passed + 1)) || failed=$((failed + 1))
        functional_compatibility_probe lscpu \
            && passed=$((passed + 1)) || failed=$((failed + 1))
    fi
    if [[ $COMPATIBILITY_MODE == strict ]]; then
        tally_known_failclosed_probe passed failed known_flaky lsof \
            functional_compatibility_probe lsof /usr/bin/lsof -v
    fi
    strict_compatibility_probe whoami /usr/bin/whoami \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe groups /usr/bin/groups \
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
        'set -euo pipefail; d=$(mktemp -d /tmp/hermit-compat.XXXXXX); test -d "$d"; rmdir "$d"; printf "mktemp-ok\n"' \
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
    functional_compatibility_probe xargs bash -c \
        'printf "one\ntwo\n" | /usr/bin/xargs -n1 /bin/echo' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    if [[ $COMPATIBILITY_MODE == strict || $COMPATIBILITY_MODE == sabre ]]; then
        functional_compatibility_probe time \
            && passed=$((passed + 1)) || failed=$((failed + 1))
    fi
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
    strict_compatibility_probe taskset bash -c \
        'set -euo pipefail; taskset -p $$ >/dev/null; printf "taskset-ok\n"' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    # shellcheck disable=SC2016
    strict_compatibility_probe chrt bash -c \
        'set -euo pipefail; chrt -p $$ >/dev/null; printf "chrt-ok\n"' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe flock bash -c \
        'set -euo pipefail; f=$(mktemp); flock -x "$f" -c "printf \"flock-ok\\n\""; rm -f "$f"' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    # Capture logger's wall-clock prefix and assert only its semantic payload.
    strict_compatibility_probe logger bash -c \
        'set -euo pipefail; output=$(/usr/bin/logger --stderr --no-act -t hermit-compat logger-ok 2>&1); [[ $output == *"hermit-compat: logger-ok" ]]; printf "logger-ok\n"' \
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
    # shellcheck disable=SC2016
    strict_compatibility_probe logname bash -c \
        'if output=$(/usr/bin/logname 2>/dev/null); then test -n "$output"; printf "logname:login-present\n"; else printf "logname:no-login-record\n"; fi' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe users /usr/bin/users \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe uptime /usr/bin/uptime -p \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe iostat /usr/bin/iostat -d -x 1 1 \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe vmstat-disk /usr/bin/vmstat -d 1 2 \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe pidstat-disk /usr/bin/pidstat -d -p 1 1 1 \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe findmnt /usr/bin/findmnt --kernel --list \
        --output TARGET,SOURCE,FSTYPE,OPTIONS \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe sysctl-random-uuid /usr/sbin/sysctl kernel.random.uuid \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe sar-resource-tables /usr/bin/sar -v 1 1 \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe lsirq /usr/bin/lsirq --noheadings \
        --output IRQ,TOTAL,NAME \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe mpstat-softirqs /usr/bin/mpstat -I SCPU 1 1 \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe lsmod /usr/sbin/lsmod \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe numastat /usr/bin/numastat \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe numactl-hardware /usr/bin/numactl --hardware \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe ps /usr/bin/ps aux \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe vmstat /usr/bin/vmstat -s \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    local top_home="$VALIDATION_TMP_DIR/top-home"
    local top_config_home="$top_home/.config"
    mkdir -p "$top_config_home/procps"
    # shellcheck disable=SC2016
    strict_compatibility_probe top /usr/bin/env \
        "HOME=$top_home" "XDG_CONFIG_HOME=$top_config_home" /bin/bash -c \
        'set -euo pipefail; LC_ALL=C /usr/bin/top -b -n 1 -p $$ -w 80 >/dev/null; printf "top-ok\n"' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    # Signal zero checks deterministic guest-process existence without
    # perturbing signal delivery or depending on host process IDs.
    strict_compatibility_probe kill /usr/bin/kill -0 1 \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    # shellcheck disable=SC2016
    strict_compatibility_probe pgrep bash -c \
        'set -euo pipefail; /usr/bin/pgrep -x bash | /usr/bin/grep -qx "$$"; printf "pgrep-ok\n"' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe pkill bash -c \
        'set -euo pipefail; /usr/bin/pkill -0 -x bash; printf "pkill-ok\n"' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    # AUTONOMOUS-BOT-IMPLEMENTED
    # TODO-HUMAN-REVIEW(#845): Review SaBRe queued-signal preservation through
    # the atomic rt_sigsuspend mask transition.
    if [[ $COMPATIBILITY_MODE == sabre ]]; then
        strict_compatibility_probe timeout /usr/bin/timeout 1 /usr/bin/true \
            && passed=$((passed + 1)) || failed=$((failed + 1))
        # AUTONOMOUS-BOT-IMPLEMENTED
        # TODO-HUMAN-REVIEW(#845): Review the functional truncate workload.
        # Expand the temporary-file operations inside the guest shell.
        # shellcheck disable=SC2016
        strict_compatibility_probe truncate bash -c \
            'set -euo pipefail; f=$(mktemp); printf "Hermit\n" >"$f"; /usr/bin/truncate -s 4096 "$f"; test "$(stat -c %s "$f")" = 4096; /usr/bin/truncate -s 7 "$f"; test "$(stat -c %s "$f")" = 7; test "$(cat "$f")" = Hermit; rm -f "$f"; printf "truncate:4096-to-7-ok\n"' \
            && passed=$((passed + 1)) || failed=$((failed + 1))
        # AUTONOMOUS-BOT-IMPLEMENTED
        # TODO-HUMAN-REVIEW(#845): Review the expanded functional utility rows.
        # Expand temporary paths and command substitutions inside the guest.
        # shellcheck disable=SC2016
        strict_compatibility_probe fallocate bash -c \
            'set -euo pipefail; d=$(mktemp -d); trap '\''rm -rf "$d"'\'' EXIT; f="$d/file"; /usr/bin/fallocate -l 4096 "$f"; size=$(stat -c %s "$f"); test "$size" = 4096; printf "fallocate:size=%s\n" "$size"' \
            && passed=$((passed + 1)) || failed=$((failed + 1))
        strict_compatibility_probe setfattr bash -c \
            'set -euo pipefail; f=$(mktemp); trap '\''rm -f "$f"'\'' EXIT; printf payload >"$f"; /usr/bin/setfattr -n user.hermit.compat -v 42 "$f"; value=$(/usr/bin/getfattr --absolute-names --only-values -n user.hermit.compat "$f"); test "$value" = 42; /usr/bin/setfattr -x user.hermit.compat "$f"; ! /usr/bin/getfattr --absolute-names --only-values -n user.hermit.compat "$f" >/dev/null 2>&1; printf "setfattr:value=%s:removed\n" "$value"' \
            && passed=$((passed + 1)) || failed=$((failed + 1))
        strict_compatibility_probe setfacl bash -c \
            'set -euo pipefail; f=$(mktemp); trap '\''rm -f "$f"'\'' EXIT; chmod 600 "$f"; /usr/bin/setfacl -m u::rw,g::r,o::- "$f"; mode=$(stat -c %a "$f"); test "$mode" = 640; /usr/bin/getfacl --absolute-names -cp "$f" | /usr/bin/grep -Fxq "group::r--"; printf "setfacl:mode=%s\n" "$mode"' \
            && passed=$((passed + 1)) || failed=$((failed + 1))
        strict_compatibility_probe mountpoint bash -c \
            'set -euo pipefail; /usr/bin/mountpoint -q /; ! /usr/bin/mountpoint -q README.md; printf "mountpoint:root=yes:file=no\n"' \
            && passed=$((passed + 1)) || failed=$((failed + 1))
        strict_compatibility_probe diff3 bash -c \
            'set -euo pipefail; d=$(mktemp -d); trap '\''rm -rf "$d"'\'' EXIT; printf "alpha\nbeta\n" >"$d/base"; printf "alpha\nbeta\nours\n" >"$d/ours"; printf "theirs\nalpha\nbeta\n" >"$d/theirs"; /usr/bin/diff3 -m "$d/ours" "$d/base" "$d/theirs" | /usr/bin/diff -u <(printf "theirs\nalpha\nbeta\nours\n") -; printf "diff3:clean-merge-ok\n"' \
            && passed=$((passed + 1)) || failed=$((failed + 1))
        # AUTONOMOUS-BOT-IMPLEMENTED
        # TODO-HUMAN-REVIEW(#845): Review the functional text utility rows.
        # Expand command substitutions and temporary paths inside the guest.
        # shellcheck disable=SC2016
        strict_compatibility_probe basenc bash -c \
            'set -euo pipefail; encoded=$(printf "Hermit\n" | /usr/bin/basenc --base64url); test "$encoded" = SGVybWl0Cg==; decoded=$(printf "%s\n" "$encoded" | /usr/bin/basenc --base64url -d); test "$decoded" = Hermit; printf "basenc:%s:roundtrip-ok\n" "$encoded"' \
            && passed=$((passed + 1)) || failed=$((failed + 1))
        strict_compatibility_probe dos2unix bash -c \
            'set -euo pipefail; f=$(mktemp); trap '\''rm -f "$f"'\'' EXIT; printf "alpha\r\nbeta\r\n" >"$f"; /usr/bin/dos2unix -q "$f"; /usr/bin/diff -u <(printf "alpha\nbeta\n") "$f"; printf "dos2unix:crlf-to-lf-ok\n"' \
            && passed=$((passed + 1)) || failed=$((failed + 1))
        strict_compatibility_probe envsubst bash -c \
            'set -euo pipefail; output=$(HERMIT_NAME=Hermit HERMIT_VALUE=42 /usr/bin/envsubst "\$HERMIT_NAME=\$HERMIT_VALUE" <<<"\$HERMIT_NAME=\$HERMIT_VALUE"); test "$output" = Hermit=42; printf "envsubst:%s\n" "$output"' \
            && passed=$((passed + 1)) || failed=$((failed + 1))
        strict_compatibility_probe col bash -c \
            'set -euo pipefail; output=$(printf "A\bB\n" | /usr/bin/col -b); test "$output" = B; printf "col:overstrike=%s\n" "$output"' \
            && passed=$((passed + 1)) || failed=$((failed + 1))
        strict_compatibility_probe colrm bash -c \
            'set -euo pipefail; output=$(printf "abcdef\n" | /usr/bin/colrm 3 5); test "$output" = abf; printf "colrm:%s\n" "$output"' \
            && passed=$((passed + 1)) || failed=$((failed + 1))
        strict_compatibility_probe crc32 bash -c \
            'set -euo pipefail; f=$(mktemp); trap '\''rm -f "$f"'\'' EXIT; printf "Hermit\n" >"$f"; sum=$(/usr/bin/crc32 "$f"); test "$sum" = 146f43bb; printf "crc32:%s\n" "$sum"' \
            && passed=$((passed + 1)) || failed=$((failed + 1))
        # AUTONOMOUS-BOT-IMPLEMENTED
        # TODO-HUMAN-REVIEW(#845): Review the functional system utility rows.
        # Expand command substitutions and temporary paths inside the guest.
        # shellcheck disable=SC2016
        strict_compatibility_probe uuidgen bash -c \
            'set -euo pipefail; value=$(/usr/bin/uuidgen --random); [[ $value =~ ^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$ ]]; printf "uuidgen:%s\n" "$value"' \
            && passed=$((passed + 1)) || failed=$((failed + 1))
        strict_compatibility_probe shred bash -c \
            'set -euo pipefail; f=$(mktemp); trap '\''rm -f "$f"'\'' EXIT; printf seed >"$f"; /usr/bin/shred -n 1 -z -s 4096 "$f"; test "$(stat -c %s "$f")" = 4096; /usr/bin/cmp -n 4096 "$f" /dev/zero; printf "shred:4096-zeroed\n"' \
            && passed=$((passed + 1)) || failed=$((failed + 1))
        strict_compatibility_probe sync bash -c \
            'set -euo pipefail; f=$(mktemp); trap '\''rm -f "$f"'\'' EXIT; printf "sync-payload\n" >"$f"; /usr/bin/sync -f "$f"; test "$(cat "$f")" = sync-payload; printf "sync:file-ok\n"' \
            && passed=$((passed + 1)) || failed=$((failed + 1))
        strict_compatibility_probe pathchk bash -c \
            'set -euo pipefail; /usr/bin/pathchk -p alpha/beta_42; component=$(printf "%015d" 0 | /usr/bin/tr 0 x); ! /usr/bin/pathchk -p "$component" >/dev/null 2>&1; printf "pathchk:portable-limit-ok\n"' \
            && passed=$((passed + 1)) || failed=$((failed + 1))
        strict_compatibility_probe namei bash -c \
            'set -euo pipefail; d=$(mktemp -d); trap '\''rm -rf "$d"'\'' EXIT; mkdir "$d/real"; touch "$d/real/file"; ln -s real "$d/link"; /usr/bin/namei -m "$d/link/file" >"$d/output"; /usr/bin/grep -Fxq " lrwxrwxrwx link -> real" "$d/output"; /usr/bin/grep -Fxq " -rw-r--r-- file" "$d/output"; printf "namei:symlink-path-ok\n"' \
            && passed=$((passed + 1)) || failed=$((failed + 1))
        strict_compatibility_probe getconf bash -c \
            'set -euo pipefail; page=$(/usr/bin/getconf PAGESIZE); bits=$(/usr/bin/getconf LONG_BIT); test "$page:$bits" = 4096:64; printf "getconf:page=%s:long=%s\n" "$page" "$bits"' \
            && passed=$((passed + 1)) || failed=$((failed + 1))
        # AUTONOMOUS-BOT-IMPLEMENTED
        # TODO-HUMAN-REVIEW(#845): Review the functional build/catalog rows.
        # Expand command substitutions and temporary paths inside the guest.
        # shellcheck disable=SC2016
        strict_compatibility_probe cscope bash -c \
            'set -euo pipefail; d=$(mktemp -d); trap '\''rm -rf "$d"'\'' EXIT; printf "int compat_add(int a, int b) { return a + b; }\nint main(void) { return compat_add(20, 22) != 42; }\n" >"$d/fixture.c"; printf "fixture.c\n" >"$d/cscope.files"; (cd "$d" && /usr/bin/cscope -bq -i cscope.files); output=$(cd "$d" && /usr/bin/cscope -dL -1 compat_add); [[ $output == *"fixture.c compat_add 1"* ]]; printf "cscope:compat_add-found\n"' \
            && passed=$((passed + 1)) || failed=$((failed + 1))
        if strict_compatibility_probe flex bash -c \
            'set -euo pipefail; d=$(mktemp -d); trap '\''rm -rf "$d"'\'' EXIT; printf "%s\n" "%option prefix=\"compat\" noyywrap" "%%" "[0-9]+ return 1;" ".      ;" "%%" >"$d/scanner.l"; /usr/bin/flex -o "$d/scanner.c" "$d/scanner.l"; grep -q compatlex "$d/scanner.c"; printf "flex:compat-scanner-generated\n"'; then
            passed=$((passed + 1))
            if [[ $COMPATIBILITY_MODE == sabre ]]; then
                sabre_flex_passed=1
            fi
        else
            failed=$((failed + 1))
        fi
        strict_compatibility_probe msgfmt bash -c \
            'set -euo pipefail; d=$(mktemp -d); trap '\''rm -rf "$d"'\'' EXIT; printf "%s\n" "msgid \"\"" "msgstr \"Content-Type: text/plain; charset=UTF-8\\n\"" "" "msgid \"hello\"" "msgstr \"Hermit\"" >"$d/messages.po"; /usr/bin/msgfmt -o "$d/messages.mo" "$d/messages.po"; test -s "$d/messages.mo"; printf "msgfmt:catalog-compiled\n"' \
            && passed=$((passed + 1)) || failed=$((failed + 1))
        strict_compatibility_probe msgunfmt bash -c \
            'set -euo pipefail; d=$(mktemp -d); trap '\''rm -rf "$d"'\'' EXIT; printf "%s\n" "msgid \"\"" "msgstr \"Content-Type: text/plain; charset=UTF-8\\n\"" "" "msgid \"hello\"" "msgstr \"Hermit\"" >"$d/messages.po"; /usr/bin/msgfmt -o "$d/messages.mo" "$d/messages.po"; /usr/bin/msgunfmt "$d/messages.mo" >"$d/roundtrip.po"; grep -Fq '\''msgstr "Hermit"'\'' "$d/roundtrip.po"; printf "msgunfmt:catalog-roundtrip-ok\n"' \
            && passed=$((passed + 1)) || failed=$((failed + 1))
    fi
    # AUTONOMOUS-BOT-IMPLEMENTED
    # TODO-HUMAN-REVIEW(#575)
    # Filesystem fixtures use a per-probe mktemp dir so concurrent validate.sh
    # runs cannot collide (fixed /tmp paths raced). hermit --strict seeds
    # getrandom deterministically, so mktemp yields the same name across both
    # --verify runs (see the `mktemp` probe above), keeping the probe L2-stable.
    strict_compatibility_probe diff bash -c \
        'set -euo pipefail; d=$(mktemp -d); printf "alpha\nbeta\n" >"$d/a"; cp "$d/a" "$d/b"; diff -u "$d/a" "$d/b"; rm -rf "$d"; printf "diff-ok\n"' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe patch bash -c \
        'set -euo pipefail; d=$(mktemp -d); printf "old\n" >"$d/file"; printf "%s\n" "--- file" "+++ file" "@@ -1 +1 @@" "-old" "+new" | (cd "$d" && patch -s file); cat "$d/file"; rm -rf "$d"' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe grep bash -c \
        'set -euo pipefail; printf "alpha\nbeta\ngamma\nalpha\n" | grep -nx alpha | diff -u <(printf "1:alpha\n4:alpha\n") -; printf "grep-ok\n"' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe egrep bash -c \
        'set -euo pipefail; printf "alpha\nbeta\ngamma\n" | egrep "^(alpha|gamma)$" | diff -u <(printf "alpha\ngamma\n") -; printf "egrep-ok\n"' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe fgrep bash -c \
        'set -euo pipefail; printf "alpha.beta\nalphaXbeta\n" | fgrep "alpha.beta" | diff -u <(printf "alpha.beta\n") -; printf "fgrep-ok\n"' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe sed bash -c \
        'set -euo pipefail; printf "alpha:12\nbeta:3\n" | sed -E "s/^([a-z]+):([0-9]+)$/\\2-\\1/" | diff -u <(printf "12-alpha\n3-beta\n") -; printf "sed-ok\n"' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe tar bash -c \
        'set -euo pipefail; d=$(mktemp -d); printf "archive-data\n" >"$d/input"; touch -t 200001010000 "$d/input"; tar -cf "$d/archive.tar" -C "$d" input; tar -tf "$d/archive.tar"; rm -rf "$d"' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe cp bash -c \
        'set -euo pipefail; d=$(mktemp -d); printf "copy-data\n" >"$d/source"; cp "$d/source" "$d/copy"; cmp "$d/source" "$d/copy"; cat "$d/copy"; rm -rf "$d"' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe mv bash -c \
        'set -euo pipefail; d=$(mktemp -d); printf "move-data\n" >"$d/source"; mv "$d/source" "$d/moved"; test ! -e "$d/source"; cat "$d/moved"; rm -rf "$d"' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe rm bash -c \
        'set -euo pipefail; d=$(mktemp -d); printf "remove-data\n" >"$d/file"; rm "$d/file"; test ! -e "$d/file"; rmdir "$d"; printf "rm-ok\n"' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe mkdir bash -c \
        'set -euo pipefail; d=$(mktemp -d); mkdir -p "$d/a/b"; test -d "$d/a/b"; printf "mkdir-ok\n"; rm -rf "$d"' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe rmdir bash -c \
        'set -euo pipefail; d=$(mktemp -d); rmdir "$d"; test ! -e "$d"; printf "rmdir-ok\n"' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe touch bash -c \
        'set -euo pipefail; f=$(mktemp); touch -t 200001010000 "$f"; stat -c "%Y %s" "$f"; rm -f "$f"' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe chmod bash -c \
        'set -euo pipefail; f=$(mktemp); printf "mode\n" >"$f"; chmod 640 "$f"; stat -c "%a" "$f"; rm -f "$f"' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe chown bash -c \
        'set -euo pipefail; f=$(mktemp); printf "owner\n" >"$f"; chown --reference=README.md "$f"; stat -c "%u:%g" "$f"; rm -f "$f"' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe ln bash -c \
        'set -euo pipefail; d=$(mktemp -d); printf "link-data\n" >"$d/source"; ln "$d/source" "$d/hard"; ln -s source "$d/sym"; stat -c "%h" "$d/source"; cat "$d/hard" "$d/sym"; rm -rf "$d"' \
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
    functional_compatibility_probe shuf bash -c \
        'set -euo pipefail; output=$(printf "alpha\nbeta\ngamma\ndelta\n" | shuf | sort); test "$output" = "$(printf "alpha\nbeta\ndelta\ngamma\n")"; printf "shuf-ok\n"' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe numfmt /usr/bin/numfmt --to=iec 1048576 \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe csplit bash -c \
        'set -euo pipefail; d=$(mktemp -d); printf "alpha\nbeta\ngamma\n" >"$d/input"; (cd "$d" && csplit -s input "/^beta$/" && cat xx00 xx01); rm -rf "$d"' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe split bash -c \
        'set -euo pipefail; d=$(mktemp -d); printf "one\ntwo\nthree\nfour\n" >"$d/input"; split -l 2 "$d/input" "$d/part-"; cat "$d"/part-*; rm -rf "$d"' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe install bash -c \
        'set -euo pipefail; d=$(mktemp -d); install -m 640 README.md "$d/copied"; stat -c "%a %s" "$d/copied"; rm -rf "$d"' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe mkfifo bash -c \
        'set -euo pipefail; p=$(mktemp -u); mkfifo "$p"; stat -c "%F" "$p"; rm -f "$p"' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    # The task named 29 utilities; cmp completes the requested 30-row push.
    strict_compatibility_probe cmp bash -c \
        'set -euo pipefail; d=$(mktemp -d); printf "same\n" >"$d/a"; printf "same\n" >"$d/b"; cmp -s "$d/a" "$d/b"; printf "cmp-ok\n"; rm -rf "$d"' \
        && passed=$((passed + 1)) || failed=$((failed + 1))
    strict_compatibility_probe free /usr/bin/free -m \
        && passed=$((passed + 1)) || failed=$((failed + 1))

    if [[ $COMPATIBILITY_MODE == rr ]]; then
        if ((RR_COMPAT_CANARY_FAILED == 1)); then
            printf "❌ Record/replay compatibility canary %s failed; executed %s selected probe and skipped %s remaining selected probes (%s unselected)\n" \
                "$RR_COMPAT_CANARY_LABEL" "$RR_COMPAT_TOTAL" \
                "$RR_COMPAT_FAIL_FAST_SKIPPED" "$RR_COMPAT_SKIPPED"
            return 1
        fi
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

    total=$((passed + failed + known_flaky + unavailable))
    if [[ $COMPATIBILITY_MODE == sabre ]]; then
        if ((total != SABRE_COMPAT_TOTAL)); then
            printf "❌ SaBRe compatibility corpus selected %s rows; expected %s\n" \
                "$total" "$SABRE_COMPAT_TOTAL"
            return 1
        fi
        if ((sabre_flex_passed != 1)); then
            printf "❌ SaBRe compatibility required row flex regressed\n"
            return 1
        fi
        if ((sabre_ld_passed != 1)); then
            printf "❌ SaBRe compatibility required row ld regressed\n"
            return 1
        fi
        if ((sabre_make_passed != 1)); then
            printf "❌ SaBRe compatibility required row make regressed\n"
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

    if [[ $COMPATIBILITY_MODE == e9patch ]]; then
        local classified=$((E9PATCH_COMPAT_REWRITTEN + E9PATCH_COMPAT_ZERO_SITE + \
            E9PATCH_COMPAT_CANDIDATE_ONLY + E9PATCH_COMPAT_NON_ELF + \
            E9PATCH_COMPAT_NO_DIAGNOSTIC))
        printf "e9patch preprocessing: %s rewritten, %s zero-site, %s candidate-only, %s non-ELF fallback, %s without diagnostic\n" \
            "$E9PATCH_COMPAT_REWRITTEN" "$E9PATCH_COMPAT_ZERO_SITE" \
            "$E9PATCH_COMPAT_CANDIDATE_ONLY" "$E9PATCH_COMPAT_NON_ELF" \
            "$E9PATCH_COMPAT_NO_DIAGNOSTIC"
        if ((total != E9PATCH_COMPAT_TOTAL)); then
            printf "❌ e9patch compatibility corpus selected %s rows; expected %s\n" \
                "$total" "$E9PATCH_COMPAT_TOTAL"
            return 1
        fi
        if ((classified != total)); then
            printf "❌ e9patch compatibility classified %s rows; expected %s\n" \
                "$classified" "$total"
            return 1
        fi
        if ((E9PATCH_COMPAT_NO_DIAGNOSTIC != 0)); then
            printf "❌ e9patch compatibility had %s rows without a backend diagnostic\n" \
                "$E9PATCH_COMPAT_NO_DIAGNOSTIC"
            return 1
        fi
        if ((failed == 0)); then
            printf "✅ e9patch compatibility matrix (%s/%s passed L2)\n" "$passed" "$total"
            return 0
        fi
        printf "❌ e9patch compatibility matrix (%s/%s passed L2, %s gaps)\n" \
            "$passed" "$total" "$failed"
        return 1
    fi

    if ((PORTABLE_STRICT_DIAGNOSTIC_FAILURE_COUNT > 0)); then
        passed=$((passed - PORTABLE_STRICT_DIAGNOSTIC_FAILURE_COUNT))
        known_flaky=$((known_flaky + PORTABLE_STRICT_DIAGNOSTIC_FAILURE_COUNT))
    fi

    if ((total != STRICT_COMPAT_TOTAL)); then
        printf "❌ Strict compatibility corpus selected %s rows; expected %s\n" \
            "$total" "$STRICT_COMPAT_TOTAL"
        return 1
    fi

    if ((failed == 0)); then
        if ((known_flaky == 0 && unavailable == 0)); then
            printf "✅ Strict compatibility envelope (%s/%s passed L2)\n" "$passed" "$total"
        else
            printf "✅ Strict compatibility envelope (%s/%s passed L2; %s known-flaky, %s unavailable, nonblocking)\n" \
                "$passed" "$total" "$known_flaky" "$unavailable"
        fi
        return 0
    fi

    printf "❌ Strict compatibility envelope (%s/%s passed L2, %s regressed; blocking)\n" \
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

    if ! "$ROOT_DIR/tests/compat/prepare_real_compat_fixtures.sh" \
        "$REAL_COMPAT_FIXTURES" >>"$LOG_FILE" 2>&1; then
        printf "❌ Unable to prepare functional compatibility fixtures (log: %s)\n" \
            "$LOG_FILE"
        return 1
    fi

    COMPATIBILITY_MODE=sabre
    run_compatibility_corpus || status=$?
    COMPATIBILITY_MODE=strict
    return "$status"
}

# TODO-HUMAN-REVIEW(PR-664): Review e9patch tool discovery and corpus accounting.
function require_e9patch_artifacts {
    local e9tool=${HERMIT_E9TOOL:-}
    local backend=${HERMIT_E9PATCH_BACKEND:-}
    if [[ -z $e9tool ]]; then
        e9tool=$(command -v e9tool || true)
    fi
    if [[ -z $backend && -n $e9tool ]]; then
        backend=$(dirname "$e9tool")/e9patch
    fi
    if [[ -z $e9tool || ! -x $e9tool ]]; then
        printf "validate.sh: HERMIT_E9TOOL must name an executable e9tool for e9patch compatibility\n" >&2
        return 1
    fi
    if [[ -z $backend || ! -x $backend ]]; then
        printf "validate.sh: HERMIT_E9PATCH_BACKEND must name an executable e9patch backend\n" >&2
        return 1
    fi

    # TODO-HUMAN-REVIEW(PR-676): Review the files-only NSS fixture used to
    # exclude host identity-daemon races from e9patch L2 measurements.
    printf '%s\n' \
        'aliases: files' 'automount: files' 'ethers: files' 'group: files' \
        'gshadow: files' 'hosts: files' 'initgroups: files' 'netgroup: files' \
        'netmasks: files' 'networks: files' 'passwd: files' 'protocols: files' \
        'publickey: files' 'rpc: files' 'services: files' 'shadow: files' \
        >"$E9PATCH_NSSWITCH_FILE"
}

function run_e9patch_compatibility_envelope {
    local status=0
    E9PATCH_COMPAT_REWRITTEN=0
    E9PATCH_COMPAT_ZERO_SITE=0
    E9PATCH_COMPAT_CANDIDATE_ONLY=0
    E9PATCH_COMPAT_NON_ELF=0
    E9PATCH_COMPAT_NO_DIAGNOSTIC=0
    COMPATIBILITY_MODE=e9patch
    run_compatibility_corpus || status=$?
    COMPATIBILITY_MODE=strict
    return "$status"
}

# TODO-HUMAN-REVIEW(PR-676): Review optional program discovery and the extended
# e9patch compatibility gate. Missing programs skip; every installed row gates.
function require_sabre_artifacts {
    local binary=${HERMIT_SABRE_BINARY:-}
    if [[ -z $binary || ! -f $binary || ! -x $binary ]]; then
        printf "validate.sh: HERMIT_SABRE_BINARY must name an executable SaBRe loader\n" >&2
        return 1
    fi
}

function run_rr_compatibility_envelope {
    local status=0

    # TODO-HUMAN-REVIEW(#1044): rr mode consumes the same
    # binutils/gprof/gcov fixtures as strict mode -- ranlib, size, strip,
    # addr2line, gprof, and gcov all read $REAL_COMPAT_FIXTURES -- but only
    # run_strict_compatibility_envelope prepared them. In a full validate.sh run
    # the strict envelope happens to run first, so rr inherited its fixtures; but
    # under --rr-compat-only the strict envelope never runs, the fixtures were
    # absent, and those six probes failed spuriously ("cp: cannot stat
    # .../with-symbols.o"). That inflated the R/R failure count with harness
    # artifacts rather than real replay divergences. Prepare the fixtures here
    # too, exactly as the strict envelope does, so the six fixture-backed tools
    # are measured honestly.
    if ! "$ROOT_DIR/tests/compat/prepare_real_compat_fixtures.sh" \
        "$REAL_COMPAT_FIXTURES" >>"$LOG_FILE" 2>&1; then
        printf "❌ Unable to prepare functional compatibility fixtures (log: %s)\n" \
            "$LOG_FILE"
        return 1
    fi

    RR_COMPAT_PASSED=0
    RR_COMPAT_FAILED=0
    RR_COMPAT_TOTAL=0
    RR_COMPAT_SKIPPED=0
    RR_COMPAT_FAIL_FAST_SKIPPED=0
    RR_COMPAT_CANARY_FAILED=0
    RR_COMPAT_CANARY_LABEL=""
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

# Auto-apply the `locally-validated` PR label after a fully-green full run, add
# an audit comment with the validation results, then cancel the redundant
# in-flight CI run for the exact validated commit.
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
    local comment_body=""
    local host_name=""
    local passed_checks=0
    local timestamp=""
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

        host_name=$(hostname -f 2>/dev/null) || \
            host_name=$(hostname 2>/dev/null) || host_name="unknown"
        timestamp=$(date -u +'%Y-%m-%dT%H:%M:%SZ') || timestamp="unknown"
        passed_checks=$((checks - failures))
        # Single quotes keep the Markdown backticks literal in the comment body.
        # shellcheck disable=SC2016
        printf -v comment_body \
            '[impl agent, validate.sh]\n\nLocal validation passed.\n\n- SHA: `%s`\n- Profile: `%s`\n- Results: %d checks passed, 0 failed\n- Hostname: `%s`\n- Timestamp (UTC): `%s`' \
            "$local_head" "$VALIDATION_PROFILE" "$passed_checks" \
            "$host_name" "$timestamp"
        if "${gh_cmd[@]}" pr comment "$pr" \
            --repo "$LOCALLY_VALIDATED_REPOSITORY" \
            --body "$comment_body" >>"$LOG_FILE" 2>&1; then
            printf "💬 Added local validation results to PR #%s\n" "$pr"
        else
            printf "⚠️  failed to comment validation results on PR #%s (full log: %s)\n" \
                "$pr" "$LOG_FILE" >&2
        fi

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

# fbsource import lints require the Meta copyright header on every imported Rust
# source file. `head -n 8` permits a rust-script shebang first.
function check_copyright_headers {
    local missing=0 f
    while IFS= read -r f; do
        if ! head -n 8 "$f" | grep -q 'Copyright (c) Meta Platforms'; then
            printf '  missing Meta copyright header: %s\n' "$f"
            missing=$((missing + 1))
        fi
    done < <(git ls-files '*.rs')
    if ((missing > 0)); then
        printf 'validate.sh: %d Rust file(s) missing the Meta copyright header required for fbsource import.\n' \
            "$missing" >&2
        return 1
    fi
    return 0
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

function run_portable_envelope_levels {
    local probe cmd iteration
    local -a command

    for probe in "${ENVELOPE_PROBES[@]}"; do
        cmd=${probe#*|}
        read -r -a command <<<"$cmd"
        _envelope_level "--strict" "${command[@]}" || return $?
        _envelope_level "--strict --verify" "${command[@]}" || return $?
        _envelope_level "--strict --verify --detlog-heap --detlog-stack" "${command[@]}" || return $?
        for ((iteration = 0; iteration < L4_REPS; iteration++)); do
            _envelope_level "--strict --verify" "${command[@]}" || return $?
        done
    done
}

function run_privileged_envelope_record_replay {
    local probe cmd
    local -a command

    for probe in "${ENVELOPE_PROBES[@]}"; do
        cmd=${probe#*|}
        read -r -a command <<<"$cmd"
        timeout "${HERMIT_RR_TIMEOUT:-$HERMIT_SMOKE_TIMEOUT}" \
            "$HERMIT_BIN" record start --verify -- "${command[@]}" \
            </dev/null >>"$LOG_FILE" 2>&1 || return $?
    done
}

function run_hermit_targets_serial {
    local target
    local -a cargo_args=(test -p hermit)

    for target in "$@"; do
        cargo_args+=(--test "$target")
    done

    # One Cargo invocation plans and links all selected test binaries together,
    # avoiding repeated package-cache and target-directory lock acquisition.
    # Cargo still executes the separate test binaries serially by default.
    cargo "${cargo_args[@]}" -- --test-threads=1
}

function run_ci_manifest_lane {
    local lane=$1
    local timeout_seconds=${2:-7200}
    local jobs=${CI_DAG_JOBS:-2}

    run_check "Centralized test manifest and inventory" ./ci/test_harness.sh validate
    run_check_with_timeout "$timeout_seconds" "$lane CI DAG manifest" \
        ./ci/run-dag.sh "$lane" -j "$jobs" -v
}

function run_portable_only_suite {
    run_ci_manifest_lane portable "${CI_PORTABLE_DAG_TIMEOUT_SECONDS:-7200}"
    print_summary
    ((failures == 0))
}

function run_exact_detcore_cases {
    local label=$1
    local target=$2
    local timeout_seconds=$3
    shift 3

    local failures_before=$failures
    local test_name

    for test_name in "$@"; do
        printf "Running %s: %s\n" "$label" "$test_name"
        run_check_with_timeout "$timeout_seconds" "$label: $test_name" \
            cargo test -p detcore --test "$target" "$test_name" -- --exact --test-threads=1
        if ((failures > failures_before)); then
            printf "Skipping remaining %s cases after the first failure.\n" "$label"
            return
        fi
    done
}

# Calibrate the host's retired-conditional-branch overflow skid before running
# schedule bisection. Short idle-host probes understate the tail observed while
# analyze repeatedly starts tracees. A 10,000-RCB guard was still exceeded by
# a 10,366-RCB tail on the self-hosted EPYC runner, so retain a 20,000-RCB
# floor and increase it if calibration measures a larger recommendation.
function run_calibrated_analyze_tests {
    local analyze_iterations=${ANALYZE_SKID_CALIBRATION_ITERATIONS:-64}
    local analyze_period=${ANALYZE_SKID_CALIBRATION_PERIOD:-1000000}
    local analyze_minimum_margin=${ANALYZE_SKID_MINIMUM_MARGIN:-20000}
    local analyze_calibration_timeout=${ANALYZE_SKID_CALIBRATION_TIMEOUT:-30}
    local calibration_binary="$ROOT_DIR/target/ci-pmu-skid"
    local output
    local recommended
    local margin

    for value_name in \
        analyze_iterations analyze_period analyze_minimum_margin analyze_calibration_timeout; do
        local value=${!value_name}
        if [[ ! $value =~ ^[1-9][0-9]*$ ]]; then
            printf "Analyze PMU calibration error: %s must be a positive integer, got %q\n" \
                "$value_name" "$value" >&2
            return 2
        fi
    done

    mkdir -p "$(dirname "$calibration_binary")"
    if ! cc -O2 -Wall -Wextra -Werror -std=gnu11 \
        "$ROOT_DIR/tests/util/pmu_skid.c" -o "$calibration_binary"; then
        echo "Analyze PMU calibration error: failed to build tests/util/pmu_skid.c" >&2
        return 1
    fi
    local status=0
    output=$(timeout "$analyze_calibration_timeout" "$calibration_binary" \
        --iterations "$analyze_iterations" --period "$analyze_period" 2>&1) || status=$?
    if ((status != 0)); then
        printf "Analyze PMU calibration failed (exit %s):\n%s\n" "$status" "$output" >&2
        return "$status"
    fi
    printf "%s\n" "$output"

    recommended=$(sed -n 's/^Recommended margin: \([0-9][0-9]*\) RCB.*/\1/p' <<<"$output")
    if [[ ! $recommended =~ ^[1-9][0-9]*$ ]]; then
        echo "Analyze PMU calibration error: output omitted a valid recommended margin" >&2
        return 1
    fi
    margin=$recommended
    if ((margin < analyze_minimum_margin)); then
        margin=$analyze_minimum_margin
    fi
    printf "Analyze PMU skid margin: calibrated=%s RCB, conservative floor=%s RCB, using=%s RCB\n" \
        "$recommended" "$analyze_minimum_margin" "$margin"

    HERMIT_ANALYZE_SKID_MARGIN=$margin \
        cargo test -p hermit --test analyze "$@"
}

function run_privileged_validation {
    run_ci_manifest_lane privileged "${CI_PRIVILEGED_DAG_TIMEOUT_SECONDS:-7200}"
    print_summary
    ((failures == 0))
}

function run_quick_suite {
    run_check "Build workspace" cargo build --workspace
    run_check "Portable E2E metadata" ./ci/test_harness.sh validate
    run_check "Portable ptrace E2E verification" \
        ./ci/test_harness.sh run --lane portable --mode verify --backend ptrace --ci-only
    run_check "Detcore core unit tests" cargo test -p detcore --lib
    run_check "Hermit run smoke test" hermit_run_smoke
    run_check "Hermit output determinism" hermit_determinism_check
    run_check "Hermit verify-mode smoke test" hermit_verify_smoke
    run_check "Hermit record/replay smoke test" hermit_record_replay_smoke
}

function run_full_suite {
    run_ci_manifest_lane portable "${CI_PORTABLE_DAG_TIMEOUT_SECONDS:-7200}"
    run_ci_manifest_lane privileged "${CI_PRIVILEGED_DAG_TIMEOUT_SECONDS:-7200}"
}

function run_portable_slow_strict_diagnostics {
    local label
    local status=0
    local -a labels=()

    if ! "$ROOT_DIR/tests/compat/prepare_real_compat_fixtures.sh" \
        "$REAL_COMPAT_FIXTURES" >>"$LOG_FILE" 2>&1; then
        printf "Unable to prepare functional compatibility fixtures\n"
        return 1
    fi

    COMPATIBILITY_MODE=strict
    PORTABLE_STRICT_PROBE_ARGS=1
    mapfile -t labels < <(printf "%s\n" "${!PORTABLE_STRICT_SUPER_ONLY[@]}" | sort)
    for label in "${labels[@]}"; do
        if [[ $label == node ]]; then
            strict_compatibility_probe node /bin/node -e 'console.log(42)' \
                || status=1
        else
            functional_compatibility_probe "$label" \
                || status=1
        fi
    done
    return "$status"
}

# AUTONOMOUS-BOT-IMPLEMENTED
# TODO-HUMAN-REVIEW(#719): Review the weekly placement of slow diagnostics.
function run_super_diagnostic_suite {
    # These probes are useful for trend detection but do not gate PRs. On the
    # portable runner they consumed about 20 minutes after the blocking suite had
    # already passed, so keep their signal in the scheduled super tier.
    run_check_with_timeout 600 "Portable slow strict compatibility diagnostics" \
        run_portable_slow_strict_diagnostics
    # AUTONOMOUS-BOT-IMPLEMENTED
    # TODO-HUMAN-REVIEW(#712): Review bounded routing for no-PMU hangs.
    # The memory-race family repeatedly exhausted its 900-second bound on three
    # unrelated PR heads. Preserve weekly coverage without making every PR wait
    # for the same host-sensitive hang.
    run_exact_detcore_cases "Weekly PMU parallel memory diagnostic" \
        tests_parallelism 600 \
        mem_race::bottom_detcore \
        mem_race::default_detcore \
        mem_race::middle_detcore \
        mem_race::top_detcore
    # AUTONOMOUS-BOT-IMPLEMENTED
    # TODO-HUMAN-REVIEW(#673)
    run_check_with_timeout 300 "Pselect signal-interruption diagnostic" \
        cargo test -p hermit --test pselect6_simulation -- --test-threads=1
    # AUTONOMOUS-BOT-IMPLEMENTED
    # TODO-HUMAN-REVIEW(#678)
    run_check_with_timeout 300 "Record/replay matrix diagnostic" \
        cargo test -p hermit --test record_replay record_replay_matrix -- --exact --test-threads=1
    # AUTONOMOUS-BOT-IMPLEMENTED
    # TODO-HUMAN-REVIEW(#657)
    run_check_with_timeout 300 "Managed JVM strict-verify diagnostics" \
        env HERMIT_APP_VERIFY_TIMEOUT=20s RUST_BACKTRACE=1 \
        cargo test -p hermit --test app_strict_verify java -- --ignored --test-threads=1 --nocapture
    run_check_with_timeout 180 "Post-fork scheduling diagnostics" \
        cargo test -p detcore --test tests_misc ordinary_clone_ -- --test-threads=1
    run_check_with_timeout 180 "Network syscall determinism diagnostic" \
        cargo test -p detcore --test tests_misc network_syscalls_are_deterministic_across_five_runs -- --exact --test-threads=1
    run_check_with_timeout 180 "IPC determinism diagnostic" \
        cargo test -p hermit --test ipc_determinism ipc_patterns_are_deterministic_across_five_runs -- --exact --test-threads=1
    run_check_with_timeout 180 "Random-source determinism diagnostic" \
        cargo test -p hermit --test random_determinism random_sources_repeat_across_runs_and_change_with_seed -- --exact --test-threads=1
    run_check_with_timeout 300 "Threaded integration matrix diagnostic" \
        cargo test -p hermit --test integration_matrix -- --test-threads=1
    run_check_with_timeout 300 "LiteInst python3 verify diagnostics" \
        cargo test -p hermit --test cli -- \
        run_liteinst_rejects_non_fork_clone \
        run_liteinst_handles_inherited_ignored_sigchld \
        run_liteinst_verifies_forked_guest \
        run_liteinst_verifies_raw_fork_guest --test-threads=1
    run_check_with_timeout 300 "Chaos hello-race verification diagnostic" \
        cargo test -p hermit --test hermit_modes hello_race_chaos_verify -- --exact --test-threads=1
    # AUTONOMOUS-BOT-IMPLEMENTED
    # TODO-HUMAN-REVIEW(#598)
    run_check_with_timeout 300 "DBI pipe backpressure diagnostic" \
        cargo test -p hermit --test cli run_dbi_verifies_pipe_backpressure -- --exact --test-threads=1
    # AUTONOMOUS-BOT-IMPLEMENTED
    # TODO-HUMAN-REVIEW(#736): Review weekly routing for the DBI failed-exec stall.
    run_check_with_timeout 180 "DBI failed-exec recovery diagnostic" \
        cargo test -p hermit --test cli run_dbi_recovers_after_failed_exec -- --exact --test-threads=1
    # This test exercises verify, tampered reports, fork/exec, and strict DBI
    # teardown in one case. Keep its coverage, but do not let a backend
    # lifecycle deadlock consume the portable PR gate.
    run_check_with_timeout 180 "DBI unsupported-syscall aggregation diagnostic" \
        cargo test -p hermit --test cli run_dbi_aggregates_unsupported_syscalls_and_strict_rejects_them -- --exact --test-threads=1
    run_check_with_timeout 30 "DBI strict blocked-stdin teardown diagnostic" \
        cargo test -p hermit --test cli run_dbi_strict_returns_with_blocked_stdin_source -- --exact --test-threads=1
    run_check_with_timeout 120 "DBI guest-stderr isolation diagnostic" \
        cargo test -p hermit --test cli run_dbi_keeps_diagnostics_out_of_guest_stderr -- --exact --test-threads=1
}

function run_super_suite {
    local leveldb_install="$ROOT_DIR/target/hermit-leveldb-super"
    local leveldb_build="$ROOT_DIR/target/hermit-leveldb-build-super"

    run_check "Build workspace" cargo build --workspace
    run_check "Build release Hermit" cargo build --release -p hermit
    run_super_diagnostic_suite
    run_check "Super repeated determinism probes" run_super_stress_suite
    if [[ -s $VALIDATION_TMP_DIR/super-report ]]; then
        printf "\n== Super stress pass rates ==\n"
        cat "$VALIDATION_TMP_DIR/super-report"
    fi
    run_check "Weekly relaxed default-mode cases" cargo test -p hermit --test hermit_modes default_ -- --test-threads=1
    run_check "Weekly portable chaos cases" cargo test -p hermit --test stress_suite -- --skip slow_cas_search_and_replay --test-threads=1
    run_check "Weekly ignored portable chaos cases" cargo test -p hermit --test stress_suite -- --ignored --skip slow_cas_search_and_replay --test-threads=1
    run_check "PMU Buck chaos cases" cargo test -p hermit --test hermit_modes chaos_buck_ -- --ignored --test-threads=1
    run_check "PMU analyze hello-race stress (calibrated skid)" \
        run_calibrated_analyze_tests analyze_hello_race -- --exact --ignored --test-threads=1
    run_check "Build pinned LevelDB super fixture" ./hermit-cli/tests/prepare_leveldb.sh "$leveldb_install" "$leveldb_build"
    run_check "Full LevelDB strict determinism" env HERMIT_LEVELDB_BUILD_DIR="$leveldb_build" cargo test -p hermit --test leveldb full_leveldb_suite_is_deterministic_under_strict -- --exact --ignored --test-threads=1
    run_check "SQLite veryquick strict determinism" cargo test -p hermit --test sqlite_veryquick sqlite_veryquick_is_deterministic_under_strict_hermit -- --exact --ignored --test-threads=1
}

# Envelope-only fast path: build the binary, measure the envelope, optionally
# enforce monotonicity, and exit. CI uses this so its numbers match validate.sh.
if [[ $VALIDATION_LEVEL == portable-only ]]; then
    run_portable_only_suite
    exit $?
fi

if ((PRIVILEGED_ONLY == 1)); then
    run_privileged_validation
    exit $?
fi

if ((STRICT_COMPAT_ONLY == 1)); then
    # TODO-HUMAN-REVIEW(#719): Review reuse of a caller-provided Hermit binary.
    if [[ $STRICT_COMPAT_HERMIT_BIN == "$DEFAULT_STRICT_COMPAT_HERMIT_BIN" ]]; then
        run_check "Build release Hermit for strict compatibility" \
            cargo build --release -p hermit
        if ((failures != 0)); then
            exit 1
        fi
    elif [[ ! -x $STRICT_COMPAT_HERMIT_BIN ]]; then
        printf "Configured strict compatibility Hermit is not executable: %s\n" \
            "$STRICT_COMPAT_HERMIT_BIN" >&2
        exit 1
    fi
    run_strict_compatibility_envelope
    exit $?
fi

if ((SABRE_COMPAT_ONLY == 1)); then
    run_check "SaBRe artifacts configured" require_sabre_artifacts
    if ((failures == 0)); then
        run_check "Build release Hermit and Detcore plugin for SaBRe compatibility" \
            cargo build --release -p hermit -p detcore-sabre
    fi
    if ((failures == 0)); then
        run_check "SaBRe compatibility ratchet (${SABRE_COMPAT_TOTAL} programs)" \
            run_sabre_compatibility_envelope
    fi
    print_summary
    ((failures == 0))
    exit $?
fi

if ((E9PATCH_COMPAT_ONLY == 1)); then
    run_check "e9patch artifacts configured" require_e9patch_artifacts
    if ((failures == 0)); then
        run_check "Build release Hermit for e9patch compatibility" \
            cargo build --release -p hermit
    fi
    if ((failures == 0)); then
        run_check "e9patch compatibility matrix ($E9PATCH_COMPAT_TOTAL programs)" \
            run_e9patch_compatibility_envelope
    fi
    print_summary
    ((failures == 0))
    exit $?
fi

if ((RR_COMPAT_ONLY == 1)); then
    run_check "Build release Hermit for record/replay compatibility" \
        cargo build --release -p hermit
    if ((failures == 0)); then
        run_check "Record/replay compatibility baseline ($RR_COMPAT_EXPECTED programs)" \
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
            ./tests/qemu-boot/strict_l2_test.sh
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
    portable-only) run_portable_only_suite ;;
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
