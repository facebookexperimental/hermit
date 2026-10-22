#!/usr/bin/env bash
#
# CI node entrypoint for `make lint-checks`.
#
# WHY THIS WRAPPER EXISTS: the target's checkers need initialized submodules --
# ci/verify-submodules.sh inspects them directly, and run-reverie-pin-check.sh and
# check-nested-lockfiles.rs both run under $(SUBMODULE_PROXY) against the pinned
# trees. A freshly created linked worktree has NO submodules initialized, so
# verify-submodules.sh reports a leading '-' inventory and exits 1.
#
# That exit is a SETUP condition, not a product failure, and letting it land as a
# `fail` is the third-state bug: an abort that reads like a failure. scripts/
# validate.rs:5362 reserves exit 75 (EX_TEMPFAIL) as "the only nonzero code that is
# not a product failure"; outcome_is_no_result() classifies it `no_result` and
# outcome_is_failure() excludes it. ci/run-with-reverie-dbt-budget.sh already uses
# 75 for its pin-mismatch refusal, so this follows the established spelling rather
# than inventing one. Every other nonzero exit from the target stays loud.
#
# The distinction is load-bearing in practice, not hypothetical: an agent hit this
# exact abort in a fresh worktree on 2026-08-25 and had to reason it out by hand to
# call it environment rather than a red.
set -euo pipefail

# Classify a `git submodule status` inventory. Each entry is marked: leading '-'
# uninitialized, '+' a revision other than the pin, 'U' a merge conflict.
#
# ONLY '-' IS A SETUP CONDITION. '+' and 'U' are real drift and must fall through
# to verify-submodules.sh and be reported as an ordinary failure -- classifying
# those as no_result would silence exactly the drift the checker exists to catch.
# Reads the inventory on stdin; echoes "ok", "empty", or "uninitialized <n>".
classify_inventory() {
    local inventory uninitialized
    inventory="$(cat)"
    if [ -z "${inventory//[[:space:]]/}" ]; then
        echo 'empty'
        return 0
    fi
    uninitialized="$(printf '%s\n' "$inventory" | grep -c '^-' || true)"
    if [ "${uninitialized:-0}" -ne 0 ]; then
        echo "uninitialized ${uninitialized}"
        return 0
    fi
    echo 'ok'
}

# The job count for a run that does not set HERMIT_LINT_CHECK_JOBS: one job per
# checker, bounded by the cores and by the memory available for checkers.
# Arguments: checker count, cores, available KiB, KiB reserved per job. Echoes N.
default_lint_jobs() {
    local targets="$1" cores="$2" available_kib="$3" per_job_kib="$4" jobs
    jobs=$targets
    [ "$cores" -lt "$jobs" ] && jobs=$cores
    [ $((available_kib / per_job_kib)) -lt "$jobs" ] && jobs=$((available_kib / per_job_kib))
    [ "$jobs" -lt 1 ] && jobs=1
    echo "$jobs"
}

# -Otarget holds a checker's output until it finishes, so a hung checker would
# print nothing. ci/lint-checks-recipe-shell.sh writes a started and a finished
# line for every recipe line to fd 9, which is this node's stderr: outside
# make's buffering and outside $node_out, so classify_run never reads them.
# A node started with stderr closed still runs its checkers; it only loses the
# trace lines. (Do not silence the first exec with `2>/dev/null`: that points
# fd 2, and so fd 9, at /dev/null and discards every trace line.)
open_trace_fd() {
    exec 9>&2 || exec 9>/dev/null
}
_recipe_shell="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/lint-checks-recipe-shell.sh"
# shellcheck disable=SC2016 # $@ is make's automatic variable, expanded by make.
trace_make_args=(
    --eval "lint-check-%: SHELL := ${_recipe_shell}"
    --eval 'lint-check-%: export LINT_CHECK_TARGET = $@'
)

_node_lib="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/node-run-classification.sh"
# shellcheck source=ci/node-run-classification.sh
. "$_node_lib"


