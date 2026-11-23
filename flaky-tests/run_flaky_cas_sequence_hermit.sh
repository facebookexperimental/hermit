#!/bin/bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Run the cas_sequence_easy guest under Hermit and assert determinism
# with schedule coverage. The guest completes a three-step CAS handoff
# (1 -> 2 -> 3 -> 4) only under one thread interleaving and exits 1 when
# it completes ("Antagonistic schedule reached"), 0 otherwise. A single
# deterministic schedule therefore proves little, and which seeds
# complete the handoff depends on the guest's retired-branch stream --
# the Hermit build, the guest build and the presented CPU can all move
# it -- so no small pinned seed set can hold a coverage contract. This
# wrapper instead searches seeds in order (0, 1, 2, ...): every tried
# seed must report bitwise parity under the canonical strict form
# (--strict with the canonical test epoch plus --verify-strict, whose
# --verify-json must report parity over a nonzero INFO comparison, the
# L2 shape in AGENTS.md), the search stops as soon as both guest
# outcomes have appeared, and it fails as vacuous if the bounded range
# runs out first. It never passes on any raw guest exit.
#
# Backend and log level: ptrace (Hermit's default; no backend flag is
# passed), default log level (no --log is passed; --verify requires
# info). CPUID is virtualized (no --no-virtualize-cpuid): the
# hermit_host_test probe admits only x86-64-v3+cpuid-faulting hosts.
# The profile is Hermit's auto, passed explicitly so an inherited
# HERMIT_CPU_PROFILE cannot replace it. Auto can still present v3 on
# one host and fall back to v2 on another, so which seeds give which
# outcome can differ between hosts; the ordered search is what makes
# that harmless, since any seed's outcome counts as long as both
# appear within the range at full parity. The one relaxation is
# --verify-allow=both: a failing
# guest exit is admitted to comparison instead of refusing
# verification outright.
# Branch-count preemption stays ON (--max-timeslice=1000000, precise
# timers): the guest's delay loops trap nothing, so without preemption
# chaos cannot move the handoff window. Imprecise timers are never
# used: they can find the schedule but forfeit the bitwise parity this
# wrapper asserts. If Hermit downgrades the timeslice for lack of perf
# counters, preemption is gone and every seed trivially exits 0; that
# host limitation is failed by name below, never as vacuous coverage.
# Per seed, --sched-seed pins the schedule and --seed pins the guest's
# random-number and fuzz seeds, each against its environment variable
# (HERMIT_SCHED_SEED / HERMIT_PRNG; the scheduler seed falls back to
# --seed only when --sched-seed is unset).
#
# Usage: run_flaky_cas_sequence_hermit.sh <hermit-binary> <cas-binary>

set -u

if [ "$#" -ne 2 ]; then
    echo "usage: $0 <hermit-binary> <cas-binary>" >&2
    exit 2
fi

hermit_bin="$1"
guest_bin="$2"

# Bounded ordered search: seeds 0..31, stop at both outcomes. Hermit
# is deterministic, so the first exit-0 seed is fixed per build and
# presented CPU profile (seed 4 for the spike's rustc guest, seed 15
# for the wired Buck guest); an unrelated change can redraw it later
# in the range, which makes the search longer within the same bounds
# below -- or, past seed 31, a named vacuous failure instead of a
# silent pass. At the spike's outcome rate the expected stop is
# around 6 seeds. The worst legitimate search is the full range at
# the measured per-seed wall (at most 32.4s wired, CPU at most
# 15.6s): about 1,036s of wall. The BUCK target carries
# tpx:timeout-multiplier:3 (about 1,800s against the runner's roughly
# 600s default) for that range. The total budget below reserves the
# per-seed backstop in its check, so the first 31 seeds may average
# up to 44.5s (headroom 1.374x over the worst legitimate range) and
# it fires about 290s before the runner timeout, by name, so a slow
# legitimate search is a named wrapper failure, never a runner kill.
# The per-seed backstop further down bounds only a wedged single
# seed; 32 of those (about 69 minutes) is not a legitimate search.
seed_max=31
total_budget_secs=1500

# Monotonic whole seconds for the total budget: /proc/uptime cannot
# step with the system clock. Sets now_secs and returns nonzero if the
# read fails, which the callers fail by name on.
now_secs=""
read_now_secs() {
    local uptime rest
    read -r uptime rest < /proc/uptime || return 1
    now_secs="${uptime%%.*}"
    [ -n "$now_secs" ]
}
read_now_secs || {
    echo "FAIL: reading /proc/uptime failed; cannot enforce the total budget" >&2
    exit 1
}
search_start_secs="$now_secs"

