#!/usr/bin/env python3
"""Demo 6 restores only a boot snapshot of the initramfs that is built now.

Background. Demo 5 (demos/05-qemu-boot/run.py) boots Linux under Hermit from
the kernel and initramfs that demos/lib/qemu-assets.sh builds, and saves the
running machine as a qcow2 snapshot: the boot snapshot. Demo 6
(demos/06-qemu-resume/run.py) restores that snapshot instead of booting. The
guest's /init, which reads demo 6's command, runs it and frames its output
(the command frame), is therefore the /init in the snapshot's memory, not the
one in the initramfs on disk. An /init of frame format 2 or older runs the
command as root with the console as its standard input, so the command can
print a whole frame itself and end the resume with a forged exit status while
it keeps running (https://github.com/rrnewton/hermit/pull/3497).

Demo 5 now writes a record next to each boot snapshot it saves,
<snapshot>.producer.json, naming the snapshot's SHA-256 and the
INITRAMFS_VERSION and SHA-256 of the initramfs it booted. Demo 6 restores the
default snapshot only when that record matches the snapshot and the initramfs
it would use now, and runs demo 5 again to rebuild it otherwise; it refuses a
snapshot named by QEMU_BOOT_SNAPSHOT_DISK without such a record, saying why and
how to rebuild it. These tests run the record functions of demo_common on
stand-in files, and demo 5's boot_once and demo 6's ensure_boot_snapshot and
resume_once with QEMU, Hermit and demo 5 replaced.
"""

import ast
import contextlib
import hashlib
import io
import json
import os
import re
import runpy
import subprocess
import sys
import tempfile
import types
import unittest
from pathlib import Path
from unittest import mock

DEMOS_DIR = Path(__file__).resolve().parent.parent
REPOSITORY_ROOT = DEMOS_DIR.parent
sys.path.insert(0, str(DEMOS_DIR / "lib"))

import demo_common as dc  # noqa: E402
import drgn_hermit as dh  # noqa: E402

INITRAMFS = b"stand-in for the initramfs"
SNAPSHOT = b"stand-in for the demo 5 boot snapshot"


def _sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def _write_assets_script(root: Path, version: int) -> None:
    """Make ``root`` a stand-in checkout whose qemu-assets.sh builds ``version``."""
    script = root / "demos/lib/qemu-assets.sh"
    script.parent.mkdir(parents=True, exist_ok=True)
    script.write_text(
        "#!/usr/bin/env bash\n"
        "set -euo pipefail\n"
        "# Change the /init below together and bump INITRAMFS_VERSION.\n"
        "INITRAMFS_VERSION={}\n"
        'INITRAMFS_VERSION_FILE="$ARTIFACT_DIR/.initramfs-version"\n'.format(version)
    )


class _StandIns(unittest.TestCase):
    """A stand-in checkout at initramfs version 9, an initramfs, a snapshot."""

    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.directory = Path(directory.name)
        self.root = self.directory / "checkout"
        _write_assets_script(self.root, 9)
        self.assets = self.directory / "assets"
        self.assets.mkdir()
        self.initramfs = self.assets / "initramfs.cpio.gz"
        self.initramfs.write_bytes(INITRAMFS)
        self.snapshot = self.assets / "hermit-boot.qcow2"

    def record(self, snapshot: Path) -> Path:
        """Write the record demo 5 writes for ``snapshot`` booted from the
        initramfs there is now."""
        return dc.write_boot_snapshot_record(
            snapshot, dc.hash_file(snapshot), dc.initramfs_producer(self.root, self.assets)
        )

    def assert_mismatch(self, snapshot: Path, expected: str, disk=None) -> None:
        with self.assertRaises(dc.BootSnapshotMismatch) as caught:
            dc.verify_boot_snapshot(snapshot, self.root, self.assets, disk=disk)
        self.assertEqual(str(caught.exception), expected)

    def symlinked_snapshot(self) -> Path:
        """A boot snapshot reached through a symlink: alias.qcow2 names
        store/boot-1.qcow2, which holds SNAPSHOT. Neither has a record yet.
        Returns the symlink."""
        target = self.directory / "store" / "boot-1.qcow2"
        target.parent.mkdir()
        target.write_bytes(SNAPSHOT)
        alias = self.directory / "alias.qcow2"
        alias.symlink_to(target)
        return alias

    def symlinked_snapshot_cases(self, alias: Path):
        """The records of the symlinked snapshot ``alias`` that are refused.

        Each case is (name, prepare, reason): ``prepare`` writes the records,
        and ``reason`` is what verify_boot_snapshot says about them when it is
        given ``alias`` and checks the bytes at ``disk``. Only the record next
        to the name ``alias`` counts, so the cases with a matching record next
        to the symlink's target are refused too.
        """
        target = alias.resolve()
        other = b"another boot snapshot"

        def no_record():
            pass

        def other_bytes():
            dc.write_boot_snapshot_record(
                alias, _sha256(other), dc.initramfs_producer(self.root, self.assets)
            )

        def missing(disk):
            return (
                "it has no record of the initramfs it was booted from ({} is "
                "missing)".format(dc.boot_snapshot_record_path(alias))
            )

        def mismatched(disk):
            return "{} has SHA-256 {}, not the {} that demo 5 recorded for it".format(
                disk, _sha256(SNAPSHOT), _sha256(other)
            )

        return (
            ("no record", no_record, missing),
            (
                "a matching record next to the target only",
                lambda: self.record(target),
                missing,
            ),
            ("a record of other bytes", other_bytes, mismatched),
            (
                "a record of other bytes next to the name and a matching one "
                "next to the target",
                lambda: (other_bytes(), self.record(target)),
                mismatched,
            ),
        )

    def clear_records(self, alias: Path) -> None:
        for snapshot in (alias, alias.resolve()):
            dc.boot_snapshot_record_path(snapshot).unlink(missing_ok=True)


