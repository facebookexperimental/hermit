#!/usr/bin/env python3
"""Build the QEMU demo initramfs with stand-in tools and check what it stores.

demos/lib/qemu-assets.sh builds the BusyBox initramfs that demos 5, 6, and 7
boot. These tests run the real script with stand-ins for QEMU, qemu-img,
file(1), the kernel image, and BusyBox, so they need no QEMU, no BusyBox, and
no network. They do run the host's cpio and gzip, which the script requires.
"""

import gzip
import hashlib
import os
import re
import stat
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

DEMO_DIR = Path(__file__).resolve().parent.parent
QEMU_ASSETS_SCRIPT = DEMO_DIR / "lib" / "qemu-assets.sh"

# The applets the guest's /init runs, where BusyBox installs them, plus a few
# it does not run. The applet usr/local/bin/extra makes the script create a
# directory of its own.
APPLETS = (
    "bin/cttyhack",
    "bin/date",
    "bin/dd",
    "bin/mount",
    "bin/sh",
    "bin/sleep",
    "bin/uname",
    "sbin/poweroff",
    "usr/bin/chpst",
    "usr/bin/env",
    "usr/bin/head",
    "usr/bin/setsid",
    "usr/bin/tr",
    "usr/local/bin/extra",
)

# The stand-in BusyBox answers only the question qemu-assets.sh asks it, with
# the applets in place of {applets}.
FAKE_BUSYBOX = """#!/bin/sh
if [ "$1" = --list-full ]; then
  printf '%s\\n' {applets}
  exit 0
fi
exit 1
"""

# The script refuses a BusyBox that file(1) does not call statically linked.
FAKE_FILE = """#!/bin/sh
printf '%s: ELF 64-bit LSB executable, x86-64, statically linked\\n' "$1"
"""

DIRECTORY_MODE = stat.S_IFDIR | 0o755
# /tmp: everyone may create files there, and only a file's owner may remove or
# rename it.
STICKY_DIRECTORY_MODE = stat.S_IFDIR | 0o1777
EXECUTABLE_MODE = stat.S_IFREG | 0o755
DATA_FILE_MODE = stat.S_IFREG | 0o644
SYMLINK_MODE = stat.S_IFLNK | 0o777


def _write_executable(path: Path, text: str, mode: int = 0o755) -> None:
    path.write_text(text)
    path.chmod(mode)


def parse_newc(archive: bytes) -> dict:
    """Map each entry name of a newc cpio archive to its stored mode."""
    return {name: mode for name, (mode, _) in parse_newc_entries(archive).items()}


def parse_newc_entries(archive: bytes) -> dict:
    """Map each entry name of a newc cpio archive to its mode and contents."""
    entries = {}
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
            return entries
        if name.startswith("./"):
            name = name[2:]
        entries[name] = (mode, archive[offset : offset + file_size])
        offset = (offset + file_size + 3) & ~3


