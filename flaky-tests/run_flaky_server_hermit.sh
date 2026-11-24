#!/bin/bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Run the original flaky server as a multi-process guest under Hermit
# and assert determinism plus the protocol property. Natively the
# fixture is flaky: flaky_server.sh answers each request with a bash
# RANDOM bit and the host-side test script made that bit its exit
# code, across a host port, netstat, hermit-wrapped nc and a
# by-port kill. Here the shell, nc and the unmodified server all run
# inside one guest (flaky_server_guest.sh) on the guest's own
# loopback under --network=local, so no host port, unshare, sudo or
# capability is involved. The wrapper passes only on bitwise parity
# under the canonical strict form (--strict with the canonical test
# epoch plus --verify-strict, whose --verify-json must report parity
# over a nonzero INFO comparison, the L2 shape in AGENTS.md) AND the
# guest's own property line: exactly the requested number of replies,
# each 0 or 1, in request order. The bits are bash RANDOM draws,
# seeded by the guest pid and virtual time, NOT by Hermit's --seed
# (two pinned seeds gave the same sequence in the spike), so no
# golden sequence is asserted and one seed is used; a Hermit PRNG or
# pid/time change would redraw the sequence without breaking parity.
#
# Backend and log level: ptrace (Hermit's default; no backend flag
# is passed), default log level (no --log is passed; --verify
# requires info). CPUID is virtualized: --cpu-profile=auto is passed
# explicitly so an inherited HERMIT_CPU_PROFILE cannot replace it,
# and the host's CPUID never reaches the guest. Branch preemption is
# OFF (--max-timeslice=disabled, as seat 4): precise-timer parity on
# this workload produced skid overshoots with real divergence, and
# disabled needs no user-space PMU at all; the guest's sleeps and
# socket traps still schedule both processes. The relaxations are
# --verify-allow=both and --max-timeslice=disabled: the first
# admits any guest status - a failing exit, even a signal death - to
# comparison instead of refusing verification outright; the wrapper
# still then requires exit 0, so no admitted status can pass alone.
# --sched-seed pins the schedule and --seed pins the guest's
# random-number and fuzz seeds, each against its environment
# variable (HERMIT_SCHED_SEED / HERMIT_PRNG; the scheduler seed
# falls back to --seed only when --sched-seed is unset). Neither
# pins bash's RANDOM directly - it seeds from the guest pid layout
# and virtual time the schedule produces - which is why the
# property, not bits, is asserted.
#
# Usage: run_flaky_server_hermit.sh <hermit-binary> <guest-driver> <flaky_server.sh> <requests> <port>

set -u

# bash prints times with the locale's decimal point; pin the C
# locale so the whole-tree CPU tokens below always parse (and so
# grep and Hermit's host-side output are locale-independent here).
LC_ALL=C
export LC_ALL

if [ "$#" -ne 5 ]; then
    echo "usage: $0 <hermit-binary> <guest-driver> <flaky_server.sh> <requests> <port>" >&2
    exit 2
fi

hermit_bin="$1"
guest_bin="$2"
server_script="$3"
requests="$4"
port="$5"
if [ ! -x "$hermit_bin" ]; then
    echo "FAIL: hermit binary is not executable: $hermit_bin" >&2
    exit 1
fi
if [ ! -x "$guest_bin" ]; then
    echo "FAIL: guest driver is not executable: $guest_bin" >&2
    exit 1
fi
if [ ! -x "$server_script" ]; then
    echo "FAIL: flaky server script is not executable: $server_script" >&2
    exit 1
fi
for number in "$requests" "$port"; do
    case "$number" in
        ''|*[!0-9]*|0)
            echo "FAIL: requests and port must be positive integers, got '$number'" >&2
            exit 2
            ;;
    esac
done

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
# wedged Hermit fails by name instead of consuming the runner's whole
# timeout (300s is headroom for a loaded host, not an expectation),
# and a WHOLE-TREE CPU budget: Hermit, its container child and every
# guest process across both verify runs. After wait, the wrapper
# reads its reaped children's CPU (bash times, redirected to a file
# in this shell; times inside $(...) would report the subshell's
# empty counters), prints the measured total, and fails by name
# above the budget. The basis is five direct runs of this wrapper
# on the unplanted test at the parent head, measuring 19.791, 20.509,
# 20.743, 19.766 and 22.499 CPU-s (whole tree, including the
# safehermit supervisor when present; only these constants differ
# from that head, and they do not affect measured CPU); the budget
# is 1.5 times the worst, rounded up: 34 CPU-s. The guest alone
# used 2.66 CPU-s, and a guest-only limit would kill the Hermit
# tracer process itself. As a runaway backstop each process also
# gets RLIMIT_CPU soft 34 (SIGXCPU, exit 152) and hard 39 (the +5
# gap is a convention so SIGXCPU can fire before SIGKILL), so no
# single process passes the tree budget by itself; a traced runaway
# can instead end as Hermit exit 125 with a container SIGKILL,
# named below too.
inner_wall_timeout_secs=300
cpu_budget_secs=34
cpu_hard_secs=39
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

