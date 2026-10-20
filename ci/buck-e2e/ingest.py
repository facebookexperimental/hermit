#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

"""Rebuild the e2e result files of a Buck test run from its Tpx results.

usage: ingest.py --plan ci/expected-e2e-plan.json --out IMPORT_DIR [--work DIR]
                 [--local-artifacts BUCK_OUT_TEST_DIR] TEST_RUN_ID...

TEST_RUN_ID is what `buck2 test --write-test-id FILE` wrote (one per invocation; the
hybrid run makes two, one for RE cells and one for local cells). Every cell execution
uploads the rows test-harness wrote as its results.jsonl Tpx artifact; this fetches
them with testx (TESTX env, default `testx`) and writes one bucket per (lane, category):

    IMPORT_DIR/<lane>/manifest_<category>/results.jsonl

    IMPORT_DIR/<lane>/manifest_<category>/summary.json

IMPORT_DIR is input for `test-harness run` in import mode (E2E_IMPORT_RESULTS=IMPORT_DIR)
and for nothing else. Import mode checks every cell's history, staleness and evidence,
then publishes the result files the cargo flow's e2e nodes write. Read directly, these
files skip all of those checks, and summary.json's counts leave out every cell whose
final execution wrote no summary.json.
summary.json aggregates each cell's final execution: counts are summed and cell lists
joined, which is what one harness process over the bucket writes.
summary.json's evidence_complete_executions lists each execution whose result.json
(written by cell.sh) records complete evidence, by its run id; import mode refuses a
PASS whose own execution is not listed. An execution's rows, result.json and run_id.
marker (see --local-artifacts) must name one run id, and no two executions may share one.
Tpx owns retries (cells run the harness with --no-retry). A cell's Nth execution in Tpx
order is attempt N, and its row (at most one, for its own cell) carries that number. An
execution that wrote no row still takes its number, so the next row shows the gap and
import mode refuses the history. "Passed only on rerun" therefore stays visible, as
attempt 1 failing before attempt 2 passes, even when attempt 1 died before writing a row.
A cell with no rows is host-inapplicable only if it ran and every one of its executions
says so in its own summary.json. Any other plan cell with no rows is an error, and so is
a cell outside the plan. Executions of one cell that end in the same second are refused:
their order is unknown, and testx addresses an artifact as RUN.TEST.END. Prints one
JSON summary line.

--local-artifacts: Buck materializes each test's artifact directory locally
(buck-out/v2/test/execution/<cell>/<target hash>/<config hash>/default/artifacts_directory),
keeping the newest execution per target. cell.sh writes an artifact named run_id.<run id>,
which the testx listing shows without a fetch. An execution is read from the one local
directory holding its marker; every other execution, including one whose marker is in
no local directory or in several, is fetched with testx.
"""

import argparse
import collections
import concurrent.futures as cf
import json
import os
import re
import subprocess
import sys
import tempfile
import time

TESTX = os.environ.get("TESTX", "testx")
MARKER = "run_id."  # cell.sh: an artifact named run_id.<run id>


def testx(*args, tries=4):
    for i in range(tries):
        p = subprocess.run([TESTX, *args], capture_output=True, text=True)
        if p.returncode == 0:
            return p.stdout
        time.sleep(5 * (i + 1))  # TestX answers 500 transiently
    sys.exit(f"ingest: testx {' '.join(args[:3])} failed: {p.stderr[-500:]}")


def rows_of(path):
    if path.endswith(".zst"):
        text = subprocess.run(
            ["zstd", "-q", "-d", "-c", path], check=True, capture_output=True, text=True
        ).stdout
    else:
        text = open(path).read()
    return [json.loads(l) for l in text.splitlines() if l.strip()]


def markers(names):
    return [n[len(MARKER) :] for n in names if n.startswith(MARKER)]


def cell_of(entry):
    return "{}/{}@{}".format(
        entry.get("test"), entry.get("mode"), entry.get("backend") or "native"
    )


def claims_host_inapplicable(cell, summary):
    return any(
        cell_of(h) == cell for h in (summary or {}).get("host_inapplicable_cells", [])
    )


