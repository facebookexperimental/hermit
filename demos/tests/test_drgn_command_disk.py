#!/usr/bin/env python3
"""Tests for demo 7's command-disk resume, QMP socket path, process cleanup,
and RESULT line."""

import contextlib
import importlib.util
import io
import os
import re
import socket
import sys
import tempfile
import types
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest import mock

LIB_DIR = Path(__file__).resolve().parent.parent / "lib"
DEMO07_DIR = Path(__file__).resolve().parent.parent / "07-drgn-kernel"
sys.path.insert(0, str(LIB_DIR))

import demo_common as dc  # noqa: E402
import drgn_hermit as dh  # noqa: E402


class FakeQmp:
    def __init__(self):
        self.commands = []

    def execute(self, command):
        self.commands.append(command)

    def status(self):
        return "paused"


class CommandDiskProtocolTest(unittest.TestCase):
    def test_command_image_is_fixed_size_and_contains_the_advance(self):
        with tempfile.TemporaryDirectory(dir="/tmp") as directory:
            image = Path(directory) / "guest-command.img"
            dh._write_command_image(image, "echo deterministic")
            payload = image.read_bytes()

        prefix = b"echo deterministic\n"
        self.assertEqual(len(payload), dh.COMMAND_IMAGE_BYTES)
        self.assertTrue(payload.startswith(prefix))
        self.assertEqual(
            payload[len(prefix) :], b"\0" * (dh.COMMAND_IMAGE_BYTES - len(prefix))
        )

    def test_advance_resumes_preloaded_disk_without_serial_injection(self):
        qmp = FakeQmp()
        program = dh.HermitGuestProgram(
            SimpleNamespace(advance_command="echo deterministic")
        )
        program._frozen = True
        program._qmp = qmp
        program._qemu_pid = 456
        program._tracer_tgid = 123
        program._tracer = mock.Mock(tgid=123)
        program._tracer.send.return_value = True
        program._serial_write_fd = 99
        program._wait_for_serial = mock.Mock()

        with mock.patch.object(dh.os, "write") as serial_write, mock.patch.object(
            dh.os, "kill"
        ) as kill, mock.patch.object(dh, "_freeze_exact_tracer") as freeze:
            program.advance("echo deterministic", b"done")

        serial_write.assert_not_called()
        program._wait_for_serial.assert_called_once_with(b"done")
        self.assertEqual(qmp.commands, ["cont", "stop"])
        self.assertTrue(program._frozen)
        # The tracer is resumed through the pidfd it is held by, never by pid,
        # and the same held tracer is frozen again.
        program._tracer.send.assert_called_once_with(dh.signal.SIGCONT)
        kill.assert_not_called()
        freeze.assert_called_once_with(456, program._tracer)
        self.assertEqual(program._tracer_tgid, 123)

    def test_advance_rejects_command_other_than_preloaded_disk(self):
        program = dh.HermitGuestProgram(SimpleNamespace(advance_command="expected"))
        program._frozen = True
        program._qmp = FakeQmp()
        program._qemu_pid = 456
        program._tracer_tgid = 123
        program._tracer = mock.Mock(tgid=123)

        with self.assertRaisesRegex(ValueError, "preloaded command disk"):
            program.advance("different", b"done")

    def test_close_stops_a_running_hermit_process_group(self):
        program = dh.HermitGuestProgram(SimpleNamespace())
        program._process_group = 456
        program._process = mock.Mock()
        program._process.poll.return_value = None

        with mock.patch.object(dh.os, "killpg") as killpg:
            program.close()

        killpg.assert_called_once_with(456, dh.signal.SIGKILL)
        program._process.wait.assert_called_once_with(timeout=10)

    def test_close_does_not_signal_a_process_that_already_exited(self):
        program = dh.HermitGuestProgram(SimpleNamespace())
        program._process_group = 456
        program._process = mock.Mock(returncode=0)
        program._process.poll.return_value = 0

        with mock.patch.object(dh.os, "killpg") as killpg:
            program.close()

        killpg.assert_not_called()
        program._process.wait.assert_called_once_with(timeout=10)


class _StopAtQmpConnect(Exception):
    """Raised in place of the QMP connection, after QEMU's command line is built."""


def _bind_socket_file(path: Path) -> None:
    """Leave a Unix-domain socket file at ``path``, as QEMU's listener would."""
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as listener:
        listener.bind(str(path))


