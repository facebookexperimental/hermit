#!/usr/bin/env python3
"""The host must refuse a guest command longer than the guest reads.

Demos 5, 6, and 7 give the Linux guest its command on a 4096-byte raw disk.
The guest's init (built by demos/lib/qemu-assets.sh) reads only the first 512
bytes of that disk and runs the first line of what it read, so a command whose
bytes and ending newline do not fit in those 512 bytes would run cut off.
Demo 6's run.py and demo 7's drgn_hermit.py write that disk and must refuse such
a command; demo 6 must refuse it before it resumes anything.

The reader tests run the guest's own reader line, taken from qemu-assets.sh,
with the host's sh, dd, tr, and head in place of the guest's BusyBox ones.
"""

import os
import re
import runpy
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

DEMO_DIR = Path(__file__).resolve().parent.parent
LIB_DIR = DEMO_DIR / "lib"
sys.path.insert(0, str(LIB_DIR))

import drgn_hermit as dh  # noqa: E402


def _load(relative: str) -> dict:
    return runpy.run_path(str(DEMO_DIR / relative))


def _guest_reader_line() -> str:
    """The one line of the guest's init that reads the command disk."""
    text = (LIB_DIR / "qemu-assets.sh").read_text()
    lines = [
        line.strip()
        for line in text.splitlines()
        if line.strip().startswith("CMD=$(dd ")
    ]
    if len(lines) != 1:
        raise AssertionError("expected one CMD=$(dd ...) line, found {}".format(lines))
    return lines[0]


def _guest_read(image: Path) -> bytes:
    """What the guest's reader line sets CMD to for this command disk."""
    script = 'CMDDEV="$1"\n{}\nprintf %s "$CMD"\n'.format(_guest_reader_line())
    environment = dict(os.environ, LC_ALL="C")
    return subprocess.run(
        ["sh", "-c", script, "sh", str(image)],
        check=True,
        stdout=subprocess.PIPE,
        env=environment,
    ).stdout


def _writers(demo6: dict):
    """Each host-side writer of the command disk, with its module's limits."""
    return (
        ("demo 6 run.py", demo6["write_command_image"], demo6),
        ("demo 7 drgn_hermit.py", dh._write_command_image, vars(dh)),
    )


class GuestCommandLengthTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.demo5 = _load("05-qemu-boot/run.py")
        cls.demo6 = _load("06-qemu-resume/run.py")

    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.directory = Path(directory.name)

    def test_the_limit_is_what_the_guest_reader_reads(self):
        reader = _guest_reader_line()
        match = re.search(r"\bbs=(\d+) count=(\d+)\b", reader)
        self.assertIsNotNone(match, reader)
        read_bytes = int(match.group(1)) * int(match.group(2))
        self.assertIn("| head -n 1)", reader)
        for name, _, limits in _writers(self.demo6):
            with self.subTest(writer=name):
                self.assertEqual(limits["GUEST_COMMAND_READ_BYTES"], read_bytes)
                self.assertEqual(limits["MAX_GUEST_COMMAND_BYTES"], read_bytes - 1)

    def test_boot_and_resume_disks_have_one_geometry(self):
        # The same drive is attached at boot (demo 5) and at resume (demos 6
        # and 7); a size change between them would not match the snapshot.
        sizes = {
            self.demo5["COMMAND_IMAGE_BYTES"],
            self.demo6["COMMAND_IMAGE_BYTES"],
            dh.COMMAND_IMAGE_BYTES,
        }
        self.assertEqual(sizes, {4096})
        self.assertLessEqual(self.demo6["GUEST_COMMAND_READ_BYTES"], 4096)

    def test_the_largest_accepted_command_reaches_the_guest_whole(self):
        for name, write, limits in _writers(self.demo6):
            with self.subTest(writer=name):
                command = "echo " + "x" * (limits["MAX_GUEST_COMMAND_BYTES"] - 5)
                self.assertEqual(len(command.encode()), 511)
                image = self.directory / "largest.img"
                write(image, command)
                payload = image.read_bytes()
                self.assertEqual(len(payload), 4096)
                self.assertEqual(payload[:512], command.encode() + b"\n")
                self.assertEqual(payload[512:], b"\0" * (4096 - 512))
                self.assertEqual(_guest_read(image), command.encode())

    def test_one_byte_more_is_refused_and_writes_nothing(self):
        for name, write, _ in _writers(self.demo6):
            with self.subTest(writer=name):
                command = "echo " + "x" * 507
                self.assertEqual(len(command.encode()), 512)
                image = self.directory / "one-more.img"
                with self.assertRaisesRegex(
                    ValueError,
                    r"guest command is 512 bytes; the guest reads only the "
                    r"first 512 bytes of its command disk, so a command can be "
                    r"at most 511 bytes",
                ):
                    write(image, command)
                self.assertFalse(image.exists())

    def test_the_old_limit_let_through_a_command_the_guest_cuts_off(self):
        command = "echo " + "y" * 595
        # The writers used to accept any command up to 4095 bytes. The guest
        # would have run this one cut off after its first 512 bytes.
        old_image = self.directory / "old.img"
        payload = command.encode() + b"\n"
        old_image.write_bytes(payload + b"\0" * (4096 - len(payload)))
        self.assertEqual(_guest_read(old_image), command.encode()[:512])
        for name, write, _ in _writers(self.demo6):
            with self.subTest(writer=name):
                with self.assertRaisesRegex(ValueError, "guest command is 600 bytes"):
                    write(self.directory / "refused.img", command)

    def test_the_limit_counts_utf8_bytes_not_characters(self):
        fits = "echo " + "é" * 253  # 5 + 2 * 253 = 511 bytes
        over = fits + "a"  # 512 bytes in 259 characters
        self.assertEqual(len(fits.encode()), 511)
        self.assertEqual(len(over.encode()), 512)
        for name, write, _ in _writers(self.demo6):
            with self.subTest(writer=name):
                image = self.directory / "utf8.img"
                write(image, fits)
                self.assertEqual(_guest_read(image), fits.encode())
                with self.assertRaisesRegex(ValueError, "guest command is 512 bytes"):
                    write(self.directory / "utf8-over.img", over)

    def test_demo6_refuses_before_it_resumes_anything(self):
        main = self.demo6["main"]
        resume_once = mock.Mock(return_value="SUCCESS")
        too_long = "echo " + "z" * 507
        with mock.patch.dict(main.__globals__, {"resume_once": resume_once}):
            with mock.patch.object(sys, "argv", ["run.py", too_long]):
                with self.assertRaisesRegex(ValueError, "at most 511 bytes"):
                    main()
            resume_once.assert_not_called()

            largest = "echo " + "z" * 506
            with mock.patch.object(sys, "argv", ["run.py", largest]):
                self.assertEqual(main(), 0)
        resume_once.assert_called_once_with(largest, True)

    def test_demo6_still_refuses_a_second_line(self):
        for command in ("echo a\necho b", "echo a\recho b"):
            with self.subTest(command=command):
                with self.assertRaisesRegex(ValueError, "single line"):
                    self.demo6["check_guest_command"](command)


if __name__ == "__main__":
    unittest.main()
