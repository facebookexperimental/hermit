#!/bin/bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Run the hello_race_mini guest under Hermit and assert determinism
# in the canonical strict form: --strict with the canonical test
# epoch plus --verify-strict, whose --verify-json must report
# bitwise parity over a nonzero INFO comparison (the L2 shape in
# AGENTS.md). The guest exits 1 on the antagonistic schedule and 0
# otherwise; either guest exit is admitted to comparison (the one
# relaxation below), Hermit propagates it, and this wrapper passes
# only on Hermit's determinism verdict and the matching guest
# outcome. It never passes on the raw guest exit alone.
#
# Backend and log level: ptrace (Hermit's default; no backend flag
# is passed), default log level (no --log is passed; --verify
# requires info).
#
# CPUID is virtualized: --cpu-profile=auto is passed explicitly so
# an inherited HERMIT_CPU_PROFILE cannot replace it, and the host's
# CPUID never reaches the guest.
#
# Timeslice scope choice (passed and printed, not counted as a
# relaxation): --max-timeslice=disabled, so there is no
# branch-count preemption and chaos perturbs scheduling only at
# trapped events.
#
# The one relaxation: --verify-allow=both, so a failing guest exit
# is admitted to comparison instead of refusing verification
# outright; the wrapper still then cross-checks the exit and the
# guest's own Final value line, so no admitted status passes alone.
# --sched-seed pins the schedule and --seed pins the guest's
# random-number and fuzz seeds, each against its environment variable
# (HERMIT_SCHED_SEED / HERMIT_PRNG; the scheduler seed falls back to
# --seed only when --sched-seed is unset).
#
# Usage: run_hello_race_mini_hermit.sh <hermit-binary> <hello_race_mini-binary> <defaults.yaml>

set -u

# bash prints times with the locale's decimal point; pin the C
# locale so the whole-tree CPU tokens below always parse (and so
# grep and Hermit's host-side output are locale-independent here).
LC_ALL=C
export LC_ALL

if [ "$#" -ne 3 ]; then
    echo "usage: $0 <hermit-binary> <hello_race_mini-binary> <defaults.yaml>" >&2
    exit 2
fi

hermit_bin="$1"
guest_bin="$2"
defaults_file="$3"

# Install cleanup before any mktemp: the EXIT trap removes whichever
# temp files exist by then, and each catchable signal is converted to
# exit 128+N so EXIT fires once with the right status.
out_file=""
err_file=""
json_file=""
times_file=""
rc=""
hermit_child=""
# Kill the running Hermit first, then exit 128+N so EXIT fires once.
on_signal() {
    if [ -n "$hermit_child" ]; then
        kill "$hermit_child" 2>/dev/null
    fi
    exit "$1"
}
trap 'rc=$?; set +e; rm -f "$out_file" "$err_file" "$json_file" "$times_file"; exit $rc' EXIT
trap 'on_signal 129' HUP
trap 'on_signal 130' INT
trap 'on_signal 131' QUIT
trap 'on_signal 141' PIPE
trap 'on_signal 143' TERM