class QmpSocketPathTest(unittest.TestCase):
    """The QMP socket path demo 7 hands QEMU must fit what AF_UNIX allows.

    QEMU, running under Hermit, creates the socket and drgn_hermit.py connects
    to it from outside Hermit. By default the socket is in the run directory
    under the checkout, which from a deep enough checkout is a path the kernel
    refuses, so QEMU could not create it and the demo failed before QMP
    connected. start() is run here with Hermit's launch and the QMP connection
    replaced, which is enough to see the command line QEMU would get.
    """

    def setUp(self):
        work = tempfile.TemporaryDirectory(dir="/tmp")
        self.addCleanup(work.cleanup)
        self.work = Path(work.name)
        # Relocated sockets go under QEMU_SOCKET_DIR. Point it at a private
        # directory, outside host /tmp as make_socket_path requires, so the
        # test leaves nothing in the shared /var/tmp/hermit-qmp-<uid>.
        socket_dir = tempfile.TemporaryDirectory(dir="/var/tmp", prefix="drgn-qmp-test-")
        self.addCleanup(socket_dir.cleanup)
        self.socket_dir = Path(socket_dir.name)
        environment = mock.patch.dict(os.environ, {"QEMU_SOCKET_DIR": str(self.socket_dir)})
        environment.start()
        self.addCleanup(environment.stop)

    def _config(self, artifact_dir: Path) -> dh.GuestConfig:
        inputs = self.work / "inputs"
        inputs.mkdir(exist_ok=True)
        for name in ("hermit", "qemu", "bzImage", "initramfs.cpio.gz", "hermit-boot.qcow2"):
            (inputs / name).write_bytes(b"")
        # The record demo 5 writes, so that start() restores the snapshot: a
        # stand-in qemu-assets.sh in the checkout names the initramfs version.
        script = self.work / "demos/lib/qemu-assets.sh"
        script.parent.mkdir(parents=True, exist_ok=True)
        script.write_text("INITRAMFS_VERSION=9\n")
        snapshot = inputs / "hermit-boot.qcow2"
        dc.write_boot_snapshot_record(
            snapshot, dc.hash_file(snapshot), dc.initramfs_producer(self.work, inputs)
        )
        return dh.GuestConfig(
            root=self.work,
            hermit=inputs / "hermit",
            qemu=inputs / "qemu",
            kernel=inputs / "bzImage",
            initrd=inputs / "initramfs.cpio.gz",
            vmlinux=inputs / "vmlinux",
            snapshot_disk=snapshot,
            snapshot_name="hermit-boot",
            advance_command="echo deterministic",
            artifact_dir=artifact_dir,
            assets=inputs,
        )

    def _start_until_qmp_connect(self, artifact_dir: Path):
        """Run start() until it connects to QMP; return what QEMU and QMP got."""
        config = self._config(artifact_dir)
        program = dh.HermitGuestProgram(config)
        popen = mock.Mock(return_value=mock.Mock(pid=4242))
        connect = mock.Mock(side_effect=_StopAtQmpConnect)
        with mock.patch.object(
            dh, "ensure_vmlinux", return_value=config.vmlinux
        ), mock.patch.object(dh.subprocess, "Popen", popen), mock.patch.object(
            dh.os, "getpgid", return_value=4242
        ), mock.patch.object(
            dh.QmpClient, "connect", connect
        ):
            with self.assertRaises(_StopAtQmpConnect):
                program.start()
        argv = popen.call_args.args[0]
        self.assertEqual(argv[0], str(config.hermit))
        separator = argv.index("--")
        qmp_index = argv.index("-qmp")
        # The -qmp option is in QEMU's command line, after Hermit's own options.
        self.assertGreater(qmp_index, separator)
        qmp_argument = argv[qmp_index + 1]
        self.assertTrue(qmp_argument.startswith("unix:"), qmp_argument)
        self.assertTrue(qmp_argument.endswith(",server=on,wait=off"), qmp_argument)
        qemu_socket = Path(qmp_argument[len("unix:") : -len(",server=on,wait=off")])
        connect_socket = connect.call_args.args[0]
        return program, qemu_socket, connect_socket

    def test_a_deep_checkout_hands_qemu_a_socket_path_that_fits(self):
        artifact_dir = self.work / ("a" * 100) / "artifacts"
        program, qemu_socket, connect_socket = self._start_until_qmp_connect(artifact_dir)

        default = program.run_dir / "qmp.sock"
        default_bytes = len(str(default).encode())
        self.assertGreater(default_bytes, dc.AF_UNIX_PATH_MAX)
        # This is the failure QEMU hit with the old path: no socket can be
        # created there.
        with self.assertRaises(OSError):
            _bind_socket_file(default)

        self.assertLessEqual(
            len(str(qemu_socket).encode()),
            dc.AF_UNIX_PATH_MAX,
            "the socket path handed to QEMU is {} bytes: {}".format(
                len(str(qemu_socket).encode()), qemu_socket
            ),
        )
        self.assertEqual(connect_socket, qemu_socket)
        self.assertEqual(program.qmp_socket, qemu_socket)
        self.assertEqual(
            qemu_socket.parent, self.socket_dir / "hermit-qmp-{}".format(os.getuid())
        )
        self.assertTrue(qemu_socket.name.startswith("drgn-"), qemu_socket)
        # Hermit gives the guest a private /tmp, so a socket under host /tmp
        # would not be the one this process connects to.
        self.assertFalse(dc._under_host_tmp(qemu_socket.parent), qemu_socket)

        # The kernel accepts the path, and close() removes the socket from the
        # shared relocation directory once the run is over.
        _bind_socket_file(qemu_socket)
        self.assertTrue(qemu_socket.exists())
        program.close()
        self.assertFalse(qemu_socket.exists())

    def test_a_short_checkout_keeps_the_socket_in_the_run_directory(self):
        artifact_dir = self.work / "artifacts"
        program, qemu_socket, connect_socket = self._start_until_qmp_connect(artifact_dir)

        self.assertEqual(qemu_socket, program.run_dir / "qmp.sock")
        self.assertLessEqual(len(str(qemu_socket).encode()), dc.AF_UNIX_PATH_MAX)
        self.assertEqual(connect_socket, qemu_socket)
        self.assertFalse((self.socket_dir / "hermit-qmp-{}".format(os.getuid())).exists())

    def test_close_removes_only_a_relocated_socket(self):
        run_dir = self.work / "run.example"
        run_dir.mkdir()
        relocation_dir = self.socket_dir / "hermit-qmp-{}".format(os.getuid())
        relocation_dir.mkdir()
        relocated = relocation_dir / "drgn-0123456789abcdef.sock"
        in_run_dir = run_dir / "qmp.sock"
        _bind_socket_file(relocated)
        _bind_socket_file(in_run_dir)

        relocated_program = dh.HermitGuestProgram(SimpleNamespace())
        relocated_program.run_dir = run_dir
        relocated_program.qmp_socket = relocated
        relocated_program.close()
        self.assertFalse(relocated.exists())
        # A run that failed before QEMU created its socket closes cleanly.
        relocated_program.close()

        # A socket in the run directory stays with the run's other files, as
        # it always has.
        kept_program = dh.HermitGuestProgram(SimpleNamespace())
        kept_program.run_dir = run_dir
        kept_program.qmp_socket = in_run_dir
        kept_program.close()
        self.assertTrue(in_run_dir.exists())