class BootSnapshotRecordTest(_StandIns):
    """The record, and what verify_boot_snapshot accepts."""

    def test_the_record_names_the_snapshot_and_the_initramfs(self):
        self.snapshot.write_bytes(SNAPSHOT)
        record = self.record(self.snapshot)
        self.assertEqual(record, self.assets / "hermit-boot.qcow2.producer.json")
        self.assertEqual(
            json.loads(record.read_text()),
            {
                "format": 1,
                "initramfs_sha256": _sha256(INITRAMFS),
                "initramfs_version": 9,
                "snapshot_sha256": _sha256(SNAPSHOT),
            },
        )
        # Renamed into place: no temporary file is left behind.
        self.assertEqual(
            sorted(path.name for path in self.assets.iterdir()),
            ["hermit-boot.qcow2", "hermit-boot.qcow2.producer.json", "initramfs.cpio.gz"],
        )
        dc.verify_boot_snapshot(self.snapshot, self.root, self.assets)

    def test_a_snapshot_without_a_record_does_not_match(self):
        # A snapshot demo 5 saved before it wrote records.
        self.snapshot.write_bytes(SNAPSHOT)
        self.assert_mismatch(
            self.snapshot,
            "it has no record of the initramfs it was booted from ({} is missing): "
            "demo 5 saved it before it wrote such records, or demo 5 did not save "
            "it".format(self.assets / "hermit-boot.qcow2.producer.json"),
        )

    def test_a_snapshot_of_an_older_initramfs_version_does_not_match(self):
        self.snapshot.write_bytes(SNAPSHOT)
        self.record(self.snapshot)
        _write_assets_script(self.root, 10)
        self.assert_mismatch(
            self.snapshot,
            "it was booted from initramfs version 9, and demos/lib/qemu-assets.sh "
            "now builds version 10",
        )

    def test_a_snapshot_of_another_initramfs_does_not_match(self):
        # The same version number, but the initramfs was rebuilt differently.
        self.snapshot.write_bytes(SNAPSHOT)
        self.record(self.snapshot)
        self.initramfs.write_bytes(b"another initramfs")
        self.assert_mismatch(
            self.snapshot,
            "it was booted from an initramfs with SHA-256 {}, and {} now has "
            "SHA-256 {}".format(
                _sha256(INITRAMFS), self.initramfs, _sha256(b"another initramfs")
            ),
        )

    def test_a_replaced_snapshot_does_not_match_the_record_of_the_one_before(self):
        self.snapshot.write_bytes(SNAPSHOT)
        self.record(self.snapshot)
        self.snapshot.write_bytes(b"a snapshot saved later")
        self.assert_mismatch(
            self.snapshot,
            "{} has SHA-256 {}, not the {} that demo 5 recorded for it".format(
                self.snapshot, _sha256(b"a snapshot saved later"), _sha256(SNAPSHOT)
            ),
        )

    def test_a_copy_is_checked_against_the_record_of_the_snapshot(self):
        self.snapshot.write_bytes(SNAPSHOT)
        self.record(self.snapshot)
        copy = self.directory / "copy.qcow2"
        copy.write_bytes(SNAPSHOT)
        dc.verify_boot_snapshot(self.snapshot, self.root, self.assets, disk=copy)
        copy.write_bytes(b"not the recorded snapshot")
        self.assert_mismatch(
            self.snapshot,
            "{} has SHA-256 {}, not the {} that demo 5 recorded for it".format(
                copy, _sha256(b"not the recorded snapshot"), _sha256(SNAPSHOT)
            ),
            disk=copy,
        )

    def test_without_an_initramfs_nothing_matches(self):
        self.snapshot.write_bytes(SNAPSHOT)
        self.record(self.snapshot)
        self.initramfs.unlink()
        self.assert_mismatch(
            self.snapshot,
            "there is no initramfs at {} to compare it with".format(self.initramfs),
        )

    def test_a_record_demo_5_did_not_write_does_not_match(self):
        self.snapshot.write_bytes(SNAPSHOT)
        record_path = self.record(self.snapshot)
        good = json.loads(record_path.read_text())
        for content, problem in (
            ("[]", "it is not a JSON object"),
            (json.dumps(dict(good, format=2)), "its format is 2, not 1"),
            (json.dumps(dict(good, format="1")), "its format is '1', not 1"),
            (json.dumps(dict(good, format=True)), "its format is True, not 1"),
            (
                json.dumps(dict(good, initramfs_version="9")),
                "its initramfs_version '9' is not a whole number",
            ),
            (
                json.dumps(dict(good, initramfs_version=True)),
                "its initramfs_version True is not a whole number",
            ),
            (
                json.dumps(dict(good, initramfs_sha256="ABC")),
                "its initramfs_sha256 'ABC' is not a SHA-256 digest",
            ),
            (
                json.dumps({key: value for key, value in good.items() if key != "snapshot_sha256"}),
                "its snapshot_sha256 None is not a SHA-256 digest",
            ),
        ):
            with self.subTest(content=content):
                record_path.write_text(content)
                self.assert_mismatch(
                    self.snapshot,
                    "its record {} is not one demo 5 wrote: {}".format(record_path, problem),
                )
        record_path.write_text("{not json")
        with self.assertRaises(dc.BootSnapshotMismatch) as caught:
            dc.verify_boot_snapshot(self.snapshot, self.root, self.assets)
        self.assertTrue(
            str(caught.exception).startswith("its record {} cannot be read: ".format(record_path)),
            str(caught.exception),
        )

    def test_the_version_is_read_from_the_one_initramfs_version_line(self):
        self.assertEqual(dc.current_initramfs_version(self.root), 9)
        script = self.root / "demos/lib/qemu-assets.sh"
        for text, found in (("echo no version\n", 0), ("INITRAMFS_VERSION=1\nINITRAMFS_VERSION=2\n", 2)):
            with self.subTest(text=text):
                script.write_text(text)
                with self.assertRaises(RuntimeError) as caught:
                    dc.current_initramfs_version(self.root)
                self.assertEqual(
                    str(caught.exception),
                    "expected one INITRAMFS_VERSION= line in {}, found {}".format(script, found),
                )

    def test_the_version_is_the_one_the_real_qemu_assets_script_builds(self):
        script = REPOSITORY_ROOT / "demos/lib/qemu-assets.sh"
        assignments = [
            line.split("=", 1)[1]
            for line in script.read_text().splitlines()
            if line.startswith("INITRAMFS_VERSION=")
        ]
        self.assertEqual(len(assignments), 1, assignments)
        self.assertEqual(dc.current_initramfs_version(REPOSITORY_ROOT), int(assignments[0]))


