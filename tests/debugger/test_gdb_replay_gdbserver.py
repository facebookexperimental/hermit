# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

"""GDB integration tests against `hermit replay` (deterministic replay).

`hermit replay` starts a gdbserver over the deterministic replay and spawns gdb
attached to it. These tests confirm that a debugger observes the *same* state on
every replay -- including a value (the pid) that would be nondeterministic
outside Hermit -- which is the whole point of record/replay debugging.
"""

import re
import unittest

from harness import (
    DebuggerTestBase,
    EXPECT_A,
    EXPECT_B,
    EXPECT_RESULT,
    EXPECT_SUM,
    pick_free_port,
    record,
    replay_under_gdb,
)


def _pid_from(out: str) -> int | None:
    m = re.search(r"pid\s*=\s*(\d+)", out)
    return int(m.group(1)) if m else None


class GdbReplayGdbserver(DebuggerTestBase):
    require_gdb = True

    @classmethod
    def setUpClass(cls):
        super().setUpClass()
        cls.data_dir = cls.guest.parent / "replay-data"
        cls.recording = record(cls.hermit, cls.guest, cls.data_dir)

    def _replay(self, gdbex):
        return replay_under_gdb(
            self.hermit,
            self.data_dir,
            self.recording,
            gdbex,
            pick_free_port(),
        )

    def test_replay_breakpoint_and_values(self):
        out = self._replay(
            ["break compute", "continue", "info args", "print a + b", "continue"]
        )
        self.assertRegex(out, r"compute \(a=%d, b=%d\)" % (EXPECT_A, EXPECT_B))
        self.assertRegex(out, r"\$\d+ = %d" % EXPECT_SUM)
        self.assertRegex(out, r"result=%d" % EXPECT_RESULT)
        self.assertIn("exited normally", out)

    def test_replay_is_deterministic_across_runs(self):
        # The pid is virtualized by Hermit: it is fixed across replays even
        # though getpid() is nondeterministic on a bare host. Replay twice and
        # require the debugger to observe an identical pid both times.
        gdbex = [
            "break debuggee.c:%d" % _line_of_main_after_pid(),
            "continue",
            "print pid",
            "continue",
        ]
        out1 = self._replay(gdbex)
        out2 = self._replay(gdbex)
        pid1 = _printed_value(out1)
        pid2 = _printed_value(out2)
        self.assertIsNotNone(pid1, msg=out1)
        self.assertEqual(pid1, pid2, "replay pid not deterministic")
        # Also stable via the guest's own stdout line.
        self.assertEqual(_pid_from(out1), _pid_from(out2))


def _printed_value(out: str):
    m = re.search(r"\$\d+ = (-?\d+)", out)
    return int(m.group(1)) if m else None


def _line_of_main_after_pid() -> int:
    """Line of the `int x = 7;` statement -- i.e. just after pid is assigned, so
    `pid` is in scope when we stop there."""
    import harness

    for i, line in enumerate(harness.GUEST_SRC.read_text().splitlines(), start=1):
        if "int x = 7;" in line:
            return i
    raise AssertionError("could not locate pid-in-scope line in guest")


if __name__ == "__main__":
    unittest.main()
