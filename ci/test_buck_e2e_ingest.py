#!/usr/bin/env python3
"""Scenario tests for ci/buck-e2e/ingest.py, run against a stand-in testx.

Each test lays out a Buck test run the way testx lists it (every cell execution
with its uploaded artifacts, and optionally Buck's local copies under buck-out),
runs ingest.py on it, and checks either the refusal or the rows, attempt numbers
and evidence list that ingest.py writes for test-harness's import mode, and the
verify logs it keeps (--failed-verify-logs) from executions that did not pass.
"""

from __future__ import annotations

import hashlib
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


INGEST = Path(__file__).resolve().parent / "buck-e2e" / "ingest.py"
X, Y = "t/x/custom@ptrace", "t/y/verify@native"
PLAN = {
    "cells": [
        {"test": "t/x", "mode": "custom", "backend": "ptrace", "lane": "portable", "category": "cat"},
        {"test": "t/y", "mode": "verify", "backend": "native", "lane": "portable", "category": "cat"},
    ]
}
# testx, for the two things ingest.py asks of it: `--as-json results list RUN`
# and `artifacts get RUN.TEST.END --output-dir DIR --artifact-names NAME...`. With
# FAKE_TESTX_GARBLE, a get that asks for any name containing it writes garbage in
# place of every file it was asked for, as a download cut short would.
FAKE_TESTX = r"""#!/usr/bin/env python3
import os, shutil, sys
root, args = os.environ["FAKE_TESTX_DIR"], sys.argv[1:]
if args[:3] == ["--as-json", "results", "list"]:
    sys.stdout.write(open(os.path.join(root, "list-" + args[3] + ".json")).read())
elif args[:2] == ["artifacts", "get"]:
    out = args[args.index("--output-dir") + 1]
    os.makedirs(out, exist_ok=True)
    names = [args[i + 1] for i, arg in enumerate(args) if arg == "--artifact-names"]
    garble = os.environ.get("FAKE_TESTX_GARBLE")
    for name in names:
        if garble and any(garble in n for n in names):
            open(os.path.join(out, name), "w").write("cut short")
        else:
            shutil.copy(os.path.join(root, "art", args[2], name), out)
else:
    sys.exit("fake testx: unexpected " + repr(args))
"""


def row(cell, outcome, run_id):
    """The row `test-harness run --no-retry` writes for CELL: always attempt 1."""
    test, rest = cell.rsplit("/", 1)
    mode, backend = rest.split("@")
    return {
        "test": test,
        "mode": mode,
        "backend": None if backend == "native" else backend,
        "outcome": outcome,
        "run_id": run_id,
        "attempt": 1,
    }


def claim(cell):
    """A summary.json host_inapplicable_cells entry for CELL."""
    return {key: row(cell, None, None)[key] for key in ("test", "mode", "backend")}


def execution(cell, end, outcome, run_id, complete=True, **changes):
    """One execution of CELL that ended at END, as cell.sh uploads it: the row
    the harness wrote (OUTCOME None: it wrote none), summary.json, result.json
    and the run_id.RUN_ID marker. CHANGES replaces any of those."""
    uploaded = {
        "cell": cell,
        "end": end,
        "rows": [row(cell, outcome, run_id)] if outcome else None,
        "summary": {"schema": 1, "passed": int(outcome == "PASS")} if outcome else None,
        "result": {"cell": cell, "run_id": run_id, "evidence_complete": complete},
        "markers": [run_id],
        "uploaded": True,
    }
    uploaded.update(changes)
    return uploaded


def write_artifacts(directory, uploaded):
    """Write an execution's artifact files into DIRECTORY; return their names."""
    directory.mkdir(parents=True)
    if uploaded["rows"] is not None:
        lines = "".join(json.dumps(r) + "\n" for r in uploaded["rows"])
        (directory / "results.jsonl").write_text(lines)
    if uploaded["summary"] is not None:
        (directory / "summary.json").write_text(json.dumps(uploaded["summary"]))
    if uploaded["result"] is not None:
        (directory / "result.json").write_text(json.dumps(uploaded["result"]))
    for marker in uploaded["markers"]:
        (directory / f"run_id.{marker}").write_text(f"{marker}\n")
    for name, blob in uploaded.get("files", {}).items():
        (directory / name).write_bytes(blob)
    return sorted(path.name for path in directory.iterdir())


def zstd(blob):
    return subprocess.run(["zstd", "-q", "-c"], input=blob, capture_output=True, check=True).stdout


def with_logs(uploaded, logs, sha256=None):
    """UPLOADED, also returning LOGS ({artifact name: bytes}) the way cell.sh does: as
    artifacts whose sha256 its result.json records (SHA256 replaces any of those)."""
    uploaded = dict(uploaded, files=dict(uploaded.get("files", {}), **logs))
    hashes = {name: hashlib.sha256(blob).hexdigest() for name, blob in uploaded["files"].items()}
    uploaded["result"] = dict(uploaded["result"], artifact_sha256=dict(hashes, **(sha256 or {})))
    return uploaded


# What hermit --verify kept in the cell's verify-logs/verify-1 after two runs that
# differed, as cell.sh returns it: run 2's log is over 256 KiB, so it is compressed.
RUN1_LOG = b"run 1 DETLOG\n"
RUN2_LOG = b"run 2 DETLOG " * 30000
DIVERGED_LOGS = {
    "cell__verify-logs__verify-1__run1_log_detlog": RUN1_LOG,
    "cell__verify-logs__verify-1__run2_log_detlog.zst": zstd(RUN2_LOG),
}


def write_test_runs(fake, runs):
    """Lay out RUNS ({test run id: [execution, ...]}) in directory FAKE as FAKE_TESTX
    serves them: each run's listing, and each uploaded execution's artifact files."""
    fake.mkdir()
    test_id = 0
    for run_id, executions in runs.items():
        listed = []
        for uploaded in executions:
            test_id += 1
            address = f"{run_id}.{test_id}.{uploaded['end']}"
            names = write_artifacts(fake / "art" / address, uploaded) if uploaded["uploaded"] else []
            name = f"//ci/buck-e2e:cell - {uploaded['cell']}"
            listed.append(
                {
                    "test_details": {"name": name, "id": test_id},
                    "end_time": uploaded["end"],
                    "artifacts": [{"name": n} for n in names],
                }
            )
            # Tpx also lists an "unmanaged" twin of every execution.
            listed.append(
                {
                    "test_details": {"name": f"{name} - unmanaged", "id": test_id + 1000},
                    "end_time": uploaded["end"],
                    "artifacts": [],
                }
            )
        listing = {"results": {"test_results": listed}}
        (fake / f"list-{run_id}.json").write_text(json.dumps(listing))


