#!/usr/bin/env python3
"""Demos 5 and 6 stop what Hermit leaves running and keep the log cap after it exits.

Demos 5 and 6 start Hermit, which runs QEMU, as the leader of a process group
of its own. A thread copies Hermit's standard output and error from a pipe into
the run's INFO log, and the demo waits for Hermit with two bounds: QEMU_TIMEOUT,
in wall-clock seconds, and QEMU_MAX_LOG_BYTES, the size of the INFO log. After
Hermit exits, the demo waits up to OUTPUT_DRAIN_TIMEOUT seconds for the rest of
the output to reach the log, because a process Hermit started inherits the pipe
and can hold it open.

Before this was fixed, reaching QEMU_TIMEOUT raised an error without stopping
Hermit, and the wait after Hermit exited checked neither the cap nor the
processes left in Hermit's group. A run past QEMU_TIMEOUT kept writing its log
for up to OUTPUT_DRAIN_TIMEOUT more seconds (one demo 6 run reached the 1 GiB
output limit of the wrapper it ran under), and a process that Hermit left
running could write the log past the cap.

These tests run each demo's own launch, copy and wait code with Hermit replaced
by a script that starts one process and then exits, leaving that process to
keep writing to the output it inherited, to hold the output open silently, or
to write a little and exit; or that never exits itself. The demo stops at its
first step after the wait, so no Hermit, QEMU or kernel is needed.

Two more checks use the same stand-in. The repeat check compares the log these
demos capture, so they start Hermit without HERMIT_LOG and HERMIT_LOG_FILE,
which would change which records the log holds or send them to a file instead;
the stand-in records the environment it was given. And demo 5 replaces its
private run directory in the saved log with a fixed token, which must leave
every other byte of the log as Hermit wrote it, a carriage return or a byte
that is not UTF-8 included; the stand-in writes such bytes.

Run directly (``python3 demos/tests/test_qemu_output_drain.py``) or via
``make -C demos test``.
"""

from __future__ import annotations

import contextlib
import hashlib
import io
import json
import os
import runpy
import signal
import sys
import tempfile
import time
import unittest
from pathlib import Path
from typing import Optional, Tuple
from unittest import mock

DEMO_DIR = Path(__file__).resolve().parent.parent
LIB_DIR = DEMO_DIR / "lib"
sys.path.insert(0, str(LIB_DIR))

import demo_common as dc  # noqa: E402

# Stands in for Hermit. FAKE_HERMIT_MODE picks the one process it starts. It
# copies FAKE_HERMIT_TRANSCRIPT, when set, to FAKE_HERMIT_SERIAL_LOG, as QEMU
# would have written the serial log, and records its own PID and the started
# process's in FAKE_HERMIT_PIDS. Then it exits 0, except in mode "hung", where
# it writes 1 KiB to its output about every 10 milliseconds and never exits.
#
# Two modes do one more thing first. "record-environment" writes the values of
# HERMIT_LOG, HERMIT_LOG_FILE and RUST_LOG it was started with, as JSON, to
# FAKE_HERMIT_ENVIRONMENT. "write-output" writes the bytes of
# FAKE_HERMIT_OUTPUT to its output, with each @RUN_DIR@ replaced by the host
# directory that the demo binds at FAKE_HERMIT_BIND_TARGET in the guest.
#
# The two processes that write start only once the fake Hermit has exited (their
# parent PID changes when it does) and the demo's wait for it has returned (the
# test then creates the file FAKE_HERMIT_WAIT_ENDED), so everything they write
# arrives after the demo's wait for Hermit has ended, however slowly a busy
# machine runs the fake Hermit or the demo. That wait checks the log once more
# after Hermit exits, so output written before it returned could be found
# past the cap there instead of in the drain these tests are about.
FAKE_HERMIT = r'''
import json, os, shutil, subprocess, sys, time
AFTER_EXIT = "import os, sys, time\nwhile os.getppid() == int(sys.argv[1]) or not os.path.exists(os.environ['FAKE_HERMIT_WAIT_ENDED']):\n    time.sleep(0.01)\n"
DESCENDANTS = {
    # Keeps writing to the output it inherited, 1 KiB about every millisecond.
    "orphan-writer": [sys.executable, "-c", AFTER_EXIT + "try:\n    while True:\n        os.write(1, b'x' * 1024)\n        time.sleep(0.001)\nexcept BrokenPipeError:\n    pass\n", str(os.getpid())],
    # Holds the output open and writes nothing.
    "orphan-holder": ["sleep", "1000"],
    # Writes 1,500 bytes, then exits.
    "late-output": [sys.executable, "-c", AFTER_EXIT + "os.write(1, b'late output\\n' * 125)\n", str(os.getpid())],
    "hung": ["sleep", "1000"],
    # Exits at once.
    "record-environment": ["true"],
    "write-output": ["true"],
}
mode = os.environ["FAKE_HERMIT_MODE"]
if os.environ.get("FAKE_HERMIT_TRANSCRIPT"):
    shutil.copyfile(os.environ["FAKE_HERMIT_TRANSCRIPT"], os.environ["FAKE_HERMIT_SERIAL_LOG"])
if mode == "record-environment":
    with open(os.environ["FAKE_HERMIT_ENVIRONMENT"], "w") as handle:
        json.dump({name: os.environ.get(name) for name in ("HERMIT_LOG", "HERMIT_LOG_FILE", "RUST_LOG")}, handle)
if mode == "write-output":
    target = ":" + os.environ["FAKE_HERMIT_BIND_TARGET"]
    run_dir = next(argument[: -len(target)] for argument in sys.argv if argument.endswith(target))
    with open(os.environ["FAKE_HERMIT_OUTPUT"], "rb") as handle:
        output = handle.read()
    sys.stdout.buffer.write(output.replace(b"@RUN_DIR@", os.fsencode(run_dir)))
    sys.stdout.buffer.flush()
descendant = subprocess.Popen(DESCENDANTS[mode])
pids = os.environ["FAKE_HERMIT_PIDS"]
with open(pids + ".tmp", "w") as handle:
    json.dump({"hermit": os.getpid(), "descendant": descendant.pid}, handle)
os.rename(pids + ".tmp", pids)
while mode == "hung":
    os.write(1, b"y" * 1024)
    time.sleep(0.01)
'''

