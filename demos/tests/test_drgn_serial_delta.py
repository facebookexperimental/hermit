#!/usr/bin/env python3
"""Tests for demo 7's count of serial bytes that arrive while a drgn read runs."""

import contextlib
import importlib.util
import io
import os
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

import drgn_hermit as dh  # noqa: E402

QEMU_PID = 456
TRACER_TGID = 123


def _stopped_state(pid):
    """Report QEMU in a ptrace stop and Hermit's tracer stopped."""
    return {QEMU_PID: "t", TRACER_TGID: "T"}[pid]


def _metrics(serial_bytes_delta):
    return dh.ObservationMetrics(
        physical_reads=1,
        physical_bytes=4096,
        qemu_state="t",
        tracer_state="T",
        serial_bytes_delta=serial_bytes_delta,
    )


def _load_task_evolution():
    """Load demo 7's drgn script with drgn itself replaced by empty modules."""
    names = ("drgn", "drgn.helpers", "drgn.helpers.linux", "drgn.helpers.linux.list")
    stubs = {name: types.ModuleType(name) for name in names}
    stubs["drgn.helpers.linux.list"].list_for_each_entry = mock.Mock(
        side_effect=AssertionError("guest memory is not read in these tests")
    )
    spec = importlib.util.spec_from_file_location(
        "demo07_task_evolution_serial", str(DEMO07_DIR / "task_evolution.py")
    )
    module = importlib.util.module_from_spec(spec)
    # The script prepends demos/lib to sys.path when it is loaded; keep that
    # from leaking into the rest of the test process.
    with mock.patch.dict(sys.modules, stubs):
        with mock.patch.object(sys, "path", list(sys.path)):
            spec.loader.exec_module(module)
    return module


class ObservationSerialTest(unittest.TestCase):
    """observation() counts the serial bytes that reach the pipe during a read.

    QEMU writes the guest's serial output to a pipe that it keeps open, so an
    empty pipe reads as "no data yet", not as end of file. The read end here is
    non-blocking, as in the demo. A byte that arrives while a read runs fails
    the read; bytes already queued before it began are logged and not counted.
    """

    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.serial_log = Path(directory.name) / "serial.log"
        read_fd, self.write_fd = os.pipe()
        self.addCleanup(os.close, read_fd)
        self.addCleanup(os.close, self.write_fd)
        os.set_blocking(read_fd, False)
        self.guest = dh.HermitGuestProgram(SimpleNamespace(timeout=1.0))
        self.guest._qemu_pid = QEMU_PID
        self.guest._tracer_tgid = TRACER_TGID
        self.guest._serial_read_fd = read_fd
        self.guest.serial_log = self.serial_log
        # No guest memory is read: the drgn program is a placeholder.
        self.guest._program = mock.Mock(return_value=object())
        patcher = mock.patch.object(dh, "_proc_state", side_effect=_stopped_state)
        patcher.start()
        self.addCleanup(patcher.stop)

    def _logged(self):
        if not self.serial_log.exists():
            return b""
        return self.serial_log.read_bytes()

    def test_bytes_that_arrive_during_a_read_fail_it(self):
        with self.assertRaisesRegex(RuntimeError, r"serial_delta=5$"):
            with self.guest.observation():
                os.write(self.write_fd, b"hello")
        self.assertEqual(len(self.guest.metrics), 1)
        self.assertEqual(self.guest.metrics[-1].serial_bytes_delta, 5)
        self.assertEqual(self._logged(), b"hello")

    def test_bytes_queued_before_a_read_are_logged_and_not_counted(self):
        os.write(self.write_fd, b"/ # ")
        with self.guest.observation():
            pass
        self.assertEqual(self.guest.metrics[-1].serial_bytes_delta, 0)
        self.assertEqual(self._logged(), b"/ # ")

    def test_a_quiet_read_records_no_serial_bytes(self):
        # Positive control: nothing on the pipe and both processes stopped.
        with self.guest.observation():
            pass
        self.assertEqual(len(self.guest.metrics), 1)
        self.assertEqual(self.guest.metrics[-1].serial_bytes_delta, 0)


class EvolutionLineTest(unittest.TestCase):
    """Each evolution line prints the serial counts that observation() measured."""

    def _run_main(self, before_delta, after_delta):
        """Run main() over two identical passes; return its status and lines."""
        module = _load_task_evolution()
        before = [(0, "swapper/0"), (1, "init"), (95, "sleep")]
        after = [(0, "swapper/0"), (1, "sh"), (101, "sleep"), (102, "sleep")]
        removed, added = module._task_diff(before, after)
        passes = (
            before,
            after,
            removed,
            added,
            _metrics(before_delta),
            _metrics(after_delta),
        )
        environment = {"DEMO07_RUNS": "2", "DEMO07_TASK_LIMIT": "16"}
        output = io.StringIO()
        with contextlib.ExitStack() as stack:
            stack.enter_context(
                mock.patch.object(module, "_config", return_value=object())
            )
            stack.enter_context(
                mock.patch.object(module, "_run_once", return_value=passes)
            )
            # The boot snapshot check is tested in test_boot_snapshot_record.
            ensure = stack.enter_context(
                mock.patch.object(module, "_ensure_boot_snapshot")
            )
            stack.enter_context(mock.patch.dict(os.environ, environment))
            stack.enter_context(contextlib.redirect_stdout(output))
            try:
                status = module.main()
            except RuntimeError as error:
                status = error
        ensure.assert_called_once_with()
        lines = [
            line
            for line in output.getvalue().splitlines()
            if line.startswith("evolution ")
        ]
        return status, lines

    def test_the_line_shows_the_measured_counts(self):
        status, lines = self._run_main(2, 3)
        self.assertEqual(len(lines), 2, lines)
        for line in lines:
            self.assertTrue(line.endswith(" serial_delta=2/3"), line)
        # main() still refuses passes whose reads saw serial output.
        self.assertIsInstance(status, RuntimeError)
        self.assertIn("serial output advanced during a drgn read", str(status))

    def test_quiet_reads_print_zero_counts_and_succeed(self):
        # Positive control.
        status, lines = self._run_main(0, 0)
        self.assertEqual(status, 0)
        self.assertEqual(len(lines), 2, lines)
        for line in lines:
            self.assertTrue(line.endswith(" serial_delta=0/0"), line)


if __name__ == "__main__":
    unittest.main()