class Demo6EnsureBootSnapshotTest(_StandIns):
    """Which boot snapshot demo 6 accepts before it resumes, and what it does
    about one it does not."""

    @classmethod
    def setUpClass(cls):
        cls.demo6 = runpy.run_path(str(DEMOS_DIR / "06-qemu-resume" / "run.py"))

    def setUp(self):
        super().setUp()
        self.commands = []
        self.demo5_command = [
            "make",
            "--no-print-directory",
            "-C",
            str(self.demo6["DEMOS_DIR"]),
            "demo5",
        ]

    def ensure(self, boot_snapshot: Path, demo5=None) -> str:
        """Run ensure_boot_snapshot with QEMU_BOOT_SNAPSHOT_DISK ``boot_snapshot``.

        ``demo5`` stands in for demo 5 when demo 6 runs it. Returns what demo 6
        printed.
        """

        def run_checked(command, **keywords):
            self.commands.append(list(command))
            if demo5 is not None:
                demo5()

        ensure_boot_snapshot = self.demo6["ensure_boot_snapshot"]
        printed = io.StringIO()
        with mock.patch.dict(
            ensure_boot_snapshot.__globals__,
            {
                "ASSETS": self.assets,
                "ROOT": self.root,
                "BOOT_SNAPSHOT_DISK": boot_snapshot,
                "run_checked": run_checked,
            },
        ), contextlib.redirect_stdout(printed):
            ensure_boot_snapshot()
        return printed.getvalue()

    def demo5_saves(self, data: bytes = b"a snapshot of the current initramfs"):
        """A stand-in for demo 5 that saves the default snapshot and its record."""

        def demo5():
            self.snapshot.write_bytes(data)
            self.record(self.snapshot)

        return demo5

    def test_a_default_snapshot_of_the_current_initramfs_is_used_as_it_is(self):
        self.snapshot.write_bytes(SNAPSHOT)
        self.record(self.snapshot)
        self.ensure(self.snapshot)
        self.assertEqual(self.commands, [])
        self.assertEqual(self.snapshot.read_bytes(), SNAPSHOT)

    def demo5_fails(self, saves=None):
        """A stand-in for demo 5 that runs ``saves``, then fails the way
        run_checked does when demo 5 exits non-zero and make exits 2."""

        def demo5():
            if saves is not None:
                saves()
            raise subprocess.CalledProcessError(2, self.demo5_command)

        return demo5

    def test_a_rebuild_whose_demo5_fails_after_saving_a_current_snapshot_uses_it(self):
        # Demo 5 publishes the snapshot and its record, then ends PARTIAL
        # against its reference run from the old initramfs and exits non-zero.
        self.snapshot.write_bytes(SNAPSHOT)
        printed = self.ensure(self.snapshot, demo5=self.demo5_fails(self.demo5_saves()))
        self.assertEqual(self.commands, [self.demo5_command])
        self.assertIn(
            "NOTE: demo 5 failed (`{}` exited with status 2), but it saved {} with a "
            "record that matches the current initramfs, so demo 6 uses that "
            "snapshot.".format(" ".join(self.demo5_command), self.snapshot),
            printed,
        )
        self.assertIn(
            "demo 5 ends PARTIAL against it until you run demos/clean.sh", printed
        )
        dc.verify_boot_snapshot(self.snapshot, self.root, self.assets)

    def test_a_rebuild_whose_demo5_fails_without_a_current_snapshot_stops(self):
        for name, saves, problem in (
            (
                "nothing saved",
                None,
                "{} does not match the current initramfs: it has no record".format(
                    self.snapshot
                ),
            ),
            (
                "saved without a record",
                lambda: self.snapshot.write_bytes(b"new bytes, no record"),
                "{} does not match the current initramfs: it has no record".format(
                    self.snapshot
                ),
            ),
            (
                "snapshot removed",
                lambda: self.snapshot.unlink(),
                "{} does not exist".format(self.snapshot),
            ),
        ):
            with self.subTest(case=name):
                self.snapshot.write_bytes(SNAPSHOT)
                self.commands = []
                with self.assertRaises(RuntimeError) as caught:
                    self.ensure(self.snapshot, demo5=self.demo5_fails(saves))
                message = str(caught.exception)
                self.assertTrue(
                    message.startswith(
                        "demo 5 failed while rebuilding the boot snapshot (`{}` exited "
                        "with status 2; its output is above), and {}".format(
                            " ".join(self.demo5_command), problem
                        )
                    ),
                    message,
                )
                self.assertTrue(
                    message.endswith(". Run demos/clean.sh, then demo 6 again"), message
                )
                self.assertIsInstance(
                    caught.exception.__cause__, subprocess.CalledProcessError
                )

    def test_a_missing_snapshot_whose_demo5_fails_still_fails(self):
        # Only the rebuild of a snapshot from another initramfs is judged by the
        # record; when there was no snapshot, demo 5's failure stays the verdict.
        with self.assertRaises(subprocess.CalledProcessError):
            self.ensure(self.snapshot, demo5=self.demo5_fails(self.demo5_saves()))

    def test_a_default_snapshot_without_a_record_is_rebuilt(self):
        # A snapshot demo 5 saved before it wrote records may hold an /init of
        # an older frame format. Demo 6 used to restore it because it existed.
        self.snapshot.write_bytes(SNAPSHOT)
        printed = self.ensure(self.snapshot, demo5=self.demo5_saves())
        self.assertEqual(self.commands, [self.demo5_command])
        self.assertIn(
            "Demo 5 boot snapshot {} is not from the current initramfs: it has no "
            "record".format(self.snapshot),
            printed,
        )
        self.assertEqual(self.snapshot.read_bytes(), b"a snapshot of the current initramfs")
        dc.verify_boot_snapshot(self.snapshot, self.root, self.assets)

    def test_a_default_snapshot_of_an_older_initramfs_is_rebuilt(self):
        for change, reason in (
            (
                lambda: _write_assets_script(self.root, 10),
                "it was booted from initramfs version 9, and demos/lib/qemu-assets.sh "
                "now builds version 10",
            ),
            (
                lambda: self.initramfs.write_bytes(b"another initramfs"),
                "it was booted from an initramfs with SHA-256 {}".format(_sha256(INITRAMFS)),
            ),
        ):
            with self.subTest(reason=reason):
                _write_assets_script(self.root, 9)
                self.initramfs.write_bytes(INITRAMFS)
                self.snapshot.write_bytes(SNAPSHOT)
                self.record(self.snapshot)
                change()
                self.commands.clear()
                printed = self.ensure(self.snapshot, demo5=self.demo5_saves())
                self.assertEqual(self.commands, [self.demo5_command])
                self.assertIn(reason, printed)
                dc.verify_boot_snapshot(self.snapshot, self.root, self.assets)

    def test_a_missing_default_snapshot_is_built(self):
        printed = self.ensure(self.snapshot, demo5=self.demo5_saves())
        self.assertEqual(self.commands, [self.demo5_command])
        self.assertIn("Demo 5 boot snapshot missing; running demo 5 first...", printed)

    def test_demo5_that_leaves_a_snapshot_without_a_record_fails(self):
        self.snapshot.write_bytes(SNAPSHOT)

        def demo5():
            self.snapshot.write_bytes(b"a snapshot without a record")

        with self.assertRaises(RuntimeError) as caught:
            self.ensure(self.snapshot, demo5=demo5)
        self.assertTrue(
            str(caught.exception).startswith(
                "Demo 5 ran, but {} still does not match the current initramfs: it has "
                "no record".format(self.snapshot)
            ),
            str(caught.exception),
        )

    def test_demo5_that_saves_no_snapshot_fails(self):
        with self.assertRaises(RuntimeError) as caught:
            self.ensure(self.snapshot)
        self.assertEqual(str(caught.exception), "Demo 5 did not produce {}".format(self.snapshot))

    def test_a_custom_snapshot_without_a_matching_record_is_refused(self):
        custom = self.directory / "custom-boot.qcow2"
        record_path = self.directory / "custom-boot.qcow2.producer.json"
        cases = (
            ("no record", lambda: None, "it has no record of the initramfs it was booted from"),
            (
                "older version",
                lambda: (self.record(custom), _write_assets_script(self.root, 10)),
                "it was booted from initramfs version 9, and demos/lib/qemu-assets.sh "
                "now builds version 10",
            ),
            (
                "another initramfs",
                lambda: (self.record(custom), self.initramfs.write_bytes(b"another initramfs")),
                "it was booted from an initramfs with SHA-256 {}".format(_sha256(INITRAMFS)),
            ),
            (
                "replaced snapshot",
                lambda: (self.record(custom), custom.write_bytes(b"replaced")),
                "{} has SHA-256 {}".format(custom, _sha256(b"replaced")),
            ),
        )
        for name, prepare, reason in cases:
            with self.subTest(case=name):
                _write_assets_script(self.root, 9)
                self.initramfs.write_bytes(INITRAMFS)
                record_path.unlink(missing_ok=True)
                custom.write_bytes(SNAPSHOT)
                prepare()
                before = custom.read_bytes()
                had_record = record_path.exists()
                self.commands.clear()
                with self.assertRaises(RuntimeError) as caught:
                    self.ensure(custom, demo5=self.demo5_saves())
                message = str(caught.exception)
                self.assertTrue(
                    message.startswith(
                        "refusing to restore the custom boot snapshot {} "
                        "(QEMU_BOOT_SNAPSHOT_DISK): {}".format(custom, reason)
                    ),
                    message,
                )
                # Why it matters, and how to rebuild it.
                self.assertIn("an /init from an older initramfs runs the command as root", message)
                self.assertIn(
                    "Rebuild it by running demo 5 with QEMU_SNAPSHOT_DISK={} and the same "
                    "QEMU_ASSETS".format(custom),
                    message,
                )
                self.assertIsInstance(caught.exception.__cause__, dc.BootSnapshotMismatch)
                # Demo 6 neither ran demo 5 nor touched the custom snapshot.
                self.assertEqual(self.commands, [])
                self.assertEqual(custom.read_bytes(), before)
                self.assertEqual(record_path.exists(), had_record)
                self.assertFalse(self.snapshot.exists())

    def test_a_custom_snapshot_of_the_current_initramfs_is_used(self):
        custom = self.directory / "custom-boot.qcow2"
        custom.write_bytes(SNAPSHOT)
        self.record(custom)
        self.ensure(custom)
        self.assertEqual(self.commands, [])

    def test_a_symlinked_custom_snapshot_is_judged_by_the_record_next_to_its_name(self):
        # Demo 6 reads the record next to the name QEMU_BOOT_SNAPSHOT_DISK
        # gives, never next to the symlink's target; demo 7 does the same
        # (Demo7SymlinkedSnapshotTest).
        alias = self.symlinked_snapshot()
        self.record(alias)
        self.ensure(alias, demo5=self.demo5_saves())
        self.assertEqual(self.commands, [])
        for name, prepare, reason in self.symlinked_snapshot_cases(alias):
            with self.subTest(case=name):
                self.clear_records(alias)
                prepare()
                with self.assertRaises(RuntimeError) as caught:
                    self.ensure(alias, demo5=self.demo5_saves())
                self.assertTrue(
                    str(caught.exception).startswith(
                        "refusing to restore the custom boot snapshot {} "
                        "(QEMU_BOOT_SNAPSHOT_DISK): {}".format(alias, reason(alias))
                    ),
                    str(caught.exception),
                )
                self.assertEqual(self.commands, [])

    def test_a_missing_custom_snapshot_is_refused_as_before(self):
        custom = self.directory / "custom-boot.qcow2"
        with self.assertRaises(RuntimeError) as caught:
            self.ensure(custom, demo5=self.demo5_saves())
        self.assertEqual(
            str(caught.exception),
            "missing custom boot snapshot: {}; produce it before Demo 6".format(custom),
        )
        self.assertEqual(self.commands, [])


class ReachedHermit(Exception):
    """Raised by the stand-in for starting Hermit."""


