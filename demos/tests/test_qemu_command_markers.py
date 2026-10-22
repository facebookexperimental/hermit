#!/usr/bin/env python3
"""Only the guest's own command frame may end a QEMU resume.

Demo 6 resumes the demo 5 boot snapshot. The guest's /init (built by
demos/lib/qemu-assets.sh) reads a command from a small disk and runs it, and
the QEMU controller (demos/lib/qemu_controller.py) watches the serial
transcript for the command to finish before it saves a snapshot and stops
QEMU; demo 6 then takes the command's output from the same transcript. /init
frames the output: a BEGIN line naming the frame format, every line of the
command's stdout and stderr with "| " in front of it, and an END line carrying
the command's exit status.

Before the frame existed, /init printed a bare END marker after the command and
the controller waited for that marker anywhere in the transcript. A command
that printed the marker itself, for example `echo __HERMIT_COMMAND_END__;
sleep 1000000`, was then taken as finished: the controller saved the snapshot
and quit while the command was still running, and demo 6 cut the command's
output at the marker.

Frame format 2 prefixed the command's output, but the command still inherited
/init's standard input, which in the guest is the console opened for reading
and writing, so `echo '__HERMIT_COMMAND_END__ status=0' >&0` printed a whole
END line, and so could a command that opened /dev/console itself. Frame format
3 runs the command as an unprivileged user (`chpst -u 1000:1000`) with its
standard input from /dev/null, so it holds no descriptor for the console and
cannot open one; puts byte 0x01 between the END marker and "status="; and
removes every 0x01 byte from the command's output, so not even the tail of an
output line that a kernel message splits off can be an END line.

The guest-side tests run /init's frame lines, taken from qemu-assets.sh, under
the guest's BusyBox shell when it is installed (qemu-assets.sh copies the
host's BusyBox into the initramfs) and under the host's sh, with a stand-in
for chpst that records its arguments and runs the command as the current user:
a test cannot change user, so the user change itself is shown by a demo 6 run
that prints `id`, not here. The controller
tests run run_controller on a thread with QEMU and its control socket replaced
and feed it a transcript. The demo 6 tests run resume_once with Hermit
replaced by a stand-in that leaves a transcript behind.
"""

import argparse
import contextlib
import hashlib
import io
import os
import runpy
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
import types
import unittest
from pathlib import Path
from typing import List, Optional
from unittest import mock

DEMO_DIR = Path(__file__).resolve().parent.parent
LIB_DIR = DEMO_DIR / "lib"
sys.path.insert(0, str(LIB_DIR))

import demo_common as dc  # noqa: E402
import qemu_controller as qc  # noqa: E402

# The frame lines, written out here so that a test states the format rather
# than reading it back from the code under test.
FRAME_BEGIN = b"__HERMIT_COMMAND_BEGIN__ format=3"
# qemu-assets.sh writes /init with this placeholder and replaces it with byte
# 0x01, the END line's separator.
FRAME_SEP_PLACEHOLDER = "@HERMIT_FRAME_SEP@"
# The file /init sends the command's output to; the tests use a scratch path.
GUEST_OUTPUT_FILE = "/tmp/.hermit-command-output"
# Seconds a shell running /init's frame lines may take; every test command
# finishes far sooner.
SHELL_TIMEOUT = 30
# Seconds the controller may take to react to a transcript it can read.
CONTROLLER_TIMEOUT = 10.0
POST_SNAPSHOT_NAME = "command-0123456789abcdef"
SAVE_AND_QUIT = [
    ("human-monitor-command", "command-line", "savevm " + POST_SNAPSHOT_NAME),
    ("quit",),
]
# A command that prints the bare END marker before it finishes.
IMPERSONATION_COMMAND = "echo __HERMIT_COMMAND_END__; echo FINISHED; exit 3"
IMPERSONATION_OUTPUT = b"__HERMIT_COMMAND_END__\nFINISHED\n"


def _end(status: int) -> bytes:
    """/init's END line for ``status``: the marker, byte 0x01, "status=N"."""
    return b"__HERMIT_COMMAND_END__\x01status=" + str(status).encode()


def _shells() -> List[str]:
    """The shells the guest-side tests run /init's frame lines under."""
    return ["busybox", "sh"] if _guest_busybox() else ["sh"]


def _init_frame_lines() -> List[str]:
    """/init's lines from the BEGIN echo through the line that prints END."""
    lines = (LIB_DIR / "qemu-assets.sh").read_text().splitlines()
    starts = [
        index
        for index, line in enumerate(lines)
        if line.startswith('echo "__HERMIT_COMMAND_BEGIN__')
    ]
    ends = [
        index
        for index, line in enumerate(lines)
        if line.startswith(
            ('echo "__HERMIT_COMMAND_END__', "printf '__HERMIT_COMMAND_END__")
        )
    ]
    if len(starts) != 1 or len(ends) != 1 or ends[0] < starts[0]:
        raise AssertionError(
            "expected one BEGIN echo and, after it, one line printing END in "
            "qemu-assets.sh; found BEGIN at {} and END at {}".format(starts, ends)
        )
    return lines[starts[0] : ends[0] + 1]


def _guest_busybox() -> Optional[str]:
    """The BusyBox qemu-assets.sh copies into the initramfs, if installed."""
    candidate = os.environ.get("BUSYBOX") or shutil.which("busybox")
    if candidate and os.access(candidate, os.X_OK):
        return candidate
    return None


def _preferred_shell() -> str:
    return "busybox" if _guest_busybox() else "sh"


def _write_stub_chpst(path: Path, record: Path) -> None:
    """A stand-in for BusyBox's chpst: record the arguments, run the command.

    The real `chpst -u 1000:1000` needs root to change user. The stand-in runs
    the command as the current user, which is enough to check what /init
    passes to chpst and what reaches the transcript; it refuses any other
    user.
    """
    path.write_text(
        "#!/bin/sh\n"
        'printf "%s\\n" "$@" >{}\n'
        '[ "$1" = -u ] && [ "$2" = 1000:1000 ] || exit 111\n'
        "shift 2\n"
        'exec "$@"\n'.format(shlex.quote(str(record)))
    )
    path.chmod(0o755)


