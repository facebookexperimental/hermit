#!/usr/bin/env python3
"""Tests that demos/clean.sh removes only files under the QEMU asset directory.

clean.sh removes the temporary files an interrupted QEMU demo run can leave in
the asset directory, such as `.bzImage.<suffix>`, by matching patterns under
that directory. A pattern written with an unquoted `*` in the array that holds
it expands when the array is defined, against the directory clean.sh was
started from, not the asset directory. A file there named `.bzImage.a b` then
replaced the pattern; the loop that removes the matches split it into
`<assets>/.bzImage.a` and `b` and removed `b` from the caller's directory, and
a name with a ` *` in it removed every entry there. The patterns are quoted, so
they expand only under the asset directory.

Each test runs a copy of clean.sh inside a temporary directory that stands in
for the repository, so its removals under `<repo>/target/` stay in that
temporary directory, and points QEMU_ASSETS at a temporary asset directory.
"""

import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path


DEMO_DIR = Path(__file__).resolve().parent.parent

# Files that clean.sh's patterns match: one for each pattern, under the asset
# directory.
TRANSIENT_FILES = (
    "run-metadata.json.tmp.456",
    "hermit-boot.qcow2.tmp.789",
    "hermit-boot.qcow2.producer.json.tmp.321",
    "hermit-snapshot.qcow2.producer.json.tmp.654",
    ".bzImage.123",
    ".initramfs.cpio.gz.1",
    ".initramfs-version.2",
    ".initramfs-build.5",
    ".vmlinux.3",
    ".vmlinux-types.4",
)

# Files under the asset directory that clean.sh without --distclean keeps: the
# downloaded and built inputs, and a file the demos do not create.
KEPT_ASSET_FILES = (
    "bzImage",
    "initramfs.cpio.gz",
    ".initramfs-version",
    ".initramfs-build",
    "vmlinux",
    "notes.txt",
)

# Entries in the directory clean.sh is started from. The first two have names
# that the patterns `.bzImage.*` and `.vmlinux.*` match and that contain a
# space; split at that space they give the words `b` and `*`.
CALLER_FILES = (
    ".bzImage.a b",
    ".vmlinux.x *",
    "b",
    "keep1",
    "keepdir/inner",
)


def _snapshot(directory: Path) -> dict:
    """Return every file and directory below DIRECTORY with its contents."""
    entries = {}
    for path in sorted(directory.rglob("*")):
        relative = str(path.relative_to(directory))
        if path.is_dir():
            entries[relative + "/"] = None
        else:
            entries[relative] = path.read_bytes()
    return entries