class Demo6ResumeCopyTest(_StandIns):
    """Demo 6 checks the copy of the boot snapshot that QEMU restores."""

    @classmethod
    def setUpClass(cls):
        cls.demo6 = runpy.run_path(str(DEMOS_DIR / "06-qemu-resume" / "run.py"))

    def resume(self, after_check=None, boot_snapshot=None):
        """Run resume_once until it would start Hermit.

        ensure_boot_snapshot is replaced by ``after_check``, which runs where
        the real one would have accepted the snapshot. ``boot_snapshot`` is
        QEMU_BOOT_SNAPSHOT_DISK, the default snapshot unless given. Returns what
        the demo raised and how often it started Hermit and released the demo
        lock.
        """
        resume_once = self.demo6["resume_once"]
        started = []
        released = []

        def start_hermit(command, **keywords):
            started.append(command)
            raise ReachedHermit()

        replacements = {
            "ASSETS": self.assets,
            "ROOT": self.root,
            "QEMU": "qemu-system-x86_64",
            "SNAPSHOT_DISK": self.assets / "hermit-snapshot.qcow2",
            "BOOT_SNAPSHOT_DISK": self.snapshot if boot_snapshot is None else boot_snapshot,
            "check_dependencies": lambda root: "dependency check replaced by the test",
            "hermit_binary": lambda: "hermit",
            "ensure_boot_snapshot": after_check or (lambda: None),
            "acquire_demo_lock": lambda path: "lock",
            "release_demo_lock": lambda handle: released.append(handle),
            "stage_guest_controller": lambda destination: destination,
            "subprocess": types.SimpleNamespace(
                Popen=start_hermit,
                DEVNULL=subprocess.DEVNULL,
                PIPE=subprocess.PIPE,
                STDOUT=subprocess.STDOUT,
            ),
            "stop_process": lambda process: None,
            "stop_process_group": lambda process: None,
        }
        printed = io.StringIO()
        with mock.patch.dict(resume_once.__globals__, replacements), mock.patch.dict(
            os.environ
        ), contextlib.redirect_stdout(printed):
            try:
                resume_once("echo done", False)
            except Exception as error:  # noqa: BLE001 - the outcome under test
                return error, started, released
        self.fail("resume_once returned without starting Hermit")

    def test_a_snapshot_replaced_after_the_check_is_not_restored(self):
        # Demo 5 does not take the demo lock: it can publish a new snapshot
        # between demo 6's check and its copy, before it writes the new record.
        self.snapshot.write_bytes(SNAPSHOT)
        self.record(self.snapshot)

        def demo5_publishes_after_the_check():
            self.snapshot.write_bytes(b"a snapshot published after the check")

        outcome, started, released = self.resume(demo5_publishes_after_the_check)
        self.assertIs(type(outcome), RuntimeError, repr(outcome))
        copy = self.assets / "hermit-snapshot.qcow2"
        self.assertEqual(
            str(outcome),
            "the copy {} of the boot snapshot {} does not match demo 5's record: {} "
            "has SHA-256 {}, not the {} that demo 5 recorded for it; demo 5 may have "
            "replaced the boot snapshot after this demo checked it, so run demo 6 "
            "again".format(
                copy,
                self.snapshot,
                copy,
                _sha256(b"a snapshot published after the check"),
                _sha256(SNAPSHOT),
            ),
        )
        self.assertEqual(started, [])
        self.assertEqual(released, ["lock"])

    def test_an_unchanged_snapshot_is_restored(self):
        # Positive control: the same run with nothing replaced starts Hermit.
        self.snapshot.write_bytes(SNAPSHOT)
        self.record(self.snapshot)
        outcome, started, released = self.resume()
        self.assertIsInstance(outcome, ReachedHermit)
        self.assertEqual(len(started), 1)
        self.assertEqual(released, ["lock"])

    def test_a_symlinked_boot_snapshot_is_checked_against_the_record_next_to_its_name(self):
        # The copy is checked against the record next to the name
        # QEMU_BOOT_SNAPSHOT_DISK gives, the record ensure_boot_snapshot read;
        # demo 7 does the same (Demo7SymlinkedSnapshotTest).
        alias = self.symlinked_snapshot()
        copy = self.assets / "hermit-snapshot.qcow2"
        self.record(alias)
        outcome, started, released = self.resume(boot_snapshot=alias)
        self.assertIsInstance(outcome, ReachedHermit, repr(outcome))
        self.assertEqual(len(started), 1)
        for name, prepare, reason in self.symlinked_snapshot_cases(alias):
            with self.subTest(case=name):
                self.clear_records(alias)
                prepare()
                outcome, started, released = self.resume(boot_snapshot=alias)
                self.assertIs(type(outcome), RuntimeError, repr(outcome))
                self.assertTrue(
                    str(outcome).startswith(
                        "the copy {} of the boot snapshot {} does not match demo 5's "
                        "record: {}".format(copy, alias, reason(copy))
                    ),
                    str(outcome),
                )
                self.assertEqual(started, [])
                self.assertEqual(released, ["lock"])


class ReachedTheNextStep(Exception):
    """Raised by the stand-in for saving the run's metadata."""


