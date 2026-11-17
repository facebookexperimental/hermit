#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

"""Unit tests for ci/buck-e2e/cell-stock.py, the stock-runner cell wrapper.

The wrapper's whole job is mapping cell.sh's TPX-shaped claim onto a
process exit code the stock runner cannot misread, so these tests pin
that mapping on synthetic claims (the live cells are exercised by the
stock targets themselves): a pass claim needs complete published
evidence, every other claim keeps the buck2 invocation red, and a
host-inapplicable claim exits 3 with its reason rather than passing.
"""

from __future__ import annotations

import importlib.machinery
import importlib.util
import json
import os
import stat
import tempfile
import unittest
from pathlib import Path
from unittest import mock


ROOT = Path(__file__).resolve().parent.parent
LOADER = importlib.machinery.SourceFileLoader(
    "cell_stock", str(ROOT / "ci" / "buck-e2e" / "cell-stock.py")
)
SPEC = importlib.util.spec_from_loader("cell_stock", LOADER)
cell_stock = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(cell_stock)

CELL = "c-programs/getcpu/verify@ptrace"
VALID = {"version": 1, "status": "valid", "reason": None}


def record(status, outcome, cell=CELL):
    details = json.dumps({"outcome": outcome, "reason": "fixture"})
    return json.dumps(
        {"op": "test_done", "test": cell, "status": status, "details": details}
    )