class CleanTransientGlobTest(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="demo-clean-")
        self.addCleanup(temporary.cleanup)
        base = Path(temporary.name)

        # A copy of clean.sh and the helper it sources, in a stand-in
        # repository: clean.sh takes the repository as the parent of its own
        # directory.
        self.root = base / "repo"
        (self.root / "demos/lib").mkdir(parents=True)
        self.script = self.root / "demos/clean.sh"
        shutil.copy2(DEMO_DIR / "clean.sh", self.script)
        shutil.copy2(DEMO_DIR / "lib/qemu-paths.sh", self.root / "demos/lib/qemu-paths.sh")

        self.assets = base / "assets"
        self.assets.mkdir()
        for name in TRANSIENT_FILES + KEPT_ASSET_FILES:
            (self.assets / name).write_text(name + "\n")

        self.caller = base / "caller"
        self.caller.mkdir()
        for name in CALLER_FILES:
            path = self.caller / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(name + "\n")
        self.caller_before = _snapshot(self.caller)

    def _clean(self, *arguments):
        environment = dict(os.environ)
        environment["QEMU_ASSETS"] = str(self.assets)
        return subprocess.run(
            ["bash", str(self.script), *arguments],
            cwd=self.caller,
            env=environment,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            timeout=60,
        )

    def test_files_in_the_callers_directory_are_never_removed(self):
        completed = self._clean()
        self.assertEqual(completed.returncode, 0, completed.stdout)
        self.assertEqual(_snapshot(self.caller), self.caller_before, completed.stdout)

    def test_matching_files_under_the_asset_directory_are_removed(self):
        completed = self._clean()
        self.assertEqual(completed.returncode, 0, completed.stdout)
        self.assertEqual(
            sorted(path.name for path in self.assets.iterdir()),
            sorted(KEPT_ASSET_FILES),
            completed.stdout,
        )

    def test_the_dry_run_names_exactly_the_matching_asset_files(self):
        completed = self._clean("--dry-run")
        self.assertEqual(completed.returncode, 0, completed.stdout)
        named = sorted(
            line.removeprefix("  would remove ")
            for line in completed.stdout.splitlines()
            if line.startswith("  would remove ")
        )
        self.assertEqual(
            named,
            sorted(str(self.assets / name) for name in TRANSIENT_FILES),
            completed.stdout,
        )
        self.assertEqual(_snapshot(self.caller), self.caller_before, completed.stdout)
        self.assertEqual(
            sorted(path.name for path in self.assets.iterdir()),
            sorted(TRANSIENT_FILES + KEPT_ASSET_FILES),
        )

    def test_the_boot_snapshot_and_its_record_are_removed(self):
        # Demo 5 writes a record of the initramfs next to each boot snapshot it
        # saves; demo 6 restores the snapshot only with a matching record.
        computed = (
            "hermit-boot.qcow2",
            "hermit-boot.qcow2.producer.json",
            "hermit-snapshot.qcow2",
            "hermit-snapshot.qcow2.id",
            "hermit-snapshot.qcow2.producer.json",
        )
        for name in computed:
            (self.assets / name).write_text(name + "\n")
        completed = self._clean()
        self.assertEqual(completed.returncode, 0, completed.stdout)
        self.assertEqual(
            sorted(path.name for path in self.assets.iterdir()),
            sorted(KEPT_ASSET_FILES),
            completed.stdout,
        )

    def test_matching_files_are_removed_when_the_caller_has_none(self):
        # Positive control: with no matching names in the caller's directory,
        # the patterns reach the asset directory with or without the quoting.
        for name in CALLER_FILES[:2]:
            (self.caller / name).unlink()
        completed = self._clean()
        self.assertEqual(completed.returncode, 0, completed.stdout)
        self.assertEqual(
            sorted(path.name for path in self.assets.iterdir()),
            sorted(KEPT_ASSET_FILES),
            completed.stdout,
        )
        self.assertEqual(
            sorted(_snapshot(self.caller)),
            ["b", "keep1", "keepdir/", "keepdir/inner"],
        )


# The results directories under target/ that clean.sh removes, each with the
# variable that moves it somewhere else.
RESULT_OVERRIDES = (
    ("DEMO07_ARTIFACTS", "target/demos/07-drgn-kernel"),
    ("DEMO08_ARTIFACTS", "target/demos/08-btrfs-convert-uaf"),
    ("DEMO_SWEEP_LOG_DIR", "target/demo-sweep"),
)


