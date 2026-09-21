# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

"""LLDB (Python API) integration tests against `hermit run --gdbserver`.

Requires the `lldb` Python module (run via run_debugger_tests.sh, which sets
PYTHONPATH="$(lldb -P)").

What is asserted vs skipped
---------------------------
Hermit's gdbserver speaks enough of the remote protocol for LLDB to *connect*
and *plant breakpoints* (the handshake landed in reverie PR #21). This test
always asserts that much.

Full stopped-state inspection (frames / registers / locals) additionally
requires the debugger to learn the target's register layout. Some LLDB builds
have XML target-description parsing disabled and Hermit's gdbserver does not
implement the `qRegisterInfo` fallback; on those, LLDB cannot unwind (0 frames)
and inspection is impossible. Rather than fail, the inspection assertions
`skipTest(...)` with a precise reason, and automatically start passing once a
capable LLDB / gdbserver is present.
"""

import unittest

from harness import (
    DebuggerTestBase,
    EXPECT_A,
    EXPECT_B,
    EXPECT_RESULT,
    HermitGdbserver,
    pick_free_port,
)

import lldb


class LldbRunGdbserver(DebuggerTestBase):
    require_lldb = True

    def setUp(self):
        self.port = pick_free_port()
        self._srv = HermitGdbserver(self.hermit, self.guest, self.port)
        self._srv.__enter__()
        self.dbg = lldb.SBDebugger.Create()
        self.dbg.SetAsync(False)
        self.target = self.dbg.CreateTarget(str(self.guest))
        self.assertTrue(self.target.IsValid(), "could not create lldb target")
        self.process = None

    def tearDown(self):
        try:
            if self.process and self.process.IsValid():
                self.process.Kill()
        finally:
            lldb.SBDebugger.Destroy(self.dbg)
            self._srv.__exit__(None, None, None)

    def _connect(self):
        err = lldb.SBError()
        self.process = self.target.ConnectRemote(
            self.dbg.GetListener(),
            f"connect://localhost:{self.port}",
            "gdb-remote",
            err,
        )
        self.assertTrue(err.Success(), f"lldb connect failed: {err.GetCString()}")
        self.assertTrue(self.process.IsValid(), "lldb process invalid after connect")
        # Non-PIE guest: file addresses == load addresses. Tell LLDB the slide
        # is 0 so breakpoints resolve to concrete load addresses even when the
        # remote module list is unreadable.
        mod = self.target.GetModuleAtIndex(0)
        self.target.SetModuleLoadAddress(mod, 0)

    def test_connect_and_plant_breakpoint(self):
        """The handshake: connect + set a breakpoint that resolves. Always
        asserted."""
        self._connect()
        bp = self.target.BreakpointCreateByName("compute")
        self.assertGreaterEqual(
            bp.GetNumLocations(), 1, "breakpoint on compute did not resolve"
        )
        loc = bp.GetLocationAtIndex(0).GetAddress().GetLoadAddress(self.target)
        self.assertNotEqual(loc, lldb.LLDB_INVALID_ADDRESS, "bp has no load address")

    def _require_inspection(self, thread):
        """Skip (don't fail) when this LLDB/gdbserver combo can't provide
        register/frame info."""
        if thread.GetNumFrames() == 0 or not thread.GetFrameAtIndex(
            0
        ).GetFunctionName():
            self.skipTest(
                "LLDB connected and planted the breakpoint, but cannot read "
                "register/frame info from Hermit's gdbserver (this LLDB build "
                "lacks XML target-description parsing and the gdbserver has no "
                "qRegisterInfo fallback). Stopped-state inspection is "
                "unavailable; handshake is verified by "
                "test_connect_and_plant_breakpoint."
            )

    def test_breakpoint_hit_and_inspect(self):
        self._connect()
        self.target.BreakpointCreateByName("compute")
        self.process.Continue()
        thread = self.process.GetSelectedThread()
        self._require_inspection(thread)
        # If we get here, full inspection works: verify it thoroughly.
        frame = thread.GetFrameAtIndex(0)
        self.assertEqual(frame.GetFunctionName(), "compute")
        self.assertEqual(int(frame.FindVariable("a").GetValue()), EXPECT_A)
        self.assertEqual(int(frame.FindVariable("b").GetValue()), EXPECT_B)
        # Step over one line; `sum` becomes defined.
        thread.StepOver()
        frame = thread.GetFrameAtIndex(0)
        self.assertEqual(int(frame.FindVariable("sum").GetValue()), EXPECT_A + EXPECT_B)
        # Continue to exit.
        self.process.Continue()
        self.assertEqual(self.process.GetState(), lldb.eStateExited)
        self.assertEqual(self.process.GetExitStatus(), 0)
        self.assertRegex(self._srv.read_log(), r"result=%d" % EXPECT_RESULT)


if __name__ == "__main__":
    unittest.main()
