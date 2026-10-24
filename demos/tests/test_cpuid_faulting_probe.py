#!/usr/bin/env python3
"""Tests for hermit_supports_cpuid_faulting in demos/lib/common.sh.

Demo 4 uses this probe to decide whether to pass --no-virtualize-cpuid to the
runs that `hermit analyze` starts. The probe runs `hermit run -- /bin/true` and
looks for the error Hermit prints when it cannot turn on CPUID interception.
At the Reverie revision Hermit pins, every such error ends with "continuing
without CPUID interception" (reverie-ptrace/src/task.rs); older builds printed
"Unable to intercept CPUID: Underlying hardware does not support CPUID
faulting".

Each test puts a stub `hermit` first on PATH that prints one message on
stderr, sources the real demos/lib/common.sh with the guest build skipped, and
prints what the probe decided. No real Hermit runs. Run directly
(`python3 demos/tests/test_cpuid_faulting_probe.py`) or via
`make -C demos test`.
"""

import os
import subprocess
import tempfile
import unittest
from pathlib import Path

COMMON = Path(__file__).resolve().parent.parent / "lib" / "common.sh"

STUB_HERMIT = """#!/bin/sh
if [ "$1" = "--version" ]; then
  echo "hermit 0.0.0-stub"
  exit 0
fi
printf '%s\\n' "$STUB_STDERR" >&2
exit 0
"""

PROBE = """. "$COMMON"
if hermit_supports_cpuid_faulting; then
  echo PROBE=supported
else
  echo PROBE=unsupported
fi
"""

# Two of the seven messages in reverie-ptrace/src/task.rs at revision
# 3a196cfb9dbb775900a1acb02890b8ca10aedd0c, the pin in Cargo.lock, behind the
# prefix tracing prints at --log=error.
PREFIX = "2026-10-03T00:00:00.000000Z ERROR reverie_ptrace::task: "
GET_CPUID_ENODEV = PREFIX + (
    "CPUID faulting is unavailable: arch_prctl(ARCH_GET_CPUID) returned ENODEV. "
    "On AMD hosts, use Linux 6.17+ upstream or a kernel with CPUID faulting "
    "backported; continuing without CPUID interception"
)
SET_CPUID_ENODEV = PREFIX + (
    "ARCH_GET_CPUID reported a valid state, but ARCH_SET_CPUID returned ENODEV. "
    "The kernel exposes CPUID state without hardware faulting support. On AMD "
    "hosts, use Linux 6.17+ upstream or a kernel with CPUID faulting backported; "
    "continuing without CPUID interception"
)
OLDER = PREFIX + "Unable to intercept CPUID: Underlying hardware does not support CPUID faulting"


class CpuidFaultingProbeTest(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.directory = Path(directory.name)
        self.bin = self.directory / "bin"
        self.bin.mkdir()
        hermit = self.bin / "hermit"
        hermit.write_text(STUB_HERMIT)
        hermit.chmod(0o755)
        # common.sh requires cargo and cc on PATH; DEMO_SKIP_BUILD=1 keeps it
        # from running them.
        for name in ("cargo", "cc"):
            tool = self.bin / name
            tool.write_text("#!/bin/sh\necho 'unexpected {} call' >&2\nexit 99\n".format(name))
            tool.chmod(0o755)
        self.guest = self.directory / "guest"
        self.guest.write_text("#!/bin/sh\nexit 99\n")
        self.guest.chmod(0o755)
        self.repo = self.directory / "repo"
        self.repo.mkdir()
        (self.repo / "Cargo.toml").write_text("")

    def _probe(self, stderr_text):
        env = dict(os.environ)
        env.update(
            PATH="{}:{}".format(self.bin, os.environ.get("PATH", "/usr/bin:/bin")),
            COMMON=str(COMMON),
            HERMIT_REPO=str(self.repo),
            DEMO_SKIP_BUILD="1",
            HELLO_RACE=str(self.guest),
            HEAP_PTRS=str(self.guest),
            DEMO_TMP=str(self.directory / "demo-tmp"),
            DEMO_ARTIFACTS=str(self.directory / "artifacts"),
            STUB_STDERR=stderr_text,
        )
        result = subprocess.run(
            ["bash", "-c", PROBE],
            env=env,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            universal_newlines=True,
            timeout=60,
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        decisions = [line for line in result.stdout.splitlines() if line.startswith("PROBE=")]
        self.assertEqual(len(decisions), 1, result.stdout + result.stderr)
        return decisions[0]

    def test_the_pinned_get_cpuid_enodev_error_means_no_faulting(self):
        self.assertEqual(self._probe(GET_CPUID_ENODEV), "PROBE=unsupported")

    def test_the_pinned_set_cpuid_enodev_error_means_no_faulting(self):
        self.assertEqual(self._probe(SET_CPUID_ENODEV), "PROBE=unsupported")

    def test_the_older_error_still_means_no_faulting(self):
        self.assertEqual(self._probe(OLDER), "PROBE=unsupported")

    def test_a_clean_run_means_faulting(self):
        self.assertEqual(self._probe(""), "PROBE=supported")

    def test_an_unrelated_error_means_faulting(self):
        self.assertEqual(self._probe(PREFIX + "something else went wrong"), "PROBE=supported")


if __name__ == "__main__":
    unittest.main()
