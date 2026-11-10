# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

"""GDB against `hermit run --gdbserver` with a guest that spawns processes.

gdb tells the stub in `qSupported` whether it follows forks, vforks and execs,
and expects a stop for each only if it asked. After a vfork it was told of, it
waits for the parent's return from it (`vforkdone`) before it puts its
breakpoints back and before it can handle another fork. The guest is
multithreaded and spawns twice with posix_spawn(), which glibc runs as a vfork,
as the programs that hit this did: with gdb's defaults gdb 17 failed an
internal assertion at the second spawn, and with fork events turned off it
hung after the first.
"""

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

EVENTS_OFF = [
    "set remote fork-event-feature-packet off",
    "set remote vfork-event-feature-packet off",
]
COMMANDS = ["break after_spawns", "continue", "print first + second", "continue"]
STRICT = [
    "--strict",
    "--base-env=minimal",
    "--epoch=2026-01-01T00:00:00Z",
]
# How long hermit may take to end once gdb is done.
HERMIT_END_TIMEOUT = 60


class GdbForkEvents(DebuggerTestBase):
    require_gdb = True

    @classmethod
    def setUpClass(cls):
        super().setUpClass()
        try:
            cls.spawner = compile_guest(
                GUESTS_DIR / "spawner.c", "spawner", ["-pthread"]
            )
        except Exception as e:  # noqa: BLE001
            raise unittest.SkipTest(f"could not compile spawner guest ({e})")

    def _spawn(self, settings, run_args=None):
        """Run the spawner under `hermit run RUN_ARGS --gdbserver` and a gdb
        with `settings`, break after the spawns and continue to the end.
        Returns gdb's output, hermit's exit status and hermit's log."""
        port = pick_free_port()
        with HermitGdbserver(self.hermit, self.spawner, port, run_args=run_args) as srv:
            out = gdb_batch(
                self.spawner,
                # A loaded host can answer slower than gdb's default 2 s.
                ["set remotetimeout 30"]
                + settings
                + [f"target remote 127.0.0.1:{port}"]
                + COMMANDS,
            )
            try:
                status = srv.wait(HERMIT_END_TIMEOUT)
            except subprocess.TimeoutExpired:
                self.fail(
                    f"hermit did not end after gdb did\ngdb:\n{out}\nhermit:\n{srv.read_log()}"
                )
            return out, status, srv.read_log()

    def assert_spawned_and_stopped_after(self, out, status, log, vfork_stops):
        msg = f"gdb:\n{out}\nhermit:\n{log}"
        self.assertNotIn("internal-error", out, msg=msg)
        self.assertEqual(
            out.count("[Detaching after vfork from child process"),
            vfork_stops,
            msg=msg,
        )
        # The breakpoint is in place after the spawns, both of which worked.
        self.assertRegex(
            out, r"Breakpoint 1, after_spawns \(first=0, second=0\)", msg=msg
        )
        self.assertRegex(out, r"\$1 = 0\b", msg=msg)
        self.assertIn("exited normally", out, msg=msg)
        self.assertEqual(status, 0, msg=msg)

    def skip_unless_strict_runs(self):
        present, evidence = exact_branch_counter(self.hermit)
        if not present:
            self.skipTest(
                "hermit run --strict refuses on this host (no exact retired-branch "
                "counter): " + evidence
            )

    def test_spawns_with_gdbs_defaults(self):
        out, status, log = self._spawn([])
        self.assert_spawned_and_stopped_after(out, status, log, vfork_stops=2)

    def test_spawns_with_fork_events_off(self):
        out, status, log = self._spawn(EVENTS_OFF)
        self.assert_spawned_and_stopped_after(out, status, log, vfork_stops=0)

    def test_strict_spawns_with_gdbs_defaults(self):
        self.skip_unless_strict_runs()
        out, status, log = self._spawn([], STRICT)
        self.assert_spawned_and_stopped_after(out, status, log, vfork_stops=2)

    def test_strict_spawns_with_fork_events_off(self):
        self.skip_unless_strict_runs()
        out, status, log = self._spawn(EVENTS_OFF, STRICT)
        self.assert_spawned_and_stopped_after(out, status, log, vfork_stops=0)


if __name__ == "__main__":
    unittest.main()
