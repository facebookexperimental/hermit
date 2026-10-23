#!/usr/bin/env python3
"""Every `hermit` a QEMU demo starts must inherit the same signal 33 disposition.

Hermit passes the guest's `SigIgn` mask through from the process that started
`hermit` (https://github.com/rrnewton/hermit/issues/3441), and QEMU reads it,
so a difference between two launches shows up in the Hermit logs that demos 5
and 6 compare. See signal_33.settle_signal_33_disposition for the mechanism.

The child script below starts two children the way demos 5 and 6 start
`hermit` (subprocess.Popen, then an output-copier thread), and reports whether
each child inherited signal 33 as ignored. Before anything else, the child sets
its own signal 33 disposition to the starting state a test asks for: ignored,
the state GNU make gives a recipe, or default, the state a shell gives a
command. glibc's sigaction(), and with it Python's signal.signal(), refuses to
change signal 33, so the child makes the rt_sigaction system call directly. The
tests then check that the starting state took effect, so they do not depend on
how the child itself was started.

A C library can install its own signal 33 handler before the child's first line
runs (glibc before 2.34 does, as libpthread starts). Every process on such a
host starts with signal 33 caught, so the child leaves that handler in place
rather than set a state no real launch is in. The settle tests still run there
and still require that no launch inherits signal 33 ignored. Only the control
test, which needs a start with signal 33 ignored, is skipped.
"""

import ast
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path

LIB_DIR = Path(__file__).resolve().parent.parent / "lib"

CHILD_SCRIPT = r"""
import ctypes
import json
import platform
import subprocess
import sys
import threading

sys.path.insert(0, sys.argv[1])
import signal_33

SIG_DFL = 0
SIG_IGN = 1
# The rt_sigaction system call numbers.
SYS_RT_SIGACTION = {"x86_64": 13, "aarch64": 134}


class KernelSigaction(ctypes.Structure):
    # The kernel's struct sigaction: handler, flags, restorer, and a 64-bit
    # signal mask. Only the handler is nonzero here, so a kernel whose
    # structure has no restorer field reads the same request.
    _fields_ = [
        ("handler", ctypes.c_ulong),
        ("flags", ctypes.c_ulong),
        ("restorer", ctypes.c_ulong),
        ("mask", ctypes.c_ulong),
    ]


def set_signal_33(handler):
    machine = platform.machine()
    if machine not in SYS_RT_SIGACTION:
        raise RuntimeError("no rt_sigaction system call number for " + machine)
    libc = ctypes.CDLL(None, use_errno=True)
    libc.syscall.restype = ctypes.c_long
    action = KernelSigaction(handler, 0, 0, 0)
    # The last argument is the kernel's signal set size: 8 bytes, 64 signals.
    result = libc.syscall(
        ctypes.c_long(SYS_RT_SIGACTION[machine]),
        ctypes.c_long(33),
        ctypes.byref(action),
        None,
        ctypes.c_long(8),
    )
    if result != 0:
        raise OSError(ctypes.get_errno(), "rt_sigaction for signal 33 failed")


def signal_33_state(status_text):
    # Returns (ignored, caught). Bit n of SigIgn and SigCgt stands for signal
    # n + 1, so signal 33 is bit 32.
    masks = {}
    for line in status_text.splitlines():
        name, _, value = line.partition(":")
        if name in ("SigIgn", "SigCgt"):
            masks[name] = int(value.split()[0], 16)
    if len(masks) != 2:
        raise RuntimeError("no SigIgn or SigCgt line")
    return bool(masks["SigIgn"] & (1 << 32)), bool(masks["SigCgt"] & (1 << 32))


def own_signal_33_state():
    with open("/proc/self/status") as status:
        return signal_33_state(status.read())


def launch():
    # The same Popen options that demos 5 and 6 use for `hermit`.
    process = subprocess.Popen(
        ["cat", "/proc/self/status"],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        start_new_session=True,
    )
    output = []
    copier = threading.Thread(target=lambda: output.append(process.stdout.read()))
    copier.start()
    process.wait()
    copier.join()
    return signal_33_state(output[0].decode())[0]


start, mode = sys.argv[2], sys.argv[3]
report = {"caught_at_entry": own_signal_33_state()[1]}
# Setting a disposition over a handler the C library already installed would
# make a state that no real launch is in, so start from that handler instead.
if not report["caught_at_entry"]:
    set_signal_33({"ignored": SIG_IGN, "default": SIG_DFL}[start])
report["started_ignored"], report["started_caught"] = own_signal_33_state()
if mode == "settle":
    signal_33.settle_signal_33_disposition()
report["first"] = launch()
report["second"] = launch()
print(json.dumps(report))
"""


