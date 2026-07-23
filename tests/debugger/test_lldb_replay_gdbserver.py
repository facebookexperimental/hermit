# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

"""LLDB (Python API) tests against deterministic replay.

Status: not yet runnable end-to-end; this file documents the gap and is wired so
it *activates automatically* once the prerequisites exist.

Two things block attaching an external LLDB to a Hermit replay today:

1. There is no serve-only replay mode. `hermit replay` starts a gdbserver over
   the replay and immediately spawns its *own* gdb client attached to it; there
   is no `hermit replay --gdbserver` that just serves and waits, so an external
   LLDB has nothing to attach to. (The run-mode path, `hermit run --gdbserver`,
   does serve-and-wait, which is why test_lldb_run_gdbserver.py works.)

2. Even with a serve-only replay mode, stopped-state inspection over LLDB needs
   register-layout negotiation that this environment lacks (see
   test_lldb_run_gdbserver.py for the XML / qRegisterInfo details).

Deterministic-replay-under-a-debugger IS covered today via GDB in
test_gdb_replay_gdbserver.py. When a serve-only replay mode lands, replace the
skip below with a ConnectRemote flow mirroring test_lldb_run_gdbserver.py.
"""

import unittest

from harness import DebuggerTestBase, hermit_bin


def _replay_has_serve_only_mode() -> bool:
    """Detect a future serve-only replay flag (e.g. `hermit replay --gdbserver`)
    so this test starts running automatically once one exists."""
    import subprocess

    hermit = hermit_bin()
    if hermit is None:
        return False
    try:
        out = subprocess.run(
            [str(hermit), "replay", "--help"],
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            timeout=30,
        ).stdout.decode(errors="replace")
    except Exception:  # noqa: BLE001
        return False
    # A bare "--gdbserver" flag (not just "--gdbserver-port") would indicate a
    # serve-only mode.
    return "--gdbserver\n" in out or "--gdbserver " in out


class LldbReplayGdbserver(DebuggerTestBase):
    require_lldb = True

    def test_lldb_attaches_to_replay(self):
        if not _replay_has_serve_only_mode():
            self.skipTest(
                "no serve-only replay mode: `hermit replay` spawns its own gdb "
                "client, so an external LLDB cannot attach. Deterministic replay "
                "under a debugger is covered by test_gdb_replay_gdbserver.py. "
                "Implement this once `hermit replay --gdbserver` (serve-and-wait) "
                "exists."
            )
        # Future implementation goes here (mirror test_lldb_run_gdbserver.py,
        # but against the replay gdbserver, asserting a deterministic pid).
        self.fail("serve-only replay mode detected but test not implemented")


if __name__ == "__main__":
    unittest.main()
