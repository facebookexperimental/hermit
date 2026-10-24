"""Small regression tests for nightly append-only publication handoffs."""

import copy
import json
import os
import shutil
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


BUILDER = ROOT / ".github/compatibility-site-builder.json"
# Registry order is identity order; publication order is deliberately different.
# Digit "3" was published before NOT_BEFORE, so no rule may keep it unless it is
# the latest build.
NOT_BEFORE = "2030-01-01T00:00:00Z"
PUBLISHED = {
    "2": "2030-01-05T05:00:00Z",
    "5": "2030-01-04T05:00:00Z",
    "1": "2030-01-03T05:00:00Z",
    "4": "2030-01-02T05:00:00Z",
    "3": "2029-12-31T05:00:00Z",
}
LATEST = "2"
PAYLOAD_BYTES = {"1": 3000, "2": 1000, "3": 5000, "4": 4000, "5": 2000}


class RetentionTests(unittest.TestCase):
    """The site serves the newest N builds and indexes them; releases keep all."""

    def setUp(self):
        scrubbed = mock.patch.dict(os.environ)
        scrubbed.start()
        self.addCleanup(scrubbed.stop)
        for name in REPOSITORY_LOCATION_VARIABLES:
            os.environ.pop(name, None)
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.addCleanup(self.make_writable, Path(directory.name))
        self.root = Path(directory.name)
        self.trees = self.root / "trees"
        self.trees.mkdir()
        self.metadata = self.root / "metadata"
        self.metadata.mkdir()
        pins = []
        for digit in sorted(PUBLISHED):
            pin = self.make_build(digit)
            pins.append(pin)
            (self.metadata / f"{pin['identity']}.json").write_text(
                json.dumps(
                    {
                        "tag_name": pin["tag"],
                        "name": pin["release_title"],
                        "draft": False,
                        "prerelease": False,
                        "published_at": PUBLISHED[digit],
                    }
                )
            )
        self.registry = {
            "schema_version": 1,
            "release_repository": "rrnewton/hermit",
            "latest_identity": LATEST * 64,
            "releases": pins,
        }
        self.registry_path = self.root / "releases.json"
        self.registry_path.write_text(json.dumps(self.registry))
        self.retention = copy.deepcopy(json.loads(BUILDER.read_bytes())["retention"])
        self.retention["not_before"] = NOT_BEFORE
        self.latest_bytes = next(
            pin["file_bytes"] for pin in pins if pin["identity"] == LATEST * 64
        )

    @staticmethod
    def make_writable(root):
        for path in [root, *root.rglob("*")]:
            if not path.is_symlink():
                path.chmod(0o700)

    def make_build(self, digit, commit=None):
        identity = digit * 64
        tree = self.trees / identity
        (tree / "assets").mkdir(parents=True)
        (tree / "assets/data.bin").write_bytes(digit.encode() * PAYLOAD_BYTES[digit])
        build = {
            "freshness_sha256": identity,
            "artifacts_sha256": "b" * 64,
            "tree_sha256": "c" * 64,
            "counts": {"cells": int(digit)},
            "directories": [{"path": "."}, {"path": "assets"}],
            "provenance": {"hermit_main_commit": commit or digit * 40},
        }
        if digit == "4" and commit is None:
            del build["provenance"]
        (tree / "build.json").write_text(json.dumps(build))
        described = json.loads(self.helper("describe", tree).stdout)
        tag = f"compatibility-website-{identity}"
        return {
            "archive_bytes": 1,
            "archive_member_count": described["file_count"]
            + len(described["directories"])
            + 1,
            "archive_sha256": "a" * 64,
            "artifacts_sha256": described["artifacts_sha256"],
            "asset": f"{tag}.tar.gz",
            "build_sha256": described["build_sha256"],
            "content_tree_sha256": described["content_tree_sha256"],
            "counts": described["counts"],
            "directories": described["directories"],
            "file_bytes": described["file_bytes"],
            "file_count": described["file_count"],
            "identity": described["identity"],
            "manifest_tree_sha256": described["manifest_tree_sha256"],
            "mode_sha256": described["mode_sha256"],
            "recursive_identity_sha256": described["recursive_identity_sha256"],
            "release_title": "Compatibility website",
            "tag": tag,
        }

    def helper(self, mode, *args, check=True):
        result = subprocess.run(
            [sys.executable, str(HELPER), mode, *map(str, args)],
            capture_output=True,
        )
        if check:
            self.assertEqual(result.returncode, 0, result.stderr.decode())
        return result

    def config(self, retention=None, text=None):
        path = self.root / "builder.json"
        if text is None:
            text = json.dumps({"retention": retention or self.retention})
        path.write_text(text)
        return path

    def retain(self, retention=None, check=True):
        return self.helper(
            "retain",
            self.registry_path,
            self.metadata,
            self.config(retention),
            check=check,
        )

    def kept(self, retention=None):
        result = self.retain(retention)
        return [line.split(" ") for line in result.stdout.decode().splitlines()]

    def index_of(self, digit):
        return str(sorted(PUBLISHED).index(digit))

    def expect(self, *digits):
        return [[self.index_of(digit), digit * 64] for digit in digits]

    def set_published(self, digit, value):
        path = self.metadata / f"{digit * 64}.json"
        path.write_text(json.dumps(dict(json.loads(path.read_bytes()), published_at=value)))

    def replace_build(self, digit, commit):
        """Rebuild one tree recording `commit`, and pin the new bytes."""
        tree = self.trees / (digit * 64)
        self.make_writable(tree)
        shutil.rmtree(tree)
        pin = self.make_build(digit, commit)
        index = int(self.index_of(digit))
        self.assertEqual(self.registry["releases"][index]["identity"], pin["identity"])
        self.registry["releases"][index] = pin
        self.registry_path.write_text(json.dumps(self.registry))

    def test_check_retention_applies_the_same_check_as_retain(self):
        result = self.helper("check-retention", BUILDER)
        self.assertIn(b"is the rule this publisher applies", result.stdout)
        wrong = self.config(dict(self.retention, rule="Keep the newest 5 builds."))
        result = self.helper("check-retention", wrong, check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(b"rule is not the rule this publisher applies", result.stderr)
        for args in [(), (BUILDER, BUILDER)]:
            with self.subTest(arguments=len(args)):
                result = self.helper("check-retention", *args, check=False)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(b"requires one builder-config path", result.stderr)

    def test_not_before_is_inclusive_to_the_second(self):
        retention = dict(self.retention, budget_bytes=10**12)
        self.set_published("4", NOT_BEFORE)
        self.assertEqual(self.kept(retention), self.expect("2", "5", "1", "4"))
        self.set_published("4", "2029-12-31T23:59:59Z")
        self.assertEqual(self.kept(retention), self.expect("2", "5", "1"))

    def test_equal_publication_times_keep_the_larger_identity(self):
        # Registry order is identity order, so a sort on time alone would keep
        # "1" here; the documented tie-break keeps "5".
        self.set_published("1", PUBLISHED["5"])
        retention = dict(self.retention, budget_bytes=10**12)
        self.assertEqual(
            self.kept(dict(retention, max_builds=2)), self.expect("2", "5")
        )
        self.assertEqual(
            self.kept(dict(retention, max_builds=3)), self.expect("2", "5", "1")
        )

    def test_checked_in_builder_config_states_the_rule_this_publisher_applies(self):
        retention = json.loads(BUILDER.read_bytes())["retention"]
        # Owner decision: N = floor(1 GB / the latest build's size), capped at 100.
        self.assertEqual(retention["budget_bytes"], 1_000_000_000)
        self.assertEqual(retention["max_builds"], 100)
        result = self.helper("retain", self.registry_path, self.metadata, BUILDER)
        self.assertIn(b"(N = 100)", result.stderr)

    def test_retain_keeps_the_newest_n_published_builds(self):
        cases = [
            ("budget gives N = 3", 3 * self.latest_bytes + self.latest_bytes - 1, 100,
             ["2", "5", "1"]),
            ("budget exactly N = 2", 2 * self.latest_bytes, 100, ["2", "5"]),
            ("max_builds caps N", 10**12, 2, ["2", "5"]),
            ("not_before excludes the early build", 10**12, 100, ["2", "5", "1", "4"]),
            ("budget below one build still keeps the latest", self.latest_bytes - 1, 100,
             ["2"]),
        ]
        for name, budget, maximum, digits in cases:
            with self.subTest(name=name):
                retention = dict(self.retention, budget_bytes=budget, max_builds=maximum)
                self.assertEqual(self.kept(retention), self.expect(*digits))
        result = self.retain(dict(self.retention, budget_bytes=3 * self.latest_bytes))
        self.assertIn(b"keeping 3 of 5 compatibility websites (N = 3)", result.stderr)

    def test_the_latest_build_is_kept_even_when_not_newest_or_before_not_before(self):
        self.registry["latest_identity"] = "3" * 64
        self.registry_path.write_text(json.dumps(self.registry))
        self.assertEqual(
            self.kept(dict(self.retention, budget_bytes=10**12)),
            self.expect("2", "5", "1", "4", "3"),
        )
        self.assertEqual(
            self.kept(dict(self.retention, budget_bytes=10**12, max_builds=3)),
            self.expect("2", "5", "3"),
        )
        self.assertEqual(
            self.kept(dict(self.retention, budget_bytes=10**12, max_builds=1)),
            self.expect("3"),
        )

    def test_retain_refuses_a_config_it_does_not_implement(self):
        wrong = [
            ("rule", "Keep the newest 5 builds.", b"rule is not the rule"),
            ("budget_bytes", 0, b"budget_bytes is not a positive integer"),
            ("budget_bytes", "1000000000", b"budget_bytes is not a positive integer"),
            ("max_builds", True, b"max_builds is not a positive integer"),
            ("max_builds", -1, b"max_builds is not a positive integer"),
            ("not_before", "2030-01-01", b"not_before is not a UTC timestamp"),
            ("not_before", "2030-02-30T00:00:00Z", b"not_before is not a valid UTC"),
            ("extra", 1, b"retention fields are not exact"),
        ]
        for key, value, message in wrong:
            with self.subTest(key=key, value=value):
                result = self.retain(dict(self.retention, **{key: value}), check=False)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(message, result.stderr)
        missing = dict(self.retention)
        del missing["max_builds"]
        result = self.retain(missing, check=False)
        self.assertIn(b"retention fields are not exact", result.stderr)
        duplicated = json.dumps({"retention": self.retention})[:-1] + ', "retention": {}}'
        path = self.config(text=duplicated)
        result = self.helper(
            "retain", self.registry_path, self.metadata, path, check=False
        )
        self.assertIn(b"duplicate object key 'retention'", result.stderr)

    def test_retain_refuses_release_metadata_that_disagrees_with_its_pin(self):
        path = self.metadata / f"{'5' * 64}.json"
        original = json.loads(path.read_bytes())
        wrong = [
            ("tag_name", "compatibility-website-" + "1" * 64, b"disagrees with the pin"),
            ("name", "Other title", b"disagrees with the pin"),
            ("draft", True, b"disagrees with the pin"),
            ("prerelease", None, b"disagrees with the pin"),
            ("published_at", "2030-01-04 05:00:00", b"published_at is not a UTC"),
            ("published_at", None, b"published_at is not a UTC"),
        ]
        for key, value, message in wrong:
            with self.subTest(key=key, value=value):
                path.write_text(json.dumps(dict(original, **{key: value})))
                result = self.retain(check=False)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(message, result.stderr)
        path.unlink()
        result = self.retain(check=False)
        self.assertIn(b"cannot read release metadata", result.stderr)

    def publish(self, digits):
        root = self.root / "publication"
        root.mkdir()
        for digit in digits:
            shutil.copytree(self.trees / (digit * 64), root / (digit * 64))
        return root

    def finalize(self, root, retention, check=True):
        return self.helper(
            "finalize",
            self.registry_path,
            root,
            self.metadata,
            self.config(retention),
            check=check,
        )

    def test_finalize_serves_the_plan_and_indexes_it(self):
        retention = dict(self.retention, budget_bytes=10**12, max_builds=3)
        root = self.publish(["1", "2", "5"])
        result = self.finalize(root, retention)
        self.assertIn(b"verified 3 retained compatibility websites of 5 released (N = 3)",
                      result.stdout)
        self.assertEqual(
            sorted(path.name for path in root.iterdir()),
            sorted(["1" * 64, "2" * 64, "5" * 64, "latest", "releases.json", "builds"]),
        )
        self.assertEqual(json.loads((root / "releases.json").read_bytes()), self.registry)
        self.assertEqual(
            (root / "latest/assets/data.bin").read_bytes(),
            (self.trees / ("2" * 64) / "assets/data.bin").read_bytes(),
        )
        page = (root / "builds/index.html").read_text()
        self.assertEqual(sorted(path.name for path in (root / "builds").iterdir()),
                         ["index.html"])
        for digit in ("2", "5", "1"):
            self.assertEqual(page.count(f'href="../{digit * 64}/"'), 1, digit)
            self.assertIn(f"/commit/{digit * 40}\"><code>{digit * 12}</code>", page)
        for digit in ("3", "4"):
            self.assertNotIn(digit * 64, page)
        # Newest first, in US Eastern time, with the latest build marked.
        positions = [page.index(f"../{digit * 64}/") for digit in ("2", "5", "1")]
        self.assertEqual(positions, sorted(positions))
        self.assertIn("2030-01-05 00:00 ET", page)
        self.assertEqual(page.count("(latest)"), 1)
        self.assertIn("so N = 3", page)
        self.assertIn("This site keeps 3 compatibility website builds, newest first.",
                      page)
        self.assertIn("N uses only the latest build's size, so the kept builds together "
                      "can hold more or less than the budget", page)
        self.assertIn("newest build is not counted.", page)
        self.assertIn("Apart from the latest build, builds published before "
                      "2029-12-31 19:00 ET are not kept here.", page)
        self.assertIn('href="../releases.json"', page)
        self.assertIn('href="../latest/"', page)

    def test_finalize_marks_a_build_without_a_recorded_hermit_commit(self):
        retention = dict(self.retention, budget_bytes=10**12)
        root = self.publish(["1", "2", "4", "5"])
        self.finalize(root, retention)
        page = (root / "builds/index.html").read_text()
        self.assertEqual(page.count("not recorded"), 1)

    def test_finalize_shows_only_a_full_lowercase_hermit_commit(self):
        retention = dict(self.retention, budget_bytes=10**12)
        for value, raw in [
            ("ABCDEF0123" * 4, "ABCDEF0123"),
            (("abcdef0123" * 4)[:39], "abcdef0123"),
            ("<i>" + "c" * 37, "<i>"),
        ]:
            with self.subTest(value=value):
                self.replace_build("5", value)
                root = self.publish(["1", "2", "4", "5"])
                try:
                    self.finalize(root, retention)
                    page = (root / "builds/index.html").read_text()
                    self.assertEqual(page.count("not recorded"), 2)
                    self.assertNotIn(raw, page)
                    self.assertIn(f"/commit/{'1' * 40}\"", page)
                finally:
                    self.make_writable(root)
                    shutil.rmtree(root)

    def test_finalize_words_a_single_kept_build_in_the_singular(self):
        retention = dict(self.retention, budget_bytes=self.latest_bytes - 1)
        root = self.publish(["2"])
        self.finalize(root, retention)
        page = (root / "builds/index.html").read_text()
        self.assertIn("so N = 1", page)
        self.assertIn("This site keeps 1 compatibility website build. It is a complete "
                      "copy", page)
        self.assertNotIn("builds, newest first", page)
        self.assertNotIn("together they hold", page)

    def test_the_site_landing_page_links_the_builds_index(self):
        landing = (ROOT / "docs/site/index.html").read_text()
        self.assertIn('href="compatibility/builds/"', landing)

    def test_finalize_refuses_a_publication_that_is_not_the_plan(self):
        retention = dict(self.retention, budget_bytes=10**12, max_builds=3)
        for name, digits in [
            ("a pruned build is still served", ["1", "2", "4", "5"]),
            ("a kept build is missing", ["2", "5"]),
            ("an excluded build is served", ["1", "2", "3", "5"]),
        ]:
            with self.subTest(name=name):
                root = self.publish(digits)
                try:
                    result = self.finalize(root, retention, check=False)
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn(b"do not exactly match the release registry's "
                                  b"retention plan", result.stderr)
                    self.assertFalse((root / "builds").exists())
                    self.assertFalse((root / "releases.json").exists())
                finally:
                    self.make_writable(root)
                    shutil.rmtree(root)
        root = self.publish(["1", "2", "5"])
        (root / ("5" * 64) / "assets/data.bin").write_bytes(b"changed")
        result = self.finalize(root, retention, check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(f"retained tree {'5' * 64} disagrees".encode(), result.stderr)


if __name__ == "__main__":
    unittest.main()