def withheld_on(route, cell, end, run_id):
    """An execution of CELL that withheld it (no row, a host-inapplicable claim)
    on ROUTE, as cell.sh records it in result.json."""
    summary = {"schema": 1, "host_inapplicable_cells": [claim(cell)]}
    result = {"cell": cell, "run_id": run_id, "evidence_complete": True, "route": route}
    return execution(cell, end, None, run_id, summary=summary, result=result)


Y_PASSES = execution(Y, 150, "PASS", "ry1")
HOST_INAPPLICABLE_X = {"schema": 1, "host_inapplicable_cells": [claim(X)]}
X_SLUG = "t-x-custom-ptrace"


class IngestTest(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory(prefix="test-buck-e2e-ingest-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.testx = self.root / "testx"
        self.testx.write_text(FAKE_TESTX)
        self.testx.chmod(0o755)
        (self.root / "plan.json").write_text(json.dumps(PLAN))

    def ingest(self, runs, local=(), extra=(), **environment):
        """Run ingest.py on RUNS ({test run id: [execution, ...]}), with each
        execution in LOCAL copied into its own buck-out artifacts directory and
        EXTRA added to its arguments. Its --withheld-on-re list goes to
        WORK/withheld-on-re."""
        work = Path(tempfile.mkdtemp(dir=self.root))
        fake = work / "fake"
        write_test_runs(fake, runs)
        command = [
            sys.executable,
            str(INGEST),
            "--plan",
            str(self.root / "plan.json"),
            "--out",
            str(work / "out"),
            "--work",
            str(work / "fetched"),
            "--withheld-on-re",
            str(work / "withheld-on-re"),
        ]
        if local:
            buck_out = work / "buck-out-test-execution"
            for index, uploaded in enumerate(local):
                target = buck_out / "root" / f"target{index}" / "config" / "default"
                write_artifacts(target / "artifacts_directory", uploaded)
            command += ["--local-artifacts", str(buck_out)]
        environment = dict(os.environ, TESTX=str(self.testx), FAKE_TESTX_DIR=str(fake), **environment)
        process = subprocess.run(
            command + list(extra) + list(runs), capture_output=True, text=True, env=environment
        )
        return work, process

    def refused(self, runs, reason, local=()):
        _, process = self.ingest(runs, local)
        self.assertEqual(process.returncode, 1, process.stdout + process.stderr)
        self.assertIn(reason, process.stderr)

    def accepted(self, runs, local=(), withheld_on_re=()):
        """Return ingest.py's printed summary, the bucket's rows as (cell,
        attempt, outcome, run id), and the bucket's summary.json. The cells it
        lists as withheld on RE by every execution must be WITHHELD_ON_RE."""
        work, process = self.ingest(runs, local)
        self.assertEqual(process.returncode, 0, process.stderr)
        self.assertEqual((work / "withheld-on-re").read_text(), "".join(f"{slug}\n" for slug in withheld_on_re))
        bucket = work / "out" / "portable" / "manifest_cat"
        written = [json.loads(line) for line in (bucket / "results.jsonl").read_text().splitlines()]
        rows = [
            (f"{r['test']}/{r['mode']}@{r['backend'] or 'native'}", r["attempt"], r["outcome"], r["run_id"])
            for r in written
        ]
        return json.loads(process.stdout), rows, json.loads((bucket / "summary.json").read_text())

    def test_a_retried_cell_keeps_both_attempts_and_lists_only_complete_evidence(self):
        printed, rows, summary = self.accepted(
            {
                "RUN": [
                    execution(X, 100, "FAIL", "rx1"),
                    execution(X, 200, "PASS", "rx2", complete=False),
                    Y_PASSES,
                ]
            }
        )
        self.assertEqual(rows, [(X, 1, "FAIL", "rx1"), (X, 2, "PASS", "rx2"), (Y, 1, "PASS", "ry1")])
        # Import mode refuses the PASS, whose own execution's evidence is incomplete.
        self.assertEqual(
            summary["evidence_complete_executions"],
            [
                {"test": "t/x", "mode": "custom", "backend": "ptrace", "run_id": "rx1"},
                {"test": "t/y", "mode": "verify", "backend": None, "run_id": "ry1"},
            ],
        )
        self.assertEqual(summary["passed"], 2)
        self.assertEqual(printed["attempts"], {"1": 2, "2": 1})
        self.assertEqual(printed["final_outcomes"], {"PASS": 2})

    def test_attempts_follow_end_time_across_invocations(self):
        # A hybrid run lists the RE cells and the local cells as two test runs.
        _, rows, _ = self.accepted(
            {
                "RUN1": [execution(X, 200, "PASS", "rx2"), Y_PASSES],
                "RUN2": [execution(X, 100, "FAIL", "rx1")],
            }
        )
        self.assertEqual(rows, [(X, 1, "FAIL", "rx1"), (X, 2, "PASS", "rx2"), (Y, 1, "PASS", "ry1")])

    def test_an_execution_without_a_row_still_takes_its_attempt_number(self):
        """So the rerun's PASS is attempt 2, whose history import mode refuses,
        rather than a first-try PASS."""
        cases = [
            ("killed before its row", execution(X, 100, None, "rx1", complete=False), [f"{X} attempt 1"]),
            ("uploaded nothing", execution(X, 100, None, None, result=None, markers=[], uploaded=False), [f"{X} attempt 1"]),
            ("claimed host inapplicability", execution(X, 100, None, "rx1", summary=HOST_INAPPLICABLE_X), []),
        ]
        for name, first, no_row in cases:
            with self.subTest(name):
                printed, rows, _ = self.accepted({"RUN": [first, execution(X, 200, "PASS", "rx2"), Y_PASSES]})
                self.assertEqual(rows, [(X, 2, "PASS", "rx2"), (Y, 1, "PASS", "ry1")])
                self.assertEqual(printed["no_row_examples"], no_row)
        printed, rows, _ = self.accepted(
            {
                "RUN": [
                    execution(X, 100, "FAIL", "rx1"),
                    execution(X, 200, None, "rx2", complete=False),
                    execution(X, 300, "PASS", "rx3"),
                    Y_PASSES,
                ]
            }
        )
        self.assertEqual(rows, [(X, 1, "FAIL", "rx1"), (X, 3, "PASS", "rx3"), (Y, 1, "PASS", "ry1")])
        self.assertEqual(printed["no_row_examples"], [f"{X} attempt 2"])

    def test_a_no_row_execution_records_its_cause_for_import_mode(self):
        """Diagnostics only: ingest names, in Tpx's and Buck's own words, why an
        execution wrote no row (no-row-executions.jsonl beside the bucket's rows), so
        import mode can quote it when it refuses the history. The rows are the same."""
        runs = {"RUN": [execution(X, 100, None, "rx1", complete=False), execution(X, 200, "PASS", "rx2"), Y_PASSES]}
        X_ID = {"test": "t/x", "mode": "custom", "backend": "ptrace"}

        def entries(work):
            bucket = work / "out" / "portable" / "manifest_cat"
            rows = [(r["test"], r["attempt"]) for r in map(json.loads, (bucket / "results.jsonl").read_text().splitlines())]
            self.assertEqual(rows, [("t/x", 2), ("t/y", 1)])
            return [json.loads(line) for line in (bucket / "no-row-executions.jsonl").read_text().splitlines()]

        work, process = self.ingest(runs)
        self.assertEqual(process.returncode, 0, process.stderr)
        self.assertEqual(entries(work), [dict(X_ID, schema="hermit-buck-e2e-no-row-execution/v1", attempt=1,
                                              cause="ingest was given no Buck event log to read this execution's record from")])

        # The execution's own TestRun span end and its Tpx TestResult, as `buck2 log show`
        # prints them (buck2_data: TestStatus 8 is RERUN), ending within 15 s of testx's end.
        events = [
            {"Event": {"timestamp": [98, 0], "data": {"SpanEnd": {"duration_us": 29000000, "data": {"TestRun": {
                "suite": {"target_label": {"label": {"package": "hermit//ci/buck-e2e", "name": X_SLUG}}},
                "command_report": {"status": {"Success": {}}, "details": {
                    "signed_exit_code": "0", "metadata": {"wall_time_us": 26078282}}}}}}}}},
            {"Event": {"timestamp": [100, 500000000], "data": {"Instant": {"data": {"TestResult": {
                "name": f"hermit//ci/buck-e2e:{X_SLUG} - unmanaged", "status": 8,
                "details": "No details found", "duration_us": 26078282}}}}}},
            # Another target's record at the same moment is not this execution's.
            {"Event": {"timestamp": [99, 0], "data": {"Instant": {"data": {"TestResult": {
                "name": "hermit//ci/buck-e2e:t-y-custom-ptrace - unmanaged", "status": 2,
                "details": "other", "duration_us": 1}}}}}},
        ]
        log = self.root / "events.jsonl"
        log.write_text("".join(json.dumps(e) + "\n" for e in events))
        buck2 = self.root / "fake-buck2"
        buck2.write_text("#!/bin/sh\n[ \"$1 $2\" = \"log show\" ] && exec cat \"$3\"\nexit 2\n")
        buck2.chmod(0o755)
        work, process = self.ingest(runs, extra=["--buck2", str(buck2), "--event-log", str(log)])
        self.assertEqual(process.returncode, 0, process.stderr)
        self.assertEqual(entries(work)[0]["cause"],
                         "Tpx RERUN (unmanaged), details 'No details found', after 26.1 s; the command's status "
                         "Success, exit code 0, after 26.1 s of wall time")

        # An unreadable event log only makes the cause say so; the ingest still succeeds.
        work, process = self.ingest(runs, extra=["--buck2", str(buck2), "--event-log", str(self.root / "missing")])
        self.assertEqual(process.returncode, 0, process.stderr)
        cause = entries(work)[0]["cause"]
        self.assertIn(f"no TestRun of {X_SLUG} ended within 15 s of 100", cause)
        self.assertIn("event logs not read", cause)

    def test_a_final_execution_without_a_row_is_reported_as_no_row(self):
        printed, rows, _ = self.accepted(
            {"RUN": [execution(X, 100, "FAIL", "rx1"), execution(X, 200, None, "rx2", complete=False), Y_PASSES]}
        )
        self.assertEqual(rows, [(X, 1, "FAIL", "rx1"), (Y, 1, "PASS", "ry1")])
        self.assertEqual(printed["final_outcomes"], {"NO-ROW": 1, "PASS": 1})
        self.assertEqual(printed["no_row_examples"], [f"{X} attempt 2"])

    def test_a_rowless_cell_is_host_inapplicable_only_if_every_execution_says_so(self):
        printed, rows, summary = self.accepted(
            {
                "RUN": [
                    execution(X, 100, None, "rx1", summary=HOST_INAPPLICABLE_X),
                    execution(X, 200, None, "rx2", summary=HOST_INAPPLICABLE_X),
                    Y_PASSES,
                ]
            }
        )
        self.assertEqual(rows, [(Y, 1, "PASS", "ry1")])
        self.assertEqual(summary["host_inapplicable_cells"], [claim(X)])
        self.assertEqual(printed["final_outcomes"], {"HOST-INAPPLICABLE": 1, "PASS": 1})
        self.assertEqual(printed["no_row_executions"], 0)
        another_cell = {"schema": 1, "host_inapplicable_cells": [claim(Y)]}
        cases = [
            ("the first execution wrote no summary", None, HOST_INAPPLICABLE_X),
            ("the final execution wrote no summary", HOST_INAPPLICABLE_X, None),
            ("the final execution claimed another cell", HOST_INAPPLICABLE_X, another_cell),
        ]
        for name, first, final in cases:
            with self.subTest(name):
                self.refused(
                    {
                        "RUN": [
                            execution(X, 100, None, "rx1", summary=first),
                            execution(X, 200, None, "rx2", summary=final),
                            Y_PASSES,
                        ]
                    },
                    f"neither a row nor a host-inapplicable claim from every execution: ['{X}'] (1)",
                )

    def test_an_execution_that_withheld_its_cell_on_re_takes_no_attempt_number(self):
        """The worker could not run the cell; Tpx retried it on another, whose
        first try is the cell's attempt 1."""
        cases = [
            ("then passed", [execution(X, 200, "PASS", "rx2")], [(X, 1, "PASS", "rx2")]),
            (
                "then failed and passed",
                [execution(X, 200, "FAIL", "rx2"), execution(X, 300, "PASS", "rx3")],
                [(X, 1, "FAIL", "rx2"), (X, 2, "PASS", "rx3")],
            ),
        ]
        for name, later, rows_wanted in cases:
            with self.subTest(name):
                printed, rows, summary = self.accepted({"RUN": [withheld_on("re", X, 100, "rx1"), *later, Y_PASSES]})
                self.assertEqual(rows, rows_wanted + [(Y, 1, "PASS", "ry1")])
                self.assertEqual(summary.get("host_inapplicable_cells"), None)
                self.assertEqual(printed["withheld_on_re_executions"], 1)
                self.assertEqual(printed["withheld_on_re_every_execution"], 0)
                self.assertEqual(printed["no_row_examples"], [])
        # A hybrid run's local rerun is another test run, listed after the RE one.
        printed, rows, _ = self.accepted(
            {
                "RE": [withheld_on("re", X, 100, "rx1"), withheld_on("re", X, 200, "rx2"), Y_PASSES],
                "RERUN": [execution(X, 300, "PASS", "rx3")],
            }
        )
        self.assertEqual(rows, [(X, 1, "PASS", "rx3"), (Y, 1, "PASS", "ry1")])
        self.assertEqual(printed["withheld_on_re_executions"], 2)

    def test_only_a_claim_made_on_re_is_skipped(self):
        """A claim this machine made, or one whose route is not recorded, still
        takes its number, so the PASS after it is attempt 2."""
        for route in ("local", None, ""):
            with self.subTest(route=route):
                first = withheld_on(route, X, 100, "rx1")
                if route is None:
                    del first["result"]["route"]
                printed, rows, _ = self.accepted({"RUN": [first, execution(X, 200, "PASS", "rx2"), Y_PASSES]})
                self.assertEqual(rows, [(X, 2, "PASS", "rx2"), (Y, 1, "PASS", "ry1")])
                self.assertEqual(printed["withheld_on_re_executions"], 0)
        # An RE execution that wrote a row, or wrote none without a claim, is an attempt.
        killed_on_re = execution(X, 100, None, "rx1", complete=False)
        killed_on_re["result"]["route"] = "re"
        printed, rows, _ = self.accepted({"RUN": [killed_on_re, execution(X, 200, "PASS", "rx2"), Y_PASSES]})
        self.assertEqual(rows, [(X, 2, "PASS", "rx2"), (Y, 1, "PASS", "ry1")])
        self.assertEqual(printed["no_row_examples"], [f"{X} attempt 1"])
        failed_on_re = execution(X, 100, "FAIL", "rx1")
        failed_on_re["result"]["route"] = "re"
        _, rows, _ = self.accepted({"RUN": [failed_on_re, execution(X, 200, "PASS", "rx2"), Y_PASSES]})
        self.assertEqual(rows, [(X, 1, "FAIL", "rx1"), (X, 2, "PASS", "rx2"), (Y, 1, "PASS", "ry1")])
        # So is one that wrote a row and also claimed the cell: the row is kept and numbered.
        failed_and_claimed_on_re = withheld_on("re", X, 100, "rx1")
        failed_and_claimed_on_re["rows"] = [row(X, "FAIL", "rx1")]
        printed, rows, _ = self.accepted({"RUN": [failed_and_claimed_on_re, execution(X, 200, "PASS", "rx2"), Y_PASSES]})
        self.assertEqual(rows, [(X, 1, "FAIL", "rx1"), (X, 2, "PASS", "rx2"), (Y, 1, "PASS", "ry1")])
        self.assertEqual(printed["withheld_on_re_executions"], 0)

    def test_a_claim_on_re_after_a_row_leaves_the_row_and_the_claim_for_import_mode(self):
        """Import mode refuses a cell with both a row and a claim; skipping the
        claim's number must not hide either."""
        printed, rows, summary = self.accepted(
            {"RUN": [execution(X, 100, "FAIL", "rx1"), withheld_on("re", X, 200, "rx2"), Y_PASSES]}
        )
        self.assertEqual(rows, [(X, 1, "FAIL", "rx1"), (Y, 1, "PASS", "ry1")])
        self.assertEqual(summary["host_inapplicable_cells"], [claim(X)])
        self.assertEqual(printed["final_outcomes"], {"HOST-INAPPLICABLE": 1, "PASS": 1})
        self.assertEqual(printed["withheld_on_re_every_execution"], 0)

    def test_a_cell_withheld_on_re_by_every_execution_is_listed_for_a_local_rerun(self):
        printed, rows, summary = self.accepted(
            {"RUN": [withheld_on("re", X, 100, "rx1"), withheld_on("re", X, 200, "rx2"), Y_PASSES]},
            withheld_on_re=[X_SLUG],
        )
        self.assertEqual(rows, [(Y, 1, "PASS", "ry1")])
        self.assertEqual(summary["host_inapplicable_cells"], [claim(X)])
        self.assertEqual(printed["withheld_on_re_every_execution"], 1)
        # Not when one of the claims is this machine's own: import mode can confirm that one.
        printed, _, _ = self.accepted(
            {"RUN": [withheld_on("re", X, 100, "rx1"), withheld_on("local", X, 200, "rx2"), Y_PASSES]}
        )
        self.assertEqual(printed["withheld_on_re_every_execution"], 0)

    def test_every_plan_cell_and_no_other_cell(self):
        self.refused(
            {"RUN": [execution(X, 100, "PASS", "rx1")]},
            f"neither a row nor a host-inapplicable claim from every execution: ['{Y}'] (1)",
        )
        z = "t/z/custom@ptrace"
        self.refused(
            {"RUN": [execution(X, 100, "PASS", "rx1"), Y_PASSES, execution(z, 100, "PASS", "rz1")]},
            f"unexpected cells: ['{z}'] (1)",
        )

    def test_executions_of_one_cell_ending_in_the_same_second_are_refused(self):
        cases = [
            ("one invocation", {"RUN": [execution(X, 100, "FAIL", "rx1"), execution(X, 100, "PASS", "rx2"), Y_PASSES]}),
            ("two invocations", {"RUN1": [execution(X, 100, "FAIL", "rx1"), Y_PASSES], "RUN2": [execution(X, 100, "PASS", "rx2")]}),
        ]
        for name, runs in cases:
            with self.subTest(name):
                self.refused(runs, f"ended in the same second, so their order is unknown and testx (RUN.TEST.END) "
                                   f"cannot tell their artifacts apart: ['{X} at 100'] (1)")

    def test_an_execution_holds_at_most_one_row_and_only_for_its_own_cell(self):
        cases = [
            ("two rows", [row(X, "FAIL", "rx1"), row(X, "PASS", "rx1")]),
            ("another cell's row", [row(Y, "PASS", "rx1")]),
        ]
        for name, rows in cases:
            with self.subTest(name):
                self.refused(
                    {"RUN": [execution(X, 100, "PASS", "rx1", rows=rows), Y_PASSES]},
                    "a cell runs the harness once (--no-retry), which writes one row, for that cell",
                )

    def test_a_result_json_for_another_cell_is_refused(self):
        result = {"cell": Y, "run_id": "rx1", "evidence_complete": True}
        self.refused(
            {"RUN": [execution(X, 100, "PASS", "rx1", result=result), Y_PASSES]},
            f"the execution of {X} that ended at 100 has a result.json for {Y}",
        )

    def test_rows_result_json_and_marker_name_one_run(self):
        cases = [
            ("a row", {"rows": [row(X, "PASS", "rx-other")]}),
            ("result.json", {"result": {"cell": X, "run_id": "rx-other", "evidence_complete": True}}),
            ("the marker", {"markers": ["rx-other"]}),
            (
                "no run id at all",
                {
                    "rows": [row(X, "PASS", None)],
                    "result": {"cell": X, "run_id": None, "evidence_complete": True},
                    "markers": [],
                },
            ),
        ]
        for name, changes in cases:
            with self.subTest(name):
                self.refused(
                    {"RUN": [execution(X, 100, "PASS", "rx1", **changes), Y_PASSES]},
                    "its rows, its result.json and its run_id. marker must all name the same one",
                )
        self.refused(
            {"RUN": [execution(X, 100, "PASS", "rx1", markers=["rx1", "rx2"]), Y_PASSES]},
            f"the execution of {X} that ended at 100 has 2 run id markers",
        )

    def test_two_executions_may_not_share_a_run_id(self):
        self.refused(
            {"RUN": [execution(X, 100, "FAIL", "r-same"), execution(X, 200, "PASS", "r-same"), Y_PASSES]},
            f"run r-same names two executions: the execution of {X} that ended at 100 "
            f"and the execution of {X} that ended at 200",
        )

    # The local copies below disagree with the uploads where the test must tell
    # which of the two ingest.py read; a real copy is byte-identical.

    def test_a_local_copy_is_read_only_for_the_execution_its_marker_names(self):
        first = execution(X, 100, "FAIL", "rx1")
        # A newer directory left by another run is neither execution.
        printed, rows, _ = self.accepted(
            {"RUN": [first, execution(X, 200, "FAIL", "rx2"), Y_PASSES]},
            local=[execution(X, 300, "PASS", "rx-foreign")],
        )
        self.assertEqual(rows, [(X, 1, "FAIL", "rx1"), (X, 2, "FAIL", "rx2"), (Y, 1, "PASS", "ry1")])
        self.assertEqual(printed["sources"], {"testx": 3})
        # Buck keeps one directory per target, which may hold attempt 1.
        printed, rows, _ = self.accepted(
            {"RUN": [first, execution(X, 200, "PASS", "rx2"), Y_PASSES]},
            local=[first],
        )
        self.assertEqual(rows, [(X, 1, "FAIL", "rx1"), (X, 2, "PASS", "rx2"), (Y, 1, "PASS", "ry1")])
        self.assertEqual(printed["sources"], {"local": 1, "testx": 2})

    def test_a_marker_that_does_not_name_one_local_directory_is_fetched(self):
        cases = [
            ("two directories hold it", [execution(X, 200, "PASS", "rx2"), execution(X, 200, "PASS", "rx2")]),
            ("its directory holds two markers", [execution(X, 200, "PASS", "rx2", markers=["rx2", "rx1"])]),
        ]
        for name, local in cases:
            with self.subTest(name):
                printed, rows, _ = self.accepted(
                    {"RUN": [execution(X, 100, "FAIL", "rx1"), execution(X, 200, "FAIL", "rx2"), Y_PASSES]},
                    local=local,
                )
                self.assertEqual(rows, [(X, 1, "FAIL", "rx1"), (X, 2, "FAIL", "rx2"), (Y, 1, "PASS", "ry1")])
                self.assertEqual(printed["sources"], {"testx": 3})

    def read_kept(self, root):
        index = {e["run_id"]: e for e in map(json.loads, (root / "index.jsonl").read_text().splitlines())}
        files = {str(p.relative_to(root)): p.read_bytes() for p in root.rglob("*") if p.is_file()}
        files.pop("index.jsonl")
        return index, files

    def test_a_diverged_cells_run_logs_are_kept_and_a_passing_cells_are_not(self):
        """The 2026-10-05 compat/zip-unzip red: a verify cell diverged on an RE worker and
        its logs were never brought back. Both logs come back, decompressed, whether the
        execution is fetched with testx (an RE cell) or read from buck-out (a local one)."""
        diverged = with_logs(execution(Y, 150, "FAIL", "ry1"), DIVERGED_LOGS)
        # A matched verify keeps run 1's log too; a cell that passed gets nothing kept.
        passing = with_logs(execution(X, 100, "PASS", "rx1"), {"cell__verify-logs__verify-1__run1_log_detlog": RUN1_LOG})
        for name, local in (("fetched with testx", ()), ("read from buck-out", [diverged, passing])):
            with self.subTest(name):
                work, process = self.ingest({"RUN": [passing, diverged]}, local,
                                            ["--failed-verify-logs", str(self.root / name / "kept")])
                self.assertEqual(process.returncode, 0, process.stderr)
                printed = json.loads(process.stdout)
                index, files = self.read_kept(self.root / name / "kept")
                self.assertEqual(files, {"ry1/verify-logs/verify-1/run1_log_detlog": RUN1_LOG,
                                         "ry1/verify-logs/verify-1/run2_log_detlog": RUN2_LOG})
                self.assertEqual(list(index), ["ry1"])
                self.assertEqual(index["ry1"]["cell"], Y)
                self.assertEqual(index["ry1"]["outcome"], "FAIL")
                self.assertIsNone(index["ry1"]["reason"])
                self.assertEqual(
                    [(one["artifact"], one["log"], one["bytes"], one["truncated"], one["reason"]) for one in index["ry1"]["logs"]],
                    [("cell__verify-logs__verify-1__run1_log_detlog", "ry1/verify-logs/verify-1/run1_log_detlog",
                      len(RUN1_LOG), False, None),
                     ("cell__verify-logs__verify-1__run2_log_detlog.zst", "ry1/verify-logs/verify-1/run2_log_detlog",
                      len(RUN2_LOG), False, None)])
                self.assertEqual((printed["failed_executions"], printed["failed_verify_logs_kept"],
                                  printed["failed_verify_logs_not_kept"]), (1, 2, 0))

    def test_a_refused_executions_stderr_and_failure_class_are_kept(self):
        """https://github.com/rrnewton/hermit/issues/3922: a Buck verify row that Hermit refused
        with exit 125 kept only a 0-byte verify log, so nothing said why. The tail of the
        attempt's stderr and the class of its HERMIT_INTERNAL_FAILURE line are kept with it,
        a long stderr is cut to its last 64 KiB, and an execution with no row says why it
        kept none."""
        refusal = ("guest stderr before the refusal\n"
                   "HERMIT_INTERNAL_FAILURE class=backend-unavailable backend=kvm\n"
                   "Error: the KVM backend is unavailable: /dev/kvm is busy\n")
        refused_row = dict(row(Y, "ERROR", "ry1"), attempts=[{"status": 125, "stderr": refusal}])
        refused = with_logs(execution(Y, 150, "ERROR", "ry1", rows=[refused_row]),
                            {"cell__verify-logs__verify-1__run1_log_detlog": b""})
        noise = "x" * 99 + "\n"
        long_stderr = noise * 700 + "HERMIT_INTERNAL_FAILURE class=cli-error\nerror: bad flag\n"
        long_row = dict(row(X, "FAIL", "rx1"), attempts=[{"status": 125, "stderr": long_stderr}])
        long = execution(X, 100, "FAIL", "rx1", rows=[long_row])
        # An execution with neither a row nor a log has nothing kept at all; one that left a
        # log but no row keeps the log and says why it has no stderr.
        died = with_logs(execution(X, 200, None, "rx2", complete=False),
                         {"cell__verify-logs__verify-1__run1_log_detlog": RUN1_LOG})
        work, process = self.ingest({"RUN": [long, refused, died, execution(X, 300, "PASS", "rx3")]},
                                    extra=["--failed-verify-logs", str(self.root / "kept")])
        self.assertEqual(process.returncode, 0, process.stderr)
        index, files = self.read_kept(self.root / "kept")
        self.assertEqual(index["ry1"]["internal_failure_class"], "backend-unavailable")
        self.assertEqual(index["ry1"]["stderr"], {"file": "ry1/hermit-stderr.tail", "bytes": len(refusal),
                                                  "truncated": False, "reason": None})
        self.assertEqual(files["ry1/hermit-stderr.tail"], refusal.encode())
        self.assertEqual(files["ry1/verify-logs/verify-1/run1_log_detlog"], b"")
        self.assertEqual(index["rx1"]["internal_failure_class"], "cli-error")
        self.assertEqual(index["rx1"]["stderr"], {"file": "rx1/hermit-stderr.tail", "bytes": len(long_stderr),
                                                  "truncated": True, "reason": None})
        self.assertEqual(files["rx1/hermit-stderr.tail"], long_stderr.encode()[-(64 << 10):])
        self.assertTrue(files["rx1/hermit-stderr.tail"].endswith(b"HERMIT_INTERNAL_FAILURE class=cli-error\nerror: bad flag\n"))
        self.assertIsNone(index["rx2"]["internal_failure_class"])
        self.assertEqual(index["rx2"]["stderr"], {"file": None, "bytes": None, "truncated": None,
                                                  "reason": "the execution wrote no row"})
        self.assertNotIn("rx3", index)
        self.assertEqual({entry["schema"] for entry in index.values()}, {2})

    def test_an_execution_that_wrote_no_row_keeps_its_logs_too(self):
        died = with_logs(execution(X, 100, None, "rx1", complete=False), DIVERGED_LOGS)
        work, process = self.ingest({"RUN": [died, execution(X, 200, "PASS", "rx2"), Y_PASSES]},
                                    extra=["--failed-verify-logs", str(self.root / "kept")])
        self.assertEqual(process.returncode, 0, process.stderr)
        index, files = self.read_kept(self.root / "kept")
        self.assertEqual(sorted(files), ["rx1/verify-logs/verify-1/run1_log_detlog",
                                         "rx1/verify-logs/verify-1/run2_log_detlog"])
        self.assertEqual(list(index), ["rx1"])
        self.assertIsNone(index["rx1"]["outcome"])

    def test_a_log_that_cannot_be_checked_is_named_and_never_fails_the_ingest(self):
        run2 = "cell__verify-logs__verify-1__run2_log_detlog.zst"
        cases = [
            ("a log without its recorded sha256", with_logs(execution(Y, 150, "FAIL", "ry1"), DIVERGED_LOGS, {run2: "0" * 64}),
             ["ry1/verify-logs/verify-1/run1_log_detlog"], None, "it does not have the sha256 its cell recorded"),
            ("no artifact_sha256 at all", dict(with_logs(execution(Y, 150, "FAIL", "ry1"), DIVERGED_LOGS),
                                              result={"cell": Y, "run_id": "ry1", "evidence_complete": True}),
             [], "records no artifact_sha256", None),
            ("no log among the artifacts", execution(Y, 150, "ERROR", "ry1"), [], "hold no run1_log_* or run2_log_* log", None),
        ]
        for name, failing, kept_files, reason, log_reason in cases:
            with self.subTest(name):
                target = self.root / name.replace(" ", "-") / "kept"
                work, process = self.ingest({"RUN": [execution(X, 100, "PASS", "rx1"), failing]},
                                            extra=["--failed-verify-logs", str(target)])
                self.assertEqual(process.returncode, 0, process.stderr)
                index, files = self.read_kept(target)
                self.assertEqual(sorted(files), kept_files)
                if reason:
                    self.assertIn(reason, index["ry1"]["reason"])
                if log_reason:
                    self.assertEqual([one["reason"] for one in index["ry1"]["logs"]], [None, log_reason])

    def test_a_log_is_cut_at_the_bound(self):
        diverged = with_logs(execution(Y, 150, "FAIL", "ry1"), DIVERGED_LOGS)
        work, process = self.ingest({"RUN": [execution(X, 100, "PASS", "rx1"), diverged]},
                                    extra=["--failed-verify-logs", str(self.root / "kept"), "--failed-log-max-bytes", "100"])
        self.assertEqual(process.returncode, 0, process.stderr)
        index, files = self.read_kept(self.root / "kept")
        self.assertEqual(files, {"ry1/verify-logs/verify-1/run1_log_detlog": RUN1_LOG,
                                 "ry1/verify-logs/verify-1/run2_log_detlog": RUN2_LOG[:100]})
        self.assertEqual([(one["bytes"], one["truncated"]) for one in index["ry1"]["logs"]],
                         [(len(RUN1_LOG), False), (100, True)])

    def test_a_directory_that_is_not_a_previous_ingests_is_never_replaced(self):
        target = self.root / "occupied"
        target.mkdir()
        (target / "someone-elses").write_text("kept\n")
        work, process = self.ingest({"RUN": [execution(X, 100, "PASS", "rx1"), Y_PASSES]},
                                    extra=["--failed-verify-logs", str(target)])
        self.assertEqual(process.returncode, 1, process.stdout + process.stderr)
        self.assertIn("is not absent, empty, or a previous ingest's", process.stderr)
        self.assertEqual((target / "someone-elses").read_text(), "kept\n")
        # A previous ingest's directory is replaced whole.
        diverged = with_logs(execution(Y, 150, "FAIL", "ry1"), DIVERGED_LOGS)
        again = self.root / "again"
        for runs in ({"RUN": [execution(X, 100, "PASS", "rx1"), diverged]}, {"RUN": [execution(X, 100, "PASS", "rx1"), Y_PASSES]}):
            work, process = self.ingest(runs, extra=["--failed-verify-logs", str(again)])
            self.assertEqual(process.returncode, 0, process.stderr)
        self.assertEqual(self.read_kept(again), ({}, {}))

    def test_a_pass_row_with_incomplete_evidence_keeps_its_logs(self):
        # cell.sh reports such an execution to Tpx as an ERROR, whatever its row says.
        incomplete = with_logs(execution(Y, 150, "PASS", "ry1", complete=False), DIVERGED_LOGS)
        work, process = self.ingest({"RUN": [execution(X, 100, "PASS", "rx1"), incomplete]},
                                    extra=["--failed-verify-logs", str(self.root / "kept")])
        self.assertEqual(process.returncode, 0, process.stderr)
        index, files = self.read_kept(self.root / "kept")
        self.assertEqual(sorted(files), ["ry1/verify-logs/verify-1/run1_log_detlog",
                                         "ry1/verify-logs/verify-1/run2_log_detlog"])
        self.assertEqual(index["ry1"]["outcome"], "PASS")

    def test_logs_are_kept_from_a_run_refused_for_a_cell_with_no_row(self):
        # Every attempt of X died before writing a row, so ingest refuses the run; the logs of
        # those attempts are what explains it, and they must survive the refusal.
        died = [with_logs(execution(X, end, None, f"rx{end}", complete=False), DIVERGED_LOGS) for end in (100, 200)]
        work, process = self.ingest({"RUN": [*died, Y_PASSES]}, extra=["--failed-verify-logs", str(self.root / "kept")])
        self.assertEqual(process.returncode, 1, process.stdout + process.stderr)
        self.assertIn(f"neither a row nor a host-inapplicable claim from every execution: ['{X}'] (1)", process.stderr)
        index, files = self.read_kept(self.root / "kept")
        self.assertEqual(sorted(index), ["rx100", "rx200"])
        self.assertEqual(len(files), 4)

    def test_a_bad_failed_log_download_never_touches_the_parity_logs(self):
        # Y diverged; its row records the verify-log directory, so the parity post-pass
        # restores its run 1 log too. The download of the failed logs is cut short.
        parity_row = dict(row(Y, "FAIL", "ry1"), argv=["--verify-log-dir", "/cell/verify-logs/verify-1"],
                          artifact_dir="/cell")
        diverged = with_logs(execution(Y, 150, "FAIL", "ry1", rows=[parity_row]), DIVERGED_LOGS)
        work, process = self.ingest({"RUN": [execution(X, 100, "PASS", "rx1"), diverged]},
                                    extra=["--failed-verify-logs", str(self.root / "kept")],
                                    FAKE_TESTX_GARBLE="run2_log_")
        self.assertEqual(process.returncode, 0, process.stderr)
        parity = [json.loads(line) for line in (work / "out" / "retained-verify-logs" / "index.jsonl").read_text().splitlines()]
        self.assertEqual([(entry["restored"], entry["reason"]) for entry in parity],
                         [("retained-verify-logs/ry1/verify-logs/verify-1", None)])
        self.assertEqual((work / "out" / "retained-verify-logs" / "ry1" / "verify-logs" / "verify-1" / "run1_log_detlog").read_bytes(),
                         RUN1_LOG)
        index, files = self.read_kept(self.root / "kept")
        self.assertEqual(files, {})
        self.assertEqual([one["reason"] for one in index["ry1"]["logs"]],
                         ["it does not have the sha256 its cell recorded"] * 2)

    def test_a_directory_overlapping_the_ingests_own_output_is_refused(self):
        diverged = with_logs(execution(Y, 150, "FAIL", "ry1"), DIVERGED_LOGS)
        # (case, --out, --failed-verify-logs), below a fresh directory: none exists yet.
        for name, out, target in (("--out itself", "out", "out"), ("the parity logs", "out", "out/retained-verify-logs"),
                                  ("a parent of --out", "nest/out", "nest"), ("inside --work", "out", "fetched/kept")):
            with self.subTest(name):
                work = Path(tempfile.mkdtemp(dir=self.root))
                fake = work / "fake"
                write_test_runs(fake, {"RUN": [execution(X, 100, "PASS", "rx1"), diverged]})
                process = subprocess.run(
                    [sys.executable, str(INGEST), "--plan", str(self.root / "plan.json"), "--out", str(work / out),
                     "--work", str(work / "fetched"), "--failed-verify-logs", str(work / target), "RUN"],
                    capture_output=True, text=True, env=dict(os.environ, TESTX=str(self.testx), FAKE_TESTX_DIR=str(fake)))
                self.assertEqual(process.returncode, 1, process.stdout + process.stderr)
                self.assertIn("overlaps", process.stderr)
                self.assertFalse((work / out).exists(), "ingest wrote before refusing")

    def test_a_directory_that_cannot_be_written_never_fails_the_ingest(self):
        diverged = with_logs(execution(Y, 150, "FAIL", "ry1"), DIVERGED_LOGS)
        locked = self.root / "locked"
        locked.mkdir()
        locked.chmod(0o555)
        self.addCleanup(locked.chmod, 0o755)
        work, process = self.ingest({"RUN": [execution(X, 100, "PASS", "rx1"), diverged]},
                                    extra=["--failed-verify-logs", str(locked / "kept")])
        self.assertEqual(process.returncode, 0, process.stderr)
        self.assertIn("the failed cells' verify logs were not kept: cannot create", process.stderr)
        self.assertIn("cannot create", json.loads(process.stdout)["failed_verify_logs_error"])
        self.assertTrue((work / "out" / "portable" / "manifest_cat" / "results.jsonl").is_file())

    def test_a_default_work_directory_inside_it_is_refused(self):
        # Without --work, ingest downloads below TMPDIR; replacing a directory that holds
        # TMPDIR would delete those downloads.
        diverged = with_logs(execution(Y, 150, "FAIL", "ry1"), DIVERGED_LOGS)
        work = Path(tempfile.mkdtemp(dir=self.root))
        fake = work / "fake"
        write_test_runs(fake, {"RUN": [execution(X, 100, "PASS", "rx1"), diverged]})
        target = work / "kept"
        (target / "tmp").mkdir(parents=True)
        (target / "index.jsonl").write_text("")
        process = subprocess.run(
            [sys.executable, str(INGEST), "--plan", str(self.root / "plan.json"), "--out", str(work / "out"),
             "--failed-verify-logs", str(target), "RUN"],
            capture_output=True, text=True,
            env=dict(os.environ, TESTX=str(self.testx), FAKE_TESTX_DIR=str(fake), TMPDIR=str(target / "tmp")))
        self.assertEqual(process.returncode, 1, process.stdout + process.stderr)
        self.assertIn("overlaps --work", process.stderr)
        self.assertFalse((work / "out").exists(), "ingest wrote before refusing")

    def test_an_earlier_directory_that_cannot_be_inspected_never_fails_the_ingest(self):
        diverged = with_logs(execution(Y, 150, "FAIL", "ry1"), DIVERGED_LOGS)
        earlier = self.root / "earlier"
        earlier.mkdir()
        (earlier / "index.jsonl").write_text("from an earlier ingest\n")
        earlier.chmod(0o000)
        self.addCleanup(earlier.chmod, 0o755)
        work, process = self.ingest({"RUN": [execution(X, 100, "PASS", "rx1"), diverged]},
                                    extra=["--failed-verify-logs", str(earlier)])
        self.assertEqual(process.returncode, 0, process.stderr)
        self.assertIn("the failed cells' verify logs will not be kept: cannot inspect", process.stderr)
        self.assertIn("cannot inspect", json.loads(process.stdout)["failed_verify_logs_error"])
        self.assertTrue((work / "out" / "portable" / "manifest_cat" / "results.jsonl").is_file())
        earlier.chmod(0o755)
        self.assertEqual((earlier / "index.jsonl").read_text(), "from an earlier ingest\n")

    def test_a_previous_directory_whose_index_cannot_be_checked_is_not_called_foreign(self):
        # Mode 0400: its names can be listed, but not one of its files can be looked at.
        diverged = with_logs(execution(Y, 150, "FAIL", "ry1"), DIVERGED_LOGS)
        earlier = self.root / "earlier"
        earlier.mkdir()
        (earlier / "index.jsonl").write_text("from an earlier ingest\n")
        earlier.chmod(0o400)
        self.addCleanup(earlier.chmod, 0o755)
        work, process = self.ingest({"RUN": [execution(X, 100, "PASS", "rx1"), diverged]},
                                    extra=["--failed-verify-logs", str(earlier)])
        self.assertEqual(process.returncode, 0, process.stderr)
        self.assertNotIn("refusing to replace it", process.stderr)
        self.assertIn("cannot inspect", json.loads(process.stdout)["failed_verify_logs_error"])
        self.assertTrue((work / "out" / "portable" / "manifest_cat" / "results.jsonl").is_file())


if __name__ == "__main__":
    unittest.main()