class Demo5RecordTest(_StandIns):
    """Demo 5 writes the record next to each boot snapshot it saves."""

    @classmethod
    def setUpClass(cls):
        cls.demo5 = runpy.run_path(str(DEMOS_DIR / "05-qemu-boot" / "run.py"))

    def setUp(self):
        super().setUp()
        (self.assets / "bzImage").write_bytes(b"stand-in for the kernel")
        # The build record qemu-assets.sh writes after it builds the initramfs.
        self.build_record = self.assets / ".initramfs-build"
        self.build_record.write_text("9 {}\n".format(_sha256(INITRAMFS)))
        # The Hermit command lines boot_once starts.
        self.commands = []

    def host_file_the_guest_reads(self, command, option: str) -> Path:
        """The host file behind the guest path ``command`` gives the guest
        controller after ``option``: the last --bind whose target covers it is
        the mount the guest sees there."""
        guest = Path(command[command.index(option) + 1])
        host = None
        for index, argument in enumerate(command[: command.index("--")]):
            if argument != "--bind":
                continue
            source, target = command[index + 1].rsplit(":", 1)
            try:
                host = Path(source) / guest.relative_to(target)
            except ValueError:
                continue
        self.assertIsNotNone(host, "no --bind covers {}".format(guest))
        return host

    def boot(
        self, snapshot_disk_override=None, during_boot=None, serial_text="2022-01-01T00:00:00\n"
    ) -> None:
        """Run boot_once with QEMU and Hermit replaced, until it saves metadata.

        ``during_boot`` runs while Hermit would be booting the guest, and
        ``serial_text`` is the serial transcript the guest writes.
        """
        boot_once = self.demo5["boot_once"]
        copier = mock.Mock()
        copier.is_alive.return_value = False

        def boot_the_guest(process, timeout, **keywords):
            if during_boot is not None:
                during_boot()
            Path(keywords["stream_path"]).write_text(serial_text)
            return 0

        def save_the_snapshot(path, name):
            Path(path).write_bytes(SNAPSHOT)
            return True

        def save_metadata(run_dir, disk, info_log, fields):
            raise ReachedTheNextStep()

        replacements = {
            "ASSETS": self.assets,
            "ROOT": self.root,
            "QEMU": "qemu-system-x86_64",
            "SNAPSHOT_DISK_OVERRIDE": snapshot_disk_override,
            "check_qemu_dependencies": lambda root: "QEMU dependency check replaced by the test",
            "check_dependencies": lambda root: "dependency check replaced by the test",
            "hermit_binary": lambda: "hermit",
            # qemu-assets.sh, `qemu-img create` and `qemu-img snapshot -l`.
            "run_checked": lambda *arguments, **keywords: None,
            "stage_guest_controller": lambda destination: destination,
            "hermit_tmp_args": lambda root: [],
            "subprocess": types.SimpleNamespace(
                Popen=lambda command, **keywords: self.commands.append(command)
                or mock.Mock(),
                DEVNULL=subprocess.DEVNULL,
                PIPE=subprocess.PIPE,
                STDOUT=subprocess.STDOUT,
            ),
            "start_output_copier": lambda process, log: copier,
            "wait_for_process": boot_the_guest,
            "drain_output": lambda *arguments, **keywords: None,
            "stop_process_group": lambda process: None,
            "stop_process": lambda process: None,
            "snapshot_exists": save_the_snapshot,
            "canonicalize_qcow2_snapshot_timestamp": lambda path, name: None,
            "extract_info_tail": lambda path: [],
            "save_metadata": save_metadata,
        }
        printed = io.StringIO()
        with mock.patch.dict(boot_once.__globals__, replacements), mock.patch.dict(
            os.environ
        ), contextlib.redirect_stdout(printed):
            with self.assertRaises(ReachedTheNextStep):
                boot_once()

    def assert_record(self, snapshot: Path, initramfs: bytes) -> None:
        record = dc.boot_snapshot_record_path(snapshot)
        self.assertEqual(
            json.loads(record.read_text()),
            {
                "format": 1,
                "initramfs_sha256": _sha256(initramfs),
                "initramfs_version": 9,
                "snapshot_sha256": _sha256(SNAPSHOT),
            },
        )

    def test_the_published_snapshot_has_a_record_that_demo_6_accepts(self):
        self.boot()
        self.assertEqual(self.snapshot.read_bytes(), SNAPSHOT)
        self.assert_record(self.snapshot, INITRAMFS)
        dc.verify_boot_snapshot(self.snapshot, self.root, self.assets)

    def test_the_record_names_the_initramfs_the_boot_started_from(self):
        # qemu-assets.sh run by another demo rebuilds the initramfs while this
        # boot runs: the snapshot holds the /init of the one it started from.
        self.boot(during_boot=lambda: self.initramfs.write_bytes(b"rebuilt during the boot"))
        self.assert_record(self.snapshot, INITRAMFS)
        with self.assertRaises(dc.BootSnapshotMismatch):
            dc.verify_boot_snapshot(self.snapshot, self.root, self.assets)

    def test_a_snapshot_saved_at_qemu_snapshot_disk_has_a_record_too(self):
        # The path demo 6's refusal tells the user to rebuild a custom snapshot at.
        custom = self.directory / "custom-boot.qcow2"
        self.boot(snapshot_disk_override=str(custom))
        self.assertEqual(custom.read_bytes(), SNAPSHOT)
        self.assert_record(custom, INITRAMFS)
        dc.verify_boot_snapshot(custom, self.root, self.assets)
        self.assert_record(self.snapshot, INITRAMFS)

    def test_the_record_names_the_bytes_qemu_boots(self):
        # Another checkout replaces the shared initramfs with its version 8
        # archive after this boot took its record of the initramfs and before
        # QEMU reads -initrd, then puts version 9 back before the boot ends.
        # The record must name the bytes QEMU read.
        version_8 = b"stand-in for the version 8 initramfs"
        booted = []

        def replace_the_initramfs_while_qemu_reads_it():
            self.initramfs.write_bytes(version_8)
            command = self.commands[-1]
            booted.append(self.host_file_the_guest_reads(command, "--initrd").read_bytes())
            self.initramfs.write_bytes(INITRAMFS)

        self.boot(during_boot=replace_the_initramfs_while_qemu_reads_it)
        record = json.loads(dc.boot_snapshot_record_path(self.snapshot).read_text())
        self.assertEqual(record["initramfs_sha256"], _sha256(booted[0]))
        self.assert_record(self.snapshot, INITRAMFS)
        # The copies QEMU booted are not kept with the run.
        self.assertEqual(list(self.assets.glob(".work/*/boot-assets")), [])

    def assert_refused_before_the_boot(self, expected: str) -> None:
        with self.assertRaises(RuntimeError) as caught:
            self.boot()
        self.assertIn(expected, str(caught.exception))
        self.assertEqual(self.commands, [])
        self.assertFalse(dc.boot_snapshot_record_path(self.snapshot).exists())
        self.assertEqual(list(self.assets.glob(".work/*/boot-assets")), [])

    def test_a_build_record_of_other_bytes_is_refused_before_the_boot(self):
        # The shared initramfs was replaced after qemu-assets.sh built it, by
        # a checkout whose qemu-assets.sh writes no build record.
        self.initramfs.write_bytes(b"stand-in for an initramfs built elsewhere")
        self.assert_refused_before_the_boot(
            "its build record {} says '9 {}'".format(self.build_record, _sha256(INITRAMFS))
        )

    def test_a_build_record_of_another_version_is_refused_before_the_boot(self):
        self.build_record.write_text("8 {}\n".format(_sha256(INITRAMFS)))
        self.assert_refused_before_the_boot(
            "is not one that demos/lib/qemu-assets.sh built at INITRAMFS_VERSION 9"
        )

    def test_without_a_build_record_the_boot_is_refused(self):
        self.build_record.unlink()
        self.assert_refused_before_the_boot("its build record {} says".format(self.build_record))

    def test_a_snapshot_disk_in_the_asset_directory_keeps_its_guest_path(self):
        custom = self.assets / "custom-boot.qcow2"
        seen = {}

        def look_while_the_guest_runs():
            command = self.commands[-1]
            seen["disk"] = command[command.index("--disk") + 1]
            seen["disk host file"] = self.host_file_the_guest_reads(command, "--disk")
            seen["kernel"] = self.host_file_the_guest_reads(command, "--kernel").read_bytes()

        self.boot(snapshot_disk_override=str(custom), during_boot=look_while_the_guest_runs)
        self.assertEqual(
            seen,
            {
                "disk": "/tmp/hermit-demo-assets/custom-boot.qcow2",
                "disk host file": custom,
                "kernel": b"stand-in for the kernel",
            },
        )
        self.assert_record(custom, INITRAMFS)

    # Review finding R7-6 on https://github.com/rrnewton/hermit/pull/3703:
    # demo 5 published the snapshot and wrote its record before it checked the
    # serial transcript for the fixed RTC date. A boot that failed that check
    # left a published snapshot with a matching record, and demos 6 and 7
    # accept such a snapshot when demo 5 exits non-zero after a rebuild
    # (accept_snapshot_after_failed_rebuild).

    def boot_without_the_rtc_date(self, snapshot_disk_override=None) -> None:
        """Boot as boot() does, with a serial transcript that lacks the fixed
        RTC date, and require the boot to fail that check."""
        with self.assertRaises(RuntimeError) as caught:
            self.boot(
                snapshot_disk_override=snapshot_disk_override,
                serial_text="rtc: 2026-10-04T10:31:07Z\n",
            )
        self.assertEqual(str(caught.exception), "serial transcript lacks the fixed RTC epoch")

    def test_a_boot_that_fails_the_rtc_check_publishes_no_snapshot(self):
        self.boot_without_the_rtc_date()
        self.assertFalse(self.snapshot.exists())
        self.assertFalse(dc.boot_snapshot_record_path(self.snapshot).exists())

    def test_a_boot_that_fails_the_rtc_check_leaves_a_stale_snapshot_refused(self):
        # The snapshot demo 6 found stale and ran demo 5 to rebuild: its record
        # names the version 8 initramfs, and qemu-assets.sh now builds version 9.
        self.snapshot.write_bytes(b"a snapshot booted from the version 8 initramfs")
        stale = dc.write_boot_snapshot_record(
            self.snapshot,
            dc.hash_file(self.snapshot),
            {"initramfs_version": 8, "initramfs_sha256": _sha256(b"version 8 initramfs")},
        )
        before = (self.snapshot.read_bytes(), stale.read_bytes())
        self.boot_without_the_rtc_date()
        self.assertEqual((self.snapshot.read_bytes(), stale.read_bytes()), before)
        # So demo 6, after demo 5 exits non-zero, does not use it.
        failure = subprocess.CalledProcessError(1, ["python3", "demos/05-qemu-boot/run.py"])
        printed = io.StringIO()
        with self.assertRaises(RuntimeError) as caught, contextlib.redirect_stdout(printed):
            dc.accept_snapshot_after_failed_rebuild(
                self.snapshot, self.root, self.assets, failure, "demo 6"
            )
        self.assertIn(
            "it was booted from initramfs version 8, and demos/lib/qemu-assets.sh now "
            "builds version 9",
            str(caught.exception),
        )
        self.assertEqual(printed.getvalue(), "")

    def test_a_boot_at_qemu_snapshot_disk_that_fails_the_rtc_check_has_no_record(self):
        custom = self.directory / "custom-boot.qcow2"
        self.boot_without_the_rtc_date(snapshot_disk_override=str(custom))
        # QEMU saved the snapshot there, but demo 5 wrote nothing next to it.
        self.assertEqual(custom.read_bytes(), SNAPSHOT)
        self.assertFalse(dc.boot_snapshot_record_path(custom).exists())
        self.assertFalse(custom.with_suffix(custom.suffix + ".id").exists())
        with self.assertRaises(dc.BootSnapshotMismatch):
            dc.verify_boot_snapshot(custom, self.root, self.assets)
        self.assertFalse(self.snapshot.exists())
        self.assertFalse(dc.boot_snapshot_record_path(self.snapshot).exists())


