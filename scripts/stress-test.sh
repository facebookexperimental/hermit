#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# Hermit stress-test framework.
#
# Runs the test suite (or a filtered subset) N times under cargo-nextest with a
# chosen degree of parallelism, aggregates per-test pass/fail counts across all
# iterations, categorizes any failures, and writes structured results
# (Markdown + JSON).
#
# CPU oversubscription is intentional: running many tests concurrently produces
# chaotic host scheduling that deterministic Hermit tests must be robust against.
# A flaky result here is a signal, not noise.
#
# Usage:
#   scripts/stress-test.sh [-n RUNS] [-j THREADS] [-E FILTERSET] [-p PKG]
#                          [-t TIMEOUT_SECS] [-o OUTDIR]
#
#   -n RUNS      Repeat count per test (default: 20)
#   -j THREADS   nextest test-threads; raise above core count to oversubscribe
#                (default: number of CPUs)
#   -E FILTERSET nextest filter expression, e.g. 'package(detcore-model)'
#   -p PKG       shorthand for -E 'package(PKG)'
#   -t SECS      per-iteration wall-clock timeout (default: 1800 = 30 min)
#   -o OUTDIR    output directory (default: docs)
#
# Examples:
#   scripts/stress-test.sh                          # whole suite, 20x
#   scripts/stress-test.sh -n 20 -j 32 -p detcore   # oversubscribed detcore
#   scripts/stress-test.sh -n 5 -E 'test(/futex/)'  # just futex tests, 5x

set -uo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR" || exit 1

RUNS=20
THREADS="$(nproc 2>/dev/null || echo 4)"
FILTERSET=""
TIMEOUT_SECS=1800
OUTDIR="docs"

while getopts "n:j:E:p:t:o:h" opt; do
  case "$opt" in
    n) RUNS="$OPTARG" ;;
    j) THREADS="$OPTARG" ;;
    E) FILTERSET="$OPTARG" ;;
    p) FILTERSET="package($OPTARG)" ;;
    t) TIMEOUT_SECS="$OPTARG" ;;
    o) OUTDIR="$OPTARG" ;;
    h) grep '^#' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "run with -h for usage" >&2; exit 2 ;;
  esac
done

command -v cargo-nextest >/dev/null 2>&1 || {
  echo "error: cargo-nextest is required (https://nexte.st). Install with:" >&2
  echo "  cargo install cargo-nextest --locked" >&2
  exit 2
}

WORK="$(mktemp -d "${TMPDIR:-/tmp}/hermit-stress.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT
JUNIT="target/nextest/ci/junit.xml"   # written by the [profile.ci.junit] config

declare -a NEXTEST_FILTER=()
[ -n "$FILTERSET" ] && NEXTEST_FILTER=(-E "$FILTERSET")

echo ":: Hermit stress test"
echo "   root:     $ROOT_DIR"
echo "   runs:     $RUNS"
echo "   threads:  $THREADS  (cores: $(nproc 2>/dev/null || echo '?'))"
echo "   filter:   ${FILTERSET:-<whole suite>}"
echo "   timeout:  ${TIMEOUT_SECS}s per iteration"
echo

# Build test binaries once so iteration timing/parallelism isn't polluted by
# compilation, and so a build failure aborts before we waste the window.
echo ":: building test binaries (cargo nextest run --no-run) ..."
if ! cargo nextest run --no-run "${NEXTEST_FILTER[@]}" >"$WORK/build.log" 2>&1; then
  echo "error: test build failed; see below" >&2
  tail -30 "$WORK/build.log" >&2
  exit 1
fi

# Run the suite RUNS times; keep each iteration's JUnit report.
for i in $(seq 1 "$RUNS"); do
  printf ":: iteration %d/%d ... " "$i" "$RUNS"
  rm -f "$JUNIT"
  start=$(date +%s)
  timeout "$TIMEOUT_SECS" cargo nextest run --profile ci --no-fail-fast \
    --test-threads "$THREADS" "${NEXTEST_FILTER[@]}" \
    >"$WORK/run_$i.log" 2>&1
  rc=$?
  end=$(date +%s)
  if [ -f "$JUNIT" ]; then
    cp "$JUNIT" "$WORK/iter_$i.xml"
    printf "done rc=%d (%ds)\n" "$rc" "$((end - start))"
  elif [ "$rc" -eq 124 ]; then
    printf "TIMEOUT after %ds (no report)\n" "$TIMEOUT_SECS"
  else
    printf "no JUnit report (rc=%d) -- see run_%d.log\n" "$rc" "$i"
    cp "$WORK/run_$i.log" "$WORK/iter_${i}_nolog.txt" 2>/dev/null || true
  fi
done

mkdir -p "$OUTDIR"
MD="$OUTDIR/STRESS_TEST_RESULTS.md"
JSON="$OUTDIR/stress-test-results.json"

# Aggregate all iteration XMLs into per-test pass/total + categorized failures.
python3 - "$WORK" "$MD" "$JSON" "$RUNS" "$THREADS" "${FILTERSET:-<whole suite>}" <<'PY'
import glob, os, sys, json, re
from xml.etree import ElementTree as ET

work, md_path, json_path, runs, threads, filterset = sys.argv[1:7]
runs = int(runs)

# Infra markers => "system-infra" rather than a hermit/test defect.
INFRA = [
    "perf_event_open", "perf_event_paranoid", "seccomp", "operation not permitted",
    "eperm", "namespace", "/dev/kvm", "cannot allocate memory", "enomem",
    "no space left", "enospc", "resource temporarily unavailable", "too many open files",
    "address already in use", "connection refused",
]

