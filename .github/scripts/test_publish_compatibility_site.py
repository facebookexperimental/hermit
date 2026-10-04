# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

"""Small regression tests for nightly append-only publication handoffs."""

import copy
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock


ROOT = Path(__file__).resolve().parents[2]
HELPER = ROOT / ".github/scripts/publish-compatibility-site.py"
PINS = ROOT / ".github/compatibility-site-releases.json"
REPOSITORY_LOCATION_VARIABLES = (
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
)


class NightlyRegistryTests(unittest.TestCase):
    def setUp(self):
        # Git exports its repository-location variables to hooks and `git
        # rebase --exec` steps, and they override `git -C` and the working
        # directory. Every repository these tests touch is named explicitly;
        # with them inherited, the fixture's `git init` would rewrite the
        # caller's repository (https://github.com/rrnewton/hermit/issues/3362).
        scrubbed = mock.patch.dict(os.environ)
        scrubbed.start()
        self.addCleanup(scrubbed.stop)
        for name in REPOSITORY_LOCATION_VARIABLES:
            os.environ.pop(name, None)

    def test_previous_publication_cannot_be_dropped_or_rewritten(self):
        baseline = json.loads(PINS.read_bytes())
        added = copy.deepcopy(baseline["releases"][-1])
        added.update(
            identity="f" * 64,
            tag="compatibility-website-" + "f" * 64,
            asset="compatibility-website-" + "f" * 64 + ".tar.gz",
        )
        previous = copy.deepcopy(baseline)
        previous["releases"] = sorted(
            [*previous["releases"], added], key=lambda pin: pin["identity"]
        )
        previous["latest_identity"] = added["identity"]
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            old, proposed = root / "published.json", root / "proposed.json"
            old.write_text(json.dumps(previous))

            def check(value):
                proposed.write_text(json.dumps(value))
                return subprocess.run(
                    [
                        sys.executable,
                        str(HELPER),
                        "validate-update",
                        str(proposed),
                        str(PINS),
                        str(ROOT),
                        str(old),
                    ],
                    capture_output=True,
                )

            self.assertNotEqual(check(baseline).returncode, 0)
            self.assertEqual(check(previous).returncode, 0)
            changed = copy.deepcopy(previous)
            changed["releases"][-1]["archive_sha256"] = "e" * 64
            self.assertNotEqual(check(changed).returncode, 0)

    def test_selection_preserves_both_histories_and_latest_precedence(self):
        baseline = json.loads(PINS.read_bytes())

        def extend(value, digit):
            result = copy.deepcopy(value)
            pin = copy.deepcopy(value["releases"][-1])
            pin.update(
                identity=digit * 64,
                tag="compatibility-website-" + digit * 64,
                asset="compatibility-website-" + digit * 64 + ".tar.gz",
                release_title="Compatibility website",
            )
            result["releases"] = sorted(
                [*result["releases"], pin], key=lambda row: row["identity"]
            )
            result["latest_identity"] = pin["identity"]
            return result

        newer = extend(baseline, "f")
        equal_other_latest = copy.deepcopy(newer)
        equal_other_latest["latest_identity"] = baseline["latest_identity"]
        divergent = extend(baseline, "e")
        rewritten = copy.deepcopy(newer)
        rewritten["releases"][-1]["archive_sha256"] = "d" * 64
        union = extend(newer, "e")
        cases = [
            ("checked-in newer", newer, baseline, None, newer),
            ("served newer", baseline, newer, None, newer),
            (
                "equal maps preserve served latest",
                newer,
                equal_other_latest,
                None,
                equal_other_latest,
            ),
            ("explicit latest precedence", newer, equal_other_latest, newer, newer),
            ("explicit union preserves both", newer, divergent, union, union),
            ("divergent maps", newer, divergent, None, None),
            ("equal keys changed descriptor", newer, rewritten, None, None),
            ("explicit cannot drop served pin", baseline, newer, baseline, None),
            ("explicit cannot rewrite descriptor", newer, newer, rewritten, None),
        ]
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            pins = root / ".github/compatibility-site-releases.json"
            pins.parent.mkdir()
            pins.write_text(json.dumps(baseline))

            def git(*args):
                subprocess.run(
                    ["git", *args], cwd=root, check=True, capture_output=True
                )

            git("init", "-q")
            git("add", str(pins.relative_to(root)))
            git(
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-qm",
                "Registry fixture",
            )
            published, requested = root / "served.json", root / "explicit.json"
            for name, checked, served, explicit, expected in cases:
                with self.subTest(name=name):
                    pins.write_text(json.dumps(checked))
                    published.write_text(json.dumps(served))
                    argv = [
                        sys.executable,
                        str(HELPER),
                        "select-registry",
                        str(pins),
                        str(root),
                        str(published),
                    ]
                    if explicit is not None:
                        requested.write_text(json.dumps(explicit))
                        argv.append(str(requested))
                    result = subprocess.run(argv, capture_output=True)
                    if expected is None:
                        self.assertNotEqual(result.returncode, 0)
                        self.assertIn(b"refused", result.stderr)
                    else:
                        self.assertEqual(result.returncode, 0, result.stderr.decode())
                        self.assertEqual(json.loads(result.stdout), expected)
            # Both live maps can agree and still have dropped a committed pin.
            pins.write_text(json.dumps(newer))
            git("add", str(pins.relative_to(root)))
            git(
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-qm",
                "Retain newer registry",
            )
            git(
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "--allow-empty",
                "-qm",
                "Later fixture revision",
            )
            pins.write_text(json.dumps(baseline))
            published.write_text(json.dumps(baseline))
            result = subprocess.run(
                [
                    sys.executable,
                    str(HELPER),
                    "select-registry",
                    str(pins),
                    str(root),
                    str(published),
                ],
                capture_output=True,
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertIn(b"removed published identity", result.stderr)

    def test_describe_reports_actual_tree_and_manifest_directories(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "assets").mkdir()
            (root / "assets/style.css").write_bytes(b"body {}")
            build = {
                "freshness_sha256": "a" * 64,
                "artifacts_sha256": "b" * 64,
                "tree_sha256": "c" * 64,
                "counts": {"cells": 1},
                "directories": [{"path": "assets", "mode": 0o555}],
            }
            (root / "build.json").write_text(json.dumps(build))
            result = subprocess.run(
                [sys.executable, str(HELPER), "describe", str(root)],
                capture_output=True,
                check=True,
            )
            described = json.loads(result.stdout)
            self.assertEqual(described["directories"], ["assets"])
            self.assertEqual(described["file_count"], 2)
            self.assertEqual(
                described["file_bytes"],
                sum(path.stat().st_size for path in root.rglob("*") if path.is_file()),
            )


if __name__ == "__main__":
    unittest.main()