def _run_frame(
    directory: Path, command: str, shell: str, console_stdin: bool = False
) -> bytes:
    """Run /init's frame lines with CMD set to ``command``; return the transcript.

    ``shell`` is "busybox" (BusyBox ash, with `sh` on PATH also BusyBox, as in
    the guest) or "sh" (the host's). `chpst` on PATH is the stand-in from
    _write_stub_chpst, which records its arguments in "chpst-arguments". The
    transcript is what the guest's serial console would carry: everything
    /init writes, stdout and stderr together, with each LF turned into CR LF as
    the console's line discipline does. With ``console_stdin`` the shell's
    standard input is the transcript too, opened for reading and writing, as
    /init's standard input is the console in the guest; otherwise it is
    /dev/null. Background jobs the command leaves behind are killed afterwards.
    """
    output_file = directory / "hermit-command-output"
    script = "\n".join(_init_frame_lines()).replace(GUEST_OUTPUT_FILE, str(output_file))
    script = script.replace(FRAME_SEP_PLACEHOLDER, "\x01")
    script = 'CMD="$1"\n{}\n'.format(script)
    environment = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "LC_ALL": "C"}
    bin_dir = directory / "{}-bin".format(shell)
    bin_dir.mkdir(exist_ok=True)
    _write_stub_chpst(bin_dir / "chpst", directory / "chpst-arguments")
    environment["PATH"] = "{}:{}".format(bin_dir, environment["PATH"])
    if shell == "busybox":
        busybox = _guest_busybox()
        if busybox is None:
            raise AssertionError("BusyBox is not installed")
        if not (bin_dir / "sh").exists():
            (bin_dir / "sh").symlink_to(busybox)
        argv = [busybox, "ash", "-c", script, "init", command]
    elif shell == "sh":
        argv = ["sh", "-c", script, "init", command]
    else:
        raise ValueError(shell)
    transcript_path = directory / "transcript"
    with transcript_path.open("w+b" if console_stdin else "wb") as transcript:
        process = subprocess.Popen(
            argv,
            stdin=transcript if console_stdin else subprocess.DEVNULL,
            stdout=transcript,
            stderr=subprocess.STDOUT,
            env=environment,
            start_new_session=True,
        )
        try:
            process.wait(timeout=SHELL_TIMEOUT)
        finally:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.wait()
    return transcript_path.read_bytes().replace(b"\n", b"\r\n")