def _load_task_evolution():
    """Load demo 7's drgn script with drgn itself replaced by empty modules.

    The script imports a drgn helper at the top, and drgn need not be installed
    to check what the script prints. None of the tests below reads guest memory.
    """
    names = ("drgn", "drgn.helpers", "drgn.helpers.linux", "drgn.helpers.linux.list")
    stubs = {name: types.ModuleType(name) for name in names}
    stubs["drgn.helpers.linux.list"].list_for_each_entry = mock.Mock(
        side_effect=AssertionError("guest memory is not read in these tests")
    )
    spec = importlib.util.spec_from_file_location(
        "demo07_task_evolution", str(DEMO07_DIR / "task_evolution.py")
    )
    module = importlib.util.module_from_spec(spec)
    # The script prepends demos/lib to sys.path when it is loaded; keep that
    # from leaking into the rest of the test process.
    with mock.patch.dict(sys.modules, stubs):
        with mock.patch.object(sys, "path", list(sys.path)):
            spec.loader.exec_module(module)
    return module


class TaskEvolutionResultTest(unittest.TestCase):
    """Demo 7's RESULT line names the sleep its command requests.

    The command asks the guest for ``usleep 1000``; the guest then runs the rest
    of the command and keeps running until the marker is seen and QEMU is
    paused. Nothing measures how far guest time moved, so the RESULT field must
    report the requested sleep, not a fixed advance.
    """

    def setUp(self):
        self.module = _load_task_evolution()

    def _requested_usleep(self) -> str:
        command = self.module.DEFAULT_ADVANCE_COMMAND
        match = re.search(r"(?:^|; )usleep (\d+);", command)
        self.assertIsNotNone(match, command)
        return match.group(1)

    def _result_line(self) -> str:
        """Run main() over two identical passes and return its RESULT line."""
        before = [(0, "swapper/0"), (1, "init"), (95, "sleep")]
        after = [(0, "swapper/0"), (1, "sh"), (101, "sleep"), (102, "sleep")]
        removed, added = self.module._task_diff(before, after)
        metrics = dh.ObservationMetrics(
            physical_reads=1,
            physical_bytes=4096,
            qemu_state="t",
            tracer_state="T",
            serial_bytes_delta=0,
        )
        passes = (before, after, removed, added, metrics, metrics)
        environment = {"DEMO07_RUNS": "2", "DEMO07_TASK_LIMIT": "16"}
        output = io.StringIO()
        with contextlib.ExitStack() as stack:
            stack.enter_context(
                mock.patch.object(self.module, "_config", return_value=object())
            )
            stack.enter_context(
                mock.patch.object(self.module, "_run_once", return_value=passes)
            )
            # The boot snapshot check is tested in test_boot_snapshot_record.
            ensure = stack.enter_context(
                mock.patch.object(self.module, "_ensure_boot_snapshot")
            )
            stack.enter_context(mock.patch.dict(os.environ, environment))
            stack.enter_context(contextlib.redirect_stdout(output))
            self.assertEqual(self.module.main(), 0)
        ensure.assert_called_once_with()
        lines = output.getvalue().splitlines()
        results = [line for line in lines if line.startswith("RESULT: ")]
        self.assertEqual(len(results), 1, output.getvalue())
        return results[0]

    def test_result_reports_the_requested_sleep(self):
        line = self._result_line()
        fields = dict(
            item.split("=", 1)
            for item in line[len("RESULT: ") :].split("; ")
            if "=" in item
        )
        self.assertNotIn("fixed_virtual_advance_us", fields, line)
        requested = fields.get("requested_sleep_us")
        self.assertEqual(requested, self._requested_usleep(), line)

    def test_the_guest_command_is_unchanged(self):
        # The command is now built from REQUESTED_SLEEP_US; the bytes the guest
        # reads from its command disk must be the same as before.
        self.assertEqual(
            self.module.DEFAULT_ADVANCE_COMMAND,
            "for n in 1 2; do sleep 1000 & done; "
            'usleep 1000; echo __HERMIT_DEMO07_ADVANCE_"DONE__"',
        )

    def test_the_readme_shows_the_result_line_the_script_prints(self):
        readme = (DEMO07_DIR / "README.md").read_text()
        self.assertIn("\n" + self._result_line() + "\n", readme)


