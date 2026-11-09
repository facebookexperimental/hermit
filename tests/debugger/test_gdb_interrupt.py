# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

"""GDB's interrupt (Ctrl-C) against `hermit run --gdbserver`.

When its user presses Ctrl-C while the guest runs, gdb sends the remote
protocol's interrupt and expects the stub to stop the guest and report a stop.
That is how a hang is inspected live. Hermit runs one guest thread at a time,
and the guest's threads spend most of their time waiting in Detcore, for a turn
or for a sleep to end in virtual time, so the interrupt must stop the guest
whether or not a thread is running, and gdb must then list and unwind every
thread. `continue` must let the guest go on, and a second Ctrl-C stop it again.
"""

import re
import signal
import subprocess
import unittest

from harness import (
    compile_guest,
    DebuggerTestBase,
    exact_branch_counter,
    gdb_batch,
    GUESTS_DIR,
    HermitGdbserver,
    pick_free_port,
)

# guests/sleepers.c: two sleepers, a yielder and main.
THREADS = 4
# How long into each `continue` the interrupt comes. gdb's shell command sends
# it the way a terminal's Ctrl-C does: SIGINT to gdb, which then sends the
# interrupt to the gdbserver.
INTERRUPT_AFTER = 3
INTERRUPT_LATER = (
    f"shell p=$PPID; (sleep {INTERRUPT_AFTER}; kill -INT $p) >/dev/null 2>&1 &"
)
INSPECT = ["info threads", "thread apply all bt"]
INTERRUPT_TWICE_THEN_KILL = (
    [INTERRUPT_LATER, "continue"] + INSPECT + [INTERRUPT_LATER, "continue"] + INSPECT
) + ["kill"]
STRICT = [
    "--strict",
    "--base-env=minimal",
    "--epoch=2026-01-01T00:00:00Z",
]
# How long hermit may take to end once gdb is done.
HERMIT_END_TIMEOUT = 60


class GdbInterrupt(DebuggerTestBase):
    require_gdb = True

    @classmethod
    def setUpClass(cls):
        super().setUpClass()
        try:
            cls.sleepers = compile_guest(
                GUESTS_DIR / "sleepers.c", "sleepers", ["-pthread"]
            )
        except Exception as e:  # noqa: BLE001
            raise unittest.SkipTest(f"could not compile sleepers guest ({e})")

    def _interrupt(self, run_args=None):
        """Run the sleepers under `hermit run RUN_ARGS --gdbserver`, interrupt
        and inspect them twice, then kill them. Returns gdb's output, hermit's
        exit status and hermit's log."""
        port = pick_free_port()
        with HermitGdbserver(
            self.hermit, self.sleepers, port, run_args=run_args
        ) as srv:
            out = gdb_batch(
                self.sleepers,
                # A loaded host can answer slower than gdb's default 2 s.
                ["set remotetimeout 30", f"target remote 127.0.0.1:{port}"]
                + INTERRUPT_TWICE_THEN_KILL,
            )
            try:
                status = srv.wait(HERMIT_END_TIMEOUT)
            except subprocess.TimeoutExpired:
                self.fail(
                    f"hermit did not end after gdb did\ngdb:\n{out}\nhermit:\n{srv.read_log()}"
                )
            return out, status, srv.read_log()

    def assert_interrupted_twice_and_inspected(self, out, log):
        msg = f"gdb:\n{out}\nhermit:\n{log}"
        self.assertEqual(out.count("received signal SIGINT, Interrupt."), 2, msg=msg)
        self.assertNotIn("while the target is running", out, msg=msg)
        self.assertNotRegex(out, r"(Could not|Couldn't) read registers", msg=msg)
        # Each `info threads` lists every thread.
        listed = re.findall(r"^\*?\s+\d+\s+Thread \d+\.\d+ ", out, re.M)
        self.assertEqual(len(listed), 2 * THREADS, msg=msg)

        # Each `thread apply all bt` unwinds every thread to its function.
        def frames(function):
            return len(re.findall(rf"^#\d+\s+.*\b{function} \(", out, re.M))

        self.assertEqual(frames("sleeper"), 2 * 2, msg=msg)
        self.assertEqual(frames("yielder"), 2, msg=msg)
        self.assertEqual(frames("main"), 2, msg=msg)

    def test_interrupt_stops_a_running_guest_twice(self):
        out, status, log = self._interrupt()
        self.assert_interrupted_twice_and_inspected(out, log)
        self.assertIn("killed", out, msg=log)
        self.assertEqual(status, -signal.SIGKILL, msg=log)

    def test_interrupt_stops_a_strict_run_twice(self):
        present, evidence = exact_branch_counter(self.hermit)
        if not present:
            self.skipTest(
                "hermit run --strict refuses on this host (no exact retired-branch "
                "counter): " + evidence
            )
        out, status, log = self._interrupt(STRICT)
        self.assert_interrupted_twice_and_inspected(out, log)
        self.assertIn("killed", out, msg=log)
        self.assertEqual(status, -signal.SIGKILL, msg=log)


if __name__ == "__main__":
    unittest.main()
