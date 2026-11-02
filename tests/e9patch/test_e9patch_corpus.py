#!/usr/bin/env python3
"""Consumer contracts of the e9patch preprocessing parity corpus driver.

`e9patch_corpus.py` beside this file is a manual driver. It needs e9tool and a
Hermit built with `--features e9patch`, and CI has neither, so CI never runs
the corpus guests. What CI can check is that the driver's typed readers accept
the records Hermit produces and refuse the records they must not trust. These
tests exercise those readers, the command builder and the corpus contract
without running Hermit or compiling a guest:

  * `e9patch_feature_from_build_info` reads the boolean `features.e9patch`
    from a `hermit version --json` record, follows a mutated value, and
    refuses a record without the field by naming the field;
  * `e9patch_engagement` reads the (candidate, mapped, B0) site counts from a
    `--backend-engagement-json` record, follows a mutated producer count, and
    refuses a record that lacks a count;
  * `hermit_command` passes the caller's private host `--tmp` root and never
    the shared host /tmp, so two concurrent corpus runs cannot collide;
  * `e9patch_corpus.py --check` finds a source for every corpus guest, and
    every guest source in the corpus directory is one the driver runs.

The build-info and engagement checks moved here from
tests/backend-parity/test_verify_tier_evidence.py, and the `--tmp` check from
tests/backend-parity/test_parallel_validate_paths.py (which no validation node
ran), when the corpus left tests/backend-parity in slice S13 of
https://github.com/rrnewton/hermit/issues/3301.

Run: python3 tests/e9patch/test_e9patch_corpus.py
"""

from __future__ import annotations

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import e9patch_corpus  # noqa: E402
from e9patch_corpus import (  # noqa: E402
    CorpusError,
    e9patch_engagement,
    e9patch_feature_from_build_info,
)


class BuildInfoFeatureTest(unittest.TestCase):
    def build_record(self, e9patch: bool) -> bytes:
        return json.dumps(
            {
                "schema": 1,
                "version": "0.2.0",
                "build_date": "2026-08-29",
                "git_sha": "0123456789ab",
                "features": {"dbt": True, "e9patch": e9patch, "sabre": True},
            }
        ).encode()

    def test_compile_time_feature_true_is_accepted(self) -> None:
        self.assertIs(e9patch_feature_from_build_info(self.build_record(True)), True)

    def test_mutating_the_typed_feature_changes_the_decision(self) -> None:
        self.assertIs(e9patch_feature_from_build_info(self.build_record(False)), False)

    def test_missing_feature_fails_by_field_name(self) -> None:
        with self.assertRaises(CorpusError) as refusal:
            e9patch_feature_from_build_info(b'{"schema":1,"features":{}}')
        self.assertIn("features.e9patch", str(refusal.exception))


class EngagementRecordTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory(prefix="e9patch-engagement-")
        self.path = Path(self.tmp.name) / "engagement.json"
        self.engagement = {
            "schema": 2,
            "engagement": {
                "backend": "e9patch",
                "candidate_sites": 7,
                "mapped_sites": 7,
                "b0_sites": 0,
            },
        }

    def tearDown(self) -> None:
        self.tmp.cleanup()

    def write(self) -> None:
        self.path.write_text(json.dumps(self.engagement), encoding="utf-8")

    def test_typed_preparation_counts_are_accepted(self) -> None:
        self.write()
        self.assertEqual(e9patch_engagement(self.path), (7, 7, 0))

    def test_mutating_the_producer_count_changes_the_consumer_result(self) -> None:
        self.engagement["engagement"]["mapped_sites"] = 6
        self.write()
        self.assertEqual(e9patch_engagement(self.path), (7, 6, 0))

    def test_missing_b0_count_fails_by_shape(self) -> None:
        del self.engagement["engagement"]["b0_sites"]
        self.write()
        with self.assertRaises(CorpusError) as refusal:
            e9patch_engagement(self.path)
        self.assertIn("incomplete", str(refusal.exception))


class HermitCommandTest(unittest.TestCase):
    def test_commands_use_the_callers_private_host_tmp(self) -> None:
        hermit = Path("/hermit")
        guest = Path("/guest")
        with tempfile.TemporaryDirectory() as raw:
            tmp_a = Path(raw) / "run-a"
            tmp_b = Path(raw) / "run-b"
            command_a = e9patch_corpus.hermit_command(hermit, False, False, guest, tmp_a)
            command_b = e9patch_corpus.hermit_command(hermit, False, False, guest, tmp_b)
        self.assertIn(f"--tmp={tmp_a}", command_a)
        self.assertIn(f"--tmp={tmp_b}", command_b)
        self.assertNotIn(f"--tmp={tmp_b}", command_a)
        self.assertNotIn(f"--tmp={tmp_a}", command_b)
        self.assertNotIn("--tmp=/tmp", command_a)
        self.assertNotIn("--tmp=/tmp", command_b)


class CorpusContractTest(unittest.TestCase):
    def test_check_mode_finds_every_guest_source(self) -> None:
        result = subprocess.run(
            [sys.executable, str(HERE / "e9patch_corpus.py"), "--check"],
            capture_output=True,
            text=True,
            timeout=60,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        contracts = [
            line.split()[1]
            for line in result.stdout.splitlines()
            if line.startswith("  contract ")
        ]
        self.assertEqual(contracts, list(e9patch_corpus.CORPUS))
        self.assertIn(
            f"CORPUS: {len(e9patch_corpus.CORPUS)} freestanding e9patch parity guests",
            result.stdout,
        )

    def test_every_guest_source_is_a_corpus_guest(self) -> None:
        sources = {path.stem for path in e9patch_corpus.CORPUS_DIR.glob("*.c")}
        self.assertEqual(sources, set(e9patch_corpus.CORPUS))


if __name__ == "__main__":
    unittest.main(verbosity=2)