# Inner wall backstop per seed so a wedged Hermit fails by name instead
# of consuming the runner's whole timeout. Each verification runs in
# seconds; 120s is headroom for a loaded host, not an expectation.
inner_timeout_secs=120
if ! command -v timeout >/dev/null 2>&1; then
    echo "FAIL: timeout(1) not found; refusing to run Hermit unbounded" >&2
    exit 1
fi
if ! command -v python3 >/dev/null 2>&1; then
    echo "FAIL: python3 not found; cannot parse the verify JSON" >&2
    exit 1
fi

# Cleanup is installed before any mktemp (make_temp runs in the seed
# loop below); each catchable signal becomes exit 128+N so EXIT fires
# once with the right status.
tmp_files=()
rc=""
cleanup() {
    # Runs from the EXIT trap below.
    if [ "${#tmp_files[@]}" -gt 0 ]; then
        rm -f "${tmp_files[@]}"
    fi
}
trap 'rc=$?; set +e; cleanup; exit $rc' EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 131' QUIT
trap 'exit 141' PIPE
trap 'exit 143' TERM

# Create a temp file in THIS shell (never inside $(...), where the
# tmp_files update and any exit would stay in the subshell and cleanup
# would remove nothing) and store its path in the named variable.
make_temp() {
    local var_name="$1"
    local made
    made="$(mktemp)" || { echo "FAIL: mktemp failed" >&2; exit 1; }
    tmp_files+=("$made")
    printf -v "$var_name" '%s' "$made"
}

echo "config: ptrace backend (default), default log level, cpu-profile=auto (explicit), ordered seed search 0..$seed_max (seed=sched-seed per run, stop at both outcomes), inner timeout ${inner_timeout_secs}s/seed (wall), total budget ${total_budget_secs}s (wall); relaxations: --verify-allow=both"

saw_exit0=0
saw_exit1=0
outcomes=""
seed=-1
# Assigned per seed by make_temp (printf -v); pre-initialized here so
# every reference below is to a set variable under set -u.
out_file=""
err_file=""
json_file=""

while [ "$seed" -lt "$seed_max" ]; do
    seed=$((seed + 1))
    read_now_secs || {
        echo "FAIL: reading /proc/uptime failed; cannot enforce the total budget" >&2
        exit 1
    }
    elapsed_secs=$((now_secs - search_start_secs))
    if [ $((elapsed_secs + inner_timeout_secs)) -gt "$total_budget_secs" ]; then
        echo "FAIL: total budget ${total_budget_secs}s (wall) would be exceeded by seed $seed after ${elapsed_secs}s (tried:$outcomes); failing by name before the runner timeout" >&2
        exit 1
    fi
    make_temp out_file
    make_temp err_file
    make_temp json_file

    timeout --kill-after=10s "${inner_timeout_secs}s" "$hermit_bin" run \
        --strict \
        --epoch=2026-01-01T00:00:00Z \
        --verify \
        --verify-strict \
        --verify-allow=both \
        --verify-json="$json_file" \
        --chaos \
        --sched-heuristic=random \
        --max-timeslice=1000000 \
        --seed="$seed" \
        --sched-seed="$seed" \
        --cpu-profile=auto \
        --base-env=minimal \
        --env=HERMIT_MODE=chaos \
        -- "$guest_bin" >"$out_file" 2>"$err_file"
    hermit_status=$?

    echo "--- seed $seed: guest stdout (via hermit) ---"
    cat "$out_file"
    echo "--- seed $seed: hermit stderr ---"
    cat "$err_file"
    echo "--- seed $seed: hermit exit status: $hermit_status (0/1 admitted to the verdict checks below; 1 may be the guest's antagonistic exit or a verification divergence, discriminated by the marker and JSON; 101 panic, 124 inner timeout and 125 Hermit error fail) ---"

    # A perf-counter downgrade disables branch preemption for the whole
    # run: no seed can then complete the handoff, and reporting that as
    # vacuous coverage would blame the guest for a host limitation.
    if grep -qF "continuing with --max-timeslice=disabled" "$err_file"; then
        echo "FAIL: seed $seed: host limitation: Hermit downgraded --max-timeslice for lack of perf counters, so branch preemption is off and no schedule can complete the CAS handoff" >&2
        exit 1
    fi

    if [ "$hermit_status" -eq 124 ]; then
        echo "FAIL: seed $seed exceeded the inner timeout (${inner_timeout_secs}s)" >&2
        exit 1
    fi

    # The guest itself only ever exits 0 or 1, and a verification
    # divergence also exits 1, so {0, 1} is admitted to the verdict
    # checks; everything else (101 panic, 125 Hermit error, 128+signo
    # signal death, ...) fails.
    if [ "$hermit_status" -eq 122 ] || [ "$hermit_status" -eq 125 ]; then
        echo "FAIL: seed $seed: Hermit refused or failed; reason above (exit $hermit_status)" >&2
        exit 1
    fi

    if [ "$hermit_status" -ne 0 ] && [ "$hermit_status" -ne 1 ]; then
        echo "FAIL: seed $seed: hermit exited $hermit_status, not an admitted status (0/1)" >&2
        exit 1
    fi

    if grep -qF "Failure: nondeterministic." "$err_file"; then
        echo "FAIL: seed $seed: hermit reported nondeterminism" >&2
        exit 1
    fi

    if ! grep -qF "Success: deterministic. Determinism verified." "$err_file"; then
        echo "FAIL: seed $seed: determinism marker missing from hermit stderr" >&2
        exit 1
    fi

    echo "--- seed $seed: verify json ---"
    cat "$json_file"
    echo

    guest_exit="$(python3 -I - "$json_file" <<'PY'
