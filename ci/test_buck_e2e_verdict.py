#!/usr/bin/env python3
"""Scenario tests for ci/buck-e2e/verdict.py and the verdict step of ci/buck-e2e/run.

verdict.py hands each (lane, category) bucket of the plan to test-harness's import mode
and turns what the harness reports into one exit status. VerdictTest runs it against a
stand-in harness that records how it was called and reports what the case gives it.
RunTest runs ci/buck-e2e/run end to end in a scratch checkout of its scripts, with
stand-ins for buck2, testx and the staged harness, and checks that a FAIL, an ERROR or
results ingest.py refuses make the run exit non-zero, and that --failed-verify-logs
reaches ingest.py, and what run keeps of each `buck2 test` invocation. ValidateNodeTest
runs ci/buck-e2e/validate-node with stand-ins for the steps it calls and checks where it
sends the logs of cells that did not pass and the invocations' records.
"""

from __future__ import annotations

import importlib.util
import json
import os
import re
import shutil
import shlex
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

import test_buck_e2e_ingest as ingest_test
import test_buck_e2e_stages as stages_test


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
# With FAKE_BUCK2_TOUCH it appends a line to that file (with FAKE_BUCK2_TOUCH_ONLY, only
# when it runs that target); with FAKE_BUCK2_COMMIT it commits
# nothing in its working directory, moving HEAD; with FAKE_BUCK2_CALLS it appends its
# arguments to that file as a JSON line.
FAKE_BUCK2 = r"""#!/usr/bin/env python3
import json, os, subprocess, sys
args = sys.argv[1:]
if os.environ.get("FAKE_BUCK2_CALLS"):
    with open(os.environ["FAKE_BUCK2_CALLS"], "a") as calls:
        calls.write(json.dumps(args) + "\n")
target = next(arg for arg in args if arg.startswith("//ci/buck-e2e:"))
with open(args[args.index("--write-test-id") + 1], "w") as f:
    f.write(target.split(":", 1)[1] + "\n")
if os.environ.get("FAKE_BUCK2_TOUCH") and os.environ.get("FAKE_BUCK2_TOUCH_ONLY", target) == target:
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
    return {"test": test, "mode": mode, "backend": backend, "lane": lane, "category": category,
            "classification": classification}


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
            dict(zip(("test", "mode", "backend"), name.rsplit("/", 2)),
                 diagnostic_reason="known divergence", failure_reason=reason)
            for name, reason in diagnostic
        ],
        "host_inapplicable": 0,
        "imported": {"root": "IMPORT", "source_run_ids": [], "missing_cells": 0, "dropped_retries": 0},
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
        self.harness = write_executable(self.root / "bin" / "test-harness", FAKE_HARNESS)
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
            sys.executable, str(VERDICT),
            "--plan", str(self.root / "plan.json"),
            "--import-dir", str(self.root / "import"),
            "--out", str(self.out),
            "--harness", str(harness or self.harness),
            "--repo-root", str(self.root / "repo"),
            *extra,
        ]
        process = subprocess.run(command, capture_output=True, text=True, env=environment)
        return process, calls_in(self.calls)

    def failed(self, reports, *messages, status=1):
        process, calls = self.verdict(reports)
        self.assertEqual(process.returncode, status, process.stdout + process.stderr)
        for message in messages:
            self.assertIn(message, process.stderr)
        return process, calls

    def test_each_bucket_is_judged_in_import_mode_with_its_cargo_nodes_selection(self):
        process, calls = self.verdict({"portable/compat": {**passing(2), "log": "judged compat\n"}})
        self.assertEqual(process.returncode, 0, process.stderr)
        self.assertEqual([f"{c['argv'][c['argv'].index('--lane') + 1]}/{c['argv'][c['argv'].index('--category') + 1]}"
                          for c in calls], BUCKETS)
        for call, bucket in zip(calls, BUCKETS):
            lane, category = bucket.split("/")
            directory = self.out / lane / ("manifest_" + category.replace("-", "_"))
            expected = ["run", "--repo-root", str(self.root / "repo"), "--lane", lane, "--category", category,
                        "--ci-only", "--prebuilt"]
            # Only the diagnostic buckets' nodes declare --diagnostic-results.
            expected += ["--diagnostic-results"] if category == "compat" else []
            expected += ["--results", str(directory / "results.jsonl"), "--junit", str(directory / "junit.xml")]
            self.assertEqual(call["argv"], expected)
            # One counts file cannot hold three runs' counts, so none is passed on.
            self.assertEqual(call["env"], {"E2E_IMPORT_RESULTS": str(self.root / "import"),
                                           "E2E_RESULT_ROOT": str(self.out), "DAGRUN_TEST_COUNTS_PATH": None})
        self.assertEqual((self.out / "portable" / "manifest_compat" / "harness.log").read_text(), "judged compat\n")
        printed = json.loads(process.stdout)
        self.assertEqual((printed["buckets"], printed["failed_buckets"], printed["cells"], printed["passed"]),
                         (3, [], 5, 5))

    def test_source_sha_is_passed_to_every_harness_run(self):
        sha = "d4676c24bfeb4e01f5e27d7ad979fa40722dba5a"
        process, calls = self.verdict({}, "--source-sha", sha)
        self.assertEqual(process.returncode, 0, process.stderr)
        self.assertEqual(len(calls), 3)
        for call in calls:
            self.assertEqual(call["argv"][1:5], ["--repo-root", str(self.root / "repo"), "--source-sha", sha])

    def test_a_failing_or_erroring_cell_fails_the_run_and_is_named(self):
        reports = {
            "portable/c-programs": {"rc": 1, "summary": summary(2, failed=1),
                                    "junit": [[A, "failure", "exit status 1\nmore detail"], [B, None, ""]]},
            "privileged/system-utils": {"rc": 1, "summary": summary(1, errors=1),
                                        "junit": [[P, "error", "import-stale: the row's test digest differs"]]},
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
        self.assertEqual(printed["failed_buckets"], ["portable/c-programs", "privileged/system-utils"])
        self.assertEqual((printed["failed"], printed["errors"]), (1, 1))

    def test_an_excused_diagnostic_failure_is_reported_but_does_not_fail_the_run(self):
        excused = {"rc": 0, "summary": summary(2, failed=1, diagnostic=[(E, "exit status 3")]),
                   "junit": [[D, None, ""], [E, "failure", "exit status 3"]]}
        process, _ = self.verdict({"portable/compat": excused})
        self.assertEqual(process.returncode, 0, process.stderr)
        self.assertIn(f"verdict: DIAGNOSTIC portable/compat {E}: a diagnostic cell's product failure "
                      "does not fail the run: exit status 3", process.stderr)
        self.assertNotIn("did not pass", process.stderr)
        self.assertEqual(json.loads(process.stdout)["diagnostic_failures"], 1)

    def test_a_failure_the_harness_did_not_excuse_fails_even_when_it_exits_zero(self):
        lying = {"rc": 0, "summary": summary(2, failed=2, diagnostic=[(E, "exit status 3")]),
                 "junit": [[D, "failure", "exit status 1"], [E, "failure", "exit status 3"]]}
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
            ("an executed run's summary.json", {k: v for k, v in summary(2).items() if k != "imported"},
             "its summary.json is not an import's, so it ignored E2E_IMPORT_RESULTS; judge with the "
             "harness staged from this checkout (ci/buck-e2e/stage)"),
            ("too few cells", summary(1), "its summary.json has 1 cells and the plan 2; judge with the "
             "harness and plan of one commit (ci/buck-e2e/stage)"),
            ("an ERROR", summary(2, errors=1), "its summary.json counts 0 FAIL (0 diagnostic) and 1 ERROR"),
            ("no counts", {k: v for k, v in summary(2).items() if k not in ("passed", "failed", "diagnostic_failures")},
             "its summary.json has no integer passed, failed, diagnostic_failures; judge with the harness "
             "staged from this checkout (ci/buck-e2e/stage)"),
            ("a count that is not an integer", {**summary(2), "errors": "0"}, "its summary.json has no integer errors;"),
            ("no missing_cells", summary(2, imported={"root": "IMPORT"}), "its summary.json has no integer missing_cells;"),
            ("outcomes that do not add up to the cells", summary(2, passed=1),
             "its summary.json counts 1 PASS, FAIL, ERROR and HOST-INAPPLICABLE outcomes for 2 cells"),
            ("a missing cell", summary(2, imported={"missing_cells": 1, "dropped_retries": 0}),
             "its summary.json counts 1 imported cells missing"),
        ]
        for name, written, reason in cases:
            with self.subTest(name):
                shutil.rmtree(self.out, ignore_errors=True)
                report = {"rc": 0, "summary": written, "junit": None, "log": "first line\nlast line\n"}
                self.failed(
                    {"portable/compat": report},
                    f"verdict: portable/compat did not pass: test-harness exited 0 but {reason}",
                    # Without a junit.xml, the end of the harness's output says why.
                    "verdict:   no failing cell in junit.xml; the end of "
                    f"{self.out}/portable/manifest_compat/harness.log:\n"
                    "verdict:   | first line\nverdict:   | last line\n",
                )

    def test_a_host_inapplicable_cell_counts_toward_the_bucket(self):
        report = {"rc": 0, "summary": summary(2, passed=1, host_inapplicable=1), "junit": []}
        process, _ = self.verdict({"portable/compat": report})
        self.assertEqual(process.returncode, 0, process.stdout + process.stderr)

    def test_an_error_on_an_excused_diagnostic_cell_is_still_listed(self):
        erroring = {"rc": 1, "summary": summary(2, errors=1, failed=1, passed=0, diagnostic=[(E, "exit status 3")]),
                    "junit": [[D, "error", "import-stale: the row's test digest differs"],
                              [E, "failure", "exit status 3"]]}
        # The cell E's FAIL is excused; an ERROR on the same name must not be.
        erroring["junit"].append([E, "error", "import-missing"])
        process, _ = self.failed({"portable/compat": erroring}, f"verdict:   ERROR {D}: import-stale",
                                 f"verdict:   ERROR {E}: import-missing\n")
        self.assertNotIn(f"verdict:   FAIL {E}", process.stderr)

    def test_a_harness_that_dies_or_cannot_be_executed_fails_the_run(self):
        for report, died in (({"rc": 126}, "exited 126"), ({"rc": 127}, "exited 127"),
                             ({"signal": 9}, "was killed by signal 9")):
            with self.subTest(died):
                shutil.rmtree(self.out, ignore_errors=True)
                self.failed({"portable/compat": {**report, "summary": None, "junit": None}},
                            f"verdict: portable/compat did not pass: test-harness {died} and wrote no summary.json")
        cases = [
            ("not executable", "chmod", "is not an executable file; stage the inputs (ci/buck-e2e/stage)"),
            ("missing", "missing", "is not an executable file; stage the inputs (ci/buck-e2e/stage)"),
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
                process, calls = self.verdict({}, harness=harness / ".." / name if change == "missing" else harness)
                self.assertEqual(process.returncode, 2, process.stderr)
                self.assertIn(message, process.stderr)
                self.assertEqual(calls, [])

    def test_an_out_directory_that_holds_anything_is_refused(self):
        self.out.mkdir()
        (self.out / "results.jsonl").write_text("{}\n")
        self.failed({}, "already exists and is not an empty directory; test-harness appends to the "
                        "results.jsonl it is given", status=2)
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
        self.assertEqual(json.loads(process.stdout)["failed_buckets"], ["portable/c-programs"])
        # Its node refuses to select the cell, so the bucket is not judged; the flag follows
        # the bucket, as in the cargo flow, not the cells it holds.
        judged = {c["argv"][c["argv"].index("--category") + 1]: c["argv"] for c in calls}
        self.assertEqual(sorted(judged), ["compat", "system-utils"])
        self.assertIn("--diagnostic-results", judged["compat"])
        self.assertNotIn("--diagnostic-results", judged["system-utils"])

    def test_the_diagnostic_buckets_are_the_validation_dags(self):
        source = (CI / "manifest-plan" / "src" / "validation_dag.rs").read_text()
        found = re.search(r"pub const DIAGNOSTIC_MANIFEST_BUCKETS: &\[&str\] = &\[([^\]]*)\];", source)
        self.assertIsNotNone(found, "DIAGNOSTIC_MANIFEST_BUCKETS moved; point this test at it")
        spec = importlib.util.spec_from_file_location("verdict", VERDICT)
        verdict = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(verdict)
        self.assertEqual(verdict.DIAGNOSTIC_BUCKETS, tuple(re.findall(r'"([^"]*)"', found.group(1))))

    def test_a_plan_that_cannot_be_read_is_refused(self):
        cases = [("no file", None), ("not JSON", "{"), ("no cells key", "{}"), ("no cells", '{"cells": []}'),
                 ("a cell without a lane", json.dumps({"cells": [{"category": "c"}]}))]
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
        (checkout / "ci" / "expected-e2e-plan.json").write_text(json.dumps(ingest_test.PLAN))
        self.tracked = checkout / "tracked.txt"
        self.tracked.write_text("committed\n")
        git = ["git", "-C", str(checkout), "-c", "user.name=test", "-c", "user.email=test@example.invalid",
               "-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null"]
        environment = {k: v for k, v in os.environ.items() if not k.startswith("GIT_")}
        for args in (["init", "-q"], ["add", "."], ["commit", "-q", "-m", "scripts"]):
            subprocess.run(git + args, check=True, env=environment)
        head = subprocess.run(git + ["rev-parse", "HEAD"], check=True, capture_output=True, text=True,
                              env=environment).stdout.strip()
        # Staged inputs are untracked, as under the ignored ci/buck-e2e/staged.
        (scripts / "staged").mkdir()
        (scripts / "staged" / "SOURCE_SHA").write_text(head + "\n")
        self.harness = write_executable(scripts / "staged" / "bin" / "test-harness", FAKE_HARNESS)
        self.run_script = scripts / "run"
        self.buck2 = write_executable(self.root / "bin" / "buck2", FAKE_BUCK2)
        self.testx = write_executable(self.root / "bin" / "testx", ingest_test.FAKE_TESTX)
        self.calls = self.root / "harness-calls.jsonl"
        self.head = head

    def run_buck_e2e(self, mode, runs, *extra, buck2_status=32, **environment):
        """Run ci/buck-e2e/run --mode MODE [EXTRA...], with buck2 exiting BUCK2_STATUS and
        testx listing RUNS ({test run id: [execution, ...]}): the run id is the target name,
        all for local mode, re and local for hybrid. ENVIRONMENT is added to run's."""
        shutil.rmtree(self.root / "fake", ignore_errors=True)
        ingest_test.write_test_runs(self.root / "fake", runs)
        environment = dict(os.environ, TESTX=str(self.testx), FAKE_TESTX_DIR=str(self.root / "fake"),
                           FAKE_BUCK2_RC=str(buck2_status), FAKE_HARNESS_CALLS=str(self.calls), **environment)
        command = [str(self.run_script), "--mode", mode, "--out", str(self.root / "import"),
                   "--work", str(self.root / "work"), "--buck2", str(self.buck2), *extra]
        return subprocess.run(command, capture_output=True, text=True, env=environment)

    def imported_rows(self):
        return sorted(str(path.relative_to(self.root / "import"))
                      for path in (self.root / "import").rglob("results.jsonl"))

    def test_a_failing_or_erroring_cell_fails_the_run_and_is_named(self):
        for outcome, tag in (("FAIL", "FAIL"), ("ERROR", "ERROR")):
            with self.subTest(outcome):
                for directory in ("import", "work", "fake"):
                    shutil.rmtree(self.root / directory, ignore_errors=True)
                runs = {"all": [ingest_test.execution(ingest_test.X, 100, outcome, "rx1"), ingest_test.Y_PASSES]}
                process = self.run_buck_e2e("local", runs)
                self.assertEqual(process.returncode, 1, process.stdout + process.stderr)
                self.assertIn("verdict: portable/cat did not pass: test-harness exited 1\n", process.stderr)
                self.assertIn(f"verdict:   {tag} t/x/custom/ptrace: the row says {outcome}\n", process.stderr)
                self.assertIn("ci/buck-e2e/run: the run did not pass (above); each bucket's result files are in "
                              f"{self.root / 'work'}/verdict", process.stderr)

    def test_a_run_whose_every_cell_passes_exits_zero(self):
        runs = {"re": [ingest_test.execution(ingest_test.X, 100, "PASS", "rx1")], "local": [ingest_test.Y_PASSES]}
        process = self.run_buck_e2e("hybrid", runs, buck2_status=0)
        self.assertEqual(process.returncode, 0, process.stdout + process.stderr)
        self.assertEqual([json.loads(line)["failed_buckets"] for line in process.stdout.splitlines()
                          if '"failed_buckets"' in line], [[]])
        calls = calls_in(self.calls)
        self.assertEqual(len(calls), 1)
        self.assertEqual(calls[0]["env"]["E2E_IMPORT_RESULTS"], str(self.root / "import"))
        self.assertEqual(calls[0]["argv"][:3], ["run", "--repo-root", str(self.root / "checkout")])

    def test_a_cell_withheld_on_every_re_worker_is_run_again_locally_and_judged(self):
        x_on_re = [ingest_test.withheld_on("re", ingest_test.X, 100, "rx1"),
                   ingest_test.withheld_on("re", ingest_test.X, 110, "rx2")]
        buck2_calls = self.root / "buck2-calls.jsonl"
        for name, rerun, status in (
            ("the local rerun passes", ingest_test.execution(ingest_test.X, 200, "PASS", "rx3"), 0),
            ("the local rerun fails", ingest_test.execution(ingest_test.X, 200, "FAIL", "rx3"), 1),
        ):
            with self.subTest(name):
                for directory in ("import", "work"):
                    shutil.rmtree(self.root / directory, ignore_errors=True)
                buck2_calls.unlink(missing_ok=True)
                self.calls.unlink(missing_ok=True)
                runs = {"re": x_on_re, "local": [ingest_test.Y_PASSES], ingest_test.X_SLUG: [rerun]}
                process = self.run_buck_e2e("hybrid", runs, buck2_status=0, FAKE_BUCK2_CALLS=str(buck2_calls))
                self.assertEqual(process.returncode, status, process.stdout + process.stderr)
                self.assertIn("1 cell(s) were withheld on every RE worker that ran them; running them locally: "
                              f"{ingest_test.X_SLUG}\n", process.stderr)
                reruns = [args for args in calls_in(buck2_calls) if f"//ci/buck-e2e:{ingest_test.X_SLUG}" in args]
                self.assertEqual(len(reruns), 1, calls_in(buck2_calls))
                self.assertIn("hermit_e2e.routing=local", reruns[0])
                self.assertEqual(len(calls_in(buck2_calls)), 3)
                rows = [json.loads(line) for path in (self.root / "import").rglob("results.jsonl")
                        for line in path.read_text().splitlines()]
                x_rows = [(row["outcome"], row["attempt"]) for row in rows if row["test"] == "t/x"]
                self.assertEqual(x_rows, [(rerun["rows"][0]["outcome"], 1)])
                self.assertEqual(len(calls_in(self.calls)), 1, "the run was not judged once, after the rerun")

    def test_a_checkout_that_changes_during_the_local_rerun_has_no_verdict(self):
        runs = {"re": [ingest_test.withheld_on("re", ingest_test.X, 100, "rx1")], "local": [ingest_test.Y_PASSES],
                ingest_test.X_SLUG: [ingest_test.execution(ingest_test.X, 200, "PASS", "rx2")]}
        process = self.run_buck_e2e("hybrid", runs, buck2_status=0, FAKE_BUCK2_TOUCH=str(self.tracked),
                                    FAKE_BUCK2_TOUCH_ONLY=f"//ci/buck-e2e:{ingest_test.X_SLUG}")
        self.assertEqual(process.returncode, 1, process.stdout + process.stderr)
        self.assertIn("running them locally: " + ingest_test.X_SLUG, process.stderr)
        self.assertIn("has uncommitted changes after the local rerun", process.stderr)
        self.assertEqual(calls_in(self.calls), [])

    def test_a_cell_still_withheld_only_on_re_after_the_local_rerun_has_no_verdict(self):
        x_on_re = ingest_test.withheld_on("re", ingest_test.X, 100, "rx1")
        runs = {"re": [x_on_re], "local": [ingest_test.Y_PASSES],
                ingest_test.X_SLUG: [ingest_test.withheld_on("re", ingest_test.X, 200, "rx2")]}
        process = self.run_buck_e2e("hybrid", runs, buck2_status=0)
        self.assertEqual(process.returncode, 1, process.stdout + process.stderr)
        self.assertIn("after the local rerun, cells still have only RE claims", process.stderr)
        self.assertEqual(calls_in(self.calls), [])

    def test_a_run_that_fails_before_judging_leaves_no_older_verdict(self):
        passing_runs = {"all": [ingest_test.execution(ingest_test.X, 100, "PASS", "rx1"), ingest_test.Y_PASSES]}
        cases = [
            ("buck2 fails", passing_runs, {"buck2_status": 1}, "ci/buck-e2e/run: buck2 test (all) failed with exit 1"),
            ("ingest.py refuses the rows", {"all": [ingest_test.execution(ingest_test.X, 100, "PASS", "rx1")]}, {},
             "ci/buck-e2e/run: ingest.py could not turn the run's results into rows"),
            ("the checkout changes during the run", passing_runs, {"FAKE_BUCK2_TOUCH": str(self.tracked)},
             "has uncommitted changes after the run"),
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
                self.assertFalse((self.root / "work" / "verdict").exists(), "an older verdict outlived a failed run")
                subprocess.run(["git", "-C", str(self.root / "checkout"), "checkout", "-q", "--", "."], check=True)

    def test_a_run_whose_results_ingest_py_refuses_has_no_verdict(self):
        process = self.run_buck_e2e("local", {"all": [ingest_test.execution(ingest_test.X, 100, "PASS", "rx1")]})
        self.assertEqual(process.returncode, 1, process.stdout + process.stderr)
        self.assertIn("ingest: cells with neither a row nor a host-inapplicable claim", process.stderr)
        self.assertIn("ci/buck-e2e/run: ingest.py could not turn the run's results into rows", process.stderr)
        self.assertEqual(calls_in(self.calls), [])

    def test_a_run_that_cannot_be_judged_fails_with_one_not_two(self):
        write_executable(self.harness, "#!/nonexistent/python3\n")
        runs = {"all": [ingest_test.execution(ingest_test.X, 100, "PASS", "rx1"), ingest_test.Y_PASSES]}
        process = self.run_buck_e2e("local", runs)
        self.assertEqual(process.returncode, 1, process.stdout + process.stderr)
        self.assertIn("ci/buck-e2e/run: verdict.py could not judge the run (exit 2, above); the run has no verdict",
                      process.stderr)

    def test_a_missing_staged_harness_is_refused_before_the_run(self):
        self.harness.unlink()
        process = self.run_buck_e2e("local", {"all": [ingest_test.Y_PASSES]})
        self.assertEqual(process.returncode, 2, process.stdout + process.stderr)
        self.assertIn("staged/bin/test-harness is missing; stage the inputs again (ci/buck-e2e/stage)",
                      process.stderr)
        self.assertFalse((self.root / "work").exists(), "buck2 ran without a harness to judge its results")

    def test_a_checkout_that_is_not_the_staged_source_is_refused_before_the_run(self):
        passing_runs = {"all": [ingest_test.execution(ingest_test.X, 100, "PASS", "rx1"), ingest_test.Y_PASSES]}
        staged = self.run_script.parent / "staged" / "SOURCE_SHA"
        cases = [
            ("HEAD is not SOURCE_SHA", lambda: staged.write_text("0" * 40 + "\n"),
             f"ci/buck-e2e/run: HEAD is {self.head} before the run, but the staged inputs are for {'0' * 40}; "
             f"check out {'0' * 40}, or re-stage at HEAD (ci/buck-e2e/stage)"),
            ("a tracked file is changed", lambda: self.tracked.write_text("edited\n"),
             f"ci/buck-e2e/run: the checkout has uncommitted changes before the run, but every row records "
             f"{self.head} as a clean source tree; commit them and re-stage, or revert them"),
            ("git cannot read the index", lambda: (self.root / "checkout" / ".git" / "index").write_bytes(b"junk"),
             "ci/buck-e2e/run: git cannot read the checkout "),
            ("HEAD names no commit", lambda: (self.root / "checkout" / ".git" / "HEAD").write_text(
                "ref: refs/heads/unborn\n"), "ci/buck-e2e/run: git cannot read the checkout "),
        ]
        for name, change, message in cases:
            with self.subTest(name):
                restore = {path: path.read_bytes()
                           for path in (staged, self.tracked, self.root / "checkout" / ".git" / "index",
                                        self.root / "checkout" / ".git" / "HEAD")}
                change()
                process = self.run_buck_e2e("local", passing_runs)
                for path, data in restore.items():
                    path.write_bytes(data)
                self.assertEqual(process.returncode, 2, process.stdout + process.stderr)
                self.assertIn(message, process.stderr)
                self.assertFalse((self.root / "work").exists(), "buck2 ran on a checkout that is not the staged one")
                self.assertEqual(calls_in(self.calls), [])
        # Untracked files are not source: the staged inputs themselves are untracked.
        (self.root / "checkout" / "untracked.txt").write_text("scratch\n")
        process = self.run_buck_e2e("local", passing_runs)
        self.assertEqual(process.returncode, 0, process.stdout + process.stderr)

    def test_a_checkout_that_changes_during_the_run_has_no_verdict(self):
        passing_runs = {"all": [ingest_test.execution(ingest_test.X, 100, "PASS", "rx1"), ingest_test.Y_PASSES]}
        cases = [
            ("a tracked file changes", {"FAKE_BUCK2_TOUCH": str(self.tracked)}, "has uncommitted changes after the run"),
            ("HEAD moves", {"FAKE_BUCK2_COMMIT": "1"}, " after the run, but the staged inputs are for "),
        ]
        for name, environment, message in cases:
            with self.subTest(name):
                for directory in ("import", "work", "fake"):
                    shutil.rmtree(self.root / directory, ignore_errors=True)
                process = self.run_buck_e2e("local", passing_runs, **environment)
                self.assertEqual(process.returncode, 1, process.stdout + process.stderr)
                self.assertIn(message, process.stderr)
                self.assertEqual(self.imported_rows(), [], "ingest.py ran on rows of a changed checkout")
                self.assertEqual(calls_in(self.calls), [])
                git = ["git", "-C", str(self.root / "checkout")]
                subprocess.run(git + ["reset", "-q", "--hard", self.head], check=True)

    def test_a_checkout_that_changes_while_the_run_is_judged_fails(self):
        runs = {"all": [ingest_test.execution(ingest_test.X, 100, "PASS", "rx1"), ingest_test.Y_PASSES]}
        process = self.run_buck_e2e("local", runs, FAKE_HARNESS_TOUCH=str(self.tracked))
        self.assertEqual(process.returncode, 1, process.stdout + process.stderr)
        self.assertIn("ci/buck-e2e/run: the checkout has uncommitted changes while the run was judged", process.stderr)
        self.assertEqual(len(calls_in(self.calls)), 1)

    def test_an_inherited_git_location_does_not_redirect_the_source_checks(self):
        runs = {"all": [ingest_test.execution(ingest_test.X, 100, "PASS", "rx1"), ingest_test.Y_PASSES]}
        nowhere = str(self.root / "nowhere")
        process = self.run_buck_e2e("local", runs, **{name: nowhere for name in (
            "GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_COMMON_DIR", "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES", "GIT_NAMESPACE", "GIT_PREFIX")})
        self.assertEqual(process.returncode, 0, process.stdout + process.stderr)

    def test_no_verdict_stops_after_ingesting_whatever_the_cells_did(self):
        # A judged, passing run leaves its verdict in --work ...
        passing_runs = {"all": [ingest_test.execution(ingest_test.X, 100, "PASS", "rx1"), ingest_test.Y_PASSES]}
        process = self.run_buck_e2e("local", passing_runs)
        self.assertEqual(process.returncode, 0, process.stdout + process.stderr)
        self.assertTrue((self.root / "work" / "verdict").exists())
        self.calls.unlink()
        shutil.rmtree(self.root / "import")
        # ... which the next run removes, even unjudged, so it cannot stand for that run's rows.
        runs = {"all": [ingest_test.execution(ingest_test.X, 100, "FAIL", "rx1"), ingest_test.Y_PASSES]}
        process = self.run_buck_e2e("local", runs, "--no-verdict")
        self.assertEqual(process.returncode, 0, process.stdout + process.stderr)
        self.assertIn(f"ci/buck-e2e/run: --no-verdict: the rows in {self.root / 'import'} are unjudged; "
                      "this run has no verdict", process.stderr)
        self.assertEqual(self.imported_rows(), ["portable/manifest_cat/results.jsonl"])
        self.assertEqual(calls_in(self.calls), [])
        self.assertFalse((self.root / "work" / "verdict").exists())

    def test_both_help_forms_document_the_exit_status(self):
        for form in ("--help", "-h"):
            with self.subTest(form):
                process = subprocess.run([str(self.run_script), form], capture_output=True, text=True)
                self.assertEqual(process.returncode, 0, process.stderr)
                self.assertIn("Exit status: 0 when every plan cell passes", process.stdout)
                process = subprocess.run([sys.executable, str(VERDICT), form], capture_output=True, text=True)
                self.assertEqual(process.returncode, 0, process.stderr)
                self.assertIn("Exit status: 0 when every bucket passes", process.stdout)

    def test_failed_verify_logs_reach_ingest(self):
        diverged = ingest_test.with_logs(ingest_test.execution(ingest_test.Y, 150, "FAIL", "ry1"), ingest_test.DIVERGED_LOGS)
        runs = {"all": [ingest_test.execution(ingest_test.X, 100, "PASS", "rx1"), diverged]}
        kept = self.root / "kept"
        process = self.run_buck_e2e("local", runs, "--no-verdict", "--failed-verify-logs", str(kept))
        self.assertEqual(process.returncode, 0, process.stdout + process.stderr)
        self.assertEqual(sorted(str(p.relative_to(kept)) for p in kept.rglob("*") if p.is_file()),
                         ["index.jsonl", "ry1/verify-logs/verify-1/run1_log_detlog",
                          "ry1/verify-logs/verify-1/run2_log_detlog"])

    def test_each_invocation_leaves_its_record_and_an_earlier_runs_records_are_removed(self):
        work = self.root / "work"
        work.mkdir()
        earlier = ["all.event-log.pb.zst", "all.times", "re.log", "rerun-local.rc", "rerun-local.test_id"]
        for name in earlier:
            (work / name).write_text("from an earlier run\n")
        (work / "unrelated.txt").write_text("not an invocation record\n")
        runs = {"all": [ingest_test.execution(ingest_test.X, 100, "PASS", "rx1"), ingest_test.Y_PASSES]}
        process = self.run_buck_e2e("local", runs)
        self.assertEqual(process.returncode, 0, process.stdout + process.stderr)
        # The stand-in buck2 writes no event log, so the earlier one must be gone, not kept.
        records = sorted(p.name for p in work.iterdir()
                         if p.name.split(".")[0] in ("re", "local", "all", "rerun-local"))
        self.assertEqual(records, ["all.log", "all.rc", "all.test_id", "all.times"])
        self.assertTrue((work / "unrelated.txt").exists())
        self.assertEqual((work / "all.rc").read_text(), "32\n")
        self.assertEqual((work / "all.test_id").read_text(), "all\n")
        start, end = map(float, (work / "all.times").read_text().splitlines())
        self.assertLessEqual(start, end)

    def test_failed_verify_logs_inside_work_or_out_is_refused_before_the_run(self):
        runs = {"all": [ingest_test.execution(ingest_test.X, 100, "PASS", "rx1"), ingest_test.Y_PASSES]}
        for target in (self.root / "work" / "verdict", self.root / "work", self.root / "import"):
            with self.subTest(str(target.relative_to(self.root))):
                process = self.run_buck_e2e("local", runs, "--failed-verify-logs", str(target))
                self.assertEqual(process.returncode, 2, process.stdout + process.stderr)
                self.assertIn("overlaps", process.stderr)
                self.assertFalse((self.root / "work").exists(), "buck2 ran before the refusal")


# Each step validate-node calls: appends its name and argv to FAKE_STEP_CALLS, and exits 17
# if it is the step FAKE_FAIL_STEP names.
FAKE_STEP = r"""#!/usr/bin/env python3
import json, os, sys
with open(os.environ["FAKE_STEP_CALLS"], "a") as calls:
    calls.write(json.dumps([os.path.basename(sys.argv[0])] + sys.argv[1:]) + "\n")
if os.environ.get("FAKE_FAIL_STEP") == os.path.basename(sys.argv[0]):
    sys.exit(17)
"""
# ci/buck-e2e/run, as a step that also writes FAKE_RUN_RECORDS ({file name: text}) into its
# --work and exits FAKE_RUN_RC, or, given FAKE_RUN_READY, creates that file and waits to be
# killed, as a run does when the cells outlast the node's deadline.
FAKE_RUN = FAKE_STEP + r"""
import time
work = sys.argv[sys.argv.index("--work") + 1]
for name, text in json.loads(os.environ.get("FAKE_RUN_RECORDS") or "{}").items():
    with open(os.path.join(work, name), "w") as f:
        f.write(text)
if os.environ.get("FAKE_RUN_READY"):
    open(os.environ["FAKE_RUN_READY"], "w").close()
    time.sleep(600)
sys.exit(int(os.environ.get("FAKE_RUN_RC", "0")))
"""
# buck2, for what validate-node asks of it: `log show FILE` prints FILE, which here holds the
# JSON lines themselves (or fails, with FAKE_BUCK2_LOG_SHOW_FAILS, or never ends, with
# FAKE_BUCK2_LOG_SHOW_HANGS), and `kill` appends a line to FAKE_BUCK2_KILLS, when set, and
# exits FAKE_BUCK2_KILL_RC.
FAKE_NODE_BUCK2 = """#!/bin/sh
case $1 in
    log)
        if [ -n "${FAKE_BUCK2_LOG_SHOW_FAILS:-}" ]; then echo "buck2: cannot read $3" >&2; exit 3; fi
        if [ -n "${FAKE_BUCK2_LOG_SHOW_HANGS:-}" ]; then exec sleep 600; fi
        exec cat -- "$3" ;;
    kill)
        if [ -n "${FAKE_BUCK2_KILLS:-}" ]; then echo kill >>"$FAKE_BUCK2_KILLS"; fi
        exit "${FAKE_BUCK2_KILL_RC:-0}" ;;
esac
exit 0
"""
# What the stand-in run leaves in --work: a hybrid run's two invocations, one with an event log.
# The retention budget the stalled-summary test gives validate-node, and what the stall may cost
# it after its exit phase: that budget plus an allowance for everything else (each stalled
# command's 0.5 s `timeout -k` grace, copying the records, `buck2 kill`, process exit). Measured on
# the validation host with the test pinned to one CPU shared with 8 busy loops, the interval ran
# at most 1.217 s beyond a 1 s budget over 128 sub-cases (80 here: 1.175-1.374 s; one of 48 in
# the review of https://github.com/rrnewton/hermit/pull/4017 reached 2.217 s); idle it is about
# 0.1 s beyond. The 2 s allowance covers that worst case with 0.78 s to spare, and the 3 s budget
# keeps a doubled budget (6 s) well above the bound.
# https://github.com/rrnewton/hermit/issues/3923
STALLED_SUMMARY_RETENTION_SECONDS = 3
AFTER_EXIT_BOUND_SECONDS = STALLED_SUMMARY_RETENTION_SECONDS + 2.0

RECORDS = {
    "re.log": "re console\n", "re.rc": "0\n", "re.test_id": "re\n", "re.times": "1.5\n9.25\n",
    "re.event-log.pb.zst": "".join(stages_test.two_cells()),
    "local.log": "local console\n", "local.rc": "32\n",
}


class ValidateNodeTest(unittest.TestCase):
    """ci/buck-e2e/validate-node in a scratch checkout whose steps are stand-ins."""

    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory(prefix="test-buck-e2e-validate-node-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        checkout = self.root / "checkout"
        (checkout / "ci" / "buck-e2e").mkdir(parents=True)
        self.node = checkout / "ci" / "buck-e2e" / "validate-node"
        shutil.copy2(BUCK_E2E / "validate-node", self.node)
        self.checkout = checkout
        shutil.copy2(BUCK_E2E / "stages.py", checkout / "ci" / "buck-e2e" / "stages.py")
        for step in ("bootstrap/regenerate-rust-deps", "shim/modes/stage-re-inputs", "ci/buck-e2e/stage"):
            write_executable(checkout / step, FAKE_STEP)
        write_executable(checkout / "ci" / "buck-e2e" / "run", FAKE_RUN)
        # with-proxy, when installed, prefixes the network steps; this one only runs them.
        write_executable(self.root / "bin" / "with-proxy", '#!/bin/sh\nexec "$@"\n')
        self.buck2 = write_executable(self.root / "bin" / "buck2", FAKE_NODE_BUCK2)
        self.calls = self.root / "calls.jsonl"

    def node_environment(self, e2e_result_root, runner, added):
        environment = {k: v for k, v in os.environ.items()
                       if k not in ("E2E_RESULT_ROOT", "HERMIT_VALIDATE_BUCK_RETENTION_SECONDS",
                                    "HERMIT_VALIDATE_BUCK_STEP_WALL_SECONDS", "DAGRUN_STEP_STARTED_MONOTONIC_NS")}
        environment.update(PATH=f"{self.root / 'bin'}:{os.environ['PATH']}",
                           HERMIT_VALIDATE_E2E_RUNNER=runner, HERMIT_VALIDATE_BUCK2=str(self.buck2),
                           VALIDATE_RUN_STATE=str(self.root / "state"), HERMIT_EPOCH="2026-10-05T00:00:00+00:00",
                           FAKE_STEP_CALLS=str(self.calls), **added)
        if e2e_result_root is not None:
            environment["E2E_RESULT_ROOT"] = e2e_result_root
        return environment

    def validate_node(self, e2e_result_root, cwd=None, args=(), runner="buck-local", **added):
        """Run validate-node ARGS from CWD with E2E_RESULT_ROOT (None: unset); ADDED is added to
        its environment."""
        return subprocess.run([str(self.node), *args], capture_output=True, text=True, timeout=120,
                              env=self.node_environment(e2e_result_root, runner, added), cwd=cwd)

    def commit_checkout(self):
        """Make the scratch checkout a git repository with one commit; return its HEAD."""
        environment = {k: v for k, v in os.environ.items() if not k.startswith("GIT_")}
        git = ["git", "-C", str(self.checkout), "-c", "user.name=test", "-c", "user.email=test@example.invalid",
               "-c", "commit.gpgsign=false"]
        for command in (["init", "-q"], ["add", "-A"], ["commit", "-q", "-m", "scratch"]):
            subprocess.run(git + command, check=True, env=environment, capture_output=True)
        return subprocess.run(git + ["rev-parse", "HEAD"], check=True, env=environment, capture_output=True,
                              text=True).stdout.strip()

    def write_staged_source_sha(self, sha):
        staged = self.checkout / "ci" / "buck-e2e" / "staged"
        staged.mkdir(parents=True, exist_ok=True)
        (staged / "SOURCE_SHA").write_text(sha + "\n")

    def test_without_an_argument_the_node_regenerates_stages_and_runs_in_order(self):
        process = self.validate_node(str(self.root / "e2e-results"))
        self.assertEqual(process.returncode, 0, process.stdout + process.stderr)
        calls = calls_in(self.calls)
        self.assertEqual([call[0] for call in calls], ["regenerate-rust-deps", "stage", "run"])
        self.assertEqual(calls[1], ["stage", "--from-cargo"])

    def test_stage_only_keeps_no_invocation_records(self):
        process = self.validate_node(str(self.root / "e2e-results"), args=["--stage-only"])
        self.assertEqual(process.returncode, 0, process.stdout + process.stderr)
        self.assertFalse((self.root / "e2e-results").exists())

    def test_stage_only_stages_and_needs_no_runner_environment(self):
        environment = {k: v for k, v in os.environ.items()
                       if k not in ("HERMIT_VALIDATE_E2E_RUNNER", "HERMIT_VALIDATE_BUCK2", "VALIDATE_RUN_STATE",
                                    "E2E_RESULT_ROOT")}
        environment.update(PATH=f"{self.root / 'bin'}:{os.environ['PATH']}", FAKE_STEP_CALLS=str(self.calls))
        process = subprocess.run([str(self.node), "--stage-only"], capture_output=True, text=True,
                                 env=environment)
        self.assertEqual(process.returncode, 0, process.stdout + process.stderr)
        self.assertEqual(calls_in(self.calls), [["stage", "--from-cargo"]])

    def test_cells_only_runs_the_cells_against_inputs_staged_for_head(self):
        self.write_staged_source_sha(self.commit_checkout())
        process = self.validate_node(str(self.root / "e2e-results"), args=["--cells-only"])
        self.assertEqual(process.returncode, 0, process.stdout + process.stderr)
        self.assertEqual([call[0] for call in calls_in(self.calls)], ["regenerate-rust-deps", "run"])

    def test_cells_only_under_buck_hybrid_stages_the_re_link_inputs_but_not_the_cargo_inputs(self):
        self.write_staged_source_sha(self.commit_checkout())
        process = self.validate_node(str(self.root / "e2e-results"), args=["--cells-only"], runner="buck-hybrid")
        self.assertEqual(process.returncode, 0, process.stdout + process.stderr)
        self.assertEqual([call[0] for call in calls_in(self.calls)],
                         ["regenerate-rust-deps", "stage-re-inputs", "run"])

    def test_cells_only_without_staged_inputs_is_refused_before_any_step(self):
        self.commit_checkout()
        process = self.validate_node(str(self.root / "e2e-results"), args=["--cells-only"])
        self.assertEqual(process.returncode, 2, process.stdout + process.stderr)
        self.assertIn("SOURCE_SHA is missing", process.stderr)
        self.assertFalse(self.calls.exists(), "a step ran before the node was refused")
        self.assertFalse((self.root / "e2e-results").exists(), "a refused node replaced the earlier records")

    def test_cells_only_with_inputs_staged_for_another_commit_is_refused_before_any_step(self):
        head = self.commit_checkout()
        other = "0" * 40
        self.write_staged_source_sha(other)
        process = self.validate_node(str(self.root / "e2e-results"), args=["--cells-only"])
        self.assertEqual(process.returncode, 2, process.stdout + process.stderr)
        self.assertIn(f"staged for {other}, but HEAD is {head}", process.stderr)
        self.assertFalse(self.calls.exists(), "a step ran before the node was refused")

    def test_two_phase_arguments_are_refused(self):
        process = self.validate_node(str(self.root / "e2e-results"), args=["--stage-only", "--cells-only"])
        self.assertEqual(process.returncode, 2, process.stdout + process.stderr)
        self.assertIn("unexpected argument: --cells-only", process.stderr)
        self.assertFalse(self.calls.exists(), "a step ran before the node was refused")

    def test_the_logs_of_cells_that_did_not_pass_go_below_the_e2e_result_root(self):
        results = self.root / "e2e-results"
        stale = results / "buck-failed-verify-logs"
        stale.mkdir(parents=True)
        (stale / "index.jsonl").write_text("from an earlier attempt\n")
        process = self.validate_node(str(results))
        self.assertEqual(process.returncode, 0, process.stdout + process.stderr)
        run = [call for call in calls_in(self.calls) if call[0] == "run"]
        self.assertEqual(len(run), 1, run)
        argv = run[0]
        self.assertEqual(argv[argv.index("--failed-verify-logs") + 1], str(stale))
        self.assertEqual(argv[argv.index("--out") + 1], str(self.root / "state" / "buck-e2e" / "results"))
        self.assertFalse(stale.exists(), "an earlier attempt's logs would stand for this run's")

    def test_an_earlier_copy_that_cannot_be_removed_costs_only_the_logs(self):
        results = self.root / "e2e-results"
        stuck = results / "buck-failed-verify-logs" / "run" / "verify-logs"
        stuck.mkdir(parents=True)
        (stuck / "run1_log_detlog").write_text("earlier\n")
        stuck.chmod(0o555)
        self.addCleanup(stuck.chmod, 0o755)
        process = self.validate_node(str(results))
        self.assertEqual(process.returncode, 0, process.stdout + process.stderr)
        self.assertIn("this run keeps no failed-cell verify logs", process.stderr)
        argv = [call for call in calls_in(self.calls) if call[0] == "run"][0]
        self.assertNotIn("--failed-verify-logs", argv)

    def kept(self, results):
        return results / "buck-invocations"

    def exit_time(self, results):
        """The epoch time phases.tsv records for the node's exit: when it began keeping records."""
        rows = [line.split("\t") for line in (self.kept(results) / "phases.tsv").read_text().splitlines()]
        exits = [float(time) for time, what in rows if what.startswith("exit ")]
        self.assertEqual(len(exits), 1, rows)
        return exits[0]

    def phases(self, results):
        rows = [line.split("\t") for line in (self.kept(results) / "phases.tsv").read_text().splitlines()]
        times = [float(time) for time, _ in rows]
        self.assertEqual(times, sorted(times))
        return [what for _, what in rows]

    def exit_phase_time(self, results):
        """The epoch time at which the node recorded its exit phase, before keeping its records."""
        rows = [line.split("\t") for line in (self.kept(results) / "phases.tsv").read_text().splitlines()]
        exits = [float(time) for time, what in rows if what.startswith("exit ")]
        self.assertEqual(len(exits), 1, rows)
        return exits[0]

    def test_each_invocations_record_is_kept_below_the_e2e_result_root(self):
        results = self.root / "e2e-results"
        (self.kept(results)).mkdir(parents=True)
        (self.kept(results) / "earlier.log").write_text("from an earlier attempt\n")
        process = self.validate_node(str(results), FAKE_RUN_RECORDS=json.dumps(RECORDS))
        self.assertEqual(process.returncode, 0, process.stdout + process.stderr)
        kept = self.kept(results)
        self.assertEqual(sorted(p.name for p in kept.iterdir()), sorted([*RECORDS, "phases.tsv", "re.stages.txt"]))
        for name, text in RECORDS.items():
            self.assertEqual((kept / name).read_text(), text, name)
        self.assertEqual((kept / "re.stages.txt").read_text(), stages_test.StagesTest.summary(
            None, stages_test.two_cells()))
        phases = self.phases(results)
        self.assertEqual(phases[:3], ["regenerating the Buck third-party rules",
                                      "staging the inputs Buck does not build (cargo, target/)",
                                      "running every cell under Buck (buck-local), "
                                      "HERMIT_EPOCH=2026-10-05T00:00:00+00:00"])
        self.assertEqual(phases[3:], ["exit 0"])

    def test_a_run_that_fails_keeps_its_records_and_its_status(self):
        results = self.root / "e2e-results"
        process = self.validate_node(str(results), FAKE_RUN_RECORDS=json.dumps(RECORDS), FAKE_RUN_RC="7")
        self.assertEqual(process.returncode, 7, process.stdout + process.stderr)
        self.assertEqual((self.kept(results) / "re.event-log.pb.zst").read_text(), RECORDS["re.event-log.pb.zst"])
        self.assertEqual(self.phases(results)[-1], "exit 7")

    def test_keeping_the_records_never_changes_the_nodes_status(self):
        results = self.root / "e2e-results"
        for name, run_status, added in (
            ("the event log cannot be shown", 0, {"FAKE_BUCK2_LOG_SHOW_FAILS": "1"}),
            ("the buck2 daemon cannot be killed", 0, {"FAKE_BUCK2_KILL_RC": "1"}),
            ("both, after a failed run", 7, {"FAKE_BUCK2_LOG_SHOW_FAILS": "1", "FAKE_BUCK2_KILL_RC": "1"}),
        ):
            with self.subTest(name):
                process = self.validate_node(str(results), FAKE_RUN_RECORDS=json.dumps(RECORDS),
                                             FAKE_RUN_RC=str(run_status), **added)
                self.assertEqual(process.returncode, run_status, process.stdout + process.stderr)
                self.assertEqual((self.kept(results) / "re.log").read_text(), RECORDS["re.log"])
                if "FAKE_BUCK2_LOG_SHOW_FAILS" in added:
                    self.assertIn("validate-node: the stage summary of the re invocation is incomplete",
                                  process.stderr)

    def test_a_stalled_stage_summary_costs_only_the_summary(self):
        results = self.root / "e2e-results"
        kills = self.root / "kills"
        summarizer = self.checkout / "ci" / "buck-e2e" / "stages.py"
        for stalled, run_status in (("log show", 0), ("log show", 7), ("stages.py", 0), ("stages.py", 7)):
            with self.subTest(stalled=stalled, run_status=run_status):
                kills.unlink(missing_ok=True)
                shutil.copy2(BUCK_E2E / "stages.py", summarizer)
                added = {"FAKE_BUCK2_LOG_SHOW_HANGS": "1"}
                if stalled == "stages.py":
                    summarizer.write_text("import time\ntime.sleep(600)\n")
                    added = {}
                process = self.validate_node(str(results), FAKE_RUN_RECORDS=json.dumps(RECORDS),
                                             FAKE_RUN_RC=str(run_status), FAKE_BUCK2_KILLS=str(kills),
                                             HERMIT_VALIDATE_BUCK_RETENTION_SECONDS=str(STALLED_SUMMARY_RETENTION_SECONDS),
                                             **added)
                ended = time.time()
                self.assertEqual(process.returncode, run_status, process.stdout + process.stderr)
                # What the stall may cost is what the node does after its exit phase: keeping the
                # records and the summary within the retention budget, each command's 0.5 s kill
                # grace, and stopping the daemon. The node's steps before its exit (staging,
                # regenerating the Buck rules) are not under test, and their wall time is mostly
                # host load.
                after_exit = ended - self.exit_phase_time(results)
                self.assertLess(after_exit, AFTER_EXIT_BOUND_SECONDS, process.stderr)
                self.assertIn("validate-node: the stage summary of the re invocation is incomplete", process.stderr)
                self.assertEqual((self.kept(results) / "re.log").read_text(), RECORDS["re.log"])
                self.assertEqual(self.phases(results)[-1], f"exit {run_status}")
                self.assertEqual(kills.read_text(), "kill\n", "the buck2 daemon was not stopped")

    def interrupt(self, results, **added):
        """Start validate-node in its own process group, as the validation DAG does, and send the
        group SIGTERM once the run is under way; return the process and the seconds it took to
        die."""
        ready = self.root / "run-ready"
        ready.unlink(missing_ok=True)
        environment = self.node_environment(str(results), "buck-local",
                                            dict(FAKE_RUN_RECORDS=json.dumps(RECORDS), FAKE_RUN_READY=str(ready),
                                                 **added))
        with open(self.root / "node.stderr", "w") as stderr:
            process = subprocess.Popen([str(self.node)], stdin=subprocess.DEVNULL, stdout=stderr, stderr=stderr,
                                       env=environment, start_new_session=True)
        try:
            deadline = time.monotonic() + 60
            while not ready.exists():
                self.assertIsNone(process.poll(), (self.root / "node.stderr").read_text())
                self.assertLess(time.monotonic(), deadline, "the run never started")
                time.sleep(0.02)
            started = time.monotonic()
            os.killpg(process.pid, signal.SIGTERM)
            process.wait(timeout=30)
            return process, time.monotonic() - started
        finally:
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait()

    def test_a_group_sigterm_is_recorded_as_a_signal_and_the_node_still_dies_of_it(self):
        results = self.root / "e2e-results"
        kills = self.root / "kills"
        for name, added in (("summaries that work", {}), ("a summary that stalls", {"FAKE_BUCK2_LOG_SHOW_HANGS": "1"})):
            with self.subTest(name):
                kills.unlink(missing_ok=True)
                process, elapsed = self.interrupt(results, FAKE_BUCK2_KILLS=str(kills), **added)
                stderr = (self.root / "node.stderr").read_text()
                self.assertEqual(process.returncode, -signal.SIGTERM, stderr)
                # The validation DAG reaps the group 5 s after its SIGTERM.
                self.assertLess(elapsed, 5, stderr)
                phases = self.phases(results)
                self.assertEqual(phases[-1], "signal TERM")
                self.assertFalse([row for row in phases if row.startswith("exit ")], phases)
                self.assertEqual((self.kept(results) / "re.log").read_text(), RECORDS["re.log"])
                if added:
                    self.assertIn("validate-node: the stage summary of the re invocation is incomplete", stderr)
                else:
                    self.assertEqual((self.kept(results) / "re.stages.txt").read_text(),
                                     stages_test.StagesTest.summary(None, stages_test.two_cells()))
                self.assertEqual(kills.read_text(), "kill\n", "the buck2 daemon was not stopped")

    def run_as_step(self, results, wall, **added):
        """Run validate-node as the validation DAG runs a step with a WALL-second bound: in its own
        process group, with the start in DAGRUN_STEP_STARTED_MONOTONIC_NS and the bound in
        HERMIT_VALIDATE_BUCK_STEP_WALL_SECONDS. Fail if it is still running at the bound, where
        dagrun would mark it timed out and reap it; return the process and its seconds from the
        start."""
        started_ns = time.monotonic_ns()
        environment = self.node_environment(str(results), "buck-local", dict(
            FAKE_RUN_RECORDS=json.dumps(RECORDS), HERMIT_VALIDATE_BUCK_STEP_WALL_SECONDS=str(wall),
            DAGRUN_STEP_STARTED_MONOTONIC_NS=str(started_ns), **added))
        with open(self.root / "node.stderr", "w") as stderr:
            process = subprocess.Popen([str(self.node)], stdin=subprocess.DEVNULL, stdout=stderr, stderr=stderr,
                                       env=environment, start_new_session=True)
        try:
            process.wait(timeout=started_ns / 1e9 + wall - time.monotonic())
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGTERM)
            process.wait(timeout=30)
            self.fail(f"the node was still running at its {wall} s wall bound, so its completed work would be "
                      f"reported as a timeout:\n{(self.root / 'node.stderr').read_text()}")
        finally:
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait()
        return process, time.monotonic() - started_ns / 1e9

    def test_keeping_the_records_ends_inside_the_steps_wall_bound(self):
        # The step's bound less the 10 s reserve is shorter than the 30 s retention budget, so it
        # sets the deadline for keeping records; the summary stalls, so the node must cut it at
        # that deadline: not earlier (it uses the time it has) and not later (the retention
        # budget, or the bound itself, would follow). Every time is judged against the deadline
        # the node computes from its own exit, never against how long the node took to get
        # there: under load the steps before the exit take seconds (10.8 s in a box validation
        # of hermit 78c0eca2), which once spent the whole 3 s a 13 s bound leaves. A bound that
        # leaves less than a second at the exit tests nothing, so the next, longer one is tried.
        results = self.root / "e2e-results"
        kills = self.root / "kills"
        for run_status in (0, 7):
            with self.subTest(run_status=run_status):
                for wall in (13, 25, 60):
                    kills.unlink(missing_ok=True)
                    started = time.time()
                    process, _elapsed = self.run_as_step(results, wall, FAKE_RUN_RC=str(run_status),
                                                         FAKE_BUCK2_KILLS=str(kills), FAKE_BUCK2_LOG_SHOW_HANGS="1",
                                                         HERMIT_VALIDATE_BUCK_RETENTION_SECONDS="30")
                    ended = time.time()
                    stderr = (self.root / "node.stderr").read_text()
                    self.assertEqual(process.returncode, run_status, stderr)
                    deadline = started + wall - 10
                    if self.exit_time(results) < deadline - 1:
                        break
                else:
                    self.fail(f"the node reached its exit less than 1 s before the deadline of a {wall} s "
                              f"bound, so nothing was left to cut:\n{stderr}")
                # Cut at the deadline: not before it (less a little for the clocks read), and no
                # later than the half second the stalled command gets after its timeout, the
                # buck2 kill and the exit.
                self.assertGreater(ended, deadline - 0.5, stderr)
                self.assertLess(ended, deadline + 3, stderr)
                self.assertIn("validate-node: the stage summary of the re invocation is incomplete", stderr)
                self.assertIn(f"validate-node: the time left before this step's {wall} s wall bound, less 10 s "
                              "for stopping buck2, ran out before ", stderr)
                self.assertEqual((self.kept(results) / "re.log").read_text(), RECORDS["re.log"])
                self.assertEqual(self.phases(results)[-1], f"exit {run_status}")
                self.assertEqual(kills.read_text(), "kill\n", "the buck2 daemon was not stopped")

    def test_no_record_is_kept_once_the_steps_wall_bound_leaves_no_time(self):
        # A 9 s bound is inside the 10 s reserve from the start.
        results = self.root / "e2e-results"
        kills = self.root / "kills"
        for run_status in (0, 7):
            with self.subTest(run_status=run_status):
                kills.unlink(missing_ok=True)
                process, _elapsed = self.run_as_step(results, 9, FAKE_RUN_RC=str(run_status),
                                                     FAKE_BUCK2_KILLS=str(kills))
                stderr = (self.root / "node.stderr").read_text()
                self.assertEqual(process.returncode, run_status, stderr)
                self.assertIn("validate-node: the time left before this step's 9 s wall bound, less 10 s for "
                              "stopping buck2, ran out before ", stderr)
                self.assertEqual(sorted(p.name for p in self.kept(results).iterdir()), ["phases.tsv"])
                self.assertEqual(self.phases(results)[-1], f"exit {run_status}")
                self.assertEqual(kills.read_text(), "kill\n", "the buck2 daemon was not stopped")

    def test_the_retention_budget_still_bounds_the_records_inside_a_long_wall_bound(self):
        results = self.root / "e2e-results"
        process, _elapsed = self.run_as_step(results, 3600, FAKE_BUCK2_LOG_SHOW_HANGS="1",
                                             HERMIT_VALIDATE_BUCK_RETENTION_SECONDS="1")
        ended = time.time()
        stderr = (self.root / "node.stderr").read_text()
        self.assertEqual(process.returncode, 0, stderr)
        # The 1 s budget counts from the node's exit, however long it took to get there; the
        # stalled summary then gets half a second after its timeout, then the buck2 kill.
        self.assertLess(ended - self.exit_time(results), 1 + 3, stderr)
        self.assertIn("validate-node: the 1 s for keeping the Buck invocation records ran out before ", stderr)

    def test_a_step_wall_bound_that_is_not_a_positive_number_of_seconds_is_refused(self):
        for value in ("0", "-1", "1.5", "soon"):
            with self.subTest(value=value):
                process = self.validate_node(str(self.root / "e2e-results"),
                                             HERMIT_VALIDATE_BUCK_STEP_WALL_SECONDS=value,
                                             DAGRUN_STEP_STARTED_MONOTONIC_NS=str(time.monotonic_ns()))
                self.assertEqual(process.returncode, 2, process.stdout + process.stderr)
                self.assertIn("HERMIT_VALIDATE_BUCK_STEP_WALL_SECONDS is '", process.stderr)
                self.assertFalse(self.calls.exists(), "a step ran before the node was refused")

    def test_a_step_wall_bound_without_the_steps_start_is_refused(self):
        later = str(time.monotonic_ns() + 3600 * 10**9)
        for start in (None, "", "soon", "-1", "1" * 19, later):
            with self.subTest(start=start):
                added = {"HERMIT_VALIDATE_BUCK_STEP_WALL_SECONDS": "3600"}
                if start is not None:
                    added["DAGRUN_STEP_STARTED_MONOTONIC_NS"] = start
                process = self.validate_node(str(self.root / "e2e-results"), **added)
                self.assertEqual(process.returncode, 2, process.stdout + process.stderr)
                self.assertIn("HERMIT_VALIDATE_BUCK_STEP_WALL_SECONDS is set, but DAGRUN_STEP_STARTED_MONOTONIC_NS",
                              process.stderr)
                self.assertFalse(self.calls.exists(), "a step ran before the node was refused")

    def slow_clock_python3(self, seconds, then):
        """Put a python3 first on the node's PATH that, for the node's clock read alone (the
        only `python3 -c` naming monotonic_ns), sleeps SECONDS and then runs the shell command
        THEN instead; every other use runs the real interpreter."""
        real = shutil.which("python3")
        write_executable(self.root / "bin" / "python3",
                         f'#!/bin/sh\ncase "$*" in *monotonic_ns*) sleep {seconds}; {then} ;; esac\n'
                         f'exec {shlex.quote(real)} "$@"\n')

    def test_a_clock_read_slowed_by_a_loaded_host_still_starts_the_step(self):
        # At a load of about 450, hermit 32e1c019's validation refused the node because the
        # interpreter reading CLOCK_MONOTONIC did not start within the 2 s it was given.
        self.slow_clock_python3(3, ":")
        process, _elapsed = self.run_as_step(self.root / "e2e-results", 3600)
        stderr = (self.root / "node.stderr").read_text()
        self.assertEqual(process.returncode, 0, stderr)
        self.assertEqual([call[0] for call in calls_in(self.calls)], ["regenerate-rust-deps", "stage", "run"])
        self.assertEqual(self.phases(self.root / "e2e-results")[-1], "exit 0")

    def test_a_clock_that_cannot_be_read_is_refused_by_name(self):
        for name, then in (("a failed interpreter", "exit 1"), ("no reading", "exit 0"),
                           ("one clock only", "echo 5; exit 0"), ("not a number", "echo soon 5; exit 0")):
            with self.subTest(name):
                self.slow_clock_python3(0, then)
                process = self.validate_node(str(self.root / "e2e-results"),
                                             HERMIT_VALIDATE_BUCK_STEP_WALL_SECONDS="3600",
                                             DAGRUN_STEP_STARTED_MONOTONIC_NS=str(time.monotonic_ns()))
                self.assertEqual(process.returncode, 2, process.stdout + process.stderr)
                self.assertIn("HERMIT_VALIDATE_BUCK_STEP_WALL_SECONDS is set, but CLOCK_MONOTONIC could not be "
                              "read: python3 exited ", process.stderr)
                self.assertNotIn("not a time on CLOCK_MONOTONIC that has passed", process.stderr)
                self.assertFalse(self.calls.exists(), "a step ran before the node was refused")

    def test_an_earlier_attempts_records_are_never_kept_for_a_step_that_fails_before_the_run(self):
        results = self.root / "e2e-results"
        work = self.root / "state" / "buck-e2e" / "work"
        work.mkdir(parents=True)
        for name, text in RECORDS.items():
            (work / name).write_text(text)
        (work / "unrelated.txt").write_text("not a record\n")
        process = self.validate_node(str(results), FAKE_FAIL_STEP="regenerate-rust-deps")
        self.assertEqual(process.returncode, 17, process.stdout + process.stderr)
        self.assertEqual([call[0] for call in calls_in(self.calls)], ["regenerate-rust-deps"])
        self.assertEqual(sorted(p.name for p in self.kept(results).iterdir()), ["phases.tsv"])
        self.assertEqual(self.phases(results), ["regenerating the Buck third-party rules", "exit 17"])
        self.assertEqual(sorted(p.name for p in work.iterdir()), ["unrelated.txt"])

    def test_an_earlier_attempts_records_that_cannot_be_removed_are_not_kept(self):
        results = self.root / "e2e-results"
        work = self.root / "state" / "buck-e2e" / "work"
        work.mkdir(parents=True)
        (work / "re.log").write_text("earlier\n")
        work.chmod(0o555)
        self.addCleanup(work.chmod, 0o755)
        process = self.validate_node(str(results), FAKE_FAIL_STEP="regenerate-rust-deps")
        self.assertEqual(process.returncode, 17, process.stdout + process.stderr)
        self.assertIn("cannot remove an earlier attempt's Buck invocation records", process.stderr)
        self.assertEqual(list(self.kept(results).iterdir()), [])

    def test_a_retention_budget_that_is_not_a_positive_number_of_seconds_is_refused(self):
        for value in ("0", "-1", "1.5", "soon"):
            with self.subTest(value=value):
                process = self.validate_node(str(self.root / "e2e-results"),
                                             HERMIT_VALIDATE_BUCK_RETENTION_SECONDS=value)
                self.assertEqual(process.returncode, 2, process.stdout + process.stderr)
                self.assertIn("HERMIT_VALIDATE_BUCK_RETENTION_SECONDS is", process.stderr)
                self.assertFalse(self.calls.exists(), "a step ran before the node was refused")

    def test_an_earlier_record_that_cannot_be_removed_costs_only_the_records(self):
        results = self.root / "e2e-results"
        stuck = self.kept(results) / "stuck"
        stuck.mkdir(parents=True)
        (stuck / "re.log").write_text("earlier\n")
        stuck.chmod(0o555)
        self.addCleanup(stuck.chmod, 0o755)
        process = self.validate_node(str(results), FAKE_RUN_RECORDS=json.dumps(RECORDS))
        self.assertEqual(process.returncode, 0, process.stdout + process.stderr)
        self.assertIn("this run keeps no Buck invocation records", process.stderr)
        self.assertEqual(len([call for call in calls_in(self.calls) if call[0] == "run"]), 1)
        self.assertEqual(sorted(p.name for p in self.kept(results).iterdir()), ["stuck"])

    def test_a_relative_e2e_result_root_is_resolved_before_the_node_changes_directory(self):
        process = self.validate_node("relative-results", cwd=self.root)
        self.assertEqual(process.returncode, 0, process.stdout + process.stderr)
        argv = [call for call in calls_in(self.calls) if call[0] == "run"][0]
        self.assertEqual(argv[argv.index("--failed-verify-logs") + 1],
                         str(self.root / "relative-results" / "buck-failed-verify-logs"))

    def test_a_node_without_an_e2e_result_root_is_refused(self):
        process = self.validate_node(None)
        self.assertEqual(process.returncode, 2, process.stdout + process.stderr)
        self.assertIn("validate-node: E2E_RESULT_ROOT is unset", process.stderr)
        self.assertFalse(self.calls.exists(), "a step ran before the node was refused")


if __name__ == "__main__":
    unittest.main()
