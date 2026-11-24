#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

"""Tests ci/buck-build-compare.py on planted run directories: equal runs pass; every changed typed
outcome, lost test and plan difference is named; every run that cannot be compared is refused."""

from __future__ import annotations

import copy
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

COMPARE = Path(__file__).resolve().parent / "buck-build-compare.py"
PROFILE_HEADER = "step,user_s,sys_s,run_id\n"
COUNTED = {"HERMIT_NEXTEST_RESULTS_SCHEMA": "5"}
UNIT = {"buck_daemon": 1}
PLAN = {
    "build.workspace_compile_in_pinned_root": ({}, {}),
    "build.reverie_buck": ({}, UNIT),
    "build.buck_release_artifact": ({}, UNIT),
    "test.detcore_unit": (COUNTED, {}),
    "test.cli": (COUNTED, {}),
}


def row(test: str, kind: str = "pass", attempts: int = 1, cause: str = "failed", reason: str = "no /dev/kvm") -> dict:
    """One schema-5 result row, as ci/nextest-test-results.rs writes it."""
    earlier = [{"attempt": n, "outcome": "failed", "detail": "retried"} for n in range(1, attempts)]
    last = {"pass": ("passed", None), "fail": (cause, "it broke"), "skip": ("skipped", reason),
            "diagnostic_fail": ("failed", "it broke")}[kind]
    return {"id": test, "result": kind, "attempts": attempts,
            "attempt_results": earlier + [{"attempt": attempts, "outcome": last[0], "detail": last[1]}],
            "diagnostic_reason": "known gap" if kind == "diagnostic_fail" else None,
            "skip_reason": reason if kind == "skip" else None}


def sidecar(node: str, placement: str = "step_cgroup", actions_measured: bool = True) -> dict:
    daemon = {"step_cgroup": {"placement": "step_cgroup", "cgroup": "/x/step", "node_cgroup": "/x/step", "pid": 9},
              "scope": {"placement": "scope", "scope": "/sys/fs/cgroup/x.scope", "cpu_usec": 3_000_000,
                        "user_usec": 2_000_000, "system_usec": 1_000_000, "memory_peak_bytes": 1 << 30},
              "unmeasured": {"placement": "unmeasured", "reason": "`buck2 status` exited 124"}}[placement]
    actions = ({"measured": True, "event_logs": 1, "decoded_bytes": 100, "total": 10, "cached": 4,
                "by_kind": {"local": 6, "action_cache": 4}} if actions_measured
               else {"measured": False, "reason": "a log was cut", "decoded_bytes": 10})
    return {"schema": "hermit-buck-measure/v2", "node": node, "attempt": "5", "buck2": {"version": "x"},
            "daemon": daemon, "bootstraps": [{"name": "regenerate-rust-deps", "rc": 0, "user_s": 1.0, "sys_s": 0.5, "measured": True}],
            "actions": actions, "outputs": {"measured": True, "files": []},
            "disk": {"measured": True, "buck_out_apparent_bytes": 10, "buck_out_allocated_bytes": 4096},
            "cache": {"configured": False}}