self_test() {
    local got failures=0
    check_case() {
        local name="$1" want="$2" input="$3"
        got="$(printf '%s' "$input" | classify_inventory)"
        if [ "$got" != "$want" ]; then
            echo "FAIL: ${name}: expected '${want}', got '${got}'" >&2
            failures=$((failures + 1))
        fi
    }

    check_case 'clean inventory'   'ok'               ' abc123 agent-utils (v1)
 def456 third-party/rr (5.9.0)'
    check_case 'uninitialized'     'uninitialized 1'  '-def456 third-party/rr'
    check_case 'two uninitialized' 'uninitialized 2'  '-abc123 agent-utils
-def456 third-party/rr'
    check_case 'empty inventory'   'empty'            ''
    check_case 'whitespace only'   'empty'            '   '
    # The negative arm, and the one that matters most: drift and conflicts are
    # NOT setup conditions and must not be reported as no_result.
    check_case 'drift is not setup'    'ok' '+def456 third-party/rr (5.9.0-1)'
    check_case 'conflict is not setup' 'ok' 'Udef456 third-party/rr'
    check_case 'mixed drift+clean'     'ok' ' abc123 agent-utils
+def456 third-party/rr'
    # A '-' anywhere in the inventory still wins, including alongside drift.
    check_case 'mixed uninit+drift' 'uninitialized 1' '+abc123 agent-utils
-def456 third-party/rr'

    # ---- classify_run: the pass / no_result / fail split -------------------
    #
    # This branch decides what the NODE REPORTS, and until now it was the one
    # thing in this target not covered by anything. Each case below is planted so
    # that it FAILS if the classification regresses -- in particular the first two
    # fail under the unanchored `grep -q "$NO_RESULT_MARKER"` this replaced, and
    # the last two fail if a no_result is ever allowed to outrank a real failure.
    local tmp
    tmp="$(mktemp)" || return 1
    check_run() {
        local name="$1" want="$2" rc="$3" body="$4"
        printf '%s\n' "$body" > "$tmp"
        got="$(classify_run "$rc" "$tmp")"
        if [ "$got" != "$want" ]; then
            echo "FAIL: ${name}: expected '${want}', got '${got}'" >&2
            failures=$((failures + 1))
        fi
    }

    # ⚠️ THE CASE THAT MUST NOT MATCH. A checker that SUCCEEDS while echoing a line
    # that merely mentions the marker -- scanning a source file, quoting the
    # convention, or testing this very feature -- must stay a pass. Unanchored,
    # this returns no_result and silently converts a green run into an invisible
    # one.
    check_run 'marker quoted mid-line is not a no_result' 'pass' 0 \
        "shellcheck: scanning ci/lint-checks-node.sh for NO-RESULT-CASE: handling
lint-checks: OK"
    check_run 'marker indented is not a no_result' 'pass' 0 \
        "    NO-RESULT-CASE: quoted inside an indented block"
    # ⚠️ AND THE CASE THAT MUST MATCH, so the anchor cannot be "fixed" by making it
    # never fire. This is the exact shape the producer emits: column 0, on its own
    # line, with make's recipe echo above it.
    check_run 'genuine emission at column 0 is a no_result' 'no_result' 0 \
        "python3 scripts/example-producer-not-a-real-checker.py
NO-RESULT-CASE: canonical adapter contract, accept arm: no parent adapter
PARTIAL: every evaluable assertion passed"
    check_run 'clean run is a pass' 'pass' 0 "lint-checks: everything passed"
    # ⚠️ A NO_RESULT MUST NEVER SWALLOW A RED. Both arms: a plain failure, and a
    # failure that ALSO carries a genuine marker. The second is the one that
    # matters -- if the order of these two tests is ever inverted, a real red is
    # reported as an invisible no_result, which is the defect this entire marker
    # channel is downstream of.
    check_run 'a failure is a failure' 'fail 2' 2 "make: *** [lint-checks] Error 1"
    check_run 'a failure outranks a genuine marker' 'fail 2' 2 \
        "NO-RESULT-CASE: canonical adapter contract, accept arm: no parent adapter
make: *** [lint-checks] Error 1"
    check_run 'a failure outranks it whatever the code' 'fail 75' 75 \
        "NO-RESULT-CASE: something unevaluable"
    rm -f "$tmp"

    # ---- default_lint_jobs: one job per checker unless cores or memory bind ----
    check_jobs() {
        local name="$1" want="$2"
        shift 2
        got="$(default_lint_jobs "$@")"
        if [ "$got" != "$want" ]; then
            echo "FAIL: ${name}: expected ${want} jobs, got '${got}'" >&2
            failures=$((failures + 1))
        fi
    }
    # More checkers than eight must get more jobs than eight: a fixed cap here
    # would serialize checkers the host has room for.
    check_jobs 'checkers bind'  44 44 316 900000000 262144
    check_jobs 'cores bind'     16 44 16  900000000 262144
    check_jobs 'memory binds'   4  44 316 1048576   262144
    check_jobs 'never zero'     1  44 316 1000      262144

    # ---- trace lines: on the node's stderr, never in make's captured output ----
    local tdir
    tdir=$(mktemp -d)
    # lint-check-env fails if the trace plumbing reaches the checker itself:
    # the target name, the descriptor variable, or descriptor 9.
    {
        printf 'lint-check-ok:\n\t@echo out-ok\n'
        printf 'lint-check-bad:\n\t@exit 7\n'
        # shellcheck disable=SC2016 # $$ is make's escape for a shell $.
        printf 'lint-check-env:\n\t@%s\n' \
            'if [ -n "$${LINT_CHECK_TARGET+x}$${LINT_CHECKS_TRACE_FD+x}" ] || [ -e /proc/self/fd/9 ]; then exit 9; fi; echo env-clean'
    } >"$tdir/Makefile"
    trace_run() {
        open_trace_fd
        export LINT_CHECKS_TRACE_FD=9
        env -u MAKEFLAGS -u MFLAGS -u MAKELEVEL make -C "$tdir" --no-print-directory \
            -k -Otarget lint-check-ok lint-check-bad lint-check-env "${trace_make_args[@]}" \
            >"$tdir/out" 2>&1
    }
    (trace_run) 2>"$tdir/err" || :  # lint-check-bad makes make exit 2
    for want in 'lint-checks: started  lint-check-ok: echo out-ok' \
        'lint-checks: finished lint-check-ok: exit 0 after' \
        'lint-checks: finished lint-check-bad: exit 7 after' \
        'lint-checks: finished lint-check-env: exit 0 after'; do
        if ! grep -qF -- "$want" "$tdir/err"; then
            echo "FAIL: trace: stderr lacks '${want}'" >&2
            failures=$((failures + 1))
        fi
    done
    if grep -q 'lint-checks:' "$tdir/out"; then
        echo 'FAIL: trace: a trace line reached the captured output classify_run reads' >&2
        failures=$((failures + 1))
    fi
    if ! grep -qx 'out-ok' "$tdir/out" || ! grep -q 'lint-check-bad.*Error 7' "$tdir/out"; then
        echo 'FAIL: trace: the recipes did not run as make would run them' >&2
        failures=$((failures + 1))
    fi
    if ! grep -qx 'env-clean' "$tdir/out"; then
        echo 'FAIL: trace: the checker saw the target name, the descriptor variable, or descriptor 9' >&2
        failures=$((failures + 1))
    fi
    rm -f "$tdir/out"
    (trace_run) 2>&- || :
    if ! grep -qx 'out-ok' "$tdir/out"; then
        echo 'FAIL: trace: with stderr closed the checkers did not run' >&2
        failures=$((failures + 1))
    fi
    # Descriptor 9 on a pipe nobody reads: every trace write fails with EPIPE or
    # SIGPIPE, and the checkers must still run. The read end opened with the
    # FIFO is closed before make starts, so no reader exists at any write.
    local keep_fd write_fd
    rm -f "$tdir/out"
    mkfifo "$tdir/fifo"
    exec {keep_fd}<>"$tdir/fifo"
    exec {write_fd}>"$tdir/fifo"
    exec {keep_fd}>&-
    (trace_run) 2>&"$write_fd" || :
    exec {write_fd}>&-
    if ! grep -qx 'out-ok' "$tdir/out" || ! grep -qx 'env-clean' "$tdir/out"; then
        echo 'FAIL: trace: with stderr on a pipe nobody reads the checkers did not run' >&2
        failures=$((failures + 1))
    fi
    rm -rf -- "$tdir"

    if [ "$failures" -ne 0 ]; then
        echo "lint-checks-node --self-test: ${failures} case(s) failed" >&2
        return 1
    fi
    echo 'PASS: lint-checks-node classifies uninitialized as no_result, drift/conflict as failure,'
    echo '      a quoted marker as pass, a column-0 marker as no_result, and never lets a marker'
    echo '      outrank a real failure, defaults to one job per checker unless cores or memory bind,'
    echo '      and writes trace lines to stderr and never into the captured output'
}

