#!/usr/bin/env python3
"""S1: tracked geomean report over qualifying workloads only, JSONL for the ledger.

Qualification rule (the whole point): a workload enters the geomean only if
  (a) its native median wall is >= --min-native-s (default 30 s, rr's rule), OR
  (b) it carries an intercept-corrected ratio (fixed startup removed via a
      fitted intercept, e.g. a per-syscall slope ratio).
Startup-dominated rows are emitted with status="excluded" and a reason, never
silently dropped and never averaged in.

First population: the seed suite (REPORT rows 1-16 and 20). Seed workloads are
emitted as population rows even before they are timed, so the ledger can track
coverage: status="not_measured" until a qualifying measurement lands.

Input: a direct_run.py results.json. Output JSONL rows:
  {"kind":"population", "workload":..., "seed":true, "status":...}
  {"kind":"measurement", "workload":..., "backend":..., "ratio":...,
   "basis":"intercept_corrected"|"long_wall", "native_median_s":...}
  {"kind":"summary", "backend":..., "geomean":..., "n_workloads":...,
   "excluded":[workload,...]}
"""

from __future__ import annotations

import argparse
import json
import math
import sys
from pathlib import Path

SEED_WORKLOADS = [
    "applications/kvm-python-examples",
    "data-handling/zstd-multithread",
    "language-runtimes/example-python-random",
    "applications/timed-progress-bar",
    "language-runtimes/python-io-subprocess-time",
    "applications/c-toolchain-workflow",
    "applications/git-repository-workflow",
    "data-handling/archive-roundtrip",
    "data-handling/jq-json-transform",
    "data-handling/shell-pipeline",
    "data-handling/sqlite-query-determinism",
    "determinism-stress/process-chains",
    "determinism-stress/thread-contention",
    "language-runtimes/node-v8-jit",
    "language-runtimes/cpp-stl-determinism",
    "language-runtimes/perl-hash-order",
    "determinism-stress/thread-interleaving",
]


def geomean(ratios: list[float]) -> float:
    if not ratios or any(r <= 0 for r in ratios):
        raise ValueError("geomean needs positive ratios")
    return math.exp(sum(math.log(r) for r in ratios) / len(ratios))


def qualifies(native_median_s: float | None, intercept_corrected: bool,
              min_native_s: float) -> bool:
    if intercept_corrected:
        return True
    return native_median_s is not None and native_median_s >= min_native_s


def corrected_ratio(backend_median_s: float, backend_intercept_s: float,
                    native_median_s: float, native_intercept_s: float) -> float:
    num = backend_median_s - backend_intercept_s
    den = native_median_s - native_intercept_s
    if den <= 0 or num <= 0:
        raise ValueError("intercept correction produced a non-positive term")
    return num / den


def build_rows(results: dict, min_native_s: float) -> list[dict]:
    rows: list[dict] = []
    workloads = results.get("workloads", {})
    getpid = workloads.get("getpid", {})
    native_slope = (getpid.get("native") or {}).get("slope_us_per_call")

    measured: set[str] = set()
    per_backend: dict[str, list[float]] = {}
    excluded: dict[str, list[str]] = {}

    # Intercept-corrected micro row: per-syscall slope ratio (qualifies by (b)).
    if native_slope:
        for backend, entry in getpid.items():
            if backend == "native" or "slope_us_per_call" not in entry:
                continue
            ratio = entry["slope_us_per_call"] / native_slope
            rows.append({"kind": "measurement", "workload": "micro/getpid-slope",
                         "backend": backend, "ratio": ratio,
                         "basis": "intercept_corrected", "native_median_s": None})
            per_backend.setdefault(backend, []).append(ratio)
        measured.add("micro/getpid-slope")

    # Long-wall rows from any workload with per-backend wall medians.
    for name, per_backend_walls in workloads.items():
        if name in ("getpid", "startup_true") or not isinstance(per_backend_walls, dict):
            continue
        native = (per_backend_walls.get("native") or {}).get("wall", {}).get("median")
        for backend, entry in per_backend_walls.items():
            if backend == "native":
                continue
            median = (entry.get("wall") or {}).get("median")
            if median is None or native is None:
                continue
            if qualifies(native, False, min_native_s):
                rows.append({"kind": "measurement", "workload": name,
                             "backend": backend, "ratio": median / native,
                             "basis": "long_wall", "native_median_s": native})
                per_backend.setdefault(backend, []).append(median / native)
                measured.add(name)
            else:
                excluded.setdefault(backend, []).append(name)

    # Startup is always excluded: it is the intercept, not a workload.
    for backend in (workloads.get("startup_true") or {}):
        if backend != "native":
            excluded.setdefault(backend, []).append("startup_true")

    for backend, ratios in sorted(per_backend.items()):
        rows.append({"kind": "summary", "backend": backend,
                     "geomean": geomean(ratios), "n_workloads": len(ratios),
                     "excluded": sorted(set(excluded.get(backend, [])))})

    for name in SEED_WORKLOADS:
        rows.append({"kind": "population", "workload": name, "seed": True,
                     "status": "measured" if name in measured else "not_measured"})
    return rows


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("results", type=Path, help="direct_run.py results.json")
    ap.add_argument("--min-native-s", type=float, default=30.0)
    ap.add_argument("--output", type=Path, default=None,
                    help="JSONL output (default: stdout)")
    args = ap.parse_args()

    results = json.loads(args.results.read_text())
    rows = build_rows(results, args.min_native_s)
    lines = "".join(json.dumps(r, sort_keys=True) + "\n" for r in rows)
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(lines)
        print(f"wrote {args.output} ({len(rows)} rows)", file=sys.stderr)
    else:
        sys.stdout.write(lines)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
