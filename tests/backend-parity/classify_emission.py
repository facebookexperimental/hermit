#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

"""Classify what each backend-parity C fixture EMITS, and how blind that is.

Why this exists as a committed tool rather than a one-off command: the emission
census for this fixture family has been re-derived by hand at least three times
and produced three different, irreconcilable denominators (46/73, 24/46/3,
23/59). Two of those were method failures -- a `head -2` that truncated
multi-line output, and a `grep -oE | grep -qv` pipeline that classified the same
input two different ways depending on whether it ran inside a loop. So:

  * every fixture's FULL stdout/stderr/rc is cached to a file, and
  * classification reads those files in Python.

No shell pipeline participates in classification, and nothing is truncated.

Fixtures are run UNDER HERMIT, not natively. That is load-bearing: the refusal
fixtures assert that hermit refuses a syscall, so run natively the syscall
succeeds and they bail to stderr before reaching their emit path, printing zero
bytes. Classifying them natively reports their shape as UNKNOWN (or, worse, as a
distinct "emits nothing" class, which they are not).

Usage:
    tests/backend-parity/classify_emission.py --hermit target/debug/hermit
    tests/backend-parity/classify_emission.py --json census.json

Exit status is 0 when the census completes; it is not a pass/fail gate.
"""

import argparse
import json
import os
import re
import subprocess
import sys
from pathlib import Path

import yaml

REPO = Path(__file__).resolve().parent.parent.parent
FIXTURE_DIR = REPO / "tests" / "backend-parity" / "fixtures"

# A key whose value is a count of checks that passed, not an observation.
TALLY_KEYS = {"ok", "checks", "count", "n"}

KV = re.compile(r"([A-Za-z_][A-Za-z0-9_]*)=([^\s]+)")

# Predicates that accept a range of values rather than one exact value. A
# fixture guarded only by these can score a clean tally on a WRONG-but-plausible
# observation, which is the failure cross-backend comparison can never catch:
# two backends wrong the same way agree.
LOOSE = re.compile(
    r"""(?x)
    (?:>\s*0|>=\s*1|!=\s*0|!=\s*-1|>\s*-1)      # numeric looseness
    | !=\s*NULL | ==\s*NULL                      # pointer-only checks
    """
)


def classify_stdout(text):
    """Return (class, emitted_keys). Pure function of the cached bytes."""
    pairs = KV.findall(text)
    if not pairs:
        return ("NO-KV-PAIRS" if text.strip() else "EMPTY"), []
    observed = []
    for key, value in pairs:
        if key in TALLY_KEYS:
            continue
        # A bare 0/1 is a de-aliased flag, not an observed value.
        if value in ("0", "1"):
            observed.append((key, value, "flag"))
        else:
            observed.append((key, value, "value"))
    if any(kind == "value" for _, _, kind in observed):
        return "EMITS-VALUE", observed
    if observed:
        return "FLAGS-ONLY", observed
    return "TALLY-ONLY", observed