if [ "${1:-}" = '--self-test' ]; then
    if [ "$#" -ne 1 ]; then
        echo 'lint-checks-node: --self-test accepts no other argument' >&2
        exit 2
    fi
    self_test
    exit $?
fi

# This parameter carries only a SHA already selected by the driver. It neither
# grants admission nor changes the ordinary standalone checker policy.
pin_args=()
if [ "$#" -ne 0 ]; then
    if [ "$#" -ne 2 ] || [ "$1" != '--reverie-pin-base-ref' ] || ! [[ "$2" =~ ^[0-9a-f]{40}$ ]]; then
        echo 'usage: ci/lint-checks-node.sh [--reverie-pin-base-ref FULL_SHA]' >&2
        exit 2
    fi
    pin_args=("VALIDATE_REVERIE_PIN_BASE_REF=$2")
fi
if [[ ${VALIDATE_REVERIE_PIN_BASE_REF+x} || ${MAKEFLAGS:-}${MFLAGS:-}${MAKEOVERRIDES:-} == *VALIDATE_REVERIE_PIN_BASE_REF* ]]; then
    echo 'lint-checks-node: an ambient admission floor is not authority' >&2
    exit 2
fi

cd "$(dirname "$0")/.."

verdict="$(git submodule status 2>/dev/null | classify_inventory)"
case "$verdict" in
    empty)
        echo 'lint-checks: NO RESULT -- `git submodule status` produced no inventory;' >&2
        echo '  cannot establish whether the submodule precondition holds.' >&2
        exit 75
        ;;
    uninitialized*)
        echo "lint-checks: NO RESULT -- ${verdict#uninitialized } submodule(s) not initialized." >&2
        echo '  This is a SETUP condition, not a lint failure: the checkers in this' >&2
        echo '  target read the pinned submodule trees. Run `make checkout-all` (or' >&2
        echo '  `git submodule update --init`) in this checkout and re-run the node.' >&2
        git submodule status | sed 's/^/    /' >&2
        exit 75
        ;;
