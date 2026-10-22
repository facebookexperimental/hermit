#!/usr/bin/env python3
"""Tests for the bounds ``demo_common`` puts on a child and on its output.

The QEMU demos launch Hermit with ``start_new_session=True`` and wait for it
with ``wait_for_process``. Two bounds can stop a run: the log cap (the INFO log
grew past ``max_log_bytes``) and the wall-clock timeout. After Hermit exits,
``drain_output`` waits for a thread to copy the rest of Hermit's output into
the log; processes Hermit started can hold that output open and keep writing
to it, so the cap still applies, and a time limit stops them. These tests
launch a real child that leads its own process group and has a descendant,
so they check what a demo depends on: the error names the bound that fired,
and the whole group, descendant included, is gone when the error arrives.

Each function checks the size once more before it returns: wait_for_process
once the child has exited, drain_output once the copy has ended. Output that
is all written between two checks is otherwise never checked. The tests for
that use finite writers and wait until all of the output is in the log before
calling the function, so that only this last check can see it.

The child's PID, and the group ID it leads, stay its own until it is reaped,
even after it has exited; once it is reaped, Linux can give the number to a
new process. So the helpers that stop a group must send every signal before
they reap the child. StopOrderTest checks that order with every system call
faked; the real-process tests check it with the child's state in /proc at
each signal.

They need no Hermit, QEMU or kernel. Run directly
(``python3 demos/tests/test_wait_for_process.py``) or via ``make -C demos test``.
"""

import contextlib
import errno
import io
import os
import signal
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from pathlib import Path
from types import SimpleNamespace
from typing import List, Optional, Tuple
from unittest import mock

DEMO_DIR = Path(__file__).resolve().parent.parent
LIB_DIR = DEMO_DIR / "lib"
sys.path.insert(0, str(LIB_DIR))

import demo_common as dc  # noqa: E402

# A child that starts a descendant in its own process group, records the
# descendant's PID, and then either appends to a log forever or just waits.
CHILD = r"""
import subprocess, sys, time
from pathlib import Path
descendant = subprocess.Popen(["sleep", "1000"])
Path(sys.argv[1]).write_text(str(descendant.pid))
log = Path(sys.argv[2]) if len(sys.argv) > 2 else None
while True:
    if log is not None:
        with log.open("ab") as handle:
            handle.write(b"x" * 1024)
    time.sleep(0.01)
"""

# A child that starts a descendant on its own standard output, records the
# descendant's PID, and exits with the status in argv[3] while the descendant
# keeps running. Depending on argv[2], the descendant keeps writing to the
# output (1 KiB about every millisecond), holds it open writing nothing,
# writes 1,500 bytes half a second later and exits, or writes 4,096 or 8,192
# bytes at once and exits.
LEAVER = r"""
import subprocess, sys
from pathlib import Path
descendants = {
    "write": "import os, time\ntry:\n    while True:\n        os.write(1, b'x' * 1024)\n        time.sleep(0.001)\nexcept BrokenPipeError:\n    pass\n",
    "hold": "import time\ntime.sleep(1000)\n",
    "late": "import os, time\ntime.sleep(0.5)\nos.write(1, b'late output\\n' * 125)\n",
    "burst-4096": "import os\nos.write(1, b'x' * 4096)\n",
    "burst-8192": "import os\nos.write(1, b'x' * 8192)\n",
}
descendant = subprocess.Popen([sys.executable, "-c", descendants[sys.argv[2]]])
Path(sys.argv[1]).write_text(str(descendant.pid))
sys.exit(int(sys.argv[3]))
"""

# A child that starts a descendant which ignores SIGTERM, and then waits until a
# signal ends it. The descendant records its own PID once it ignores SIGTERM, so
# from then on only SIGKILL stops it.
IGNORER = r"""
import subprocess, sys, time
member = (
    "import os, signal, sys, time\n"
    "signal.signal(signal.SIGTERM, signal.SIG_IGN)\n"
    "open(sys.argv[1] + '.tmp', 'w').write(str(os.getpid()))\n"
    "os.rename(sys.argv[1] + '.tmp', sys.argv[1])\n"
    "time.sleep(1000)\n"
)
subprocess.Popen([sys.executable, "-c", member, sys.argv[1]])
time.sleep(1000)
"""

# A child that starts a descendant in its own process group, records the
# descendant's PID, writes argv[3] bytes to the log argv[2] at once, and exits
# with status 5 while the descendant keeps running.
BURST = r"""
import subprocess, sys
from pathlib import Path
descendant = subprocess.Popen(["sleep", "1000"])
Path(sys.argv[1]).write_text(str(descendant.pid))
Path(sys.argv[2]).write_bytes(b"x" * int(sys.argv[3]))
sys.exit(5)
"""


def _gone(pid: int) -> bool:
    """Whether ``pid`` has exited (a zombie awaiting its reaper counts)."""
    try:
        state = Path("/proc/{}/stat".format(pid)).read_text().rsplit(")", 1)[1].split()[0]
    except (FileNotFoundError, ProcessLookupError, IndexError):
        return True
    return state in ("Z", "X")


def _state(pid: int) -> Optional[str]:
    """The state /proc shows for ``pid`` (Z: exited, not yet reaped), or None once it is gone."""
    try:
        return Path("/proc/{}/stat".format(pid)).read_text().rsplit(")", 1)[1].split()[0]
    except (FileNotFoundError, ProcessLookupError, IndexError):
        return None


