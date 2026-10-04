#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Nextest flake census: run a nextest selection N times with retries off and
# give every test a verdict.
#
# This is the Rust-test half of the flake census. The end-to-end manifest half
# is `ci/compat-envelope/pressure-test.rs run --green --repetitions N
# --no-retry`; see ci/compat-envelope/README.md "Flake census". Both use the
# same verdicts:
#
#   CLEAN       every run passed
#   FLAKY       some runs passed and some failed
#   FAILING     no run passed, and at least one failed
#   INCOMPLETE  no product failure, but some run produced no product verdict
#
# There is no loop here and no message matching. Nextest repeats the selection
# itself (`--stress-count`), and each failure is classified from the typed
# `type` that nextest writes on its JUnit <failure>, mirroring the manifest
# harness's failure classes:
#
#   "test failure ..." (nonzero exit, leak)   -> product failure
#   "test abort" with a signal other than 9   -> product failure (crash)
#   "test abort" with SIGKILL                 -> no result (OOM or external kill)
#   "test timeout"                            -> no result (wall limit; a hang
#                                                looks like this, so read it)
#   anything else                             -> no result, and named
#
# Usage:
#   scripts/stress-test.sh [-n RUNS] [-o OUTDIR] -- NEXTEST_SELECTION...
#
#   -n RUNS    nextest --stress-count (default: 20)
#   -o OUTDIR  output directory (default: ignored/stress-test/<UTC time>)
#   NEXTEST_SELECTION  the selection exactly as a validate DAG test.* node passes
#              it to ci/run-nextest-counted.sh, e.g. for test.detcore_misc:
#              -p hermit-detcore --test tests_misc -j 1 -- --skip has_rdrand_without_detcore
#
# Writes OUTDIR/stress-test-results.json, OUTDIR/STRESS_TEST_RESULTS.md and
# OUTDIR/junit.xml. Exit status: 0 when every test is CLEAN, 1 otherwise, 2 on
# usage or build errors.
#
# The canonical .config/nextest.toml timeouts apply. The validate node's
# per-machine wall multiplier and per-test CPU budgets come from
# ci/run-nextest-counted.sh, which does not support --stress-count yet: its
# per-attempt CPU records are keyed without the stress iteration.

set -uo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR" || exit 2

RUNS=20
OUTDIR=""

while getopts "n:o:h" opt; do
  case "$opt" in
    n) RUNS="$OPTARG" ;;
    o) OUTDIR="$OPTARG" ;;
    h) sed -n '8,48p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "run with -h for usage" >&2; exit 2 ;;
  esac
done
shift $((OPTIND - 1))
[ "${1:-}" = "--" ] && shift
if [ "$#" -eq 0 ]; then
  echo "error: give the nextest selection after --; run with -h for usage" >&2
  exit 2
fi
case "$RUNS" in
  ''|*[!0-9]*|0) echo "error: -n needs a positive integer" >&2; exit 2 ;;
esac
OUTDIR="${OUTDIR:-ignored/stress-test/$(date -u +%Y%m%dT%H%M%SZ)}"
mkdir -p "$OUTDIR" || exit 2

JUNIT="target/nextest/ci/junit.xml"   # written by the [profile.ci.junit] config
rm -f "$JUNIT"
echo ":: nextest flake census: $RUNS run(s), retries off: $*"
start=$(date +%s)
cargo nextest run --profile ci --no-fail-fast --retries 0 \
  --stress-count "$RUNS" "$@" >"$OUTDIR/nextest.log" 2>&1
rc=$?
end=$(date +%s)
echo ":: nextest exited $rc after $((end - start))s; log: $OUTDIR/nextest.log"
if [ ! -f "$JUNIT" ]; then
  echo "error: nextest wrote no JUnit report; see $OUTDIR/nextest.log" >&2
  tail -20 "$OUTDIR/nextest.log" >&2
  exit 2
fi
cp "$JUNIT" "$OUTDIR/junit.xml"