esac

# ⚠️ THE PARENT-ADAPTER PRECONDITION IS CLASSIFIED AFTER THE TARGET RUNS, NOT BEFORE.
# An earlier version exited 75 HERE, before `make lint-checks`, and so skipped every
# checker in the target -- 17 of them -- for a precondition that affects one arm of
# one case in one of them. It shipped saying "Every other checker in this target is
# unaffected and would have run", which was false: none of them ran. That is worse
# than the false main-red it was fixing, and it landed one day after this node was
# created precisely so those checkers would be gated by construction.
#
# So: run the whole target. If the target SUCCEEDED but something announced itself
# unevaluated on a line starting with the machine-readable prefix, the run as a
# whole is a no_result: everything that could be checked was checked and passed,
# and something could not be checked.
#
# The canonical adapter accept arm is NO LONGER one of those announcements. This
# node runs in lanes with no dev-hermit parent (hosted-portable on every GitHub
# runner), and an arm that can never be evaluated in a lane made this node a
# permanent no_result there, which the hosted gate reads as red: that was
# https://github.com/rrnewton/hermit/actions/runs/36550265580. The target now runs
# scripts/test_validate_stop_paths.py --exclude-canonical-adapter-accept-arm, which
# prints that the arm is NOT COVERED by this run and where it is. The arm is its
# own DAG node, check.canonical_adapter_accept, labelled `full` only -- the lane
# whose checkouts sit under the parent -- where a missing parent is still exit 75
# and a failure is still a failure. The exclude mode itself refuses unless that
# node is present, is the only owner of the arm, is `full`-only, and runs the
# script directly rather than through make.
#
# Any real failure still propagates unchanged -- a nonzero from make is a failure,
# never a no_result, because a no_result must not be able to swallow a red.
#
# ⚠️ WHY 75 AND NOT A NEW CODE. scripts/validate.rs recognises exactly one no-result
# value -- NO_RESULT_EXIT_CODE = 75, matched by outcome_is_no_result() and excluded
# by outcome_is_failure() -- so any other number is classified a FAILURE and would
# reintroduce the false main-red. The code space has one slot. (The general rule
# about not collapsing two conditions into one code is stated in
# ci-hub/bin/gh-merge-verified in the DEV-HERMIT PARENT repository; this repository
# has no ci-hub/ directory, so that path does not resolve from here.)