class TaskListDisplayTest(unittest.TestCase):
    """Demo 7 prints only the first rows of each task list.

    The comparison uses every row, so a row that is not printed can be in the
    task-list diff. The line that counts the rows left out must not call them
    unchanged.
    """

    def setUp(self):
        self.module = _load_task_evolution()

    def _printed(self, label, rows, limit):
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            self.module._print_tasks(label, rows, limit)
        return output.getvalue().splitlines()

    def test_rows_left_out_are_counted_but_not_called_unchanged(self):
        before = [(0, "swapper/0"), (1, "init"), (95, "sleep")]
        after = [(0, "swapper/0"), (1, "sh"), (101, "sleep"), (102, "sleep")]
        removed, added = self.module._task_diff(before, after)
        # Every row after the first, which is the only one shown, is in the
        # difference.
        self.assertEqual(sorted(removed + added), sorted(before[1:] + after[1:]))
        for label, rows, omitted in (("before", before, 2), ("after", after, 3)):
            with self.subTest(label=label):
                lines = self._printed(label, rows, 1)
                self.assertEqual(
                    lines,
                    [
                        "{} tasks ({} total; first 1 shown, pid comm):".format(
                            label, len(rows)
                        ),
                        "      0 swapper/0",
                        "  ... {} rows omitted from display".format(omitted),
                    ],
                )
                self.assertNotIn("unchanged", "\n".join(lines))

    def test_a_list_within_the_limit_has_no_omission_line(self):
        # Positive control: nothing is left out, so nothing is counted.
        rows = [(0, "swapper/0"), (1, "init")]
        self.assertEqual(
            self._printed("before", rows, 16),
            [
                "before tasks (2 total; first 2 shown, pid comm):",
                "      0 swapper/0",
                "      1 init",
            ],
        )

    def test_the_readme_shows_the_omission_lines_the_script_prints(self):
        readme = (DEMO07_DIR / "README.md").read_text()
        headers = re.findall(
            r"^(before|after) tasks \((\d+) total; first (\d+) shown, pid comm\):$",
            readme,
            re.MULTILINE,
        )
        self.assertEqual([label for label, _, _ in headers], ["before", "after"])
        expected = []
        for label, total, shown in headers:
            rows = [(pid, "task") for pid in range(int(total))]
            expected.append(self._printed(label, rows, int(shown))[-1])
        in_readme = re.findall(
            r"^  \.\.\. .*omitted from display$", readme, re.MULTILINE
        )
        self.assertEqual(in_readme, expected)


if __name__ == "__main__":
    unittest.main()