# Two named budgets for the single verification: a WALL backstop so a
# wedged Hermit fails this test by name instead of consuming the
# runner's whole timeout (300s is headroom for a loaded host, not an
# expectation; the guest runs in seconds), and a WHOLE-TREE CPU
# budget: Hermit, its container child and every guest process
# across both verify runs. After wait, the wrapper reads its
# reaped children's CPU (bash times, redirected to a file in this
# shell; times inside $(...) would report the subshell's empty
# counters), prints the measured total, and fails by name above
# the budget. CPU seconds are the outer runaway backstop at
# the project default, the only blocking CPU cap: no per-test
# exception and no measured basis is owed for a default. The
# default is inherited by reference, never restated here: the
# third argument is tests/e2e/manifests/defaults.yaml (passed
# by BUCK), whose cpu_timeout_seconds mirrors
# DEFAULT_TEST_CPU_TIMEOUT_SECONDS in
# ci/manifest-plan/src/timeouts.rs. The wrapper reads that one
# key exactly, applies HERMIT_TEST_CPU_TIMEOUT_MULTIPLIER the
# way timeouts.rs does (ceil of base x multiplier, at least
# 1), and fails closed by name on a missing file, a missing,
# duplicate or non-integer key, an out-of-range value, or a
# bad multiplier. Twelve direct runs measured 6.120-7.393
# CPU-s whole tree over identical work (identical verify-JSON
# counters); the spread tracked host load, so a tight
# CPU-seconds cap detects a busier host, not a regression.
# As the per-process form of the backstop each process also
# gets RLIMIT_CPU soft at the budget (SIGXCPU, exit 152) and
# hard at budget + 5 (the +5 gap is a convention so SIGXCPU
# can fire before SIGKILL), so no single process passes the
# tree budget by itself; a traced runaway can instead end as
# Hermit exit 125 with a container SIGKILL, named below too.
inner_wall_timeout_secs=300
cpu_budget_secs=""
cpu_hard_secs=""
# Convert one bash times token ("0m8.697s") to milliseconds.
times_token_ms() {
    local tok="$1" mins secs frac
    case "$tok" in
        *m*s) ;;
        *) return 1 ;;
    esac
    mins="${tok%%m*}"
    secs="${tok#*m}"
    secs="${secs%s}"
    case "$mins" in
        ''|*[!0-9]*) return 1 ;;
    esac
    case "$secs" in
        *.*)
            frac="${secs#*.}"
            secs="${secs%%.*}"
            ;;
        *)
            frac="0"
            ;;
    esac
    case "$secs" in
        ''|*[!0-9]*) return 1 ;;
    esac
    case "$frac" in
        ''|*[!0-9]*) return 1 ;;
    esac
    frac="${frac}000"
    printf '%s' $(( mins * 60000 + secs * 1000 + 10#${frac:0:3} ))
}
if ! command -v timeout >/dev/null 2>&1; then
    echo "FAIL: timeout(1) not found; refusing to run Hermit unbounded" >&2
    exit 1
fi
if ! command -v python3 >/dev/null 2>&1; then
    echo "FAIL: python3 not found; cannot parse the verify JSON" >&2
    exit 1
fi
cpu_budget_secs="$(python3 -I - "$defaults_file" <<'PY'
import math
import os
import sys

try:
    with open(sys.argv[1], encoding="utf-8") as handle:
        lines = handle.read().splitlines()
except OSError as error:
    print(f"FAIL: defaults file unreadable ({sys.argv[1]}): {error}", file=sys.stderr)
    raise SystemExit(1)
values = []
for line in lines:
    if line.startswith("cpu_timeout_seconds:"):
        values.append(line.split(":", 1)[1].strip())
if len(values) != 1:
    print(f"FAIL: defaults file must carry exactly one cpu_timeout_seconds key, found {len(values)}", file=sys.stderr)
    raise SystemExit(1)
raw = values[0]
if not raw.isdigit():
    print(f"FAIL: cpu_timeout_seconds is not a whole number of seconds: {raw!r}", file=sys.stderr)
    raise SystemExit(1)
base = int(raw)
if base < 1 or base > 1800:
    print(f"FAIL: cpu_timeout_seconds out of range 1..1800: {base}", file=sys.stderr)
    raise SystemExit(1)
mult_raw = os.environ.get("HERMIT_TEST_CPU_TIMEOUT_MULTIPLIER")
if mult_raw is None:
    mult = 1.0
else:
    # Rust's f64 parser (timeouts.rs) refuses surrounding whitespace,
    # digit-group underscores and non-ASCII digits that Python's
    # float() accepts; refuse exactly those here so both parsers agree.
    if mult_raw != mult_raw.strip() or "_" in mult_raw or not mult_raw.isascii():
        print(f"FAIL: HERMIT_TEST_CPU_TIMEOUT_MULTIPLIER must be a positive finite number, got {mult_raw!r}", file=sys.stderr)
        raise SystemExit(1)
    try:
        mult = float(mult_raw)
    except ValueError:
        print(f"FAIL: HERMIT_TEST_CPU_TIMEOUT_MULTIPLIER must be a positive finite number, got {mult_raw!r}", file=sys.stderr)
        raise SystemExit(1)
if not math.isfinite(mult) or mult <= 0:
    print(f"FAIL: HERMIT_TEST_CPU_TIMEOUT_MULTIPLIER must be finite and greater than zero, got {mult!r}", file=sys.stderr)
    raise SystemExit(1)
scaled = math.ceil(base * mult)
print(max(1, scaled))
PY
)" || { echo "FAIL: cannot resolve the default CPU backstop (see above)" >&2; exit 1; }
case "$cpu_budget_secs" in
    ''|*[!0-9]*)
        echo "FAIL: resolved CPU backstop is not a whole number of seconds: '$cpu_budget_secs'" >&2
        exit 1
        ;;
esac
cpu_hard_secs=$((cpu_budget_secs + 5))

out_file="$(mktemp)" || { echo "FAIL: mktemp for stdout failed" >&2; exit 1; }
err_file="$(mktemp)" || { echo "FAIL: mktemp for stderr failed" >&2; exit 1; }
json_file="$(mktemp)" || { echo "FAIL: mktemp for verify json failed" >&2; exit 1; }
times_file="$(mktemp)" || { echo "FAIL: mktemp for cpu times failed" >&2; exit 1; }