class WaitForProcessBoundsTest(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.directory = Path(directory.name)
        self.pid_file = self.directory / "descendant.pid"

    def _launch(self, *arguments: str, script: str = CHILD, **keywords) -> Tuple[subprocess.Popen, int]:
        process = subprocess.Popen(
            [sys.executable, "-c", script, str(self.pid_file), *arguments],
            start_new_session=True,
            **keywords,
        )
        # Whatever the test does, leave nothing running.
        self.addCleanup(dc.stop_process, process)
        deadline = time.monotonic() + 30
        while not self.pid_file.is_file() or not self.pid_file.read_text():
            self.assertLess(time.monotonic(), deadline, "the child never started its descendant")
            time.sleep(0.01)
        descendant = int(self.pid_file.read_text())
        self.addCleanup(self._kill_if_running, descendant)
        return process, descendant

    @staticmethod
    def _kill_if_running(pid: int) -> None:
        if not _gone(pid):
            with contextlib.suppress(ProcessLookupError):
                os.kill(pid, 9)

    def _assert_descendant_stops(self, descendant: int) -> None:
        # The descendant was signalled with its group; allow its reaper a moment.
        deadline = time.monotonic() + 5
        while not _gone(descendant) and time.monotonic() < deadline:
            time.sleep(0.01)
        self.assertTrue(_gone(descendant), "the child's descendant {} kept running".format(descendant))

    def test_a_log_past_its_cap_stops_the_group_and_names_the_cap(self):
        log = self.directory / "hermit-info.log"
        process, descendant = self._launch(str(log))
        with contextlib.redirect_stdout(io.StringIO()):
            with self.assertRaises(dc.LogCapExceeded) as caught:
                dc.wait_for_process(process, timeout=60, log_path=log, max_log_bytes=4096)
        error = caught.exception
        self.assertIsInstance(error, RuntimeError)
        self.assertEqual(error.log_path, log)
        self.assertEqual(error.max_log_bytes, 4096)
        self.assertGreater(error.log_size, 4096)
        self.assertLess(error.elapsed, 60)
        self.assertEqual(
            str(error),
            "{} grew to {} bytes, past the 4096-byte log cap; the demo then signalled "
            "the launched process's process group".format(log, error.log_size),
        )
        self.assertIsNotNone(process.poll(), "the child kept running past the log cap")
        self._assert_descendant_stops(descendant)

    def _assert_descendant_stopped(self, descendant: int) -> None:
        # Checked at once: the group is stopped before the error is raised.
        self.assertTrue(
            _gone(descendant),
            "the child's descendant {} was still running when the error arrived".format(descendant),
        )

    def test_the_timeout_stops_the_group_before_it_raises(self):
        process, descendant = self._launch()
        with contextlib.redirect_stdout(io.StringIO()):
            with self.assertRaises(TimeoutError) as caught:
                dc.wait_for_process(process, timeout=1)
        self.assertEqual(str(caught.exception), "process exceeded timeout of 1s")
        self.assertIsNotNone(process.poll(), "the child was still running when the timeout was raised")
        self._assert_descendant_stopped(descendant)

    def _leave_behind(self, descendant: str, exit_status: int) -> Tuple[subprocess.Popen, int, threading.Thread, Path]:
        """Start LEAVER, copy its output into a log on a thread, wait for it to exit.

        Returns the exited child, its descendant's PID, the copying thread and
        the log, as a demo has them when its Hermit has exited.
        """
        log = self.directory / "hermit-info.log"
        process, pid = self._launch(
            descendant, str(exit_status), script=LEAVER, stdout=subprocess.PIPE, stderr=subprocess.STDOUT
        )
        self.addCleanup(process.stdout.close)
        handle = log.open("wb", buffering=0)
        self.addCleanup(handle.close)

        def copy() -> None:
            try:
                while True:
                    chunk = os.read(process.stdout.fileno(), 1 << 16)
                    if not chunk:
                        break
                    handle.write(chunk)
            except (OSError, ValueError):
                pass

        copier = threading.Thread(target=copy, daemon=True)
        copier.start()
        # As the demos wait for Hermit: the child is left unreaped, for
        # drain_output and stop_process_group.
        self.assertEqual(dc.wait_for_process(process, 30), exit_status)
        return process, pid, copier, log

    def test_a_log_past_its_cap_after_the_child_exits_stops_the_rest_of_its_group(self):
        process, descendant, copier, log = self._leave_behind("write", 7)
        with self.assertRaises(dc.LogCapExceeded) as caught:
            dc.drain_output(copier, process, 60, log_path=log, max_log_bytes=4096)
        error = caught.exception
        self.assertEqual(error.log_path, log)
        self.assertEqual(error.max_log_bytes, 4096)
        self.assertGreater(error.log_size, 4096)
        self.assertEqual(error.exit_status, 7)
        self.assertLess(error.elapsed, 60)
        self.assertEqual(
            str(error),
            "{} grew to {} bytes, past the 4096-byte log cap, after the launched "
            "process exited with status 7, while processes still wrote to it; the "
            "demo then signalled the launched process's process group".format(
                log, error.log_size
            ),
        )
        self._assert_descendant_stopped(descendant)
        # Nothing holds the output any more, so the copy reaches its end.
        copier.join(10)
        self.assertFalse(copier.is_alive(), "the output was still open after the group was stopped")

    def test_output_still_open_at_the_drain_limit_is_stopped_and_named(self):
        process, descendant, copier, log = self._leave_behind("hold", 0)
        with self.assertRaises(RuntimeError) as caught:
            dc.drain_output(copier, process, 1, log_path=log, max_log_bytes=4096, label="the child")
        self.assertNotIsInstance(caught.exception, dc.LogCapExceeded)
        self.assertEqual(
            str(caught.exception),
            "the child's output was still open 1s after it exited, so processes still "
            "held it; the demo then signalled the child's process group",
        )
        self._assert_descendant_stopped(descendant)
        copier.join(10)
        self.assertFalse(copier.is_alive(), "the output was still open after the group was stopped")

    def test_output_that_ends_in_time_is_copied_whole(self):
        # Positive control: output written after the child exited, within the
        # cap and the drain limit, reaches the log and fails nothing.
        process, descendant, copier, log = self._leave_behind("late", 0)
        dc.drain_output(copier, process, 30, log_path=log, max_log_bytes=4096)
        self.assertFalse(copier.is_alive())
        self.assertEqual(log.read_bytes(), b"late output\n" * 125)
        self._assert_descendant_stops(descendant)

    def _wait_until_exited(self, process: subprocess.Popen) -> None:
        """Wait for ``process`` to exit, leaving it unreaped as wait_for_process expects."""
        deadline = time.monotonic() + 30
        while os.waitid(os.P_PID, process.pid, os.WEXITED | os.WNOHANG | os.WNOWAIT) is None:
            self.assertLess(time.monotonic(), deadline, "the child never exited")
            time.sleep(0.01)

    def test_a_finite_burst_past_the_cap_is_caught_once_the_child_has_exited(self):
        # The child writes 8,192 bytes to the log and exits before
        # wait_for_process first looks, so only the check made after the exit
        # can see the log past the cap.
        log = self.directory / "hermit-info.log"
        process, descendant = self._launch(str(log), "8192", script=BURST)
        self._wait_until_exited(process)
        self.assertEqual(log.stat().st_size, 8192)
        with self.assertRaises(dc.LogCapExceeded) as caught:
            dc.wait_for_process(process, timeout=60, log_path=log, max_log_bytes=4096)
        error = caught.exception
        self.assertEqual(error.log_path, log)
        self.assertEqual(error.log_size, 8192)
        self.assertEqual(error.max_log_bytes, 4096)
        self.assertEqual(error.exit_status, 5)
        self.assertTrue(error.final_check)
        self.assertLess(error.elapsed, 60)
        self.assertEqual(
            str(error),
            "{} was 8192 bytes, past the 4096-byte log cap, when checked after the "
            "launched process exited with status 5; the demo then signalled its "
            "process group".format(log),
        )
        self.assertEqual(
            process.returncode, 5, "the child was not reaped after its group was stopped"
        )
        self._assert_descendant_stopped(descendant)

    def test_a_child_that_exits_with_its_log_at_the_cap_is_not_stopped(self):
        # Positive control: the log may reach the cap; only a log past it fails.
        log = self.directory / "hermit-info.log"
        process, descendant = self._launch(str(log), "4096", script=BURST)
        self._wait_until_exited(process)
        status = dc.wait_for_process(process, timeout=60, log_path=log, max_log_bytes=4096)
        self.assertEqual(status, 5)
        self.assertIsNone(process.returncode, "wait_for_process reaped the child")
        self.assertFalse(_gone(descendant), "wait_for_process stopped the child's group")

    def test_finite_output_past_the_cap_copied_before_the_drain_is_caught(self):
        # The descendant writes 8,192 bytes to the output and exits, and the
        # copy reaches the end of the output before drain_output first looks,
        # so only the check made after the copy ended can see the log past
        # the cap.
        process, descendant, copier, log = self._leave_behind("burst-8192", 3)
        copier.join(30)
        self.assertFalse(copier.is_alive(), "the copy never reached the end of the output")
        self.assertEqual(log.stat().st_size, 8192)
        with self.assertRaises(dc.LogCapExceeded) as caught:
            dc.drain_output(copier, process, 60, log_path=log, max_log_bytes=4096)
        error = caught.exception
        self.assertEqual(error.log_path, log)
        self.assertEqual(error.log_size, 8192)
        self.assertEqual(error.max_log_bytes, 4096)
        self.assertEqual(error.exit_status, 3)
        self.assertTrue(error.final_check)
        self.assertLess(error.elapsed, 60)
        self.assertEqual(
            str(error),
            "{} was 8192 bytes, past the 4096-byte log cap, when checked after the "
            "launched process exited with status 3; the demo then signalled its "
            "process group".format(log),
        )
        self.assertEqual(
            process.returncode, 3, "the child was not reaped after its group was stopped"
        )
        self._assert_descendant_stops(descendant)

    def test_output_that_ends_at_the_cap_passes_the_drain(self):
        # Positive control: the log may reach the cap; only a log past it fails.
        process, descendant, copier, log = self._leave_behind("burst-4096", 0)
        copier.join(30)
        self.assertFalse(copier.is_alive(), "the copy never reached the end of the output")
        self.assertIsNone(dc.drain_output(copier, process, 60, log_path=log, max_log_bytes=4096))
        self.assertEqual(log.read_bytes(), b"x" * 4096)
        self.assertIsNone(process.returncode, "drain_output reaped the child")
        self._assert_descendant_stops(descendant)

    def _slow_group_stops(self, seconds: float) -> List[float]:
        """Make every stop_process_group call wait ``seconds`` before it stops the group.

        Returns the list of times (time.monotonic()) at which the calls began.
        """
        began: List[float] = []
        real_stop = dc.stop_process_group

        def slow_stop(process: Optional[subprocess.Popen]) -> None:
            began.append(time.monotonic())
            time.sleep(seconds)
            real_stop(process)

        patcher = mock.patch.object(dc, "stop_process_group", slow_stop)
        patcher.start()
        self.addCleanup(patcher.stop)
        return began

    def test_the_time_of_a_cap_seen_while_the_child_runs_leaves_out_the_stop(self):
        # The elapsed time is read when the size is found past the cap. The
        # group's stop is made 1 s slower, so a time read after the stop is at
        # least 1 s later than the stop's start.
        log = self.directory / "hermit-info.log"
        process, descendant = self._launch(str(log))
        began = self._slow_group_stops(1.0)
        called = time.monotonic()
        with contextlib.redirect_stdout(io.StringIO()):
            with self.assertRaises(dc.LogCapExceeded) as caught:
                dc.wait_for_process(process, timeout=60, log_path=log, max_log_bytes=4096)
        error = caught.exception
        self.assertIsNone(error.exit_status, "the cap was not seen while the child ran")
        self.assertEqual(len(began), 1)
        self.assertLessEqual(
            error.elapsed,
            began[0] - called,
            "the elapsed time includes the time it took to stop the group",
        )
        self._assert_descendant_stopped(descendant)

    def test_the_time_of_a_cap_seen_in_the_drain_leaves_out_the_stop(self):
        process, descendant, copier, log = self._leave_behind("write", 7)
        began = self._slow_group_stops(1.0)
        called = time.monotonic()
        with self.assertRaises(dc.LogCapExceeded) as caught:
            dc.drain_output(
                copier, process, 60, log_path=log, max_log_bytes=4096, started=called
            )
        error = caught.exception
        self.assertEqual(error.exit_status, 7)
        self.assertFalse(error.final_check, "the cap was not seen while the output was open")
        self.assertEqual(len(began), 1)
        self.assertLessEqual(
            error.elapsed,
            began[0] - called,
            "the elapsed time includes the time it took to stop the group",
        )
        self._assert_descendant_stopped(descendant)
        copier.join(10)
        self.assertFalse(copier.is_alive(), "the output was still open after the group was stopped")

    def _record_signals(self, leader: int) -> List[Tuple[str, int, int, Optional[str]]]:
        """Record every kill() and killpg() made from now on, with ``leader``'s state then.

        Each entry is (call, PID or group, signal, state of ``leader`` in /proc
        when the call was made); the signal is still sent.
        """
        sent: List[Tuple[str, int, int, Optional[str]]] = []
        real_killpg, real_kill = os.killpg, os.kill

        def killpg(group: int, sig: int) -> None:
            sent.append(("killpg", group, sig, _state(leader)))
            real_killpg(group, sig)

        def kill(pid: int, sig: int) -> None:
            sent.append(("kill", pid, sig, _state(leader)))
            real_kill(pid, sig)

        for name, replacement in (("killpg", killpg), ("kill", kill)):
            patcher = mock.patch.object(dc.os, name, replacement)
            patcher.start()
            self.addCleanup(patcher.stop)
        return sent

    def test_stop_process_sends_its_sigkill_before_it_reaps_the_child(self):
        # SIGTERM ends the child but not its descendant, which ignores it. The
        # SIGKILL that follows goes to the group after the child has exited, so
        # the child must still be unreaped then, or the group ID could be
        # another process's.
        process, descendant = self._launch(script=IGNORER)
        sent = self._record_signals(process.pid)
        dc.stop_process(process)
        self.assertEqual(
            [entry[:3] for entry in sent],
            [("killpg", process.pid, signal.SIGTERM), ("killpg", process.pid, signal.SIGKILL)],
        )
        self.assertNotIn(sent[0][3], (None, "Z"), "the child was not running when SIGTERM was sent")
        self.assertEqual(
            sent[1][3],
            "Z",
            "SIGKILL was sent to the child's group when /proc showed the child as {!r}; "
            "only an exited child that is not yet reaped (Z) keeps its group ID".format(sent[1][3]),
        )
        self.assertEqual(process.returncode, -signal.SIGTERM, "stop_process did not reap the child")
        self._assert_descendant_stops(descendant)

    def test_stop_process_group_signals_the_group_while_the_exited_child_is_unreaped(self):
        process, descendant = self._launch("hold", "7", script=LEAVER)
        self.assertEqual(dc.wait_for_process(process, 30), 7)
        self.assertIsNone(process.returncode, "wait_for_process reaped the child")
        self.assertEqual(_state(process.pid), "Z")
        sent = self._record_signals(process.pid)
        dc.stop_process_group(process)
        self.assertTrue(sent, "nothing was sent to the child's group")
        self.assertEqual(sent[0][:3], ("killpg", process.pid, signal.SIGTERM))
        # SIGTERM ends the descendant, but the group's SIGKILL is sent anyway:
        # what /proc showed after SIGTERM is a sample, and it never decides it.
        self.assertEqual(
            sent[-1][:3],
            ("killpg", process.pid, signal.SIGKILL),
            "the last signal was not SIGKILL to the child's group: {}".format(sent),
        )
        self.assertEqual(
            [entry for entry in sent if entry[3] != "Z"],
            [],
            "a signal was sent when the child was not an exited, unreaped process: {}".format(sent),
        )
        self.assertEqual(process.returncode, 7, "stop_process_group did not reap the child")
        self._assert_descendant_stopped(descendant)

    def test_wait_for_process_reports_a_signal_as_popen_does(self):
        process = subprocess.Popen(
            [sys.executable, "-c", "import os, signal; os.kill(os.getpid(), signal.SIGKILL)"],
            start_new_session=True,
        )
        self.addCleanup(dc.stop_process, process)
        self.assertEqual(dc.wait_for_process(process, 30), -signal.SIGKILL)
        self.assertIsNone(process.returncode, "wait_for_process reaped the child")
        dc.stop_process_group(process)
        self.assertEqual(process.returncode, -signal.SIGKILL)

    def test_drain_output_refuses_a_child_that_was_already_reaped(self):
        process, descendant, copier, log = self._leave_behind("hold", 0)
        process.wait(timeout=30)  # Reaped, which a caller of drain_output must not do.
        sent = self._record_signals(process.pid)
        with self.assertRaises(ValueError) as caught:
            dc.drain_output(copier, process, 1, log_path=log, max_log_bytes=4096)
        self.assertEqual(
            str(caught.exception),
            "drain_output needs the launched process unreaped, but it was already "
            "reaped (exit status 0), so the processes it left in its group can no "
            "longer be signalled safely; wait for it with wait_for_process",
        )
        self.assertEqual(sent, [], "drain_output signalled the group of a child that was already reaped")
        self._kill_if_running(descendant)
        copier.join(10)


# The fake child's PID. The fakes never pass it to the real kill() or killpg().
FAKE_PID = 4242
# The process group the fakes report for the test itself.
FAKE_OWN_GROUP = 1000


class ObservationFailed(Exception):
    """Raised by a faked observation (a scan of /proc, a waitid) that fails."""


def _raise_on_call(function, call: int):
    """``function``, except that its ``call``-th call raises ObservationFailed instead."""
    calls = [0]

    def wrapper(*arguments):
        calls[0] += 1
        if calls[0] == call:
            raise ObservationFailed("call {} of {} failed".format(call, function.__name__))
        return function(*arguments)

    return wrapper


def _interrupt_once(sleep):
    """``sleep``, except that its first call raises KeyboardInterrupt, as a SIGINT would."""
    interrupted = [False]

    def wrapper(seconds: float) -> None:
        if not interrupted[0]:
            interrupted[0] = True
            raise KeyboardInterrupt
        sleep(seconds)

    return wrapper


class _FakeGroup:
    """A child leading its own process group, with Linux and Popen replaced by fakes.

    Nothing real is signalled. ``events`` records, in order, every signal sent
    to the child's PID or to its group, and the moment the child is reaped:
    ``("killpg", group, signal)``, ``("kill", pid, signal)`` and
    ``("reap", pid)``. The child exits on the first signal in
    ``leader_dies_on`` (status: minus that signal), or with ``status`` once the
    fake clock reaches ``exits_at``. Each entry of ``others`` is another member
    of the group, given as the set of signals that end it. With
    ``reaped_elsewhere_on``, something other than the Popen reaps the child as
    soon as that signal ends it.
    """

    def __init__(
        self,
        leader_dies_on=(),
        others=(),
        exits_at: Optional[float] = None,
        status: int = 0,
        exited: bool = False,
        reaped: bool = False,
        reaped_elsewhere_on: Optional[int] = None,
    ):
        self.events: List[tuple] = []
        self.now = 0.0
        self.leader_dies_on = set(leader_dies_on)
        self.others = [set(signals) for signals in others]
        self.exits_at = exits_at
        self.status = status
        self.exited = exited or reaped
        self.reaped = reaped
        self.reaped_elsewhere_on = reaped_elsewhere_on
        self.process = _FakePopen(self)
        if reaped:
            self.process.returncode = status
        self.os = _FakeOs(self)
        self.time = SimpleNamespace(monotonic=self.monotonic, sleep=self.sleep)

    def monotonic(self) -> float:
        return self.now

    def sleep(self, seconds: float) -> None:
        self.now += seconds

    def check_exit(self) -> None:
        if not self.exited and self.exits_at is not None and self.now >= self.exits_at:
            self.exited = True

    def reap(self) -> None:
        self.reaped = True
        self.events.append(("reap", FAKE_PID))

    def deliver(self, sig: int, to_group: bool) -> None:
        if sig in self.leader_dies_on and not self.exited:
            self.exited = True
            self.status = -sig
            if self.reaped_elsewhere_on == sig:
                self.reap()
        if to_group:
            self.others = [signals for signals in self.others if sig not in signals]

    # The system calls demo_common makes, as Linux answers them.
    def killpg(self, group: int, sig: int) -> None:
        self.events.append(("killpg", group, sig))
        if group != FAKE_PID or (self.reaped and not self.others):
            raise ProcessLookupError(errno.ESRCH, "No such process")
        if sig:
            self.deliver(sig, to_group=True)

    def kill(self, pid: int, sig: int) -> None:
        self.events.append(("kill", pid, sig))
        if pid != FAKE_PID or self.reaped:
            raise ProcessLookupError(errno.ESRCH, "No such process")
        if sig:
            self.deliver(sig, to_group=False)

    def getpgid(self, pid: int) -> int:
        if pid != FAKE_PID or self.reaped:
            raise ProcessLookupError(errno.ESRCH, "No such process")
        return FAKE_PID

    def getpgrp(self) -> int:
        return FAKE_OWN_GROUP

    def waitid(self, idtype: int, pid: int, options: int):
        assert (idtype, pid) == (os.P_PID, FAKE_PID), (idtype, pid)
        self.check_exit()
        if self.reaped:
            raise ChildProcessError(errno.ECHILD, "No child processes")
        if not self.exited:
            assert options & os.WNOHANG, "a waitid without WNOHANG would wait forever here"
            return None
        if not options & os.WNOWAIT:
            self.reap()
        if self.status >= 0:
            return SimpleNamespace(si_code=os.CLD_EXITED, si_status=self.status)
        return SimpleNamespace(si_code=os.CLD_KILLED, si_status=-self.status)

    def other_group_members(self, group: int, leader: int) -> bool:
        assert (group, leader) == (FAKE_PID, FAKE_PID), (group, leader)
        return bool(self.others)


class _FakeOs:
    """The os module, with the calls _FakeGroup fakes replaced by its own."""

    def __init__(self, fake: _FakeGroup):
        self.killpg = fake.killpg
        self.kill = fake.kill
        self.getpgid = fake.getpgid
        self.getpgrp = fake.getpgrp
        self.waitid = fake.waitid

    def __getattr__(self, name: str):
        return getattr(os, name)


class _FakeProbe:
    """The pidfd question about a process outside the fake group: may it be signalled?

    ``refusal`` is None when signal 0 is accepted, or ``(call, error)``: the
    call ("pidfd_open" or "pidfd_send_signal") that raises ``error``, as Linux
    answers it. ``calls`` records every call in order, and ``opened`` counts
    the pidfds handed out, which the caller must close.
    """

    # Never used for real: os.close is faked wherever a _FakeProbe is.
    PIDFD = 1_000_001

    def __init__(self, refusal=None):
        self.refusal = refusal
        self.calls: List[tuple] = []
        self.opened = 0

    def pidfd_open(self, pid: int, *rest) -> int:
        self.calls.append(("pidfd_open", pid))
        if self.refusal is not None and self.refusal[0] == "pidfd_open":
            raise self.refusal[1]
        self.opened += 1
        return self.PIDFD

    def pidfd_send_signal(self, pidfd: int, sig: int, *rest) -> None:
        self.calls.append(("pidfd_send_signal", pidfd, sig))
        if self.refusal is not None and self.refusal[0] == "pidfd_send_signal":
            raise self.refusal[1]


class _FakeSignal:
    """The signal module, with pidfd_send_signal answered by a _FakeProbe."""

    def __init__(self, probe: _FakeProbe):
        self.pidfd_send_signal = probe.pidfd_send_signal

    def __getattr__(self, name: str):
        return getattr(signal, name)


class _FakePopen:
    """What demo_common uses of subprocess.Popen, for a _FakeGroup child."""

    def __init__(self, fake: _FakeGroup):
        self.fake = fake
        self.pid = FAKE_PID
        self.returncode: Optional[int] = None

    def poll(self) -> Optional[int]:
        if self.returncode is None:
            self.fake.check_exit()
            if self.fake.reaped:
                self.returncode = 0  # What Popen records for a child reaped elsewhere.
            elif self.fake.exited:
                self.fake.reap()
                self.returncode = self.fake.status
        return self.returncode

    def wait(self, timeout: Optional[float] = None) -> int:
        deadline = self.fake.now + (3600 if timeout is None else timeout)
        while self.poll() is None:
            if self.fake.now >= deadline:
                if timeout is None:
                    raise AssertionError("the fake child never exits")
                raise subprocess.TimeoutExpired("fake child", timeout)
            self.fake.sleep(0.05)
        return self.returncode

    def send_signal(self, sig: int) -> None:
        # As Popen does: poll first, and signal only a child not known to have exited.
        if self.poll() is None:
            self.fake.kill(self.pid, sig)


class StopOrderTest(unittest.TestCase):
    """The order of the signals and the reap, with every system call faked.

    The fake child's PID, and the group ID it leads, are its own until it is
    reaped, so nothing may be sent to either number after the reap.
    """

    def run_faked(self, fake: _FakeGroup, function, *arguments):
        with mock.patch.object(dc, "os", fake.os), mock.patch.object(
            dc, "time", fake.time
        ), mock.patch.object(dc, "_other_group_members", fake.other_group_members, create=True):
            return function(fake.process, *arguments)

    @contextlib.contextmanager
    def faked_proc(self, fake: _FakeGroup, listed: List[str], open_file):
        """As run_faked, but with the real _other_group_members, reading a fake /proc.

        /proc lists the entries named in ``listed``, and ``open_file`` stands in
        for os.open. A member of ``fake.others`` is listed only if named here, as
        when a scan reads /proc before that member exists.
        """

        def scandir(path):
            self.assertEqual(path, "/proc")
            return iter([SimpleNamespace(name=name) for name in listed])

        fake.os.scandir = scandir
        fake.os.open = open_file
        with mock.patch.object(dc, "os", fake.os), mock.patch.object(dc, "time", fake.time):
            yield

    def assert_nothing_sent_after_the_reap(self, fake: _FakeGroup) -> None:
        reaps = [index for index, event in enumerate(fake.events) if event[0] == "reap"]
        after = reaps[0] + 1 if reaps else len(fake.events)
        self.assertEqual(
            fake.events[after:],
            [],
            "something was sent after the child was reaped, when its PID and group "
            "ID could belong to another process; events: {}".format(fake.events),
        )

    def test_stop_process_sends_its_sigkill_before_it_reaps_the_child(self):
        # SIGTERM ends the child; another member of its group ignores SIGTERM.
        fake = _FakeGroup(leader_dies_on={signal.SIGTERM}, others=[{signal.SIGKILL}])
        self.run_faked(fake, dc.stop_process)
        self.assert_nothing_sent_after_the_reap(fake)
        self.assertEqual(
            fake.events,
            [
                ("killpg", FAKE_PID, signal.SIGTERM),
                ("killpg", FAKE_PID, signal.SIGKILL),
                ("reap", FAKE_PID),
            ],
        )
        self.assertEqual(fake.others, [], "the member that ignores SIGTERM was not stopped")
        self.assertEqual(fake.process.returncode, -signal.SIGTERM)

    def test_stop_process_group_stops_a_running_group_before_it_reaps_the_child(self):
        fake = _FakeGroup(leader_dies_on={signal.SIGTERM}, others=[{signal.SIGKILL}])
        self.run_faked(fake, dc.stop_process_group)
        self.assert_nothing_sent_after_the_reap(fake)
        self.assertEqual(fake.events[-1], ("reap", FAKE_PID))
        self.assertIn(("killpg", FAKE_PID, signal.SIGKILL), fake.events)
        self.assertEqual(fake.others, [], "the member that ignores SIGTERM was not stopped")
        self.assertEqual(fake.process.returncode, -signal.SIGTERM)

    def test_stop_process_group_signals_an_exited_childs_group_before_it_reaps_the_child(self):
        # As wait_for_process leaves it: exited with status 7, not yet reaped,
        # while another member of its group still runs.
        fake = _FakeGroup(exited=True, status=7, others=[{signal.SIGTERM}])
        self.run_faked(fake, dc.stop_process_group)
        self.assert_nothing_sent_after_the_reap(fake)
        # The SIGKILL follows even though the group looked empty after SIGTERM.
        self.assertEqual(
            fake.events,
            [
                ("killpg", FAKE_PID, signal.SIGTERM),
                ("killpg", FAKE_PID, signal.SIGKILL),
                ("reap", FAKE_PID),
            ],
        )
        self.assertEqual(fake.others, [])
        self.assertEqual(fake.process.returncode, 7)

    def test_the_group_is_killed_even_when_a_scan_of_proc_finds_it_empty(self):
        # /proc lists only the child, but another member of its group, which
        # ignores SIGTERM, still runs: a scan is a sample and can miss a process,
        # one forked after the scan read the directory for example.
        fake = _FakeGroup(exited=True, status=7, others=[{signal.SIGKILL}])

        def no_stat_opened(path, flags, *rest):
            raise AssertionError("opened {}, which /proc did not list".format(path))

        with self.faked_proc(fake, ["self", str(FAKE_PID)], no_stat_opened):
            self.assertFalse(dc._other_group_members(FAKE_PID, FAKE_PID))
            dc.stop_process_group(fake.process)
        self.assert_nothing_sent_after_the_reap(fake)
        self.assertEqual(
            fake.events,
            [
                ("killpg", FAKE_PID, signal.SIGTERM),
                ("killpg", FAKE_PID, signal.SIGKILL),
                ("reap", FAKE_PID),
            ],
        )
        self.assertEqual(fake.others, [], "the member the scan missed was not stopped")
        self.assertEqual(fake.process.returncode, 7)

    def test_a_member_whose_stat_cannot_be_read_is_not_taken_as_gone(self):
        # /proc lists a second process, but reading its stat fails for a reason
        # other than its having exited, so whether it is in the child's group is
        # unknown. It is, and it ignores SIGTERM. A refusal (EACCES) is followed
        # by the pidfd question, which answers that this process may signal it;
        # any other failure counts without that question.
        descriptor = 1_000_000  # Never used for real: os.read and os.close are faked.
        for failing, error in (
            ("open", PermissionError(errno.EACCES, "Permission denied", "/proc/4243/stat")),
            ("open", OSError(errno.EMFILE, "Too many open files", "/proc/4243/stat")),
            ("read", OSError(errno.EIO, "Input/output error")),
        ):
            with self.subTest(failing=failing, error=errno.errorcode[error.errno]):
                fake = _FakeGroup(exited=True, status=7, others=[{signal.SIGKILL}])
                probe = _FakeProbe()  # Signal 0 is accepted.
                closed: List[int] = []

                def open_stat(path, flags, *rest):
                    self.assertEqual(path, "/proc/4243/stat")
                    if failing == "open":
                        raise error
                    return descriptor

                def read(fd, size):
                    self.assertEqual(fd, descriptor)
                    raise error

                fake.os.read = read
                fake.os.close = closed.append
                fake.os.pidfd_open = probe.pidfd_open
                with self.faked_proc(
                    fake, ["self", str(FAKE_PID), "4243"], open_stat
                ), mock.patch.object(dc, "signal", _FakeSignal(probe)):
                    self.assertTrue(
                        dc._other_group_members(FAKE_PID, FAKE_PID),
                        "a process whose stat could not be read was counted as gone",
                    )
                    dc.stop_process_group(fake.process)
                self.assert_nothing_sent_after_the_reap(fake)
                self.assertEqual(
                    fake.events,
                    [
                        ("killpg", FAKE_PID, signal.SIGTERM),
                        ("killpg", FAKE_PID, signal.SIGKILL),
                        ("reap", FAKE_PID),
                    ],
                )
                self.assertEqual(fake.others, [], "the member whose stat was unreadable was not stopped")
                self.assertEqual(fake.process.returncode, 7)
                if error.errno == errno.EACCES:
                    self.assertTrue(
                        probe.calls, "a refused stat was not followed by the pidfd question"
                    )
                    self.assertEqual(
                        set(probe.calls),
                        {("pidfd_open", 4243), ("pidfd_send_signal", _FakeProbe.PIDFD, 0)},
                    )
                    self.assertEqual(
                        closed.count(_FakeProbe.PIDFD), probe.opened, "a pidfd was left open"
                    )
                else:
                    self.assertEqual(probe.calls, [], "a failure other than a refusal was probed")
                if failing == "read":
                    self.assertTrue(closed, "the stat descriptor was not closed")
                    self.assertEqual(set(closed), {descriptor})

    def test_a_refused_stat_counts_only_a_process_this_process_may_signal(self):
        # /proc mounted with hidepid=1 refuses another user's /proc/<pid>/stat
        # with EPERM, and a security module refuses with EACCES, so that
        # process's group cannot be read. The pidfd question decides instead: a
        # process this process may not signal does not count, as when its stat
        # is readable. Before, every refusal counted, so on such a host no scan
        # found the group empty and each wait ran its full 10 seconds (review
        # finding R7-5 on https://github.com/rrnewton/hermit/pull/3703). Only how
        # long the waits last depends on this: the group's SIGKILL and the reap
        # follow either way.
        descriptor = 1_000_000  # Never used for real: os.read and os.close are faked.
        probes = (
            # (what the pidfd question answers, the refusal, whether it counts)
            (
                "signal 0 refused: EPERM",
                ("pidfd_send_signal", PermissionError(errno.EPERM, "Operation not permitted")),
                False,
            ),
            (
                "gone: ESRCH from pidfd_open",
                ("pidfd_open", ProcessLookupError(errno.ESRCH, "No such process")),
                False,
            ),
            ("signal 0 accepted", None, True),
            (
                "cannot ask: EMFILE from pidfd_open",
                ("pidfd_open", OSError(errno.EMFILE, "Too many open files")),
                True,
            ),
        )
        for failing in ("open", "read"):
            for refused in (errno.EPERM, errno.EACCES):
                for answer, refusal, counted in probes:
                    with self.subTest(
                        failing=failing, refused=errno.errorcode[refused], probe=answer
                    ):
                        # The child has exited, and nothing else is in its group.
                        fake = _FakeGroup(exited=True, status=7)
                        probe = _FakeProbe(refusal)
                        closed: List[int] = []

                        def open_stat(path, flags, *rest):
                            self.assertEqual(path, "/proc/4243/stat")
                            if failing == "open":
                                raise PermissionError(refused, os.strerror(refused), path)
                            return descriptor

                        def read(fd, size):
                            self.assertEqual(fd, descriptor)
                            raise PermissionError(refused, os.strerror(refused))

                        fake.os.read = read
                        fake.os.close = closed.append
                        fake.os.pidfd_open = probe.pidfd_open
                        with self.faked_proc(
                            fake, ["self", str(FAKE_PID), "4243"], open_stat
                        ), mock.patch.object(dc, "signal", _FakeSignal(probe)):
                            self.assertEqual(
                                dc._other_group_members(FAKE_PID, FAKE_PID),
                                counted,
                                "a process whose stat was refused, and which the pidfd "
                                "question answered {!r} about".format(answer),
                            )
                            dc.stop_process_group(fake.process)
                        # The question was asked about that process, with signal 0 only.
                        self.assertTrue(
                            probe.calls, "a refused stat was not followed by the pidfd question"
                        )
                        self.assertEqual(
                            {call[1] for call in probe.calls if call[0] == "pidfd_open"}, {4243}
                        )
                        self.assertLessEqual(
                            {call[1:] for call in probe.calls if call[0] == "pidfd_send_signal"},
                            {(_FakeProbe.PIDFD, 0)},
                        )
                        self.assertEqual(
                            closed.count(_FakeProbe.PIDFD), probe.opened, "a pidfd was left open"
                        )
                        if failing == "read":
                            self.assertIn(descriptor, closed, "the stat descriptor was not closed")
                        # The SIGKILL and the reap follow whatever the scans answered.
                        self.assert_killed_then_reaped(fake, 7)
                        if counted:
                            self.assertGreaterEqual(
                                fake.now, 20, "a wait ended although a possible member was counted"
                            )
                        else:
                            self.assertLess(
                                fake.now,
                                1,
                                "the waits ran {}s although no process that could be signalled "
                                "was left in the group".format(fake.now),
                            )

    def assert_killed_then_reaped(self, fake: _FakeGroup, status: int, expected=None) -> None:
        """The group's SIGKILL was sent before the child was reaped, and the child was reaped."""
        self.assert_nothing_sent_after_the_reap(fake)
        self.assertEqual(
            fake.events,
            expected
            or [
                ("killpg", FAKE_PID, signal.SIGTERM),
                ("killpg", FAKE_PID, signal.SIGKILL),
                ("reap", FAKE_PID),
            ],
        )
        self.assertEqual(fake.others, [], "the member that ignores SIGTERM was not stopped")
        self.assertEqual(fake.process.returncode, status, "the child was not reaped")

    def no_stat_opened(self, path, flags, *rest):
        raise AssertionError("opened {}, which /proc did not list".format(path))

    def test_a_proc_that_cannot_be_opened_is_not_taken_as_an_empty_group(self):
        # The descriptor table is full, so /proc cannot be opened, while another
        # member of the exited child's group ignores SIGTERM.
        fake = _FakeGroup(exited=True, status=7, others=[{signal.SIGKILL}])
        listings: List[str] = []

        def scandir(path):
            listings.append(path)
            raise OSError(errno.EMFILE, "Too many open files", path)

        fake.os.scandir = scandir
        with mock.patch.object(dc, "os", fake.os), mock.patch.object(dc, "time", fake.time):
            self.assertTrue(
                dc._other_group_members(FAKE_PID, FAKE_PID),
                "a /proc that could not be opened was taken as a group with no other member",
            )
            dc.stop_process_group(fake.process)
        self.assertTrue(listings, "/proc was never read")
        self.assert_killed_then_reaped(fake, 7)

    def test_a_proc_listing_that_fails_part_way_is_not_taken_as_an_empty_group(self):
        # The listing of /proc fails after two entries, before it reaches the
        # other member of the exited child's group, which ignores SIGTERM.
        fake = _FakeGroup(exited=True, status=7, others=[{signal.SIGKILL}])
        listed: List[str] = []

        def scandir(path):
            self.assertEqual(path, "/proc")

            def entries():
                for name in ("self", str(FAKE_PID)):
                    listed.append(name)
                    yield SimpleNamespace(name=name)
                raise OSError(errno.ENOMEM, "Cannot allocate memory")

            return entries()

        fake.os.scandir = scandir
        fake.os.open = self.no_stat_opened
        with mock.patch.object(dc, "os", fake.os), mock.patch.object(dc, "time", fake.time):
            self.assertTrue(
                dc._other_group_members(FAKE_PID, FAKE_PID),
                "a /proc listing that failed part-way was taken as a group with no other member",
            )
            dc.stop_process_group(fake.process)
        self.assertEqual(listed[:2], ["self", str(FAKE_PID)])
        self.assert_killed_then_reaped(fake, 7)

    def test_an_observation_that_raises_in_the_group_wait_skips_neither_the_sigkill_nor_the_reap(self):
        # The child has exited (as wait_for_process leaves it); another member of
        # its group ignores SIGTERM. The wait after the group's SIGTERM raises.
        def failed_scan(group, leader):
            raise ObservationFailed("the scan of /proc failed")

        for failing in ("scan raises every time", "sleep interrupted once"):
            with self.subTest(failing=failing):
                fake = _FakeGroup(exited=True, status=7, others=[{signal.SIGKILL}])
                if failing == "scan raises every time":
                    expected_error = ObservationFailed
                    members = failed_scan
                else:
                    expected_error = KeyboardInterrupt
                    members = fake.other_group_members
                    fake.time.sleep = _interrupt_once(fake.sleep)
                with mock.patch.object(dc, "os", fake.os), mock.patch.object(
                    dc, "time", fake.time
                ), mock.patch.object(dc, "_other_group_members", members):
                    with self.assertRaises(expected_error):
                        dc.stop_process_group(fake.process)
                self.assert_killed_then_reaped(fake, 7)

    def test_a_wait_that_raises_while_the_child_runs_skips_neither_the_sigkill_nor_the_reap(self):
        # The child ignores SIGTERM, and so does another member of its group.
        # The wait after the SIGTERM to the child raises.
        for stop in (dc.stop_process, dc.stop_process_group):
            for failing in ("waitid raises once", "sleep interrupted once"):
                with self.subTest(stop=stop.__name__, failing=failing):
                    fake = _FakeGroup(leader_dies_on={signal.SIGKILL}, others=[{signal.SIGKILL}])
                    if failing == "waitid raises once":
                        expected_error = ObservationFailed
                        # Call 1 is the check before the SIGTERM; call 2 is the
                        # first one in the wait after it.
                        fake.os.waitid = _raise_on_call(fake.os.waitid, 2)
                    else:
                        expected_error = KeyboardInterrupt
                        fake.time.sleep = _interrupt_once(fake.sleep)
                    with self.assertRaises(expected_error):
                        self.run_faked(fake, stop)
                    expected = [
                        ("killpg", FAKE_PID, signal.SIGTERM),
                        ("killpg", FAKE_PID, signal.SIGKILL),
                    ]
                    if stop is dc.stop_process_group:
                        # The group's own SIGKILL, sent while the child is unreaped.
                        expected.append(("killpg", FAKE_PID, signal.SIGKILL))
                    expected.append(("reap", FAKE_PID))
                    self.assert_killed_then_reaped(fake, -signal.SIGKILL, expected)

    def test_nothing_is_sent_for_a_child_that_was_already_reaped(self):
        for stop in (dc.stop_process, dc.stop_process_group):
            with self.subTest(stop=stop.__name__):
                fake = _FakeGroup(reaped=True, status=3, others=[{signal.SIGTERM}])
                self.run_faked(fake, stop)
                self.assertEqual(fake.events, [])
                self.assertEqual(fake.process.returncode, 3)

    def test_nothing_more_is_sent_once_the_child_turns_out_to_be_reaped_elsewhere(self):
        for stop in (dc.stop_process, dc.stop_process_group):
            with self.subTest(stop=stop.__name__):
                fake = _FakeGroup(
                    leader_dies_on={signal.SIGTERM},
                    reaped_elsewhere_on=signal.SIGTERM,
                    others=[{signal.SIGKILL}],
                )
                self.run_faked(fake, stop)
                self.assertEqual(
                    fake.events, [("killpg", FAKE_PID, signal.SIGTERM), ("reap", FAKE_PID)]
                )
                # Left running: its group ID may be another process's by now.
                self.assertEqual(fake.others, [{signal.SIGKILL}])
                self.assertEqual(fake.process.returncode, 0)

    def test_wait_for_process_returns_the_exit_status_without_reaping(self):
        for status in (5, -signal.SIGKILL):
            with self.subTest(status=status):
                fake = _FakeGroup(exits_at=0.35, status=status)
                with contextlib.redirect_stdout(io.StringIO()):
                    returned = self.run_faked(fake, dc.wait_for_process, 10)
                self.assertEqual(returned, status)
                self.assertEqual(fake.events, [], "wait_for_process reaped the child")
                self.assertIsNone(fake.process.returncode)


if __name__ == "__main__":
    unittest.main()