# THE CHECKERS RUN CONCURRENTLY. lint-checks is one make target per checker, so
# -j runs them side by side; serially the node's wall time was the sum of their
# CPU time (about 366 s, measured 2026-10-04). The DAG sets HERMIT_LINT_CHECK_JOBS
# to the node's CPU width.
#
# A run without it uses one job per checker, bounded by the cores and by memory
# (default_lint_jobs). Measured 2026-10-05 at 32e2d4bb3e on the lint-checks
# host in docs/TESTING_ENVIRONMENTS.md, "Named measurement hosts", 44 checkers: the node's anonymous memory peaked at 1.46 GB with -j8 and 4.18 GB
# with -j44, and the largest single checker process, in the lint-check-shellcheck
# recipe, reached 230 MiB. So each job is budgeted 256 MiB, against MemAvailable or,
# inside a cgroup with a memory.max, the room left under it.
#   -k        keep going, so every failing checker is reported, not only the first.
#   -Otarget  print each checker's output in one piece when it finishes. This is
#             what keeps a NO-RESULT-CASE marker at column 0 on its own line for
#             classify_run; interleaved output could split it.
# make names each failing target (`[Makefile:N: lint-check-<name>] Error 1`), and
# its exit status is still nonzero when any checker fails.
lint_job_kib=262144
if [ -n "${HERMIT_LINT_CHECK_JOBS:-}" ]; then
    jobs=$HERMIT_LINT_CHECK_JOBS
else
    target_count=$(env -u MAKEFLAGS -u MFLAGS -u MAKELEVEL make -s --no-print-directory \
        --eval '_lint_check_target_count: ; @echo $(words $(LINT_CHECK_TARGETS))' \
        _lint_check_target_count)
    if ! [[ "$target_count" =~ ^[1-9][0-9]*$ ]]; then
        echo "lint-checks-node: could not count LINT_CHECK_TARGETS, got '${target_count}'" >&2
        exit 2
    fi
    available_kib=$(awk '$1 == "MemAvailable:" { print $2 }' /proc/meminfo)
    # A memory.max on any enclosing cgroup bounds this node, not only the
    # innermost one (a run-*.scope usually sits under a limited slice), so take
    # the least room left at any level up to the root.
    cgroup_dir=/sys/fs/cgroup$(sed -n 's/^0:://p' /proc/self/cgroup)
    while :; do
        if [ -r "$cgroup_dir/memory.max" ] && [ -r "$cgroup_dir/memory.current" ]; then
            cgroup_max=$(cat "$cgroup_dir/memory.max")
            if [[ "$cgroup_max" =~ ^[0-9]+$ ]]; then
                cgroup_room_kib=$(( (cgroup_max - $(cat "$cgroup_dir/memory.current")) / 1024 ))
                [ "$cgroup_room_kib" -lt "$available_kib" ] && available_kib=$cgroup_room_kib
            fi
        fi
        case "$cgroup_dir" in
            /sys/fs/cgroup/*) cgroup_dir=${cgroup_dir%/*} ;;
            *) break ;;
        esac
    done
    jobs=$(default_lint_jobs "$target_count" "$(nproc)" "$available_kib" "$lint_job_kib")
    echo "lint-checks: ${target_count} checkers, $(nproc) cores, $((available_kib / 1024)) MiB available at $((lint_job_kib / 1024)) MiB per job"
fi
if ! [[ "$jobs" =~ ^[1-9][0-9]*$ ]]; then
    echo "lint-checks-node: HERMIT_LINT_CHECK_JOBS must be a positive integer, got '${jobs}'" >&2
    exit 2
fi
echo "lint-checks: running the checkers with make -j${jobs} -k -Otarget"

open_trace_fd
export LINT_CHECKS_TRACE_FD=9

node_out=$(mktemp) || exit 1
trap 'rm -f "$node_out"' EXIT
set +e
make -j"$jobs" -k -Otarget lint-checks "${pin_args[@]}" \
    "${trace_make_args[@]}" 2>&1 | tee "$node_out"
pipeline_status=("${PIPESTATUS[@]}")
set -e
make_rc=${pipeline_status[0]}
tee_rc=${pipeline_status[1]}

# The capture file is the input to classify_run. A tee failure can leave it
# empty or partial even though marker text reached the terminal, which would
# otherwise turn an unevaluable run into pass. Any capture failure is a real
# node failure; preserve make's status when both commands fail.
if [ "$tee_rc" -ne 0 ]; then
    echo "lint-checks: output capture failed with exit ${tee_rc}" >&2
    if [ "$make_rc" -eq 0 ]; then
        make_rc=$tee_rc
    fi
fi
verdict="$(classify_run "$make_rc" "$node_out")"
case "$verdict" in
    fail*)
        exit "${verdict#fail }"
        ;;
    no_result)
        echo "lint-checks: NO RESULT -- the target PASSED, and at least one case could not be" >&2
        echo '  evaluated from this checkout. Every checker ran; the unevaluable cases are' >&2
        echo '  listed above, each on a line beginning with the marker below.' >&2
        grep "^${NO_RESULT_MARKER}" "$node_out" | sed 's/^/    /' >&2
        exit 75
        ;;
esac
exit 0
