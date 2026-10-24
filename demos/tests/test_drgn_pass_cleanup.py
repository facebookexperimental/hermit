#!/usr/bin/env python3
"""Tests for how demo 7 starts a pass under Hermit and what a pass leaves behind.

Each demo 7 pass copies demo 5's snapshot into a fresh run directory under
DEMO07_ARTIFACTS and restores it in QEMU under Hermit. QEMU opens the run
directory's QMP socket, serial FIFOs and disks from inside Hermit, which gives
it a private /tmp unless it is passed --tmp=/tmp. Only the checkout's location
used to decide that, so a run directory under host /tmp, from a checkout
elsewhere, was invisible to QEMU: it could not create its QMP socket, and the
demo printed only "Hermit exited before QMP connected (status 1)" and kept the
failed pass's 94 MB snapshot copy.

start() is run here with Hermit's launch and the QMP connection replaced, which
is enough to see the command line Hermit would get and what a failed pass
leaves.

A pass also must not leave processes behind. The demo used to kill only the
process group of the `hermit` it started, and safehermit runs Hermit as a
systemd user unit outside that group, so Hermit's tracer, which the demo stops
with SIGSTOP, and QEMU outlived every pass, about 140 MB each.
"""

import contextlib
import dataclasses
import errno
import io
import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest import mock

LIB_DIR = Path(__file__).resolve().parent.parent / "lib"
sys.path.insert(0, str(LIB_DIR))

import demo_common as dc  # noqa: E402
import drgn_hermit as dh  # noqa: E402


class _StopAtQmpConnect(Exception):
    """Raised in place of the QMP connection, after Hermit's command line is built."""


def _directory(test: unittest.TestCase, parent: str, prefix: str) -> Path:
    directory = tempfile.TemporaryDirectory(dir=parent, prefix=prefix)
    test.addCleanup(directory.cleanup)
    return Path(directory.name)


def _hermit_options(argv):
    """Return Hermit's own options: the words between `run` and `--`."""
    return argv[argv.index("run") + 1 : argv.index("--")]


def _record_boot_snapshot(root: Path, assets: Path) -> None:
    """Give ``assets``/hermit-boot.qcow2 the record demo 5 writes, so that
    start() restores it: ``root`` is a stand-in checkout whose qemu-assets.sh
    builds INITRAMFS_VERSION 9, and ``assets`` holds the initramfs."""
    script = root / "demos/lib/qemu-assets.sh"
    script.parent.mkdir(parents=True, exist_ok=True)
    script.write_text("INITRAMFS_VERSION=9\n")
    snapshot = assets / "hermit-boot.qcow2"
    dc.write_boot_snapshot_record(
        snapshot, dc.hash_file(snapshot), dc.initramfs_producer(root, assets)
    )


class _PassHarness(unittest.TestCase):
    def setUp(self):
        # Neither the checkout nor the inputs are under host /tmp unless a test
        # puts them there.
        self.outside_tmp = _directory(self, "/var/tmp", "drgn-pass-test-")
        self.root = self.outside_tmp / "checkout"
        self.root.mkdir()
        self.inputs = self.outside_tmp / "inputs"
        self.inputs.mkdir()
        environment = mock.patch.dict(
            os.environ, {"QEMU_SOCKET_DIR": str(self.outside_tmp / "sockets")}
        )
        environment.start()
        self.addCleanup(environment.stop)

    def _config(self, artifact_dir: Path, inputs: Path = None) -> dh.GuestConfig:
        inputs = inputs or self.inputs
        for name in ("hermit", "qemu", "bzImage", "initramfs.cpio.gz"):
            (inputs / name).write_bytes(b"")
        (inputs / "hermit-boot.qcow2").write_bytes(b"q" * 4096)
        _record_boot_snapshot(self.root, inputs)
        return dh.GuestConfig(
            root=self.root,
            hermit=inputs / "hermit",
            qemu=inputs / "qemu",
            kernel=inputs / "bzImage",
            initrd=inputs / "initramfs.cpio.gz",
            vmlinux=inputs / "vmlinux",
            snapshot_disk=inputs / "hermit-boot.qcow2",
            snapshot_name="hermit-boot",
            advance_command="echo deterministic",
            artifact_dir=artifact_dir,
            assets=inputs,
        )

    def _launch_patches(self, config, popen, connect):
        stack = contextlib.ExitStack()
        stack.enter_context(mock.patch.object(dh, "ensure_vmlinux", return_value=config.vmlinux))
        stack.enter_context(mock.patch.object(dh.subprocess, "Popen", popen))
        stack.enter_context(mock.patch.object(dh.os, "getpgid", return_value=4242))
        stack.enter_context(mock.patch.object(dh.QmpClient, "connect", connect))
        return stack

    def _hermit_argv(self, config):
        """Run start() until it connects to QMP; return the command Hermit got."""
        popen = mock.Mock(return_value=mock.Mock(pid=4242))
        connect = mock.Mock(side_effect=_StopAtQmpConnect)
        program = dh.HermitGuestProgram(config)
        with self._launch_patches(config, popen, connect):
            with self.assertRaises(_StopAtQmpConnect):
                program.start()
        return program, popen.call_args.args[0]


