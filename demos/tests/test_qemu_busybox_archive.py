#!/usr/bin/env python3
"""Demo 9's initramfs stores the same modes, and bytes, under any umask.

demos/09-qemu-busybox/build-initramfs.sh archives a BusyBox, its applet links,
and the guest's /init. cpio stores each entry's mode as it finds it on disk,
and the script creates directories and two files under the caller's umask.
These tests run copies of the real script with a stand-in BusyBox and file(1)
under several umasks and read the archive back. They need no QEMU and no
BusyBox; they run the host's cpio and gzip, which the script requires.
"""

import gzip
import hashlib
import os
import shutil
import stat
import subprocess
import tempfile
import unittest
from pathlib import Path

DEMO_DIR = Path(__file__).resolve().parent.parent
DEMO9 = DEMO_DIR / "09-qemu-busybox"
BUILDER = DEMO9 / "build-initramfs.sh"
INIT = DEMO9 / "init"

# Where the stand-in BusyBox says it installs each applet. They include every
# applet the guest's /init runs, which build-initramfs.sh requires, and
# usr/local/bin/extra and usr/sbin/rdate, for which the script creates
# directories itself.
APPLET_PATHS = (
    "bin/ls",
    "bin/mknod",
    "bin/mount",
    "bin/sh",
    "bin/uname",
    "linuxrc",
    "sbin/poweroff",
    "usr/bin/bc",
    "usr/bin/head",
    "usr/bin/printf",
    "usr/bin/sha256sum",
    "usr/bin/sort",
    "usr/local/bin/extra",
    "usr/sbin/rdate",
)

# A stand-in BusyBox that answers what build-initramfs.sh asks it. Its shell
# is bash, which supports `set -o pipefail`.
FAKE_BUSYBOX = """#!/bin/sh
case "${{1:-}}" in
  --list) printf '%s\\n' {names} ;;
  --list-full) printf '%s\\n' {paths} ;;
  sh) shift; exec bash "$@" ;;
  *) exit 1 ;;
esac
"""

# The script refuses a BusyBox that file(1) does not call statically linked.
FAKE_FILE = """#!/bin/sh
printf '%s: ELF 64-bit LSB executable, x86-64, statically linked\\n' "$1"
"""

# The modes every build stores. They are the modes a build under umask 022
# stored before the script set them explicitly. The archive's root entry is the
# build directory, which keeps 0700, the mode mktemp gives it.
ROOT_MODE = stat.S_IFDIR | 0o700
DIRECTORY_MODE = stat.S_IFDIR | 0o755
EXECUTABLE_MODE = stat.S_IFREG | 0o755
DATA_FILE_MODE = stat.S_IFREG | 0o644
SYMLINK_MODE = stat.S_IFLNK | 0o777

DIRECTORIES = (
    "bin",
    "dev",
    "etc",
    "home",
    "proc",
    "root",
    "sbin",
    "sys",
    "tmp",
    "usr",
    "usr/bin",
    "usr/local",
    "usr/local/bin",
    "usr/sbin",
)


def _write_executable(path: Path, text: str, mode: int = 0o755) -> None:
    path.write_text(text)
    path.chmod(mode)


def newc_modes(archive: bytes) -> dict:
    """Map each entry name of a newc cpio archive to its stored mode."""
    modes = {}
    offset = 0
    while True:
        header = archive[offset : offset + 110]
        if header[:6] != b"070701":
            raise ValueError("no newc header at offset {}".format(offset))
        fields = [int(header[6 + 8 * i : 14 + 8 * i], 16) for i in range(13)]
        mode, file_size, name_size = fields[1], fields[6], fields[11]
        name_start = offset + 110
        name = archive[name_start : name_start + name_size - 1].decode()
        offset = (name_start + name_size + 3) & ~3
        if name == "TRAILER!!!":
            return modes
        if name.startswith("./"):
            name = name[2:]
        modes[name] = mode
        offset = (offset + file_size + 3) & ~3