# What the late-output process writes.
LATE_OUTPUT = b"late output\n" * 125
# Seconds the fake Hermit may take to start on a busy machine. The demo's
# QEMU_TIMEOUT is counted only from then on (see _run).
READY_TIMEOUT = 60
# The cap the drain test sets, about a second of the orphan writer's output,
# and how far past it the log may still grow: drain_output checks the size
# every 0.1 seconds, and the pipe holds up to 64 KiB more when the group is
# stopped. Unstopped, the writer passes the slack in a little over 5 seconds.
SMALL_CAP = 1024 * 1024
CAP_SLACK = 4 * 1024 * 1024
# A cap no other test comes near.
LARGE_CAP = 64 * 1024 * 1024


class ReachedTheNextStep(Exception):
    """The demo got past its wait and on to the step after it."""


def _gone(pid: int) -> bool:
    """Whether ``pid`` has exited (a zombie awaiting its reaper counts)."""
    try:
        state = Path("/proc/{}/stat".format(pid)).read_text().rsplit(")", 1)[1].split()[0]
    except (FileNotFoundError, ProcessLookupError, IndexError):
        return True
    return state in ("Z", "X")


def _reached(*arguments, **keywords):
    raise ReachedTheNextStep()


class _OutputDrainScenarios:
    """The scenarios, run against one demo. A subclass says how to run it."""

    script = ""
    function_name = ""

    @classmethod
    def setUpClass(cls):
        cls.namespace = runpy.run_path(str(DEMO_DIR / cls.script))

    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.directory = Path(directory.name)
        self.assets = self.directory / "assets"
        self.assets.mkdir()
        self.hermit = self.directory / "hermit"
        self.hermit.write_text("#!{}\n{}".format(sys.executable, FAKE_HERMIT))
        self.hermit.chmod(0o755)
        self.pids_file = self.directory / "pids.json"
        self.log_path: Optional[Path] = None
        self.ready_at: Optional[float] = None
        # Whatever a test does, leave nothing running.
        self.addCleanup(self._stop_leftovers)

    def _stop_leftovers(self) -> None:
        if not self.pids_file.is_file():
            return
        pids = json.loads(self.pids_file.read_text())
        for pid in pids.values():
            # Only a process still in the fake Hermit's session: a PID number
            # reused since then belongs to someone else.
            with contextlib.suppress(ProcessLookupError):
                if not _gone(pid) and os.getsid(pid) == pids["hermit"]:
                    os.kill(pid, signal.SIGKILL)

    # Supplied by the subclass.
    def replacements(self) -> dict:
        raise NotImplementedError

    def fake_environment(self, mode: str) -> dict:
        raise NotImplementedError

    def call(self, function):
        raise NotImplementedError

    def _run(
        self,
        mode: str,
        timeout: int = 30,
        max_log_bytes: int = LARGE_CAP,
        drain_timeout: int = 30,
        extra_environment: Optional[dict] = None,
        extra_replacements: Optional[dict] = None,
    ) -> Tuple[object, float]:
        """Run the demo once with the fake Hermit in ``mode``.

        ``extra_environment`` is added to the environment the demo runs in, and
        ``extra_replacements`` to the names this test replaces in the demo.
        Returns what the demo returned or raised, and the seconds from when the
        fake Hermit had started (it had recorded its PIDs) until then.
        """
        real_wait = self.namespace["wait_for_process"]
        wait_ended = self.directory / "wait-ended"

        def wait_when_ready(process, timeout, **keywords):
            # Start the clock once the fake Hermit is up, so that a slow start
            # on a busy machine does not count against QEMU_TIMEOUT. Asked
            # without reaping it: the demo's drain_output and
            # stop_process_group need it unreaped.
            self.log_path = Path(keywords["log_path"])
            deadline = time.monotonic() + READY_TIMEOUT
            while (
                not self.pids_file.is_file()
                and dc._exit_status_without_reaping(process) is None
                and time.monotonic() < deadline
            ):
                time.sleep(0.01)
            self.ready_at = time.monotonic()
            status = real_wait(process, timeout, **keywords)
            # Only now may the processes the fake Hermit left start writing.
            wait_ended.touch()
            return status

        replacements = self.replacements()
        replacements.update(
            {
                "ASSETS": self.assets,
                "QEMU": "qemu-system-x86_64",
                "hermit_binary": lambda: str(self.hermit),
                "TIMEOUT": timeout,
                "MAX_LOG_BYTES": max_log_bytes,
                "OUTPUT_DRAIN_TIMEOUT": drain_timeout,
                "wait_for_process": wait_when_ready,
            }
        )
        replacements.update(extra_replacements or {})
        environment = {
            "FAKE_HERMIT_MODE": mode,
            "FAKE_HERMIT_PIDS": str(self.pids_file),
            "FAKE_HERMIT_WAIT_ENDED": str(wait_ended),
        }
        environment.update(self.fake_environment(mode))
        environment.update(extra_environment or {})
        function = self.namespace[self.function_name]
        printed = io.TextIOWrapper(io.BytesIO(), encoding="utf-8", write_through=True)
        with mock.patch.dict(function.__globals__, replacements), mock.patch.dict(
            os.environ, environment
        ), contextlib.redirect_stdout(printed):
            try:
                outcome = self.call(function)
            except Exception as error:  # noqa: BLE001 - the outcome under test
                outcome = error
        finished = time.monotonic()
        self.assertIsNotNone(self.ready_at, "the demo never waited for Hermit: {!r}".format(outcome))
        self.assertTrue(self.pids_file.is_file(), "the fake Hermit never started")
        return outcome, finished - self.ready_at

    def _assert_stopped(self, role: str) -> None:
        # Checked at once: the demo must have stopped it before returning.
        pid = json.loads(self.pids_file.read_text())[role]
        self.assertTrue(
            _gone(pid),
            "the fake Hermit's {} (PID {}) was still running after the demo "
            "returned".format("own process" if role == "hermit" else "child", pid),
        )

    # Messages, which differ between the demos.
    def cap_error(self, outcome) -> dc.LogCapExceeded:
        raise NotImplementedError

    def cap_message(self, cap: dc.LogCapExceeded) -> str:
        raise NotImplementedError

    def assert_timeout_reported(self, outcome) -> None:
        raise NotImplementedError

    def test_a_process_left_writing_after_hermit_exits_is_stopped_at_the_log_cap(self):
        outcome, _ = self._run("orphan-writer", max_log_bytes=SMALL_CAP)
        log_size = self.log_path.stat().st_size
        self.assertLess(
            log_size,
            SMALL_CAP + CAP_SLACK,
            "the INFO log grew to {} bytes after Hermit exited, past the {}-byte "
            "cap: {!r}".format(log_size, SMALL_CAP, outcome),
        )
        self._assert_stopped("descendant")
        cap = self.cap_error(outcome)
        self.assertEqual(cap.exit_status, 0)
        self.assertEqual(cap.max_log_bytes, SMALL_CAP)
        self.assertGreater(cap.log_size, SMALL_CAP)
        self.assertEqual(cap.log_path, self.log_path)
        self.assertEqual(str(outcome), self.cap_message(cap))

    def test_a_process_left_holding_the_output_is_stopped_at_the_drain_limit(self):
        outcome, _ = self._run("orphan-holder", drain_timeout=1)
        self._assert_stopped("descendant")
        self.assertEqual(
            str(outcome),
            "Hermit's output was still open 1s after it exited, so processes still "
            "held it; the demo then signalled Hermit's process group",
        )
        self.assertIs(type(outcome), RuntimeError)

    def test_a_hermit_that_never_exits_is_stopped_at_qemu_timeout(self):
        outcome, elapsed = self._run("hung", timeout=1, drain_timeout=20)
        # Before the fix the demo went on waiting, up to OUTPUT_DRAIN_TIMEOUT
        # (20 s here), for the output of a Hermit that was still running.
        self.assertLess(
            elapsed,
            10,
            "the demo took {:.1f}s to report a 1s QEMU_TIMEOUT: {!r}".format(elapsed, outcome),
        )
        self.assert_timeout_reported(outcome)
        self._assert_stopped("hermit")
        self._assert_stopped("descendant")

    def test_output_written_after_hermit_exits_reaches_the_log_whole(self):
        # Positive control: a process that writes after Hermit exited and then
        # exits, within the cap and the drain limit, fails nothing, and all of
        # its output is in the log.
        outcome, _ = self._run("late-output")
        self.assertIsInstance(outcome, ReachedTheNextStep)
        self.assertEqual(self.log_path.read_bytes(), LATE_OUTPUT)
        self._assert_stopped("descendant")

    def test_hermit_starts_without_the_variables_that_move_or_change_its_log(self):
        # HERMIT_LOG_FILE would send Hermit's tracing records to that file, so
        # the log the demo compares would hold none, and HERMIT_LOG would change
        # which records it holds. The demo clears both and sets RUST_LOG itself.
        recorded = self.directory / "environment.json"
        outcome, _ = self._run(
            "record-environment",
            extra_environment={
                "HERMIT_LOG": "trace",
                "HERMIT_LOG_FILE": str(self.directory / "hermit.log"),
                "FAKE_HERMIT_ENVIRONMENT": str(recorded),
            },
        )
        self.assertIsInstance(outcome, ReachedTheNextStep)
        self.assertEqual(
            json.loads(recorded.read_text()),
            {
                "HERMIT_LOG": None,
                "HERMIT_LOG_FILE": None,
                "RUST_LOG": self.namespace["LOG_FILTER"],
            },
        )