class CompareTest(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory(prefix="test-buck-build-compare-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.tests = [row("suite$a"), row("suite$b"), row("suite$kvm", "skip")]
        self.a = self.make_run("a", {"build_mode": "cargo", "release_builder": "cargo"},
                               {"build.workspace_compile_in_pinned_root": 7900.0, "build.reverie_buck": 0.1,
                                "build.buck_release_artifact": 0.1, "test.detcore_unit": 100.0, "test.cli": 50.0})
        self.b = self.make_run("b", {"build_mode": "buck", "release_builder": "cargo"},
                               {"build.workspace_compile_in_pinned_root": 7000.0, "build.reverie_buck": 50.0,
                                "build.buck_release_artifact": 0.1, "test.detcore_unit": 100.0, "test.cli": 50.0})
        self.write_sidecar(self.b, sidecar("build.reverie_buck"))
        for run, attempt in ((self.a, "100"), (self.b, "200")):
            for step in ("test.detcore_unit", "test.cli"):
                self.write_results(run, step, attempt, self.tests)

    def make_run(self, name: str, row_fields: dict, cpu: dict[str, float], plan: dict | None = None,
                 retries: dict[str, int] | None = None) -> Path:
        run = self.root / name
        (run / "dagrun-profiles").mkdir(parents=True)
        (run / "dagrun-profiles" / "profile.csv").write_text(
            PROFILE_HEADER + "".join(f"{step},{seconds * 0.8},{seconds * 0.2},r1\n" for step, seconds in cpu.items()))
        self.write_plan(run, PLAN if plan is None else plan)
        gates = [{"name": step, "retries": (retries or {}).get(step, 0), "result": "pass"} for step in cpu]
        (run / "history-row.json").write_text(json.dumps({"gates": gates, **row_fields}))
        return run

    def write_plan(self, run: Path, plan: dict) -> None:
        steps = [{"group": tag.split(".", 1)[0], "job": tag.split(".", 1)[1], "env": env, "hint": {"resources": resources},
                  "cmd": ":"} for tag, (env, resources) in plan.items()]
        (run / "constructed-plan.json").write_text(json.dumps({"schema": 1, "dag_json": json.dumps({"steps": steps})}))

    def edit_row(self, run: Path, edit) -> None:
        path = run / "history-row.json"
        record = json.loads(path.read_text())
        edit(record)
        path.write_text(json.dumps(record))

    def write_results(self, run: Path, step: str, attempt: str, rows: list[dict] | None, selection=None) -> None:
        directory = run / "e2e" / "nextest-events"
        directory.mkdir(parents=True, exist_ok=True)
        (directory / f"{step}.{attempt}.started").write_text("")
        (directory / f"{step}.{attempt}.jsonl").write_text('{"type":"suite","event":"started"}\n')
        if rows is not None:
            (directory / f"{step}.{attempt}.results.json").write_text(json.dumps(
                {"schema": 5, "executed_tests": len(rows), "filtered_tests": 0, "results": rows, "selection": selection}))

    def write_sidecar(self, run: Path, record: dict | str) -> None:
        directory = run / "e2e" / "buck-measure"
        directory.mkdir(parents=True, exist_ok=True)
        text = record if isinstance(record, str) else json.dumps(record)
        (directory / f"{json.loads(text)['node'] if not isinstance(record, str) else 'build.reverie_buck'}.json").write_text(text)

    def compare(self) -> subprocess.CompletedProcess:
        return subprocess.run([sys.executable, "-B", str(COMPARE), str(self.a), str(self.b)],
                              capture_output=True, text=True, timeout=60)

    def assert_refused(self, needle: str) -> None:
        process = self.compare()
        self.assertEqual(process.returncode, 2, process.stdout + process.stderr)
        self.assertIn(needle, process.stderr)

    def assert_difference(self, line: str) -> None:
        process = self.compare()
        self.assertEqual(process.returncode, 1, process.stdout + process.stderr)
        self.assertIn(line, process.stdout)

    def test_equal_runs_compare_equal_and_print_both_cost_tables(self):
        process = self.compare()
        self.assertEqual(process.returncode, 0, process.stdout + process.stderr)
        self.assertIn("Build steps (build.*, setup.*)", process.stdout)
        self.assertIn("ran no buck2 (build_mode cargo)", process.stdout)
        self.assertIn("ran no buck2 (release_builder cargo)", process.stdout)
        self.assertRegex(process.stdout, r"B build\.reverie_buck +CPU-s +50\.0  daemon in cgroup  bootstraps 1\.5"
                                         r"  actions 10, cache hits 4 \(40\.0%\)")
        # 7900 + 0.1 + 0.1 against 7000 + 50 + 0.1: the daemon in the cgroup adds nothing.
        self.assertIn("total: A 7900.2  B 7050.1  B/A 0.892", process.stdout)
        self.assertIn("2 counted steps in both plans, 6 tests; 0 differences, 0 retry differences", process.stdout)
        self.assertNotIn("Buck E2E steps", process.stdout)

    def test_pass_to_skip_and_skip_to_pass_are_differences(self):
        self.write_results(self.b, "test.cli", "201", [row("suite$a", "skip", reason="feature liteinst is off"),
                                                       row("suite$b"), row("suite$kvm")])
        self.edit_row(self.b, lambda record: next(g for g in record["gates"] if g["name"] == "test.cli").update(retries=1))
        process = self.compare()
        self.assertEqual(process.returncode, 1, process.stdout + process.stderr)
        self.assertIn("test.cli: suite$a: A passed, B skipped (feature liteinst is off)", process.stdout)
        self.assertIn("test.cli: suite$kvm: A skipped (no /dev/kvm), B passed", process.stdout)

    def test_a_test_absent_from_either_run_is_a_difference(self):
        self.write_results(self.b, "test.cli", "201", self.tests[1:])
        self.edit_row(self.b, lambda record: next(g for g in record["gates"] if g["name"] == "test.cli").update(retries=1))
        self.assert_difference("test.cli: suite$a: A passed, B absent")
        self.write_results(self.a, "test.cli", "101", self.tests + [row("suite$new")])
        self.edit_row(self.a, lambda record: next(g for g in record["gates"] if g["name"] == "test.cli").update(retries=1))
        self.assert_difference("test.cli: suite$new: A passed, B absent")

    def test_a_pass_after_retries_is_flaky_not_a_difference(self):
        self.write_results(self.b, "test.cli", "201", [row("suite$a", attempts=3)] + self.tests[1:])
        self.edit_row(self.b, lambda record: next(g for g in record["gates"] if g["name"] == "test.cli").update(retries=1))
        process = self.compare()
        self.assertEqual(process.returncode, 0, process.stdout + process.stderr)
        self.assertIn("0 differences, 1 retry differences", process.stdout)
        self.assertIn("test.cli: suite$a: A passed after 1 attempt(s), B after 3", process.stdout)

    def test_the_same_failure_or_skip_after_more_retries_is_not_a_difference(self):
        # A review finding on the first landed version: the attempt count must not make an
        # identical outcome a difference.
        self.write_results(self.a, "test.cli", "101", [row("suite$a", "fail"), row("suite$b"),
                                                       row("suite$kvm", "skip")])
        self.edit_row(self.a, lambda record: next(g for g in record["gates"] if g["name"] == "test.cli").update(retries=1))
        self.write_results(self.b, "test.cli", "201", [row("suite$a", "fail", attempts=3), row("suite$b"),
                                                       row("suite$kvm", "skip", attempts=2)])
        self.edit_row(self.b, lambda record: next(g for g in record["gates"] if g["name"] == "test.cli").update(retries=1))
        process = self.compare()
        self.assertEqual(process.returncode, 0, process.stdout + process.stderr)
        self.assertIn("0 differences, 2 retry differences", process.stdout)
        self.assertIn("test.cli: suite$a: A failed (failed) after 1 attempt(s), B after 3", process.stdout)
        self.assertIn("test.cli: suite$kvm: A skipped (no /dev/kvm) after 1 attempt(s), B after 2", process.stdout)
        # A different cause after more retries is still a difference.
        self.write_results(self.b, "test.cli", "202", [row("suite$a", "fail", attempts=3, cause="cpu_timeout"),
                                                       row("suite$b"), row("suite$kvm", "skip", attempts=2)])
        self.edit_row(self.b, lambda record: next(g for g in record["gates"] if g["name"] == "test.cli").update(retries=2))
        self.assert_difference("test.cli: suite$a: A failed (failed), B failed (cpu_timeout)")

    def test_a_counted_step_that_executed_zero_tests_is_refused(self):
        # The census never passes a counted node that executed nothing, so zero
        # tests in either run is no result, never an equal comparison. Zero in B only, in A only
        # (B's last attempt has its tests again), then in both.
        self.write_results(self.b, "test.cli", "201", [])
        self.edit_row(self.b, lambda record: next(g for g in record["gates"] if g["name"] == "test.cli").update(retries=1))
        self.assert_refused("test.cli.201.results.json: counted step test.cli's last attempt executed zero tests: no result")
        self.write_results(self.b, "test.cli", "201", self.tests)
        self.write_results(self.a, "test.cli", "101", [])
        self.edit_row(self.a, lambda record: next(g for g in record["gates"] if g["name"] == "test.cli").update(retries=1))
        self.assert_refused("test.cli.101.results.json: counted step test.cli's last attempt executed zero tests: no result")
        self.write_results(self.b, "test.cli", "201", [])
        self.assert_refused("executed zero tests: no result")

    def test_a_changed_failure_or_a_failure_that_passes_is_a_difference(self):
        self.write_results(self.a, "test.cli", "101", [row("suite$a", "fail")] + self.tests[1:])
        self.edit_row(self.a, lambda record: next(g for g in record["gates"] if g["name"] == "test.cli").update(retries=1))
        self.assert_difference("test.cli: suite$a: A failed (failed), B passed")
        self.write_results(self.b, "test.cli", "201", [row("suite$a", "fail", cause="cpu_timeout")] + self.tests[1:])
        self.edit_row(self.b, lambda record: next(g for g in record["gates"] if g["name"] == "test.cli").update(retries=1))
        self.assert_difference("test.cli: suite$a: A failed (failed), B failed (cpu_timeout)")

    def test_a_counted_step_in_one_plan_only_or_a_changed_selection_is_a_difference(self):
        plan = dict(PLAN, **{"test.extra": (COUNTED, {})})
        self.write_plan(self.b, plan)
        self.assert_difference("counted step test.extra is in B's plan only")
        self.write_plan(self.b, PLAN)
        self.write_results(self.b, "test.cli", "201", self.tests, selection={"expression": "test(=suite$a)",
                                                                             "focused_tests": [], "disabled_in_config": [],
                                                                             "disabled_in_config_source": None})
        self.edit_row(self.b, lambda record: next(g for g in record["gates"] if g["name"] == "test.cli").update(retries=1))
        self.assert_difference("test.cli: the selection changed")

    def test_records_of_steps_outside_the_plan_are_listed_and_not_read(self):
        self.write_results(self.a, "fixture.writable-pass", "300", None)
        process = self.compare()
        self.assertEqual(process.returncode, 0, process.stdout + process.stderr)
        self.assertIn("Not read (A): kept records of steps not in the run's plan: fixture.writable-pass", process.stdout)

    def test_a_scope_daemon_adds_its_cpu_and_buck_e2e_steps_get_their_own_table(self):
        self.write_sidecar(self.b, sidecar("build.reverie_buck", placement="scope"))
        self.write_plan(self.b, dict(PLAN, **{"e2e.buck_cells": ({}, UNIT)}))
        with (self.b / "dagrun-profiles" / "profile.csv").open("a") as handle:
            handle.write("e2e.buck_cells,400,100,r1\n")
        self.edit_row(self.b, lambda record: record["gates"].append({"name": "e2e.buck_cells", "retries": 0}))
        self.write_sidecar(self.b, sidecar("e2e.buck_cells"))
        process = self.compare()
        self.assertEqual(process.returncode, 0, process.stdout + process.stderr)
        self.assertIn("total: A 7900.2  B 7053.1", process.stdout)
        self.assertIn("daemon scope 3.0", process.stdout)
        self.assertIn("Buck E2E steps (e2e.buck_*)", process.stdout)
        self.assertRegex(process.stdout, r"B e2e\.buck_cells +CPU-s +500\.0")
        self.assertIn("total: A 0.0  B 500.0  B/A n/a", process.stdout)

    # Refusals: a run that cannot be compared as recorded exits 2, naming why.

    def test_runs_sharing_no_counted_step_are_refused(self):
        plan = {tag: spec for tag, spec in PLAN.items() if not tag.startswith("test.")}
        self.write_plan(self.a, plan)
        self.write_plan(self.b, plan)
        self.assert_refused("the runs share no counted step")

    def test_a_counted_step_that_did_not_run_is_refused(self):
        self.edit_row(self.b, lambda record: record.update(gates=[g for g in record["gates"] if g["name"] != "test.cli"]))
        self.assert_refused("counted step test.cli did not run")

    def test_a_last_attempt_without_results_or_without_a_marker_is_refused(self):
        self.edit_row(self.b, lambda record: next(g for g in record["gates"] if g["name"] == "test.cli").update(retries=1))
        self.assert_refused("made 2 attempt(s) (history-row.json) and 1 left a start marker")
        self.write_results(self.b, "test.cli", "201", None)
        self.assert_refused("test.cli.201.results.json is missing: counted step test.cli's last attempt published no typed results")
        (self.b / "e2e/nextest-events/test.cli.202.jsonl").write_text("")
        self.assert_refused("test.cli kept records of attempt 202 with no start marker")

    def test_an_attempt_that_cannot_be_ordered_is_refused(self):
        self.write_results(self.b, "test.cli", "1700000000-42", self.tests)
        self.assert_refused("is not DAGRUN_STEP_STARTED_MONOTONIC_NS")

    def test_malformed_results_are_refused_never_compared(self):
        cases = [
            ({"schema": 3, "executed_tests": 0, "filtered_tests": 0, "results": []}, "only schema 5 types a runtime skip"),
            ({"schema": 5, "executed_tests": 2, "filtered_tests": 0, "results": [row("x")]}, "one row per executed test"),
            ({"schema": 5, "executed_tests": 2, "filtered_tests": 0, "results": [row("x"), row("x")]}, "appears twice"),
            ({"schema": 5, "executed_tests": 1, "filtered_tests": 0, "results": [dict(row("x", "skip"), skip_reason=None)]},
             "result 'skip' with skip_reason None"),
            ({"schema": 5, "executed_tests": 1, "filtered_tests": 0, "results": [dict(row("x"), result="maybe")]},
             "unknown result 'maybe'"),
            ({"schema": 5, "executed_tests": 1, "filtered_tests": 0, "results": [dict(row("x"), attempts=2)]},
             "does not hold one row per attempt"),
            ("not json", "unreadable or not JSON"),
        ]
        for document, needle in cases:
            with self.subTest(needle=needle):
                path = self.b / "e2e/nextest-events/test.cli.200.results.json"
                path.write_text(document if isinstance(document, str) else json.dumps(document))
                self.assert_refused(needle)

    def test_a_buck_step_that_ran_without_a_whole_measurement_is_refused(self):
        (self.b / "e2e/buck-measure/build.reverie_buck.json").unlink()
        self.assert_refused("build.reverie_buck holds the buck_daemon unit and ran, but left no measurement sidecar")
        self.write_sidecar(self.b, sidecar("build.reverie_buck", placement="unmeasured"))
        self.assert_refused("build.reverie_buck's buck2 daemon is unmeasured: `buck2 status` exited 124")
        self.write_sidecar(self.b, sidecar("build.reverie_buck", actions_measured=False))
        self.assert_refused("build.reverie_buck's actions are unmeasured: a log was cut")
        record = sidecar("build.reverie_buck")
        record["schema"] = "hermit-buck-measure/v1"
        self.write_sidecar(self.b, record)
        self.assert_refused("not a hermit-buck-measure/v2 sidecar")
        self.write_sidecar(self.b, '{"node": ')
        self.assert_refused("unreadable or not JSON")

    def test_a_buck_step_that_ran_twice_is_refused(self):
        self.edit_row(self.b, lambda record: next(g for g in record["gates"] if g["name"] == "build.reverie_buck").update(retries=1))
        self.assert_refused("Buck step build.reverie_buck ran 2 times, so its cost is not one build's")

    def test_a_stray_sidecar_or_an_unknown_mode_is_refused(self):
        self.write_sidecar(self.a, sidecar("build.reverie_buck"))
        self.assert_refused("a sidecar from build.reverie_buck, which ran no buck2 (build_mode cargo)")
        (self.a / "e2e/buck-measure/build.reverie_buck.json").unlink()
        self.write_sidecar(self.a, sidecar("build.workspace_compile_in_pinned_root"))
        self.assert_refused("which holds no buck_daemon unit")
        (self.a / "e2e/buck-measure/build.workspace_compile_in_pinned_root.json").unlink()
        self.edit_row(self.a, lambda record: record.pop("build_mode"))
        self.assert_refused("build_mode is None, so whether build.reverie_buck ran buck2 is unknown")

    def test_malformed_run_records_are_refused_never_a_difference(self):
        for name, text, needle in (("history-row.json", "[]", "history-row.json: no gates"),
                                   ("history-row.json", '{"gates": [{"name": "x"}]}', "a gate without a name and a retry count"),
                                   ("constructed-plan.json", '{"dag_json": "{\\"steps\\": [{}]}"}', "a step without a group and a job"),
                                   ("constructed-plan.json", "\xff", "unreadable or not JSON")):
            with self.subTest(needle=needle):
                path = self.b / name
                original = path.read_bytes()
                path.write_bytes(text.encode("latin-1"))
                self.assert_refused(needle)
                path.write_bytes(original)
        profile = self.b / "dagrun-profiles/profile.csv"
        lines = [line if not line.startswith("build.reverie_buck,") else "build.reverie_buck,,,r1"
                 for line in profile.read_text().splitlines()]
        profile.write_text("\n".join(lines) + "\n")
        self.assert_refused("build.reverie_buck's user_s and sys_s are not numbers")

    def test_the_planted_outcomes_are_the_rows_the_writer_would_write(self):
        # Guard the fixture: each planted row is internally consistent (a pass ends passed, a skip
        # ends skipped with a reason), so the cases above test the comparison, not a broken row.
        for kind in ("pass", "fail", "skip", "diagnostic_fail"):
            planted = row("x", kind, attempts=2)
            self.assertEqual(len(planted["attempt_results"]), planted["attempts"])
            self.assertEqual(planted["skip_reason"] is not None, kind == "skip")
        self.assertEqual(copy.deepcopy(self.tests)[2]["attempt_results"][-1]["outcome"], "skipped")


if __name__ == "__main__":
    unittest.main()