class ArchiveModesTest(unittest.TestCase):
    def setUp(self):
        # Every build of one test runs in the same temporary directory, so all
        # are on the same filesystem: cpio also stores each directory's link
        # count, which depends on the filesystem.
        holder = tempfile.TemporaryDirectory()
        self.addCleanup(holder.cleanup)
        self.scratch = Path(holder.name)
        demo = self.scratch / "repo" / "demos" / "09-qemu-busybox"
        demo.mkdir(parents=True)
        shutil.copy2(BUILDER, demo / "build-initramfs.sh")
        shutil.copy2(INIT, demo / "init")
        self.builder = demo / "build-initramfs.sh"
        fake_bin = self.scratch / "bin"
        fake_bin.mkdir()
        _write_executable(fake_bin / "file", FAKE_FILE)
        self.busybox = self.scratch / "busybox"
        names = sorted(Path(path).name for path in APPLET_PATHS)
        _write_executable(
            self.busybox,
            FAKE_BUSYBOX.format(names=" ".join(names), paths=" ".join(APPLET_PATHS)),
        )
        self.environment = dict(os.environ)
        self.environment.update(
            PATH="{}:{}".format(fake_bin, self.environment.get("PATH", "/usr/bin:/bin")),
            BUSYBOX=str(self.busybox),
            LC_ALL="C",
        )

    def _build(self, umask: int) -> bytes:
        """Run the copied script under `umask`; return the uncompressed archive."""
        archive = self.scratch / "out-{:03o}".format(umask) / "initramfs.cpio.gz"
        result = subprocess.run(
            ["bash", str(self.builder), str(archive)],
            env=self.environment,
            cwd=self.scratch,
            umask=umask,
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            timeout=120,
        )
        self.assertEqual(
            0,
            result.returncode,
            "build-initramfs.sh failed under umask {:03o}:\n{}{}".format(
                umask, result.stdout, result.stderr
            ),
        )
        return gzip.decompress(archive.read_bytes())

    def _assert_fixed_modes(self, modes: dict, umask: int) -> None:
        def check(expected: int, name: str) -> None:
            self.assertEqual(
                oct(expected),
                oct(modes[name]),
                "/{} built under umask {:03o}".format(name, umask),
            )

        check(ROOT_MODE, ".")
        for name in DIRECTORIES:
            check(DIRECTORY_MODE, name)
        for name in ("bin/busybox", "init"):
            check(EXECUTABLE_MODE, name)
        for name in ("etc/group", "etc/passwd"):
            check(DATA_FILE_MODE, name)
        for name in APPLET_PATHS:
            check(SYMLINK_MODE, name)
        expected_names = (
            {".", "bin/busybox", "init", "etc/group", "etc/passwd"}
            | set(DIRECTORIES)
            | set(APPLET_PATHS)
        )
        self.assertEqual(expected_names, set(modes))

    def test_the_usual_umask_stores_the_modes_it_stored_before(self):
        """Positive control: under umask 022 the stored modes are unchanged."""
        self._assert_fixed_modes(newc_modes(self._build(umask=0o022)), 0o022)

    def test_a_restrictive_umask_does_not_change_the_stored_modes(self):
        self._assert_fixed_modes(newc_modes(self._build(umask=0o077)), 0o077)

    def test_a_group_writable_umask_does_not_change_the_stored_modes(self):
        # 002 is the default umask for users who have their own group on many
        # distributions.
        self._assert_fixed_modes(newc_modes(self._build(umask=0o002)), 0o002)

    def test_the_archive_is_byte_identical_under_umask_002_022_and_077(self):
        builds = {umask: self._build(umask) for umask in (0o002, 0o022, 0o077)}
        digests = {
            "{:03o}".format(umask): hashlib.sha256(archive).hexdigest()
            for umask, archive in builds.items()
        }
        self.assertEqual(1, len(set(digests.values())), digests)


if __name__ == "__main__":
    unittest.main()