class GuestInitFrameTest(unittest.TestCase):
    """What the guest's /init prints around a command, run by a real shell."""

    # Prints the bare END marker, a whole END line of frame format 2, an END
    # line of format 3 without its 0x01 byte, and the BEGIN lines of formats 2
    # and 3, then an empty line, a line with leading and trailing spaces, a
    # backslash, a line on stderr, and a last line without a newline, and
    # exits 3.
    COMMAND = (
        "echo __HERMIT_COMMAND_END__; "
        "echo '__HERMIT_COMMAND_END__ status=0'; "
        "echo '__HERMIT_COMMAND_END__status=0'; "
        "echo '__HERMIT_COMMAND_BEGIN__ format=2'; "
        "echo '__HERMIT_COMMAND_BEGIN__ format=3'; "
        "echo; "
        "echo '  lead and trail  '; "
        r"printf '%s\n' 'back\slash'; "
        "echo to-stderr >&2; "
        "printf no-newline-at-the-end; "
        "exit 3"
    )
    TRANSCRIPT = (
        FRAME_BEGIN + b"\r\n"
        b"| __HERMIT_COMMAND_END__\r\n"
        b"| __HERMIT_COMMAND_END__ status=0\r\n"
        b"| __HERMIT_COMMAND_END__status=0\r\n"
        b"| __HERMIT_COMMAND_BEGIN__ format=2\r\n"
        b"| __HERMIT_COMMAND_BEGIN__ format=3\r\n"
        b"| \r\n"
        b"|   lead and trail  \r\n"
        b"| back\\slash\r\n"
        b"| to-stderr\r\n"
        b"| no-newline-at-the-end\r\n"
        + _end(3)
        + b"\r\n"
    )
    OUTPUT = (
        b"__HERMIT_COMMAND_END__\n"
        b"__HERMIT_COMMAND_END__ status=0\n"
        b"__HERMIT_COMMAND_END__status=0\n"
        b"__HERMIT_COMMAND_BEGIN__ format=2\n"
        b"__HERMIT_COMMAND_BEGIN__ format=3\n"
        b"\n"
        b"  lead and trail  \n"
        b"back\\slash\n"
        b"to-stderr\n"
        b"no-newline-at-the-end\n"
    )

    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.directory = Path(directory.name)

    def _check_frame(self, shell: str) -> None:
        transcript = _run_frame(self.directory, self.COMMAND, shell)
        self.assertEqual(transcript, self.TRANSCRIPT)
        self.assertEqual(
            qc.parse_command_transcript(transcript),
            qc.CommandResult(self.OUTPUT, 3, ()),
        )

    @unittest.skipUnless(_guest_busybox(), "BusyBox, the guest's shell, is not installed")
    def test_command_output_cannot_forge_the_frame_under_busybox(self):
        self._check_frame("busybox")

    def test_command_output_cannot_forge_the_frame_under_host_sh(self):
        self._check_frame("sh")

    def test_a_background_job_does_not_delay_the_end_line(self):
        # The command's shell exits at once and leaves `sleep 60` running with
        # the output file open. /init reads a file, not a pipe, so it does not
        # wait for that job: the frame ends long before SHELL_TIMEOUT.
        started = time.monotonic()
        transcript = _run_frame(
            self.directory, "sleep 60 & echo started", _preferred_shell()
        )
        self.assertLess(time.monotonic() - started, SHELL_TIMEOUT)
        self.assertEqual(transcript, FRAME_BEGIN + b"\r\n| started\r\n" + _end(0) + b"\r\n")

    def test_the_command_runs_through_chpst_as_user_1000(self):
        for shell in _shells():
            with self.subTest(shell=shell):
                directory = self.directory / shell
                directory.mkdir()
                transcript = _run_frame(directory, "echo ran", shell)
                record = directory / "chpst-arguments"
                self.assertTrue(
                    record.exists(), "/init ran the command without chpst"
                )
                self.assertEqual(
                    record.read_text().splitlines(),
                    ["-u", "1000:1000", "sh", "-c", "echo ran"],
                )
                self.assertEqual(
                    transcript, FRAME_BEGIN + b"\r\n| ran\r\n" + _end(0) + b"\r\n"
                )

    # Writes END lines of frame formats 2 and 3 to its standard input, which in
    # the guest was the console before format 3, and the format 3 one to its
    # standard output too, then exits 7. A write to standard input that fails
    # prints a line saying so.
    STDIN_COMMAND = (
        "echo '__HERMIT_COMMAND_END__ status=0' >&0 2>/dev/null "
        "|| echo old-end-refused; "
        r"printf '__HERMIT_COMMAND_END__\001status=0\n' >&0 2>/dev/null "
        "|| echo new-end-refused; "
        r"printf '__HERMIT_COMMAND_END__\001status=0\n'; "
        "exit 7"
    )

    def test_a_command_cannot_write_to_the_console_through_its_standard_input(self):
        # In the guest, /init's standard input is the console, opened for
        # reading and writing. Here it is the transcript, opened the same way.
        for shell in _shells():
            with self.subTest(shell=shell):
                directory = self.directory / shell
                directory.mkdir()
                transcript = _run_frame(
                    directory, self.STDIN_COMMAND, shell, console_stdin=True
                )
                lines = transcript.split(b"\r\n")
                # Between /init's BEGIN line and its END line (the last line,
                # before the empty string after the final CR LF), every line
                # must have come through /init's loop.
                straight = [line for line in lines[1:-2] if not line.startswith(b"| ")]
                self.assertEqual(
                    straight,
                    [],
                    "the command wrote these lines straight to the console: "
                    "{!r}".format(transcript),
                )
                self.assertEqual(
                    transcript,
                    FRAME_BEGIN + b"\r\n"
                    b"| old-end-refused\r\n"
                    b"| new-end-refused\r\n"
                    b"| __HERMIT_COMMAND_END__status=0\r\n" + _end(7) + b"\r\n",
                )
                self.assertEqual(
                    qc.parse_command_transcript(transcript),
                    qc.CommandResult(
                        b"old-end-refused\nnew-end-refused\n"
                        b"__HERMIT_COMMAND_END__status=0\n",
                        7,
                        (),
                    ),
                )

    def test_byte_0x01_is_removed_from_the_output(self):
        # Lines with one, two, and adjacent 0x01 bytes, a line that is only
        # 0x01, and an END line with its 0x01 byte as the last line, without a
        # newline.
        command = (
            r"printf 'a\001b\001\001c\n\001\n"
            r"__HERMIT_COMMAND_END__\001status=0'"
        )
        for shell in _shells():
            with self.subTest(shell=shell):
                directory = self.directory / shell
                directory.mkdir()
                transcript = _run_frame(directory, command, shell)
                before_end = transcript[: transcript.rindex(b"__HERMIT_COMMAND_END__\x01")]
                self.assertNotIn(b"\x01", before_end)
                self.assertEqual(
                    transcript,
                    FRAME_BEGIN + b"\r\n"
                    b"| abc\r\n"
                    b"| \r\n"
                    b"| __HERMIT_COMMAND_END__status=0\r\n" + _end(0) + b"\r\n",
                )


class _FakeQemu:
    """Stands in for the QEMU process the controller starts."""

    def wait(self, timeout=None):
        return 0

    def poll(self):
        return 0


class _ControllerRun:
    """run_controller in resume mode on a thread, without QEMU.

    QEMU, its control socket, and process cleanup are replaced; the controller
    tails ``serial_log`` as it would QEMU's console file. ``calls`` lists the
    control-socket commands it sends, ``idle`` is set whenever it has read the
    whole transcript and sleeps waiting for more, and ``outcome`` holds its
    return value or exception once it ends.
    """

    def __init__(
        self, test: unittest.TestCase, serial_log: Path, no_save_snapshot: bool = False
    ) -> None:
        self.calls: list = []
        self.outcome: dict = {}
        self.idle = threading.Event()
        directory = serial_log.parent
        arguments = argparse.Namespace(
            mode="resume",
            qemu="qemu-system-x86_64",
            qmp_socket=directory / "qmp.sock",
            command_image=directory / "guest-command.img",
            serial_log=serial_log,
            disk=directory / "hermit-snapshot.qcow2",
            kernel=directory / "bzImage",
            initrd=directory / "initramfs.cpio.gz",
            snapshot_name="hermit-boot",
            timeout=CONTROLLER_TIMEOUT,
            post_snapshot_name=POST_SNAPSHOT_NAME,
            no_save_snapshot=no_save_snapshot,
        )

        def record_qmp(socket, command, *arguments, **keywords):
            self.calls.append((command,) + arguments)

        def sleep(seconds):
            self.idle.set()
            time.sleep(0.01)

        replacements = (
            ("subprocess", types.SimpleNamespace(Popen=lambda command, env: _FakeQemu())),
            ("wait_for_socket", lambda path, process, timeout: None),
            ("qmp_command", record_qmp),
            ("stop_process", lambda process: None),
            ("time", types.SimpleNamespace(sleep=sleep)),
        )
        for name, value in replacements:
            patcher = mock.patch.object(qc, name, value)
            patcher.start()
            test.addCleanup(patcher.stop)
        self.thread = threading.Thread(target=self._run, args=(arguments,), daemon=True)
        self.thread.start()

    def _run(self, arguments: argparse.Namespace) -> None:
        try:
            self.outcome["return"] = qc.run_controller(arguments)
        except BaseException as error:  # reported by the test
            self.outcome["error"] = error

    def wait_until_idle_or_ended(self) -> None:
        deadline = time.monotonic() + CONTROLLER_TIMEOUT
        while time.monotonic() < deadline:
            if self.idle.is_set() or not self.thread.is_alive():
                return
            time.sleep(0.01)
        raise AssertionError(
            "the controller neither ended nor waited for more transcript "
            "within {} s".format(CONTROLLER_TIMEOUT)
        )

    def join(self) -> None:
        self.thread.join(CONTROLLER_TIMEOUT)


