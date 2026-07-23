# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

"""GDB integration tests against `hermit run --gdbserver`.

Covers the full debugging surface: breakpoint set + hit, argument/variable
inspection, expression evaluation, step-in / step-over / step-out, register
inspection, and continue-to-exit. Each test starts a fresh Hermit gdbserver so
the tests are independent and order-insensitive.
"""

import re
import unittest

import harness
from harness import (
    DebuggerTestBase,
    EXPECT_A,
    EXPECT_B,
    EXPECT_RESULT,
    EXPECT_SUM,
    gdb_batch,
    HermitGdbserver,
    pick_free_port,
)


class GdbRunGdbserver(DebuggerTestBase):
    require_gdb = True

    def _gdb(self, commands):
        """Start hermit gdbserver, connect gdb with `target remote`, and run
        `commands`."""
        port = pick_free_port()
        with HermitGdbserver(self.hermit, self.guest, port) as srv:
            out = gdb_batch(
                self.guest,
                [f"target remote :{port}"] + commands,
            )
            return out, srv.read_log()

    def test_breakpoint_hit_and_inspect_args(self):
        out, _ = self._gdb(
            ["break compute", "continue", "info args", "print a + b", "kill"]
        )
        self.assertIn("Breakpoint 1", out)
        self.assertRegex(out, r"compute \(a=%d, b=%d\)" % (EXPECT_A, EXPECT_B))
        self.assertRegex(out, r"a = %d" % EXPECT_A)
        self.assertRegex(out, r"b = %d" % EXPECT_B)
        # `print a + b` => "$N = 13"
        self.assertRegex(out, r"\$\d+ = %d" % EXPECT_SUM)

    def test_step_over_and_local_variable(self):
        # Stop at compute, step over the first statement, then `sum` is defined.
        out, _ = self._gdb(
            ["break compute", "continue", "next", "print sum", "kill"]
        )
        self.assertRegex(out, r"\$\d+ = %d" % EXPECT_SUM)

    def test_step_in_from_main(self):
        # Break at the `compute(x, y)` call site in main, then step *into* the
        # callee and verify we entered compute with the expected arguments.
        call_line = _line_of("BP_MAIN")
        out, _ = self._gdb(
            [
                f"break debuggee.c:{call_line}",
                "continue",
                "step",
                "backtrace",
                "kill",
            ]
        )
        self.assertRegex(out, r"compute \(a=%d, b=%d\)" % (EXPECT_A, EXPECT_B))

    def test_step_out_returns_value(self):
        # step-out (`finish`) from compute reports the return value 55.
        # NOTE: `finish` must NOT be followed by `continue` -- that currently
        # panics reverie ("unexpected resume action"), see README. We end with
        # `kill` instead.
        out, _ = self._gdb(
            ["break compute", "continue", "finish", "kill"]
        )
        self.assertRegex(out, r"Value returned is \$\d+ = %d" % EXPECT_RESULT)

    def test_register_inspection(self):
        out, _ = self._gdb(
            ["break compute", "continue", "info registers rip", "kill"]
        )
        # rip should be a concrete hex value inside the text segment.
        self.assertRegex(out, r"rip\s+0x[0-9a-f]+")

    def test_continue_to_exit(self):
        out, log = self._gdb(["break compute", "continue", "continue"])
        self.assertIn("exited normally", out)
        # The guest actually ran under Hermit and produced its output.
        self.assertRegex(log, r"result=%d" % EXPECT_RESULT)


def _line_of(marker: str) -> int:
    """Find the 1-based source line number containing `marker` in the guest."""
    src = (harness.GUEST_SRC).read_text().splitlines()
    for i, line in enumerate(src, start=1):
        if marker in line:
            return i
    raise AssertionError(f"marker {marker!r} not found in guest source")


if __name__ == "__main__":
    unittest.main()