def scan_source(path):
    """Static risk features of the fixture's C source."""
    src = path.read_text(errors="replace")
    # Strip comments so prose about `> 0` does not count as a loose predicate.
    src = re.sub(r"/\*.*?\*/", "", src, flags=re.S)
    src = re.sub(r"//[^\n]*", "", src)
    # Fail-fast: a failed check returns immediately, so the tally cannot alias
    # (if it prints at all, every check passed). Accumulating fixtures can.
    returns = len(re.findall(r"\breturn\s+[1-9]\b", src))
    increments = len(re.findall(r"\bok\s*\+\+|\bok\s*\+=", src))
    return {
        "fail_fast_returns": returns,
        "ok_increments": increments,
        "accumulates": increments > 0 and returns == 0,
        "loose_predicates": len(LOOSE.findall(src)),
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--hermit", default="target/debug/hermit")
    ap.add_argument("--cache", default="ignored/emission-census")
    ap.add_argument("--json", default=None)
    ap.add_argument("--native", action="store_true",
                    help="run without hermit (WRONG for refusal fixtures; for comparison only)")
    ap.add_argument("--fixture-dir", default=None,
                    help="classify a different checkout of the fixtures (for A/B against another rev)")
    ap.add_argument("--virtualize-cpuid", action="store_true",
                    help="drop --no-virtualize-cpuid (the portable lane sets it; cpuid_probe needs it off)")
    ap.add_argument("--population", choices=("dir", "bucket"), default="dir",
                    help="'dir' = every *.c under fixtures/; 'bucket' = every program "
                         "the backend-parity-c manifest registers")
    args = ap.parse_args()

    global FIXTURE_DIR
    if args.fixture_dir:
        FIXTURE_DIR = Path(args.fixture_dir).resolve()

    cache = REPO / args.cache
    (cache / "out").mkdir(parents=True, exist_ok=True)
    hermit = (REPO / args.hermit).resolve()
    if not args.native and not hermit.exists():
        sys.exit(f"hermit binary not found: {hermit}  (cargo build -p hermit --bin hermit)")

    # Per-test cflags come from the manifest. Hardcoding -D_GNU_SOURCE for every
    # fixture is wrong: numa_node_identity defines _GNU_SOURCE itself and is
    # declared with no cflags, so forcing the flag makes it fail -Werror on a
    # redefinition that the real harness never triggers.
    manifest = yaml.safe_load(
        (REPO / "tests/e2e/manifests/backend-parity-c.yaml").read_text()
    )
    cflags_by_program = {}
    for test in manifest["test"]:
        program = test.get("program")
        if not program:
            continue
        cflags_by_program[program] = test.get("build", {}).get("cflags", [])

    if args.population == "bucket":
        fixtures = [REPO / m for m in sorted(cflags_by_program)]
        missing = [f for f in fixtures if not f.exists()]
        if missing:
            sys.exit(f"manifest references missing programs: {missing}")
    else:
        fixtures = sorted(FIXTURE_DIR.glob("*.c"))
    env = dict(os.environ, LC_ALL="C", TZ="UTC")

    rows = []
    for src in fixtures:
        name = src.stem
        binary = cache / name
        rel = str(src.relative_to(REPO)) if str(src).startswith(str(REPO)) else None
        cflags = cflags_by_program.get(rel)
        if cflags is None:
            # A fixture-dir A/B run resolves cflags by basename instead.
            cflags = next((v for k, v in cflags_by_program.items()
                           if Path(k).name == src.name), ["-D_GNU_SOURCE"])
        compile_proc = subprocess.run(
            ["cc", "-std=c11", "-O2", "-g", "-Wall", "-Wextra", "-Werror",
             *cflags, "-pthread", str(src), "-o", str(binary)],
            capture_output=True, text=True)
        if compile_proc.returncode != 0:
            rows.append({"fixture": name, "class": "COMPILE-FAIL", "rc": None,
                         "stdout": "", "compile_stderr": compile_proc.stderr[-2000:],
                         **scan_source(src)})
            continue

        if args.native:
            cmd = [str(binary)]
        else:
            cmd = [str(hermit), "--log=off", "run", "--backend", "ptrace", "--strict"]
            if not args.virtualize_cpuid:
                cmd.append("--no-virtualize-cpuid")
            cmd += ["--max-timeslice=disabled", "--", str(binary)]
        try:
            proc = subprocess.run(cmd, capture_output=True, text=True, timeout=90, env=env)
            out, err, rc = proc.stdout, proc.stderr, proc.returncode
        except subprocess.TimeoutExpired:
            out, err, rc = "", "TIMEOUT", None

        # Cache the FULL streams before any classification happens.
        (cache / "out" / f"{name}.out").write_text(out)
        (cache / "out" / f"{name}.err").write_text(err)

        klass, observed = classify_stdout(out)
        rows.append({
            "fixture": name, "class": klass, "rc": rc,
            "stdout": out, "stdout_lines": len(out.splitlines()),
            "observed": [{"key": k, "value": v, "kind": t} for k, v, t in observed],
            **scan_source(src),
        })

    blind = [r for r in rows if r["class"] in ("TALLY-ONLY", "NO-KV-PAIRS", "EMPTY")]
    aliasing = [r for r in blind if r.get("accumulates")]
    loose = [r for r in rows if r.get("loose_predicates", 0) > 0]

    summary = {
        "population": len(rows),
        "by_class": {k: sum(1 for r in rows if r["class"] == k)
                     for k in sorted({r["class"] for r in rows})},
        "blind_total": len(blind),
        "blind_and_accumulating": len(aliasing),
        "any_loose_predicate": len(loose),
        "multiline_stdout": sum(1 for r in rows if r.get("stdout_lines", 0) > 1),
        "nonzero_rc": sum(1 for r in rows if r.get("rc") not in (0, None)),
    }

    print(json.dumps(summary, indent=2))
    print()
    print(f"{'fixture':34} {'class':13} {'rc':>3} {'acc':>4} {'loose':>6}  stdout")
    for r in sorted(rows, key=lambda r: (r["class"], r["fixture"])):
        print(f"{r['fixture']:34} {r['class']:13} {str(r['rc']):>3} "
              f"{str(r.get('accumulates', '')):>4} {r.get('loose_predicates', 0):>6}  "
              f"{r.get('stdout', '').strip()[:60]}")

    if args.json:
        Path(args.json).write_text(json.dumps({"summary": summary, "rows": rows}, indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main())