class HostTmpBindingTest(_PassHarness):
    def test_a_run_directory_under_host_tmp_gets_the_host_tmp(self):
        # The configuration that failed: a checkout outside /tmp and
        # DEMO07_ARTIFACTS a short directory under it.
        artifact_dir = _directory(self, "/tmp", "d7-") / "artifacts"
        program, argv = self._hermit_argv(self._config(artifact_dir))
        self.assertTrue(dc._under_host_tmp(program.run_dir), program.run_dir)
        self.assertEqual(_hermit_options(argv).count("--tmp=/tmp"), 1, argv)
        # The socket stays in the run directory, where QEMU can now create it.
        self.assertEqual(program.qmp_socket, program.run_dir / "qmp.sock")

    def test_a_kernel_under_host_tmp_gets_the_host_tmp(self):
        inputs = _directory(self, "/tmp", "d7-inputs-")
        artifact_dir = self.outside_tmp / "artifacts"
        _, argv = self._hermit_argv(self._config(artifact_dir, inputs))
        self.assertEqual(_hermit_options(argv).count("--tmp=/tmp"), 1, argv)

    def test_a_library_directory_under_host_tmp_gets_the_host_tmp(self):
        # DEMO07_QEMU_LIBRARY_PATH is prepended to LD_LIBRARY_PATH, so any
        # entry of a list counts, not only the first.
        libraries = _directory(self, "/tmp", "d7-lib-")
        config = dataclasses.replace(
            self._config(self.outside_tmp / "artifacts"),
            qemu_library_path=Path("{}:{}".format(self.outside_tmp / "lib", libraries)),
        )
        _, argv = self._hermit_argv(config)
        self.assertEqual(_hermit_options(argv).count("--tmp=/tmp"), 1, argv)

    def test_nothing_under_host_tmp_keeps_the_private_tmp(self):
        # Positive control: the usual layout is unchanged.
        artifact_dir = self.outside_tmp / "artifacts"
        program, argv = self._hermit_argv(self._config(artifact_dir))
        self.assertFalse(dc._under_host_tmp(program.run_dir), program.run_dir)
        self.assertNotIn("--tmp=/tmp", argv)

    def test_the_helper_considers_every_path_it_is_given(self):
        self.assertEqual(
            dc.hermit_tmp_args(Path("/srv/hermit"), Path("/tmp/d7/run.x")), ["--tmp=/tmp"]
        )
        self.assertEqual(
            dc.hermit_tmp_args(Path("/srv/hermit"), None, Path("/var/tmp/run.x")), []
        )
        # The checkout alone still decides as before.
        self.assertEqual(dc.hermit_tmp_args(Path("/tmp/work/hermit")), ["--tmp=/tmp"])
        self.assertEqual(dc.hermit_tmp_args(Path("/srv/hermit")), [])


class FailedPassTest(_PassHarness):
    LOG_LINES = ["hermit line {:02d}".format(number) for number in range(1, 61)] + [
        "qemu-system-x86_64: -qmp unix:/tmp/d7/run.x/qmp.sock,server=on,wait=off: "
        "Failed to bind socket to /tmp/d7/run.x/qmp.sock: No such file or directory"
    ]

    def _failed_start(self, artifact_dir: Path):
        """Start a pass whose Hermit writes LOG_LINES and exits before QMP connects."""
        config = self._config(artifact_dir)

        def popen(command, **options):
            options["stdout"].write(("\n".join(self.LOG_LINES) + "\n").encode())
            process = mock.Mock(pid=4242, returncode=1)
            process.poll.return_value = 1
            return process

        connect = mock.Mock(
            side_effect=RuntimeError("Hermit exited before QMP connected (status 1)")
        )
        stderr = io.StringIO()
        with self._launch_patches(config, mock.Mock(side_effect=popen), connect):
            with contextlib.redirect_stderr(stderr):
                with self.assertRaises(RuntimeError) as raised:
                    with dh.program_from_hermit(config):
                        self.fail("the pass must not start")
        (run_dir,) = artifact_dir.iterdir()
        return run_dir, stderr.getvalue(), raised.exception

    def test_a_failed_start_shows_the_end_of_the_hermit_log(self):
        artifact_dir = self.outside_tmp / "artifacts"
        run_dir, stderr, error = self._failed_start(artifact_dir)
        # The error itself is unchanged; the log's end is printed with it.
        self.assertEqual(str(error), "Hermit exited before QMP connected (status 1)")
        lines = stderr.splitlines()
        header = "End of the failed pass's Hermit log, {}:".format(run_dir / "hermit.log")
        self.assertIn(header, lines, stderr)
        shown = dh.FAILED_LOG_TAIL_LINES
        self.assertLess(shown, len(self.LOG_LINES))
        first = lines.index(header) + 1
        self.assertEqual(
            lines[first : first + shown],
            ["  " + line for line in self.LOG_LINES[-shown:]],
            stderr,
        )
        # QEMU's reason, the log's last line, is among them; older lines are not.
        self.assertIn("  " + self.LOG_LINES[-1], lines)
        self.assertNotIn("  " + self.LOG_LINES[-shown - 1], lines)
        # Hermit had exited by itself, so the demo stopped nothing.
        self.assertNotIn("SIGKILL", stderr)

    def test_a_failed_start_removes_the_snapshot_copy(self):
        artifact_dir = self.outside_tmp / "artifacts"
        run_dir, stderr, _ = self._failed_start(artifact_dir)
        self.assertFalse((run_dir / "snapshot.qcow2").exists(), sorted(os.listdir(run_dir)))
        self.assertIn(
            "Removed the failed pass's 4096-byte snapshot copy {}".format(
                run_dir / "snapshot.qcow2"
            ),
            stderr,
        )
        # The log and the rest of the run directory stay for inspection.
        self.assertTrue((run_dir / "hermit.log").exists())

    def test_a_failure_after_the_start_also_removes_the_snapshot_copy(self):
        run_dir = self.outside_tmp / "run.after"
        run_dir.mkdir()
        (run_dir / "snapshot.qcow2").write_bytes(b"q" * 100)
        (run_dir / "hermit.log").write_text("advance timed out\n")

        def start(program):
            program.run_dir = run_dir
            return program

        stderr = io.StringIO()
        with mock.patch.object(dh.HermitGuestProgram, "start", start):
            with contextlib.redirect_stderr(stderr):
                with self.assertRaisesRegex(TimeoutError, "^marker$"):
                    with dh.program_from_hermit(mock.Mock()):
                        raise TimeoutError("marker")
        self.assertFalse((run_dir / "snapshot.qcow2").exists())
        self.assertIn("  advance timed out\n", stderr.getvalue())

    def test_a_pass_that_succeeds_keeps_its_files_and_prints_nothing(self):
        # Positive control: the README promises a passing run's working copy.
        run_dir = self.outside_tmp / "run.ok"
        run_dir.mkdir()
        (run_dir / "snapshot.qcow2").write_bytes(b"q" * 100)
        (run_dir / "hermit.log").write_text("fine\n")

        def start(program):
            program.run_dir = run_dir
            return program

        stderr = io.StringIO()
        with mock.patch.object(dh.HermitGuestProgram, "start", start):
            with contextlib.redirect_stderr(stderr):
                with dh.program_from_hermit(mock.Mock()):
                    pass
        self.assertTrue((run_dir / "snapshot.qcow2").exists())
        self.assertEqual(stderr.getvalue(), "")

    def test_a_pass_that_fails_before_hermit_starts_prints_no_log(self):
        # No run directory yet: nothing to show and nothing to remove.
        config = self._config(self.outside_tmp / "artifacts")
        config.kernel.unlink()
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            with self.assertRaises(FileNotFoundError):
                with dh.program_from_hermit(config):
                    self.fail("the pass must not start")
        self.assertEqual(stderr.getvalue(), "")