class CleanMovedResultsTest(unittest.TestCase):
    """clean.sh keeps a results directory that an override moved, and says so.

    DEMO07_ARTIFACTS, DEMO08_ARTIFACTS and DEMO_SWEEP_LOG_DIR move a demo's
    results out of target/. Such a path can name any directory, / or $HOME
    included, and nothing records which entries in it a demo created, so
    clean.sh does not remove it. It used to say nothing about it either: with
    DEMO07_ARTIFACTS set, clean.sh reported "clean complete" while every
    94 MB snapshot copy that demo 7 had left there was still on disk. It now
    names each such directory as not removed.
    """

    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="demo-clean-moved-")
        self.addCleanup(temporary.cleanup)
        self.base = Path(temporary.name)

        self.root = self.base / "repo"
        (self.root / "demos/lib").mkdir(parents=True)
        self.script = self.root / "demos/clean.sh"
        shutil.copy2(DEMO_DIR / "clean.sh", self.script)
        shutil.copy2(DEMO_DIR / "lib/qemu-paths.sh", self.root / "demos/lib/qemu-paths.sh")

        self.assets = self.base / "assets"
        self.assets.mkdir()

    def _make_default_results(self):
        for _, default in RESULT_OVERRIDES:
            run = self.root / default / "run.default"
            run.mkdir(parents=True, exist_ok=True)
            (run / "hermit.log").write_text("default\n")

    def _make_moved_results(self, variable):
        directory = self.base / ("moved-" + variable.lower())
        (directory / "run.moved").mkdir(parents=True, exist_ok=True)
        (directory / "run.moved/hermit.log").write_text("moved\n")
        (directory / "unrelated.txt").write_text("not written by a demo\n")
        return directory

    def _clean(self, *arguments, **overrides):
        variables = {variable for variable, _ in RESULT_OVERRIDES}
        environment = {
            key: value for key, value in os.environ.items() if key not in variables
        }
        environment["QEMU_ASSETS"] = str(self.assets)
        environment.update(overrides)
        return subprocess.run(
            ["bash", str(self.script), *arguments],
            cwd=self.base,
            env=environment,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            timeout=60,
        )

    @staticmethod
    def _not_removed_lines(output):
        return [line for line in output.splitlines() if line.startswith("  not removed: ")]

    def test_a_moved_results_directory_is_kept_and_named(self):
        for variable, default in RESULT_OVERRIDES:
            with self.subTest(variable=variable):
                self._make_default_results()
                moved = self._make_moved_results(variable)
                before = _snapshot(moved)
                completed = self._clean(**{variable: str(moved)})
                self.assertEqual(completed.returncode, 0, completed.stdout)
                self.assertEqual(
                    self._not_removed_lines(completed.stdout),
                    [
                        f"  not removed: {variable}={moved} (clean.sh removes only "
                        f"{default}; delete that directory yourself when you no "
                        "longer need it)"
                    ],
                    completed.stdout,
                )
                self.assertEqual(_snapshot(moved), before, completed.stdout)
                self.assertFalse((self.root / default).exists(), completed.stdout)

    def test_every_moved_directory_is_named(self):
        self._make_default_results()
        moved = {variable: self._make_moved_results(variable) for variable, _ in RESULT_OVERRIDES}
        completed = self._clean(**{variable: str(path) for variable, path in moved.items()})
        self.assertEqual(completed.returncode, 0, completed.stdout)
        named = self._not_removed_lines(completed.stdout)
        self.assertEqual(len(named), len(RESULT_OVERRIDES), completed.stdout)
        for (variable, _), line in zip(RESULT_OVERRIDES, named):
            self.assertTrue(line.startswith(f"  not removed: {variable}={moved[variable]} "), line)

    def test_the_dry_run_names_a_moved_directory_and_deletes_nothing(self):
        self._make_default_results()
        moved = self._make_moved_results("DEMO07_ARTIFACTS")
        before = _snapshot(self.base)
        completed = self._clean("--dry-run", DEMO07_ARTIFACTS=str(moved))
        self.assertEqual(completed.returncode, 0, completed.stdout)
        self.assertIn("  would remove target/demos/07-drgn-kernel\n", completed.stdout)
        self.assertEqual(
            self._not_removed_lines(completed.stdout),
            [
                f"  not removed: DEMO07_ARTIFACTS={moved} (clean.sh removes only "
                "target/demos/07-drgn-kernel; delete that directory yourself when "
                "you no longer need it)"
            ],
            completed.stdout,
        )
        self.assertEqual(_snapshot(self.base), before, completed.stdout)

    def test_an_override_that_names_the_default_directory_is_not_reported(self):
        # Positive control: the default directory, however it is spelled, is
        # removed, and there is nothing left behind to report.
        link = self.base / "link-to-default"
        link.symlink_to(self.root / "target/demos/07-drgn-kernel")
        spellings = (
            str(self.root / "target/demos/07-drgn-kernel"),
            str(self.root / "target/demos/07-drgn-kernel") + "//",
            str(link),
        )
        for spelling in spellings:
            with self.subTest(spelling=spelling):
                self._make_default_results()
                completed = self._clean(DEMO07_ARTIFACTS=spelling)
                self.assertEqual(completed.returncode, 0, completed.stdout)
                self.assertEqual(self._not_removed_lines(completed.stdout), [], completed.stdout)
                self.assertIn("  removed target/demos/07-drgn-kernel\n", completed.stdout)
                self.assertFalse((self.root / "target/demos/07-drgn-kernel").exists())

    def test_an_empty_override_is_not_reported(self):
        # Positive control: an empty value means the default, as in the demos.
        self._make_default_results()
        completed = self._clean(DEMO07_ARTIFACTS="")
        self.assertEqual(completed.returncode, 0, completed.stdout)
        self.assertEqual(self._not_removed_lines(completed.stdout), [], completed.stdout)
        self.assertFalse((self.root / "target/demos/07-drgn-kernel").exists())

    def test_the_help_says_that_moved_directories_are_not_removed(self):
        completed = self._clean("--help")
        self.assertEqual(completed.returncode, 0, completed.stdout)
        help_text = " ".join(completed.stdout.split())
        self.assertIn(
            "A results directory moved with DEMO07_ARTIFACTS, DEMO08_ARTIFACTS or "
            "DEMO_SWEEP_LOG_DIR is not removed",
            help_text,
        )
        # The help is the script's header comment and nothing after it.
        self.assertTrue(help_text.endswith("so that you can remove it yourself."), help_text)
        self.assertNotIn("set -euo pipefail", completed.stdout)


if __name__ == "__main__":
    unittest.main()