class Demo5OutputDrainTest(_OutputDrainScenarios, unittest.TestCase):
    """Demo 5's boot."""

    script = "05-qemu-boot/run.py"
    function_name = "boot_once"

    def replacements(self) -> dict:
        # Demo 5 records the initramfs it boots before it starts Hermit: it
        # boots copies of the kernel and initramfs, and records the version
        # that qemu-assets.sh's build record names for the copy's SHA-256.
        initramfs = b"stand-in for the initramfs"
        (self.assets / "initramfs.cpio.gz").write_bytes(initramfs)
        (self.assets / "bzImage").write_bytes(b"stand-in for the kernel")
        (self.assets / ".initramfs-build").write_text(
            "{} {}\n".format(
                dc.current_initramfs_version(self.namespace["ROOT"]),
                hashlib.sha256(initramfs).hexdigest(),
            )
        )
        return {
            "check_qemu_dependencies": lambda root: "QEMU dependency check replaced by the test",
            "check_dependencies": lambda root: "dependency check replaced by the test",
            # Never a disk named by QEMU_SNAPSHOT_DISK outside the test.
            "SNAPSHOT_DISK_OVERRIDE": None,
            # The asset check and `qemu-img create`.
            "run_checked": lambda *arguments, **keywords: None,
            "stage_guest_controller": lambda destination: destination,
            # The first step after the wait.
            "snapshot_exists": _reached,
        }

    def fake_environment(self, mode: str) -> dict:
        return {}

    def call(self, function):
        return function()

    def cap_error(self, outcome) -> dc.LogCapExceeded:
        self.assertIsInstance(outcome, dc.LogCapExceeded)
        return outcome

    def cap_message(self, cap: dc.LogCapExceeded) -> str:
        return (
            "{} grew to {} bytes, past the {}-byte log cap, after the launched "
            "process exited with status 0, while processes still wrote to it; the "
            "demo then signalled the launched process's process group".format(
                cap.log_path, cap.log_size, SMALL_CAP
            )
        )

    def assert_timeout_reported(self, outcome) -> None:
        self.assertIsInstance(outcome, TimeoutError)
        self.assertEqual(str(outcome), "process exceeded timeout of 1s")

    def test_the_saved_log_keeps_every_byte_but_the_run_directory(self):
        # Demo 5 replaces its private run directory in the saved log with a
        # fixed token. It used to do that on the log decoded as text, which
        # turned each carriage return into a newline and each byte that is not
        # UTF-8 into U+FFFD, so the repeat check could not see them.
        output = (
            b"2026-08-17T04:27:14.000001Z  INFO detcore: opened @RUN_DIR@/qmp.sock\r\n"
            b"2026-08-17T04:27:14.000002Z  INFO detcore: read \xff\n"
            b"progress\rdone\n"
        )
        source = self.directory / "output"
        source.write_bytes(output)
        saved = {}

        def snapshot_exists(path, name):
            # The snapshot and the serial log that QEMU would have written.
            path.write_bytes(b"stand-in for the snapshot")
            (path.parent / "serial.log").write_text("2022-01-01T00:00:00\n")
            return True

        def save_metadata(run_dir, disk, info_log, fields):
            saved["log"] = Path(info_log).read_bytes()
            raise ReachedTheNextStep()

        outcome, _ = self._run(
            "write-output",
            extra_environment={
                "FAKE_HERMIT_OUTPUT": str(source),
                "FAKE_HERMIT_BIND_TARGET": str(self.namespace["GUEST_RUN_DIR"]),
            },
            extra_replacements={
                "snapshot_exists": snapshot_exists,
                "canonicalize_qcow2_snapshot_timestamp": lambda path, name: None,
                "save_metadata": save_metadata,
            },
        )
        self.assertIsInstance(outcome, ReachedTheNextStep)
        self.assertEqual(saved["log"], output.replace(b"@RUN_DIR@", b"<run-dir>"))


