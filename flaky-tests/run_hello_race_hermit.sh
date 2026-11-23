#!/bin/bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Run the hello_race guest under Hermit and assert determinism, mirroring
# hermit_modes::hello_race_chaos_verify (hermit-cli/tests/hermit_modes.rs)
# and strengthening its lossy Stripped comparison to the canonical strict
# form: --strict with the canonical test epoch plus --verify-strict, whose
# --verify-json must report bitwise parity over a nonzero INFO comparison
# (the L2 shape in AGENTS.md). The guest exits 1 on the
# antagonistic schedule and 0 otherwise; either guest exit is admitted to
# comparison (see relaxations), Hermit propagates it, and this wrapper
# passes only on Hermit's determinism verdict. It never passes on the raw
# guest exit.
#
# Backend and log level: ptrace (Hermit's default; no backend flag is
# passed), default log level (no --log is passed; --verify requires info).
# Relaxations, all inherited from the mirrored test unless noted:
#   --no-virtualize-cpuid (with --cpu-profile=x86-64-v2, the one profile
#     admitted without virtualization): host CPUID reaches the guest;
#   --max-timeslice=disabled: no branch-count preemption, so chaos
#     perturbs scheduling only at trapped events;
#   --verify-allow=both: a failing guest exit is admitted to comparison
#     instead of refusing verification outright.
# --sched-seed pins the schedule and --seed pins the guest's
# random-number and fuzz seeds, each against its environment variable
# (HERMIT_SCHED_SEED / HERMIT_PRNG; the scheduler seed falls back to
# --seed only when --sched-seed is unset).
#
# Usage: run_hello_race_hermit.sh <hermit-binary> <hello_race-binary>

set -u

if [ "$#" -ne 2 ]; then
    echo "usage: $0 <hermit-binary> <hello_race-binary>" >&2
    exit 2
fi

hermit_bin="$1"
guest_bin="$2"

# Install cleanup before any mktemp: the EXIT trap removes whichever
# temp files exist by then, and each catchable signal is converted to
# exit 128+N so EXIT fires once with the right status.
out_file=""
err_file=""
json_file=""
rc=""
trap 'rc=$?; set +e; rm -f "$out_file" "$err_file" "$json_file"; exit $rc' EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 131' QUIT
trap 'exit 141' PIPE
trap 'exit 143' TERM

# Inner wall backstop so a wedged Hermit fails this test by name instead
# of consuming the runner's whole timeout. The guest runs in seconds;
# 300s is headroom for a loaded host, not an expected duration.
inner_timeout_secs=300
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

echo "config: ptrace backend (default), default log level, seed=0, sched-seed=0, inner timeout ${inner_timeout_secs}s; relaxations: --no-virtualize-cpuid, --max-timeslice=disabled, --verify-allow=both"

timeout --kill-after=10s "${inner_timeout_secs}s" "$hermit_bin" run \
    --strict \
    --epoch=2026-01-01T00:00:00Z \
    --verify \
    --verify-strict \
    --verify-allow=both \
    --verify-json="$json_file" \
    --chaos \
    --seed=0 \
    --sched-seed=0 \
    --base-env=minimal \
    --no-virtualize-cpuid \
    --cpu-profile=x86-64-v2 \
    --max-timeslice=disabled \
    --env=HERMIT_MODE=chaos \
    -- "$guest_bin" >"$out_file" 2>"$err_file"
hermit_status=$?

echo "--- guest stdout (via hermit) ---"
cat "$out_file"
echo "--- hermit stderr ---"
cat "$err_file"
echo "--- hermit exit status: $hermit_status (0/1 admitted to the verdict checks below; 1 may be the guest's antagonistic exit or a verification divergence, discriminated by the marker and JSON; 101 panic, 124 inner timeout and 125 Hermit error fail) ---"

if [ "$hermit_status" -eq 124 ]; then
    echo "FAIL: hermit run exceeded the inner timeout (${inner_timeout_secs}s)" >&2
    exit 1
fi

# hello_race itself only ever exits 0 or 1, and a verification divergence
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

if ! python3 -I - "$json_file" <<'PY'
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
PY
then
    exit 1
fi

echo "PASS: hello_race deterministic under Hermit strict chaos-verify (ptrace backend, default log level; guest exit $hermit_status admitted, bitwise parity)"
exit 0