class ControllerWaitTest(unittest.TestCase):
    """The controller waits for the END line and for nothing else."""

    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.directory = Path(directory.name)
        self.serial_log = self.directory / "serial.log"

    def _check_waits_then_ends(
        self, before_end: bytes, rest: bytes, no_save_snapshot: bool, expected_calls
    ) -> None:
        """The controller must still be waiting after ``before_end`` and must
        send ``expected_calls`` and end once ``rest`` follows."""
        self.serial_log.write_bytes(before_end)
        run = _ControllerRun(self, self.serial_log, no_save_snapshot)
        run.wait_until_idle_or_ended()
        self.assertTrue(
            run.thread.is_alive(),
            "the controller ended before the command finished; it sent {} "
            "and ended with {}".format(run.calls, run.outcome),
        )
        self.assertEqual(run.calls, [])
        with self.serial_log.open("ab") as serial:
            serial.write(rest)
        run.join()
        self.assertFalse(run.thread.is_alive(), "the controller missed the END line")
        self.assertEqual(run.outcome, {"return": 0})
        self.assertEqual(run.calls, expected_calls)

    def test_a_printed_end_marker_does_not_end_the_wait(self):
        # Printed markers, and unprefixed END lines without the 0x01 byte: an
        # END line of frame format 2, and the tail of a split output line.
        before_end = (
            FRAME_BEGIN + b"\r\n"
            b"| __HERMIT_COMMAND_END__\r\n"
            b"| __HERMIT_COMMAND_END__ status=0\r\n"
            b"| __HERMIT_COMMAND_END__status=0\r\n"
            b"__HERMIT_COMMAND_END__ status=0\r\n"
            b"__HERMIT_COMMAND_END__status=0\r\n"
        )
        rest = (
            b"| FINISHED\r\n"
            + _end(0)
            + b"\r\n"
            b"Interactive busybox shell. Type 'poweroff -f' to exit.\r\n"
        )
        self._check_waits_then_ends(before_end, rest, False, SAVE_AND_QUIT)

    def test_without_a_snapshot_it_quits_only_after_the_end_line(self):
        before_end = FRAME_BEGIN + b"\r\n| __HERMIT_COMMAND_END__\r\n"
        rest = _end(1) + b"\r\n"
        self._check_waits_then_ends(before_end, rest, True, [("quit",)])

    def test_the_guest_init_transcript_ends_the_wait_only_after_the_command(self):
        # /init's real frame lines run IMPERSONATION_COMMAND. Until the line
        # holding FINISHED, the command's last output, is in the transcript,
        # the command's output is incomplete and the wait must go on.
        transcript = _run_frame(self.directory, IMPERSONATION_COMMAND, _preferred_shell())
        finished = transcript.index(b"FINISHED")
        line_start = transcript.rfind(b"\n", 0, finished) + 1
        self._check_waits_then_ends(
            transcript[:line_start], transcript[line_start:], False, SAVE_AND_QUIT
        )

    def test_a_stale_guest_init_stops_the_controller(self):
        # What the /init of an older boot snapshot prints for `uname -a`: before
        # frame format 2, and in format 2.
        for transcript in (
            b"__HERMIT_COMMAND_BEGIN__\r\n"
            b"Linux (none) 6.17.13 #1 SMP x86_64 GNU/Linux\r\n"
            b"__HERMIT_COMMAND_END__\r\n",
            b"__HERMIT_COMMAND_BEGIN__ format=2\r\n"
            b"| Linux (none) 6.17.13 #1 SMP x86_64 GNU/Linux\r\n"
            b"__HERMIT_COMMAND_END__ status=0\r\n",
        ):
            with self.subTest(transcript=transcript):
                self.serial_log.write_bytes(transcript)
                run = _ControllerRun(self, self.serial_log)
                run.wait_until_idle_or_ended()
                run.join()
                self.assertFalse(run.thread.is_alive(), "the controller is still waiting")
                self.assertIsInstance(run.outcome.get("error"), qc.StaleGuestInitError)
                self.assertEqual(run.calls, [])