import json
import sys

try:
    with open(sys.argv[1], encoding="utf-8") as handle:
        report = json.load(handle)
except (OSError, ValueError) as error:
    print(f"FAIL: verify json unreadable: {error}", file=sys.stderr)
    raise SystemExit(1)
if report.get("bitwise_parity") is not True:
    print("FAIL: verify json does not report bitwise parity", file=sys.stderr)
    raise SystemExit(1)
compared = report.get("compared_log_messages") or {}
left, right = compared.get("left"), compared.get("right")
if not isinstance(left, int) or not isinstance(right, int) or left <= 0 or left != right:
    print(f"FAIL: verify json INFO comparison vacuous or uneven: left={left!r} right={right!r}", file=sys.stderr)
    raise SystemExit(1)
guest_exit = report.get("guest_exit_code")
if guest_exit not in (0, 1):
    print(f"FAIL: verify json guest exit is not 0/1: {guest_exit!r}", file=sys.stderr)
    raise SystemExit(1)
print(guest_exit)
PY
    )" || { echo "FAIL: seed $seed: verify json rejected (see above)" >&2; exit 1; }

    if [ "$guest_exit" != "$hermit_status" ]; then
        echo "FAIL: seed $seed: verify json guest exit $guest_exit != hermit status $hermit_status" >&2
        exit 1
    fi

    # The exit code alone could come from any guest; require this
    # guest's own line for the claimed outcome.
    if [ "$guest_exit" -eq 1 ]; then
        if ! grep -qF "Antagonistic schedule reached" "$out_file"; then
            echo "FAIL: seed $seed: guest exit 1 without its antagonistic-schedule line" >&2
            exit 1
        fi
        saw_exit1=1
    else
        if ! grep -qF "Did not find antagonistic schedule" "$out_file"; then
            echo "FAIL: seed $seed: guest exit 0 without its did-not-find line" >&2
            exit 1
        fi
        saw_exit0=1
    fi

    outcomes="$outcomes seed$seed=exit$guest_exit"
    echo "seed $seed: deterministic, guest exit $guest_exit"

    if [ "$saw_exit0" -eq 1 ] && [ "$saw_exit1" -eq 1 ]; then
        break
    fi
done

if [ "$saw_exit0" -ne 1 ] || [ "$saw_exit1" -ne 1 ]; then
    echo "FAIL: vacuous coverage over seeds 0..$seed_max (outcomes:$outcomes); need at least one guest exit 0 and one guest exit 1 (antagonistic CAS completion)" >&2
    exit 1
fi

echo "PASS: cas_sequence_easy deterministic under Hermit strict chaos-verify with coverage (ptrace backend, default log level; tried seeds 0..$seed with outcomes:$outcomes; bitwise parity every tried seed)"
exit 0
