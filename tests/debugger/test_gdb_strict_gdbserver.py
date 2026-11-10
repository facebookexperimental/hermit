# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

"""GDB integration tests against `hermit run --strict --gdbserver`.

A failing strict run (for example a `--chaos` seed) is debugged as the strict
run itself: the guest keeps its isolated network, because Hermit binds the
gdbserver's port in the host's network namespace. These tests attach gdb to a
strict run, stop at a breakpoint, and check that a session that only continues
leaves the run's output and exit status as they are without gdb.

`hermit run --strict` on ptrace refuses where the binary reports no exact
retired-branch counter (`hermit host-capabilities --json`), so these tests skip
there, as the CI runner skips strict cells on such hosts.
"""

import subprocess
import unittest
from pathlib import Path

from harness import (
    DebuggerTestBase,
    EXPECT_A,
    EXPECT_B,
    EXPECT_RESULT,
    exact_branch_counter,
    gdb_batch,
    HermitGdbserver,
    pick_free_port,
)

STRICT = [
    "--strict",
    "--base-env=minimal",
    "--epoch=2026-01-01T00:00:00Z",
]
# A chaos schedule, as when debugging a failing seed.
STRICT_CHAOS = STRICT + ["--chaos", "--seed=17"]


class GdbStrictGdbserver(DebuggerTestBase):
    require_gdb = True

    @classmethod
    def setUpClass(cls):
        super().setUpClass()
        present, evidence = exact_branch_counter(cls.hermit)
        if not present:
            raise unittest.SkipTest(
                "hermit run --strict refuses on this host (no exact retired-branch "
                "counter): " + evidence
            )

    def _debug(self, run_args, gdb_commands, guest=None, guest_args=None):
        """Run the guest under `hermit run RUN_ARGS --gdbserver`, drive gdb
        with GDB_COMMANDS, and return (gdb output, hermit status, guest
        stdout, hermit stderr)."""
        guest = guest or self.guest
        port = pick_free_port()
        with HermitGdbserver(
            self.hermit,
            guest,
            port,
            run_args=run_args,
            guest_args=guest_args,
            separate_stdout=True,
        ) as srv:
            out = gdb_batch(guest, [f"target remote 127.0.0.1:{port}"] + gdb_commands)
            status = srv.wait()
            return out, status, srv.stdout.read_text(), srv.read_log()

    def _run(self, run_args, guest_args=None):
        """The same run without gdb: (status, stdout)."""
        r = subprocess.run(
            [str(self.hermit), "run", *run_args, "--", str(self.guest)]
            + (guest_args or []),
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            stdin=subprocess.DEVNULL,
            timeout=120,
        )
        return r.returncode, r.stdout.decode(errors="replace")

    def test_breakpoint_in_a_strict_chaos_run(self):
        out, status, stdout, log = self._debug(
            STRICT_CHAOS, ["break compute", "continue", "info args", "continue"]
        )
        self.assertIn("Breakpoint 1, compute", out, log)
        self.assertRegex(out, r"a = %d" % EXPECT_A)
        self.assertRegex(out, r"b = %d" % EXPECT_B)
        self.assertIn("exited normally", out, log)
        self.assertEqual(status, 0, log)
        self.assertRegex(stdout, r"^pid=\d+ result=%d\n$" % EXPECT_RESULT)
        self.assertNotIn("host networking", log)

    def test_continue_only_matches_the_strict_run_without_gdb(self):
        expected = self._run(STRICT_CHAOS)
        self.assertEqual(expected[0], 0, expected)
        out, status, stdout, log = self._debug(STRICT_CHAOS, ["continue"])
        self.assertIn("exited normally", out, log)
        self.assertEqual((status, stdout), expected, log)

    def test_strict_guest_keeps_its_isolated_network(self):
        out, status, stdout, log = self._debug(
            STRICT, ["continue"], guest=Path("/bin/ls"), guest_args=["/sys/class/net"]
        )
        self.assertIn("exited normally", out, log)
        self.assertEqual(status, 0, log)
        # The strict guest's own network namespace: loopback only.
        self.assertEqual(stdout, "lo\n", log)


if __name__ == "__main__":
    unittest.main()
