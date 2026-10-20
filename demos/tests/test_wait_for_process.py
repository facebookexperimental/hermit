#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

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
They need no Hermit, QEMU or kernel. Run directly
(``python3 demos/tests/test_wait_for_process.py``) or via ``make -C demos test``.
"""

import contextlib
import io
import os
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from pathlib import Path
from typing import Tuple

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
# output (1 KiB about every millisecond), holds it open writing nothing, or
# writes 1,500 bytes half a second later and exits.
LEAVER = r"""
import subprocess, sys
from pathlib import Path
descendants = {
    "write": "import os, time\ntry:\n    while True:\n        os.write(1, b'x' * 1024)\n        time.sleep(0.001)\nexcept BrokenPipeError:\n    pass\n",
    "hold": "import time\ntime.sleep(1000)\n",
    "late": "import os, time\ntime.sleep(0.5)\nos.write(1, b'late output\\n' * 125)\n",
}
descendant = subprocess.Popen([sys.executable, "-c", descendants[sys.argv[2]]])
Path(sys.argv[1]).write_text(str(descendant.pid))
sys.exit(int(sys.argv[3]))
"""


def _gone(pid: int) -> bool:
    """Whether ``pid`` has exited (a zombie awaiting its reaper counts)."""
    try:
        state = (
            Path("/proc/{}/stat".format(pid)).read_text().rsplit(")", 1)[1].split()[0]
        )
    except (FileNotFoundError, ProcessLookupError, IndexError):
        return True
    return state in ("Z", "X")


class WaitForProcessBoundsTest(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.directory = Path(directory.name)
        self.pid_file = self.directory / "descendant.pid"

    def _launch(
        self, *arguments: str, script: str = CHILD, **keywords
    ) -> Tuple[subprocess.Popen, int]:
        process = subprocess.Popen(
            [sys.executable, "-c", script, str(self.pid_file), *arguments],
            start_new_session=True,
            **keywords,
        )
        # Whatever the test does, leave nothing running.
        self.addCleanup(dc.stop_process, process)
        deadline = time.monotonic() + 30
        while not self.pid_file.is_file() or not self.pid_file.read_text():
            self.assertLess(
                time.monotonic(), deadline, "the child never started its descendant"
            )
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
        self.assertTrue(
            _gone(descendant),
            "the child's descendant {} kept running".format(descendant),
        )

    def test_a_log_past_its_cap_stops_the_group_and_names_the_cap(self):
        log = self.directory / "hermit-info.log"
        process, descendant = self._launch(str(log))
        with contextlib.redirect_stdout(io.StringIO()):
            with self.assertRaises(dc.LogCapExceeded) as caught:
                dc.wait_for_process(
                    process, timeout=60, log_path=log, max_log_bytes=4096
                )
        error = caught.exception
        self.assertIsInstance(error, RuntimeError)
        self.assertEqual(error.log_path, log)
        self.assertEqual(error.max_log_bytes, 4096)
        self.assertGreater(error.log_size, 4096)
        self.assertLess(error.elapsed, 60)
        self.assertEqual(
            str(error),
            "{} grew to {} bytes, past the 4096-byte log cap; the run was stopped".format(
                log, error.log_size
            ),
        )
        self.assertIsNotNone(process.poll(), "the child kept running past the log cap")
        self._assert_descendant_stops(descendant)

    def _assert_descendant_stopped(self, descendant: int) -> None:
        # Checked at once: the group is stopped before the error is raised.
        self.assertTrue(
            _gone(descendant),
            "the child's descendant {} was still running when the error arrived".format(
                descendant
            ),
        )

    def test_the_timeout_stops_the_group_before_it_raises(self):
        process, descendant = self._launch()
        with contextlib.redirect_stdout(io.StringIO()):
            with self.assertRaises(TimeoutError) as caught:
                dc.wait_for_process(process, timeout=1)
        self.assertEqual(str(caught.exception), "process exceeded timeout of 1s")
        self.assertIsNotNone(
            process.poll(), "the child was still running when the timeout was raised"
        )
        self._assert_descendant_stopped(descendant)

    def _leave_behind(
        self, descendant: str, exit_status: int
    ) -> Tuple[subprocess.Popen, int, threading.Thread, Path]:
        """Start LEAVER, copy its output into a log on a thread, wait for it to exit.

        Returns the exited child, its descendant's PID, the copying thread and
        the log, as a demo has them when its Hermit has exited.
        """
        log = self.directory / "hermit-info.log"
        process, pid = self._launch(
            descendant,
            str(exit_status),
            script=LEAVER,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
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
        self.assertEqual(process.wait(timeout=30), exit_status)
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
            "process exited with status 7; the processes still writing to it were "
            "stopped".format(log, error.log_size),
        )
        self._assert_descendant_stopped(descendant)
        # Nothing holds the output any more, so the copy reaches its end.
        copier.join(10)
        self.assertFalse(
            copier.is_alive(), "the output was still open after the group was stopped"
        )

    def test_output_still_open_at_the_drain_limit_is_stopped_and_named(self):
        process, descendant, copier, log = self._leave_behind("hold", 0)
        with self.assertRaises(RuntimeError) as caught:
            dc.drain_output(
                copier, process, 1, log_path=log, max_log_bytes=4096, label="the child"
            )
        self.assertNotIsInstance(caught.exception, dc.LogCapExceeded)
        self.assertEqual(
            str(caught.exception),
            "the child's output was still open 1s after it exited, so the processes "
            "still holding it were stopped",
        )
        self._assert_descendant_stopped(descendant)
        copier.join(10)
        self.assertFalse(
            copier.is_alive(), "the output was still open after the group was stopped"
        )

    def test_output_that_ends_in_time_is_copied_whole(self):
        # Positive control: output written after the child exited, within the
        # cap and the drain limit, reaches the log and fails nothing.
        process, descendant, copier, log = self._leave_behind("late", 0)
        dc.drain_output(copier, process, 30, log_path=log, max_log_bytes=4096)
        self.assertFalse(copier.is_alive())
        self.assertEqual(log.read_bytes(), b"late output\n" * 125)
        self._assert_descendant_stops(descendant)


if __name__ == "__main__":
    unittest.main()