class ClassifyTest(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.artifacts = Path(self.dir.name)
        (self.artifacts / "run_id.fixture-run").write_text("fixture-run\n")
        self.write_result()

    def write_result(self, **changes):
        result = {
            "cell": CELL,
            "run_id": "fixture-run",
            "outcome": "PASS",
            "evidence_complete": True,
            "producer_completion": VALID,
        }
        result.update(changes)
        (self.artifacts / "result.json").write_text(json.dumps(result))

    def tearDown(self):
        self.dir.cleanup()

    def test_pass_with_complete_evidence_exits_zero(self):
        code, verdict = cell_stock.classify(record("passed", "PASS"), 0, self.artifacts)
        self.assertEqual(code, 0)
        self.assertEqual(verdict, "PASS")

    def test_details_after_a_human_summary_are_parsed(self):
        published = record("passed", "PASS")
        parsed = json.loads(published)
        parsed["details"] = (
            "c-programs/getcpu/verify@ptrace: PASS (all 3 checks passed)\n"
            + parsed["details"]
        )
        code, _ = cell_stock.classify(json.dumps(parsed), 0, self.artifacts)
        self.assertEqual(code, 0)

    def test_pass_without_complete_evidence_is_an_error(self):
        (self.artifacts / "result.json").unlink()
        code, verdict = cell_stock.classify(record("passed", "PASS"), 0, self.artifacts)
        self.assertEqual(code, 2)
        self.assertIn("without complete evidence", verdict)

    def test_failed_claim_exits_one(self):
        code, verdict = cell_stock.classify(record("failed", "FAIL"), 0, self.artifacts)
        self.assertEqual(code, 1)
        self.assertIn("FAIL", verdict)

    def test_host_inapplicable_fails_closed_with_its_reason(self):
        code, verdict = cell_stock.classify(
            record("skipped", "HOST-INAPPLICABLE"), 0, self.artifacts
        )
        self.assertEqual(code, 3)
        self.assertIn("HOST-INAPPLICABLE", verdict)
        self.assertIn("fixture", verdict)

    def test_skipped_fail_is_a_failure_not_a_skip(self):
        code, _ = cell_stock.classify(record("skipped", "FAIL"), 0, self.artifacts)
        self.assertEqual(code, 1)

    def test_protocol_violations_are_errors(self):
        two = record("passed", "PASS") + "\n" + record("passed", "PASS")
        code, _ = cell_stock.classify(two, 0, self.artifacts)
        self.assertEqual(code, 2)
        code, _ = cell_stock.classify(record("passed", "PASS"), 1, self.artifacts)
        self.assertEqual(code, 2)

    def test_no_record_is_an_error(self):
        code, verdict = cell_stock.classify("no json here\n", 0, self.artifacts)
        self.assertEqual(code, 2)
        self.assertIn("expected exactly one test_done record, saw 0", verdict)

    def test_unknown_status_is_an_error(self):
        code, _ = cell_stock.classify(record("melted", "PASS"), 0, self.artifacts)
        self.assertEqual(code, 2)

    def test_non_json_details_are_an_error(self):
        bad = json.dumps(
            {"op": "test_done", "test": CELL, "status": "passed", "details": "nope {"}
        )
        code, _ = cell_stock.classify(bad, 0, self.artifacts)
        self.assertEqual(code, 2)

    def test_passed_without_pass_outcome_is_an_error(self):
        code, _ = cell_stock.classify(record("passed", "FAIL"), 0, self.artifacts)
        self.assertEqual(code, 2)

    def test_skipped_with_unknown_outcome_is_an_error(self):
        code, _ = cell_stock.classify(record("skipped", "WOBBLE"), 0, self.artifacts)
        self.assertEqual(code, 2)

    def test_pass_for_another_cell_is_refused(self):
        code, verdict = cell_stock.classify(
            record("passed", "PASS", cell="c-programs/other/verify@ptrace"),
            0,
            self.artifacts,
        )
        self.assertEqual(code, 2)
        self.assertIn("without complete evidence", verdict)

    def test_incomplete_evidence_is_refused(self):
        self.write_result(evidence_complete=False)
        code, verdict = cell_stock.classify(record("passed", "PASS"), 0, self.artifacts)
        self.assertEqual(code, 2)
        self.assertIn("evidence_complete is not true", verdict)

    def test_invalid_producer_completion_is_refused(self):
        self.write_result(producer_completion={"status": "invalid"})
        code, verdict = cell_stock.classify(record("passed", "PASS"), 0, self.artifacts)
        self.assertEqual(code, 2)
        self.assertIn("no valid producer completion", verdict)

    def test_missing_producer_completion_is_refused(self):
        self.write_result(producer_completion=None)
        code, _ = cell_stock.classify(record("passed", "PASS"), 0, self.artifacts)
        self.assertEqual(code, 2)

    def test_result_outcome_other_than_pass_is_refused(self):
        self.write_result(outcome="FAIL")
        code, verdict = cell_stock.classify(record("passed", "PASS"), 0, self.artifacts)
        self.assertEqual(code, 2)
        self.assertIn("without complete evidence", verdict)

    def test_marker_mismatch_is_refused(self):
        self.write_result(run_id="some-earlier-run")
        code, verdict = cell_stock.classify(record("passed", "PASS"), 0, self.artifacts)
        self.assertEqual(code, 2)
        self.assertIn("run id markers", verdict)

    def test_extra_stale_marker_is_refused(self):
        (self.artifacts / "run_id.stale").write_text("stale\n")
        code, _ = cell_stock.classify(record("passed", "PASS"), 0, self.artifacts)
        self.assertEqual(code, 2)

    def test_result_json_that_is_not_an_object_is_an_error(self):
        (self.artifacts / "result.json").write_text('["not", "an", "object"]')
        code, verdict = cell_stock.classify(record("passed", "PASS"), 0, self.artifacts)
        self.assertEqual(code, 2)
        self.assertIn("without complete evidence", verdict)


class MainTest(unittest.TestCase):
    def test_main_runs_a_fake_cell_and_keeps_its_artifacts(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "ci" / "buck-e2e").mkdir(parents=True)
            (root / "ci" / "buck-e2e" / "ingest.py").write_text("# root marker\n")
            fake = root / "fake-cell.sh"
            fake.write_text(
                "#!/bin/bash\n"
                'printf \'{"cell": "c-programs/getcpu/verify@ptrace", '
                '"run_id": "fixture-run", "outcome": "PASS", "evidence_complete": true, '
                '"producer_completion": {"status": "valid"}}\' '
                '> "$TEST_RESULT_ARTIFACTS_DIR/result.json"\n'
                'printf "fixture-run\\n" > "$TEST_RESULT_ARTIFACTS_DIR/run_id.fixture-run"\n'
                "cat <<'JSON'\n"
                + record("passed", "PASS")
                + "\nJSON\n"
            )
            fake.chmod(fake.stat().st_mode | stat.S_IXUSR)
            old_cwd, old_env = os.getcwd(), dict(os.environ)
            try:
                os.chdir(root)
                os.environ["HERMIT_E2E_BUNDLE"] = str(root)
                with mock.patch.object(
                    cell_stock, "locate_cell_script", return_value=fake
                ):
                    code = cell_stock.main(
                        ["--slug", "probe", "c-programs/getcpu", "verify", "ptrace"]
                    )
            finally:
                os.chdir(old_cwd)
                os.environ.clear()
                os.environ.update(old_env)
            self.assertEqual(code, 0)
            found = list(root.glob("buck-out/v2/test/execution/stock/probe/*/artifacts_directory/run_id.fixture-run"))
            self.assertEqual(len(found), 1, found)


if __name__ == "__main__":
    unittest.main()