class LogTailTest(unittest.TestCase):
    def setUp(self):
        self.directory = _directory(self, "/var/tmp", "drgn-log-tail-")

    def test_only_the_last_lines_are_returned(self):
        log = self.directory / "hermit.log"
        log.write_text("".join("line {}\n".format(number) for number in range(50)))
        self.assertEqual(
            dh._log_tail(log, 3, 1 << 20), ["line 47", "line 48", "line 49"]
        )

    def test_a_line_cut_by_the_byte_limit_is_left_out(self):
        log = self.directory / "hermit.log"
        log.write_text("a" * 100 + "\n" + "b" * 10 + "\n" + "c" * 10 + "\n")
        # The last 30 bytes start inside the run of a's.
        self.assertEqual(dh._log_tail(log, 40, 30), ["b" * 10, "c" * 10])

    def test_a_missing_log_is_none_and_an_empty_log_has_no_lines(self):
        self.assertIsNone(dh._log_tail(self.directory / "absent.log", 40, 1024))
        empty = self.directory / "empty.log"
        empty.write_bytes(b"")
        self.assertEqual(dh._log_tail(empty, 40, 1024), [])


class ArtifactDirectoryDocumentationTest(unittest.TestCase):
    DEMO07_DIR = Path(__file__).resolve().parent.parent / "07-drgn-kernel"

    def test_the_readme_controls_table_documents_the_artifact_directory(self):
        readme = (self.DEMO07_DIR / "README.md").read_text()
        rows = [line for line in readme.splitlines() if line.startswith("| `DEMO07_ARTIFACTS` |")]
        self.assertEqual(len(rows), 1, "no controls-table row for DEMO07_ARTIFACTS")
        self.assertIn("`target/demos/07-drgn-kernel`", rows[0])
        self.assertIn("--tmp=/tmp", rows[0])

    def test_the_usage_lists_the_artifact_directory(self):
        usage = subprocess.run(
            ["bash", str(self.DEMO07_DIR / "run.sh"), "--help"],
            capture_output=True,
            text=True,
            check=True,
            timeout=30,
        ).stdout
        self.assertRegex(usage, r"(?m)^  DEMO07_ARTIFACTS=/path +\S")


# Stands in for a `hermit` wrapper such as safehermit. It starts two `sleep`
# processes in sessions of their own, so outside its process group as a
# systemd unit's processes are, waits for both to exit, then does its own
# cleanup (writes a marker file) and exits 0. Given a program named
# qemu-system-* and the pass's -qmp argument, its first process runs that
# program with that argument instead, as the QEMU that _find_qemu looks for.
_WRAPPER = r"""
import os, subprocess, sys
pid_file, marker = sys.argv[1], sys.argv[2]
sleeper = ["sleep", "1000"]
qemu = sleeper
if len(sys.argv) > 3:
    qemu = [sys.argv[3], "-c", "import time; time.sleep(1000)", "-qmp", sys.argv[4]]
children = [subprocess.Popen(command, start_new_session=True) for command in (qemu, sleeper)]
with open(pid_file + ".partial", "w") as output:
    output.write(" ".join(str(child.pid) for child in children))
os.rename(pid_file + ".partial", pid_file)
for child in children:
    child.wait()
with open(marker, "w") as output:
    output.write("cleanup ran\n")
"""