echo "config: ptrace backend (default), default log level, cpu-profile=auto (explicit), network=local (explicit), seed=0, sched-seed=0, max-timeslice=disabled, sched-heuristic=default (none passed), wall timeout ${inner_wall_timeout_secs}s, cpu outer backstop ${cpu_budget_secs}s whole tree (per-process ${cpu_budget_secs}s soft/${cpu_hard_secs}s hard), counter caps 99/791/5616937500; relaxations: --verify-allow=both"

( ulimit -S -t "$cpu_budget_secs" || { echo "FAIL-SETUP: ulimit soft cpu limit failed" >&2; exit 126; }; ulimit -H -t "$cpu_hard_secs" || { echo "FAIL-SETUP: ulimit hard cpu limit failed" >&2; exit 126; }; exec timeout --verbose --kill-after=10s "${inner_wall_timeout_secs}s" "$hermit_bin" run \
    --strict \
    --epoch=2026-01-01T00:00:00Z \
    --verify \
    --verify-strict \
    --verify-allow=both \
    --verify-json="$json_file" \
    --chaos \
    --seed=0 \
    --sched-seed=0 \
    --cpu-profile=auto \
    --network=local \
    --base-env=minimal \
    --max-timeslice=disabled \
    --env=HERMIT_MODE=chaos \
    -- "$guest_bin" ) >"$out_file" 2>"$err_file" &
hermit_child=$!
wait "$hermit_child"
hermit_status=$?
hermit_child=""

echo "--- guest stdout (via hermit) ---"
cat "$out_file"
echo "--- hermit stderr ---"
cat "$err_file"
echo "--- hermit exit status: $hermit_status (0/1 admitted to the verdict checks below; 1 may be the guest's antagonistic exit or a verification divergence, discriminated by the marker, JSON and Final value below; 101 panic, 124/137 wall timeout, 152 CPU budget and 125 Hermit error fail) ---"

# Whole-tree CPU: this shell's reaped children include the waited
# subshell and, transitively, everything it waited for (timeout,
# Hermit, its container child and the guest processes).
times >"$times_file"
child_user_tok=""
child_sys_tok=""
_own_times_line=""
{
    read -r _own_times_line \
        && read -r child_user_tok child_sys_tok
} <"$times_file" || {
    echo "FAIL: cannot read bash times output; whole-tree CPU unmeasured" >&2
    exit 1
}
cpu_user_ms="$(times_token_ms "$child_user_tok")" || {
    echo "FAIL: cannot parse bash times user token '$child_user_tok'; whole-tree CPU unmeasured" >&2
    exit 1
}
cpu_sys_ms="$(times_token_ms "$child_sys_tok")" || {
    echo "FAIL: cannot parse bash times sys token '$child_sys_tok'; whole-tree CPU unmeasured" >&2
    exit 1
}
cpu_total_ms=$(( cpu_user_ms + cpu_sys_ms ))
printf -v cpu_total_s '%d.%03d' $(( cpu_total_ms / 1000 )) $(( cpu_total_ms % 1000 ))
echo "cpu: whole-tree measured ${cpu_total_s}s (reaped children user+sys) of ${cpu_budget_secs}s outer backstop"

# The CPU backstop itself could not be set (for example an
# inherited hard limit below the budget): Hermit never ran, and
# running it unbounded is refused by name.
if grep -qF "FAIL-SETUP: ulimit" "$err_file"; then
    echo "FAIL: cannot set the CPU limits (soft ${cpu_budget_secs}s, hard ${cpu_hard_secs}s; inherited hard limit too low?); refusing to run Hermit unbounded" >&2
    exit 1
fi

# A traced runaway reaches the per-process hard limit as Hermit exit
# 125 with a container SIGKILL, not as 152; name it before the
# generic 125 branch below.
if [ "$hermit_status" -eq 125 ] \
        && grep -qF "container-child-exit status=Signaled(SIGKILL" "$err_file"; then
    echo "FAIL: a Hermit child was SIGKILLed (CPU hard limit, OOM or external); hermit exit 125, reason above" >&2
    exit 1
fi

if [ "$cpu_total_ms" -gt $(( cpu_budget_secs * 1000 )) ]; then
    echo "FAIL: hermit run exceeded the whole-tree CPU budget (${cpu_budget_secs} CPU-s): measured ${cpu_total_s}s" >&2
    exit 1
fi

if [ "$hermit_status" -eq 124 ]; then
    echo "FAIL: hermit run exceeded the wall timeout (${inner_wall_timeout_secs}s)" >&2
    exit 1
fi

