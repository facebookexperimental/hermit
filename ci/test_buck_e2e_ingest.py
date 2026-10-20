#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

"""Scenario tests for ci/buck-e2e/ingest.py, run against a stand-in testx.

Each test lays out a Buck test run the way testx lists it (every cell execution
with its uploaded artifacts, and optionally Buck's local copies under buck-out),
runs ingest.py on it, and checks either the refusal or the rows, attempt numbers
and evidence list that ingest.py writes for test-harness's import mode.
"""

from __future__ import annotations

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
        {
            "test": "t/x",
            "mode": "custom",
            "backend": "ptrace",
            "lane": "portable",
            "category": "cat",
        },
        {
            "test": "t/y",
            "mode": "verify",
            "backend": "native",
            "lane": "portable",
            "category": "cat",
        },
    ]
}
# testx, for the two things ingest.py asks of it: `--as-json results list RUN`
# and `artifacts get RUN.TEST.END --output-dir DIR --artifact-names NAME...`.
FAKE_TESTX = r"""#!/usr/bin/env python3
import os, shutil, sys
root, args = os.environ["FAKE_TESTX_DIR"], sys.argv[1:]
if args[:3] == ["--as-json", "results", "list"]:
    sys.stdout.write(open(os.path.join(root, "list-" + args[3] + ".json")).read())
elif args[:2] == ["artifacts", "get"]:
    out = args[args.index("--output-dir") + 1]
    os.makedirs(out, exist_ok=True)
    for i, arg in enumerate(args):
        if arg == "--artifact-names":
            shutil.copy(os.path.join(root, "art", args[2], args[i + 1]), out)
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
    return sorted(path.name for path in directory.iterdir())


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
            names = (
                write_artifacts(fake / "art" / address, uploaded)
                if uploaded["uploaded"]
                else []
            )
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
                    "test_details": {
                        "name": f"{name} - unmanaged",
                        "id": test_id + 1000,
                    },
                    "end_time": uploaded["end"],
                    "artifacts": [],
                }
            )
        listing = {"results": {"test_results": listed}}
        (fake / f"list-{run_id}.json").write_text(json.dumps(listing))


Y_PASSES = execution(Y, 150, "PASS", "ry1")
HOST_INAPPLICABLE_X = {"schema": 1, "host_inapplicable_cells": [claim(X)]}


class IngestTest(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory(prefix="test-buck-e2e-ingest-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.testx = self.root / "testx"
        self.testx.write_text(FAKE_TESTX)
        self.testx.chmod(0o755)
        (self.root / "plan.json").write_text(json.dumps(PLAN))

    def ingest(self, runs, local=()):
        """Run ingest.py on RUNS ({test run id: [execution, ...]}), with each
        execution in LOCAL copied into its own buck-out artifacts directory."""
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
        ]
        if local:
            buck_out = work / "buck-out-test-execution"
            for index, uploaded in enumerate(local):
                target = buck_out / "root" / f"target{index}" / "config" / "default"
                write_artifacts(target / "artifacts_directory", uploaded)
            command += ["--local-artifacts", str(buck_out)]
        environment = dict(os.environ, TESTX=str(self.testx), FAKE_TESTX_DIR=str(fake))
        process = subprocess.run(
            command + list(runs), capture_output=True, text=True, env=environment
        )
        return work, process

    def refused(self, runs, reason, local=()):
        _, process = self.ingest(runs, local)
        self.assertEqual(process.returncode, 1, process.stdout + process.stderr)
        self.assertIn(reason, process.stderr)

    def accepted(self, runs, local=()):
        """Return ingest.py's printed summary, the bucket's rows as (cell,
        attempt, outcome, run id), and the bucket's summary.json."""
        work, process = self.ingest(runs, local)
        self.assertEqual(process.returncode, 0, process.stderr)
        bucket = work / "out" / "portable" / "manifest_cat"
        written = [
            json.loads(line)
            for line in (bucket / "results.jsonl").read_text().splitlines()
        ]
        rows = [
            (
                f"{r['test']}/{r['mode']}@{r['backend'] or 'native'}",
                r["attempt"],
                r["outcome"],
                r["run_id"],
            )
            for r in written
        ]
        return (
            json.loads(process.stdout),
            rows,
            json.loads((bucket / "summary.json").read_text()),
        )

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
        self.assertEqual(
            rows, [(X, 1, "FAIL", "rx1"), (X, 2, "PASS", "rx2"), (Y, 1, "PASS", "ry1")]
        )
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
        self.assertEqual(
            rows, [(X, 1, "FAIL", "rx1"), (X, 2, "PASS", "rx2"), (Y, 1, "PASS", "ry1")]
        )

    def test_an_execution_without_a_row_still_takes_its_attempt_number(self):
        """So the rerun's PASS is attempt 2, whose history import mode refuses,
        rather than a first-try PASS."""
        cases = [
            (
                "killed before its row",
                execution(X, 100, None, "rx1", complete=False),
                [f"{X} attempt 1"],
            ),
            (
                "uploaded nothing",
                execution(X, 100, None, None, result=None, markers=[], uploaded=False),
                [f"{X} attempt 1"],
            ),
            (
                "claimed host inapplicability",
                execution(X, 100, None, "rx1", summary=HOST_INAPPLICABLE_X),
                [],
            ),
        ]
        for name, first, no_row in cases:
            with self.subTest(name):
                printed, rows, _ = self.accepted(
                    {"RUN": [first, execution(X, 200, "PASS", "rx2"), Y_PASSES]}
                )
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
        self.assertEqual(
            rows, [(X, 1, "FAIL", "rx1"), (X, 3, "PASS", "rx3"), (Y, 1, "PASS", "ry1")]
        )
        self.assertEqual(printed["no_row_examples"], [f"{X} attempt 2"])

    def test_a_final_execution_without_a_row_is_reported_as_no_row(self):
        printed, rows, _ = self.accepted(
            {
                "RUN": [
                    execution(X, 100, "FAIL", "rx1"),
                    execution(X, 200, None, "rx2", complete=False),
                    Y_PASSES,
                ]
            }
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
            (
                "the final execution claimed another cell",
                HOST_INAPPLICABLE_X,
                another_cell,
            ),
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

    def test_every_plan_cell_and_no_other_cell(self):
        self.refused(
            {"RUN": [execution(X, 100, "PASS", "rx1")]},
            f"neither a row nor a host-inapplicable claim from every execution: ['{Y}'] (1)",
        )
        z = "t/z/custom@ptrace"
        self.refused(
            {
                "RUN": [
                    execution(X, 100, "PASS", "rx1"),
                    Y_PASSES,
                    execution(z, 100, "PASS", "rz1"),
                ]
            },
            f"unexpected cells: ['{z}'] (1)",
        )

    def test_executions_of_one_cell_ending_in_the_same_second_are_refused(self):
        cases = [
            (
                "one invocation",
                {
                    "RUN": [
                        execution(X, 100, "FAIL", "rx1"),
                        execution(X, 100, "PASS", "rx2"),
                        Y_PASSES,
                    ]
                },
            ),
            (
                "two invocations",
                {
                    "RUN1": [execution(X, 100, "FAIL", "rx1"), Y_PASSES],
                    "RUN2": [execution(X, 100, "PASS", "rx2")],
                },
            ),
        ]
        for name, runs in cases:
            with self.subTest(name):
                self.refused(
                    runs,
                    f"ended in the same second, so their order is unknown and testx (RUN.TEST.END) "
                    f"cannot tell their artifacts apart: ['{X} at 100'] (1)",
                )

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
            (
                "result.json",
                {
                    "result": {
                        "cell": X,
                        "run_id": "rx-other",
                        "evidence_complete": True,
                    }
                },
            ),
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
            {
                "RUN": [
                    execution(X, 100, "PASS", "rx1", markers=["rx1", "rx2"]),
                    Y_PASSES,
                ]
            },
            f"the execution of {X} that ended at 100 has 2 run id markers",
        )

    def test_two_executions_may_not_share_a_run_id(self):
        self.refused(
            {
                "RUN": [
                    execution(X, 100, "FAIL", "r-same"),
                    execution(X, 200, "PASS", "r-same"),
                    Y_PASSES,
                ]
            },
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
        self.assertEqual(
            rows, [(X, 1, "FAIL", "rx1"), (X, 2, "FAIL", "rx2"), (Y, 1, "PASS", "ry1")]
        )
        self.assertEqual(printed["sources"], {"testx": 3})
        # Buck keeps one directory per target, which may hold attempt 1.
        printed, rows, _ = self.accepted(
            {"RUN": [first, execution(X, 200, "PASS", "rx2"), Y_PASSES]},
            local=[first],
        )
        self.assertEqual(
            rows, [(X, 1, "FAIL", "rx1"), (X, 2, "PASS", "rx2"), (Y, 1, "PASS", "ry1")]
        )
        self.assertEqual(printed["sources"], {"local": 1, "testx": 2})

    def test_a_marker_that_does_not_name_one_local_directory_is_fetched(self):
        cases = [
            (
                "two directories hold it",
                [execution(X, 200, "PASS", "rx2"), execution(X, 200, "PASS", "rx2")],
            ),
            (
                "its directory holds two markers",
                [execution(X, 200, "PASS", "rx2", markers=["rx2", "rx1"])],
            ),
        ]
        for name, local in cases:
            with self.subTest(name):
                printed, rows, _ = self.accepted(
                    {
                        "RUN": [
                            execution(X, 100, "FAIL", "rx1"),
                            execution(X, 200, "FAIL", "rx2"),
                            Y_PASSES,
                        ]
                    },
                    local=local,
                )
                self.assertEqual(
                    rows,
                    [
                        (X, 1, "FAIL", "rx1"),
                        (X, 2, "FAIL", "rx2"),
                        (Y, 1, "PASS", "ry1"),
                    ],
                )
                self.assertEqual(printed["sources"], {"testx": 3})


if __name__ == "__main__":
    unittest.main()