def _stat_identity(pid: int):
    """(state, start time) from /proc/<pid>/stat, read here independently of the demo's code."""
    try:
        data = Path("/proc/{}/stat".format(pid)).read_bytes()
    except (FileNotFoundError, ProcessLookupError):
        return None
    fields = data[data.rindex(b")") + 1 :].split()
    return fields[0].decode(), int(fields[19])


def _alive(pid: int, start_time: int) -> bool:
    identity = _stat_identity(pid)
    return identity is not None and identity[0] not in ("Z", "X") and identity[1] == start_time


def _wait_for_state(test: unittest.TestCase, pid: int, state: str) -> None:
    deadline = time.monotonic() + 30
    while _stat_identity(pid)[0] != state:
        if time.monotonic() > deadline:
            test.fail("pid {} did not reach state {}: {}".format(pid, state, _stat_identity(pid)))
        time.sleep(0.005)


def _kill_leftover(pid: int, start_time: int) -> None:
    """SIGKILL a stand-in that a test left running, if the pid is still that process."""
    try:
        pidfd = os.pidfd_open(pid)
    except OSError:
        return
    try:
        if _alive(pid, start_time):
            signal.pidfd_send_signal(pidfd, signal.SIGKILL)
    finally:
        os.close(pidfd)


class _PausedQmp:
    def status(self):
        return "paused"

    def execute(self, command):
        raise AssertionError("unexpected QMP command {}".format(command))

    def close(self):
        pass


class LeftoverProcessTest(_PassHarness):
    """A pass, finished or failed, leaves neither QEMU nor Hermit's tracer running.

    start() runs for real except for what needs QEMU: the `hermit` it starts is
    _WRAPPER, whose two stopped `sleep` processes play QEMU and Hermit's tracer
    and are found and frozen where start() finds and freezes the real ones.
    """

    def _run_pass(self, body_error=None, qmp_error=None):
        """Run one pass; with ``qmp_error``, the pass fails with it while waiting for QMP.

        That is before start() looks for QEMU, so the QEMU stand-in is then a
        program named qemu-system-x86_64 that carries the pass's -qmp argument,
        and QEMU's TracerPid names the tracer stand-in.
        """
        config = self._config(self.outside_tmp / "artifacts")
        pid_file = self.outside_tmp / "stand-ins.pids"
        marker = self.outside_tmp / "wrapper-cleanup-ran"
        qemu_program = self.outside_tmp / "qemu-system-x86_64"
        qemu_program.symlink_to(sys.executable)
        real_popen = subprocess.Popen
        real_status_value = dh._proc_status_value
        stand_ins = []  # (pid, start time) of the QEMU and tracer stand-ins

        def popen(command, **options):
            wrapper = [sys.executable, "-c", _WRAPPER, str(pid_file), str(marker)]
            if qmp_error is not None:
                wrapper += [str(qemu_program), command[command.index("-qmp") + 1]]
            return real_popen(wrapper, **options)

        def read_stand_ins():
            deadline = time.monotonic() + 30
            while not pid_file.exists():
                if time.monotonic() > deadline:
                    raise TimeoutError("the stand-ins did not start")
                time.sleep(0.01)
            for pid in (int(word) for word in pid_file.read_text().split()):
                start_time = _stat_identity(pid)[1]
                stand_ins.append((pid, start_time))
                self.addCleanup(_kill_leftover, pid, start_time)

        def wait_for_qemu(process, qmp_socket, timeout):
            read_stand_ins()
            return stand_ins[0][0]

        def connect(path, process, timeout):
            if qmp_error is None:
                return _PausedQmp()
            read_stand_ins()
            # QEMU waits in a ptrace stop, as in a pass that hung under Hermit.
            os.kill(stand_ins[0][0], signal.SIGSTOP)
            _wait_for_state(self, stand_ins[0][0], "T")
            raise qmp_error

        def status_value(pid, key):
            # The tracer stand-in does not really ptrace the QEMU stand-in.
            if stand_ins:
                tracer = stand_ins[1][0]
                if pid == stand_ins[0][0] and key == "TracerPid":
                    return str(tracer)
                if pid == tracer and key == "Tgid":
                    return str(tracer)
            return real_status_value(pid, key)

        def freeze_exact_tracer(qemu_pid, tracer, timeout=20.0):
            # The real QEMU waits in a ptrace stop and the tracer is stopped
            # with SIGSTOP; both stand-ins are stopped here. start() held the
            # tracer stand-in through a real pidfd.
            self.assertEqual(tracer.tgid, stand_ins[1][0])
            for pid, _ in stand_ins:
                os.kill(pid, signal.SIGSTOP)
            for pid, _ in stand_ins:
                _wait_for_state(self, pid, "T")

        build_id = "0123abcd"
        with contextlib.ExitStack() as stack:
            for name, replacement in (
                ("ensure_vmlinux", mock.Mock(return_value=config.vmlinux)),
                ("_open_serial_pipe", mock.Mock(side_effect=lambda *_: os.pipe())),
                ("_wait_for_qemu", wait_for_qemu),
                ("_freeze_exact_tracer", freeze_exact_tracer),
                ("_ram_region", mock.Mock(return_value=(0, 4096))),
                (
                    "_scan_vmcoreinfo",
                    mock.Mock(return_value=(0, "BUILD-ID={}\n".format(build_id).encode())),
                ),
                ("_elf_build_id", mock.Mock(return_value=build_id)),
                ("_proc_status_value", status_value),
            ):
                stack.enter_context(mock.patch.object(dh, name, replacement))
            stack.enter_context(mock.patch.object(dh.subprocess, "Popen", popen))
            stack.enter_context(mock.patch.object(dh.QmpClient, "connect", connect))
            program = dh.HermitGuestProgram(config)
            stack.enter_context(
                mock.patch.object(dh, "HermitGuestProgram", mock.Mock(return_value=program))
            )
            stderr = io.StringIO()
            stack.enter_context(contextlib.redirect_stderr(stderr))
            try:
                with dh.program_from_hermit(config):
                    if body_error is not None:
                        raise body_error
            except (RuntimeError, TimeoutError) as error:
                if error is not body_error and error is not qmp_error:
                    raise
        return program, stand_ins, marker, stderr.getvalue()

    def _assert_nothing_left(self, guest, stand_ins, marker, stderr):
        self.assertEqual(len(stand_ins), 2, stand_ins)
        for name, (pid, start_time) in zip(("QEMU", "Hermit's tracer"), stand_ins):
            self.assertFalse(
                _alive(pid, start_time),
                "the {} stand-in (pid {}) is still running after close(): {}".format(
                    name, pid, _stat_identity(pid)
                ),
            )
        # The wrapper was not killed: it saw both exit, ran its own cleanup and
        # exited by itself.
        self.assertEqual(guest._process.returncode, 0)
        self.assertTrue(marker.exists(), "the wrapper's own cleanup did not run")
        self.assertNotIn("still running", stderr)

    # Hermit logs its tracer's SIGKILL as its own failure (container-child-exit),
    # so a failed pass's report says that the demo sent it.
    STOPPED_NOTE = (
        "The demo stopped QEMU and Hermit's tracer with SIGKILL before reading the log; "
        "a SIGKILL that the log reports is this cleanup, not the pass's failure."
    )

    def test_a_pass_that_finishes_leaves_no_process(self):
        guest, stand_ins, marker, stderr = self._run_pass()
        self._assert_nothing_left(guest, stand_ins, marker, stderr)
        self.assertEqual(stderr, "")

    def test_a_pass_that_fails_leaves_no_process(self):
        error = RuntimeError("guest advanced during read")
        guest, stand_ins, marker, stderr = self._run_pass(body_error=error)
        self._assert_nothing_left(guest, stand_ins, marker, stderr)
        self.assertIn("Removed the failed pass's", stderr)
        self.assertIn(self.STOPPED_NOTE, stderr)

    def test_a_pass_that_fails_before_qemu_is_found_leaves_no_process(self):
        # As in a pass whose QEMU hung under Hermit before it created its QMP
        # socket: start() had not looked for QEMU yet.
        error = TimeoutError("QMP socket did not become ready: qmp.sock (timed out)")
        guest, stand_ins, marker, stderr = self._run_pass(qmp_error=error)
        self.assertIsNone(guest._qemu_pid)
        self._assert_nothing_left(guest, stand_ins, marker, stderr)
        self.assertIn("Removed the failed pass's", stderr)
        self.assertIn(self.STOPPED_NOTE, stderr)