tests = {}   # id -> {"pass":int, "fail":int, "msgs":set}
iters_seen = 0
for xml in sorted(glob.glob(os.path.join(work, "iter_*.xml"))):
    iters_seen += 1
    try:
        root = ET.parse(xml).getroot()
    except Exception:
        continue
    for tc in root.iter("testcase"):
        cls = tc.get("classname", "")
        name = tc.get("name", "")
        tid = f"{cls}::{name}" if cls else name
        t = tests.setdefault(tid, {"pass": 0, "fail": 0, "msgs": set()})
        failed = False
        for child in tc:
            if child.tag in ("failure", "error"):
                failed = True
                msg = (child.get("message") or "") + " " + (child.text or "")
                msg = re.sub(r"\s+", " ", msg).strip()
                if msg:
                    t["msgs"].add(msg[:500])
        if failed:
            t["fail"] += 1
        else:
            t["pass"] += 1

def categorize(rec):
    total = rec["pass"] + rec["fail"]
    if rec["fail"] == 0:
        return "green"
    blob = " ".join(rec["msgs"]).lower()
    if any(m in blob for m in INFRA):
        return "system-infra"
    if rec["pass"] > 0:
        return "flaky"          # nondeterministic: hermit flake or test nondeterminism
    return "consistent-failure" # 0/N: test bug or hermit bug (needs triage)

results = []
for tid, rec in sorted(tests.items()):
    total = rec["pass"] + rec["fail"]
    results.append({
        "test": tid,
        "category": categorize(rec),
        "pass": rec["pass"],
        "total": total,
        "failures": sorted(rec["msgs"]),
    })

# Note: os.environ TZ-free timestamp; the harness disallows live clocks in some
# contexts, so read wall time from the filesystem of the newest iteration file.
ts = "unknown"
xmls = sorted(glob.glob(os.path.join(work, "iter_*.xml")))
if xmls:
    import datetime
    ts = datetime.datetime.fromtimestamp(
        os.path.getmtime(xmls[-1]), datetime.timezone.utc
    ).strftime("%Y-%m-%dT%H:%M:%SZ")

by_cat = {}
for r in results:
    by_cat.setdefault(r["category"], []).append(r)

summary = {c: len(v) for c, v in by_cat.items()}
payload = {
    "timestamp": ts,
    "runs_requested": runs,
    "iterations_recorded": iters_seen,
    "threads": int(threads),
    "filter": filterset,
    "total_tests": len(results),
    "summary": summary,
    "results": results,
}
with open(json_path, "w") as f:
    json.dump(payload, f, indent=2)
    f.write("\n")

green = summary.get("green", 0)
flaky = by_cat.get("flaky", [])
infra = by_cat.get("system-infra", [])
consistent = by_cat.get("consistent-failure", [])

lines = []
lines.append("# Hermit stress-test results")
lines.append("")
lines.append("Generated by `scripts/stress-test.sh` (each test run N times under "
             "cargo-nextest; CPU oversubscription is intentional). Re-run with a "
             "wider filter for full coverage; `-E`/`-p` scope the run.")
lines.append("")
lines.append(f"- Last stress-tested: `{ts}`")
lines.append(f"- Iterations: {iters_seen}/{runs} recorded  |  test-threads: {threads}"
             f"  |  filter: `{filterset}`")
lines.append(f"- Tests: {len(results)}  |  "
             f"green: {green}  |  flaky: {len(flaky)}  |  "
             f"system-infra: {len(infra)}  |  consistent-failure: {len(consistent)}")
lines.append("")
verdict = "ALL GREEN ✅" if len(results) and not (flaky or consistent or infra) else \
          ("FLAKY / FAILURES ⚠️" if results else "NO RESULTS ❌")
lines.append(f"**Verdict: {verdict}**")
lines.append("")
lines.append("Categories: **green** = passed every run; **flaky** = passed some / "
             "failed some (hermit flake or test nondeterminism); **system-infra** = "
             "failure text matches a known host/runner limitation; "
             "**consistent-failure** = failed every run (test bug or hermit bug — needs triage).")
lines.append("")

def table(rows):
    out = ["| test | pass/total | category |", "| --- | --- | --- |"]
    for r in rows:
        out.append(f"| `{r['test']}` | {r['pass']}/{r['total']} | {r['category']} |")
    return out

for title, rows in (("Flaky", flaky), ("Consistent failures", consistent),
                    ("System-infra failures", infra)):
    if rows:
        lines.append(f"## {title}")
        lines += table(rows)
        lines.append("")
        for r in rows:
            if r["failures"]:
                lines.append(f"<details><summary>{r['test']} diagnostics</summary>")
                lines.append("")
                for m in r["failures"][:5]:
                    lines.append(f"- {m}")
                lines.append("")
                lines.append("</details>")
                lines.append("")

lines.append(f"## All tests ({len(results)})")
lines += table(results)
lines.append("")
lines.append(f"Machine-readable results: `{os.path.basename(json_path)}`.")

with open(md_path, "w") as f:
    f.write("\n".join(lines) + "\n")

print(f"\n:: wrote {md_path} and {json_path}")
print(f":: tests={len(results)} green={green} flaky={len(flaky)} "
      f"infra={len(infra)} consistent-failure={len(consistent)}")
sys.exit(0 if not (flaky or consistent) else 1)
PY
STATUS=$?

echo
if [ "$STATUS" -eq 0 ]; then
  echo ":: RESULT: all recorded tests are green."
else
  echo ":: RESULT: flaky or failing tests detected (see $MD)."
fi
exit "$STATUS"
