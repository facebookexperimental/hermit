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

function find_dev_hermit_parent {
    local candidate=$ROOT_DIR
    local hermit_path

    while [[ $candidate != / ]]; do
        if [[ -f $candidate/.gitmodules ]]; then
            hermit_path=$(git -C "$candidate" config -f .gitmodules \
                --get submodule.hermit.path 2>/dev/null || true)
            if [[ $hermit_path == hermit ]]; then
                printf "%s\n" "$candidate"
                return 0
            fi
        fi
        candidate=$(dirname -- "$candidate")
    done
    return 1
}

function validation_slot_name {
    local parent=$1
    local relative

    if [[ -z $parent ]]; then
        printf "standalone\n"
        return
    fi
    relative=${ROOT_DIR#"$parent"/}
    case "$relative" in
        hermit) printf "primary\n" ;;
        worktrees/*/hermit)
            relative=${relative#worktrees/}
            printf "%s\n" "${relative%%/*}"
            ;;
        *) printf "standalone\n" ;;
    esac
}

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
#   ./validate.sh --liteinst-compat-only      # run the portable CI liteinst_strict test
#   ./validate.sh --qemu-l2-only              # run the heavyweight QEMU L2 boot
#   ./validate.sh --portable-only               # no PMU/CPUID hardware required
#   ./validate.sh --privileged-only             # PMU/CPUID-dependent tests only
#   ./validate.sh --only <lane> <group.job>[,...] # run ONE DAG shard, no deps,
#                                            # skipping the full harness — fast
#                                            # local iteration on a single failing
#                                            # shard (e.g. --only portable
#                                            # test.sabre_examples). Build the tree
#                                            # first (ci/run-dag.sh <lane>).
#   ./validate.sh --verbose                  # stream each gate's command, PID,
#                                            # elapsed time, and subprocess output
# Every foreground/background gate has a process-tree WALL timeout. Override the
# profile default with VALIDATE_GATE_TIMEOUT_SECONDS; tune TERM-to-KILL grace
# with VALIDATE_TIMEOUT_KILL_GRACE_SECONDS. A gate may ALSO be given a
# load-immune CPU-time budget with VALIDATE_GATE_CPU_TIMEOUT_SECONDS (0=off, the
# default): the gate is killed once its process-tree CPU (user+sys) crosses the
# budget, catching hangs that burn CPU (e.g. a reap/futex spin) regardless of
# machine load, while the wall timeout remains the backstop for idle-stuck gates.
# After a fully-green full run writes its counted ledger row, the parent ci-hub
# publisher binds `locally-validated` to an immutable receipt. PR_NUMBER=N
# overrides branch-based PR detection. Use --no-label-pr or VALIDATE_LABEL_PR=0
# to disable that non-fatal publication.
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
LITEINST_COMPAT_ONLY=0
QEMU_L2_ONLY=0
PRIVILEGED_ONLY=0
ONLY_MODE=0
ONLY_LANE=""
ONLY_NODES=""
SELECTIVE_MODE=0
SELECTIVE_BASELINE=""
# --shallow-select: force the selective baseline to HEAD~1 (footprint of the most
# recent commit only, trusting the parent as green). Additive over --selective.
SHALLOW_SELECT=0
# --all / --full-run / VALIDATE_FORCE_FULL=1: assert the COMPLETE suite. A no-op
# on top of the default full level today (plain ./validate.sh is already full),
# but an explicit, recordable force-everything intent that rejects every non-full
# level and every focused/selective mode — and a forward guard should the default
# ever flip to smart selection.
FORCE_FULL=0
[[ ${VALIDATE_FORCE_FULL:-0} == 1 ]] && FORCE_FULL=1
RUN_ON_DIRTY_TREE=0
[[ ${VALIDATE_RUN_ON_DIRTY_TREE:-0} == 1 ]] && RUN_ON_DIRTY_TREE=1
IGNORE_CACHE=0
[[ ${VALIDATE_IGNORE_CACHE:-0} == 1 ]] && IGNORE_CACHE=1
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
        --liteinst-compat-only) LITEINST_COMPAT_ONLY=1; shift ;;
        --qemu-l2-only) QEMU_L2_ONLY=1; shift ;;
        --privileged-only) PRIVILEGED_ONLY=1; shift ;;
        --selective|--since-green) SELECTIVE_MODE=1; shift ;;
        --shallow-select) SELECTIVE_MODE=1; SHALLOW_SELECT=1; shift ;;
        --all|--full-run) FORCE_FULL=1; shift ;;
        --baseline)
            SELECTIVE_BASELINE=${2:-}
            [[ -n $SELECTIVE_BASELINE ]] || { echo "validate.sh: --baseline needs a SHA" >&2; exit 2; }
            shift 2 ;;
        --only)
            ONLY_LANE=${2:-}; ONLY_NODES=${3:-}
            if [[ -z $ONLY_LANE || -z $ONLY_NODES ]]; then
                echo "validate.sh: --only needs <lane> <group.job>[,<group.job>...]" >&2
                echo "             e.g. ./validate.sh --only portable test.sabre_examples" >&2
                exit 2
            fi
            ONLY_MODE=1; shift 3 ;;
        --run-on-dirty-tree) RUN_ON_DIRTY_TREE=1; shift ;;
        --ignore-cache) IGNORE_CACHE=1; shift ;;
        --label-pr) LABEL_PR=1; shift ;;
        --verbose) VERBOSE=1; shift ;;
        --no-label-pr) LABEL_PR=0; shift ;;
        -h|--help)
            cat <<'USAGE'
Usage: ./validate.sh [LEVEL] [OPTIONS]

Run Hermit's local validation suite. With no LEVEL, runs the full suite and
prints the working-envelope vector at the end.

Levels:
  quick            Core ptrace run/verify/record smoke tests; no alternate backends.
  portable-only    Portable build, test, lint, format, and doc gates matching
                   GitHub-managed portable CI; no PMU or namespace requirements.
  full             quick plus the complete suite and DBI/KVM gates (default).
  super            Repeat stress probes (20x by default) under moderate
                   oversubscription; report a pass rate per probe.
  --quick          Alias for the quick level.
  --portable       Alias for the portable-only level.

Focused gates (run one matrix/lane and exit):
  --envelope-only               Measure and emit the working-envelope vector (JSON + human).
  --envelope-compare FILE       Measure, then fail if any count regressed below FILE's baseline.
  --strict-compat-only          Run the blocking L2 app matrix.
  --portable-strict-compat-only Portable L2 matrix with bounded diagnostics.
  --rr-compat-only              Gate the known-passing record/replay matrix.
  --sabre-compat-only           Gate the measured SaBRe matrix (needs HERMIT_SABRE_BINARY).
  --e9patch-compat-only         Gate core + installed e9patch L2 apps.
  --liteinst-compat-only        Run the portable CI liteinst_strict test.
  --qemu-l2-only                Run the heavyweight QEMU L2 boot.
  --portable-only               No PMU/CPUID hardware required.
  --privileged-only             PMU/CPUID-dependent tests only.
  --only <lane> <group.job>[,...]  Run ONE DAG shard (no deps) against the already-built
                                tree; build first with ci/run-dag.sh <lane>.
  --selective, --since-green    Run only the portable DAG nodes affected by changes
                                since the last known-green baseline (fail-safe: any
                                doubt or no trustworthy baseline runs the full lane).
  --shallow-select              Like --selective but pin the baseline to HEAD~1:
                                validate only the footprint of the most recent
                                commit, trusting its parent as green. Upper-bound
                                reduction; still fail-safe (unknown path/no HEAD~1
                                runs the full lane). Cannot be combined with --baseline.
  --baseline <sha>              Known-green baseline commit for --selective (else
                                $HERMIT_LAST_GREEN_SHA, else the ledger's last green).
  --all, --full-run             Assert the COMPLETE suite explicitly. Refuses to be
                                combined with a non-full level or any focused/selective
                                mode. (Equivalent to VALIDATE_FORCE_FULL=1.)

Other options:
  --verbose        Stream each gate's command, PID, elapsed time, and output.
  --run-on-dirty-tree  Escape hatch: run despite uncommitted changes. AGENTS SHOULD
                   NOT USE THIS. By default a dirty working tree is a hard error,
                   because a result validated against uncommitted changes describes
                   a tree that exists nowhere in history and cannot be reproduced
                   or compared. A run forced with this flag is recorded as
                   NOT-commit-anchored (commit_anchored=false) and never applies
                   the `locally-validated` label.
  --label-pr       Publish a receipt and label the PR after a full green (default).
  --no-label-pr    Disable the non-fatal receipt publication and label update.
  --ignore-cache   Force a real run even when the run-ledger already holds a clean
                   PASS for this exact TREE (commit content, submodule pins) on
                   this host+toolchain. By default such a run is announced as a
                   CACHE HIT and skipped -- the fastest validate is the one you do
                   not run. The key is the tree, not the commit SHA, so a rebase
                   or amend that leaves file content identical still hits.
  -h, --help       Show this help and exit.

Environment:
  VALIDATE_LEVEL=quick|portable-only|full|super  Select the level.
  VALIDATE_GATE_TIMEOUT_SECONDS=N                Override per-gate process-tree WALL timeout.
  VALIDATE_GATE_CPU_TIMEOUT_SECONDS=N            Per-gate CPU-time budget (user+sys, whole tree); 0=off (default).
  VALIDATE_TIMEOUT_KILL_GRACE_SECONDS=N          TERM-to-KILL grace period.
  VALIDATE_LABEL_PR=0                            Disable receipt publication/labeling.
  CI_HUB_APPLY_LOCAL_LABEL=PATH                  Override the parent ci-hub receipt publisher.
  VALIDATE_VERBOSE=1                             Same as --verbose.
  VALIDATE_RUN_ON_DIRTY_TREE=1                   Same as --run-on-dirty-tree (agents: do not use).
  VALIDATE_IGNORE_CACHE=1                        Same as --ignore-cache (force a real run).
  HERMIT_VALIDATE_LEDGER=FILE                    Override the parent JSONL ledger path.
  PR_NUMBER=N                                    Override branch-based PR detection.

Examples:
  ./validate.sh                    # full suite + working-envelope vector
  ./validate.sh quick              # fast ptrace smoke tests
  ./validate.sh --portable         # portable CI-equivalent gates
  ./validate.sh --only portable test.sabre_examples
  ./validate.sh --envelope-compare envelope.json
USAGE
            exit 0 ;;
        *) echo "validate.sh: unknown argument: $1 (try --help)" >&2; exit 2 ;;
    esac
done

function force_full_policy_allows {
    local force_full=$1 level=$2 focused_mode=$3
    ((force_full == 0)) || [[ $level == full && -z $focused_mode ]]
}

# Exercise the exact predicate used below. These inert brackets cannot launch a
# validation run or authorize a receipt: they only prove that every selectable
# non-full level and every focused/selective CLI mode is refused under FORCE_FULL,
# while the one qualifying full/unfocused case is accepted.
function force_full_policy_self_test {
    local level mode
    local -a non_full_levels=(quick portable-only super)
    local -a focused_modes=(
        envelope-only envelope-compare strict-compat-only
        portable-strict-compat-only rr-compat-only sabre-compat-only
        e9patch-compat-only liteinst-compat-only qemu-l2-only privileged-only
        only selective shallow-select
    )

    force_full_policy_allows 1 full "" || return 1
    force_full_policy_allows 0 quick rr-compat-only || return 1
    for level in "${non_full_levels[@]}"; do
        ! force_full_policy_allows 1 "$level" "" || return 1
    done
    for mode in "${focused_modes[@]}"; do
        ! force_full_policy_allows 1 full "$mode" || return 1
    done
}

# AUTONOMOUS-BOT-IMPLEMENTED
# TODO-HUMAN-REVIEW(#553)
declare -a active_focused_modes=()
if [[ $ENVELOPE_MODE == only ]]; then
    if [[ -n $ENVELOPE_BASELINE ]]; then
        active_focused_modes+=(envelope-compare)
    else
        active_focused_modes+=(envelope-only)
    fi
fi
if ((STRICT_COMPAT_ONLY == 1)); then
    if ((PORTABLE_STRICT_COMPAT_ONLY == 1)); then
        active_focused_modes+=(portable-strict-compat-only)
    else
        active_focused_modes+=(strict-compat-only)
    fi
fi
((RR_COMPAT_ONLY == 1)) && active_focused_modes+=(rr-compat-only)
((SABRE_COMPAT_ONLY == 1)) && active_focused_modes+=(sabre-compat-only)
((E9PATCH_COMPAT_ONLY == 1)) && active_focused_modes+=(e9patch-compat-only)
((LITEINST_COMPAT_ONLY == 1)) && active_focused_modes+=(liteinst-compat-only)
((QEMU_L2_ONLY == 1)) && active_focused_modes+=(qemu-l2-only)
((PRIVILEGED_ONLY == 1)) && active_focused_modes+=(privileged-only)
((ONLY_MODE == 1)) && active_focused_modes+=(only)
if ((SELECTIVE_MODE == 1)); then
    if ((SHALLOW_SELECT == 1)); then
        active_focused_modes+=(shallow-select)
    else
        active_focused_modes+=(selective)
    fi
fi
only_modes=${#active_focused_modes[@]}
if ((only_modes > 1)); then
    echo "validate.sh: choose only one focused validation mode" >&2
    exit 2
fi
if ((VALIDATION_LEVEL_EXPLICIT == 1 && only_modes > 0)); then
    echo "validate.sh: validation levels cannot be combined with focused validation modes" >&2
    exit 2
fi
if ! force_full_policy_self_test; then
    echo "validate.sh: internal force-full policy brackets failed" >&2
    exit 2
fi
if ! force_full_policy_allows "$FORCE_FULL" "$VALIDATION_LEVEL" \
    "${active_focused_modes[0]:-}"; then
    echo "validate.sh: --all/--full-run requires level full and forbids every focused or selective mode" >&2
    exit 2
fi
if ((SHALLOW_SELECT == 1)) && [[ -n $SELECTIVE_BASELINE ]]; then
    echo "validate.sh: --shallow-select forces a HEAD~1 baseline; do not also pass --baseline" >&2
    exit 2
fi
VALIDATION_PROFILE=$VALIDATION_LEVEL
[[ $ENVELOPE_MODE == only ]] && VALIDATION_PROFILE="envelope-only"
((STRICT_COMPAT_ONLY == 1)) && VALIDATION_PROFILE="strict-compat-only"
((PORTABLE_STRICT_COMPAT_ONLY == 1)) && VALIDATION_PROFILE="portable-strict-compat-only"
((RR_COMPAT_ONLY == 1)) && VALIDATION_PROFILE="rr-compat-only"
((SABRE_COMPAT_ONLY == 1)) && VALIDATION_PROFILE="sabre-compat-only"
((E9PATCH_COMPAT_ONLY == 1)) && VALIDATION_PROFILE="e9patch-compat-only"
((LITEINST_COMPAT_ONLY == 1)) && VALIDATION_PROFILE="liteinst-compat-only"
((QEMU_L2_ONLY == 1)) && VALIDATION_PROFILE="qemu-l2-only"
((PRIVILEGED_ONLY == 1)) && VALIDATION_PROFILE="privileged-only"
((ONLY_MODE == 1)) && VALIDATION_PROFILE="only-$ONLY_LANE"
((SELECTIVE_MODE == 1)) && VALIDATION_PROFILE="selective"

# The runtime estimate is NOT a hand-written static guess anymore (a fabricated
# range is exactly the cost-blindness we want to avoid). It is measured from this
# machine's own validate-run history for the SAME profile and the SAME build-cache
# state (warm/cold target/ dominates wall time), computed at banner time by
# history_estimate below. When history is too thin the banner says so honestly
# instead of printing an invented range.

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
# Per-gate CPU-time budget (user+sys across the whole process tree), a load-immune
# companion to the wall timeout above. Default 0 = disabled: no per-gate CPU
# budget is HAND-WRITTEN here because there is no measured per-gate CPU history to
# justify a specific value, and a fabricated constant is exactly the cost-blindness
# this file avoids elsewhere. The mechanism ships enabled-by-opt-in; a data-derived
# default (round(max_cpu*1.5), >=5 samples) can be set once the ledger accumulates
# per-gate CPU history, mirroring the DAG-node cpu_timeout derivation.
default_gate_cpu_timeout_seconds=0
GATE_CPU_TIMEOUT_SECONDS=${VALIDATE_GATE_CPU_TIMEOUT_SECONDS:-$default_gate_cpu_timeout_seconds}
if [[ ! $GATE_TIMEOUT_SECONDS =~ ^[1-9][0-9]*$ ]]; then
    echo "validate.sh: VALIDATE_GATE_TIMEOUT_SECONDS must be a positive integer" >&2
    exit 2
fi
if [[ ! $GATE_CPU_TIMEOUT_SECONDS =~ ^[0-9]+$ ]]; then
    echo "validate.sh: VALIDATE_GATE_CPU_TIMEOUT_SECONDS must be a non-negative integer (0 disables)" >&2
    exit 2
fi
# Clock ticks per second, needed to convert /proc/<pid>/stat utime+stime into
# seconds. Cached once; falls back to the near-universal 100 if getconf is absent.
CLK_TCK_CACHED=$(getconf CLK_TCK 2>/dev/null || echo 100)
[[ $CLK_TCK_CACHED =~ ^[1-9][0-9]*$ ]] || CLK_TCK_CACHED=100
if [[ ! $TIMEOUT_KILL_GRACE_SECONDS =~ ^[0-9]+$ ]]; then
    echo "validate.sh: VALIDATE_TIMEOUT_KILL_GRACE_SECONDS must be a non-negative integer" >&2
    exit 2
fi
if [[ ! $VERBOSE_INTERVAL_SECONDS =~ ^[1-9][0-9]*$ ]]; then
    echo "validate.sh: VALIDATE_VERBOSE_INTERVAL_SECONDS must be a positive integer" >&2
    exit 2
fi
readonly VERBOSE GATE_TIMEOUT_SECONDS GATE_CPU_TIMEOUT_SECONDS CLK_TCK_CACHED
readonly TIMEOUT_KILL_GRACE_SECONDS VERBOSE_INTERVAL_SECONDS
readonly STRICT_COMPAT_ONLY PORTABLE_STRICT_COMPAT_ONLY RR_COMPAT_ONLY SABRE_COMPAT_ONLY
readonly E9PATCH_COMPAT_ONLY LITEINST_COMPAT_ONLY QEMU_L2_ONLY PRIVILEGED_ONLY
readonly VALIDATION_LEVEL VALIDATION_PROFILE

VALIDATION_STARTED_AT=$(date -u +%Y-%m-%dT%H:%M:%SZ)
VALIDATION_STARTED_EPOCH=$(date +%s)
VALIDATION_HOST=$(hostname -s 2>/dev/null || hostname 2>/dev/null || printf "unknown")
DEV_HERMIT_PARENT=$(find_dev_hermit_parent || true)
VALIDATION_SLOT=$(validation_slot_name "$DEV_HERMIT_PARENT")
VALIDATION_LEDGER_FILE=${HERMIT_VALIDATE_LEDGER:-}
if [[ -z $VALIDATION_LEDGER_FILE && -n $DEV_HERMIT_PARENT ]]; then
    VALIDATION_LEDGER_FILE="$DEV_HERMIT_PARENT/ignored/validate-run-ledger.jsonl"
fi
VALIDATION_COMMIT=$(git rev-parse HEAD 2>/dev/null || printf "unknown")
# Content-addressed identity of exactly what validate builds and tests: the root
# tree object. It hashes the tracked file content AND the submodule gitlink SHAs
# (gitlinks are tree entries), so a reverie/liteinst2 pin change still changes the
# tree. It does NOT vary with commit metadata (message, author/committer,
# timestamp, parent), so a rebase or amend that leaves file content byte-identical
# yields the SAME tree. This -- not the commit SHA -- is the result-cache key,
# because a SHA key would miss exactly during a drain when the lander re-anchors
# or rebases a commit whose tree never changed. The commit is still recorded (the
# lander's landing predicate joins on it), but the cache dereferences the tree.
VALIDATION_TREE=$(git rev-parse "HEAD^{tree}" 2>/dev/null || printf "unknown")
# Record the Rust toolchain so a cache hit keyed on the TREE can also require a
# matching build environment. The tree pins source and submodules, but not the
# compiler; a result produced by a different rustc is not safely reusable.
VALIDATION_TOOLCHAIN=$(rustc --version 2>/dev/null || printf "unknown")
VALIDATION_GIT_DEPTH=$(git rev-list --count HEAD 2>/dev/null || printf "0")
VALIDATION_GIT_AHEAD=0
VALIDATION_GIT_BEHIND=0
if git rev-parse --verify --quiet refs/remotes/origin/main >/dev/null; then
    read -r VALIDATION_GIT_BEHIND VALIDATION_GIT_AHEAD < <(
        git rev-list --left-right --count origin/main...HEAD 2>/dev/null || printf "0 0\n"
    )
fi

# Commit anchoring: VALIDATION_COMMIT faithfully names what ran only when the
# tree exactly matches HEAD. Otherwise the record would be misattributed to a
# HEAD that never actually ran, and selection baselines, green-time, and
# speculative-land verification -- all of which join on the SHA -- would compare
# against a tree that exists nowhere in history. Detect once, up front, before
# any gate mutates build outputs (which live under gitignored target/, so they
# do not themselves make the tree dirty).
#
# Two distinct notions, both recorded honestly:
#   tree_dirty      = the tree differs from HEAD in ANY way (staged or not). This
#                     is the porcelain-nonempty condition and drives anchoring.
#   worktree_dirty  = the WORKING TREE proper carries changes that `git add`
#                     would capture: unstaged edits to tracked files, or untracked
#                     files. This drives the hard gate, because staging WIP (or
#                     committing) is the caller's escape from it.
# Outside a git repo VALIDATION_COMMIT is "unknown" and both probes are empty, so
# the run is simply "not anchored" rather than "dirty".
if [[ -n "$(git status --porcelain 2>/dev/null || printf "")" ]]; then
    VALIDATION_TREE_DIRTY=1
else
    VALIDATION_TREE_DIRTY=0
fi
VALIDATION_WORKTREE_DIRTY=0
if ! git diff --quiet 2>/dev/null; then
    VALIDATION_WORKTREE_DIRTY=1
elif [[ -n "$(git ls-files --others --exclude-standard 2>/dev/null || printf "")" ]]; then
    VALIDATION_WORKTREE_DIRTY=1
fi
if [[ $VALIDATION_COMMIT != unknown ]] && ((VALIDATION_TREE_DIRTY == 0)); then
    VALIDATION_COMMIT_ANCHORED=1
else
    VALIDATION_COMMIT_ANCHORED=0
fi
# Selection mode: whether this run validated the whole configured lane or only an
# affected/explicit subset. Recorded separately from the profile so the ledger
# distinguishes a partial run from a complete one (selection is only sound on a
# complete commit, so a subset run must never masquerade as full coverage).
if ((SELECTIVE_MODE == 1)); then
    VALIDATION_SELECTION_MODE=selective
elif ((ONLY_MODE == 1)); then
    VALIDATION_SELECTION_MODE=only
else
    VALIDATION_SELECTION_MODE=full
fi
readonly VALIDATION_STARTED_AT VALIDATION_STARTED_EPOCH VALIDATION_HOST DEV_HERMIT_PARENT
readonly VALIDATION_SLOT VALIDATION_LEDGER_FILE VALIDATION_COMMIT VALIDATION_TREE
readonly VALIDATION_TOOLCHAIN VALIDATION_GIT_DEPTH
readonly VALIDATION_GIT_AHEAD VALIDATION_GIT_BEHIND
readonly VALIDATION_TREE_DIRTY VALIDATION_WORKTREE_DIRTY
readonly VALIDATION_COMMIT_ANCHORED VALIDATION_SELECTION_MODE

# Refuse to run on a dirty working tree so no validation record is silently
# misattributed to a HEAD that never ran. The caller's escapes are, in order of
# preference: commit (fully anchored), stage the WIP with `git add` (captured and
# runnable; the record is still commit_anchored=false because HEAD does not yet
# contain it), or force with --run-on-dirty-tree / VALIDATE_RUN_ON_DIRTY_TREE=1
# (agents must not). A forced run is likewise stamped commit_anchored=false. This
# gate runs before the validation tmp dir and EXIT trap are established, so a
# refused run leaves no partial state behind.
if ((VALIDATION_WORKTREE_DIRTY == 1 && RUN_ON_DIRTY_TREE == 0)); then
    {
        printf "validate.sh: refusing to run on a dirty working tree.\n"
        printf "  HEAD %s has uncommitted changes in the working tree, so a\n" "$VALIDATION_COMMIT"
        printf "  validation record anchored to it would describe a tree that exists\n"
        printf "  nowhere in history and cannot be reproduced or compared. Commit your\n"
        printf "  changes (preferred), or at least stage the WIP with 'git add' so it\n"
        printf "  is captured, then re-run. To force an explicitly unanchored run pass\n"
        printf "  --run-on-dirty-tree (VALIDATE_RUN_ON_DIRTY_TREE=1) -- agents must not.\n"
        printf "  Working-tree changes:\n"
        git status --short 2>/dev/null | sed 's/^/    /'
    } >&2
    exit 2
fi

# ---------------------------------------------------------------------------
# Tree-keyed result cache. The run-ledger written by append_validation_ledger is
# the single source of truth: this reads exactly what validate writes and what
# the lander consumes -- one store, never a second. A clean, commit-anchored,
# FULL run whose exact TREE already has a PASS record on THIS host+toolchain is
# reused -- announced LOUDLY (never a silent skip that could be mistaken for a
# fresh pass) and exited 0 without running a single gate. --ignore-cache /
# VALIDATE_IGNORE_CACHE=1 forces a real run.
#
# Why the key is the TREE, not the commit SHA:
#   The tree is the content-addressed identity of what actually gets built and
#   tested (tracked files + submodule gitlink SHAs). The commit SHA additionally
#   varies with metadata that does not affect the build -- committer timestamp,
#   message, parent. During a drain the lander re-anchors/rebases commits whose
#   tree never changed; a SHA key would MISS on byte-identical content and pay a
#   full run, which is exactly when caching matters most. A tree key hits.
#
# Why a hit is SOUND (each condition is checked against the stored record, not
# assumed -- this is what the first cut of this feature got wrong: it reused a
# bare PASS without confirming the run had actually executed anything):
#   tree match            -> the built+tested content is identical.
#   commit_anchored,
#     tree not dirty      -> the record described a real HEAD, not a WIP tree.
#   result == pass,
#     failures == 0       -> nothing failed.
#   executed_tests > 0    -> a green must carry a NONZERO executed-test count; a
#                            "test result: ok" with zero executed tests is a
#                            no-result and must never satisfy a run.
#   gates_run >=
#     gates_expected      -> full gate coverage, not a partial run.
#   selection_mode == full-> a selective/only record covered only a subset.
#   host + toolchain      -> the tree pins neither the compiler nor the box.
# Because old (pre-tree) records carry no `tree` field they can never hit, so the
# cache warms forward and never serves a result from an unverifiable environment.
# A prior FAIL for the tree does NOT skip: it may be flaky/environmental, and only
# a PASS satisfies the landing predicate, so we note it and run. This gate runs
# before the tmp dir and EXIT trap (like the dirty-tree gate above), so a hit
# leaves no partial state and appends no derived record.
function human_hms {
    local s=$1 h m
    [[ $s =~ ^[0-9]+$ ]] || s=0
    h=$((s / 3600)); m=$(((s % 3600) / 60))
    if ((h > 0)); then printf '%dh%02dm%02ds' "$h" "$m" "$((s % 60))"
    else printf '%dm%02ds' "$m" "$((s % 60))"; fi
}

# Emit the newest fully-qualifying ledger record with result==$1 for the current
# TREE/profile/host/toolchain as TSV
# "finished_at<TAB>real_seconds<TAB>cpu_seconds<TAB>executed_tests<TAB>commit",
# or nothing. Bounded by a pre-grep on the tree hash so the whole ledger is not
# slurped. Fail-open (prints nothing) when jq or the ledger is unavailable, so a
# missing tool can never manufacture a false hit. For a pass, every reuse
# condition -- including a nonzero executed-test count and full gate coverage --
# is verified here; a record that does not carry them is not a hit.
function cache_lookup_record {
    local want_result=$1 ledger=$VALIDATION_LEDGER_FILE
    [[ -n $ledger && -f $ledger ]] || return 0
    [[ $VALIDATION_TREE != unknown ]] || return 0
    command -v jq >/dev/null 2>&1 || return 0
    grep -F "\"tree\":\"$VALIDATION_TREE\"" "$ledger" 2>/dev/null \
        | jq -rs --arg tree "$VALIDATION_TREE" --arg prof "$VALIDATION_PROFILE" \
              --arg host "$VALIDATION_HOST" --arg tc "$VALIDATION_TOOLCHAIN" \
              --arg res "$want_result" '
            map(select(
                (.tree // "") == $tree
                and .commit_anchored == true and (.tree_dirty | not)
                and .result == $res and .profile == $prof
                and .selection_mode == "full"
                and .host == $host and (.toolchain // "") == $tc
                and (
                    $res != "pass"
                    or (
                        (.failures == 0)
                        and (.executed_tests != null) and (.executed_tests > 0)
                        and (.gates_expected == null
                             or (.gates_run != null and .gates_run >= .gates_expected))
                    )
                )))
            | sort_by(.finished_at) | last
            | if . == null then empty
              else [ .finished_at,
                     (.real_seconds // 0 | tostring),
                     ((.user_seconds // 0) + (.sys_seconds // 0) | tostring),
                     (.executed_tests // 0 | tostring),
                     (.commit // "unknown")
                   ] | @tsv
              end' 2>/dev/null
}

if ((IGNORE_CACHE == 0)) && ((VALIDATION_COMMIT_ANCHORED == 1)) \
   && [[ $VALIDATION_SELECTION_MODE == full && $VALIDATION_TREE != unknown ]]; then
    cache_hit_tsv=$(cache_lookup_record pass)
    if [[ -n $cache_hit_tsv ]]; then
        IFS=$'\t' read -r hit_when hit_wall hit_cpu hit_tests hit_commit <<<"$cache_hit_tsv"
        printf '# ============================================================\n'
        printf '# validate CACHE HIT for tree %s\n' "$VALIDATION_TREE"
        printf '#   (commit %s)\n' "$VALIDATION_COMMIT"
        printf '#   passed %s (wall %s, CPU %s, %s tests executed)\n' \
            "$hit_when" "$(human_hms "${hit_wall:-0}")" "$(human_hms "${hit_cpu:-0}")" "${hit_tests:-0}"
        printf '#   from a run of commit %s -- use --ignore-cache to force a real run\n' "${hit_commit:-unknown}"
        printf '#   profile=%s host=%s toolchain=%s\n' \
            "$VALIDATION_PROFILE" "$VALIDATION_HOST" "$VALIDATION_TOOLCHAIN"
        printf '#   NO gates ran this invocation; reused a clean, commit-anchored\n'
        printf '#   passing record (>0 executed tests, full gate coverage) from the\n'
        printf '#   run-ledger (%s).\n' "$VALIDATION_LEDGER_FILE"
        printf '# ============================================================\n'
        exit 0
    fi
    cache_fail_tsv=$(cache_lookup_record fail)
    if [[ -n $cache_fail_tsv ]]; then
        IFS=$'\t' read -r fail_when _ _ _ _ <<<"$cache_fail_tsv"
        printf '# validate: tree %s has a prior FAIL record (%s) on this host+toolchain; running anyway (a fail may be flaky/environmental). Only a PASS satisfies the landing predicate.\n' \
            "$VALIDATION_TREE" "$fail_when" >&2
    fi
fi

SUPER_REPETITIONS=${SUPER_REPETITIONS:-20}
if [[ ! $SUPER_REPETITIONS =~ ^[1-9][0-9]*$ ]]; then
    echo "validate.sh: SUPER_REPETITIONS must be a positive integer" >&2
    exit 2
fi
host_cpus=$(getconf _NPROCESSORS_ONLN 2>/dev/null || nproc 2>/dev/null || echo 1)
if [[ ! $host_cpus =~ ^[1-9][0-9]*$ ]]; then
    host_cpus=1
fi

# Default scheduler width for the CI DAG lanes (`./ci/run-dag.sh -j N`).
# SINGLE SOURCE OF TRUTH: every run-dag.sh invocation below reads
# ${CI_DAG_JOBS:-$CI_DAG_JOBS_DEFAULT}; do not re-hardcode the fallback (it lived
# in two places as `:-2` and drifted). Override per run with the CI_DAG_JOBS env var.
#
# HOST-ADAPTIVE, capped at 16. The cap is measurement-backed, not a guess: on the
# 316-CPU dev box the portable DAG measured CPU/wall 2.6x at -j2 (the old flat
# default) vs ~21.8x at -j16 (target/perf-width-sweep/run-j16.log), and it becomes
# critical-path-bound near width 16 (longest node ~106s + serial build spine), so
# wider buys little wall while raising peak demand. But the SAME validate.sh runs
# on GitHub's ubuntu-latest portable job (~4 CPU / 16 GiB); a flat 16 there would
# schedule many 5-8 GiB build/e2e nodes at once (the runner sets no --max-mem, so
# -j is a hard cap, not memory-gated) and OOM a job that -j2 kept green. Scaling
# with host_cpus keeps ubuntu-latest at 2 while the big box reaches the measured 16.
# host_cpus/8: 316->16(cap), 128->16, 64->8, 32->4, <=16->2(floor). Concurrency
# budget on the dev box: lander runs one validate per active worktree slot (cap 12)
# concurrently; 12 x ~22 effective cores/validate = 264 <= 316, fits.
CI_DAG_JOBS_DEFAULT=$((host_cpus / 8))
((CI_DAG_JOBS_DEFAULT < 2)) && CI_DAG_JOBS_DEFAULT=2
((CI_DAG_JOBS_DEFAULT > 16)) && CI_DAG_JOBS_DEFAULT=16
VALIDATION_DAG_JOBS=${CI_DAG_JOBS:-$CI_DAG_JOBS_DEFAULT}
if [[ ! $VALIDATION_DAG_JOBS =~ ^[1-9][0-9]*$ ]]; then
    echo "validate.sh: CI_DAG_JOBS must be a positive integer" >&2
    exit 2
fi

# Gate-count obligation for landing-eligible `full` runs, DERIVED from what the
# run actually executed rather than a hardcoded number, so it can never go stale
# as gates are added or removed. `run_check` is not fail-fast: a `full` run that
# reaches the end of `run_full_suite` has recorded EVERY gate in its plan exactly
# once (the preflight submodule + Reverie-pin checks, then the portable and
# privileged manifest lanes). We therefore DEFER the expected count to
# ledger-write time and set it to the observed `gates_run` -- but ONLY once
# VALIDATION_SUITE_COMPLETE proves the whole plan ran. An incomplete `full` run
# (e.g. a preflight abort) leaves the flag 0 and the count `null`, so the outcome
# authority applies no completeness check and can never misread a partial run as a
# complete FAILED one.
#
# The prior literal `5` predated the unconditional "Reverie pin consistency"
# preflight gate: every complete full run then recorded 6 gates while declaring 5,
# so the shared authority read gates_run(6) != expected(5) as TRUNCATED. A genuine
# full red could never be recorded as FAILED, and a genuine full green was
# discarded from the qualified population. A magic `6` would drift again on the
# next gate change (which is exactly how the `5` went stale); deriving from the
# executed set removes the drift class entirely.
#
# Partial/custom profiles stay `null`: they carry no full-landing contract (a
# single constant cannot be correct for `full` and, e.g.,
# `portable-strict-compat-only`, whose plans have different gate counts -- that
# divergence is itself the proof the value must be derived, not hardcoded). The
# authority treats gates_run < expected (and, in the classifier, run != expected)
# as TRUNCATED, never FAILED.
VALIDATION_SUITE_COMPLETE=0
VALIDATION_GATES_EXPECTED_JSON=null

SUPER_JOBS=${SUPER_JOBS:-$(((host_cpus * 3 + 1) / 2))}
if [[ ! $SUPER_JOBS =~ ^[1-9][0-9]*$ ]]; then
    echo "validate.sh: SUPER_JOBS must be a positive integer" >&2
    exit 2
fi
readonly SUPER_REPETITIONS SUPER_JOBS host_cpus CI_DAG_JOBS_DEFAULT VALIDATION_DAG_JOBS
# VALIDATION_GATES_EXPECTED_JSON / VALIDATION_SUITE_COMPLETE are intentionally NOT
# readonly here: the expected count is derived at ledger-write time from the
# gates actually executed, and the completion flag is set by run_full_suite.

HOST_OS=$(sed -n 's/^PRETTY_NAME=//p' /etc/os-release 2>/dev/null | head -n 1)
HOST_OS=${HOST_OS#\"}
HOST_OS=${HOST_OS%\"}
[[ -n $HOST_OS ]] || HOST_OS="unknown Linux"
readonly HOST_OS

# Cap the parallelism of the vendored third-party (DynamoRIO/elfutils) build.
# The DAG build cells run `CARGO_BUILD_JOBS=${THIRD_PARTY_BUILD_JOBS:-$(nproc)}
# cargo build ... --features third-party-backends`, and CARGO_BUILD_JOBS flows
# through NUM_JOBS into reverie-dbi/build.rs as the cmake `--build --parallel N`
# for the bundled DynamoRIO. On a many-core host nproc can be 300+, and an
# unbounded `--parallel` drives the elfutils dependency scan into a
# concurrency-exposed SIGABRT (core dump) roughly half the time -- an
# ENVIRONMENTAL flake, not a Hermit defect (measured ~2/4 portable-only runs at
# nproc=316). Cap the DEFAULT so that build is stable, while still honoring an
# explicit THIRD_PARTY_BUILD_JOBS override. This bounds only the third-party
# build cells; the main workspace build keeps full parallelism. Override the cap
# itself with VALIDATE_THIRD_PARTY_BUILD_JOBS_CAP.
VALIDATE_THIRD_PARTY_BUILD_JOBS_CAP=${VALIDATE_THIRD_PARTY_BUILD_JOBS_CAP:-32}
if [[ ! $VALIDATE_THIRD_PARTY_BUILD_JOBS_CAP =~ ^[1-9][0-9]*$ ]]; then
    VALIDATE_THIRD_PARTY_BUILD_JOBS_CAP=32
fi
if [[ -z ${THIRD_PARTY_BUILD_JOBS:-} ]]; then
    if ((host_cpus > VALIDATE_THIRD_PARTY_BUILD_JOBS_CAP)); then
        THIRD_PARTY_BUILD_JOBS=$VALIDATE_THIRD_PARTY_BUILD_JOBS_CAP
    else
        THIRD_PARTY_BUILD_JOBS=$host_cpus
    fi
fi
export THIRD_PARTY_BUILD_JOBS
readonly VALIDATE_THIRD_PARTY_BUILD_JOBS_CAP

checks=0
failures=0
# The signal trap sets this before EXIT cleanup writes the ledger. An explicit
# operator stop is a NO-RESULT unless a completed gate already proved a failure.
VALIDATION_INTERRUPTION_SIGNAL=""
# A validate receipt is trustworthy only when this run proved that the recorded
# Reverie dependency equals the live main tip. cleanup fails closed if any path
# reaches a nominally successful exit without setting this after the gate.
REVERIE_PIN_GATE_PASSED=0
# Environmental (sandbox) blocks that survived all retries. Counted toward
# `failures` too, so every existing `((failures == 0))` exit gate still fails a
# blocked run, but tracked separately so the summary can distinguish an
# INFRASTRUCTURE block from a genuine TEST failure. See is_environmental_block.
environmental=0
# Number of automatic retries when a check dies on a transient environmental
# sandbox block (e.g. a BPFJailer FS/EXEC/NET enforcer killing a build/test
# subprocess). Total attempts = retries + 1. Override with VALIDATE_ENV_BLOCK_RETRIES.
ENV_BLOCK_MAX_RETRIES=${VALIDATE_ENV_BLOCK_RETRIES:-2}
readonly ENV_BLOCK_MAX_RETRIES
active_check_pid=""
declare -a background_pids=()
declare -a background_names=()
declare -a background_logs=()
declare -a background_duration_files=()
declare -a ledger_gate_names=()
declare -a ledger_gate_statuses=()
declare -a ledger_gate_durations=()
VALIDATION_CONCURRENCY_MONITOR_PID=""

VALIDATION_TMP_PARENT="$ROOT_DIR/target/validation"
if [[ ${HERMIT_VALIDATE_STOP_TEST_MODE:-0} == 1 && -n ${VALIDATE_STOP_TEST_TMP_ROOT:-} ]]; then
    VALIDATION_TMP_PARENT=$VALIDATE_STOP_TEST_TMP_ROOT
fi
mkdir -p "$VALIDATION_TMP_PARENT"
VALIDATION_TMP_DIR=$(mktemp -d "$VALIDATION_TMP_PARENT/hermit-validate.XXXXXX")
if [[ -z $VALIDATION_TMP_DIR ]]; then
    echo "Unable to create validation workspace." >&2
    exit 1
fi
readonly VALIDATION_TMP_DIR
VALIDATION_CONCURRENT_MARKER="$VALIDATION_TMP_DIR/concurrent-validate-observed"
readonly VALIDATION_CONCURRENT_MARKER
export XDG_CONFIG_HOME="$VALIDATION_TMP_DIR/xdg-config"
mkdir -p "$XDG_CONFIG_HOME"
readonly XDG_CONFIG_HOME

# Classify the build-cache state BEFORE this run builds anything. A cold target/
# forces a full rebuild of hermit + its dependency graph, which dominates wall
# time; a warm target/ reuses it and the build becomes near-incremental. This is
# the single biggest factor in how long a run takes, so the estimate and the
# history ledger both record it. The presence of a compiled hermit binary is a
# reliable proxy for "target/ has been populated by a prior build":
#   warm    = both debug and release binaries present
#   partial = exactly one present (the other profile still rebuilds cold)
#   cold    = neither present (fresh target/, full rebuild ahead)
function detect_cache_state {
    local have_debug=0 have_release=0
    [[ -x "$ROOT_DIR/target/debug/hermit" ]] && have_debug=1
    [[ -x "$ROOT_DIR/target/release/hermit" ]] && have_release=1
    if ((have_debug == 1 && have_release == 1)); then
        printf "warm"
    elif ((have_debug == 1 || have_release == 1)); then
        printf "partial"
    else
        printf "cold"
    fi
}

# Artifact-integrity pre-flight. A compiler/archiver killed mid-write leaves a
# TRUNCATED zero-length *.o in a build tree -- classically the OOM-killer firing
# on a NEIGHBOUR's step cgroup with memory.oom.group=1, so make never runs its
# .DELETE_ON_ERROR cleanup. cmake/make key incremental freshness on TIMESTAMP not
# CONTENT, so they trust the empty object forever and link it, producing an
# "undefined reference" that reads as a source defect and never self-corrects.
# Scan before we trust the tree and delete any such object so the build rebuilds
# it. This is a CONTENT FACT, not a heuristic: it removes ONLY genuinely-corrupt
# (0-byte) objects, so healthy artifacts -- and thus incremental skipping and the
# warm cache -- are preserved (a blanket "clean rebuild after any failure" would
# not be: cold rebuilds cost +232s and fail more). Covers DynamoRIO (reverie-dbi),
# SaBRe + e9patch (hermit-install); rustc's target/deps self-heal via fingerprints.
# Catches corruption from a kill we did not observe, which the per-crate build.rs
# guard cannot: cargo re-runs a build script only on input change or prior failure,
# so a neighbour that truncates an already-built object is otherwise linked as-is.
# True when a linkable build artifact is structurally incomplete.
#
# Size is only a PROXY for corruption: an OOM-killed compiler routinely leaves a
# partial write that is nonzero AND retains valid magic, so both `-size 0` and a
# bare magic check pass it through and the linker then reports a bogus
# "undefined reference". ELF is self-describing, so truncation is detectable from
# the artifact's own header: the section table must fit inside the file.
#
# Magic is PER-FORMAT. `.a`/`.rlib` are ar archives ("!<arch>\n"), NOT ELF, so a
# single ELF-magic test would delete every valid static archive.
function artifact_is_corrupt {
    local f=$1 magic size
    size=$(stat -c %s -- "$f" 2>/dev/null) || return 1
    ((size == 0)) && return 0
    magic=$(head -c 8 -- "$f" 2>/dev/null | od -An -tx1 | tr -d ' \n')
    case "$f" in
        *.a | *.rlib)
            [[ $magic == 213c617263683e0a* ]] || return 0 # !<arch>\n
            ((size < 68)) && return 0                     # ar header + one member header
            return 1
            ;;
        *.o | *.so | *.so.* | *.lo)
            [[ $magic == 7f454c46* ]] || return 0 # \x7fELF
            python3 - "$f" <<'PY' && return 1 || return 0
import struct, sys
with open(sys.argv[1], "rb") as fh:
    head = fh.read(64)
if len(head) < 64:
    sys.exit(1)
if head[4] != 2:  # not ELFCLASS64: magic-only check
    sys.exit(0)
shoff = struct.unpack_from("<Q", head, 0x28)[0]
need = shoff + struct.unpack_from("<H", head, 0x3A)[0] * struct.unpack_from("<H", head, 0x3C)[0]
import os
sys.exit(1 if shoff and need > os.path.getsize(sys.argv[1]) else 0)
PY
            ;;
    esac
    return 1
}

# Delete build artifacts a killed compiler left structurally incomplete. cmake
# compares TIMESTAMPS, not content, so a truncated object with a fresh mtime is
# trusted forever and every later build links against a symbol-less file.
function purge_zero_byte_objects {
    local root=$1 removed=0 f
    [[ -d $root ]] || { printf 0; return 0; }
    while IFS= read -r -d '' f; do
        artifact_is_corrupt "$f" && rm -f -- "$f" && removed=$((removed + 1))
    done < <(find "$root" -type f \( -name '*.o' -o -name '*.a' -o -name '*.so' \
        -o -name '*.so.*' -o -name '*.rlib' -o -name '*.lo' \) -print0 2>/dev/null)
    printf '%s' "$removed"
}

# Print a REAL runtime estimate derived from this machine's validate-run history,
# or an honest "not enough history" message. It consumes the shared
# validate-run-ledger schema (schema_version >= 1) that this script writes and
# that ci-hub/validate/aggregate.py aggregates machine-wide -- we read the same
# records rather than inventing a parallel store. Only successful (result=="pass")
# runs of the SAME profile count, because a fast-failing or timed-out run is not a
# representative completion time. The estimate is bucketed by cache state because
# warm vs cold dominates wall time; it degrades through progressively broader
# scopes and, when even the broadest is too thin, says so instead of fabricating.
# Args: profile cache_state host ledger_file
function history_estimate {
    local profile=$1 cache=$2 host=$3 ledger=$4

    if [[ -z $ledger || ! -f $ledger ]]; then
        printf "no measured estimate yet (no run-history ledger; this run seeds it)"
        return 0
    fi

    awk -v PROFILE="$profile" -v CACHE="$cache" -v HOST="$host" '
        # POSIX-awk scalar extractors (no gawk match()-with-array extension).
        # The ledger fields we read (profile/cache_state/host/result are simple
        # token strings; real_seconds is a bare integer) never contain escaped
        # quotes, so anchored regex extraction is safe.
        function field(line, key,   re, s) {
            re = "\"" key "\":\"[^\"]*\""
            if (match(line, re)) {
                s = substr(line, RSTART, RLENGTH)
                sub("^\"" key "\":\"", "", s)
                sub("\"$", "", s)
                return s
            }
            return ""
        }
        function numfield(line, key,   re, s) {
            re = "\"" key "\":[0-9]+"
            if (match(line, re)) {
                s = substr(line, RSTART, RLENGTH)
                sub("^\"" key "\":", "", s)
                return s + 0
            }
            return -1
        }
        function isort(a, n,   i, j, key) {
            for (i = 1; i < n; i++) {
                key = a[i]; j = i - 1
                while (j >= 0 && a[j] > key) { a[j + 1] = a[j]; j-- }
                a[j + 1] = key
            }
        }
        function dur(s,   x, h, m, sec) {
            x = int(s + 0.5)
            h = int(x / 3600); m = int((x % 3600) / 60); sec = x % 60
            if (h > 0) return sprintf("%dh%02dm%02ds", h, m, sec)
            if (m > 0) return sprintf("%dm%02ds", m, sec)
            return sprintf("%ds", sec)
        }
        function emit(a, n, scope,   md, lo, hi) {
            isort(a, n)
            lo = a[0]; hi = a[n - 1]
            if (n % 2 == 1) md = a[int(n / 2)]
            else md = (a[n / 2 - 1] + a[n / 2]) / 2
            if (lo == hi)
                printf "~%s (%s, n=%d)\n", dur(md), scope, n
            else
                printf "~%s (median; range %s-%s; %s, n=%d)\n", \
                    dur(md), dur(lo), dur(hi), scope, n
        }
        BEGIN { n1 = 0; n2 = 0; n3 = 0; MIN = 3 }
        {
            if (field($0, "profile") != PROFILE) next
            if (field($0, "result") != "pass") next
            w = numfield($0, "real_seconds")
            if (w <= 0) next
            cs = field($0, "cache_state")
            hs = field($0, "host")
            t3[n3++] = w                                   # any cache, any host
            if (cs == CACHE) {
                t2[n2++] = w                               # same cache, any host
                if (hs == HOST) t1[n1++] = w               # same cache, same host
            }
        }
        END {
            if (n1 >= MIN)
                emit(t1, n1, CACHE " cache, " HOST ", this profile")
            else if (n2 >= MIN)
                emit(t2, n2, CACHE " cache, any host, this profile")
            else if (n3 >= MIN)
                emit(t3, n3, "MIXED warm/cold -- no " CACHE \
                    "-specific history yet, treat as a wide prior; this profile")
            else
                printf "insufficient history to estimate (only %d prior successful %s run(s); need >=%d). Current cache: %s. This run seeds the estimate.\n", \
                    n3, PROFILE, MIN, CACHE
        }
    ' "$ledger"
}

VALIDATION_CACHE_STATE=$(detect_cache_state)
readonly VALIDATION_CACHE_STATE

LOG_FILE=$(mktemp "${TMPDIR:-/tmp}/hermit-validate.XXXXXX.log")
if [[ -z $LOG_FILE ]]; then
    echo "Unable to create validation log." >&2
    exit 1
fi
readonly LOG_FILE
printf "Hermit validation log\nRoot: %s\nLevel: %s\nHost OS: %s\n\n" \
    "$ROOT_DIR" "$VALIDATION_PROFILE" "$HOST_OS" >"$LOG_FILE"
printf "Validation level: %s (host OS: %s)\n" "$VALIDATION_PROFILE" "$HOST_OS"
if ((VALIDATION_COMMIT_ANCHORED == 1)); then
    printf "Commit: %s (clean tree, commit-anchored); selection: %s\n" \
        "$VALIDATION_COMMIT" "$VALIDATION_SELECTION_MODE"
else
    printf "Commit: %s (⚠️  NOT commit-anchored: %s); selection: %s\n" \
        "$VALIDATION_COMMIT" \
        "$([[ $VALIDATION_COMMIT == unknown ]] && printf 'not a git checkout' || printf 'dirty tree')" \
        "$VALIDATION_SELECTION_MODE"
fi
printf "Build cache: %s (target/ debug=%s release=%s)\n" \
    "$VALIDATION_CACHE_STATE" \
    "$([[ -x "$ROOT_DIR/target/debug/hermit" ]] && printf present || printf absent)" \
    "$([[ -x "$ROOT_DIR/target/release/hermit" ]] && printf present || printf absent)"
VALIDATION_ZERO_BYTE_PURGED=$(purge_zero_byte_objects "$ROOT_DIR/target")
readonly VALIDATION_ZERO_BYTE_PURGED
if ((VALIDATION_ZERO_BYTE_PURGED > 0)); then
    printf "🧹 Artifact-integrity: purged %s zero-byte object(s) from target/ before build (killed/OOM-truncated; would otherwise link as 'undefined reference'). Rebuild will regenerate them.\n" \
        "$VALIDATION_ZERO_BYTE_PURGED"
    printf "validate.sh: purged %s zero-byte object(s) from target/ pre-build\n" \
        "$VALIDATION_ZERO_BYTE_PURGED" >>"$LOG_FILE"
fi
printf "Estimated time: %s\n" \
    "$(history_estimate "$VALIDATION_PROFILE" "$VALIDATION_CACHE_STATE" "$VALIDATION_HOST" "$VALIDATION_LEDGER_FILE")"
if [[ $VALIDATION_LEVEL == super ]]; then
    printf "Super stress: %s repetitions/probe, up to %s concurrent jobs (%s online CPUs)\n" \
        "$SUPER_REPETITIONS" "$SUPER_JOBS" "$host_cpus"
fi
if ((VERBOSE == 1)); then
    printf "Verbose validation enabled\n"
    printf "  root: %s\n" "$ROOT_DIR"
    printf "  log: %s\n" "$LOG_FILE"
    printf "  gate timeout: %ss wall (kill grace: %ss; heartbeat: %ss)\n" \
        "$GATE_TIMEOUT_SECONDS" "$TIMEOUT_KILL_GRACE_SECONDS" "$VERBOSE_INTERVAL_SECONDS"
    if ((GATE_CPU_TIMEOUT_SECONDS > 0)); then
        printf "  gate CPU budget: %ss CPU-time (user+sys, whole tree)\n" "$GATE_CPU_TIMEOUT_SECONDS"
    else
        printf "  gate CPU budget: off (set VALIDATE_GATE_CPU_TIMEOUT_SECONDS to enable)\n"
    fi
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
readonly SABRE_COMPAT_EXPECTED=207
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

# Aggregate CPU seconds (user+sys) consumed by the process tree rooted at $1,
# summed from /proc/<pid>/stat over the root and all its descendants. This is
# controller-free and host-portable: it does NOT need a delegated cgroup cpu
# controller (often absent on the many-core dev hosts), unlike reading cpu.stat.
# Prints an integer count of CPU-seconds, or 0 when /proc is unreadable or the
# tree has already exited. The comm field (2) can contain spaces and parentheses,
# so parsing splits on the LAST ")" and indexes the fixed fields after it.
function tree_cpu_seconds {
    local root=$1
    cat /proc/[0-9]*/stat 2>/dev/null | awk -v root="$root" -v clk="$CLK_TCK_CACHED" '
    {
        rp = 0
        for (i = length($0); i >= 1; i--) {
            if (substr($0, i, 1) == ")") { rp = i; break }
        }
        if (rp == 0) next
        pid = $1 + 0
        n = split(substr($0, rp + 2), f, " ")
        if (n < 13) next
        ppid[pid] = f[2] + 0        # ppid: field 4 of stat = field 2 after comm
        ticks[pid] = f[12] + f[13]  # utime (14) + stime (15) = fields 12,13 after comm
        seen[pid] = 1
    }
    END {
        intree[root] = 1
        changed = 1
        while (changed) {
            changed = 0
            for (p in seen) {
                if (!intree[p] && (p in ppid) && intree[ppid[p]]) {
                    intree[p] = 1
                    changed = 1
                }
            }
        }
        total = 0
        for (p in seen) if (intree[p]) total += ticks[p]
        printf "%d", int(total / clk)
    }'
}

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