class WriteBootSnapshotRecordDocumentationTest(unittest.TestCase):
    """write_boot_snapshot_record's docstring names the producer demo 5 passes.

    Review finding R7-8 on https://github.com/rrnewton/hermit/pull/3703: it
    said ``producer`` "is what initramfs_producer returned before the boot",
    but demo 5 passes what booted_initramfs_producer returned for the private
    copy of the initramfs that QEMU boots.
    """

    def test_the_docstring_names_booted_initramfs_producer(self):
        doc = " ".join(dc.write_boot_snapshot_record.__doc__.split())
        self.assertIn(
            "Demo 5 passes what booted_initramfs_producer returned before the boot for "
            "the private copy of the initramfs that QEMU boots",
            doc,
        )
        self.assertNotIn("is what initramfs_producer returned", doc)

    def test_demo_5_passes_what_booted_initramfs_producer_returned(self):
        # What the docstring says is what demo 5 does: every record it writes
        # gets ``producer``, and ``producer`` is set once, from
        # booted_initramfs_producer.
        tree = ast.parse((DEMOS_DIR / "05-qemu-boot" / "run.py").read_text())
        records = [
            node.args[2]
            for node in ast.walk(tree)
            if isinstance(node, ast.Call)
            and isinstance(node.func, ast.Name)
            and node.func.id == "write_boot_snapshot_record"
        ]
        producer = ast.dump(ast.Name(id="producer", ctx=ast.Load()))
        self.assertEqual([ast.dump(argument) for argument in records], [producer, producer])
        assigned = [
            node.value
            for node in ast.walk(tree)
            if isinstance(node, (ast.Assign, ast.AnnAssign, ast.AugAssign))
            and any(
                isinstance(target, ast.Name) and target.id == "producer"
                for target in (node.targets if isinstance(node, ast.Assign) else [node.target])
            )
        ]
        self.assertEqual(len(assigned), 1)
        self.assertIsInstance(assigned[0], ast.Call)
        self.assertEqual(
            ast.dump(assigned[0].func),
            ast.dump(ast.Name(id="booted_initramfs_producer", ctx=ast.Load())),
        )


class Demo7EnsureBootSnapshotTest(_StandIns):
    """Demo 7 accepts the boot snapshot by demo 6's rules
    (drgn_hermit.ensure_boot_snapshot), not because it exists."""

    def setUp(self):
        super().setUp()
        self.rebuilds = 0

    def ensure(self, snapshot: Path, demo5=None) -> str:
        """Run ensure_boot_snapshot for ``snapshot``; ``demo5`` stands in for
        demo 5 when it is run. Returns what was printed."""

        def rebuild():
            self.rebuilds += 1
            if demo5 is not None:
                demo5()

        printed = io.StringIO()
        with contextlib.redirect_stdout(printed):
            dh.ensure_boot_snapshot(snapshot, self.root, self.assets, rebuild)
        return printed.getvalue()

    def demo5_saves(self, data: bytes = b"a snapshot of the current initramfs"):
        def demo5():
            self.snapshot.write_bytes(data)
            self.record(self.snapshot)

        return demo5

    def test_a_default_snapshot_of_the_current_initramfs_is_used_as_it_is(self):
        self.snapshot.write_bytes(SNAPSHOT)
        self.record(self.snapshot)
        self.ensure(self.snapshot)
        self.assertEqual(self.rebuilds, 0)
        self.assertEqual(self.snapshot.read_bytes(), SNAPSHOT)

    def test_a_default_snapshot_without_a_matching_record_is_rebuilt(self):
        # Demo 7 used to restore any snapshot that existed.
        for name, prepare, reason in (
            ("no record", lambda: None, "it has no record"),
            (
                "older version",
                lambda: (self.record(self.snapshot), _write_assets_script(self.root, 10)),
                "it was booted from initramfs version 9, and demos/lib/qemu-assets.sh "
                "now builds version 10",
            ),
            (
                "replaced snapshot",
                lambda: (self.record(self.snapshot), self.snapshot.write_bytes(b"replaced")),
                "{} has SHA-256 {}".format(self.snapshot, _sha256(b"replaced")),
            ),
        ):
            with self.subTest(case=name):
                _write_assets_script(self.root, 9)
                dc.boot_snapshot_record_path(self.snapshot).unlink(missing_ok=True)
                self.snapshot.write_bytes(SNAPSHOT)
                prepare()
                self.rebuilds = 0
                printed = self.ensure(self.snapshot, demo5=self.demo5_saves())
                self.assertEqual(self.rebuilds, 1)
                self.assertIn(
                    "Demo 5 boot snapshot {} is not from the current initramfs: "
                    "{}".format(self.snapshot, reason),
                    printed,
                )
                dc.verify_boot_snapshot(self.snapshot, self.root, self.assets)

    def test_a_missing_default_snapshot_is_built(self):
        printed = self.ensure(self.snapshot, demo5=self.demo5_saves())
        self.assertEqual(self.rebuilds, 1)
        self.assertIn("Demo 5 boot snapshot missing; running demo 5 first...", printed)

    def test_demo5_that_leaves_a_snapshot_without_a_record_fails(self):
        self.snapshot.write_bytes(SNAPSHOT)

        def demo5():
            self.snapshot.write_bytes(b"a snapshot without a record")

        with self.assertRaises(RuntimeError) as caught:
            self.ensure(self.snapshot, demo5=demo5)
        self.assertTrue(
            str(caught.exception).startswith(
                "Demo 5 ran, but {} still does not match the current initramfs: it has "
                "no record".format(self.snapshot)
            ),
            str(caught.exception),
        )

    def test_demo5_that_saves_no_snapshot_fails(self):
        with self.assertRaises(RuntimeError) as caught:
            self.ensure(self.snapshot)
        self.assertEqual(str(caught.exception), "Demo 5 did not produce {}".format(self.snapshot))

    def test_a_custom_snapshot_without_a_matching_record_is_refused(self):
        custom = self.directory / "custom-boot.qcow2"
        record_path = dc.boot_snapshot_record_path(custom)
        for name, prepare, reason in (
            ("no record", lambda: None, "it has no record of the initramfs it was booted from"),
            (
                "another initramfs",
                lambda: (self.record(custom), self.initramfs.write_bytes(b"another initramfs")),
                "it was booted from an initramfs with SHA-256 {}".format(_sha256(INITRAMFS)),
            ),
            (
                "replaced snapshot",
                lambda: (self.record(custom), custom.write_bytes(b"replaced")),
                "{} has SHA-256 {}".format(custom, _sha256(b"replaced")),
            ),
        ):
            with self.subTest(case=name):
                self.initramfs.write_bytes(INITRAMFS)
                record_path.unlink(missing_ok=True)
                custom.write_bytes(SNAPSHOT)
                prepare()
                before = custom.read_bytes()
                self.rebuilds = 0
                with self.assertRaises(RuntimeError) as caught:
                    self.ensure(custom, demo5=self.demo5_saves())
                message = str(caught.exception)
                self.assertTrue(
                    message.startswith(
                        "refusing to restore the custom boot snapshot {} "
                        "(DEMO07_SNAPSHOT_DISK): {}".format(custom, reason)
                    ),
                    message,
                )
                self.assertIn(
                    "Rebuild it by running demo 5 with QEMU_SNAPSHOT_DISK={} and the same "
                    "QEMU_ASSETS, or unset DEMO07_SNAPSHOT_DISK".format(custom),
                    message,
                )
                self.assertIsInstance(caught.exception.__cause__, dc.BootSnapshotMismatch)
                # Demo 5 did not run, and nothing was touched.
                self.assertEqual(self.rebuilds, 0)
                self.assertEqual(custom.read_bytes(), before)
                self.assertFalse(self.snapshot.exists())

    def test_a_custom_snapshot_of_the_current_initramfs_is_used(self):
        custom = self.directory / "custom-boot.qcow2"
        custom.write_bytes(SNAPSHOT)
        self.record(custom)
        self.ensure(custom)
        self.assertEqual(self.rebuilds, 0)

    def test_a_missing_custom_snapshot_is_refused(self):
        custom = self.directory / "custom-boot.qcow2"
        with self.assertRaises(RuntimeError) as caught:
            self.ensure(custom, demo5=self.demo5_saves())
        self.assertEqual(
            str(caught.exception),
            "missing custom boot snapshot: {}; produce it before demo 7".format(custom),
        )
        self.assertEqual(self.rebuilds, 0)

    def demo5_fails(self, saves=None):
        """A stand-in for demo 5 that runs ``saves``, then fails the way
        subprocess.run(check=True) does when make exits 2."""

        def demo5():
            if saves is not None:
                saves()
            raise subprocess.CalledProcessError(2, self.demo5_command)

        return demo5

    demo5_command = ["make", "--no-print-directory", "-C", str(DEMOS_DIR), "demo5"]

    def test_a_rebuild_whose_demo5_fails_after_saving_a_current_snapshot_uses_it(self):
        self.snapshot.write_bytes(SNAPSHOT)
        printed = self.ensure(self.snapshot, demo5=self.demo5_fails(self.demo5_saves()))
        self.assertEqual(self.rebuilds, 1)
        self.assertIn(
            "NOTE: demo 5 failed (`{}` exited with status 2), but it saved {} with a "
            "record that matches the current initramfs, so demo 7 uses that "
            "snapshot.".format(" ".join(self.demo5_command), self.snapshot),
            printed,
        )
        dc.verify_boot_snapshot(self.snapshot, self.root, self.assets)

    def test_a_rebuild_whose_demo5_fails_without_a_current_snapshot_stops(self):
        self.snapshot.write_bytes(SNAPSHOT)
        with self.assertRaises(RuntimeError) as caught:
            self.ensure(self.snapshot, demo5=self.demo5_fails())
        message = str(caught.exception)
        self.assertTrue(
            message.startswith(
                "demo 5 failed while rebuilding the boot snapshot (`{}` exited with "
                "status 2; its output is above), and {} does not match the current "
                "initramfs: it has no record".format(
                    " ".join(self.demo5_command), self.snapshot
                )
            ),
            message,
        )
        self.assertTrue(message.endswith(". Run demos/clean.sh, then demo 7 again"), message)
        self.assertIsInstance(caught.exception.__cause__, subprocess.CalledProcessError)

    def test_a_missing_snapshot_whose_demo5_fails_still_fails(self):
        with self.assertRaises(subprocess.CalledProcessError):
            self.ensure(self.snapshot, demo5=self.demo5_fails(self.demo5_saves()))