python3 - "$OUTDIR" "$RUNS" "$((end - start))" "$*" <<'PY'
import json, os, re, sys
from xml.etree import ElementTree as ET

outdir, runs, wall, selection = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), sys.argv[4]
root = ET.parse(os.path.join(outdir, "junit.xml")).getroot()

def classify(failure):
    kind = failure.get("type") or ""
    message = failure.get("message") or ""
    if kind.startswith("test failure"):
        return "product", kind
    if kind == "test abort":
        signal = re.search(r"signal (\d+)", message)
        if signal and signal.group(1) == "9":
            return "no_result", message
        return "product", message or kind
    if kind == "test timeout":
        return "no_result", kind
    return "no_result", kind or "unclassified JUnit failure"

tests = {}
iterations = set()
for suite in root.iter("testsuite"):
    # Nextest names each suite <binary>@stress-<i>; count iterations, not
    # binaries x iterations.
    stress = re.search(r"@stress-(\d+)$", suite.get("name") or "")
    iterations.add(stress.group(1) if stress else suite.get("name"))
    for case in suite.iter("testcase"):
        if any(c.tag == "skipped" for c in case):
            continue
        test = f"{case.get('classname')}::{case.get('name')}"
        row = tests.setdefault(test, {"test": test, "runs": 0, "passes": 0,
                                      "product_failures": 0, "no_results": 0,
                                      "signatures": {}})
        row["runs"] += 1
        failures = [c for c in case if c.tag in ("failure", "error")]
        if not failures:
            row["passes"] += 1
            continue
        klass, signature = classify(failures[0])
        row["product_failures" if klass == "product" else "no_results"] += 1
        row["signatures"][signature] = row["signatures"].get(signature, 0) + 1

def verdict(row):
    # Same rule as flake_verdict in ci/compat-envelope/pressure-test.rs: a
    # test is CLEAN only if every requested run was observed and passed.
    if row["product_failures"] > 0:
        return "FLAKY" if row["passes"] > 0 else "FAILING"
    if row["runs"] == runs and row["passes"] == runs:
        return "CLEAN"
    return "INCOMPLETE"

rows = sorted(tests.values(), key=lambda r: r["test"])
counts = {}
for row in rows:
    row["verdict"] = verdict(row)
    counts[row["verdict"]] = counts.get(row["verdict"], 0) + 1
summary = {"runs_requested": runs, "iterations_recorded": len(iterations),
           "selection": selection, "retries": 0, "wall_seconds": wall,
           "tests": len(rows), "verdicts": counts, "results": rows}
with open(os.path.join(outdir, "stress-test-results.json"), "w") as f:
    json.dump(summary, f, indent=2)
    f.write("\n")

lines = ["# Nextest flake census", "",
         f"- Selection: `{selection}`",
         f"- Runs: {len(iterations)}/{runs} recorded, retries off, {wall} s wall",
         "- Verdicts: " + ", ".join(f"{counts.get(v, 0)} {v}" for v in
                                     ("CLEAN", "FLAKY", "FAILING", "INCOMPLETE")),
         "",
         "| Test | Runs | Passes | Product failures | No result | Signatures | Verdict |",
         "| --- | ---: | ---: | ---: | ---: | --- | --- |"]
for row in rows:
    signatures = "; ".join(f"{k} x{v}" for k, v in sorted(row["signatures"].items()))
    lines.append(f"| `{row['test']}` | {row['runs']} | {row['passes']} | "
                 f"{row['product_failures']} | {row['no_results']} | {signatures} | {row['verdict']} |")
with open(os.path.join(outdir, "STRESS_TEST_RESULTS.md"), "w") as f:
    f.write("\n".join(lines) + "\n")
print("\n".join(lines[:5]))
for row in rows:
    if row["verdict"] != "CLEAN":
        print(f"  {row['verdict']} {row['test']}: {row['passes']}/{row['runs']} passed {row['signatures']}")
sys.exit(0 if counts.get("CLEAN", 0) == len(rows) and rows else 1)
PY