class InitramfsBuildTest(unittest.TestCase):
    def _run_script(
        self, umask: int, busybox_mode: int = 0o755, applets=APPLETS
    ) -> "tuple[subprocess.CompletedProcess, Path]":
        """Run qemu-assets.sh under `umask`; return its result and assets dir."""
        holder = tempfile.TemporaryDirectory()
        self.addCleanup(holder.cleanup)
        scratch = Path(holder.name)
        fake_bin = scratch / "bin"
        fake_bin.mkdir()
        _write_executable(fake_bin / "file", FAKE_FILE)
        _write_executable(fake_bin / "qemu-img", "#!/bin/sh\nexit 0\n")
        _write_executable(fake_bin / "qemu-system-x86_64", "#!/bin/sh\nexit 0\n")
        busybox = scratch / "busybox"
        _write_executable(
            busybox, FAKE_BUSYBOX.format(applets=" ".join(applets)), busybox_mode
        )
        kernel = scratch / "bzImage"
        kernel.write_bytes(b"stand-in kernel image\n")
        assets = scratch / "assets"
        repo = scratch / "repo"
        repo.mkdir()

        env = dict(os.environ)
        env.update(
            PATH="{}:{}".format(fake_bin, env.get("PATH", "/usr/bin:/bin")),
            QEMU_ASSETS=str(assets),
            HERMIT_REPO=str(repo),
            BUSYBOX=str(busybox),
            KERNEL_IMAGE=str(kernel),
            QEMU_KERNEL_SHA256=hashlib.sha256(kernel.read_bytes()).hexdigest(),
            QEMU_BIN=str(fake_bin / "qemu-system-x86_64"),
            QEMU_DEMO_PYTHON=sys.executable,
        )
        self._scratch = scratch
        self._env = env
        return self._run_again(umask), assets

    def _run_again(self, umask: int = 0o022) -> subprocess.CompletedProcess:
        """Run qemu-assets.sh again with the scratch of the last _run_script."""
        return subprocess.run(
            ["bash", str(QEMU_ASSETS_SCRIPT)],
            env=self._env,
            cwd=self._scratch,
            umask=umask,
            capture_output=True,
            text=True,
            timeout=120,
        )

    def _build(self, umask: int, busybox_mode: int = 0o755) -> bytes:
        """Run qemu-assets.sh under `umask` and return the initramfs bytes."""
        result, assets = self._run_script(umask, busybox_mode)
        self.assertEqual(
            0,
            result.returncode,
            "qemu-assets.sh failed under umask {:03o}:\n{}{}".format(
                umask, result.stdout, result.stderr
            ),
        )
        return gzip.decompress((assets / "initramfs.cpio.gz").read_bytes())

    def _assert_fixed_modes(self, modes: dict) -> None:
        for name in (
            ".",
            "bin",
            "dev",
            "etc",
            "proc",
            "sbin",
            "sys",
            "usr",
            "usr/bin",
            "usr/local",
            "usr/local/bin",
            "usr/sbin",
        ):
            self.assertEqual(
                oct(DIRECTORY_MODE), oct(modes[name]), "directory /" + name
            )
        self.assertEqual(oct(STICKY_DIRECTORY_MODE), oct(modes["tmp"]), "directory /tmp")
        for name in ("bin/busybox", "init"):
            self.assertEqual(oct(EXECUTABLE_MODE), oct(modes[name]), "/" + name)
        for name in ("etc/group", "etc/passwd"):
            self.assertEqual(oct(DATA_FILE_MODE), oct(modes[name]), "/" + name)
        for name in APPLETS:
            self.assertEqual(oct(SYMLINK_MODE), oct(modes[name]), "/" + name)

    def test_the_build_record_names_the_archive_built_and_its_version(self):
        # Demo 5 records an initramfs's version only when this record names
        # the SHA-256 of the copy it boots (booted_initramfs_producer).
        result, assets = self._run_script(0o022)
        self.assertEqual(0, result.returncode, result.stdout + result.stderr)
        version = re.search(
            r"^INITRAMFS_VERSION=([0-9]+)$", QEMU_ASSETS_SCRIPT.read_text(), re.MULTILINE
        ).group(1)
        archive = (assets / "initramfs.cpio.gz").read_bytes()
        self.assertEqual(
            (assets / ".initramfs-build").read_text(),
            "{} {}\n".format(version, hashlib.sha256(archive).hexdigest()),
        )

    def test_a_cached_archive_its_build_record_does_not_name_is_rebuilt(self):
        # Another checkout replaced the archive and left .initramfs-version as
        # it was: the build record names other bytes, so the archive is rebuilt.
        result, assets = self._run_script(0o022)
        self.assertEqual(0, result.returncode, result.stdout + result.stderr)
        archive = assets / "initramfs.cpio.gz"
        built = archive.read_bytes()
        archive.write_bytes(b"an initramfs another checkout built")
        result = self._run_again()
        self.assertEqual(0, result.returncode, result.stdout + result.stderr)
        self.assertEqual(archive.read_bytes(), built)
        self.assertEqual(
            (assets / ".initramfs-build").read_text().split()[1],
            hashlib.sha256(built).hexdigest(),
        )

    def test_a_restrictive_umask_does_not_change_the_stored_modes(self):
        self._assert_fixed_modes(parse_newc(self._build(umask=0o077)))

    def test_the_archive_is_byte_identical_under_umask_022_and_077(self):
        # Both builds run in this test's temporary directories, so they are on
        # the same filesystem: cpio also stores each directory's link count,
        # which depends on the filesystem.
        usual = self._build(umask=0o022)
        restrictive = self._build(umask=0o077)
        self.assertEqual(parse_newc(usual), parse_newc(restrictive))
        self.assertEqual(
            hashlib.sha256(usual).hexdigest(), hashlib.sha256(restrictive).hexdigest()
        )

    def test_the_installed_busybox_mode_does_not_reach_the_archive(self):
        # A BusyBox from a read-only store, such as Nix's, is installed 0555.
        modes = parse_newc(self._build(umask=0o022, busybox_mode=0o555))
        self._assert_fixed_modes(modes)

    def test_init_has_the_frame_separator_byte_and_runs_the_command_unprivileged(self):
        init = parse_newc_entries(self._build(umask=0o022))["init"][1]
        self.assertNotIn(b"@HERMIT_FRAME_SEP@", init)
        self.assertIn(b"\nSEP='\x01'\n", init)
        self.assertEqual(init.count(b"\x01"), 1)
        self.assertIn(
            b'\nchpst -u 1000:1000 sh -c "$CMD" </dev/null '
            b">/tmp/.hermit-command-output 2>&1\n",
            init,
        )
        self.assertIn(b"""\nprintf '__HERMIT_COMMAND_END__%sstatus=%s\\n' "$SEP" "$STATUS"\n""", init)

    def test_a_busybox_without_an_applet_init_runs_is_refused(self):
        without = tuple(
            applet for applet in APPLETS if applet not in ("usr/bin/chpst", "bin/dd")
        )
        result, assets = self._run_script(umask=0o022, applets=without)
        self.assertNotEqual(0, result.returncode, result.stdout + result.stderr)
        self.assertIn(
            "BusyBox lacks applets the guest's /init runs (chpst dd)", result.stderr
        )
        self.assertFalse((assets / "initramfs.cpio.gz").exists())


if __name__ == "__main__":
    unittest.main()