class Demo7RestoreCopyTest(_StandIns):
    """Each demo 7 pass checks the copy of the boot snapshot QEMU restores."""

    def setUp(self):
        super().setUp()
        for name in ("hermit", "qemu", "bzImage", "vmlinux"):
            (self.directory / name).write_bytes(b"")
        self.snapshot.write_bytes(SNAPSHOT)
        self.record(self.snapshot)
        environment = mock.patch.dict(
            os.environ, {"QEMU_SOCKET_DIR": str(self.directory / "sockets")}
        )
        environment.start()
        self.addCleanup(environment.stop)
        self.config = dh.GuestConfig(
            root=self.root,
            hermit=self.directory / "hermit",
            qemu=self.directory / "qemu",
            kernel=self.directory / "bzImage",
            initrd=self.initramfs,
            vmlinux=self.directory / "vmlinux",
            snapshot_disk=self.snapshot,
            snapshot_name="hermit-boot",
            advance_command="echo deterministic",
            artifact_dir=self.directory / "artifacts",
            assets=self.assets,
        )

    def start(self, copy=None):
        """Run start() until it would start Hermit; ``copy`` replaces the copy
        of the snapshot. Returns the error start() raised and whether Hermit
        was started."""
        popen = mock.Mock(side_effect=ReachedHermit)
        program = dh.HermitGuestProgram(self.config)
        with contextlib.ExitStack() as stack:
            stack.enter_context(
                mock.patch.object(dh, "ensure_vmlinux", return_value=self.config.vmlinux)
            )
            stack.enter_context(mock.patch.object(dh.subprocess, "Popen", popen))
            if copy is not None:
                stack.enter_context(mock.patch.object(dh.shutil, "copyfile", copy))
            try:
                program.start()
            except Exception as error:  # noqa: BLE001 - returned to the test
                caught = error
            else:
                caught = None
        return caught, popen.called

    def test_a_matching_copy_is_restored(self):
        caught, started = self.start()
        self.assertIsInstance(caught, ReachedHermit)
        self.assertTrue(started)

    def test_a_copy_that_does_not_match_the_record_is_not_restored(self):
        # The snapshot still matches its record; only the copy QEMU would
        # restore does not. A check of the snapshot alone would accept it.
        def copy_other_bytes(source, destination):
            Path(destination).write_bytes(b"other bytes than the snapshot")

        caught, started = self.start(copy=copy_other_bytes)
        self.assertIsInstance(caught, RuntimeError)
        self.assertIsInstance(caught.__cause__, dc.BootSnapshotMismatch)
        message = str(caught)
        self.assertRegex(
            message,
            r"^the copy \S+/snapshot\.qcow2 of the boot snapshot {} does not match "
            r"demo 5's record: \S+/snapshot\.qcow2 has SHA-256 {}".format(
                re.escape(str(self.snapshot)), _sha256(b"other bytes than the snapshot")
            ),
        )
        self.assertFalse(started)
        dc.verify_boot_snapshot(self.snapshot, self.root, self.assets)

    def test_a_snapshot_replaced_after_the_check_is_not_restored(self):
        self.snapshot.write_bytes(b"replaced after the check")
        caught, started = self.start()
        self.assertIsInstance(caught, RuntimeError)
        self.assertIsInstance(caught.__cause__, dc.BootSnapshotMismatch)
        self.assertIn("demo 5 may have replaced the boot snapshot", str(caught))
        self.assertFalse(started)

    def test_a_snapshot_of_another_initramfs_is_not_restored(self):
        self.initramfs.write_bytes(b"another initramfs")
        caught, started = self.start()
        self.assertIsInstance(caught, RuntimeError)
        self.assertIn(
            "it was booted from an initramfs with SHA-256 {}".format(_sha256(INITRAMFS)),
            str(caught),
        )
        self.assertFalse(started)