# A pid that no process has: above the largest pid_max Linux allows (2**22).
ABSENT_QEMU_PID = 2 ** 22 + 1


class TracerSignalTest(unittest.TestCase):
    """Hermit's tracer, which the demo finds but does not start, is signalled only through a pidfd.

    A `sleep` this test starts plays the tracer. QEMU's /proc status is faked:
    it names the `sleep` as QEMU's tracer and shows QEMU in a trace-stop.
    Signals sent by pid or process group are recorded and not sent.
    """

    def setUp(self):
        self.tracer = subprocess.Popen(["sleep", "1000"])

        def reap():
            self.tracer.kill()
            self.tracer.wait(timeout=30)

        self.addCleanup(reap)
        real_status_value = dh._proc_status_value
        real_send = signal.pidfd_send_signal
        self.sent = []  # signals sent through a pidfd
        self.by_pid = []  # signals asked for by pid or process group

        def status_value(pid, key):
            if pid == ABSENT_QEMU_PID:
                return {"TracerPid": str(self.tracer.pid), "State": "t"}[key]
            return real_status_value(pid, key)

        def pidfd_send_signal(pidfd, sig, *rest):
            self.sent.append(sig)
            return real_send(pidfd, sig, *rest)

        for target, name, replacement in (
            (dh, "_proc_status_value", status_value),
            (dh.signal, "pidfd_send_signal", pidfd_send_signal),
            (dh.os, "kill", lambda *call: self.by_pid.append(("kill",) + call)),
            (dh.os, "killpg", lambda *call: self.by_pid.append(("killpg",) + call)),
        ):
            patcher = mock.patch.object(target, name, replacement)
            patcher.start()
            self.addCleanup(patcher.stop)

    def _hold(self):
        tracer = dh._hold_exact_tracer(ABSENT_QEMU_PID)
        self.addCleanup(tracer.close)
        self.assertEqual(tracer.tgid, self.tracer.pid)
        self.assertEqual(tracer.start_time, _stat_identity(self.tracer.pid)[1])
        return tracer

    def test_the_freeze_and_the_advance_signal_the_tracer_only_through_its_pidfd(self):
        tracer = self._hold()
        dh._freeze_exact_tracer(ABSENT_QEMU_PID, tracer)
        self.assertEqual(_stat_identity(self.tracer.pid)[0], "T")
        program = dh.HermitGuestProgram(SimpleNamespace(advance_command="echo deterministic"))
        program._frozen = True
        program._qmp = mock.Mock()
        program._qmp.status.return_value = "paused"
        program._qemu_pid = ABSENT_QEMU_PID
        program._tracer = tracer
        program._tracer_tgid = tracer.tgid
        program._wait_for_serial = mock.Mock()
        program.advance("echo deterministic", b"done")
        self.assertEqual(self.by_pid, [], "a signal was sent to the tracer by pid")
        self.assertEqual(self.sent[0], signal.SIGSTOP)
        self.assertIn(signal.SIGCONT, self.sent)
        self.assertEqual(self.sent[-1], signal.SIGSTOP)
        self.assertLessEqual(set(self.sent), {signal.SIGSTOP, signal.SIGCONT})
        self.assertEqual(_stat_identity(self.tracer.pid)[0], "T")

    def test_a_tracer_pid_given_to_another_process_meanwhile_is_not_held(self):
        real_identity = dh._proc_identity
        reads = []

        def identity(pid):
            found = real_identity(pid)
            reads.append(pid)
            if len(reads) == 1:
                # Read before the pidfd is opened: an earlier process had the pid.
                return found[0], found[1] - 1
            return found

        descriptors = len(os.listdir("/proc/self/fd"))
        with mock.patch.object(dh, "_proc_identity", identity):
            with self.assertRaisesRegex(RuntimeError, "changed while it was being held"):
                dh._hold_exact_tracer(ABSENT_QEMU_PID)
        self.assertEqual(len(os.listdir("/proc/self/fd")), descriptors, "the pidfd was left open")
        self.assertEqual(self.sent, [])
        self.assertEqual(self.by_pid, [])

    def test_a_pidfd_that_cannot_be_opened_is_reported_and_nothing_is_sent_by_pid(self):
        with mock.patch.object(
            dh.os, "pidfd_open", side_effect=OSError(errno.EMFILE, "Too many open files")
        ):
            with self.assertRaisesRegex(
                RuntimeError,
                r"could not open a pidfd for Hermit's tracer \(pid {}\): .*Too many open files.*"
                r"not signalled by pid".format(self.tracer.pid),
            ):
                dh._hold_exact_tracer(ABSENT_QEMU_PID)
        self.assertEqual(self.sent, [])
        self.assertEqual(self.by_pid, [])

    def test_a_pidfd_signal_error_is_reported_and_nothing_is_sent_by_pid(self):
        tracer = self._hold()
        with mock.patch.object(
            dh.signal,
            "pidfd_send_signal",
            side_effect=PermissionError(errno.EPERM, "Operation not permitted"),
        ):
            with self.assertRaisesRegex(
                RuntimeError,
                r"could not send SIGSTOP to Hermit's tracer \(pid {}\) through its pidfd: "
                r".*Operation not permitted.*no signal was sent by pid".format(self.tracer.pid),
            ):
                dh._freeze_exact_tracer(ABSENT_QEMU_PID, tracer)
        self.assertEqual(self.by_pid, [])
        self.assertNotEqual(_stat_identity(self.tracer.pid)[0], "T")

    def test_a_tracer_that_has_exited_is_not_signalled_by_pid(self):
        # ESRCH through the pidfd: the tracer has exited, so the freeze cannot
        # happen, and nothing is sent to a pid that may be another process's.
        tracer = self._hold()
        with mock.patch.object(
            dh.signal, "pidfd_send_signal", side_effect=ProcessLookupError(errno.ESRCH, "No such process")
        ):
            with self.assertRaisesRegex(
                RuntimeError, r"Hermit's tracer \(pid {}\) exited".format(self.tracer.pid)
            ):
                dh._freeze_exact_tracer(ABSENT_QEMU_PID, tracer)
        self.assertEqual(self.by_pid, [])

    def test_close_kills_the_held_tracer_through_the_same_pidfd(self):
        tracer = self._hold()
        program = dh.HermitGuestProgram(SimpleNamespace())
        program._tracer = tracer
        program._tracer_tgid = tracer.tgid
        program._owned_processes = [("Hermit's tracer", tracer.tgid, tracer.start_time)]
        held = tracer.pidfd
        opened = []
        real_open = os.pidfd_open

        def pidfd_open(pid, *rest):
            opened.append(pid)
            return real_open(pid, *rest)

        with mock.patch.object(dh.os, "pidfd_open", pidfd_open), mock.patch.object(
            dh, "OWNED_PROCESS_EXIT_SECONDS", 10.0
        ):
            program.close()
        self.assertEqual(opened, [], "close() opened a second pidfd for the held tracer")
        self.assertEqual(self.sent, [signal.SIGKILL])
        self.assertEqual(self.by_pid, [])
        self.assertIsNone(tracer.pidfd, "close() did not release the tracer's pidfd")
        self.assertEqual(self.tracer.wait(timeout=30), -signal.SIGKILL)
        self.assertIsNotNone(held)

    def test_the_module_sends_no_signal_by_pid(self):
        # Only the process group of the Hermit the demo started is signalled
        # without a pidfd; that group's leader is the demo's own unreaped child.
        source = Path(dh.__file__).read_text()
        self.assertNotIn("os.kill(", source)
        self.assertEqual(re.findall(r"os\.killpg\(([^,]*),", source), ["self._process_group"])


