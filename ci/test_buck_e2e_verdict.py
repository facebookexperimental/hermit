#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

"""Scenario tests for ci/buck-e2e/verdict.py and the verdict step of ci/buck-e2e/run.

verdict.py hands each (lane, category) bucket of the plan to test-harness's import mode
and turns what the harness reports into one exit status. VerdictTest runs it against a
stand-in harness that records how it was called and reports what the case gives it.
RunTest runs ci/buck-e2e/run end to end in a scratch checkout of its scripts, with
stand-ins for buck2, testx and the staged harness, and checks that a FAIL, an ERROR or
results ingest.py refuses make the run exit non-zero.
"""

from __future__ import annotations

import importlib.util
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

import test_buck_e2e_ingest as ingest_test


CI = Path(__file__).resolve().parent
BUCK_E2E = CI / "buck-e2e"
VERDICT = BUCK_E2E / "verdict.py"
A, B = "c/a/custom/ptrace", "c/b/verify/kvm"
D, E = "compat/d/verify/ptrace", "compat/e/verify/ptrace"
P = "s/p/custom/ptrace"
CELLS = {
    A: ("portable", "c-programs", "required"),
    B: ("portable", "c-programs", "required"),
    D: ("portable", "compat", "required"),
    E: ("portable", "compat", "diagnostic"),
    P: ("privileged", "system-utils", "required"),
}
BUCKETS = ["portable/c-programs", "portable/compat", "privileged/system-utils"]
# test-harness, for the one thing verdict.py asks of it: `run ... --lane LANE
# --category CATEGORY --results FILE --junit FILE` in import mode. It appends how it
# was called to FAKE_HARNESS_CALLS and reports each bucket as FAKE_HARNESS_REPORTS says
# ({"LANE/CATEGORY": {"rc", "signal", "summary", "junit", "log"}}), or, for a bucket that has no
# report there, as the imported rows say: each cell's last row is its outcome. With
# FAKE_HARNESS_TOUCH, it first appends a line to that file.
FAKE_HARNESS = r"""#!/usr/bin/env python3
import json, os, sys
from xml.sax.saxutils import escape
args = sys.argv[1:]
flag = lambda name: args[args.index(name) + 1]
lane, category, results, junit = flag("--lane"), flag("--category"), flag("--results"), flag("--junit")
seen = {k: os.environ.get(k) for k in ("E2E_IMPORT_RESULTS", "E2E_RESULT_ROOT", "DAGRUN_TEST_COUNTS_PATH")}
with open(os.environ["FAKE_HARNESS_CALLS"], "a") as calls:
    calls.write(json.dumps({"argv": args, "env": seen}) + "\n")
if os.environ.get("FAKE_HARNESS_TOUCH"):
    with open(os.environ["FAKE_HARNESS_TOUCH"], "a") as touched:
        touched.write("changed while judging\n")
report = json.loads(os.environ.get("FAKE_HARNESS_REPORTS") or "{}").get(lane + "/" + category)
if report is None:
    bucket = os.path.join(os.environ["E2E_IMPORT_RESULTS"], lane, "manifest_" + category.replace("-", "_"))
    final = {}
    for line in open(os.path.join(bucket, "results.jsonl")):
        row = json.loads(line)
        final["{}/{}/{}".format(row["test"], row["mode"], row["backend"] or "none")] = row["outcome"]
    count = lambda outcome: sum(o == outcome for o in final.values())
    report = {
        "rc": int(count("FAIL") + count("ERROR") > 0),
        "summary": {"cells": len(final), "passed": count("PASS"), "failed": count("FAIL"),
                    "errors": count("ERROR"), "diagnostic_failures": 0, "diagnostic_failure_cells": [],
                    "host_inapplicable": 0, "imported": {"missing_cells": 0, "dropped_retries": 0}},
        "junit": [[name, {"FAIL": "failure", "ERROR": "error"}.get(o), "the row says " + o]
                  for name, o in sorted(final.items())],
    }
os.makedirs(os.path.dirname(results), exist_ok=True)
summary = report.get("summary")
if summary is not None:
    with open(os.path.join(os.path.dirname(results), "summary.json"), "w") as f:
        f.write(summary if isinstance(summary, str) else json.dumps(summary))
if report.get("junit") is not None:
    with open(junit, "w") as f:
        f.write('<?xml version="1.0" encoding="UTF-8"?>\n<testsuite name="hermit-e2e">\n')
        for name, tag, text in report["junit"]:
            body = "<{0}>{1}</{0}>".format(tag, escape(text)) if tag else ""
            f.write('  <testcase classname="{}" name="{}">{}</testcase>\n'.format(category, escape(name), body))
        f.write("</testsuite>\n")
sys.stdout.write(report.get("log", "fake harness: judged {}/{}\n".format(lane, category)))
sys.stdout.flush()
if report.get("signal"):
    os.kill(os.getpid(), report["signal"])
sys.exit(report.get("rc", 0))
"""
# buck2, for the one thing ci/buck-e2e/run asks of it: `test ... --write-test-id FILE
# //ci/buck-e2e:NAME -- ...`. It writes NAME as the test run id and exits FAKE_BUCK2_RC.
# With FAKE_BUCK2_TOUCH it appends a line to that file; with FAKE_BUCK2_COMMIT it commits
# nothing in its working directory, moving HEAD.
FAKE_BUCK2 = r"""#!/usr/bin/env python3
import os, subprocess, sys
args = sys.argv[1:]
target = next(arg for arg in args if arg.startswith("//ci/buck-e2e:"))
with open(args[args.index("--write-test-id") + 1], "w") as f:
    f.write(target.split(":", 1)[1] + "\n")
if os.environ.get("FAKE_BUCK2_TOUCH"):
    with open(os.environ["FAKE_BUCK2_TOUCH"], "a") as touched:
        touched.write("changed during the run\n")
if os.environ.get("FAKE_BUCK2_COMMIT"):
    subprocess.run(["git", "-c", "user.name=test", "-c", "user.email=test@example.invalid",
                    "-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null",
                    "commit", "-q", "--allow-empty", "-m", "moved"], check=True)
sys.exit(int(os.environ.get("FAKE_BUCK2_RC", "0")))
"""