# The frame /init writes around a command that printed "done" and exited 0.
FINISHED_FRAME = (
    b"__HERMIT_COMMAND_BEGIN__ format=3\r\n| done\r\n__HERMIT_COMMAND_END__\x01status=0\r\n"
)


class Demo6OutputDrainTest(_OutputDrainScenarios, unittest.TestCase):
    """Demo 6's resume, without saving a snapshot."""

    script = "06-qemu-resume/run.py"
    function_name = "resume_once"

    def replacements(self) -> dict:
        # The stand-in for demo 5's snapshot, with the record demo 5 writes for
        # a snapshot of the current initramfs.
        boot_disk = self.assets / "hermit-boot.qcow2"
        boot_disk.write_bytes(b"stand-in for the demo 5 boot snapshot")
        (self.assets / "initramfs.cpio.gz").write_bytes(b"stand-in for the initramfs")
        dc.write_boot_snapshot_record(
            boot_disk,
            dc.hash_file(boot_disk),
            dc.initramfs_producer(self.namespace["ROOT"], self.assets),
        )
        return {
            "SNAPSHOT_DISK": self.assets / "hermit-snapshot.qcow2",
            "BOOT_SNAPSHOT_DISK": boot_disk,
            "check_dependencies": lambda root: "dependency check replaced by the test",
            "ensure_boot_snapshot": lambda: None,
            "acquire_demo_lock": lambda path: None,
            "release_demo_lock": lambda handle: None,
            "stage_guest_controller": lambda destination: destination,
            # The first step after the wait that depends on what Hermit did.
            "parse_command_transcript": _reached,
        }

    def fake_environment(self, mode: str) -> dict:
        if mode == "hung":
            return {}
        transcript = self.directory / "transcript"
        transcript.write_bytes(FINISHED_FRAME)
        return {
            "FAKE_HERMIT_TRANSCRIPT": str(transcript),
            "FAKE_HERMIT_SERIAL_LOG": str(self.assets / "serial.log"),
        }

    def call(self, function):
        return function("echo done", False)

    def cap_error(self, outcome) -> dc.LogCapExceeded:
        self.assertIs(type(outcome), RuntimeError)
        self.assertIsInstance(outcome.__cause__, dc.LogCapExceeded)
        return outcome.__cause__

    def cap_message(self, cap: dc.LogCapExceeded) -> str:
        return (
            "Hermit's INFO log {} grew to {} bytes, past the {}-byte cap "
            "(QEMU_MAX_LOG_BYTES), {:.1f}s into the resume, after Hermit had exited "
            "with status 0: processes it left running still wrote to its output, and "
            "the demo then signalled Hermit's process group; the guest command had "
            "finished with exit status 0".format(
                cap.log_path, cap.log_size, SMALL_CAP, cap.elapsed
            )
        )

    def assert_timeout_reported(self, outcome) -> None:
        self.assertIs(type(outcome), RuntimeError)
        self.assertIsInstance(outcome.__cause__, TimeoutError)
        self.assertEqual(
            str(outcome),
            "Hermit/QEMU did not exit within QEMU_TIMEOUT (1s); QEMU had not written "
            "the serial log {}".format(self.assets / "serial.log"),
        )


if __name__ == "__main__":
    unittest.main()