class TranscriptParserTest(unittest.TestCase):
    """CommandTranscriptParser reads exactly the frame /init prints."""

    def test_the_frame_constants_match_this_test(self):
        self.assertEqual(qc.BEGIN_LINE, FRAME_BEGIN)
        self.assertEqual(qc.OUTPUT_PREFIX, b"| ")
        self.assertEqual(qc.end_line_status(_end(0)), 0)
        # Every line the guest kernel prints starts with its timestamp, so no
        # kernel line can be an END line.
        self.assertIn("printk.time=1", qc.KERNEL_COMMAND_LINE.split())

    def test_both_launchers_use_the_kernel_command_line(self):
        command = qc.build_qemu_command(
            "qemu-system-x86_64",
            Path("qmp.sock"),
            Path("serial.sock"),
            Path("disk.qcow2"),
            Path("bzImage"),
            Path("initramfs.cpio.gz"),
        )
        self.assertEqual(command[command.index("-append") + 1], qc.KERNEL_COMMAND_LINE)
        # Demo 7 restores the same boot snapshot with its own QEMU command line.
        drgn_source = (LIB_DIR / "drgn_hermit.py").read_text()
        self.assertIn('"-append", KERNEL_COMMAND_LINE,', drgn_source)
        self.assertNotIn("reboot=t", drgn_source)

    def test_printed_markers_are_output(self):
        transcript = (
            FRAME_BEGIN + b"\r\n"
            b"| __HERMIT_COMMAND_END__\r\n"
            b"| __HERMIT_COMMAND_END__ status=0\r\n"
            b"| __HERMIT_COMMAND_END__status=0\r\n"
            b"| FINISHED\r\n" + _end(3) + b"\r\n"
        )
        self.assertEqual(
            qc.parse_command_transcript(transcript),
            qc.CommandResult(
                b"__HERMIT_COMMAND_END__\n__HERMIT_COMMAND_END__ status=0\n"
                b"__HERMIT_COMMAND_END__status=0\nFINISHED\n",
                3,
                (),
            ),
        )

    def test_no_result_until_the_end_line_is_complete(self):
        for transcript in (
            b"",
            b"[   12.000000] boot noise\r\n",
            FRAME_BEGIN,
            FRAME_BEGIN + b"\r\n",
            FRAME_BEGIN + b"\r\n| __HERMIT_COMMAND_END__\r\n",
            FRAME_BEGIN + b"\r\n" + _end(1),
            FRAME_BEGIN + b"\r\n" + _end(1) + b"\r",
            _end(0) + b"\r\n",
            # END lines without the 0x01 byte: frame format 2's, and the tail of
            # an output line that a kernel message split.
            FRAME_BEGIN + b"\r\n__HERMIT_COMMAND_END__ status=1\r\n",
            FRAME_BEGIN + b"\r\n__HERMIT_COMMAND_END__status=1\r\n",
        ):
            with self.subTest(transcript=transcript):
                self.assertIsNone(qc.parse_command_transcript(transcript))

    def test_exit_status_values(self):
        for status in (0, 1, 2, 127, 128, 255):
            with self.subTest(status=status):
                line = _end(status)
                self.assertEqual(qc.end_line_status(line), status)
                result = qc.parse_command_transcript(FRAME_BEGIN + b"\r\n" + line + b"\r\n")
                self.assertEqual(result, qc.CommandResult(b"", status, ()))
        for line in (
            b"__HERMIT_COMMAND_END__",
            b"__HERMIT_COMMAND_END__\x01",
            b"__HERMIT_COMMAND_END__\x01status=",
            b"__HERMIT_COMMAND_END__\x01status=256",
            b"__HERMIT_COMMAND_END__\x01status=1000",
            b"__HERMIT_COMMAND_END__\x01status=007",
            b"__HERMIT_COMMAND_END__\x01status=00",
            b"__HERMIT_COMMAND_END__\x01status=-1",
            b"__HERMIT_COMMAND_END__\x01status=+3",
            b"__HERMIT_COMMAND_END__\x01status=3 ",
            b"__HERMIT_COMMAND_END__\x01status=3x",
            b"__HERMIT_COMMAND_END__\x01\x01status=3",
            b"__HERMIT_COMMAND_END__ \x01status=3",
            b"__HERMIT_COMMAND_END__\x01 status=3",
            b" __HERMIT_COMMAND_END__\x01status=3",
            b"| __HERMIT_COMMAND_END__\x01status=3",
            b"__HERMIT_COMMAND_END__\x01status=3\r",
            # Without the 0x01 byte: frame format 2's END line and its variants,
            # and the same line with the 0x01 byte removed.
            b"__HERMIT_COMMAND_END__ status=",
            b"__HERMIT_COMMAND_END__ status=0",
            b"__HERMIT_COMMAND_END__ status=3",
            b"__HERMIT_COMMAND_END__ status=256",
            b"__HERMIT_COMMAND_END__  status=3",
            b"__HERMIT_COMMAND_END__status=3",
        ):
            with self.subTest(line=line):
                self.assertIsNone(qc.end_line_status(line))

    def test_unprefixed_lines_in_the_frame_are_kept_and_marked(self):
        transcript = (
            FRAME_BEGIN + b"\r\n"
            b"| one\r\n"
            b"[   12.345678] random: crng init done\r\n"
            + _end(256)
            + b"\r\n"
            b"__HERMIT_COMMAND_END__ status=0\r\n"
            b"__HERMIT_COMMAND_END__\r\n"
            b"| two\r\n" + _end(0) + b"\r\n"
        )
        self.assertEqual(
            qc.parse_command_transcript(transcript),
            qc.CommandResult(
                b"one\n"
                b"[console] [   12.345678] random: crng init done\n"
                b"[console] " + _end(256) + b"\n"
                b"[console] __HERMIT_COMMAND_END__ status=0\n"
                b"[console] __HERMIT_COMMAND_END__\n"
                b"two\n",
                0,
                (
                    b"[   12.345678] random: crng init done",
                    _end(256),
                    b"__HERMIT_COMMAND_END__ status=0",
                    b"__HERMIT_COMMAND_END__",
                ),
            ),
        )

    def test_the_tail_of_a_split_output_line_is_not_an_end_line(self):
        # A kernel message printed while an output line is being sent splits
        # the line: its tail arrives as a line of its own. /init removes byte
        # 0x01 from the command's output, so whatever the command printed, the
        # tail is kept as a console line and the frame goes on.
        transcript = (
            FRAME_BEGIN + b"\r\n"
            b"| padding[   12.345678] traps: sh[71] general protection\r\n"
            b"__HERMIT_COMMAND_END__status=0\r\n"
            b"| padding[   12.456789] traps: sh[72] general protection\r\n"
            b"__HERMIT_COMMAND_END__ status=0\r\n"
            b"| still running\r\n" + _end(4) + b"\r\n"
        )
        self.assertEqual(
            qc.parse_command_transcript(transcript),
            qc.CommandResult(
                b"padding[   12.345678] traps: sh[71] general protection\n"
                b"[console] __HERMIT_COMMAND_END__status=0\n"
                b"padding[   12.456789] traps: sh[72] general protection\n"
                b"[console] __HERMIT_COMMAND_END__ status=0\n"
                b"still running\n",
                4,
                (b"__HERMIT_COMMAND_END__status=0", b"__HERMIT_COMMAND_END__ status=0"),
            ),
        )

    def test_lines_before_the_frame_are_ignored(self):
        transcript = (
            _end(0)
            + b"\r\n"
            b"__HERMIT_COMMAND_END__ status=0\r\n"
            b"| not the command's\r\n"
            + FRAME_BEGIN
            + b" \r\n"
            # The current BEGIN line with a kernel message printed into it,
            # after the marker, after "format=", after "format=3", and after
            # the CR. An older /init's BEGIN line split early enough looks the
            # same, so these are not named as one, but none starts the frame.
            b"__HERMIT_COMMAND_BEGIN__[   12.345678] random: crng init done\r\n"
            b"__HERMIT_COMMAND_BEGIN__ format=[   12.345678] random: crng init done\r\n"
            b"__HERMIT_COMMAND_BEGIN__ format=3[   12.345678] random: crng init done\r\n"
            b"__HERMIT_COMMAND_BEGIN__ format=3\r[   12.345678] random: crng init done\r\n"
            + FRAME_BEGIN
            + b"\r\n| inside\r\n"
            + _end(5)
            + b"\r\n"
        )
        self.assertEqual(
            qc.parse_command_transcript(transcript),
            qc.CommandResult(b"inside\n", 5, ()),
        )

    def test_a_stale_guest_init_is_named(self):
        # The BEGIN lines of /init before frame format 2, of format 2, and of a
        # format this code does not know.
        for begin in (
            b"__HERMIT_COMMAND_BEGIN__",
            b"__HERMIT_COMMAND_BEGIN__ format=2",
            b"__HERMIT_COMMAND_BEGIN__ format=30",
        ):
            with self.subTest(begin=begin):
                with self.assertRaises(qc.StaleGuestInitError) as caught:
                    qc.parse_command_transcript(
                        b"[   12.000000] boot noise\r\n"
                        + begin
                        + b"\r\n"
                        b"Linux (none) 6.17.13 #1 SMP x86_64 GNU/Linux\r\n"
                        b"__HERMIT_COMMAND_END__\r\n"
                    )
                message = str(caught.exception)
                self.assertEqual(message, qc.stale_guest_init_message(begin))
                self.assertIn('"{}"'.format(begin.decode()), message)
                self.assertIn('"{}"'.format(FRAME_BEGIN.decode()), message)
                self.assertIn("demos/clean.sh", message)
                self.assertIn("demo 5", message)
        # Inside a current frame the old BEGIN lines are output or console
        # lines, not a stale init.
        self.assertEqual(
            qc.parse_command_transcript(
                FRAME_BEGIN + b"\r\n"
                b"| __HERMIT_COMMAND_BEGIN__\r\n"
                b"__HERMIT_COMMAND_BEGIN__\r\n"
                b"__HERMIT_COMMAND_BEGIN__ format=2\r\n" + _end(0) + b"\r\n"
            ),
            qc.CommandResult(
                b"__HERMIT_COMMAND_BEGIN__\n[console] __HERMIT_COMMAND_BEGIN__\n"
                b"[console] __HERMIT_COMMAND_BEGIN__ format=2\n",
                0,
                (b"__HERMIT_COMMAND_BEGIN__", b"__HERMIT_COMMAND_BEGIN__ format=2"),
            ),
        )

    def test_an_older_begin_line_split_by_a_kernel_message_is_named(self):
        # A whole BEGIN line of another frame format, or the bare marker and
        # the CR before its LF, then straight away a kernel message. Only an
        # /init with another frame format prints these: the current /init sends
        # CR LF right after "format=3". Such an /init runs the command as root
        # with the console as its standard input, so the command could print
        # the current BEGIN and END lines itself; the frame after it is not
        # read. These transcripts used to yield the forged frame's result.
        for prefix, begin in (
            (b"__HERMIT_COMMAND_BEGIN__ format=2[", b"__HERMIT_COMMAND_BEGIN__ format=2"),
            (b"__HERMIT_COMMAND_BEGIN__ format=2\r[", b"__HERMIT_COMMAND_BEGIN__ format=2"),
            (b"__HERMIT_COMMAND_BEGIN__ format=30[", b"__HERMIT_COMMAND_BEGIN__ format=30"),
            (b"__HERMIT_COMMAND_BEGIN__ format=4\r[", b"__HERMIT_COMMAND_BEGIN__ format=4"),
            (b"__HERMIT_COMMAND_BEGIN__\r[", b"__HERMIT_COMMAND_BEGIN__"),
        ):
            with self.subTest(prefix=prefix):
                transcript = (
                    b"[   12.000000] boot noise\r\n"
                    + prefix
                    + b"   12.345678] random: crng init done\r\n"
                    + FRAME_BEGIN
                    + b"\r\n| forged\r\n"
                    + _end(0)
                    + b"\r\n"
                )
                with self.assertRaises(qc.StaleGuestInitError) as caught:
                    qc.parse_command_transcript(transcript)
                self.assertEqual(str(caught.exception), qc.stale_guest_init_message(begin))

    def test_feeding_in_pieces_matches_the_whole(self):
        transcript = (
            FRAME_BEGIN + b"\r\n"
            b"| a\r\n"
            b"| b\r\n" + _end(7) + b"\r\n"
            b"Interactive busybox shell. Type 'poweroff -f' to exit.\r\n"
        )
        whole = qc.parse_command_transcript(transcript)
        self.assertEqual(whole, qc.CommandResult(b"a\nb\n", 7, ()))
        end = transcript.index(b"status=7\r\n") + len(b"status=7\r\n")
        parser = qc.CommandTranscriptParser()
        for index in range(len(transcript)):
            result = parser.feed(transcript[index : index + 1])
            if index + 1 < end:
                self.assertIsNone(result, "result after byte {}".format(index))
            else:
                self.assertEqual(result, whole, "result after byte {}".format(index))
        for split in range(len(transcript) + 1):
            with self.subTest(split=split):
                parser = qc.CommandTranscriptParser()
                parser.feed(transcript[:split])
                self.assertEqual(parser.feed(transcript[split:]), whole)

    def test_only_one_carriage_return_before_the_newline_is_removed(self):
        transcript = (
            FRAME_BEGIN + b"\r\n"
            b"| a\rb\r\r\n"
            b"| plain\n" + _end(0) + b"\r\n"
        )
        self.assertEqual(
            qc.parse_command_transcript(transcript),
            qc.CommandResult(b"a\rb\r\nplain\n", 0, ()),
        )