def _spawn_child(start, mode):
    """Run CHILD_SCRIPT through os.posix_spawn and return its JSON report."""
    with tempfile.TemporaryDirectory() as scratch:
        script = Path(scratch) / "child.py"
        script.write_text(CHILD_SCRIPT)
        read_end, write_end = os.pipe()
        try:
            pid = os.posix_spawn(
                sys.executable,
                [sys.executable, str(script), str(LIB_DIR), start, mode],
                dict(os.environ, PYTHONDONTWRITEBYTECODE="1"),
                file_actions=[(os.POSIX_SPAWN_DUP2, write_end, 1)],
            )
        finally:
            os.close(write_end)
        with os.fdopen(read_end, "rb") as stream:
            output = stream.read()
        _, status = os.waitpid(pid, 0)
    if os.waitstatus_to_exitcode(status) != 0:
        raise AssertionError("child script failed: {!r}".format(output))
    return json.loads(output)


class Signal33DispositionTest(unittest.TestCase):
    def _report(self, start, mode):
        """Run the child from `start` and check that the starting state took.

        When the C library caught signal 33 before the child's first line ran,
        the child keeps that handler, so it starts caught whatever `start` is.
        """
        report = _spawn_child(start, mode)
        if report["caught_at_entry"]:
            expected = (False, True)
            state = "caught by the C library's own handler"
        else:
            expected = (start == "ignored", False)
            state = start
        self.assertEqual(
            (report["started_ignored"], report["started_caught"]),
            expected,
            "signal 33 did not start {}: {}".format(state, report),
        )
        return report

    def test_without_settling_the_second_launch_differs(self):
        """Control: the flip that settle_signal_33_disposition removes exists here."""
        report = self._report("ignored", "control")
        if report["caught_at_entry"]:
            # Only this control needs a start with signal 33 ignored. The skip
            # is decided by the child's state on entry, before it changes
            # anything or calls the code under test.
            self.skipTest(
                "the C library installed its own signal 33 handler before the "
                "child's first line ran (glibc before 2.34 does this as "
                "libpthread starts), so no launch can inherit signal 33 "
                "ignored and there is no difference between launches to "
                "show: {}".format(report)
            )
        self.assertEqual(
            (report["first"], report["second"]),
            (True, False),
            "expected the launch before the first thread to inherit signal 33 "
            "ignored and the launch after it not to: {}".format(report),
        )

    def test_settling_gives_every_launch_the_same_signal_33_disposition(self):
        """Only signal 33 is checked: glibc never resets signal 32 for us.

        GNU make also leaves signal 32 ignored, and nothing the demo does
        changes that, so a launch through make and a launch from a shell can
        still differ in signal 32 (see the demo 5 and 6 READMEs).
        """
        report = self._report("ignored", "settle")
        self.assertEqual(
            (report["first"], report["second"]),
            (False, False),
            "every launch after settle_signal_33_disposition should run with "
            "signal 33 at its default disposition: {}".format(report),
        )

    def test_a_shell_start_settles_to_the_same_disposition(self):
        """Reset first, as a shell leaves signal 33: the result must not change.

        Demos 5 and 6 can be started through make (signal 33 ignored) or from
        a shell (signal 33 at its default disposition). After settling, every
        launch must see the same disposition whichever way the script started.
        """
        from_shell = self._report("default", "settle")
        from_make = self._report("ignored", "settle")
        self.assertEqual(
            (from_shell["first"], from_shell["second"]),
            (False, False),
            "every launch after settle_signal_33_disposition should run with "
            "signal 33 at its default disposition: {}".format(from_shell),
        )
        self.assertEqual(
            (from_shell["first"], from_shell["second"]),
            (from_make["first"], from_make["second"]),
            "a shell start and a make start should settle to the same "
            "disposition: {} and {}".format(from_shell, from_make),
        )

    def test_qemu_demos_settle_before_anything_else(self):
        """Demos 5 and 6 must settle before main() can start any `hermit`."""
        demos_dir = LIB_DIR.parent
        for script in ("05-qemu-boot/run.py", "06-qemu-resume/run.py"):
            with self.subTest(script=script):
                tree = ast.parse((demos_dir / script).read_text())
                mains = [
                    node
                    for node in tree.body
                    if isinstance(node, ast.FunctionDef) and node.name == "main"
                ]
                self.assertEqual(len(mains), 1, "expected one main() in " + script)
                first = mains[0].body[0]
                self.assertTrue(
                    isinstance(first, ast.Expr)
                    and isinstance(first.value, ast.Call)
                    and isinstance(first.value.func, ast.Name)
                    and first.value.func.id == "settle_signal_33_disposition",
                    "the first statement of main() in {} must be "
                    "settle_signal_33_disposition()".format(script),
                )

    def test_the_helper_stays_out_of_the_guest(self):
        """The guest runs copies of these sources; editing them changes the snapshot."""
        sys.path.insert(0, str(LIB_DIR))
        try:
            import demo_common
        finally:
            sys.path.remove(str(LIB_DIR))
        self.assertNotIn("signal_33.py", demo_common.GUEST_CONTROLLER_SOURCES)
        for name in demo_common.GUEST_CONTROLLER_SOURCES:
            with self.subTest(source=name):
                self.assertNotIn(
                    "settle_signal_33_disposition", (LIB_DIR / name).read_text()
                )


if __name__ == "__main__":
    unittest.main()