def plan_cell(name, lane, category, classification):
    test, mode, backend = name.rsplit("/", 2)
    return {
        "test": test,
        "mode": mode,
        "backend": backend,
        "lane": lane,
        "category": category,
        "classification": classification,
    }


def plan(cells=CELLS):
    return {"cells": [plan_cell(name, *where) for name, where in cells.items()]}


def summary(cells, passed=None, failed=0, errors=0, diagnostic=(), **changes):
    """The summary.json import mode writes for a bucket of CELLS cells; DIAGNOSTIC holds
    (cell, reason) for each FAIL it excused."""
    written = {
        "schema": 1,
        "cells": cells,
        "passed": cells - failed - errors if passed is None else passed,
        "failed": failed,
        "errors": errors,
        "diagnostic_failures": len(diagnostic),
        "diagnostic_failure_cells": [
            dict(
                zip(("test", "mode", "backend"), name.rsplit("/", 2)),
                diagnostic_reason="known divergence",
                failure_reason=reason,
            )
            for name, reason in diagnostic
        ],
        "host_inapplicable": 0,
        "imported": {
            "root": "IMPORT",
            "source_run_ids": [],
            "missing_cells": 0,
            "dropped_retries": 0,
        },
    }
    written.update(changes)
    return written


def passing(cells):
    return {"rc": 0, "summary": summary(cells), "junit": []}


def write_executable(path, text):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)
    path.chmod(0o755)
    return path


def calls_in(path):
    """How the stand-in harness was called, in order; [] if never."""
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text().splitlines()]