def main():
    ap = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    ap.add_argument("--plan", required=True)
    ap.add_argument(
        "--out",
        required=True,
        help="IMPORT_DIR: input for test-harness run's import mode only",
    )
    ap.add_argument(
        "--work", help="where fetched artifacts go (default: a temporary directory)"
    )
    ap.add_argument("-j", type=int, default=32)
    ap.add_argument(
        "--local-artifacts",
        help="buck-out/v2/test/execution: read an execution from here when one directory holds its marker",
    )
    ap.add_argument("run_ids", nargs="+")
    a = ap.parse_args()
    plan = json.load(open(a.plan))
    want = {
        "{}/{}@{}".format(c["test"], c["mode"], c["backend"]): c for c in plan["cells"]
    }
    work = a.work or tempfile.mkdtemp(prefix="buck-e2e-ingest-")
    os.makedirs(work, exist_ok=True)  # testx needs the parent of --output-dir to exist
    executions = []  # (cell, end, test id, test run id, marker run id, result)
    for rid in a.run_ids:
        for t in json.loads(testx("--as-json", "results", "list", rid))["results"][
            "test_results"
        ]:
            name = t["test_details"]["name"]
            # Each target also reports an "unmanaged" twin of the same execution.
            if " - " in name and not name.endswith(" - unmanaged"):
                cell, end, tid = (
                    name.split(" - ", 1)[1],
                    int(t["end_time"]),
                    t["test_details"]["id"],
                )
                found = markers(x["name"] for x in t.get("artifacts") or [])
                if len(found) > 1:
                    sys.exit(
                        f"ingest: the execution of {cell} that ended at {end} has {len(found)} run id markers: {found[:5]}"
                    )
                executions.append((cell, end, tid, rid, found[0] if found else None, t))
    ends = collections.Counter((cell, end) for cell, end, *_ in executions)
    ties = sorted(f"{cell} at {end}" for (cell, end), n in ends.items() if n > 1)
    if ties:
        sys.exit(
            f"ingest: executions of one cell ended in the same second, so their order is unknown "
            f"and testx (RUN.TEST.END) cannot tell their artifacts apart: {ties[:20]} ({len(ties)})"
        )

    local = collections.defaultdict(
        list
    )  # run id -> local artifact directories holding its marker
    if a.local_artifacts:
        for dirpath, dirnames, filenames in os.walk(a.local_artifacts):
            if os.path.basename(dirpath) == "artifacts_directory":
                dirnames[:] = []
                found = markers(filenames)
                if len(found) == 1:
                    local[found[0]].append(dirpath)

    def fetch(item):
        cell, end, tid, rid, marker, t = item
        if marker is not None and len(local.get(marker, [])) == 1:
            source, d = "local", local[marker][0]
            names = os.listdir(d)
            rows_name = next(
                (n for n in ("results.jsonl", "results.jsonl.zst") if n in names), None
            )
            wanted = [
                n for n in (rows_name, "summary.json", "result.json") if n in names
            ]
        else:
            source = "testx"
            names = [x["name"] for x in t.get("artifacts") or []]
            rows_name = next(
                (n for n in ("results.jsonl", "results.jsonl.zst") if n in names), None
            )
            wanted = [
                n for n in (rows_name, "summary.json", "result.json") if n in names
            ]
            d = os.path.join(
                work, re.sub(r"[^A-Za-z0-9._-]+", "_", f"{cell}__{tid}__{end}")
            )
            if wanted and not all(os.path.exists(os.path.join(d, n)) for n in wanted):
                args = ["artifacts", "get", f"{rid}.{tid}.{end}", "--output-dir", d]
                for n in wanted:
                    args += ["--artifact-names", n]
                testx(*args)
        rows = rows_of(os.path.join(d, rows_name)) if rows_name else []
        summary = (
            json.load(open(os.path.join(d, "summary.json")))
            if "summary.json" in wanted
            else None
        )
        result = (
            json.load(open(os.path.join(d, "result.json")))
            if "result.json" in wanted
            else None
        )
        return cell, end, marker, source, rows, summary, result

    with cf.ThreadPoolExecutor(a.j) as ex:
        fetched = list(ex.map(fetch, executions))
    sources = collections.Counter(source for _, _, _, source, *_ in fetched)
    per_cell = collections.defaultdict(list)  # cell -> [(rows, summary)] in Tpx order
    complete_runs = collections.defaultdict(
        list
    )  # cell -> run ids of evidence-complete executions
    run_cells = {}
    for cell, end, marker, source, rows, summary, result in sorted(
        fetched, key=lambda x: (x[0], x[1])
    ):
        where = f"the execution of {cell} that ended at {end}"
        if result is not None and result.get("cell") != cell:
            sys.exit(f"ingest: {where} has a result.json for {result.get('cell')}")
        if len(rows) > 1 or any(cell_of(row) != cell for row in rows):
            sys.exit(
                f"ingest: {where} has {len(rows)} rows, for cells {sorted({cell_of(row) for row in rows})}; "
                f"a cell runs the harness once (--no-retry), which writes one row, for that cell"
            )
        ids = [row.get("run_id") for row in rows] + (
            [result.get("run_id")] if result is not None else []
        )
        ids += [marker] if marker is not None else []
        if len(set(ids)) > 1 or None in ids:
            sys.exit(
                f"ingest: {where} has run ids {sorted(set(map(str, ids)))}; "
                f"its rows, its result.json and its {MARKER} marker must all name the same one"
            )
        run_id = ids[0] if ids else None
        if run_id is not None:
            if run_id in run_cells:
                sys.exit(
                    f"ingest: run {run_id} names two executions: {run_cells[run_id]} and {where}"
                )
            run_cells[run_id] = where
        if result is not None and result.get("evidence_complete") is True:
            complete_runs[cell].append(run_id)
        per_cell[cell].append((rows, summary))

    def covered(cell):
        runs = per_cell.get(cell, [])
        return any(rows for rows, _ in runs) or (
            runs and all(claims_host_inapplicable(cell, s) for _, s in runs)
        )

    missing = sorted(c for c in want if not covered(c))
    extra = sorted(set(per_cell) - set(want))
    if missing or extra:
        sys.exit(
            f"ingest: cells with neither a row nor a host-inapplicable claim from every execution: "
            f"{missing[:20]} ({len(missing)}); unexpected cells: {extra[:20]} ({len(extra)})"
        )
    buckets = collections.defaultdict(list)
    summaries = collections.defaultdict(list)
    evidence_complete = collections.defaultdict(list)
    attempts = collections.Counter()
    final = collections.Counter()
    no_row = []  # "cell attempt N" for each execution that wrote no row and claimed no host inapplicability
    for cell, runs in per_cell.items():
        lane, category = want[cell]["lane"], want[cell]["category"]
        final_rows, final_summary = runs[-1]
        if final_summary:
            summaries[(lane, category)].append(final_summary)
        c = want[cell]
        for run_id in complete_runs[cell]:
            evidence_complete[(lane, category)].append(
                {
                    "test": c["test"],
                    "mode": c["mode"],
                    "backend": None if c["backend"] == "native" else c["backend"],
                    "run_id": run_id,
                }
            )
        for attempt, (rows, summary) in enumerate(runs, 1):
            for row in rows:
                buckets[(lane, category)].append(dict(row, attempt=attempt))
                attempts[attempt] += 1
            if not rows and not claims_host_inapplicable(cell, summary):
                no_row.append(f"{cell} attempt {attempt}")
        final[
            final_rows[-1]["outcome"]
            if final_rows
            else "HOST-INAPPLICABLE"
            if claims_host_inapplicable(cell, final_summary)
            else "NO-ROW"
        ] += 1
    for key in sorted(set(buckets) | set(summaries)):
        lane, category = key
        d = os.path.join(a.out, lane, "manifest_" + category.replace("-", "_"))
        os.makedirs(d, exist_ok=True)
        with open(os.path.join(d, "results.jsonl"), "w") as f:
            for row in sorted(
                buckets[key],
                key=lambda r: (
                    "{}/{}@{}".format(r["test"], r["mode"], r.get("backend")),
                    r["attempt"],
                ),
            ):
                f.write(json.dumps(row, sort_keys=True) + "\n")
        total = {}
        for one in summaries[key]:
            for k, v in one.items():
                if k == "schema":
                    total[k] = v
                elif isinstance(v, list):
                    total.setdefault(k, []).extend(v)
                elif isinstance(v, (int, float)):
                    total[k] = total.get(k, 0) + v
        total["evidence_complete_executions"] = sorted(
            evidence_complete[key],
            key=lambda c: (c["test"], c["mode"], c["backend"] or "", c["run_id"]),
        )
        with open(os.path.join(d, "summary.json"), "w") as f:
            json.dump(total, f, indent=2, sort_keys=True)
            f.write("\n")
    print(
        json.dumps(
            {
                "sources": dict(sources),
                "cells": len(per_cell),
                "buckets": len(set(buckets) | set(summaries)),
                "rows": sum(attempts.values()),
                "attempts": dict(attempts),
                "final_outcomes": dict(final),
                "no_row_executions": len(no_row),
                "no_row_examples": sorted(no_row)[:20],
                "evidence_complete_executions": sum(
                    len(v) for v in evidence_complete.values()
                ),
            }
        )
    )


if __name__ == "__main__":
    main()