# Send TERM to the process tree rooted at $1, wait out the kill grace, then
# escalate to KILL if anything survives, and reap the root. Mirrors the wall
# timeout's teardown so a CPU-budget kill leaves no stragglers behind.
function terminate_gate_tree {
    local pid=$1
    local grace_deadline
    kill_process_tree "$pid" TERM
    grace_deadline=$((SECONDS + TIMEOUT_KILL_GRACE_SECONDS))
    while kill -0 "$pid" 2>/dev/null && ((SECONDS < grace_deadline)); do
        sleep 0.2
    done
    if kill -0 "$pid" 2>/dev/null; then
        kill_process_tree "$pid" KILL
    fi
    wait "$pid" 2>/dev/null || true
}

function record_ledger_gate {
    ledger_gate_names+=("$1")
    ledger_gate_statuses+=("$2")
    ledger_gate_durations+=("$3")
}

function interruption_is_no_result {
    local status

    [[ -n $VALIDATION_INTERRUPTION_SIGNAL ]] || return 1
    ((failures == 0)) || return 1
    for status in "${ledger_gate_statuses[@]}"; do
        ((status == 0)) || return 1
    done
    return 0
}

function json_quote {
    local value=$1
    value=${value//\\/\\\\}
    value=${value//\"/\\\"}
    value=${value//$'\n'/\\n}
    value=${value//$'\r'/\\r}
    value=${value//$'\t'/\\t}
    printf '"%s"' "$value"
}

# Observe overlapping top-level validate process groups for the whole run. A
# point-in-time count at start or finish misses a validate that starts and ends
# in the middle; the one-second monitor leaves a durable marker for this receipt.
# Subshells of this validate share its process group and are excluded, so a gate
# invoking another validate.sh internally cannot forge concurrency.
function start_validation_concurrency_monitor {
    local root_pid=$$ root_pgid
    root_pgid=$(ps -o pgid= -p "$root_pid" 2>/dev/null | tr -d ' ')
    [[ $root_pgid =~ ^[0-9]+$ ]] || return 0
    if [[ ${CI_HUB_VALIDATE_CONCURRENT:-} == true ]]; then
        printf '1\n' >"$VALIDATION_CONCURRENT_MARKER"
    fi
    (
        while kill -0 "$root_pid" 2>/dev/null; do
            local count previous=0
            count=$(ps -eo pgid=,args= 2>/dev/null | awk -v own="$root_pgid" '
                $1 != own && /(^|[\/ ])validate\.sh([ ]|$)/ { seen[$1]=1 }
                END { print length(seen) }
            ')
            [[ $count =~ ^[0-9]+$ ]] || count=0
            if [[ -r $VALIDATION_CONCURRENT_MARKER ]]; then
                previous=$(<"$VALIDATION_CONCURRENT_MARKER")
                [[ $previous =~ ^[0-9]+$ ]] || previous=0
            fi
            if ((count > previous)); then
                printf '%s\n' "$count" >"$VALIDATION_CONCURRENT_MARKER"
            fi
            sleep 1
        done
    ) &
    VALIDATION_CONCURRENCY_MONITOR_PID=$!
}

# Prove that this shell is a descendant of the live process-bound validate-lock
# owner. Merely setting an environment variable is not enough: the owner sidecar
# must name the same PID, and that PID must occur in this process's ancestry.
function validate_lock_exclusivity_proven {
    local owner_pid=${CI_HUB_VALIDATE_LOCK_OWNER_PID:-}
    local owner_file=${CI_HUB_VALIDATE_LOCK_OWNER_FILE:-}
    local recorded_pid current=$$
    [[ $owner_pid =~ ^[1-9][0-9]*$ && -r $owner_file ]] || return 1
    recorded_pid=$(sed -n 's/^pid=//p' "$owner_file" 2>/dev/null)
    [[ $recorded_pid == "$owner_pid" ]] || return 1
    while [[ $current =~ ^[1-9][0-9]*$ ]] && ((current > 1)); do
        [[ $current == "$owner_pid" ]] && return 0
        current=$(sed -n 's/^PPid:[[:space:]]*//p' "/proc/$current/status" 2>/dev/null)
    done
    return 1
}

# Append one JSONL record to the shared validate-run ledger. Wall and CPU seconds
# are computed once by the caller (cleanup) in the top-level shell so they match
# the human summary exactly and so the `times` builtin sees the accumulated child
# CPU (a subshell would report only its own times). See print_wall_cpu_summary.
function append_validation_ledger {
    local exit_status=$1
    local wall_seconds=$2 cpu_user=$3 cpu_sys=$4
    local finished_at result raw_result gates_json gate_result line
    local count_helper counts executed_tests_json=null filtered_tests_json=null
    local commit_anchored_json tree_dirty_json concurrent_validates_json concurrency_proof_json gates_run
    local evidence_helper evidence_json failed_substeps_json='[]' flaky_failed_substeps_json='[]'
    local known_flaky_failure_json=null solo_rerun_confirmation_json=false
    local solo_rerun_of_json=null
    local first_error_line_json=null failed_substep_classes_json='[]'
    local evidence_available=0 failure_origin_json gate_substeps_json
    local interruption_signal_json=null
    local reverie_pin_current_json
    local i

    [[ -n $VALIDATION_LEDGER_FILE ]] || return 0

    finished_at=$(date -u +%Y-%m-%dT%H:%M:%SZ)

    if ((exit_status == 0 && failures == 0)); then
        raw_result=pass
    else
        raw_result=fail
    fi
    if interruption_is_no_result; then
        # An operator stop learned nothing new about the product. Preserve the
        # raw shell result for forensics, but do not mint a FAILED verdict unless
        # a completed gate had already established one before the stop.
        result=no_result
    else
        result=$raw_result
    fi

    if [[ -r $VALIDATION_CONCURRENT_MARKER ]]; then
        concurrent_validates_json=$(<"$VALIDATION_CONCURRENT_MARKER")
        [[ $concurrent_validates_json =~ ^[1-9][0-9]*$ ]] || concurrent_validates_json=null
        concurrency_proof_json='"process_group_overlap_monitor"'
    elif validate_lock_exclusivity_proven; then
        concurrent_validates_json=0
        concurrency_proof_json='"validate_lock_owner_ancestry"'
    else
        # A bare run with no observed peer is UNKNOWN, not proven exclusive.
        concurrent_validates_json=null
        concurrency_proof_json=null
    fi

    evidence_helper="$DEV_HERMIT_PARENT/ci-hub/validate/failure_evidence.py"
    if [[ -n $DEV_HERMIT_PARENT && -r $evidence_helper ]] \
        && command -v python3 >/dev/null 2>&1 && command -v jq >/dev/null 2>&1; then
        if evidence_json=$(python3 "$evidence_helper" --log "$LOG_FILE" \
            --ledger "$VALIDATION_LEDGER_FILE" --commit "$VALIDATION_COMMIT" \
            --dag-jobs "$VALIDATION_DAG_JOBS" \
            --concurrent-validates "$concurrent_validates_json" 2>>"$LOG_FILE"); then
            failed_substeps_json=$(jq -c '.failed_substeps' <<<"$evidence_json")
            flaky_failed_substeps_json=$(jq -c '.flaky_failed_substeps' <<<"$evidence_json")
            known_flaky_failure_json=$(jq -r '.known_flaky_failure' <<<"$evidence_json")
            solo_rerun_confirmation_json=$(jq -r '.solo_rerun_confirmation' <<<"$evidence_json")
            solo_rerun_of_json=$(jq -c '.solo_rerun_of' <<<"$evidence_json")
            # DURABLE ATTRIBUTION: the per-node fault verdict and the verbatim
            # headline fault line are computed by failure_evidence.py; inline them
            # into the row so a red is attributable to WHICH bug (infra vs code,
            # first_error_line) from the row ALONE — after the /tmp log is evicted.
            # The read side (ci-hub/validate/attribute_reds.py) prefers these
            # row-carried classes over dereferencing the ephemeral log_file.
            first_error_line_json=$(jq -c '.first_error_line' <<<"$evidence_json")
            failed_substep_classes_json=$(jq -c '.failed_substep_classes' <<<"$evidence_json")
            evidence_available=1
        fi
    fi

    gates_json='['
    for i in "${!ledger_gate_names[@]}"; do
        ((i == 0)) || gates_json+=','
        if ((ledger_gate_statuses[i] == 0)); then
            gate_result=pass
        else
            gate_result=fail
        fi
        gates_json+="{\"name\":$(json_quote "${ledger_gate_names[i]}"),"
        gates_json+="\"result\":\"$gate_result\","
        gates_json+="\"exit_code\":${ledger_gate_statuses[i]},"
        gates_json+="\"real_seconds\":${ledger_gate_durations[i]}"
        if ((ledger_gate_statuses[i] != 0)); then
            failure_origin_json=null
            gate_substeps_json='[]'
            if ((evidence_available == 1)); then
                if [[ ${ledger_gate_names[i]} == *" CI DAG lane" && $failed_substeps_json != '[]' ]]; then
                    failure_origin_json='"lane_substep"'
                    gate_substeps_json=$failed_substeps_json
                else
                    failure_origin_json='"outer_gate"'
                fi
            fi
            gates_json+=",\"failure_origin\":$failure_origin_json,"
            gates_json+="\"failed_substeps\":$gate_substeps_json"
        fi
        gates_json+='}'
    done
    gates_json+=']'
    gates_run=${#ledger_gate_names[@]}

    # Derive the full-run gate obligation from what actually executed (see the
    # VALIDATION_SUITE_COMPLETE comment near the config block): a completed full
    # plan recorded every gate exactly once, so gates_run IS the expected full
    # coverage. This stays correct automatically as gates are added or removed,
    # unlike the former hardcoded literal. Incomplete full runs and all partial
    # profiles keep expected `null`, so no false completeness check is applied.
    if [[ $VALIDATION_PROFILE == full ]] && ((VALIDATION_SUITE_COMPLETE == 1)); then
        VALIDATION_GATES_EXPECTED_JSON=$gates_run
    fi

    if ((VALIDATION_COMMIT_ANCHORED == 1)); then commit_anchored_json=true; else commit_anchored_json=false; fi
    if ((VALIDATION_TREE_DIRTY == 1)); then tree_dirty_json=true; else tree_dirty_json=false; fi
    if [[ -n $VALIDATION_INTERRUPTION_SIGNAL ]]; then
        interruption_signal_json=$(json_quote "$VALIDATION_INTERRUPTION_SIGNAL")
    fi
    if ((REVERIE_PIN_GATE_PASSED == 1)); then reverie_pin_current_json=true; else reverie_pin_current_json=false; fi

    # Use the parent's single-sourced libtest-banner parser. Unknown stays null;
    # the receipt publisher fails closed rather than turning missing evidence
    # into zero or a pass. The fields are additive to schema 3 during the
    # coverage-schema transition.
    count_helper="$DEV_HERMIT_PARENT/ci-hub/remediation/nonzero_result.py"
    if [[ -n $DEV_HERMIT_PARENT && -r $count_helper ]] && command -v python3 >/dev/null 2>&1; then
        counts=$(python3 "$count_helper" --ledger-fields "$LOG_FILE" 2>/dev/null) || counts="null null"
        read -r executed_tests_json filtered_tests_json <<<"$counts"
        [[ $executed_tests_json =~ ^(null|[0-9]+)$ ]] || executed_tests_json=null
        [[ $filtered_tests_json =~ ^(null|[0-9]+)$ ]] || filtered_tests_json=null
    fi

    # schema_version 3 adds commit_anchored/tree_dirty/selection_mode; schema_version
    # 4 adds `tree` (the content-addressed build+test identity, the result-cache
    # key), `toolchain` (the rustc build environment the cache must match), and
    # first-class interruption evidence. A stopped run with no established gate
    # failure records result=no_result; raw_result preserves the shell outcome
    # without granting it product-verdict authority. The fields are additive;
    # the parent ledger aggregator reads via .get() and is
    # unaffected until it is taught to surface them. (warm-vs-cold is already
    # recorded as cache_state, so this does not duplicate it.)
    line="{\"schema_version\":4,\"started_at\":$(json_quote "$VALIDATION_STARTED_AT"),"
    line+="\"finished_at\":$(json_quote "$finished_at"),\"host\":$(json_quote "$VALIDATION_HOST"),"
    line+="\"toolchain\":$(json_quote "$VALIDATION_TOOLCHAIN"),"
    line+="\"slot\":$(json_quote "$VALIDATION_SLOT"),\"cwd\":$(json_quote "$ROOT_DIR"),"
    line+="\"profile\":$(json_quote "$VALIDATION_PROFILE"),"
    line+="\"selection_mode\":$(json_quote "$VALIDATION_SELECTION_MODE"),"
    line+="\"cache_state\":$(json_quote "$VALIDATION_CACHE_STATE"),"
    line+="\"zero_byte_purged\":${VALIDATION_ZERO_BYTE_PURGED:-0},"
    line+="\"commit\":$(json_quote "$VALIDATION_COMMIT"),\"tree\":$(json_quote "$VALIDATION_TREE"),"
    line+="\"git_depth\":$VALIDATION_GIT_DEPTH,"
    line+="\"git_ahead\":$VALIDATION_GIT_AHEAD,\"git_behind\":$VALIDATION_GIT_BEHIND,"
    line+="\"commit_anchored\":$commit_anchored_json,\"tree_dirty\":$tree_dirty_json,"
    line+="\"reverie_pin_current\":$reverie_pin_current_json,"
    line+="\"result\":\"$result\",\"raw_result\":\"$raw_result\",\"exit_code\":$exit_status,"
    line+="\"checks\":$checks,\"failures\":$failures,"
    line+="\"dag_jobs\":$VALIDATION_DAG_JOBS,\"concurrent_validates\":$concurrent_validates_json,"
    line+="\"concurrency_proof\":$concurrency_proof_json,"
    line+="\"known_flaky_failure\":$known_flaky_failure_json,"
    line+="\"first_error_line\":$first_error_line_json,"
    line+="\"failed_substep_classes\":$failed_substep_classes_json,"
    line+="\"flaky_failed_substeps\":$flaky_failed_substeps_json,"
    line+="\"solo_rerun_confirmation\":$solo_rerun_confirmation_json,"
    line+="\"solo_rerun_of\":$solo_rerun_of_json,"
    line+="\"gates_run\":$gates_run,\"gates_expected\":$VALIDATION_GATES_EXPECTED_JSON,"
    line+="\"interruption_signal\":$interruption_signal_json,"
    line+="\"executed_tests\":$executed_tests_json,\"filtered_tests\":$filtered_tests_json,"
    line+="\"real_seconds\":$wall_seconds,\"user_seconds\":$cpu_user,\"sys_seconds\":$cpu_sys,"
    line+="\"log_file\":$(json_quote "$LOG_FILE"),\"gates\":$gates_json}"

    if ! mkdir -p "$(dirname -- "$VALIDATION_LEDGER_FILE")"; then
        printf "⚠️  unable to create validation ledger directory for %s\n" \
            "$VALIDATION_LEDGER_FILE" >&2
        return 0
    fi
    if command -v flock >/dev/null 2>&1; then
        if ! (
            flock -x 9
            printf "%s\n" "$line" >&9
        ) 9>>"$VALIDATION_LEDGER_FILE"; then
            printf "⚠️  unable to append validation ledger %s\n" \
                "$VALIDATION_LEDGER_FILE" >&2
        fi
    elif ! printf "%s\n" "$line" >>"$VALIDATION_LEDGER_FILE"; then
        printf "⚠️  unable to append validation ledger %s\n" \
            "$VALIDATION_LEDGER_FILE" >&2
    fi
}

function human_duration {
    awk -v t="$1" 'BEGIN {
        x = int(t + 0.5)
        h = int(x / 3600); m = int((x % 3600) / 60); s = x % 60
        if (h > 0) printf "%dh%02dm%02ds", h, m, s
        else if (m > 0) printf "%dm%02ds", m, s
        else printf "%ds", s
    }'
}

# Always-printed final line: wall AND CPU for the whole run. CPU (user+sys, whole
# process tree) vs wall is what distinguishes a genuinely-busy run from one that
# is blocked or spinning while merely appearing hung: CPU near zero against a
# large wall means waiting/blocked, while ~1 core pinned across a multi-core host
# can mean single-threaded work or a spin. Emitted on success, failure, timeout,
# and interruption alike.
function print_wall_cpu_summary {
    local exit_status=$1 wall=$2 user=$3 sys=$4
    local cpu ratio marker hint=""

    cpu=$(awk -v u="$user" -v s="$sys" 'BEGIN { printf "%.1f", u + s }')
    if ((wall > 0)); then
        ratio=$(awk -v c="$cpu" -v w="$wall" 'BEGIN { printf "%.1f", c / w }')
    else
        ratio="n/a"
    fi
    if interruption_is_no_result; then
        marker="⏹"
    elif ((exit_status == 0 && failures == 0)); then
        marker="✅"
    else
        marker="❌"
    fi
    if ((wall >= 30)); then
        if awk -v c="$cpu" -v w="$wall" 'BEGIN { exit !(c < 0.10 * w) }'; then
            hint="  (low CPU vs wall — mostly waiting/blocked, not compute-bound)"
        elif ((host_cpus > 2)) && \
            awk -v r="$ratio" 'BEGIN { exit !(r + 0 >= 0.8 && r + 0 <= 1.2) }'; then
            hint="  (~1 core busy — single-threaded or possibly spinning)"
        fi
    fi
    printf "%s Elapsed: wall %s | CPU %s (user %s, sys %s) | CPU/wall %sx across %s cores%s\n" \
        "$marker" "$(human_duration "$wall")" "$(human_duration "$cpu")" \
        "$(human_duration "$user")" "$(human_duration "$sys")" \
        "$ratio" "$host_cpus" "$hint"
}

function publish_receipt_backed_label {
    local pr=${PR_NUMBER:-}
    local ci_hub=${CI_HUB_APPLY_LOCAL_LABEL:-}
    local -a gh_cmd=(gh)

    if [[ -z $ci_hub && -n $DEV_HERMIT_PARENT ]]; then
        ci_hub="$DEV_HERMIT_PARENT/ci-hub/ci-hub"
    fi
    if [[ -z $ci_hub || ! -x $ci_hub ]]; then
        printf "⚠️  counted validation recorded, but the ci-hub receipt publisher is unavailable; not applying locally-validated\n" >&2
        return 0
    fi
    if [[ -z $pr ]] && command -v gh >/dev/null 2>&1; then
        if command -v with-proxy >/dev/null 2>&1; then
            gh_cmd=(with-proxy gh)
        fi
        pr=$("${gh_cmd[@]}" pr view --repo rrnewton/hermit \
            --json number -q .number 2>/dev/null) || true
    fi
    if [[ -z $pr ]]; then
        printf "⚠️  counted validation recorded, but no PR was found; not applying locally-validated\n" >&2
        return 0
    fi
    if ! "$ci_hub" apply-local-label --pr "$pr" --repo rrnewton/hermit \
        --ledger "$VALIDATION_LEDGER_FILE"; then
        printf "⚠️  receipt publication failed for PR #%s; locally-validated was not authorized\n" \
            "$pr" >&2
    fi
}

function cleanup {
    local exit_status=$?
    local pid

    trap - EXIT
    # Cleanup is the evidence commit point. A second stop signal must not abort
    # it between child teardown and the single ledger append.
    trap '' INT TERM HUP

    if [[ -n ${VALIDATE_STOP_TEST_CLEANUP_READY_FILE:-} ]]; then
        printf '%s\n' "$$" >"$VALIDATE_STOP_TEST_CLEANUP_READY_FILE"
        sleep "${VALIDATE_STOP_TEST_CLEANUP_DELAY_SECONDS:-0.5}"
    fi

    if [[ -n $VALIDATION_CONCURRENCY_MONITOR_PID ]]; then
        terminate_gate_tree "$VALIDATION_CONCURRENCY_MONITOR_PID"
    fi

    # Receipt production is itself an enforcement path. If a new fast path or
    # early return accidentally bypasses the pin gate, it must not emit PASS or
    # return success merely because its selected tests happened to pass.
    if ((exit_status == 0 && REVERIE_PIN_GATE_PASSED != 1)); then
        printf "❌ Validation path bypassed the latest-Reverie pin gate; refusing a passing receipt.\n" >&2
        failures=$((failures + 1))
        exit_status=1
    fi

    if [[ -n $active_check_pid ]]; then
        terminate_gate_tree "$active_check_pid"
    fi
    for pid in "${background_pids[@]}"; do
        terminate_gate_tree "$pid"
    done

    # Wall + CPU for the whole run, computed ONCE here in the trap's top-level
    # shell context (a subshell's `times` would miss the accumulated child CPU).
    # The same numbers feed both the ledger and the always-printed summary.
    local finished_epoch validation_wall validation_user="0" validation_sys="0"
    finished_epoch=$(date +%s)
    validation_wall=$((finished_epoch - VALIDATION_STARTED_EPOCH))
    if times >"$VALIDATION_TMP_DIR/cpu-times" 2>/dev/null; then
        read -r validation_user validation_sys < <(
            awk '
                function seconds(value, parts) {
                    split(value, parts, "m")
                    sub(/s$/, "", parts[2])
                    return parts[1] * 60 + parts[2]
                }
                NR == 1 { user += seconds($1); sys += seconds($2) }
                NR == 2 { user += seconds($1); sys += seconds($2) }
                END { printf "%.3f %.3f\n", user, sys }
            ' "$VALIDATION_TMP_DIR/cpu-times"
        )
    fi

    if declare -F print_compatibility_summary >/dev/null; then
        print_compatibility_summary
    fi
    append_validation_ledger "$exit_status" \
        "$validation_wall" "$validation_user" "$validation_sys"
    if ((exit_status == 0 && failures == 0 && LABEL_PR == 1 && \
        VALIDATION_COMMIT_ANCHORED == 1 && VALIDATION_TREE_DIRTY == 0)) && \
       [[ $VALIDATION_LEVEL == full ]]; then
        publish_receipt_backed_label
    fi
    rm -rf "$VALIDATION_TMP_DIR"
    rm -rf "$REAL_COMPAT_FIXTURES"
    print_wall_cpu_summary "$exit_status" \
        "$validation_wall" "$validation_user" "$validation_sys"
    exit "$exit_status"
}

function interrupted {
    local signal=$1
    VALIDATION_INTERRUPTION_SIGNAL=$signal
    trap - INT TERM
    trap - HUP
    printf "⏹ Validation interrupted by %s; preserving any earlier gate failure, otherwise recording NO-RESULT (full log: %s)\n" \
        "$signal" "$LOG_FILE"
    exit 130
}
trap cleanup EXIT
trap 'interrupted INT' INT
trap 'interrupted TERM' TERM
trap 'interrupted HUP' HUP
start_validation_concurrency_monitor

# Test seam for scripts/test_validate_stop_paths.py. It exercises this script's
# real traps and ledger writer without starting a product build. The mode cannot
# produce a pass: it deliberately waits until a test sends a stop signal.
if [[ ${HERMIT_VALIDATE_STOP_TEST_MODE:-0} == 1 ]]; then
    if [[ ${VALIDATE_STOP_TEST_PRIOR_FAILURE:-0} == 1 ]]; then
        record_ledger_gate "stop-test completed gate 1" 1 0
        failures=1
    else
        record_ledger_gate "stop-test completed gate 1" 0 0
    fi
    record_ledger_gate "stop-test completed gate 2" 0 0
    checks=2
    if [[ -n ${VALIDATE_STOP_TEST_PID_FILE:-} ]]; then
        printf "%s\n" "$$" >"$VALIDATE_STOP_TEST_PID_FILE"
    fi
    printf "VALIDATE_STOP_TEST_READY pid=%s\n" "$$"
    if [[ ${VALIDATE_STOP_TEST_EXIT_EARLY:-0} == 1 ]]; then
        exit 1
    fi
    while :; do sleep 1; done
fi

# Return success (0) when the failed check's log region carries the signature of
# an ENVIRONMENTAL sandbox denial rather than a product/test failure. The Claude
# Code agent runs inside a BPFJailer jail inherited by every descendant
# (validate.sh -> cargo -> rustc/cmake/cc1/ld); its FS/EXEC/NET enforcers can
# transiently deny a file open by a build or test subprocess for reasons
# unrelated to the code under test.
#
# The denial surfaces in TWO forms, both of which we must catch:
#
#   1. The canonical BPFJailer banner ("blocked on this server based on a
#      security policy", "BpfJailer", "Enforcer: FS, Reason: ..."). This is what
#      appears when the jailer itself prints to the process's output.
#   2. A raw EPERM/EACCES leaked to a build tool with NO banner. The FS enforcer
#      denies open() and the toolchain reports the errno verbatim, e.g. cc1
#      `fatal error: /usr/lib/gcc/.../stddef.h: Operation not permitted` while
#      building DynamoRIO, or a CMake/linker "Permission denied" on a system
#      path. On this host those files are world-readable (root:root -rw-r--r--),
#      so a *compiler* reporting it cannot open a header for a permission reason
#      is never legitimate product behavior -- it is always the sandbox. This is
#      how the DynamoRIO "host permission" block (validate-dynamorio-host-
#      permission-block) manifests: same jail, same FS/FILE_OPEN denial as the
#      BPFJailer transient, but banner-less.
#
# The form-2 patterns are anchored on compiler/build-tool phrasing
# (`fatal error: <path>:`, `CMake Error`, `cannot open ...`) so ordinary GUEST
# test output that legitimately produces EPERM -- DETLOG lines such as
# `madvise ... EPERM (Operation not permitted)`, the kcmp-eperm fixture, or a
# `context: Mount` EPERM -- never trips a false positive. Misclassifying a real
# test failure as environmental is as harmful as the reverse, so keep these
# signatures build-toolchain-specific.
#
#   3. A failure of the vendored third-party DynamoRIO build, surfaced by cargo
#      as `failed to run custom build command for reverie-dbi ...` or a panic in
#      `reverie-dbi/build.rs` (which asserts the DynamoRIO cmake `--build`
#      status). The bundled DynamoRIO/elfutils build is driven at
#      `--parallel ${THIRD_PARTY_BUILD_JOBS:-$(nproc)}`; on a many-core host an
#      unbounded parallelism drives the elfutils dependency scan into a
#      concurrency-exposed SIGABRT (`Aborted (core dumped)`), observed ~2/4
#      portable-only runs at nproc=316. That is a HOST build flake, not a Hermit
#      product defect (Hermit source is not what failed to compile), so it is the
#      same class as the sandbox blocks above. We PREVENT it by capping
#      THIRD_PARTY_BUILD_JOBS (see the export near the counters), and detect it
#      here so any residual transient is retried and clearly labeled. This anchor
#      is narrow to the reverie-dbi third-party build script: a *persistent*
#      breakage (e.g. a bad reverie pin) fails every retry and still leaves the
#      run RED via the retry-exhaustion path -- it is never silently greened,
#      only relabeled from "test failure" to "third-party build (environmental)".
#      Only reverie-dbi's own build script matches, so a Hermit test that merely
#      prints "panicked at .../build.rs" for a different crate cannot trip it.
function is_environmental_block {
    local output_start=$1
    tail -n "+$output_start" "$LOG_FILE" |
        sed $'s/\033\\[[0-9;]*[[:alpha:]]//g' |
        grep -qiE 'blocked on this server based on a security policy|\bBpfJailer\b|Enforcer: (FS|EXEC|NET), Reason:|fatal error: [^:]*:.*(operation not permitted|permission denied)|CMake Error.*(operation not permitted|permission denied)|(cannot open|error opening|failed to open|could not open)[^,]*: (operation not permitted|permission denied)|failed to run custom build command for [^[:space:]]*reverie-dbi|panicked at [^[:space:]]*reverie-dbi/build\.rs'
}

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
    local cpu_seconds
    local cpu_next_sample=$((SECONDS + 1))

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

        # Load-immune CPU-time budget: kill a gate that burns more CPU (user+sys,
        # whole tree) than allowed, even while it is well under the wall timeout.
        # Sampled ~1 Hz (cheaper than the 0.2s wall poll) to bound /proc overhead.
        if ((GATE_CPU_TIMEOUT_SECONDS > 0 && SECONDS >= cpu_next_sample)); then
            cpu_seconds=$(tree_cpu_seconds "$pid")
            cpu_next_sample=$((SECONDS + 1))
            if ((cpu_seconds >= GATE_CPU_TIMEOUT_SECONDS)); then
                terminate_gate_tree "$pid"
                active_check_pid=""
                printf "Gate exceeded CPU budget: %ss CPU >= %ss budget (wall %ss, subprocess PID %s)\n" \
                    "$cpu_seconds" "$GATE_CPU_TIMEOUT_SECONDS" "$elapsed" "$pid" >>"$log_file"
                printf "🔥 %s exceeded CPU budget: %ss CPU-time >= %ss (wall %ss, subprocess PID %s)\n" \
                    "$name" "$cpu_seconds" "$GATE_CPU_TIMEOUT_SECONDS" "$elapsed" "$pid"
                return 125
            fi
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
    local duration
    local attempt=1
    local max_attempts=$((ENV_BLOCK_MAX_RETRIES + 1))

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

    # Run the check, auto-retrying transient environmental blocks so a host
    # FS-permission denial (BPFJailer banner, or a banner-less EPERM leaked to
    # cc1/cmake/ld) or a vendored third-party (DynamoRIO/elfutils) build flake
    # that kills a build/test subprocess never masquerades as a product failure
    # or an ambiguous early death. Each retry starts a fresh log region so
    # classification only inspects the latest attempt.
    while :; do
        if run_timed_command "$name" "$LOG_FILE" "$timeout_seconds" "$@"; then
            status=0
            duration=$((SECONDS - started_at))
            if ((attempt > 1)); then
                printf "✅ %s (1 passed, 0 failed, %ss; recovered after %s environmental retry attempt(s))\n" \
                    "$name" "$duration" "$((attempt - 1))"
            else
                printf "✅ %s (1 passed, 0 failed, %ss)\n" "$name" "$duration"
            fi
            break
        else
            status=$?
        fi

        duration=$((SECONDS - started_at))

        if is_environmental_block "$output_start"; then
            if ((attempt < max_attempts)); then
                printf "⚠️  %s: ENVIRONMENTAL block (host sandbox FS-permission denial or third-party build flake, not a test failure) on attempt %s/%s — retrying\n" \
                    "$name" "$attempt" "$max_attempts"
                printf "validate.sh: ENVIRONMENTAL block on attempt %s/%s; retrying (not a test failure)\n" \
                    "$attempt" "$max_attempts" >>"$LOG_FILE"
                attempt=$((attempt + 1))
                started_at=$SECONDS
                output_start=$(($(wc -l <"$LOG_FILE") + 1))
                continue
            fi
            # Retries exhausted: fail (non-green) but label unambiguously as an
            # infrastructure block, and tally it separately from test failures.
            failures=$((failures + 1))
            environmental=$((environmental + 1))
            summary=$(failure_summary "$output_start")
            printf "🧱 %s (ENVIRONMENTAL BLOCK after %s attempt(s): host sandbox FS-permission denial (BPFJailer) or vendored third-party (DynamoRIO) build flake, NOT a test failure — validate could not complete; exit %s: %s; full log: %s)\n" \
                "$name" "$max_attempts" "$status" "$summary" "$LOG_FILE"
        else
            failures=$((failures + 1))
            summary=$(failure_summary "$output_start")
            printf "❌ %s (0 passed, 1 failed, exit %s: %s; full log: %s)\n" \
                "$name" "$status" "$summary" "$LOG_FILE"
        fi
        break
    done

    {
        printf "Exit: %s\n" "$status"
        printf "Duration: %ss\n\n" "$duration"
    } >>"$LOG_FILE"
    record_ledger_gate "$name" "$status" "$duration"
    checks=$((checks + 1))
}

function run_check {
    run_check_with_timeout "$GATE_TIMEOUT_SECONDS" "$@"
}

function initialize_repository_submodules {
    local status
    local -a git_command=(git)

    if command -v with-proxy >/dev/null 2>&1; then
        git_command=(with-proxy git)
    fi
    "${git_command[@]}" submodule update --init --recursive
    status=$(git submodule status --recursive)
    printf "%s\n" "$status"
    if grep -Eq '^[-+U]' <<<"$status"; then
        printf "validate.sh: a required submodule is missing or not at its pinned revision\n" >&2
        return 1
    fi
    [[ -f agent-utils/README.md ]] || {
        printf "validate.sh: agent-utils submodule is missing\n" >&2
        return 1
    }
    [[ -f third-party/rr/CMakeLists.txt ]] || {
        printf "validate.sh: rr submodule is missing\n" >&2
        return 1
    }
}

# Independent enforcement of the Reverie dependency pin. `git commit --no-verify`
# bypasses the pre-commit hook, so validate.sh (and therefore every CI profile
# that runs it) must catch a drifted or orphaned pin on its own. The check is
# cheap: the canonical Reverie-pin checker scans tracked Cargo.toml/Cargo.lock and confirms
# the pin is a real commit on rrnewton/reverie:main history. When the nested
# lockfile guard is present (lands separately as rrnewton/hermit#1609) it also
# runs so liteinst-runtime-build/Cargo.lock cannot drift from the root pin.
#
# Run a remaining standalone repository checker WITHOUT requiring the
# `rust-script` interpreter on PATH. The Reverie-pin checker itself always uses
# ci/run-reverie-pin-check.sh, the one canonical rustc launcher shared with the
# DAGs, hosted workflow, hook, Makefile, and LiteInst staging. The helper below
# remains for check-nested-lockfiles.rs until it receives its own launcher.
function run_repo_rust_script {
    local script=$1
    shift
    local -a proxy=()
    if command -v with-proxy >/dev/null 2>&1; then
        proxy=(with-proxy)
    fi
    if command -v rust-script >/dev/null 2>&1; then
        "${proxy[@]}" "$script" "$@"
        return
    fi
    local name binary
    name=$(basename -- "$script" .rs)
    binary="$VALIDATION_TMP_DIR/rust-script-$name"
    if [[ ! -x $binary ]]; then
        printf 'rust-script is not on PATH; compiling %s with rustc instead.\n' "$script"
        rustc --edition=2021 "$script" -o "$binary" || return 1
    fi
    "${proxy[@]}" "$binary" "$@"
}

function validate_reverie_pin_consistency {
    "$ROOT_DIR/ci/run-reverie-pin-check.sh" --repo "$ROOT_DIR" || return 1
    if [[ -x ./scripts/check-nested-lockfiles.rs ]]; then
        run_repo_rust_script ./scripts/check-nested-lockfiles.rs || return 1
    fi
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
        elif is_environmental_block "$output_start"; then
            # Background checks run to completion before collection, so they
            # cannot be retried in place; still label the block unambiguously as
            # infrastructure rather than a test failure (already counted in
            # failures above) and tally it separately.
            environmental=$((environmental + 1))
            summary=$(failure_summary "$output_start")
            printf "🧱 %s (ENVIRONMENTAL BLOCK: host sandbox FS-permission denial (BPFJailer) or vendored third-party (DynamoRIO) build flake, NOT a test failure — validate could not complete; exit %s: %s; full log: %s)\n" \
                "$name" "$status" "$summary" "$LOG_FILE"
        else
            summary=$(failure_summary "$output_start")
            printf "❌ %s (0 passed, 1 failed, exit %s: %s; full log: %s)\n" \
                "$name" "$status" "$summary" "$LOG_FILE"
        fi
        {
            printf "Exit: %s\n" "$status"
            printf "Duration: %ss\n\n" "$duration"
        } >>"$LOG_FILE"
        record_ledger_gate "$name" "$status" "$duration"
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

# NOTE: a former `hermit_determinism_check` reimplemented determinism checking in
# bash (run the guest twice, string-compare stdout). That duplicated the shipped
# `hermit run --verify` path and only compared stdout, so it could pass while the
# product's own verifier (which also compares stderr, the DETLOG event stream, and
# exit status) diverged. It was removed; `hermit_verify_smoke` below is the
# product-backed determinism check for the same echo workload.

function hermit_verify_smoke {
    timeout "$HERMIT_SMOKE_TIMEOUT" \
        "$HERMIT_BIN" "${HERMIT_RUN_ARGS[@]}" --verify -- \
        /bin/echo "$SMOKE_MARKER"
}

function hermit_record_replay_smoke {
    # Delegate to the shipped record/replay verifier instead of reimplementing it
    # in bash. `record start --verify` records the guest, replays the recording,
    # and diffs stdout, stderr, the DETLOG event stream, and exit status, failing
    # nonzero on any divergence. The former bash version only recorded (without
    # --verify), replayed with --autopilot, and `cmp`-ed stdout, so it exercised a
    # strictly weaker check than the product actually ships.
    timeout "$HERMIT_SMOKE_TIMEOUT" \
        "$HERMIT_BIN" record start --verify -- /bin/echo "$SMOKE_MARKER"
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

# The product report is the typed verdict channel. The wrapper exit may instead
# be the deterministic guest's nonzero exit, so it is diagnostic only; parity is
# true exactly when the producer's boolean `bitwise_parity` field is typed true.
function rr_report_has_bitwise_parity {
    local report=$1
    [[ -s $report ]] || return 1
    jq -e '
        type == "object"
        and (.bitwise_parity | type == "boolean")
        and .bitwise_parity == true
    ' "$report" >/dev/null 2>&1
}

# Bracket that load-bearing consumer with producer-shaped reports. In
# particular, verified=true is insufficient for stripped or zero-evidence
# comparisons, while a matched parity report remains authoritative when the
# guest (and therefore the wrapper) exits nonzero.
function rr_report_consumer_self_test {
    local fixture_dir="$VALIDATION_TMP_DIR/rr-report-consumer-self-test"
    local simulated_wrapper_status=3
    mkdir -p "$fixture_dir"

    ! rr_report_has_bitwise_parity "$fixture_dir/missing.json" || return 1

    printf '%s\n' \
        '{"verified":false,"bitwise_parity":false,"verdict":"no_result","comparison":null,"compared_log_messages":null,"guest_exit_code":null,"guest_signal":null}' \
        >"$fixture_dir/no-result.json"
    ! rr_report_has_bitwise_parity "$fixture_dir/no-result.json" || return 1

    printf '%s\n' \
        '{"verified":false,"bitwise_parity":false,"verdict":"diverged","comparison":{"strip_lines":false},"compared_log_messages":{"left":2,"right":2},"guest_exit_code":0,"guest_signal":null}' \
        >"$fixture_dir/diverged.json"
    ! rr_report_has_bitwise_parity "$fixture_dir/diverged.json" || return 1

    printf '%s\n' \
        '{"verified":true,"bitwise_parity":false,"verdict":"matched","comparison":{"strip_lines":true},"compared_log_messages":{"left":2,"right":2},"guest_exit_code":0,"guest_signal":null}' \
        >"$fixture_dir/stripped.json"
    ! rr_report_has_bitwise_parity "$fixture_dir/stripped.json" || return 1

    printf '%s\n' \
        '{"verified":true,"bitwise_parity":false,"verdict":"matched","comparison":{"strip_lines":false},"compared_log_messages":{"left":0,"right":0},"guest_exit_code":0,"guest_signal":null}' \
        >"$fixture_dir/zero-counts.json"
    ! rr_report_has_bitwise_parity "$fixture_dir/zero-counts.json" || return 1

    printf '%s\n' \
        '{"verified":true,"bitwise_parity":true,"verdict":"matched","comparison":{"strip_lines":false},"compared_log_messages":{"left":2,"right":2},"guest_exit_code":0,"guest_signal":null}' \
        >"$fixture_dir/matched.json"
    rr_report_has_bitwise_parity "$fixture_dir/matched.json" || return 1

    printf '%s\n' \
        '{"verified":true,"bitwise_parity":true,"verdict":"matched","comparison":{"strip_lines":false},"compared_log_messages":{"left":2,"right":2},"guest_exit_code":3,"guest_signal":null}' \
        >"$fixture_dir/nonzero-guest.json"
    ((simulated_wrapper_status != 0)) || return 1
    rr_report_has_bitwise_parity "$fixture_dir/nonzero-guest.json" || return 1
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
    local started_at=$SECONDS
    local output_start
    local verify_status
    local summary
    local verify_report="$case_dir/verify.json"

    RR_COMPAT_TOTAL=$((RR_COMPAT_TOTAL + 1))
    mkdir -p "$case_dir"
    {
        printf "=== R/R compatibility: %s ===\n" "$label"
        printf "Verify:"
        printf " %q" "$STRICT_COMPAT_HERMIT_BIN" record start --verify \
            --verify-strict --verify-json "$verify_report" -- "$@"
        printf "\n"
    } >>"$LOG_FILE"
    output_start=$(($(wc -l <"$LOG_FILE") + 1))

    if ((VERBOSE == 1)); then
        printf "  R/R compatibility probe: %s\n" "$label"
    fi

    # Use the shipped strict record/replay verifier as the single source of
    # truth. The JSON artifact binds the verdict to the exact comparison and
    # nonzero evidence counts; wrapper status is not the verdict because a
    # deterministic guest may itself exit nonzero.
    run_rr_compatibility_phase "$case_dir/verify.stdout" "$case_dir/verify.stderr" \
        "$STRICT_COMPAT_HERMIT_BIN" record start --verify --verify-strict \
        --verify-json "$verify_report" -- "$@"
    verify_status=$?

    if rr_report_has_bitwise_parity "$verify_report"; then
        RR_COMPAT_PASSED=$((RR_COMPAT_PASSED + 1))
        printf "  ✅ %-12s PASS R/R (%ss)\n" "$label" "$((SECONDS - started_at))"
        printf "Verify exit (diagnostic): %s; typed bitwise_parity: true\n\n" \
            "$verify_status" >>"$LOG_FILE"
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
        printf "Verify exit (diagnostic): %s; typed bitwise_parity: false\n" \
            "$verify_status"
        if [[ -s $verify_report ]]; then
            printf '%s\n' "--- verify report ---"
            cat "$verify_report"
        fi
        if [[ -s $case_dir/verify.stdout ]]; then
            printf '%s\n' "--- verify stdout ---"
            tail -n 120 "$case_dir/verify.stdout"
        fi
        if [[ -s $case_dir/verify.stderr ]]; then
            printf '%s\n' "--- verify stderr ---"
            tail -n 120 "$case_dir/verify.stderr"
        fi
        printf "\n"
    } >>"$LOG_FILE"
    summary=$(failure_summary "$output_start")
    printf "  ❌ %-12s FAIL R/R (verify %s: %s)\n" \
        "$label" "$verify_status" "$summary"
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
    local sabre_cargo_passed=0
    local sabre_cpp_passed=0
    local sabre_flex_passed=0
    local sabre_gcc_passed=0
    local sabre_gxx_passed=0
    local sabre_ld_passed=0
    local sabre_make_passed=0
    local sabre_rustc_passed=0
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
    if functional_compatibility_probe cargo; then
        passed=$((passed + 1))
        if [[ $COMPATIBILITY_MODE == sabre ]]; then
            sabre_cargo_passed=1
        fi
    else
        failed=$((failed + 1))
    fi
    if defer_portable_strict_diagnostic_to_super rustc; then
        unavailable=$((unavailable + 1))
    else
        if functional_compatibility_probe rustc; then
            passed=$((passed + 1))
            if [[ $COMPATIBILITY_MODE == sabre ]]; then
                sabre_rustc_passed=1
            fi
        else
            failed=$((failed + 1))
        fi
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
        if functional_compatibility_probe gcc; then
            passed=$((passed + 1))
            if [[ $COMPATIBILITY_MODE == sabre ]]; then
                sabre_gcc_passed=1
            fi
        else
            failed=$((failed + 1))
        fi
    fi
    if functional_compatibility_probe g++; then
        passed=$((passed + 1))
        if [[ $COMPATIBILITY_MODE == sabre ]]; then
            sabre_gxx_passed=1
        fi
    else
        failed=$((failed + 1))
    fi
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
    if functional_compatibility_probe cpp; then
        passed=$((passed + 1))
        if [[ $COMPATIBILITY_MODE == sabre ]]; then
            sabre_cpp_passed=1
        fi
    else
        failed=$((failed + 1))
    fi
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
        if ((sabre_cargo_passed != 1)); then
            printf "❌ SaBRe compatibility required row cargo regressed\n"
            return 1
        fi
        if ((sabre_cpp_passed != 1)); then
            printf "❌ SaBRe compatibility required row cpp regressed\n"
            return 1
        fi
        if ((sabre_gcc_passed != 1)); then
            printf "❌ SaBRe compatibility required row gcc regressed\n"
            return 1
        fi
        if ((sabre_gxx_passed != 1)); then
            printf "❌ SaBRe compatibility required row g++ regressed\n"
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
        if ((sabre_rustc_passed != 1)); then
            printf "❌ SaBRe compatibility required row rustc regressed\n"
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
    local test_failures=$((failures - environmental))
    if ((failures == 0)); then
        printf "✅ Validation summary [%s] (%s passed, 0 failed; full log: %s)\n" \
            "$VALIDATION_PROFILE" "$passed" "$LOG_FILE"
    elif ((test_failures == 0)); then
        # Only environmental (sandbox) blocks failed — validate could not
        # complete, but nothing under test is broken. Keep it non-green (never a
        # false pass) while making the cause unambiguous.
        printf "🧱 Validation summary [%s] (%s passed, 0 TEST failures, %s ENVIRONMENTAL block(s) — validate INCOMPLETE due to a host sandbox/third-party-build block, not a product failure; full log: %s)\n" \
            "$VALIDATION_PROFILE" "$passed" "$environmental" "$LOG_FILE"
    elif ((environmental > 0)); then
        printf "❌ Validation summary [%s] (%s passed, %s failed = %s test + %s environmental; full log: %s)\n" \
            "$VALIDATION_PROFILE" "$passed" "$failures" "$test_failures" "$environmental" "$LOG_FILE"
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

    run_check "Centralized test manifest and inventory" ./ci/test_harness.sh validate
    run_check_with_timeout "$timeout_seconds" "$lane CI DAG lane" \
        ./ci/run-dag.sh "$lane" -j "$VALIDATION_DAG_JOBS" -v
}

function run_portable_only_suite {
    run_ci_manifest_lane portable "${CI_PORTABLE_DAG_TIMEOUT_SECONDS:-7200}"
    print_summary
    ((failures == 0))
}

# Resolve the last-known-green baseline commit for --selective. Precedence:
# explicit --baseline, then $HERMIT_LAST_GREEN_SHA, then the most recent passing
# validate-run-ledger entry (preferring this slot). Only a commit that exists
# locally is returned; anything else prints nothing so selection falls back to
# the full lane. Never fail-open on a stale or missing baseline.
function resolve_selective_baseline {
    local sha=""
    # --shallow-select pins the baseline to HEAD~1 (footprint of the newest commit
    # only). If HEAD has no parent (root commit) we emit nothing, so selection
    # fails safe to the full lane.
    if ((SHALLOW_SELECT == 1)); then
        sha=$(git rev-parse --verify HEAD~1 2>/dev/null) || return 0
        [[ -n $sha ]] && printf '%s\n' "$sha"
        return 0
    fi
    if [[ -n ${SELECTIVE_BASELINE:-} ]]; then
        sha=$SELECTIVE_BASELINE
    elif [[ -n ${HERMIT_LAST_GREEN_SHA:-} ]]; then
        sha=$HERMIT_LAST_GREEN_SHA
    elif [[ -n $VALIDATION_LEDGER_FILE && -f $VALIDATION_LEDGER_FILE ]] \
        && command -v jq >/dev/null 2>&1; then
        sha=$(jq -r --arg slot "$VALIDATION_SLOT" '
            select(.result == "pass" and .commit != "unknown" and .slot == $slot)
            | .commit' "$VALIDATION_LEDGER_FILE" 2>/dev/null | tail -n 1)
        if [[ -z $sha ]]; then
            sha=$(jq -r '
                select(.result == "pass" and .commit != "unknown")
                | .commit' "$VALIDATION_LEDGER_FILE" 2>/dev/null | tail -n 1)
        fi
    fi
    [[ -n $sha ]] || return 0
    # select-tests diffs against this commit; trust it only if it exists here.
    if git cat-file -e "${sha}^{commit}" 2>/dev/null; then
        printf '%s\n' "$sha"
    fi
}

# Write a dependency-closed subset of ci/dag/portable.json (keeping only the
# selected nodes and top-level lane config) to $2. Each surviving step's deps are
# pruned to the selected set; because select-tests emits a closed node set, no
# genuine dependency is dropped. Returns non-zero if no step survives.
function build_selected_portable_dag {
    local nodes_csv=$1 out=$2 nodes_json
    nodes_json=$(printf '%s' "$nodes_csv" | tr ', ' '\n\n' \
        | sed '/^$/d' | jq -R . | jq -s .) || return 1
    jq --argjson keep "$nodes_json" '
        ($keep) as $k
        | .steps |= [ .[]
            | (.group + "." + .job) as $tag
            | select($k | index($tag))
            | if (.deps // null) != null
              then .deps |= [ .[] | select($k | index(.)) ]
              else . end ]
    ' "$ROOT_DIR/ci/dag/portable.json" > "$out" || return 1
    [[ $(jq '.steps | length' "$out" 2>/dev/null || echo 0) -gt 0 ]]
}

# --selective / --since-green: run only the portable DAG nodes affected by the
# delta since the last known-green baseline. FAIL-SAFE: a skip decision runs
# nothing, a selective decision runs the dependency-closed subset, and ANY other
# outcome (full, tool error, no baseline, empty/failed subset build) runs the
# complete portable lane — never fewer tests than the tool proved safe to omit.
function run_selective_suite {
    local baseline sel_json decision nodes total dag_override rc
    baseline=$(resolve_selective_baseline)
    local -a sel_args=(--since-green --format json)
    if [[ -n $baseline ]]; then
        sel_args=(--since-green --baseline "$baseline" --format json)
        printf "Selective validation: last-known-green baseline = %s\n" "$baseline"
    else
        printf "Selective validation: no trustworthy green baseline; running the FULL portable lane.\n"
    fi

    if ! sel_json=$("$ROOT_DIR/ci/select-tests.rs" "${sel_args[@]}" 2>>"$LOG_FILE"); then
        printf "Selective validation: select-tests.rs failed; running the FULL portable lane.\n"
        run_portable_only_suite
        return $?
    fi
    decision=$(printf '%s' "$sel_json" | jq -r '.decision // "full"' 2>/dev/null || echo full)
    total=$(jq '.steps | length' "$ROOT_DIR/ci/dag/portable.json")

    # Transparent coverage report: select-tests.rs --format human enumerates the
    # baseline/evidence, every changed path, the selector reason, and every
    # SKIPPED portable node / test shard / e2e cell. Inability to produce that
    # report is treated as doubt and runs the FULL lane — a subset must never run
    # without a human-auditable account of what it dropped and why.
    local -a human_args=(--since-green --format human)
    [[ -n $baseline ]] && human_args=(--since-green --baseline "$baseline" --format human)
    local coverage_report
    if ! coverage_report=$("$ROOT_DIR/ci/select-tests.rs" "${human_args[@]}" 2>>"$LOG_FILE") \
        || [[ -z $coverage_report ]]; then
        printf "Selective validation: could not produce the coverage report; running the FULL portable lane.\n"
        run_portable_only_suite
        return $?
    fi
    printf -- '----- selective coverage report (skipped nodes/shards/e2e cells + reasons) -----\n'
    printf '%s\n' "$coverage_report"
    printf -- '-------------------------------------------------------------------------------\n'

    case "$decision" in
        skip)
            printf "Selective validation: no CI-relevant changes since baseline — nothing to run (0/%s nodes).\n" \
                "$total"
            print_summary
            ((failures == 0))
            return $?
            ;;
        selective)
            nodes=$(printf '%s' "$sel_json" | jq -r '.nodes | join(",")' 2>/dev/null || echo "")
            if [[ -z $nodes ]]; then
                printf "Selective validation: empty selected node set — running the FULL portable lane.\n"
                run_portable_only_suite
                return $?
            fi
            dag_override="$VALIDATION_TMP_DIR/portable-selective.json"
            if ! build_selected_portable_dag "$nodes" "$dag_override"; then
                printf "Selective validation: could not build subset DAG; running the FULL portable lane.\n"
                run_portable_only_suite
                return $?
            fi
            printf "Selective validation: running %s/%s portable DAG nodes:\n  %s\n" \
                "$(printf '%s' "$sel_json" | jq -r '.node_count')" "$total" \
                "${nodes//,/ }"
            run_check "Centralized test manifest and inventory" ./ci/test_harness.sh validate
            export RUN_DAG_FILE_OVERRIDE="$dag_override"
            run_check_with_timeout "${CI_PORTABLE_DAG_TIMEOUT_SECONDS:-7200}" \
                "portable CI DAG (selective subset)" \
                ./ci/run-dag.sh portable -j "$VALIDATION_DAG_JOBS" -v
            rc=$?
            unset RUN_DAG_FILE_OVERRIDE
            print_summary
            ((failures == 0))
            return $?
            ;;
        *)
            printf "Selective validation: decision=%s — running the FULL portable lane.\n" "$decision"
            run_portable_only_suite
            return $?
            ;;
    esac
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
            cargo test -p hermit-detcore --test "$target" "$test_name" -- --exact --test-threads=1
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
        cargo test -p hermit --features third-party-backends --test analyze "$@"
}

function run_privileged_validation {
    run_ci_manifest_lane privileged "${CI_PRIVILEGED_DAG_TIMEOUT_SECONDS:-7200}"
    print_summary
    ((failures == 0))
}

function run_quick_suite {
    run_check "Build workspace" cargo build --workspace --features third-party-backends
    run_check "Portable E2E metadata" ./ci/test_harness.sh validate
    run_check "Portable ptrace E2E verification" \
        ./ci/test_harness.sh run --lane portable --mode verify --backend ptrace --ci-only
    run_check "Detcore core unit tests" cargo test -p hermit-detcore --lib
    run_check "Hermit run smoke test" hermit_run_smoke
    run_check "Hermit verify-mode smoke test" hermit_verify_smoke
    run_check "Hermit record/replay smoke test" hermit_record_replay_smoke
}

function run_full_suite {
    run_ci_manifest_lane portable "${CI_PORTABLE_DAG_TIMEOUT_SECONDS:-7200}"
    run_ci_manifest_lane privileged "${CI_PRIVILEGED_DAG_TIMEOUT_SECONDS:-7200}"
    # Both lanes ran to completion, so every gate in the full plan has been
    # recorded (run_check is not fail-fast). This authorizes deriving the
    # expected gate count from the observed gates_run at ledger-write time.
    VALIDATION_SUITE_COMPLETE=1
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
    run_check_with_timeout 1800 "Relaxed Hermit flag matrix" \
        env HERMIT_FLAG_MATRIX_REPORT="$ROOT_DIR/target/relaxed-flag-matrix/results.tsv" \
        cargo test -p hermit --features third-party-backends --test relaxed_flag_matrix \
        meaningful_flag_combinations_run_without_crashing -- --exact --ignored --test-threads=1 --nocapture
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
        cargo test -p hermit --features third-party-backends --test pselect6_simulation -- --test-threads=1
    # AUTONOMOUS-BOT-IMPLEMENTED
    # TODO-HUMAN-REVIEW(#678)
    run_check_with_timeout 300 "Record/replay matrix diagnostic" \
        cargo test -p hermit --features third-party-backends --test record_replay record_replay_matrix -- --exact --test-threads=1
    # AUTONOMOUS-BOT-IMPLEMENTED
    # TODO-HUMAN-REVIEW(#657)
    run_check_with_timeout 300 "Managed JVM strict-verify diagnostics" \
        env HERMIT_APP_VERIFY_TIMEOUT=20s RUST_BACKTRACE=1 \
        cargo test -p hermit --features third-party-backends --test app_strict_verify java -- --ignored --test-threads=1 --nocapture
    run_check_with_timeout 180 "Post-fork scheduling diagnostics" \
        cargo test -p hermit-detcore --test tests_misc ordinary_clone_ -- --test-threads=1
    run_check_with_timeout 180 "Network syscall determinism diagnostic" \
        cargo test -p hermit-detcore --test tests_misc network_syscalls_are_deterministic_across_five_runs -- --exact --test-threads=1
    run_check_with_timeout 180 "IPC determinism diagnostic" \
        cargo test -p hermit --features third-party-backends --test ipc_determinism ipc_patterns_are_deterministic_across_five_runs -- --exact --test-threads=1
    run_check_with_timeout 180 "Random-source determinism diagnostic" \
        cargo test -p hermit --features third-party-backends --test random_determinism random_sources_repeat_across_runs_and_change_with_seed -- --exact --test-threads=1
    run_check_with_timeout 300 "Threaded integration matrix diagnostic" \
        cargo test -p hermit --features third-party-backends --test integration_matrix -- --test-threads=1
    run_check_with_timeout 300 "LiteInst python3 verify diagnostics" \
        cargo test -p hermit --features third-party-backends --test cli -- \
        run_liteinst_rejects_non_fork_clone \
        run_liteinst_handles_inherited_ignored_sigchld \
        run_liteinst_verifies_forked_guest \
        run_liteinst_verifies_raw_fork_guest --test-threads=1
    run_check_with_timeout 300 "Chaos hello-race verification diagnostic" \
        cargo test -p hermit --features third-party-backends --test hermit_modes hello_race_chaos_verify -- --exact --test-threads=1
    # AUTONOMOUS-BOT-IMPLEMENTED
    # TODO-HUMAN-REVIEW(#598)
    run_check_with_timeout 300 "DBI pipe backpressure diagnostic" \
        cargo test -p hermit --features third-party-backends --test cli run_dbi_verifies_pipe_backpressure -- --exact --test-threads=1
    # AUTONOMOUS-BOT-IMPLEMENTED
    # TODO-HUMAN-REVIEW(#736): Review weekly routing for the DBI failed-exec stall.
    run_check_with_timeout 180 "DBI failed-exec recovery diagnostic" \
        cargo test -p hermit --features third-party-backends --test cli run_dbi_recovers_after_failed_exec -- --exact --test-threads=1
    # This test exercises verify, tampered reports, fork/exec, and strict DBI
    # teardown in one case. Keep its coverage, but do not let a backend
    # lifecycle deadlock consume the portable PR gate.
    run_check_with_timeout 180 "DBI unsupported-syscall aggregation diagnostic" \
        cargo test -p hermit --features third-party-backends --test cli run_dbi_aggregates_unsupported_syscalls_and_strict_rejects_them -- --exact --test-threads=1
    run_check_with_timeout 30 "DBI strict blocked-stdin teardown diagnostic" \
        cargo test -p hermit --features third-party-backends --test cli run_dbi_strict_returns_with_blocked_stdin_source -- --exact --test-threads=1
    run_check_with_timeout 120 "DBI guest-stderr isolation diagnostic" \
        cargo test -p hermit --features third-party-backends --test cli run_dbi_keeps_diagnostics_out_of_guest_stderr -- --exact --test-threads=1
}

function run_super_suite {
    local leveldb_install="$ROOT_DIR/target/hermit-leveldb-super"
    local leveldb_build="$ROOT_DIR/target/hermit-leveldb-build-super"

    run_check "Build workspace" cargo build --workspace --features third-party-backends
    run_check "Build release Hermit" cargo build --release -p hermit --features third-party-backends
    run_super_diagnostic_suite
    run_check "Super repeated determinism probes" run_super_stress_suite
    if [[ -s $VALIDATION_TMP_DIR/super-report ]]; then
        printf "\n== Super stress pass rates ==\n"
        cat "$VALIDATION_TMP_DIR/super-report"
    fi
    run_check "Weekly relaxed default-mode cases" cargo test -p hermit --features third-party-backends --test hermit_modes default_ -- --test-threads=1
    run_check "Weekly portable chaos cases" cargo test -p hermit --features third-party-backends --test stress_suite -- --skip slow_cas_search_and_replay --test-threads=1
    run_check "Weekly ignored portable chaos cases" cargo test -p hermit --features third-party-backends --test stress_suite -- --ignored --skip slow_cas_search_and_replay --test-threads=1
    run_check "PMU Buck chaos cases" cargo test -p hermit --features third-party-backends --test hermit_modes chaos_buck_ -- --ignored --test-threads=1
    run_check "PMU analyze hello-race stress (calibrated skid)" \
        run_calibrated_analyze_tests analyze_hello_race -- --exact --ignored --test-threads=1
    run_check "Build pinned LevelDB super fixture" ./hermit-cli/tests/prepare_leveldb.sh "$leveldb_install" "$leveldb_build"
    run_check "Full LevelDB strict determinism" env HERMIT_LEVELDB_BUILD_DIR="$leveldb_build" cargo test -p hermit --features third-party-backends --test leveldb full_leveldb_suite_is_deterministic_under_strict -- --exact --ignored --test-threads=1
    run_check "SQLite veryquick strict determinism" cargo test -p hermit --features third-party-backends --test sqlite_veryquick sqlite_veryquick_is_deterministic_under_strict_hermit -- --exact --ignored --test-threads=1
}

# Run both semantic policy brackets before any authority-bearing validation
# gate. They are inert fixtures and cannot publish a receipt themselves.
if ! rr_report_consumer_self_test; then
    printf "validate.sh: record/replay report consumer brackets failed\n" >&2
    exit 2
fi

# The archival pin is not a testing exemption: validate always proves it equals
# the live Reverie main tip before initializing dependencies or running tests.
run_check "Reverie dependency pin equals latest main" \
    "$ROOT_DIR/ci/run-reverie-pin-check.sh" --repo "$ROOT_DIR"
if ((failures != 0)); then
    print_summary
    exit 1
fi
REVERIE_PIN_GATE_PASSED=1

# Keep direct ./validate.sh invocations as self-sufficient as `make validate`.
# This initializes Hermit's registered submodules; Cargo's pinned Reverie build
# script separately materializes its nested DynamoRIO checkout.
run_check "Initialize repository submodules" initialize_repository_submodules
# Fail fast on Reverie pin drift before any heavy build/test work. Independent of
# the pre-commit hook, which git commit --no-verify can bypass.
run_check "Reverie pin consistency" validate_reverie_pin_consistency
if ((failures != 0)); then
    print_summary
    exit 1
fi

# --only is the first-class fast path for one already-built DAG shard. Run it
# through the normal gate wrapper so it receives the same log and parent-ledger
# accounting as every other validation profile.
if ((ONLY_MODE == 1)); then
    run_check "DAG shard ${ONLY_LANE}:${ONLY_NODES}" \
        "$ROOT_DIR/ci/run-node.sh" "$ONLY_LANE" "$ONLY_NODES"
    print_summary
    ((failures == 0))
    exit $?
fi

# --selective / --since-green: run only the portable DAG nodes affected by the
# delta since the last known-green baseline, falling back to the full portable
# lane on any doubt (see run_selective_suite for the fail-safe rules).
if ((SELECTIVE_MODE == 1)); then
    run_selective_suite
    exit $?
fi

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
            cargo build --release -p hermit --features third-party-backends
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

if ((LITEINST_COMPAT_ONLY == 1)); then
    run_check_with_timeout 1200 "Build release Hermit for LiteInst compatibility" \
        cargo build --release --locked -p hermit --features third-party-backends
    if ((failures == 0)); then
        run_check_with_timeout 900 "Build release LiteInst runtime" \
            "$ROOT_DIR/scripts/stage-liteinst-runtime.sh" release \
            "$ROOT_DIR/target/release/libreverie_liteinst.so" \
            "$ROOT_DIR/target/liteinst-runtime-build"
    fi
    if ((failures == 0)); then
        run_check_with_timeout 900 "Portable CI liteinst_strict" \
            env HERMIT_LITEINST_TEST_BINARY="$ROOT_DIR/target/release/hermit" \
            cargo test -p hermit --features third-party-backends --test liteinst_advanced -- --test-threads=1
    fi
    print_summary
    ((failures == 0))
    exit $?
fi

if ((SABRE_COMPAT_ONLY == 1)); then
    run_check "SaBRe artifacts configured" require_sabre_artifacts
    if ((failures == 0)); then
        run_check "Build release Hermit and Detcore plugin for SaBRe compatibility" \
            cargo build --release -p hermit --features third-party-backends -p detcore-sabre
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
            cargo build --release -p hermit --features third-party-backends
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
        cargo build --release -p hermit --features third-party-backends
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
        cargo build --release -p hermit --features third-party-backends
    if ((failures == 0)); then
        run_check "QEMU strict L2 boot (heavyweight)" \
            ./tests/qemu-boot/strict_l2_test.sh
    fi
    print_summary
    ((failures == 0))
    exit $?
fi

if [[ $ENVELOPE_MODE == only ]]; then
    run_check "Build workspace for envelope measurement" cargo build --workspace --features third-party-backends
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

((failures == 0))