class OwnedProcessKillTest(unittest.TestCase):
    def _stopped_sleep(self, executable: str = "sleep"):
        child = subprocess.Popen([executable, "1000"])

        def reap():
            child.kill()
            child.wait(timeout=30)

        self.addCleanup(reap)
        os.kill(child.pid, signal.SIGSTOP)
        _wait_for_state(self, child.pid, "T")
        return child, _stat_identity(child.pid)[1]

    @contextlib.contextmanager
    def _signals_by_pid(self):
        """Record, without sending them, the signals sent by pid or by process group."""
        sent = []
        with mock.patch.object(
            dh.os, "kill", side_effect=lambda *call: sent.append(("kill",) + call)
        ), mock.patch.object(
            dh.os, "killpg", side_effect=lambda *call: sent.append(("killpg",) + call)
        ):
            yield sent

    @staticmethod
    def _not_signalled_line(name: str, pid: int, reason: str) -> str:
        return (
            "Could not stop {} (pid {}): {}. It was sent no signal: a signal sent by pid "
            "alone could reach another process given that pid.\n".format(name, pid, reason)
        )

    def test_the_recorded_process_is_killed_and_its_exit_confirmed(self):
        child, start_time = self._stopped_sleep()
        # Nothing still running, and nothing that could not be signalled.
        self.assertEqual(dh._kill_and_wait([("QEMU", child.pid, start_time)], 10), ([], []))
        # It has exited; its parent, this test, has not reaped it yet.
        self.assertEqual(_stat_identity(child.pid)[0], "Z")
        self.assertEqual(child.wait(timeout=10), -signal.SIGKILL)

    def test_a_pid_now_held_by_another_process_is_not_signalled(self):
        # As if the recorded process had exited and a process that started
        # later had been given its pid.
        child, start_time = self._stopped_sleep()
        program = dh.HermitGuestProgram(mock.Mock())
        program._owned_processes = [("QEMU", child.pid, start_time - 1)]
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            program.close()
        self.assertEqual(_stat_identity(child.pid), ("T", start_time))
        self.assertEqual(stderr.getvalue(), "")

    def test_a_process_that_does_not_exit_is_reported_within_the_bound(self):
        program = dh.HermitGuestProgram(mock.Mock())
        program._owned_processes = [("QEMU", 4242, 17)]
        stderr = io.StringIO()
        with mock.patch.object(dh, "_kill_if_running") as kill, mock.patch.object(
            dh, "_is_running", return_value=True
        ), mock.patch.object(dh, "OWNED_PROCESS_EXIT_SECONDS", 0.2), contextlib.redirect_stderr(
            stderr
        ):
            started = time.monotonic()
            with self.assertRaisesRegex(
                RuntimeError, r"^could not stop QEMU \(pid 4242\) after the pass$"
            ):
                program.close()
            elapsed = time.monotonic() - started
        kill.assert_called_once_with(4242, 17)
        self.assertGreaterEqual(elapsed, 0.2)
        self.assertLess(elapsed, 5)
        self.assertIn("QEMU (pid 4242) is still running 0.2 s after SIGKILL.", stderr.getvalue())

    def test_a_failed_pass_reports_a_survivor_without_replacing_its_error(self):
        program = dh.HermitGuestProgram(mock.Mock())
        program._owned_processes = [("Hermit's tracer", 4242, 17)]
        stderr = io.StringIO()
        with mock.patch.object(dh, "_kill_if_running"), mock.patch.object(
            dh, "_is_running", return_value=True
        ), mock.patch.object(dh, "OWNED_PROCESS_EXIT_SECONDS", 0.2), contextlib.redirect_stderr(
            stderr
        ):
            program.close(failed=True)
        self.assertIn(
            "Hermit's tracer (pid 4242) is still running 0.2 s after SIGKILL.", stderr.getvalue()
        )

    def test_no_signal_is_sent_by_pid_when_a_pidfd_cannot_be_opened(self):
        # QEMU and Hermit's tracer are not this process's children, so their
        # pids can be reused at any moment; only a pidfd names the process
        # that a signal reaches.
        for error_number in (errno.EPERM, errno.ENOSYS):
            with self.subTest(error=errno.errorcode[error_number]):
                child, start_time = self._stopped_sleep()
                program = dh.HermitGuestProgram(mock.Mock())
                program._owned_processes = [("QEMU", child.pid, start_time)]
                error = OSError(error_number, os.strerror(error_number))
                stderr = io.StringIO()
                with self._signals_by_pid() as sent, mock.patch.object(
                    dh.os, "pidfd_open", side_effect=error
                ), contextlib.redirect_stderr(stderr):
                    started = time.monotonic()
                    with self.assertRaisesRegex(
                        RuntimeError,
                        r"^could not stop QEMU \(pid {}\) after the pass$".format(child.pid),
                    ):
                        program.close()
                    elapsed = time.monotonic() - started
                self.assertEqual(sent, [])
                # Still the same stopped process: it was sent nothing.
                self.assertEqual(_stat_identity(child.pid), ("T", start_time))
                self.assertEqual(
                    stderr.getvalue(),
                    self._not_signalled_line(
                        "QEMU", child.pid, "pidfd_open failed: {}".format(error)
                    ),
                )
                self.assertEqual(program._owned_processes, [("QEMU", child.pid, start_time)])
                # The wait for signalled processes to exit is not spent on it.
                self.assertLess(elapsed, dh.OWNED_PROCESS_EXIT_SECONDS)

    def test_no_signal_is_sent_by_pid_when_the_pidfd_refuses_the_signal(self):
        child, start_time = self._stopped_sleep()
        program = dh.HermitGuestProgram(mock.Mock())
        program._owned_processes = [("Hermit's tracer", child.pid, start_time)]
        error = PermissionError(errno.EPERM, os.strerror(errno.EPERM))
        stderr = io.StringIO()
        with self._signals_by_pid() as sent, mock.patch.object(
            dh.signal, "pidfd_send_signal", side_effect=error
        ), contextlib.redirect_stderr(stderr):
            with self.assertRaisesRegex(
                RuntimeError,
                r"^could not stop Hermit's tracer \(pid {}\) after the pass$".format(child.pid),
            ):
                program.close()
        self.assertEqual(sent, [])
        self.assertEqual(_stat_identity(child.pid), ("T", start_time))
        self.assertEqual(
            stderr.getvalue(),
            self._not_signalled_line(
                "Hermit's tracer", child.pid, "pidfd_send_signal failed: {}".format(error)
            ),
        )

    def test_a_failed_pass_names_only_the_processes_it_signalled(self):
        tracer, tracer_start = self._stopped_sleep()
        qemu, qemu_start = self._stopped_sleep()
        program = dh.HermitGuestProgram(mock.Mock())
        program._owned_processes = [
            ("QEMU", qemu.pid, qemu_start),
            ("Hermit's tracer", tracer.pid, tracer_start),
        ]
        error = OSError(errno.ENOSYS, os.strerror(errno.ENOSYS))
        real_pidfd_open = os.pidfd_open

        def pidfd_open(pid, *arguments):
            if pid == qemu.pid:
                raise error
            return real_pidfd_open(pid, *arguments)

        stderr = io.StringIO()
        with self._signals_by_pid() as sent, mock.patch.object(
            dh.os, "pidfd_open", pidfd_open
        ), mock.patch.object(program, "_report_failed_pass") as report, contextlib.redirect_stderr(
            stderr
        ):
            program.close(failed=True)
        self.assertEqual(sent, [])
        self.assertEqual(_stat_identity(qemu.pid), ("T", qemu_start))
        # The tracer was killed through its pidfd; this test has not reaped it.
        self.assertEqual(_stat_identity(tracer.pid)[0], "Z")
        self.assertEqual(tracer.wait(timeout=10), -signal.SIGKILL)
        # Only the tracer may be named as stopped with SIGKILL by the demo.
        report.assert_called_once_with(["Hermit's tracer"])
        self.assertEqual(
            stderr.getvalue(),
            self._not_signalled_line("QEMU", qemu.pid, "pidfd_open failed: {}".format(error)),
        )

    def test_a_process_that_has_exited_needs_no_pidfd(self):
        # Not a failure even where pidfds are unavailable: there is nothing to
        # signal.
        child, start_time = self._stopped_sleep()
        child.kill()
        child.wait(timeout=30)
        program = dh.HermitGuestProgram(mock.Mock())
        program._owned_processes = [("QEMU", child.pid, start_time)]
        stderr = io.StringIO()
        with self._signals_by_pid() as sent, mock.patch.object(
            dh.os, "pidfd_open", side_effect=OSError(errno.ENOSYS, os.strerror(errno.ENOSYS))
        ), contextlib.redirect_stderr(stderr):
            program.close()
        self.assertEqual(sent, [])
        self.assertEqual(stderr.getvalue(), "")
        self.assertEqual(program._owned_processes, [])

    def test_the_state_is_read_after_the_last_parenthesis(self):
        # A command name may contain ") S (", which a parse from the first ")"
        # would take for the state of this stopped process.
        directory = _directory(self, "/var/tmp", "drgn-comm-")
        link = directory / "d7) S (x"
        link.symlink_to(shutil.which("sleep"))
        child, _ = self._stopped_sleep(str(link))
        self.assertEqual(Path("/proc/{}/comm".format(child.pid)).read_text(), "d7) S (x\n")
        state, start_time = dh._proc_identity(child.pid)
        self.assertEqual(state, "T")
        self.assertEqual(start_time, _stat_identity(child.pid)[1])

    def test_a_tracer_is_returned_only_while_the_pid_is_the_recorded_process(self):
        child, start_time = self._stopped_sleep()
        values = {(child.pid, "TracerPid"): "777", (777, "Tgid"): "770"}
        with mock.patch.object(dh, "_proc_status_value", lambda pid, key: values[(pid, key)]):
            self.assertEqual(dh._tracer_of(child.pid, start_time), 770)
            # A later process given this pid: its tracer is not Hermit's.
            self.assertIsNone(dh._tracer_of(child.pid, start_time - 1))
            values[(child.pid, "TracerPid")] = "0"
            self.assertIsNone(dh._tracer_of(child.pid, start_time))
        # Read for real: a process stopped by SIGSTOP has no tracer.
        self.assertIsNone(dh._tracer_of(child.pid, start_time))
        gone = subprocess.Popen(["true"])
        gone.wait(timeout=30)
        self.assertIsNone(dh._tracer_of(gone.pid, 0))


if __name__ == "__main__":
    unittest.main()
