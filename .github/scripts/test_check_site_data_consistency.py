#!/usr/bin/env python3
"""Tests for check-site-data-consistency.py (fixtures only, no network)."""

import gzip
import hashlib
import importlib.util
import json
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("check-site-data-consistency.py")
SPEC = importlib.util.spec_from_file_location("site_consistency", SCRIPT)
CHECK = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CHECK)


def make_tree(root: Path, *, cells=2, green=1, red=1, entries=1,
              sha_ok=True, counts=True, claimed_green=None) -> None:
    latest = root / "compatibility" / "latest"
    (latest / "data").mkdir(parents=True)
    summary = {"cells": [{"latest_result": "pass"}] * green
               + [{"latest_result": "product_failure"}] * red
               + [{"latest_result": "skipped"}] * max(0, cells - green - red)}
    body = gzip.compress(json.dumps(summary).encode())
    (latest / "data/site-summary.json.gz").write_bytes(body)
    build = {"artifacts": [{"path": "data/site-summary.json.gz",
                            "sha256": hashlib.sha256(body).hexdigest()
                            if sha_ok else "0" * 64,
                            "bytes": len(body)}] * entries}
    if counts:
        build["counts"] = {"green_cell_count": green if claimed_green is None else claimed_green, "red_cell_count": red}
    (latest / "build.json").write_text(json.dumps(build))


class SiteDataConsistencyTest(unittest.TestCase):
    def run_check(self, **kwargs) -> list[str]:
        with tempfile.TemporaryDirectory() as td:
            root = Path(td)
            make_tree(root, **kwargs)
            return CHECK.check(root / "compatibility" / "latest")

    def test_clean_tree_passes(self):
        self.assertEqual(self.run_check(), [])

    def test_tampered_summary_refused(self):
        with tempfile.TemporaryDirectory() as td:
            root = Path(td)
            make_tree(root)
            path = root / "compatibility/latest/data/site-summary.json.gz"
            body = bytearray(path.read_bytes())
            body[len(body) // 2] ^= 0xFF
            path.write_bytes(bytes(body))
            problems = CHECK.check(root / "compatibility" / "latest")
            self.assertTrue(problems)

    def test_skipped_cells_are_not_measured(self):
        # A summary whose only cells are skipped measures 0, matching
        # 0 green + 0 red: a len(cells) regression would count 2.
        self.assertEqual(self.run_check(cells=2, green=0, red=0), [])

    def test_corrupt_gzip_refused(self):
        with tempfile.TemporaryDirectory() as td:
            root = Path(td)
            make_tree(root)
            path = root / "compatibility/latest/data/site-summary.json.gz"
            path.write_bytes(b"not gzip at all")
            # refresh the recorded sha so only the inflate can refuse
            import hashlib as hl
            build_p = root / "compatibility/latest/build.json"
            build = json.loads(build_p.read_text())
            body = path.read_bytes()
            build["artifacts"][0]["sha256"] = hl.sha256(body).hexdigest()
            build["artifacts"][0]["bytes"] = len(body)
            build_p.write_text(json.dumps(build))
            self.assertTrue(CHECK.check(root / "compatibility" / "latest"))

    def test_missing_entry_refused(self):
        self.assertTrue(self.run_check(entries=0))

    def test_sha_mismatch_refused(self):
        self.assertTrue(self.run_check(sha_ok=False))

    def test_missing_counts_refused(self):
        self.assertTrue(self.run_check(counts=False))

    def test_count_mismatch_refused(self):
        # counts claim 2 green but only 1 measured pass cell exists
        self.assertTrue(self.run_check(cells=1, green=1, red=0, claimed_green=2))

    def test_inflated_counts_refused(self):
        # 1 pass + 1 product_failure + 1 skipped measures 2; counts
        # claiming 2 green + 1 red inflate past it and must refuse
        # (a len(cells) regression would count the skipped cell too).
        self.assertTrue(self.run_check(cells=3, green=1, red=1, claimed_green=2))

    def test_timeout_cell_is_measured(self):
        with tempfile.TemporaryDirectory() as td:
            root = Path(td)
            make_tree(root, cells=1, green=0, red=0)
            latest = root / "compatibility/latest"
            summary = {"cells": [{"latest_result": "timeout"}]}
            body = gzip.compress(json.dumps(summary).encode())
            (latest / "data/site-summary.json.gz").write_bytes(body)
            build = json.loads((latest / "build.json").read_text())
            build["artifacts"][0]["sha256"] = hashlib.sha256(body).hexdigest()
            build["artifacts"][0]["bytes"] = len(body)
            build["counts"] = {"green_cell_count": 0, "red_cell_count": 1}
            (latest / "build.json").write_text(json.dumps(build))
            self.assertEqual(CHECK.check(latest), [])


if __name__ == "__main__":
    unittest.main()