out_file="$(mktemp)" || { echo "FAIL: mktemp for stdout failed" >&2; exit 1; }
err_file="$(mktemp)" || { echo "FAIL: mktemp for stderr failed" >&2; exit 1; }
json_file="$(mktemp)" || { echo "FAIL: mktemp for verify json failed" >&2; exit 1; }
times_file="$(mktemp)" || { echo "FAIL: mktemp for cpu times failed" >&2; exit 1; }

echo "config: ptrace backend (default), default log level, cpu-profile=auto (explicit), network=local (explicit), seed=0, sched-seed=0, max-timeslice=disabled, sched-heuristic=default (none passed), requests=$requests, port=$port, wall timeout ${inner_wall_timeout_secs}s, cpu budget ${cpu_budget_secs}s whole tree (per-process backstop ${cpu_budget_secs}s soft/${cpu_hard_secs}s hard); relaxations: --verify-allow=both, --max-timeslice=disabled"

( ulimit -S -t "$cpu_budget_secs" || { echo "FAIL-SETUP: ulimit soft cpu limit failed" >&2; exit 126; }; ulimit -H -t "$cpu_hard_secs" || { echo "FAIL-SETUP: ulimit hard cpu limit failed" >&2; exit 126; }; exec timeout --verbose --kill-after=10s "${inner_wall_timeout_secs}s" "$hermit_bin" run \
    --strict \
    --epoch=2026-01-01T00:00:00Z \
    --verify \
    --verify-strict \
    --verify-allow=both \
    --verify-json="$json_file" \
    --seed=0 \
    --sched-seed=0 \
    --cpu-profile=auto \
    --network=local \
    --base-env=minimal \
    --max-timeslice=disabled \
    -- "$guest_bin" "$server_script" "$requests" "$port" ) >"$out_file" 2>"$err_file" &
hermit_child=$!
wait "$hermit_child"
hermit_status=$?
hermit_child=""

echo "--- guest stdout (via hermit) ---"
cat "$out_file"
echo "--- hermit stderr ---"
cat "$err_file"
echo "--- hermit exit status: $hermit_status (0 is the only passing status; 1 may be a guest property failure or a verification divergence, discriminated by the marker, JSON and guest line below; 101 panic, 124/137 wall timeout, 152 CPU budget and 125 Hermit error fail) ---"

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
echo "cpu: whole-tree measured ${cpu_total_s}s (reaped children user+sys) of ${cpu_budget_secs}s budget"

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

# A perf-counter downgrade would disable branch preemption for the
# whole run: unreachable with the requested disabled timeslice, kept
# as a guard so a future preemption change fails it by name as a
# host limitation, never as a guest failure.
if grep -qF "continuing with --max-timeslice=disabled" "$err_file"; then
    echo "FAIL: host limitation: Hermit downgraded --max-timeslice for lack of perf counters, so branch preemption is off" >&2
    exit 1
fi

if [ "$hermit_status" -eq 124 ]; then
    echo "FAIL: hermit run exceeded the wall timeout (${inner_wall_timeout_secs}s)" >&2
    exit 1
fi

if [ "$hermit_status" -eq 137 ]; then
    # 137 is the wall timeout only when timeout itself sent the kill;
    # its --verbose line in the captured stderr is the evidence. Any
    # other 137 was a SIGKILL from elsewhere.
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

# The guest driver exits 0 only on a complete in-order bit sequence
# with verified cleanup; its named failures exit 1, and a
# verification divergence also exits 1, so {0, 1} is admitted to the
# verdict checks and only 0 can pass overall. Everything else (101
# panic, 125 Hermit error, 128+signo signal death, ...) fails here.
if [ "$hermit_status" -eq 122 ] || [ "$hermit_status" -eq 125 ]; then
    echo "FAIL: Hermit refused or failed; reason above (exit $hermit_status)" >&2
    exit 1
fi

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

if [ "$guest_exit" -ne 0 ]; then
    echo "FAIL: guest driver failed deterministically (exit $guest_exit); see guest output above" >&2
    exit 1
fi

# The exit code alone could come from any guest; require this
# driver's own property lines: a sequence of exactly the requested
# length over {0,1} and the in-order completion line.
sequence_line="$(grep -E '^sequence=[01]+$' "$out_file" || true)"
if [ -z "$sequence_line" ]; then
    echo "FAIL: guest stdout lacks the sequence line" >&2
    exit 1
fi
sequence="${sequence_line#sequence=}"
if [ "${#sequence}" -ne "$requests" ]; then
    echo "FAIL: sequence length ${#sequence} != requests $requests" >&2
    exit 1
fi
if ! grep -qxF "guest: $requests replies in order, each 0/1" "$out_file"; then
    echo "FAIL: guest stdout lacks the in-order property line for $requests replies" >&2
    exit 1
fi

echo "PASS: flaky server deterministic under Hermit strict verify (ptrace backend, default log level; relaxations: --verify-allow=both, --max-timeslice=disabled; $requests replies in order (sequence=$sequence), bitwise parity; cpu ${cpu_total_s}s of ${cpu_budget_secs}s whole-tree budget)"
exit 0