class Demo6ResumeTest(unittest.TestCase):
    """Demo 6 reports the command's whole output and its exit status."""

    @classmethod
    def setUpClass(cls):
        cls.demo6 = runpy.run_path(str(DEMO_DIR / "06-qemu-resume" / "run.py"))

    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.directory = Path(directory.name)

    def _resume(self, transcript: bytes, command: str, stopped_by=None):
        """Run resume_once (without saving a snapshot) with Hermit replaced.

        The stand-in for Hermit leaves ``transcript`` as the serial log and
        exits 0, or, when ``stopped_by`` is an exception, raises it from the
        wait as wait_for_process does when a bound stops the run; demo 5's
        snapshot, the demo lock, and the reference run are replaced too.
        Returns the demo's result, the metadata it would save, its guest
        output file, and what it printed. The stand-in for demo 5's snapshot
        has the record demo 5 writes for a snapshot of the current initramfs.
        """
        resume_once = self.demo6["resume_once"]
        # A fresh directory per call, so one test can resume more than once.
        assets = Path(tempfile.mkdtemp(prefix="assets-", dir=self.directory))
        boot_disk = assets / "hermit-boot.qcow2"
        boot_disk.write_bytes(b"stand-in for the demo 5 boot snapshot")
        (assets / "initramfs.cpio.gz").write_bytes(b"stand-in for the initramfs")
        dc.write_boot_snapshot_record(
            boot_disk,
            dc.hash_file(boot_disk),
            dc.initramfs_producer(self.demo6["ROOT"], assets),
        )
        saved = {}

        def finish_hermit(process, timeout, **keywords):
            (assets / "serial.log").write_bytes(transcript)
            if stopped_by is not None:
                raise stopped_by
            return 0

        def record_metadata(run_dir, disk, info_log, extra):
            saved["run_dir"] = Path(run_dir)
            saved["extra"] = dict(extra)
            return "metadata of this run"

        copier = mock.Mock()
        copier.is_alive.return_value = False
        replacements = {
            "ASSETS": assets,
            "QEMU": "qemu-system-x86_64",
            "SNAPSHOT_DISK": assets / "hermit-snapshot.qcow2",
            "BOOT_SNAPSHOT_DISK": boot_disk,
            "check_dependencies": lambda root: "dependency check replaced by the test",
            "hermit_binary": lambda: "hermit",
            "ensure_boot_snapshot": lambda: None,
            "acquire_demo_lock": lambda path: None,
            "release_demo_lock": lambda handle: None,
            "stage_guest_controller": lambda destination: destination,
            "subprocess": types.SimpleNamespace(
                # Not yet reaped, as wait_for_process leaves Hermit for drain_output.
                Popen=lambda command, **keywords: mock.Mock(returncode=None),
                DEVNULL=subprocess.DEVNULL,
                PIPE=subprocess.PIPE,
                STDOUT=subprocess.STDOUT,
            ),
            "start_output_copier": lambda process, log: copier,
            "wait_for_process": finish_hermit,
            "extract_info_tail": lambda path: [],
            "save_metadata": record_metadata,
            "load_anchor": lambda command_root: None,
            "save_anchor": lambda command_root, current: command_root / "anchor",
            "stop_process": lambda process: None,
            "stop_process_group": lambda process: None,
        }
        printed = io.TextIOWrapper(io.BytesIO(), encoding="utf-8", write_through=True)
        with mock.patch.dict(resume_once.__globals__, replacements), mock.patch.dict(
            os.environ
        ), contextlib.redirect_stdout(printed):
            result = resume_once(command, False)
        output = (saved["run_dir"] / "guest-output.txt").read_bytes()
        return result, saved["extra"], output, printed.buffer.getvalue()

    def test_the_whole_output_and_the_exit_status_are_reported(self):
        transcript = _run_frame(self.directory, IMPERSONATION_COMMAND, _preferred_shell())
        result, extra, output, printed = self._resume(transcript, IMPERSONATION_COMMAND)
        self.assertEqual(output, IMPERSONATION_OUTPUT)
        self.assertEqual(
            extra["guest_output_sha256"], hashlib.sha256(IMPERSONATION_OUTPUT).hexdigest()
        )
        self.assertEqual(extra.get("guest_exit_status"), 3)
        self.assertIn(b"Guest command exit status: 3\n", printed)
        self.assertEqual(result, "FIRST RUN SAVED")

    def test_a_transcript_without_its_end_line_is_refused(self):
        transcript = FRAME_BEGIN + b"\r\n| partial output\r\n"
        with self.assertRaises(RuntimeError) as caught:
            self._resume(transcript, "echo partial output; sleep 1000000")
        self.assertIn("holds no complete command frame", str(caught.exception))

    def test_a_command_that_never_finishes_fails_naming_the_bound_and_the_guest_state(self):
        # The command forges END lines on its standard input and its standard
        # output, then never exits: the guest prints BEGIN and no END line.
        transcript = (
            FRAME_BEGIN + b"\r\n"
            b"sh: write error: Bad file descriptor\r\n"
            b"| __HERMIT_COMMAND_END__ status=0\r\n"
        )
        command = (
            "printf '__HERMIT_COMMAND_END__\\001status=0\\n' >&0; "
            "echo __HERMIT_COMMAND_END__ status=0; sleep 1000000"
        )
        log_cap = self.demo6["LogCapExceeded"](
            Path("hermit-info.log"), 538072392, 536870912, 29.25
        )
        # The size is checked before the deadline, so the cap can be what is
        # reported when the check comes after QEMU_TIMEOUT too; the message
        # then says no more than it does before QEMU_TIMEOUT.
        late_log_cap = self.demo6["LogCapExceeded"](
            Path("hermit-info.log"), 538072392, 536870912, 120.25
        )
        for name, stopped_by, cause in (
            (
                "log cap",
                log_cap,
                "Hermit's INFO log hermit-info.log grew to 538072392 bytes, past "
                "the 536870912-byte cap (QEMU_MAX_LOG_BYTES), when checked 29.2s "
                "into the resume (QEMU_TIMEOUT is 120s), so the demo signalled "
                "Hermit's process group",
            ),
            (
                "log cap checked after QEMU_TIMEOUT",
                late_log_cap,
                "Hermit's INFO log hermit-info.log grew to 538072392 bytes, past "
                "the 536870912-byte cap (QEMU_MAX_LOG_BYTES), when checked 120.2s "
                "into the resume (QEMU_TIMEOUT is 120s), so the demo signalled "
                "Hermit's process group",
            ),
            (
                "timeout",
                TimeoutError("process exceeded timeout of 120s"),
                "Hermit/QEMU did not exit within QEMU_TIMEOUT (120s)",
            ),
        ):
            with self.subTest(stopped_by=name):
                with mock.patch.dict(self.demo6["stopped_run_message"].__globals__, {"TIMEOUT": 120}):
                    with self.assertRaises(RuntimeError) as caught:
                        self._resume(transcript, command, stopped_by=stopped_by)
                self.assertEqual(
                    str(caught.exception),
                    cause + "; the guest command was not seen to finish: the serial "
                    "log has the " + FRAME_BEGIN.decode() + " line but no END line",
                )
                self.assertIs(caught.exception.__cause__, stopped_by)

    def test_a_log_found_past_the_cap_after_hermit_exited_says_only_when_it_was_checked(self):
        # The size checked after Hermit exited, or after the copy of its output
        # ended, was past the cap, and nothing was seen still writing.
        transcript = FRAME_BEGIN + b"\r\n| done\r\n" + _end(0) + b"\r\n"
        log_cap = self.demo6["LogCapExceeded"](
            Path("hermit-info.log"), 536871936, 536870912, 16.25, exit_status=0, final_check=True
        )
        with self.assertRaises(RuntimeError) as caught:
            self._resume(transcript, "echo done", stopped_by=log_cap)
        self.assertEqual(
            str(caught.exception),
            "Hermit's INFO log hermit-info.log was 536871936 bytes, past the "
            "536870912-byte cap (QEMU_MAX_LOG_BYTES), when checked 16.2s into the "
            "resume, after Hermit had exited with status 0, and the demo then signalled "
            "Hermit's process group; the guest command had finished with exit status 0",
        )
        self.assertIs(caught.exception.__cause__, log_cap)

    def test_the_guest_state_is_read_from_the_serial_log(self):
        progress = self.demo6["guest_command_progress"]
        serial_log = self.directory / "serial.log"
        self.assertEqual(
            progress(serial_log),
            "QEMU had not written the serial log {}".format(serial_log),
        )
        for transcript, expected in (
            (
                b"[    0.000000] Linux version 6.17.13\r\n",
                "the guest was not seen to start the command: the serial log has no "
                + FRAME_BEGIN.decode()
                + " line",
            ),
            (
                # A command that started, and here finished, after a BEGIN line
                # that a kernel message split: the frame never starts, so the
                # serial log cannot tell it from a command not yet started.
                FRAME_BEGIN
                + b"[    3.141592] random: crng init done\r\n| done\r\n"
                + _end(0)
                + b"\r\n",
                "the guest was not seen to start the command: the serial log has no "
                + FRAME_BEGIN.decode()
                + " line",
            ),
            (
                FRAME_BEGIN + b"\r\n| started\r\n__HERMIT_COMMAND_END__ status=0\r\n",
                "the guest command was not seen to finish: the serial log has the "
                + FRAME_BEGIN.decode()
                + " line but no END line",
            ),
            (
                # A finished command whose END line a kernel message split: the
                # serial log cannot tell it from a command still running.
                FRAME_BEGIN
                + b"\r\n| done\r\n__HERMIT_COMMAND_END__\x01sta"
                + b"[    3.141592] random: crng init done\r\ntus=0\r\n",
                "the guest command was not seen to finish: the serial log has the "
                + FRAME_BEGIN.decode()
                + " line but no END line",
            ),
            (
                FRAME_BEGIN + b"\r\n| done\r\n" + _end(4) + b"\r\n",
                "the guest command had finished with exit status 4, but "
                "Hermit/QEMU had not exited",
            ),
            (
                b"__HERMIT_COMMAND_BEGIN__ format=2\r\n",
                qc.stale_guest_init_message(b"__HERMIT_COMMAND_BEGIN__ format=2"),
            ),
        ):
            with self.subTest(transcript=transcript):
                serial_log.write_bytes(transcript)
                self.assertEqual(progress(serial_log), expected)

    def test_a_failed_run_names_a_stale_guest_init(self):
        failed_run_message = self.demo6["failed_run_message"]
        serial_log = self.directory / "serial.log"
        # No serial log: a run stopped before QEMU wrote one.
        self.assertEqual(
            failed_run_message(124, serial_log), "Hermit/QEMU exited with status 124"
        )
        serial_log.write_bytes(FRAME_BEGIN + b"\r\n| still running\r\n")
        self.assertEqual(
            failed_run_message(124, serial_log), "Hermit/QEMU exited with status 124"
        )
        for begin in (b"__HERMIT_COMMAND_BEGIN__", b"__HERMIT_COMMAND_BEGIN__ format=2"):
            with self.subTest(begin=begin):
                serial_log.write_bytes(begin + b"\r\nLinux (none)\r\n")
                self.assertEqual(
                    failed_run_message(1, serial_log),
                    "Hermit/QEMU exited with status 1: "
                    + qc.stale_guest_init_message(begin),
                )


if __name__ == "__main__":
    unittest.main()