class Demo7SymlinkedSnapshotTest(_StandIns):
    """Demo 7 reads demo 5's record next to the boot snapshot's name as
    DEMO07_SNAPSHOT_DISK gives it, both before its first pass and when each
    pass checks its copy (https://github.com/rrnewton/hermit/pull/3703). The
    name may be a symlink and is not resolved, so the record next to the
    symlink's target is never the one read; and the record still binds the
    bytes, because each check hashes what the name reaches or the copy QEMU
    restores. These run demo 7's own _config, _ensure_boot_snapshot and a
    pass's start(), in main()'s order, with demo 5, drgn and Hermit replaced."""

    def setUp(self):
        super().setUp()
        for name in ("hermit", "qemu", "bzImage", "vmlinux"):
            (self.directory / name).write_bytes(b"")
        names = ("drgn", "drgn.helpers", "drgn.helpers.linux", "drgn.helpers.linux.list")
        stubs = {name: types.ModuleType(name) for name in names}
        stubs["drgn.helpers.linux.list"].list_for_each_entry = mock.Mock()
        with mock.patch.dict(sys.modules, stubs), mock.patch.object(sys, "path", list(sys.path)):
            self.module = runpy.run_path(
                str(DEMOS_DIR / "07-drgn-kernel" / "task_evolution.py"),
                run_name="demo07_task_evolution",
            )
        self.alias = self.symlinked_snapshot()
        self.rebuilds = []
        for patch in (
            mock.patch.dict(
                self.module["_config"].__globals__,
                {
                    "ROOT": self.root,
                    "hermit_binary": lambda: str(self.directory / "hermit"),
                    "_rebuild_boot_snapshot": lambda: self.rebuilds.append("demo 5"),
                },
            ),
            mock.patch.dict(
                os.environ,
                {
                    "QEMU_BIN": str(self.directory / "qemu"),
                    "DEMO07_KERNEL": str(self.directory / "bzImage"),
                    "DEMO07_INITRD": str(self.initramfs),
                    "DEMO07_VMLINUX": str(self.directory / "vmlinux"),
                    "DEMO07_SNAPSHOT_DISK": str(self.alias),
                    "DEMO07_ARTIFACTS": str(self.directory / "artifacts"),
                    "DEMO07_ASSETS": str(self.assets),
                    "QEMU_SOCKET_DIR": str(self.directory / "sockets"),
                },
            ),
        ):
            patch.start()
            self.addCleanup(patch.stop)

    def first_check(self):
        """Demo 7's check before its first pass; returns what it raised."""
        try:
            with contextlib.redirect_stdout(io.StringIO()):
                self.module["_ensure_boot_snapshot"]()
        except Exception as error:  # noqa: BLE001 - returned to the test
            return error
        return None

    def restore(self, config):
        """Run a pass's start() until it would start Hermit. Returns what it
        raised and whether it started Hermit."""
        popen = mock.Mock(side_effect=ReachedHermit)
        program = dh.HermitGuestProgram(config)
        with mock.patch.object(
            dh, "ensure_vmlinux", return_value=config.vmlinux
        ), mock.patch.object(dh.subprocess, "Popen", popen):
            try:
                program.start()
            except Exception as error:  # noqa: BLE001 - returned to the test
                return error, popen.called
        self.fail("start() returned without starting Hermit")

    def assert_refused(self, first, restored, reason) -> None:
        """Both checks refused the symlinked snapshot, for ``reason``."""
        self.assertIsInstance(first, RuntimeError, repr(first))
        self.assertIsInstance(first.__cause__, dc.BootSnapshotMismatch)
        self.assertTrue(
            str(first).startswith(
                "refusing to restore the custom boot snapshot {} "
                "(DEMO07_SNAPSHOT_DISK): {}".format(self.alias, reason(self.alias))
            ),
            str(first),
        )
        caught, started = restored
        self.assertIsInstance(caught, RuntimeError, repr(caught))
        self.assertIsInstance(caught.__cause__, dc.BootSnapshotMismatch)
        match = re.match(
            r"the copy (\S+/snapshot\.qcow2) of the boot snapshot (\S+) does not "
            r"match demo 5's record: ",
            str(caught),
        )
        self.assertIsNotNone(match, str(caught))
        self.assertEqual(match.group(2), str(self.alias))
        self.assertTrue(
            str(caught)[match.end() :].startswith(reason(match.group(1))), str(caught)
        )
        self.assertFalse(started)
        self.assertEqual(self.rebuilds, [])

    def test_a_valid_snapshot_reached_through_a_symlink_passes_and_is_restored(self):
        # demo 5's record sits next to the name, alias.qcow2, and none next to
        # the target: the check of each copy must read the same record as the
        # first check did.
        self.record(self.alias)
        self.assertFalse(dc.boot_snapshot_record_path(self.alias.resolve()).exists())
        config = self.module["_config"]()
        self.assertIsNone(self.first_check())
        caught, started = self.restore(config)
        self.assertIsInstance(caught, ReachedHermit, repr(caught))
        self.assertTrue(started)
        self.assertEqual(self.rebuilds, [])

    def test_a_symlinked_snapshot_without_a_matching_record_is_refused(self):
        for name, prepare, reason in self.symlinked_snapshot_cases(self.alias):
            with self.subTest(case=name):
                self.clear_records(self.alias)
                prepare()
                config = self.module["_config"]()
                self.assert_refused(self.first_check(), self.restore(config), reason)

    def test_a_symlink_pointed_at_other_bytes_after_the_check_is_not_restored(self):
        # The record binds the bytes, not the name: a symlink that names other
        # bytes by the time a pass copies it is refused at that pass.
        self.record(self.alias)
        config = self.module["_config"]()
        self.assertIsNone(self.first_check())
        other = self.directory / "store" / "boot-2.qcow2"
        other.write_bytes(b"a boot snapshot published after the check")
        self.alias.unlink()
        self.alias.symlink_to(other)
        caught, started = self.restore(config)
        self.assertIsInstance(caught, RuntimeError, repr(caught))
        self.assertRegex(
            str(caught),
            r"^the copy (\S+/snapshot\.qcow2) of the boot snapshot {} does not match "
            r"demo 5's record: \1 has SHA-256 {}, not the {} that demo 5 recorded "
            r"for it".format(
                re.escape(str(self.alias)),
                _sha256(b"a boot snapshot published after the check"),
                _sha256(SNAPSHOT),
            ),
        )
        self.assertFalse(started)


class Demo7ChecksTheSnapshotFirstTest(unittest.TestCase):
    """Demo 7's drgn script checks the boot snapshot before its first pass."""

    def setUp(self):
        names = ("drgn", "drgn.helpers", "drgn.helpers.linux", "drgn.helpers.linux.list")
        stubs = {name: types.ModuleType(name) for name in names}
        stubs["drgn.helpers.linux.list"].list_for_each_entry = mock.Mock()
        with mock.patch.dict(sys.modules, stubs), mock.patch.object(sys, "path", list(sys.path)):
            self.module = runpy.run_path(
                str(DEMOS_DIR / "07-drgn-kernel" / "task_evolution.py"),
                run_name="demo07_task_evolution",
            )
        self.environment = {
            "DEMO07_RUNS": "2",
            "DEMO07_TASK_LIMIT": "16",
            "DEMO07_SNAPSHOT_DISK": "assets-dir/hermit-boot.qcow2",
            "DEMO07_ASSETS": "assets-dir",
        }

    def test_the_snapshot_is_checked_before_any_pass(self):
        events = []
        main = self.module["main"]
        with mock.patch.dict(
            main.__globals__,
            {
                "_config": lambda: object(),
                "ensure_boot_snapshot": lambda *arguments: events.append(
                    ("ensure",) + arguments
                ),
                "_run_once": mock.Mock(
                    side_effect=lambda *_: events.append(("pass",)) or ReachedHermit()
                ),
            },
        ), mock.patch.dict(os.environ, self.environment):
            with self.assertRaises(Exception):
                main()
        self.assertEqual(
            events[0],
            (
                "ensure",
                Path("assets-dir/hermit-boot.qcow2"),
                REPOSITORY_ROOT,
                Path("assets-dir"),
                main.__globals__["_rebuild_boot_snapshot"],
            ),
        )
        self.assertEqual(events[1], ("pass",))

    def test_a_refused_snapshot_starts_no_pass(self):
        run_once = mock.Mock()

        def refuse(*_):
            raise RuntimeError("refusing to restore the custom boot snapshot")

        main = self.module["main"]
        with mock.patch.dict(
            main.__globals__,
            {"_config": lambda: object(), "ensure_boot_snapshot": refuse, "_run_once": run_once},
        ), mock.patch.dict(os.environ, self.environment):
            with self.assertRaises(RuntimeError):
                main()
        run_once.assert_not_called()

    def test_the_rebuild_runs_demo_5_with_demo_7s_assets(self):
        run = mock.Mock()
        rebuild = self.module["_rebuild_boot_snapshot"]
        with mock.patch.object(subprocess, "run", run), mock.patch.dict(
            os.environ, self.environment
        ):
            rebuild()
        run.assert_called_once()
        self.assertEqual(
            run.call_args.args[0],
            ["make", "--no-print-directory", "-C", str(DEMOS_DIR), "demo5"],
        )
        self.assertEqual(run.call_args.kwargs["env"]["QEMU_ASSETS"], "assets-dir")
        self.assertTrue(run.call_args.kwargs["check"])


class FailedRebuildDocumentationTest(unittest.TestCase):
    """The demo 6 and demo 7 READMEs describe the failed-rebuild rule as coded.

    Both demos call accept_snapshot_after_failed_rebuild, which judges demo 5's
    rebuild by its record instead of its exit status, only when a snapshot was
    there and did not match. When the snapshot was missing, demo 5's failure is
    raised again and ends the demo, even if demo 5 saved a snapshot first
    (test_a_missing_snapshot_whose_demo5_fails_still_fails, for each demo).
    """

    MISSING = (
        "If the snapshot was missing and demo 5 exits non-zero, this demo stops "
        "with demo 5's failure, even if demo 5 saved a snapshot first."
    )
    STALE_ONLY = (
        "In that case only, this demo checks demo 5's record instead of its exit "
        "status"
    )

    @staticmethod
    def _words(text):
        return " ".join(text.split())

    def _prerequisites(self, demo):
        readme = (DEMOS_DIR / demo / "README.md").read_text()
        section = readme.split("\n## Prerequisites\n", 1)[1].split("\n## ", 1)[0]
        return self._words(section)

    def test_each_readme_limits_the_record_check_to_a_snapshot_that_was_there(self):
        for demo in ("06-qemu-resume", "07-drgn-kernel"):
            with self.subTest(demo=demo):
                prerequisites = self._prerequisites(demo)
                self.assertIn(self.MISSING, prerequisites)
                self.assertIn(self.STALE_ONLY, prerequisites)

    def test_demo_6s_summary_of_the_record_limits_it_too(self):
        readme = self._words((DEMOS_DIR / "06-qemu-resume" / "README.md").read_text())
        self.assertIn(
            "That allowance is only for a snapshot that was there and did not "
            "match: when the snapshot is missing and demo 5 exits non-zero, the "
            "demo stops with demo 5's failure (see Prerequisites).",
            readme,
        )


if __name__ == "__main__":
    unittest.main()