class VerdictTest(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory(prefix="test-buck-e2e-verdict-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.harness = write_executable(
            self.root / "bin" / "test-harness", FAKE_HARNESS
        )
        self.calls = self.root / "calls.jsonl"
        self.out = self.root / "out"
        self.write_plan(plan())

    def write_plan(self, written):
        (self.root / "plan.json").write_text(json.dumps(written))
        self.sizes = {}
        for cell in written["cells"]:
            key = f"{cell['lane']}/{cell['category']}"
            self.sizes[key] = self.sizes.get(key, 0) + 1

    def verdict(self, reports, *extra, harness=None):
        """Run verdict.py with each bucket reported as REPORTS says, defaulting to a
        pass of the cells write_plan gave it; return the process and the harness calls."""
        reports = {**{key: passing(n) for key, n in self.sizes.items()}, **reports}
        environment = dict(
            os.environ,
            FAKE_HARNESS_CALLS=str(self.calls),
            FAKE_HARNESS_REPORTS=json.dumps(reports),
            DAGRUN_TEST_COUNTS_PATH=str(self.root / "counts.json"),
        )
        command = [
            sys.executable,
            str(VERDICT),
            "--plan",
            str(self.root / "plan.json"),
            "--import-dir",
            str(self.root / "import"),
            "--out",
            str(self.out),
            "--harness",
            str(harness or self.harness),
            "--repo-root",
            str(self.root / "repo"),
            *extra,
        ]
        process = subprocess.run(
            command, capture_output=True, text=True, env=environment
        )
        return process, calls_in(self.calls)

    def failed(self, reports, *messages, status=1):
        process, calls = self.verdict(reports)
        self.assertEqual(process.returncode, status, process.stdout + process.stderr)
        for message in messages:
            self.assertIn(message, process.stderr)
        return process, calls

    def test_each_bucket_is_judged_in_import_mode_with_its_cargo_nodes_selection(self):
        process, calls = self.verdict(
            {"portable/compat": {**passing(2), "log": "judged compat\n"}}
        )
        self.assertEqual(process.returncode, 0, process.stderr)
        self.assertEqual(
            [
                f"{c['argv'][c['argv'].index('--lane') + 1]}/{c['argv'][c['argv'].index('--category') + 1]}"
                for c in calls
            ],
            BUCKETS,
        )
        for call, bucket in zip(calls, BUCKETS):
            lane, category = bucket.split("/")
            directory = self.out / lane / ("manifest_" + category.replace("-", "_"))
            expected = [
                "run",
                "--repo-root",
                str(self.root / "repo"),
                "--lane",
                lane,
                "--category",
                category,
                "--ci-only",
                "--prebuilt",
            ]
            # Only the diagnostic buckets' nodes declare --diagnostic-results.
            expected += ["--diagnostic-results"] if category == "compat" else []
            expected += [
                "--results",
                str(directory / "results.jsonl"),
                "--junit",
                str(directory / "junit.xml"),
            ]
            self.assertEqual(call["argv"], expected)
            # One counts file cannot hold three runs' counts, so none is passed on.
            self.assertEqual(
                call["env"],
                {
                    "E2E_IMPORT_RESULTS": str(self.root / "import"),
                    "E2E_RESULT_ROOT": str(self.out),
                    "DAGRUN_TEST_COUNTS_PATH": None,
                },
            )
        self.assertEqual(
            (self.out / "portable" / "manifest_compat" / "harness.log").read_text(),
            "judged compat\n",
        )
        printed = json.loads(process.stdout)
        self.assertEqual(
            (
                printed["buckets"],
                printed["failed_buckets"],
                printed["cells"],
                printed["passed"],
            ),
            (3, [], 5, 5),
        )

    def test_source_sha_is_passed_to_every_harness_run(self):
        sha = "d4676c24bfeb4e01f5e27d7ad979fa40722dba5a"
        process, calls = self.verdict({}, "--source-sha", sha)
        self.assertEqual(process.returncode, 0, process.stderr)
        self.assertEqual(len(calls), 3)
        for call in calls:
            self.assertEqual(
                call["argv"][1:5],
                ["--repo-root", str(self.root / "repo"), "--source-sha", sha],
            )

    def test_a_failing_or_erroring_cell_fails_the_run_and_is_named(self):
        reports = {
            "portable/c-programs": {
                "rc": 1,
                "summary": summary(2, failed=1),
                "junit": [[A, "failure", "exit status 1\nmore detail"], [B, None, ""]],
            },
            "privileged/system-utils": {
                "rc": 1,
                "summary": summary(1, errors=1),
                "junit": [[P, "error", "import-stale: the row's test digest differs"]],
            },
        }
        process, _ = self.failed(
            reports,
            "verdict: portable/c-programs did not pass: test-harness exited 1\n",
            f"verdict:   FAIL {A}: exit status 1\n",
            "verdict: privileged/system-utils did not pass: test-harness exited 1\n",
            f"verdict:   ERROR {P}: import-stale: the row's test digest differs\n",
        )
        self.assertNotIn("portable/compat did not pass", process.stderr)
        printed = json.loads(process.stdout)
        self.assertEqual(
            printed["failed_buckets"],
            ["portable/c-programs", "privileged/system-utils"],
        )
        self.assertEqual((printed["failed"], printed["errors"]), (1, 1))

    def test_an_excused_diagnostic_failure_is_reported_but_does_not_fail_the_run(self):
        excused = {
            "rc": 0,
            "summary": summary(2, failed=1, diagnostic=[(E, "exit status 3")]),
            "junit": [[D, None, ""], [E, "failure", "exit status 3"]],
        }
        process, _ = self.verdict({"portable/compat": excused})
        self.assertEqual(process.returncode, 0, process.stderr)
        self.assertIn(
            f"verdict: DIAGNOSTIC portable/compat {E}: a diagnostic cell's product failure "
            "does not fail the run: exit status 3",
            process.stderr,
        )
        self.assertNotIn("did not pass", process.stderr)
        self.assertEqual(json.loads(process.stdout)["diagnostic_failures"], 1)

    def test_a_failure_the_harness_did_not_excuse_fails_even_when_it_exits_zero(self):
        lying = {
            "rc": 0,
            "summary": summary(2, failed=2, diagnostic=[(E, "exit status 3")]),
            "junit": [[D, "failure", "exit status 1"], [E, "failure", "exit status 3"]],
        }
        process, _ = self.failed(
            {"portable/compat": lying},
            "verdict: portable/compat did not pass: test-harness exited 0 but its summary.json counts "
            "2 FAIL (1 diagnostic) and 0 ERROR",
            f"verdict:   FAIL {D}: exit status 1\n",
        )
        self.assertNotIn(f"FAIL {E}", process.stderr)

    def test_a_zero_exit_passes_only_with_an_imports_summary_of_the_whole_bucket(self):
        cases = [
            ("no summary.json", None, "wrote no summary.json"),
            ("an unreadable summary.json", "{", "its summary.json is unreadable"),
            (
                "an executed run's summary.json",
                {k: v for k, v in summary(2).items() if k != "imported"},
                "its summary.json is not an import's, so it ignored E2E_IMPORT_RESULTS; judge with the "
                "harness staged from this checkout (ci/buck-e2e/stage)",
            ),
            (
                "too few cells",
                summary(1),
                "its summary.json has 1 cells and the plan 2; judge with the "
                "harness and plan of one commit (ci/buck-e2e/stage)",
            ),
            (
                "an ERROR",
                summary(2, errors=1),
                "its summary.json counts 0 FAIL (0 diagnostic) and 1 ERROR",
            ),
            (
                "no counts",
                {
                    k: v
                    for k, v in summary(2).items()
                    if k not in ("passed", "failed", "diagnostic_failures")
                },
                "its summary.json has no integer passed, failed, diagnostic_failures; judge with the harness "
                "staged from this checkout (ci/buck-e2e/stage)",
            ),
            (
                "a count that is not an integer",
                {**summary(2), "errors": "0"},
                "its summary.json has no integer errors;",
            ),
            (
                "no missing_cells",
                summary(2, imported={"root": "IMPORT"}),
                "its summary.json has no integer missing_cells;",
            ),
            (
                "outcomes that do not add up to the cells",
                summary(2, passed=1),
                "its summary.json counts 1 PASS, FAIL, ERROR and HOST-INAPPLICABLE outcomes for 2 cells",
            ),
            (
                "a missing cell",
                summary(2, imported={"missing_cells": 1, "dropped_retries": 0}),
                "its summary.json counts 1 imported cells missing",
            ),
        ]
        for name, written, reason in cases:
            with self.subTest(name):
                shutil.rmtree(self.out, ignore_errors=True)
                report = {
                    "rc": 0,
                    "summary": written,
                    "junit": None,
                    "log": "first line\nlast line\n",
                }
                self.failed(
                    {"portable/compat": report},
                    f"verdict: portable/compat did not pass: test-harness exited 0 but {reason}",
                    # Without a junit.xml, the end of the harness's output says why.
                    "verdict:   no failing cell in junit.xml; the end of "
                    f"{self.out}/portable/manifest_compat/harness.log:\n"
                    "verdict:   | first line\nverdict:   | last line\n",
                )

    def test_a_host_inapplicable_cell_counts_toward_the_bucket(self):
        report = {
            "rc": 0,
            "summary": summary(2, passed=1, host_inapplicable=1),
            "junit": [],
        }
        process, _ = self.verdict({"portable/compat": report})
        self.assertEqual(process.returncode, 0, process.stdout + process.stderr)

    def test_an_error_on_an_excused_diagnostic_cell_is_still_listed(self):
        erroring = {
            "rc": 1,
            "summary": summary(
                2, errors=1, failed=1, passed=0, diagnostic=[(E, "exit status 3")]
            ),
            "junit": [
                [D, "error", "import-stale: the row's test digest differs"],
                [E, "failure", "exit status 3"],
            ],
        }
        # The cell E's FAIL is excused; an ERROR on the same name must not be.
        erroring["junit"].append([E, "error", "import-missing"])
        process, _ = self.failed(
            {"portable/compat": erroring},
            f"verdict:   ERROR {D}: import-stale",
            f"verdict:   ERROR {E}: import-missing\n",
        )
        self.assertNotIn(f"verdict:   FAIL {E}", process.stderr)

    def test_a_harness_that_dies_or_cannot_be_executed_fails_the_run(self):
        for report, died in (
            ({"rc": 126}, "exited 126"),
            ({"rc": 127}, "exited 127"),
            ({"signal": 9}, "was killed by signal 9"),
        ):
            with self.subTest(died):
                shutil.rmtree(self.out, ignore_errors=True)
                self.failed(
                    {"portable/compat": {**report, "summary": None, "junit": None}},
                    f"verdict: portable/compat did not pass: test-harness {died} and wrote no summary.json",
                )
        cases = [
            (
                "not executable",
                "chmod",
                "is not an executable file; stage the inputs (ci/buck-e2e/stage)",
            ),
            (
                "missing",
                "missing",
                "is not an executable file; stage the inputs (ci/buck-e2e/stage)",
            ),
            ("no interpreter", "#!/nonexistent/python3\n", "cannot run"),
        ]
        for name, change, message in cases:
            with self.subTest(name):
                shutil.rmtree(self.out, ignore_errors=True)
                self.calls.unlink(missing_ok=True)
                harness = self.root / "broken" / "test-harness"
                if change == "chmod":
                    write_executable(harness, FAKE_HARNESS).chmod(0o644)
                elif change != "missing":
                    write_executable(harness, change)
                process, calls = self.verdict(
                    {},
                    harness=harness / ".." / name if change == "missing" else harness,
                )
                self.assertEqual(process.returncode, 2, process.stderr)
                self.assertIn(message, process.stderr)
                self.assertEqual(calls, [])

    def test_an_out_directory_that_holds_anything_is_refused(self):
        self.out.mkdir()
        (self.out / "results.jsonl").write_text("{}\n")
        self.failed(
            {},
            "already exists and is not an empty directory; test-harness appends to the "
            "results.jsonl it is given",
            status=2,
        )
        self.assertEqual(calls_in(self.calls), [])
        shutil.rmtree(self.out)
        self.out.write_text("")
        self.failed({}, "already exists and is not an empty directory", status=2)
        self.out.unlink()
        self.out.mkdir()
        process, _ = self.verdict({})
        self.assertEqual(process.returncode, 0, process.stderr)

    def test_a_diagnostic_cell_outside_the_diagnostic_buckets_fails_its_bucket(self):
        cells = dict(CELLS)
        cells[E] = ("portable", "compat", "required")
        cells[B] = ("portable", "c-programs", "diagnostic")
        self.write_plan(plan(cells))
        process, calls = self.failed(
            {},
            f"verdict: portable/c-programs did not pass: the plan puts the diagnostic cells {B} in it, "
            "but only a compat bucket may hold one",
        )
        self.assertEqual(
            json.loads(process.stdout)["failed_buckets"], ["portable/c-programs"]
        )
        # Its node refuses to select the cell, so the bucket is not judged; the flag follows
        # the bucket, as in the cargo flow, not the cells it holds.
        judged = {
            c["argv"][c["argv"].index("--category") + 1]: c["argv"] for c in calls
        }
        self.assertEqual(sorted(judged), ["compat", "system-utils"])
        self.assertIn("--diagnostic-results", judged["compat"])
        self.assertNotIn("--diagnostic-results", judged["system-utils"])

    def test_the_diagnostic_buckets_are_the_validation_dags(self):
        source = (CI / "manifest-plan" / "src" / "validation_dag.rs").read_text()
        found = re.search(
            r"pub const DIAGNOSTIC_MANIFEST_BUCKETS: &\[&str\] = &\[([^\]]*)\];", source
        )
        self.assertIsNotNone(
            found, "DIAGNOSTIC_MANIFEST_BUCKETS moved; point this test at it"
        )
        spec = importlib.util.spec_from_file_location("verdict", VERDICT)
        verdict = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(verdict)
        self.assertEqual(
            verdict.DIAGNOSTIC_BUCKETS, tuple(re.findall(r'"([^"]*)"', found.group(1)))
        )

    def test_a_plan_that_cannot_be_read_is_refused(self):
        cases = [
            ("no file", None),
            ("not JSON", "{"),
            ("no cells key", "{}"),
            ("no cells", '{"cells": []}'),
            ("a cell without a lane", json.dumps({"cells": [{"category": "c"}]})),
        ]
        for name, text in cases:
            with self.subTest(name):
                path = self.root / "plan.json"
                path.unlink(missing_ok=True)
                if text is not None:
                    path.write_text(text)
                self.failed({}, "; pass ci/expected-e2e-plan.json", status=2)


class RunTest(unittest.TestCase):
    """ci/buck-e2e/run on the two-cell plan of test_buck_e2e_ingest.py."""

    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory(prefix="test-buck-e2e-run-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        checkout = self.root / "checkout"
        scripts = checkout / "ci" / "buck-e2e"
        scripts.mkdir(parents=True)
        for name in ("run", "ingest.py", "verdict.py"):
            shutil.copy2(BUCK_E2E / name, scripts / name)
        (checkout / "ci" / "expected-e2e-plan.json").write_text(
            json.dumps(ingest_test.PLAN)
        )
        self.tracked = checkout / "tracked.txt"
        self.tracked.write_text("committed\n")
        git = [
            "git",
            "-C",
            str(checkout),
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ]
        environment = {k: v for k, v in os.environ.items() if not k.startswith("GIT_")}
        for args in (["init", "-q"], ["add", "."], ["commit", "-q", "-m", "scripts"]):
            subprocess.run(git + args, check=True, env=environment)
        head = subprocess.run(
            git + ["rev-parse", "HEAD"],
            check=True,
            capture_output=True,
            text=True,
            env=environment,
        ).stdout.strip()
        # Staged inputs are untracked, as under the ignored ci/buck-e2e/staged.
        (scripts / "staged").mkdir()
        (scripts / "staged" / "SOURCE_SHA").write_text(head + "\n")
        self.harness = write_executable(
            scripts / "staged" / "bin" / "test-harness", FAKE_HARNESS
        )
        self.run_script = scripts / "run"
        self.buck2 = write_executable(self.root / "bin" / "buck2", FAKE_BUCK2)
        self.testx = write_executable(
            self.root / "bin" / "testx", ingest_test.FAKE_TESTX
        )
        self.calls = self.root / "harness-calls.jsonl"
        self.head = head

    def run_buck_e2e(self, mode, runs, *extra, buck2_status=32, **environment):
        """Run ci/buck-e2e/run --mode MODE [EXTRA...], with buck2 exiting BUCK2_STATUS and
        testx listing RUNS ({test run id: [execution, ...]}): the run id is the target name,
        all for local mode, re and local for hybrid. ENVIRONMENT is added to run's."""
        shutil.rmtree(self.root / "fake", ignore_errors=True)
        ingest_test.write_test_runs(self.root / "fake", runs)
        environment = dict(
            os.environ,
            TESTX=str(self.testx),
            FAKE_TESTX_DIR=str(self.root / "fake"),
            FAKE_BUCK2_RC=str(buck2_status),
            FAKE_HARNESS_CALLS=str(self.calls),
            **environment,
        )
        command = [
            str(self.run_script),
            "--mode",
            mode,
            "--out",
            str(self.root / "import"),
            "--work",
            str(self.root / "work"),
            "--buck2",
            str(self.buck2),
            *extra,
        ]
        return subprocess.run(command, capture_output=True, text=True, env=environment)

    def imported_rows(self):
        return sorted(
            str(path.relative_to(self.root / "import"))
            for path in (self.root / "import").rglob("results.jsonl")
        )

    def test_a_failing_or_erroring_cell_fails_the_run_and_is_named(self):
        for outcome, tag in (("FAIL", "FAIL"), ("ERROR", "ERROR")):
            with self.subTest(outcome):
                for directory in ("import", "work", "fake"):
                    shutil.rmtree(self.root / directory, ignore_errors=True)
                runs = {
                    "all": [
                        ingest_test.execution(ingest_test.X, 100, outcome, "rx1"),
                        ingest_test.Y_PASSES,
                    ]
                }
                process = self.run_buck_e2e("local", runs)
                self.assertEqual(process.returncode, 1, process.stdout + process.stderr)
                self.assertIn(
                    "verdict: portable/cat did not pass: test-harness exited 1\n",
                    process.stderr,
                )
                self.assertIn(
                    f"verdict:   {tag} t/x/custom/ptrace: the row says {outcome}\n",
                    process.stderr,
                )
                self.assertIn(
                    "ci/buck-e2e/run: the run did not pass (above); each bucket's result files are in "
                    f"{self.root / 'work'}/verdict",
                    process.stderr,
                )

    def test_a_run_whose_every_cell_passes_exits_zero(self):
        runs = {
            "re": [ingest_test.execution(ingest_test.X, 100, "PASS", "rx1")],
            "local": [ingest_test.Y_PASSES],
        }
        process = self.run_buck_e2e("hybrid", runs, buck2_status=0)
        self.assertEqual(process.returncode, 0, process.stdout + process.stderr)
        self.assertEqual(
            [
                json.loads(line)["failed_buckets"]
                for line in process.stdout.splitlines()
                if '"failed_buckets"' in line
            ],
            [[]],
        )
        calls = calls_in(self.calls)
        self.assertEqual(len(calls), 1)
        self.assertEqual(
            calls[0]["env"]["E2E_IMPORT_RESULTS"], str(self.root / "import")
        )
        self.assertEqual(
            calls[0]["argv"][:3], ["run", "--repo-root", str(self.root / "checkout")]
        )

    def test_a_run_that_fails_before_judging_leaves_no_older_verdict(self):
        passing_runs = {
            "all": [
                ingest_test.execution(ingest_test.X, 100, "PASS", "rx1"),
                ingest_test.Y_PASSES,
            ]
        }
        cases = [
            (
                "buck2 fails",
                passing_runs,
                {"buck2_status": 1},
                "ci/buck-e2e/run: buck2 test (all) failed with exit 1",
            ),
            (
                "ingest.py refuses the rows",
                {"all": [ingest_test.execution(ingest_test.X, 100, "PASS", "rx1")]},
                {},
                "ci/buck-e2e/run: ingest.py could not turn the run's results into rows",
            ),
            (
                "the checkout changes during the run",
                passing_runs,
                {"FAKE_BUCK2_TOUCH": str(self.tracked)},
                "has uncommitted changes after the run",
            ),
        ]
        for name, runs, options, message in cases:
            with self.subTest(name):
                for directory in ("import", "work"):
                    shutil.rmtree(self.root / directory, ignore_errors=True)
                process = self.run_buck_e2e("local", passing_runs)
                self.assertEqual(process.returncode, 0, process.stdout + process.stderr)
                self.assertTrue((self.root / "work" / "verdict").exists())
                shutil.rmtree(self.root / "import")
                process = self.run_buck_e2e("local", runs, **options)
                self.assertEqual(process.returncode, 1, process.stdout + process.stderr)
                self.assertIn(message, process.stderr)
                self.assertFalse(
                    (self.root / "work" / "verdict").exists(),
                    "an older verdict outlived a failed run",
                )
                subprocess.run(
                    [
                        "git",
                        "-C",
                        str(self.root / "checkout"),
                        "checkout",
                        "-q",
                        "--",
                        ".",
                    ],
                    check=True,
                )

    def test_a_run_whose_results_ingest_py_refuses_has_no_verdict(self):
        process = self.run_buck_e2e(
            "local", {"all": [ingest_test.execution(ingest_test.X, 100, "PASS", "rx1")]}
        )
        self.assertEqual(process.returncode, 1, process.stdout + process.stderr)
        self.assertIn(
            "ingest: cells with neither a row nor a host-inapplicable claim",
            process.stderr,
        )
        self.assertIn(
            "ci/buck-e2e/run: ingest.py could not turn the run's results into rows",
            process.stderr,
        )
        self.assertEqual(calls_in(self.calls), [])

    def test_a_run_that_cannot_be_judged_fails_with_one_not_two(self):
        write_executable(self.harness, "#!/nonexistent/python3\n")
        runs = {
            "all": [
                ingest_test.execution(ingest_test.X, 100, "PASS", "rx1"),
                ingest_test.Y_PASSES,
            ]
        }
        process = self.run_buck_e2e("local", runs)
        self.assertEqual(process.returncode, 1, process.stdout + process.stderr)
        self.assertIn(
            "ci/buck-e2e/run: verdict.py could not judge the run (exit 2, above); the run has no verdict",
            process.stderr,
        )

    def test_a_missing_staged_harness_is_refused_before_the_run(self):
        self.harness.unlink()
        process = self.run_buck_e2e("local", {"all": [ingest_test.Y_PASSES]})
        self.assertEqual(process.returncode, 2, process.stdout + process.stderr)
        self.assertIn(
            "staged/bin/test-harness is missing; stage the inputs again (ci/buck-e2e/stage)",
            process.stderr,
        )
        self.assertFalse(
            (self.root / "work").exists(),
            "buck2 ran without a harness to judge its results",
        )

    def test_a_checkout_that_is_not_the_staged_source_is_refused_before_the_run(self):
        passing_runs = {
            "all": [
                ingest_test.execution(ingest_test.X, 100, "PASS", "rx1"),
                ingest_test.Y_PASSES,
            ]
        }
        staged = self.run_script.parent / "staged" / "SOURCE_SHA"
        cases = [
            (
                "HEAD is not SOURCE_SHA",
                lambda: staged.write_text("0" * 40 + "\n"),
                f"ci/buck-e2e/run: HEAD is {self.head} before the run, but the staged inputs are for {'0' * 40}; "
                f"check out {'0' * 40}, or re-stage at HEAD (ci/buck-e2e/stage)",
            ),
            (
                "a tracked file is changed",
                lambda: self.tracked.write_text("edited\n"),
                f"ci/buck-e2e/run: the checkout has uncommitted changes before the run, but every row records "
                f"{self.head} as a clean source tree; commit them and re-stage, or revert them",
            ),
            (
                "git cannot read the index",
                lambda: (self.root / "checkout" / ".git" / "index").write_bytes(
                    b"junk"
                ),
                "ci/buck-e2e/run: git cannot read the checkout ",
            ),
            (
                "HEAD names no commit",
                lambda: (self.root / "checkout" / ".git" / "HEAD").write_text(
                    "ref: refs/heads/unborn\n"
                ),
                "ci/buck-e2e/run: git cannot read the checkout ",
            ),
        ]
        for name, change, message in cases:
            with self.subTest(name):
                restore = {
                    path: path.read_bytes()
                    for path in (
                        staged,
                        self.tracked,
                        self.root / "checkout" / ".git" / "index",
                        self.root / "checkout" / ".git" / "HEAD",
                    )
                }
                change()
                process = self.run_buck_e2e("local", passing_runs)
                for path, data in restore.items():
                    path.write_bytes(data)
                self.assertEqual(process.returncode, 2, process.stdout + process.stderr)
                self.assertIn(message, process.stderr)
                self.assertFalse(
                    (self.root / "work").exists(),
                    "buck2 ran on a checkout that is not the staged one",
                )
                self.assertEqual(calls_in(self.calls), [])
        # Untracked files are not source: the staged inputs themselves are untracked.
        (self.root / "checkout" / "untracked.txt").write_text("scratch\n")
        process = self.run_buck_e2e("local", passing_runs)
        self.assertEqual(process.returncode, 0, process.stdout + process.stderr)

    def test_a_checkout_that_changes_during_the_run_has_no_verdict(self):
        passing_runs = {
            "all": [
                ingest_test.execution(ingest_test.X, 100, "PASS", "rx1"),
                ingest_test.Y_PASSES,
            ]
        }
        cases = [
            (
                "a tracked file changes",
                {"FAKE_BUCK2_TOUCH": str(self.tracked)},
                "has uncommitted changes after the run",
            ),
            (
                "HEAD moves",
                {"FAKE_BUCK2_COMMIT": "1"},
                " after the run, but the staged inputs are for ",
            ),
        ]
        for name, environment, message in cases:
            with self.subTest(name):
                for directory in ("import", "work", "fake"):
                    shutil.rmtree(self.root / directory, ignore_errors=True)
                process = self.run_buck_e2e("local", passing_runs, **environment)
                self.assertEqual(process.returncode, 1, process.stdout + process.stderr)
                self.assertIn(message, process.stderr)
                self.assertEqual(
                    self.imported_rows(),
                    [],
                    "ingest.py ran on rows of a changed checkout",
                )
                self.assertEqual(calls_in(self.calls), [])
                git = ["git", "-C", str(self.root / "checkout")]
                subprocess.run(git + ["reset", "-q", "--hard", self.head], check=True)

    def test_a_checkout_that_changes_while_the_run_is_judged_fails(self):
        runs = {
            "all": [
                ingest_test.execution(ingest_test.X, 100, "PASS", "rx1"),
                ingest_test.Y_PASSES,
            ]
        }
        process = self.run_buck_e2e("local", runs, FAKE_HARNESS_TOUCH=str(self.tracked))
        self.assertEqual(process.returncode, 1, process.stdout + process.stderr)
        self.assertIn(
            "ci/buck-e2e/run: the checkout has uncommitted changes while the run was judged",
            process.stderr,
        )
        self.assertEqual(len(calls_in(self.calls)), 1)

    def test_an_inherited_git_location_does_not_redirect_the_source_checks(self):
        runs = {
            "all": [
                ingest_test.execution(ingest_test.X, 100, "PASS", "rx1"),
                ingest_test.Y_PASSES,
            ]
        }
        nowhere = str(self.root / "nowhere")
        process = self.run_buck_e2e(
            "local",
            runs,
            **{
                name: nowhere
                for name in (
                    "GIT_DIR",
                    "GIT_WORK_TREE",
                    "GIT_INDEX_FILE",
                    "GIT_COMMON_DIR",
                    "GIT_OBJECT_DIRECTORY",
                    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
                    "GIT_NAMESPACE",
                    "GIT_PREFIX",
                )
            },
        )
        self.assertEqual(process.returncode, 0, process.stdout + process.stderr)

    def test_no_verdict_stops_after_ingesting_whatever_the_cells_did(self):
        # A judged, passing run leaves its verdict in --work ...
        passing_runs = {
            "all": [
                ingest_test.execution(ingest_test.X, 100, "PASS", "rx1"),
                ingest_test.Y_PASSES,
            ]
        }
        process = self.run_buck_e2e("local", passing_runs)
        self.assertEqual(process.returncode, 0, process.stdout + process.stderr)
        self.assertTrue((self.root / "work" / "verdict").exists())
        self.calls.unlink()
        shutil.rmtree(self.root / "import")
        # ... which the next run removes, even unjudged, so it cannot stand for that run's rows.
        runs = {
            "all": [
                ingest_test.execution(ingest_test.X, 100, "FAIL", "rx1"),
                ingest_test.Y_PASSES,
            ]
        }
        process = self.run_buck_e2e("local", runs, "--no-verdict")
        self.assertEqual(process.returncode, 0, process.stdout + process.stderr)
        self.assertIn(
            f"ci/buck-e2e/run: --no-verdict: the rows in {self.root / 'import'} are unjudged; "
            "this run has no verdict",
            process.stderr,
        )
        self.assertEqual(self.imported_rows(), ["portable/manifest_cat/results.jsonl"])
        self.assertEqual(calls_in(self.calls), [])
        self.assertFalse((self.root / "work" / "verdict").exists())

    def test_both_help_forms_document_the_exit_status(self):
        for form in ("--help", "-h"):
            with self.subTest(form):
                process = subprocess.run(
                    [str(self.run_script), form], capture_output=True, text=True
                )
                self.assertEqual(process.returncode, 0, process.stderr)
                self.assertIn(
                    "Exit status: 0 when every plan cell passes", process.stdout
                )
                process = subprocess.run(
                    [sys.executable, str(VERDICT), form], capture_output=True, text=True
                )
                self.assertEqual(process.returncode, 0, process.stderr)
                self.assertIn("Exit status: 0 when every bucket passes", process.stdout)


if __name__ == "__main__":
    unittest.main()