if [ "$hermit_status" -eq 137 ]; then
    if grep -qF "sending signal" "$err_file"; then
        echo "FAIL: hermit run exceeded the wall timeout (${inner_wall_timeout_secs}s, kill phase)" >&2
    else
        echo "FAIL: hermit run killed by SIGKILL (CPU hard limit, OOM killer or external)" >&2
    fi
    exit 1
fi

if [ "$hermit_status" -eq 152 ]; then
    echo "FAIL: hermit run exceeded the per-process CPU limit (${cpu_budget_secs} CPU-s soft, SIGXCPU)" >&2
    exit 1
fi

if [ "$hermit_status" -eq 122 ] || [ "$hermit_status" -eq 125 ]; then
    echo "FAIL: Hermit refused or failed; reason above (exit $hermit_status)" >&2
    exit 1
fi

# hello_race_mini itself only ever exits 0 or 1, and a verification divergence
# also exits 1, so {0, 1} is admitted to the verdict checks; everything
# else (101 panic, 125 Hermit error, 128+signo signal death, ...) fails.
if [ "$hermit_status" -ne 0 ] && [ "$hermit_status" -ne 1 ]; then
    echo "FAIL: hermit exited $hermit_status, not an admitted status (0/1)" >&2
    exit 1
fi

if grep -qF "Failure: nondeterministic." "$err_file"; then
    echo "FAIL: hermit reported nondeterminism" >&2
    exit 1
fi

if ! grep -qF "Success: deterministic. Determinism verified." "$err_file"; then
    echo "FAIL: determinism marker missing from hermit stderr" >&2
    exit 1
fi

echo "--- verify json ---"
cat "$json_file"
echo

# Load-invariant counter gate (in the parse below): every
# recorded run of this guest reports scheduler turns 66,
# syscalls 527 and virtual nanoseconds 3,744,625,000 in
# runtime.run1 (run1 == run2 by parity), including loaded runs
# whose CPU seconds rose with host load. The caps are
# ceil(1.5x) of those values. --cpu-profile=auto can present
# v2 or v3 by host; the 1.5x absorbs that difference. A missing
# or unreadable counter fails by name, never passes.

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
if type(left) is not int or type(right) is not int or left <= 0 or left != right:
    print(f"FAIL: verify json INFO comparison vacuous or uneven: left={left!r} right={right!r}", file=sys.stderr)
    raise SystemExit(1)
runtime = report.get("runtime") or {}
run1 = runtime.get("run1") or {}
caps = {"scheduler_turns": 99, "syscalls": 791, "virtual_nanoseconds": 5616937500}
seen = {}
for name, cap in caps.items():
    value = run1.get(name)
    if type(value) is not int or value < 0:
        print(f"FAIL: verify json counter {name} missing or unreadable: {value!r}", file=sys.stderr)
        raise SystemExit(1)
    seen[name] = value
    if value > cap:
        print(f"FAIL: verify json counter {name}={value} exceeds cap {cap} (load-invariant runaway gate)", file=sys.stderr)
        raise SystemExit(1)
print(f"counters: scheduler_turns={seen['scheduler_turns']} syscalls={seen['syscalls']} virtual_nanoseconds={seen['virtual_nanoseconds']} (caps 99/791/5616937500)", file=sys.stderr)
guest_exit, guest_signal = report.get("guest_exit_code"), report.get("guest_signal")
if guest_exit not in (0, 1) or guest_signal is not None:
    print(f"FAIL: verify json guest outcome is not a clean exit: exit={guest_exit!r} signal={guest_signal!r}", file=sys.stderr)
    raise SystemExit(1)
print(guest_exit)
PY
)" || { echo "FAIL: verify json rejected (see above)" >&2; exit 1; }

if [ "$guest_exit" != "$hermit_status" ]; then
    echo "FAIL: verify json guest exit $guest_exit != hermit status $hermit_status" >&2
    exit 1
fi

# The exit code alone could come from any guest (even /bin/true):
# the guest's own Final value must name the same schedule the exit
# code claims (exit 0 stores 1 last, the antagonistic exit 1 stores
# 2 last - hello_race_mini.rs). A broken copy flips the expected
# value for the outcome the run observed, and must fail by name;
# each run exercises only its observed outcome's branch below.
if [ "$guest_exit" -eq 0 ]; then
    expected_final="Final value: 1"
else
    expected_final="Final value: 2"
fi
if ! grep -qxF "$expected_final" "$out_file"; then
    echo "FAIL: guest stdout lacks '$expected_final' for guest exit $guest_exit" >&2
    exit 1
fi

echo "PASS: hello_race_mini deterministic under Hermit strict chaos-verify (ptrace backend, default log level; relaxations: --verify-allow=both; guest exit $guest_exit with $expected_final, bitwise parity, counters within caps; cpu ${cpu_total_s}s of ${cpu_budget_secs}s outer backstop)"
exit 0
