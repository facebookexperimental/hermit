#!/usr/bin/env python3
"""Tests for demo 8's result checks and for its seed calibration.

Both scripts decide PASS or FAIL from what `hermit run --chaos` returns: the
exit status (134 is the ASAN abort, 124 the timeout, 125 a wrapper failure,
122 Hermit's refusal after a late performance-counter interrupt), whether the
output holds a complete AddressSanitizer report, and whether two runs of one
seed print the same complete report. Each test puts a stub `hermit` first on
PATH that returns one scripted outcome, runs the real script, and checks that
the script accepts or refuses it for the stated reason. No real Hermit and no
btrfs-convert build are involved.

CalibrationControlsTest also builds a tiny C program with a real heap
use-after-free under AddressSanitizer, so the "a crash was found" path is fed
a genuine ASAN abort rather than hand-written text. It fails, rather than
skips, when the host cannot build it: without that program the tests could not
show that calibration ever accepts a seed. The stub runs that program on a
seed's first crash and prints the same output again on the seed's replay, as
two Hermit runs of one seed do; run natively, the program prints a different
PID and different addresses every time. CalibrationReportComparisonTest needs
no compiler: its planted program prints a saved report.

Hermit gives the program it runs a private /tmp. TmpBindTest and
CalibrationTmpBindTest check that both scripts bind the converter's directory
and its image into it when their paths begin with /tmp/, refuse a path under
/tmp that they cannot bind, and add nothing for paths outside /tmp. The stubs
record each run's arguments, and the tests compare them exactly.
"""

import hashlib
import os
import re
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

DEMO_DIR = Path(__file__).resolve().parent.parent
DEMO8 = DEMO_DIR / "08-btrfs-convert-uaf"
RUN = DEMO8 / "run.sh"
PREPARE = DEMO8 / "prepare-assets.sh"
FIXTURES = DEMO8 / "fixtures"

# Must equal PREP_VERSION and BTRFS_COMMIT in prepare-assets.sh. If they drift,
# the stamp written below no longer matches, prepare-assets.sh takes its build
# path instead of the calibration path, and every calibration test fails on
# the "Searching for a crashing seed" check.
PREP_VERSION = 2
BTRFS_COMMIT = "4ab0e80be9e3bb1db2e6038e6d4316d35fb7ba8b"

# ASAN's complete report from a crashing chaos run of the reference build's
# buggy btrfs-convert, every line from the ERROR line through the closing
# ABORTING line. Both scripts compare exactly this between two runs of a seed.
FULL_REPORT = """\
==3==ERROR: AddressSanitizer: heap-use-after-free on address 0x606000000210 at pc 0x0000004e68e1 bp 0x7ffff3ffeaf0 sp 0x7ffff3ffeae0
READ of size 8 at 0x606000000210 thread T1
    #0 0x4e68e0 in task_period_wait common/task-utils.c:154
    #1 0x41215a in print_copied_inodes convert/main.c:169
    #2 0x7ffff708b568 in start_thread (/lib64/libc.so.6+0x8b568)
    #3 0x7ffff7110a7f in clone3 (/lib64/libc.so.6+0x110a7f)

0x606000000210 is located 16 bytes inside of 56-byte region [0x606000000200,0x606000000238)
freed by thread T0 here:
    #0 0x7ffff74b46b7 in free (/lib64/libasan.so.6+0xb46b7)
    #1 0x4e65a6 in task_deinit common/task-utils.c:100
    #2 0x418691 in do_convert convert/main.c:1354
    #3 0x418691 in main convert/main.c:2116
    #4 0x7ffff702a60f in __libc_start_call_main (/lib64/libc.so.6+0x2a60f)

previously allocated by thread T0 here:
    #0 0x7ffff74b4bd7 in calloc (/lib64/libasan.so.6+0xb4bd7)
    #1 0x4e621a in task_init common/task-utils.c:29
    #2 0x4185b1 in do_convert convert/main.c:1343
    #3 0x4185b1 in main convert/main.c:2116
    #4 0x7ffff702a60f in __libc_start_call_main (/lib64/libc.so.6+0x2a60f)

Thread T1 created by T0 here:
    #0 0x7ffff74587d5 in pthread_create (/lib64/libasan.so.6+0x587d5)
    #1 0x4e6346 in task_start common/task-utils.c:56
    #2 0x4185e5 in do_convert convert/main.c:1345
    #3 0x4185e5 in main convert/main.c:2116
    #4 0x7ffff702a60f in __libc_start_call_main (/lib64/libc.so.6+0x2a60f)

SUMMARY: AddressSanitizer: heap-use-after-free common/task-utils.c:154 in task_period_wait
Shadow bytes around the buggy address:
  0x0c0c7fff7ff0: 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
  0x0c0c7fff8000: fa fa fa fa fd fd fd fd fd fd fd fa fa fa fa fa
  0x0c0c7fff8010: 00 00 00 00 00 00 06 fa fa fa fa fa 00 00 00 00
  0x0c0c7fff8020: 00 00 00 00 fa fa fa fa 00 00 00 00 00 00 00 00
  0x0c0c7fff8030: fa fa fa fa 00 00 00 00 00 00 00 00 fa fa fa fa
=>0x0c0c7fff8040: fd fd[fd]fd fd fd fd fa fa fa fa fa 00 00 00 00
  0x0c0c7fff8050: 00 00 00 00 fa fa fa fa fd fd fd fd fd fd fd fa
  0x0c0c7fff8060: fa fa fa fa fd fd fd fd fd fd fd fa fa fa fa fa
  0x0c0c7fff8070: fd fd fd fd fd fd fd fa fa fa fa fa fd fd fd fd
  0x0c0c7fff8080: fd fd fd fa fa fa fa fa fd fd fd fd fd fd fd fa
  0x0c0c7fff8090: fa fa fa fa fd fd fd fd fd fd fd fa fa fa fa fa
Shadow byte legend (one shadow byte represents 8 application bytes):
  Addressable:           00
  Partially addressable: 01 02 03 04 05 06 07\x20
  Heap left redzone:       fa
  Freed heap region:       fd
  Stack left redzone:      f1
  Stack mid redzone:       f2
  Stack right redzone:     f3
  Stack after return:      f5
  Stack use after scope:   f8
  Global redzone:          f9
  Global init order:       f6
  Poisoned by user:        f7
  Container overflow:      fc
  Array cookie:            ac
  Intra object redzone:    bb
  ASan internal:           fe
  Left alloca redzone:     ca
  Right alloca redzone:    cb
  Shadow gap:              cc
==3==ABORTING
"""

# What the guest printed before the report, ending with ASAN's separator line.
REPORT_PREAMBLE = """\
btrfs-convert from btrfs-progs v7.1
Create btrfs metadata
Copy inodes [o] [         0/       111]\r
=================================================================
"""

# Hermit's two log events after the abort. Their order is not fixed.
EXIT_EVENTS = (
    "2026-10-01T21:20:16.078060Z ERROR reverie_ptrace::lifecycle: guest terminated "
    "by signal tid=5 pid=3 signal=SIGABRT core_dumped=true\n"
    "2026-10-01T21:20:16.081167Z ERROR reverie_ptrace::lifecycle: guest terminated "
    "by signal tid=3 pid=3 signal=SIGABRT core_dumped=true\n"
)

# A crashing chaos run's whole output.
UAF_REPORT = REPORT_PREAMBLE + FULL_REPORT + EXIT_EVENTS

# Another run of the same seed whose report is the same but whose other lines are
# not: a Hermit log line inside the report, the exit events in the other order
# with other timestamps, and the per-run summary bin/safehermit appends when a
# run goes through it. None of these lines is part of the guest's report.
SAME_REPORT_OTHER_LOGS = (
    REPORT_PREAMBLE
    + FULL_REPORT.replace(
        "freed by thread T0 here:\n",
        "freed by thread T0 here:\n"
        "2026-10-01T21:22:03.517204Z ERROR detcore::tool_global: example event\n",
    )
    + "2026-10-01T21:22:04.045954Z ERROR reverie_ptrace::lifecycle: guest terminated "
    "by signal tid=3 pid=3 signal=SIGABRT core_dumped=true\n"
    "2026-10-01T21:22:04.046018Z ERROR reverie_ptrace::lifecycle: guest terminated "
    "by signal tid=5 pid=3 signal=SIGABRT core_dumped=true\n"
    "safehermit: elapsed_secs=2\n"
    "safehermit: exec_main_pid=3954865\n"
    "safehermit: exit_code=134\n"
)


def _vary(text, old, new):
    """`text` with the one line `old` replaced by `new`; refuses a no-op edit."""
    if text.count(old) != 1:
        raise AssertionError("expected exactly one {!r} in the fixture".format(old))
    return text.replace(old, new)


# The same run with one line of the shadow-memory map changed, as a different
# image path length changed it in README.md's measurements.
SHADOW_DIFFERENT = _vary(
    UAF_REPORT,
    "  0x0c0c7fff8020: 00 00 00 00 fa fa fa fa 00 00 00 00 00 00 00 00\n",
    "  0x0c0c7fff8020: 00 00 00 01 fa fa fa fa 00 00 00 00 00 00 00 01\n",
)

# The same run with a different frame in the stack that freed the memory.
FREE_STACK_DIFFERENT = _vary(
    UAF_REPORT,
    "    #2 0x418691 in do_convert convert/main.c:1354\n",
    "    #2 0x418702 in do_convert convert/main.c:1360\n",
)

# The same run without ASAN's closing ABORTING line. It still has the SUMMARY.
NO_CLOSING_LINE = _vary(UAF_REPORT, "==3==ABORTING\n", "")

# The report cut off before its SUMMARY line.
PARTIAL_REPORT = """\
==1234==ERROR: AddressSanitizer: heap-use-after-free on address 0x606000000210
    #0 0x4e69f6 in task_period_wait common/task-utils.c:154
"""

# A complete report at a different heap address, for the replay comparison.
OTHER_REPORT = UAF_REPORT.replace("0x606000000210", "0x606000000330")

# Stands in for `hermit ... run --chaos ... -- <btrfs-convert> <image>` in
# run.sh. The buggy binary's outcome comes from DEMO08_TEST_BUGGY_MODE and can
# differ between Step 2 (first buggy run) and Step 4 (second buggy run); the
# fixed binary's outcome comes from DEMO08_TEST_FIXED_MODE.
RUN_STUB = r"""#!/usr/bin/env bash
if [ "${1:-}" = --version ]; then
  echo 'hermit 0.2.0 (2026-09-30, g0123456789ab)'
  exit 0
fi
printf '%s\n' "$*" >>"$DEMO08_TEST_ARGS_FILE"
# What Reverie and Hermit print when a precise-timer interrupt arrives past its
# target: Hermit refuses the run with HERMIT_POLICY_REFUSAL_EXIT (122).
skid_refusal() {
  echo 'HERMIT_SKID_OVERSHOOT rcb_actual=1008 rcb_target=1000 skid_margin=32 overshoot=8' >&2
  echo 'HERMIT_POLICY_REFUSAL class=policy-refusal cause=skid-overshoot count=1' >&2
  exit 122
}
conv=""
seen=0
for a in "$@"; do
  if [ "$seen" = 1 ]; then conv="$a"; break; fi
  [ "$a" = "--" ] && seen=1
done
case "$conv" in
  */buggy/btrfs-convert)
    count=0
    [ ! -r "$DEMO08_TEST_COUNT_FILE" ] || count=$(cat "$DEMO08_TEST_COUNT_FILE")
    count=$((count + 1))
    printf '%s\n' "$count" >"$DEMO08_TEST_COUNT_FILE"
    case "$DEMO08_TEST_BUGGY_MODE" in
      complete-abort) cat "$DEMO08_TEST_UAF_FILE"; exit 134 ;;
      partial-rc0) cat "$DEMO08_TEST_PARTIAL_FILE"; exit 0 ;;
      partial-rc124) cat "$DEMO08_TEST_PARTIAL_FILE"; exit 124 ;;
      truncated-abort) cat "$DEMO08_TEST_PARTIAL_FILE"; exit 134 ;;
      skid-refusal) skid_refusal ;;
      replay-partial-rc0|replay-partial-rc124|replay-truncated-abort|replay-different|replay-skid-refusal|replay-custom)
        if [ "$count" -eq 1 ]; then cat "$DEMO08_TEST_UAF_FILE"; exit 134; fi
        case "$DEMO08_TEST_BUGGY_MODE" in
          replay-partial-rc0) cat "$DEMO08_TEST_PARTIAL_FILE"; exit 0 ;;
          replay-partial-rc124) cat "$DEMO08_TEST_PARTIAL_FILE"; exit 124 ;;
          replay-truncated-abort) cat "$DEMO08_TEST_PARTIAL_FILE"; exit 134 ;;
          replay-different) cat "$DEMO08_TEST_OTHER_FILE"; exit 134 ;;
          replay-skid-refusal) skid_refusal ;;
          # The ASAN abort with whatever output the test wrote to the file.
          replay-custom) cat "$DEMO08_TEST_REPLAY_FILE"; exit 134 ;;
        esac ;;
      *) echo "stub: unknown DEMO08_TEST_BUGGY_MODE" >&2; exit 9 ;;
    esac ;;
  */fixed/btrfs-convert)
    case "$DEMO08_TEST_FIXED_MODE" in
      clean) exit 0 ;;
      exit-zero-uaf) cat "$DEMO08_TEST_UAF_FILE"; exit 0 ;;
      timeout) echo "conversion still running"; exit 124 ;;
      wrapper-failure) echo "wrapper: limit exceeded" >&2; exit 125 ;;
      regression-uaf) cat "$DEMO08_TEST_UAF_FILE"; exit 134 ;;
      skid-refusal) skid_refusal ;;
      *) echo "stub: unknown DEMO08_TEST_FIXED_MODE" >&2; exit 9 ;;
    esac ;;
  *) echo "stub: unexpected btrfs-convert path: $conv" >&2; exit 9 ;;
esac
"""

# Stands in for the calibration's `hermit ... --sched-seed N ... -- <conv> <img>`.
# The outcome depends on DEMO08_TEST_MODE, the seed, the variant, and how many
# times this (variant, seed) pair has run, so a mode can answer differently on
# a seed's first run and on its confirmation replay.
CALIBRATION_STUB = r"""#!/usr/bin/env bash
set -euo pipefail
if [ "${1:-}" = --version ]; then
  echo 'hermit 0.2.0 (2026-09-30, g0123456789ab)'
  exit 0
fi
[ -z "${DEMO08_TEST_ARGS_FILE:-}" ] || printf '%s\n' "$*" >>"$DEMO08_TEST_ARGS_FILE"
seed=""
conv=""
previous=""
seen=0
for a in "$@"; do
  if [ "$seen" = 1 ]; then conv="$a"; break; fi
  [ "$previous" != --sched-seed ] || seed="$a"
  [ "$a" != "--" ] || seen=1
  previous="$a"
done
case "$conv" in
  */buggy/btrfs-convert) variant=buggy ;;
  */fixed/btrfs-convert) variant=fixed ;;
  *) echo "stub: unexpected btrfs-convert path: $conv" >&2; exit 9 ;;
esac
[ -n "$seed" ] || { echo "stub: no --sched-seed" >&2; exit 9; }

mkdir -p "$DEMO08_TEST_COUNT_DIR"
counter="$DEMO08_TEST_COUNT_DIR/$variant-$seed"
count=1
[ ! -r "$counter" ] || count=$(($(cat "$counter") + 1))
printf '%s\n' "$count" >"$counter"

engage() { printf 'Copy inodes [o] [         0/         1]\r\n'; }
# Run the planted program on this (variant, seed) pair's first crash, and print
# that crash's output again, with its exit status, on every later one. Under
# Hermit two runs of one seed print the same report: the guest's PID is
# virtual and its addresses do not move. Run natively, the planted program
# prints a different PID and different addresses every time.
abort_with_uaf() {
  local saved="$DEMO08_TEST_COUNT_DIR/uaf-$variant-$seed" rc=0
  if [ ! -e "$saved.rc" ]; then
    ASAN_OPTIONS=detect_leaks=0:abort_on_error=1 "${DEMO08_TEST_UAF_BIN:?}" \
      >"$saved.out" 2>&1 || rc=$?
    printf '%s\n' "$rc" >"$saved.rc"
  fi
  cat "$saved.out"
  return "$(cat "$saved.rc")"
}
partial_report() {
  echo '==123==ERROR: AddressSanitizer: heap-use-after-free on address 0x606000000210'
  echo 'READ of size 8 at 0x606000000210 thread T1'
}
# Hermit's refusal after a late performance-counter interrupt (exit 122).
refusal() {
  echo 'HERMIT_SKID_OVERSHOOT rcb_actual=1008 rcb_target=1000 skid_margin=32 overshoot=8' >&2
  echo 'HERMIT_POLICY_REFUSAL class=policy-refusal cause=skid-overshoot count=1' >&2
  exit 122
}

case "${DEMO08_TEST_MODE:?}" in
  refused-once)
    # Each (variant, seed) pair is refused on its first run only; after that
    # the buggy variant crashes on DEMO08_TEST_UAF_SEED and the fixed one is
    # clean.
    if [ "$count" -eq 1 ]; then refusal; fi
    engage
    if [ "$variant" = buggy ] && [ "$seed" = "${DEMO08_TEST_UAF_SEED:?}" ]; then
      abort_with_uaf
    fi
    echo 'Conversion complete' ;;
  always-refused)
    refusal ;;
  replay-refused)
    engage
    if [ "$variant" = fixed ]; then echo 'Conversion complete'; exit 0; fi
    if [ "$count" -eq 1 ]; then abort_with_uaf; fi
    refusal ;;
  fixed-refused-uaf)
    engage
    if [ "$variant" = buggy ]; then abort_with_uaf; fi
    abort_with_uaf || true
    refusal ;;
  fixed-refused)
    engage
    if [ "$variant" = buggy ]; then abort_with_uaf; fi
    refusal ;;
  no-engagement)
    echo 'exited before the progress thread started' ;;
  engaged-no-hit)
    engage; echo 'Conversion complete' ;;
  planted-uaf)
    engage
    if [ "$variant" = buggy ] && [ "$seed" = "${DEMO08_TEST_UAF_SEED:?}" ]; then
      abort_with_uaf
    fi
    echo 'Conversion complete' ;;
  replay-timeout)
    engage
    if [ "$variant" = fixed ]; then echo 'Conversion complete'; exit 0; fi
    if [ "$count" -eq 1 ]; then abort_with_uaf; fi
    partial_report; exit 124 ;;
  replay-clean)
    engage
    if [ "$variant" = buggy ] && [ "$count" -eq 1 ]; then abort_with_uaf; fi
    echo 'Conversion complete' ;;
  replay-report)
    # A seed's first buggy run crashes with the planted program's report; every
    # later buggy run prints DEMO08_TEST_REPLAY_REPORT and aborts. The fixed
    # variant is clean.
    engage
    if [ "$variant" = fixed ]; then echo 'Conversion complete'; exit 0; fi
    if [ "$count" -eq 1 ]; then abort_with_uaf; fi
    cat "${DEMO08_TEST_REPLAY_REPORT:?}"; exit 134 ;;
  fixed-timeout)
    engage
    if [ "$variant" = buggy ]; then abort_with_uaf; fi
    echo 'Conversion incomplete'; exit 124 ;;
  fixed-uaf)
    engage; abort_with_uaf ;;
  uaf-no-engagement)
    abort_with_uaf ;;
  partial-uaf-rc0)
    engage; partial_report; exit 0 ;;
  partial-uaf-rc124)
    engage; partial_report; exit 124 ;;
  complete-uaf-rc0)
    # A complete report from a guest that did not abort, which is what a
    # binary built without abort_on_error prints.
    engage
    if [ "$variant" = fixed ]; then echo 'Conversion complete'; exit 0; fi
    partial_report
    echo 'SUMMARY: AddressSanitizer: heap-use-after-free common/task-utils.c:154 in task_period_wait'
    exit 0 ;;
  runner-failure)
    echo 'could not start the guest' >&2; exit 127 ;;
  runner-failure-with-signatures)
    engage; abort_with_uaf || true; exit 127 ;;
  *)
    echo "stub: unknown DEMO08_TEST_MODE" >&2; exit 9 ;;
esac
"""

PLANTED_UAF_C = r"""#include <stdint.h>
#include <stdlib.h>

int main(void) {
  volatile uint8_t *value = malloc(1);
  if (value == NULL)
    return 2;
  *value = 7;
  free((void *)value);
  return *value;
}
"""


def _write_executable(path, text):
    path.write_text(text)
    path.chmod(0o755)


def _base_environment(stub_dir):
    """The caller's environment without DEMO08_* settings, stub first on PATH."""
    environment = {
        key: value
        for key, value in os.environ.items()
        if not key.startswith("DEMO08_")
    }
    environment["PATH"] = "{}:{}".format(stub_dir, environment.get("PATH", ""))
    environment["LC_ALL"] = "C"
    return environment


def _make_assets(assets):
    """Placeholder assets: both binaries exit 0 when run natively."""
    for variant in ("buggy", "fixed"):
        (assets / variant).mkdir(parents=True)
        _write_executable(
            assets / variant / "btrfs-convert", "#!/usr/bin/env bash\nexit 0\n"
        )
    (assets / "pop-tiny.img").write_bytes(b"")


# The flags of every chaos run in run.sh and prepare-assets.sh. A checkout
# outside /tmp must keep getting exactly this command line, which is what its
# recorded seed was found with.
CHAOS_FLAGS = (
    "--log=error run --chaos --sched-seed {seed} --no-virtualize-cpuid "
    "--base-env=minimal --epoch=2026-01-01T00:00:00Z"
)


def _chaos_args(assets, artifacts, variant, seed, binds=()):
    """The recorded argv of one chaos run: the flags, `binds`, then the command."""
    return " ".join(
        [CHAOS_FLAGS.format(seed=seed), *binds, "--"]
        + [
            "{}/{}/btrfs-convert".format(assets, variant),
            "{}/chaos-{}.img".format(artifacts, variant),
        ]
    )


def _tmp_binds(assets, artifacts, variant):
    """The --bind options a chaos run gets for its converter and image.

    Each is bound only when its path begins with /tmp/, where Hermit would hide
    it from the converter: the converter through its directory, the image by
    itself.
    """
    binds = []
    if str(assets).startswith("/tmp/"):
        binds += ["--bind", "{}/{}".format(assets, variant)]
    if str(artifacts).startswith("/tmp/"):
        binds += ["--bind", "{}/chaos-{}.img".format(artifacts, variant)]
    return binds


def _is_under_tmp(path):
    return path == "/tmp" or path.startswith("/tmp/")


def _temporary_root(test, under_tmp):
    """A new directory under /tmp, or outside it, removed after `test`.

    The parent is the system temporary directory when it is on the requested
    side of /tmp, so a run with TMPDIR set stays inside it; otherwise /tmp, or
    /var/tmp for a directory outside /tmp. Parents are resolved first, so a
    symbolic link in TMPDIR cannot put the directory on the wrong side. The
    test is skipped if neither parent is usable.
    """
    for parent in (tempfile.gettempdir(), "/tmp" if under_tmp else "/var/tmp"):
        resolved = os.path.realpath(parent)
        if _is_under_tmp(resolved) != under_tmp:
            continue
        if not os.access(resolved, os.W_OK | os.X_OK):
            continue
        holder = tempfile.TemporaryDirectory(dir=resolved, prefix="demo08-test-")
        test.addCleanup(holder.cleanup)
        return Path(holder.name)
    test.skipTest(
        "no writable directory {} /tmp".format("under" if under_tmp else "outside")
    )


def _sha256_hex(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def fixture_source_digest():
    """The fixture-src digest that prepare-assets.sh puts in its cache stamp.

    It mirrors fixture_source_identity: sha256sum of the patch, then one
    "<relative name> <sha256sum>" line per fixture file in byte order, hashed
    together.
    """
    lines = ["{}  -\n".format(_sha256_hex(FIXTURES / "convert-main-v7.1.patch"))]
    relatives = []
    for directory, _subdirs, files in os.walk(FIXTURES):
        for name in files:
            path = Path(directory) / name
            if path.is_file() and not path.is_symlink():
                relatives.append(str(path.relative_to(FIXTURES)))
    for relative in sorted(relatives, key=lambda name: name.encode()):
        lines.append("{} {}  -\n".format(relative, _sha256_hex(FIXTURES / relative)))
    return hashlib.sha256("".join(lines).encode()).hexdigest()


class DemoRunControlsTest(unittest.TestCase):
    """run.sh must pass only on the full result: crash, clean fix, same crash."""

    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.tmp = Path(self._tmp.name)
        self.addCleanup(self._tmp.cleanup)
        self.stub_dir = self.tmp / "bin"
        self.stub_dir.mkdir()
        _write_executable(self.stub_dir / "hermit", RUN_STUB)
        self.assets = self.tmp / "assets"
        _make_assets(self.assets)
        self.artifacts = self.tmp / "artifacts"
        for name, text in (
            ("uaf.txt", UAF_REPORT),
            ("partial.txt", PARTIAL_REPORT),
            ("other.txt", OTHER_REPORT),
        ):
            (self.tmp / name).write_text(text)

    def _run(self, fixed_mode="clean", buggy_mode="complete-abort", **overrides):
        environment = _base_environment(self.stub_dir)
        environment.update(
            {
                "DEMO08_DIR": str(self.assets),
                "DEMO08_ARTIFACTS": str(self.artifacts),
                "DEMO08_CRASH_SEED": "7",
                "DEMO08_TIMEOUT": "30",
                "DEMO08_REQUIRE_ASSETS": "1",
                "DEMO08_TEST_FIXED_MODE": fixed_mode,
                "DEMO08_TEST_BUGGY_MODE": buggy_mode,
                "DEMO08_TEST_COUNT_FILE": str(self.tmp / "buggy-count"),
                "DEMO08_TEST_ARGS_FILE": str(self.tmp / "hermit-args"),
                "DEMO08_TEST_UAF_FILE": str(self.tmp / "uaf.txt"),
                "DEMO08_TEST_PARTIAL_FILE": str(self.tmp / "partial.txt"),
                "DEMO08_TEST_OTHER_FILE": str(self.tmp / "other.txt"),
                "DEMO08_TEST_REPLAY_FILE": str(self.tmp / "replay.txt"),
            }
        )
        for key, value in overrides.items():
            if value is None:
                environment.pop(key, None)
            else:
                environment[key] = value
        return subprocess.run(
            [str(RUN)],
            env=environment,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            timeout=120,
        )

    def _assert_refused(self, result, reason):
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn(reason, result.stdout)
        self.assertNotIn("SUCCESS ===", result.stdout)

    def test_the_full_result_passes(self):
        """Positive control: without it, a script that refuses everything passes."""
        result = self._run()
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn(
            "=== Demo 8: btrfs-convert Use-After-Free: SUCCESS ===", result.stdout
        )
        lines = len(FULL_REPORT.splitlines())
        self.assertEqual(lines, 63)
        self.assertIn(
            "replay: ASAN report byte-identical: all {} lines from the ERROR line "
            "through ABORTING".format(lines),
            result.stdout,
        )
        self.assertIn(
            "chaos buggy: reproduced the use-after-free; the complete {}-line "
            "ASAN report is in".format(lines),
            result.stdout,
        )
        # Steps 2 and 4 each save the complete report and nothing else: not the
        # program's output before it, and not Hermit's exit events after it.
        for name in ("asan-report.txt", "asan-report-replay.txt"):
            self.assertEqual((self.artifacts / name).read_text(), FULL_REPORT, name)
        # Every chaos run uses the documented command line and the same seed,
        # plus a --bind for each path under /tmp, which is where the system
        # temporary directory usually puts this test's files. The tests in
        # TmpBindTest pin both cases.
        self.assertEqual(
            (self.tmp / "hermit-args").read_text().splitlines(),
            [
                _chaos_args(
                    self.assets,
                    self.artifacts,
                    variant,
                    7,
                    _tmp_binds(self.assets, self.artifacts, variant),
                )
                for variant in ("buggy", "fixed", "buggy")
            ],
        )

    def _native_buggy_prints(self, report_name, status):
        """Make the native buggy binary print one of the reports and exit."""
        _write_executable(
            self.assets / "buggy" / "btrfs-convert",
            "#!/usr/bin/env bash\ncat {}\nexit {}\n".format(
                self.tmp / report_name, status
            ),
        )

    def test_a_native_run_with_no_report_says_so(self):
        result = self._run()
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn(
            "native buggy: rc=0 with no use-after-free report", result.stdout
        )
        self.assertIn("the native run showed no use-after-free;", result.stdout)

    def test_a_native_partial_report_is_reported_not_called_clean(self):
        """A native run can print half a report and still exit 0."""
        self._native_buggy_prints("partial.txt", 0)
        result = self._run()
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn(
            "native buggy: rc=0; ASAN started a use-after-free report, but the "
            "process exited before the report's SUMMARY:",
            result.stdout,
        )
        self.assertIn(
            "the native run started an ASAN report but exited rc=0 before it "
            "completed;",
            result.stdout,
        )
        self.assertNotIn("no use-after-free report", result.stdout)
        self.assertNotIn("showed no use-after-free", result.stdout)

    def test_a_native_complete_report_is_reported_as_a_crash(self):
        self._native_buggy_prints("uaf.txt", 134)
        result = self._run()
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn(
            "native buggy: rc=134 with a complete ASAN use-after-free report",
            result.stdout,
        )
        self.assertIn("the native run also crashed this time (rc=134);", result.stdout)
        self.assertNotIn("showed no use-after-free", result.stdout)

    def test_a_fixed_run_that_exits_0_with_a_uaf_report_is_refused(self):
        self._assert_refused(
            self._run(fixed_mode="exit-zero-uaf"),
            "regression: the fixed variant reported a use-after-free",
        )

    def test_a_fixed_run_that_crashes_with_a_uaf_is_refused(self):
        self._assert_refused(
            self._run(fixed_mode="regression-uaf"),
            "regression: the fixed variant reported a use-after-free",
        )

    def test_a_fixed_run_cut_off_by_the_timeout_is_refused(self):
        self._assert_refused(
            self._run(fixed_mode="timeout"),
            "rc=124 is the 30s timeout: the control was cut off",
        )

    def test_a_fixed_run_stopped_by_a_wrapper_is_refused(self):
        self._assert_refused(
            self._run(fixed_mode="wrapper-failure"),
            "rc=125 is a wrapper failure, not a guest result",
        )

    def test_a_fixed_run_refused_after_a_skid_overshoot_is_refused(self):
        self._assert_refused(
            self._run(fixed_mode="skid-refusal"),
            "rc=122 means Hermit refused the run",
        )

    def test_a_buggy_run_refused_after_a_skid_overshoot_is_explained(self):
        result = self._run(buggy_mode="skid-refusal")
        self._assert_refused(result, "rc=122 means Hermit refused the run")
        self.assertIn("chaos-buggy.out", result.stdout)

    def test_a_partial_report_with_exit_0_is_not_a_crash(self):
        self._assert_refused(self._run(buggy_mode="partial-rc0"), "expected 134")

    def test_a_partial_report_cut_off_by_the_timeout_is_not_a_crash(self):
        self._assert_refused(
            self._run(buggy_mode="partial-rc124"),
            "rc=124 means the 30s timeout cut the run off",
        )

    def test_an_abort_with_a_truncated_report_is_not_a_complete_crash(self):
        """Exit 134 alone is not enough: the report must reach its SUMMARY."""
        self._assert_refused(
            self._run(buggy_mode="truncated-abort"),
            "chaos buggy seed 7 exited 134 without a complete ASAN use-after-free report",
        )

    def test_a_second_run_with_exit_0_is_refused(self):
        self._assert_refused(
            self._run(buggy_mode="replay-partial-rc0"),
            "replay did not complete with the ASAN abort",
        )

    def test_a_second_run_cut_off_by_the_timeout_is_refused(self):
        self._assert_refused(
            self._run(buggy_mode="replay-partial-rc124"),
            "replay did not complete with the ASAN abort",
        )

    def test_a_second_run_refused_after_a_skid_overshoot_is_explained(self):
        """Step 4 explains a refusal as steps 2 and 3 do, and still exits 1."""
        result = self._run(buggy_mode="replay-skid-refusal")
        self._assert_refused(result, "replay did not complete with the ASAN abort: rc=122")
        self.assertIn("rc=122 means Hermit refused the run", result.stdout)
        self.assertIn("chaos-buggy-replay.out", result.stdout)

    def test_a_second_run_with_a_truncated_report_is_refused(self):
        self._assert_refused(
            self._run(buggy_mode="replay-truncated-abort"),
            "replay exited 134 without a complete",
        )

    def test_a_second_run_with_a_different_report_is_refused(self):
        self._assert_refused(
            self._run(buggy_mode="replay-different"),
            "replay: ASAN reports differ between runs",
        )

    def _replay_prints(self, text):
        """Make the second buggy run print `text` and exit 134."""
        (self.tmp / "replay.txt").write_text(text)
        return self._run(buggy_mode="replay-custom")

    def test_a_second_run_with_a_different_shadow_memory_map_is_refused(self):
        """Same address, PC, and stacks; one shadow-memory line differs."""
        result = self._replay_prints(SHADOW_DIFFERENT)
        self._assert_refused(result, "replay: ASAN reports differ between runs")
        # diff shows the replay's line.
        self.assertIn(
            ">   0x0c0c7fff8020: 00 00 00 01 fa fa fa fa 00 00 00 00 00 00 00 01\n",
            result.stdout,
        )

    def test_a_second_run_with_a_different_free_stack_is_refused(self):
        """Same address, PC, and program frames; the freeing stack differs."""
        result = self._replay_prints(FREE_STACK_DIFFERENT)
        self._assert_refused(result, "replay: ASAN reports differ between runs")
        self.assertIn(
            ">     #2 0x418702 in do_convert convert/main.c:1360\n", result.stdout
        )

    def test_a_second_run_without_the_closing_line_is_refused(self):
        """A report that has its SUMMARY but not ABORTING cannot be compared."""
        self._assert_refused(
            self._replay_prints(NO_CLOSING_LINE),
            "replay exited 134, but its ASAN report stops before ASAN's closing "
            "==PID==ABORTING line, so the complete report cannot be compared",
        )

    def test_a_first_run_without_the_closing_line_is_refused(self):
        (self.tmp / "no-closing.txt").write_text(NO_CLOSING_LINE)
        result = self._run(DEMO08_TEST_UAF_FILE=str(self.tmp / "no-closing.txt"))
        self._assert_refused(
            result,
            "chaos buggy seed 7 exited 134, but its ASAN report stops before "
            "ASAN's closing ==PID==ABORTING line, so the complete report cannot "
            "be compared",
        )
        # Refused in Step 2: neither the fixed control nor the replay ran.
        invocations = (self.tmp / "hermit-args").read_text().splitlines()
        self.assertEqual(len(invocations), 1, invocations)

    def test_lines_from_hermit_and_its_wrapper_are_not_part_of_the_report(self):
        """Positive control: only the guest's report is compared.

        The second run differs from the first only outside the report: a Hermit
        log line inside it, Hermit's exit events in the other order with other
        timestamps, and bin/safehermit's summary after it.
        """
        result = self._replay_prints(SAME_REPORT_OTHER_LOGS)
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("replay: ASAN report byte-identical: all 63 lines", result.stdout)
        for name in ("asan-report.txt", "asan-report-replay.txt"):
            self.assertEqual((self.artifacts / name).read_text(), FULL_REPORT, name)

    def test_missing_assets_skip_by_default(self):
        # A skip run on its own exits 77, the conventional "skipped" status,
        # not 0: it produced no result, so it must not read as a pass.
        shutil.rmtree(self.assets / "buggy")
        result = self._run(DEMO08_REQUIRE_ASSETS=None, DEMO_SKIP_EXIT_STATUS=None)
        self.assertEqual(result.returncode, 77, result.stdout)
        self.assertIn("=== Demo 8: SKIPPED -- missing asset:", result.stdout)
        self.assertNotIn("SUCCESS ===", result.stdout)
        self.assertFalse((self.tmp / "hermit-args").exists())

    def test_a_skip_exits_with_the_status_the_caller_asks_for(self):
        # run-all.sh asks for 0 and reads the SKIPPED line itself.
        shutil.rmtree(self.assets / "buggy")
        for status in ("0", "5", "255"):
            with self.subTest(status=status):
                result = self._run(
                    DEMO08_REQUIRE_ASSETS=None, DEMO_SKIP_EXIT_STATUS=status
                )
                self.assertEqual(result.returncode, int(status), result.stdout)
                self.assertIn("=== Demo 8: SKIPPED -- missing asset:", result.stdout)
                self.assertFalse((self.tmp / "hermit-args").exists())

    def test_a_malformed_skip_exit_status_is_refused(self):
        # Refused before the asset check, so a typo cannot turn a skip into
        # some other status.
        shutil.rmtree(self.assets / "buggy")
        for status in ("abc", "256", "-1", "08", "1.5", " 7", "77x"):
            with self.subTest(status=status):
                result = self._run(
                    DEMO08_REQUIRE_ASSETS=None, DEMO_SKIP_EXIT_STATUS=status
                )
                self.assertEqual(result.returncode, 2, result.stdout)
                self.assertIn(
                    "error: DEMO_SKIP_EXIT_STATUS must be an exit status from 0 to 255",
                    result.stdout,
                )
                self.assertNotIn("SKIPPED", result.stdout)
                self.assertFalse((self.tmp / "hermit-args").exists())

    def test_required_assets_still_fail_when_a_skip_may_exit_0(self):
        # Positive control: asking for exit 0 on a skip does not soften
        # DEMO08_REQUIRE_ASSETS=1, which _run sets.
        shutil.rmtree(self.assets / "buggy")
        result = self._run(DEMO_SKIP_EXIT_STATUS="0")
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("=== Demo 8: FAILURE -- required asset is missing:", result.stdout)
        self.assertNotIn("SKIPPED", result.stdout)

    def test_missing_assets_fail_when_required(self):
        shutil.rmtree(self.assets / "buggy")
        result = self._run()
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("=== Demo 8: FAILURE -- required asset is missing:", result.stdout)

    def test_a_seed_recorded_for_another_binary_is_refused(self):
        (self.assets / ".crash-seed").write_text("15 {}\n".format("0" * 64))
        result = self._run(DEMO08_CRASH_SEED=None)
        self.assertEqual(result.returncode, 2, result.stdout)
        self.assertIn("was recorded for buggy binary 000000000000", result.stdout)
        self.assertFalse((self.tmp / "hermit-args").exists())

    def test_a_missing_hermit_is_named(self):
        without_hermit = [
            entry
            for entry in os.environ.get("PATH", "").split(":")
            if entry and not os.access(os.path.join(entry, "hermit"), os.X_OK)
        ]
        result = self._run(PATH=":".join(without_hermit))
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("error: hermit is not on PATH", result.stdout)


class CalibrationControlsTest(unittest.TestCase):
    """prepare-assets.sh must record a seed only when run.sh would pass on it."""

    CALIBRATION_TIMEOUT = 5

    @classmethod
    def setUpClass(cls):
        cls._class_tmp = tempfile.TemporaryDirectory()
        cls.class_tmp = Path(cls._class_tmp.name)
        cls.planted_uaf = cls._build_planted_uaf(cls.class_tmp)
        cls.digest = fixture_source_digest()

    @classmethod
    def tearDownClass(cls):
        cls._class_tmp.cleanup()

    @staticmethod
    def _asan_link_flags(scratch):
        """Extra linker flags for AddressSanitizer, or [] when none are needed.

        On some hosts `-fsanitize=address` names a libasan that is not on the
        default library path although the runtime is installed elsewhere. Find
        it and add both -L and an rpath, so the program links and also runs.
        """
        probe = scratch / "asan-probe.c"
        probe.write_text("int main(void) { return 0; }\n")
        linked = subprocess.run(
            ["gcc", "-fsanitize=address", str(probe), "-o", str(scratch / "asan-probe")],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=120,
        )
        if linked.returncode == 0:
            return []
        script = subprocess.run(
            ["gcc", "-print-file-name=libasan.so"],
            stdout=subprocess.PIPE,
            text=True,
            timeout=60,
        ).stdout.strip()
        wanted = "libasan.so"
        try:
            match = re.search(r"libasan\.so\.[0-9.]+", Path(script).read_text(errors="replace"))
            if match:
                wanted = match.group(0)
        except OSError:
            pass
        found = subprocess.run(
            ["find", "/opt", "/usr/local", "-name", wanted, "-print", "-quit"],
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            timeout=300,
        ).stdout.strip()
        if not found:
            raise AssertionError(
                "no AddressSanitizer runtime ({}) found; the calibration tests "
                "need a C compiler with AddressSanitizer".format(wanted)
            )
        directory = str(Path(found).parent)
        return ["-L" + directory, "-Wl,-rpath," + directory]

    @classmethod
    def _build_planted_uaf(cls, scratch):
        if shutil.which("gcc") is None:
            raise AssertionError(
                "gcc is required: the calibration tests build a real "
                "AddressSanitizer use-after-free"
            )
        source = scratch / "planted-uaf.c"
        source.write_text(PLANTED_UAF_C)
        binary = scratch / "planted-uaf"
        subprocess.run(
            ["gcc", "-O0", "-g", "-fsanitize=address", "-fno-omit-frame-pointer",
             str(source), "-o", str(binary)] + cls._asan_link_flags(scratch),
            check=True,
            timeout=120,
        )
        # Check the program itself before relying on it: a program that stopped
        # reporting the use-after-free would make every "seed found" test vacuous.
        environment = dict(os.environ, ASAN_OPTIONS="detect_leaks=0:abort_on_error=1")
        ran = subprocess.run(
            [str(binary)],
            env=environment,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            timeout=60,
        )
        if "heap-use-after-free" not in ran.stdout or ran.returncode == 0:
            raise AssertionError(
                "the planted use-after-free did not produce an ASAN abort "
                "(exit {}); first lines:\n{}".format(
                    ran.returncode, "\n".join(ran.stdout.splitlines()[:5])
                )
            )
        return binary

    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.tmp = Path(self._tmp.name)
        self.addCleanup(self._tmp.cleanup)
        self.stub_dir = self.tmp / "bin"
        self.stub_dir.mkdir()
        _write_executable(self.stub_dir / "hermit", CALIBRATION_STUB)
        self.assets = self.tmp / "assets"
        _make_assets(self.assets)
        # The cache stamp prepare-assets.sh expects, so it skips the build and
        # goes straight to calibration.
        (self.assets / ".nightly-prep-version").write_text(
            "prep={} btrfs={} fixture-src={}\n".format(
                PREP_VERSION, BTRFS_COMMIT, self.digest
            )
        )
        self.artifacts = self.tmp / "artifacts"
        self.fixture = _sha256_hex(self.assets / "buggy" / "btrfs-convert")

    def _prepare(self, mode, seeds, **overrides):
        environment = _base_environment(self.stub_dir)
        environment.update(
            {
                "DEMO08_DIR": str(self.assets),
                "DEMO08_BUILD_ROOT": str(self.tmp / "build-unused"),
                # A local path that does not exist, so a stamp mismatch can
                # never fetch anything.
                "DEMO08_BTRFS_REPO": str(self.tmp / "no-such-repository"),
                "DEMO08_ARTIFACTS": str(self.artifacts),
                "DEMO08_CALIBRATION_SEEDS": str(seeds),
                "DEMO08_CALIBRATION_TIMEOUT": str(self.CALIBRATION_TIMEOUT),
                "DEMO08_TEST_COUNT_DIR": str(self.tmp / "counts"),
                "DEMO08_TEST_MODE": mode,
                "DEMO08_TEST_UAF_BIN": str(self.planted_uaf),
            }
        )
        environment.update(overrides)
        result = subprocess.run(
            [str(PREPARE)],
            env=environment,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            timeout=300,
        )
        return result

    def _calibrated(self, result):
        """Assert the cached-asset calibration path ran (not the build path)."""
        self.assertIn("Searching for a crashing seed", result.stdout)
        return result

    def _rows(self):
        return (self.artifacts / "calibration.tsv").read_text().splitlines()

    def _count_rows(self, pattern):
        return sum(1 for row in self._rows() if re.search(pattern, row))

    def _assert_refused(self, result, *reasons):
        self._calibrated(result)
        self.assertNotEqual(result.returncode, 0, result.stdout)
        for reason in reasons:
            self.assertIn(reason, result.stdout)
        self.assertFalse((self.assets / ".crash-seed").exists())

    def test_a_real_asan_crash_is_found_confirmed_and_recorded(self):
        """Positive control: calibration accepts a seed that run.sh would pass."""
        result = self._calibrated(self._prepare("planted-uaf", 3, DEMO08_TEST_UAF_SEED="1"))
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("engagement=2/2 uaf_hits=1/2", result.stdout)
        self.assertIn("Demo 8 crash seed calibrated: 1", result.stdout)
        self.assertNotIn("Demo 8 crash seed replayed:", result.stdout)
        self.assertEqual(
            (self.assets / ".crash-seed").read_text(), "1 {}\n".format(self.fixture)
        )
        # Seed 0 ran clean; seed 1 crashed, crashed again, and the fixed variant
        # was clean on it. Seed 2 was never needed.
        self.assertEqual(len(self._rows()), 5)
        self.assertEqual(self._count_rows(r"^0\tcold\tbuggy\treached\tnone\t0\t"), 1)
        self.assertEqual(
            self._count_rows(r"^1\tcold\tbuggy\treached\tcomplete\t134\t\d+\tyes\t"), 1
        )
        self.assertEqual(
            self._count_rows(r"^1\tcold\tbuggy-replay\treached\tcomplete\t134\t\d+\tyes\t"), 1
        )
        self.assertEqual(
            self._count_rows(r"^1\tcold\tfixed\treached\tnone\t0\t\d+\tn/a\t"), 1
        )
        for name in ("calibration-cold-seed-1.out", "calibration-confirm-replay-seed-1.out"):
            self.assertIn(
                "AddressSanitizer: heap-use-after-free",
                (self.artifacts / name).read_text(errors="replace"),
            )

    def test_a_recorded_seed_is_replayed_not_trusted(self):
        (self.assets / ".crash-seed").write_text("1 {}\n".format(self.fixture))
        result = self._calibrated(self._prepare("planted-uaf", 3, DEMO08_TEST_UAF_SEED="1"))
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn(
            "engagement=1/1 uaf_hits=1/1 qualified=1/1 executed=1/1", result.stdout
        )
        self.assertIn("Demo 8 crash seed replayed: cached seed 1", result.stdout)
        self.assertNotIn("Demo 8 crash seed calibrated:", result.stdout)
        self.assertEqual(len(self._rows()), 4)
        self.assertEqual(
            self._count_rows(r"^1\tcached\tbuggy\treached\tcomplete\t134\t\d+\tyes\t"), 1
        )
        self.assertEqual(
            self._count_rows(r"^1\tcached\tbuggy-replay\treached\tcomplete\t134\t\d+\tyes\t"),
            1,
        )
        self.assertEqual(
            self._count_rows(r"^1\tcached\tfixed\treached\tnone\t0\t\d+\tn/a\t"), 1
        )

    def test_a_recorded_seed_that_no_longer_runs_is_refused(self):
        (self.assets / ".crash-seed").write_text("1 {}\n".format(self.fixture))
        result = self._prepare("runner-failure", 1)
        self._calibrated(result)
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn("never executed the guest: 0 of 2 seeds", result.stdout)
        self.assertEqual(len(self._rows()), 3)
        self.assertEqual(self._count_rows(r"^1\tcached\tbuggy\tdid-not-reach\tnone\t127\t"), 1)

    def test_runs_that_never_reach_the_progress_thread_are_no_result(self):
        result = self._prepare("no-engagement", 2)
        self._assert_refused(result, "NO-RESULT: path engagement 0/2")
        self.assertEqual(len(self._rows()), 3)
        self.assertEqual(self._count_rows(r"\tdid-not-reach\t"), 2)
        self.assertEqual(len(list(self.artifacts.glob("calibration-cold-seed-*.out"))), 2)

    def test_engaged_runs_without_a_crash_are_not_called_no_result(self):
        result = self._prepare("engaged-no-hit", 2)
        self._assert_refused(result, "no ASAN UAF found", "path engagement 2/2")
        self.assertNotIn("NO-RESULT", result.stdout)
        self.assertEqual(self._count_rows(r"\treached\tnone\t"), 2)

    def test_a_partial_report_with_exit_0_does_not_qualify(self):
        result = self._prepare("partial-uaf-rc0", 2)
        self._assert_refused(result, "uaf_hits=2/2 qualified=0/2")
        self.assertEqual(self._count_rows(r"\treached\thit\t0\t\d+\tno\t"), 2)
        self.assertEqual(self._count_rows(r"\tyes\t"), 0)

    def test_a_partial_report_cut_off_by_the_timeout_does_not_qualify(self):
        result = self._prepare("partial-uaf-rc124", 2)
        self._assert_refused(result, "uaf_hits=2/2 qualified=0/2")
        self.assertEqual(self._count_rows(r"\treached\thit\t124\t\d+\tno\t"), 2)

    def test_a_complete_report_without_the_abort_does_not_qualify(self):
        """Only the exit status refuses this one: the report text is complete."""
        result = self._prepare("complete-uaf-rc0", 2)
        self._assert_refused(result, "uaf_hits=2/2 qualified=0/2")
        self.assertEqual(self._count_rows(r"\tbuggy\treached\tcomplete\t0\t\d+\tno\t"), 2)
        self.assertEqual(self._count_rows(r"\tyes\t"), 0)
        self.assertEqual(self._count_rows(r"\tbuggy-replay\t"), 0)
        self.assertEqual(self._count_rows(r"\tfixed\t"), 0)

    def test_a_calibration_budget_above_the_demo_timeout_is_refused(self):
        result = self._prepare(
            "planted-uaf",
            2,
            DEMO08_TEST_UAF_SEED="1",
            DEMO08_TIMEOUT="30",
            DEMO08_CALIBRATION_TIMEOUT="60",
        )
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn("exceeds the demo's per-run budget", result.stdout)
        self.assertFalse((self.assets / ".crash-seed").exists())

    def test_a_seed_whose_replay_times_out_is_not_recorded(self):
        result = self._prepare("replay-timeout", 2)
        self._assert_refused(
            result,
            "replayed past the {}s budget".format(self.CALIBRATION_TIMEOUT),
            "qualifying seed(s) but confirmed none",
            "unconfirmed=2",
        )
        self.assertNotIn("no ASAN UAF found", result.stdout)
        self.assertEqual(
            self._count_rows(r"\tbuggy-replay\treached\thit\t124\t\d+\tno\t"), 2
        )

    def test_a_seed_that_crashes_once_stops_the_calibration(self):
        """Two runs of one seed disagreeing is a failure, not a reason to move on."""
        result = self._prepare("replay-clean", 2)
        self._assert_refused(result, "did not on its replay")
        # Stopped at seed 0: its first run and its replay only.
        self.assertEqual(len(self._rows()), 3)

    def test_a_seed_whose_fixed_run_times_out_is_not_recorded(self):
        result = self._prepare("fixed-timeout", 1)
        self._assert_refused(
            result,
            "fixed control on seed 0 ran past the {}s budget".format(
                self.CALIBRATION_TIMEOUT
            ),
            "unconfirmed=1",
        )

    def test_a_fixed_variant_uaf_stops_the_calibration(self):
        result = self._prepare("fixed-uaf", 1)
        self._assert_refused(result, "fixed variant reported a use-after-free on seed 0")

    def test_a_uaf_without_the_progress_thread_does_not_qualify(self):
        result = self._prepare("uaf-no-engagement", 1)
        self._assert_refused(
            result, "NO-RESULT: path engagement 0/1", "engagement=0/1 uaf_hits=1/1"
        )

    def test_a_failure_to_start_the_guest_is_not_a_missing_crash(self):
        result = self._prepare("runner-failure", 1)
        self._assert_refused(result, "never executed the guest: 0 of 1 seeds")
        self.assertNotIn("no ASAN UAF found", result.stdout)

    def test_crash_text_from_a_failed_run_does_not_qualify(self):
        result = self._prepare("runner-failure-with-signatures", 1)
        self._assert_refused(result, "never executed the guest: 0 of 1 seeds")

    def test_a_refused_run_is_retried_and_the_seed_still_found(self):
        """A refusal (exit 122) is not "this seed did not crash": run it again."""
        result = self._calibrated(
            self._prepare("refused-once", 1, DEMO08_TEST_UAF_SEED="0")
        )
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("Demo 8 crash seed calibrated: 0", result.stdout)
        self.assertIn(
            "Hermit refused the buggy run on seed 0 (rc=122, attempt 1 of 3)",
            result.stdout,
        )
        self.assertIn(
            "Hermit refused the fixed run on seed 0 (rc=122, attempt 1 of 3)",
            result.stdout,
        )
        self.assertIn("executed=1/1 unconfirmed=0 refused=0/1 refused_runs=2", result.stdout)
        self.assertEqual(
            (self.assets / ".crash-seed").read_text(), "0 {}\n".format(self.fixture)
        )
        # The two refused attempts are rows of their own, with their output kept.
        self.assertEqual(len(self._rows()), 6)
        self.assertEqual(self._count_rows(r"\t122\t\d+\trefused\t"), 2)
        self.assertEqual(
            self._count_rows(r"^0\tcold\tbuggy\treached\tcomplete\t134\t\d+\tyes\t"), 1
        )
        for name in (
            "calibration-cold-seed-0-refused-1.out",
            "calibration-confirm-fixed-seed-0-refused-1.out",
        ):
            self.assertIn(
                "HERMIT_POLICY_REFUSAL", (self.artifacts / name).read_text()
            )

    def test_seeds_refused_on_every_attempt_are_reported_as_refused(self):
        result = self._prepare("always-refused", 2)
        self._assert_refused(
            result,
            "refused by Hermit on every seed: each of the 2 attempted seeds "
            "exited 122 on all 3 attempt(s)",
            "refused=2/2 refused_runs=6",
            "seed 0 was not tested: Hermit refused every attempt",
        )
        self.assertNotIn("never executed the guest", result.stdout)
        self.assertEqual(len(self._rows()), 7)
        self.assertEqual(self._count_rows(r"\t122\t\d+\trefused\t"), 6)

    def test_refusal_retries_can_be_turned_off(self):
        result = self._prepare("always-refused", 1, DEMO08_REFUSAL_RETRIES="0")
        self._assert_refused(result, "exited 122 on all 1 attempt(s)", "refused_runs=1")
        self.assertEqual(len(self._rows()), 2)

    def test_a_replay_refused_on_every_attempt_is_not_a_disagreement(self):
        result = self._prepare("replay-refused", 1)
        self._assert_refused(
            result,
            "Hermit refused every replay attempt (rc=122), so the replay was not tested",
            "qualifying seed(s) but confirmed none",
            "1 had a replay or fixed control that Hermit refused on every attempt",
        )
        # refused= counts only seeds whose first run was refused; a refused
        # replay is counted under unconfirmed=, and refused_runs= counts every
        # refused attempt.
        self.assertIn("unconfirmed=1 refused=0/1 refused_runs=3", result.stdout)
        self.assertNotIn("did not on its replay", result.stdout)
        self.assertEqual(
            self._count_rows(r"\tbuggy-replay\treached\tnone\t122\t\d+\trefused\t"), 3
        )

    def test_a_fixed_run_refused_on_every_attempt_is_unconfirmed_not_refused(self):
        result = self._prepare("fixed-refused", 1)
        self._assert_refused(
            result,
            "fixed control on seed 0 was refused by Hermit on every attempt",
            "qualifying seed(s) but confirmed none",
            "unconfirmed=1 refused=0/1 refused_runs=3",
        )
        self.assertEqual(
            self._count_rows(r"\tfixed\treached\tnone\t122\t\d+\trefused\t"), 3
        )

    def test_a_refused_fixed_run_with_a_uaf_is_not_retried_away(self):
        """The fixed variant's use-after-free stops calibration even when refused."""
        result = self._prepare("fixed-refused-uaf", 1)
        self._assert_refused(result, "fixed variant reported a use-after-free on seed 0")
        self.assertEqual(self._count_rows(r"\tfixed\t"), 1)


class CalibrationReportComparisonTest(unittest.TestCase):
    """prepare-assets.sh must record a seed only if its two reports are the same.

    run.sh's Step 4 compares the complete ASAN report of two runs of the seed,
    so calibration must make the same comparison. The planted program here is a
    shell script that prints a saved run's output and exits 134, so these tests
    need no compiler; CalibrationControlsTest feeds calibration a real ASAN abort.
    """

    CALIBRATION_TIMEOUT = 5

    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.tmp = Path(self._tmp.name)
        self.addCleanup(self._tmp.cleanup)
        self.stub_dir = self.tmp / "bin"
        self.stub_dir.mkdir()
        _write_executable(self.stub_dir / "hermit", CALIBRATION_STUB)
        self.assets = self.tmp / "assets"
        _make_assets(self.assets)
        # The cache stamp prepare-assets.sh expects, so it skips the build and
        # goes straight to calibration.
        (self.assets / ".nightly-prep-version").write_text(
            "prep={} btrfs={} fixture-src={}\n".format(
                PREP_VERSION, BTRFS_COMMIT, fixture_source_digest()
            )
        )
        self.artifacts = self.tmp / "artifacts"
        self.fixture = _sha256_hex(self.assets / "buggy" / "btrfs-convert")
        self.first = self.tmp / "first-run.txt"
        self.first.write_text(UAF_REPORT)
        self.guest = self.tmp / "planted-uaf"
        _write_executable(
            self.guest, "#!/usr/bin/env bash\ncat '{}'\nexit 134\n".format(self.first)
        )

    def _prepare(self, mode, replay_text=None, **overrides):
        environment = _base_environment(self.stub_dir)
        environment.update(
            {
                "DEMO08_DIR": str(self.assets),
                "DEMO08_BUILD_ROOT": str(self.tmp / "build-unused"),
                "DEMO08_BTRFS_REPO": str(self.tmp / "no-such-repository"),
                "DEMO08_ARTIFACTS": str(self.artifacts),
                "DEMO08_CALIBRATION_SEEDS": "1",
                "DEMO08_CALIBRATION_TIMEOUT": str(self.CALIBRATION_TIMEOUT),
                "DEMO08_TEST_COUNT_DIR": str(self.tmp / "counts"),
                "DEMO08_TEST_MODE": mode,
                "DEMO08_TEST_UAF_BIN": str(self.guest),
                "DEMO08_TEST_UAF_SEED": "0",
            }
        )
        if replay_text is not None:
            replay = self.tmp / "replay-run.txt"
            replay.write_text(replay_text)
            environment["DEMO08_TEST_REPLAY_REPORT"] = str(replay)
        environment.update(overrides)
        result = subprocess.run(
            [str(PREPARE)],
            env=environment,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            timeout=300,
        )
        self.assertIn("Searching for a crashing seed", result.stdout)
        return result

    def _rows(self):
        return (self.artifacts / "calibration.tsv").read_text().splitlines()

    def _assert_stopped(self, result, *reasons):
        self.assertEqual(result.returncode, 1, result.stdout)
        for reason in reasons:
            self.assertIn(reason, result.stdout)
        self.assertFalse((self.assets / ".crash-seed").exists())
        self.assertNotIn("Demo 8 crash seed calibrated", result.stdout)

    def test_the_same_report_is_confirmed_whatever_hermit_logs_around_it(self):
        """Positive control: only the guest's report is compared.

        The replay differs from the first run only outside the report: a Hermit
        log line inside it, Hermit's exit events in the other order with other
        timestamps, and bin/safehermit's summary after it.
        """
        result = self._prepare("replay-report", SAME_REPORT_OTHER_LOGS)
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("Demo 8 crash seed calibrated: 0", result.stdout)
        self.assertIn(
            "seed=0 replay printed the same complete ASAN report as the first run "
            "(63 lines:",
            result.stdout,
        )
        self.assertEqual(
            (self.assets / ".crash-seed").read_text(), "0 {}\n".format(self.fixture)
        )
        for name in (
            "calibration-cold-seed-0.asan.txt",
            "calibration-confirm-replay-seed-0.asan.txt",
        ):
            self.assertEqual((self.artifacts / name).read_text(), FULL_REPORT, name)
        # The first run, its replay, and the fixed control, after the header.
        self.assertEqual(len(self._rows()), 4)

    def test_a_replay_with_a_different_shadow_memory_map_stops_the_calibration(self):
        result = self._prepare("replay-report", SHADOW_DIFFERENT)
        self._assert_stopped(
            result,
            "demo 8 seed 0 crashed on its first run and on its replay, but the two "
            "ASAN reports differ",
            ">   0x0c0c7fff8020: 00 00 00 01 fa fa fa fa 00 00 00 00 00 00 00 01\n",
        )
        # Stopped before the fixed control: the first run and the replay only.
        self.assertEqual(len(self._rows()), 3)

    def test_a_replay_with_a_different_free_stack_stops_the_calibration(self):
        result = self._prepare("replay-report", FREE_STACK_DIFFERENT)
        self._assert_stopped(
            result,
            "the two ASAN reports differ",
            ">     #2 0x418702 in do_convert convert/main.c:1360\n",
        )
        self.assertEqual(len(self._rows()), 3)

    def test_a_replay_without_the_closing_line_stops_the_calibration(self):
        result = self._prepare("replay-report", NO_CLOSING_LINE)
        self._assert_stopped(
            result,
            "demo 8 seed 0: the replay run exited 134 with the report's SUMMARY "
            "line, but no complete ASAN report could be saved from",
        )
        self.assertEqual(len(self._rows()), 3)

    def test_a_first_run_without_the_closing_line_stops_before_the_replay(self):
        self.first.write_text(NO_CLOSING_LINE)
        result = self._prepare("planted-uaf")
        self._assert_stopped(
            result,
            "demo 8 seed 0: the first run exited 134 with the report's SUMMARY "
            "line, but no complete ASAN report could be saved from",
        )
        # The header and the first run; the replay never ran.
        self.assertEqual(len(self._rows()), 2)
        self.assertFalse(
            (self.artifacts / "calibration-confirm-replay-seed-0.out").exists()
        )


# The start of the refusal both scripts print for a path under /tmp that they
# cannot show the converter, and the advice that ends it.
TMP_REFUSAL = (
    "error: {} is under /tmp. Hermit gives the program it runs a private /tmp, "
    "and the demo cannot make this path visible there: {}. "
)
TMP_HINT = (
    "Run demo 8 from a checkout outside /tmp or through a path that begins with "
    "/tmp/, or set DEMO08_DIR and DEMO08_ARTIFACTS to directories outside /tmp "
    "or to absolute paths that begin with /tmp/."
)


class TmpBindTest(unittest.TestCase):
    """run.sh shows the converter its program and image when they are in /tmp.

    Hermit hides the host's /tmp from the program it runs and refuses to start
    a program there, so run.sh adds `--bind DIR` for the converter's directory
    and `--bind IMAGE` for the image when their paths begin with /tmp/. A
    checkout outside /tmp must keep exactly the command line it had before, the
    one its recorded crash seed was found with.
    """

    def setUp(self):
        holder = tempfile.TemporaryDirectory()
        self.addCleanup(holder.cleanup)
        # The stub and its records. Their location is not on the command line.
        self.state = Path(holder.name)
        (self.state / "bin").mkdir()
        _write_executable(self.state / "bin" / "hermit", RUN_STUB)
        (self.state / "uaf.txt").write_text(UAF_REPORT)
        self.inside = _temporary_root(self, under_tmp=True)
        self.outside = _temporary_root(self, under_tmp=False)

    def _run(self, assets, artifacts, cwd=None):
        """run.sh on the full result: a crash, a clean fix, the same crash."""
        for name in ("hermit-args", "buggy-count"):
            (self.state / name).unlink(missing_ok=True)
        environment = _base_environment(self.state / "bin")
        environment.update(
            {
                "DEMO08_DIR": str(assets),
                "DEMO08_ARTIFACTS": str(artifacts),
                "DEMO08_CRASH_SEED": "7",
                "DEMO08_TIMEOUT": "30",
                "DEMO08_REQUIRE_ASSETS": "1",
                "DEMO08_TEST_FIXED_MODE": "clean",
                "DEMO08_TEST_BUGGY_MODE": "complete-abort",
                "DEMO08_TEST_COUNT_FILE": str(self.state / "buggy-count"),
                "DEMO08_TEST_ARGS_FILE": str(self.state / "hermit-args"),
                "DEMO08_TEST_UAF_FILE": str(self.state / "uaf.txt"),
            }
        )
        return subprocess.run(
            [str(RUN)],
            cwd=cwd,
            env=environment,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            timeout=120,
        )

    def _assert_passed_with(self, result, expected):
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn(
            "=== Demo 8: btrfs-convert Use-After-Free: SUCCESS ===", result.stdout
        )
        self.assertEqual(
            (self.state / "hermit-args").read_text().splitlines(), expected
        )

    def _assert_tmp_refusal(self, result, path, problem):
        """run.sh stopped before any Hermit run, naming the path and the fix."""
        self.assertEqual(result.returncode, 2, result.stdout)
        self.assertIn(TMP_REFUSAL.format(path, problem) + TMP_HINT, result.stdout)
        self.assertNotIn("SUCCESS ===", result.stdout)
        self.assertFalse((self.state / "hermit-args").exists(), result.stdout)

    def test_a_checkout_under_tmp_binds_the_converter_directory_and_the_image(self):
        assets, artifacts = self.inside / "assets", self.inside / "artifacts"
        _make_assets(assets)
        result = self._run(assets, artifacts)
        self._assert_passed_with(
            result,
            [
                "--log=error run --chaos --sched-seed 7 --no-virtualize-cpuid "
                "--base-env=minimal --epoch=2026-01-01T00:00:00Z "
                "--bind {a}/{v} --bind {o}/chaos-{v}.img "
                "-- {a}/{v}/btrfs-convert {o}/chaos-{v}.img".format(
                    a=assets, o=artifacts, v=variant
                )
                for variant in ("buggy", "fixed", "buggy")
            ],
        )

    def test_a_checkout_outside_tmp_keeps_its_command_line(self):
        """Positive control: no --bind at all, as before the binds existed."""
        assets, artifacts = self.outside / "assets", self.outside / "artifacts"
        _make_assets(assets)
        result = self._run(assets, artifacts)
        self._assert_passed_with(
            result,
            [
                "--log=error run --chaos --sched-seed 7 --no-virtualize-cpuid "
                "--base-env=minimal --epoch=2026-01-01T00:00:00Z "
                "-- {a}/{v}/btrfs-convert {o}/chaos-{v}.img".format(
                    a=assets, o=artifacts, v=variant
                )
                for variant in ("buggy", "fixed", "buggy")
            ],
        )

    def test_only_the_paths_under_tmp_are_bound(self):
        for name, assets, artifacts, binds in (
            (
                "image under /tmp",
                self.outside / "assets",
                self.inside / "artifacts",
                "--bind {o}/chaos-{v}.img ",
            ),
            (
                "converter under /tmp",
                self.inside / "assets",
                self.outside / "artifacts",
                "--bind {a}/{v} ",
            ),
        ):
            with self.subTest(name):
                _make_assets(assets)
                result = self._run(assets, artifacts)
                self._assert_passed_with(
                    result,
                    [
                        (
                            "--log=error run --chaos --sched-seed 7 "
                            "--no-virtualize-cpuid --base-env=minimal "
                            "--epoch=2026-01-01T00:00:00Z "
                            + binds
                            + "-- {a}/{v}/btrfs-convert {o}/chaos-{v}.img"
                        ).format(a=assets, o=artifacts, v=variant)
                        for variant in ("buggy", "fixed", "buggy")
                    ],
                )

    def test_a_relative_image_in_a_working_directory_under_tmp_is_left_as_is(self):
        """The converter inherits the working directory, which reaches the image."""
        assets = self.inside / "assets"
        _make_assets(assets)
        result = self._run(assets, "artifacts", cwd=self.inside)
        self._assert_passed_with(
            result,
            [
                "--log=error run --chaos --sched-seed 7 --no-virtualize-cpuid "
                "--base-env=minimal --epoch=2026-01-01T00:00:00Z "
                "--bind {a}/{v} "
                "-- {a}/{v}/btrfs-convert artifacts/chaos-{v}.img".format(
                    a=assets, v=variant
                )
                for variant in ("buggy", "fixed", "buggy")
            ],
        )

    def test_a_path_into_tmp_through_a_symbolic_link_is_refused(self):
        _make_assets(self.inside / "assets")
        (self.outside / "link").symlink_to(self.inside)
        assets = self.outside / "link" / "assets"
        result = self._run(assets, self.outside / "artifacts")
        self._assert_tmp_refusal(
            result,
            "{}/buggy/btrfs-convert".format(assets),
            "it resolves to {}, under /tmp, but it does not begin with /tmp/".format(
                os.path.realpath(self.inside / "assets" / "buggy" / "btrfs-convert")
            ),
        )

    def test_a_path_with_a_colon_is_refused(self):
        assets = self.inside / "as:sets"
        _make_assets(assets)
        result = self._run(assets, self.inside / "artifacts")
        self._assert_tmp_refusal(
            result,
            "{}/buggy/btrfs-convert".format(assets),
            "it contains ':', which --bind reads as SOURCE:TARGET",
        )

    def test_an_image_path_with_a_colon_is_refused(self):
        assets, artifacts = self.inside / "assets", self.inside / "arti:facts"
        _make_assets(assets)
        result = self._run(assets, artifacts)
        self._assert_tmp_refusal(
            result,
            "{}/chaos-buggy.img".format(artifacts),
            "it contains ':', which --bind reads as SOURCE:TARGET",
        )
        # Nor is the artifact directory created.
        self.assertFalse(artifacts.exists())

    def test_a_path_with_a_dot_dot_component_is_refused(self):
        _make_assets(self.inside / "assets")
        (self.inside / "x").mkdir()
        assets = "{}/x/../assets".format(self.inside)
        result = self._run(assets, self.inside / "artifacts")
        self._assert_tmp_refusal(
            result,
            "{}/buggy/btrfs-convert".format(assets),
            "it contains a '..' component",
        )

    def test_a_converter_directory_that_is_a_symbolic_link_is_refused(self):
        assets = self.inside / "assets"
        _make_assets(assets)
        (assets / "buggy").rename(self.inside / "real-buggy")
        (assets / "buggy").symlink_to(self.inside / "real-buggy")
        result = self._run(assets, self.inside / "artifacts")
        self._assert_tmp_refusal(
            result,
            "{}/buggy/btrfs-convert".format(assets),
            "its directory, {}/buggy, is a symbolic link, which Hermit would "
            "mount as a file".format(assets),
        )

    def test_a_relative_converter_in_a_working_directory_under_tmp_is_refused(self):
        """Hermit starts the converter by its absolute path, which it hides."""
        _make_assets(self.inside / "assets")
        result = self._run("assets", self.inside / "artifacts", cwd=self.inside)
        self._assert_tmp_refusal(
            result,
            "assets/buggy/btrfs-convert",
            "Hermit starts a program by the absolute path it resolves on the host, "
            "{}/assets/buggy/btrfs-convert, which is under /tmp".format(self.inside),
        )


class CalibrationTmpBindTest(unittest.TestCase):
    """prepare-assets.sh calibrates with the binds that run.sh replays.

    A seed found without the binds would be replayed with them, so calibration
    must run each converter with exactly run.sh's command line, and refuse the
    paths run.sh refuses before it builds or runs anything.
    """

    def setUp(self):
        holder = tempfile.TemporaryDirectory()
        self.addCleanup(holder.cleanup)
        self.state = Path(holder.name)
        (self.state / "bin").mkdir()
        _write_executable(self.state / "bin" / "hermit", CALIBRATION_STUB)
        first = self.state / "first-run.txt"
        first.write_text(UAF_REPORT)
        self.guest = self.state / "planted-uaf"
        _write_executable(
            self.guest, "#!/usr/bin/env bash\ncat '{}'\nexit 134\n".format(first)
        )
        self.inside = _temporary_root(self, under_tmp=True)
        self.outside = _temporary_root(self, under_tmp=False)

    def _prepare(self, assets, artifacts):
        """Calibrate one seed, 0, which crashes on its first run and replay."""
        _make_assets(assets)
        # The cache stamp prepare-assets.sh expects, so it skips the build.
        (assets / ".nightly-prep-version").write_text(
            "prep={} btrfs={} fixture-src={}\n".format(
                PREP_VERSION, BTRFS_COMMIT, fixture_source_digest()
            )
        )
        environment = _base_environment(self.state / "bin")
        environment.update(
            {
                "DEMO08_DIR": str(assets),
                "DEMO08_BUILD_ROOT": str(self.state / "build-unused"),
                "DEMO08_BTRFS_REPO": str(self.state / "no-such-repository"),
                "DEMO08_ARTIFACTS": str(artifacts),
                "DEMO08_CALIBRATION_SEEDS": "1",
                "DEMO08_CALIBRATION_TIMEOUT": "5",
                "DEMO08_TEST_COUNT_DIR": str(self.state / "counts"),
                "DEMO08_TEST_MODE": "planted-uaf",
                "DEMO08_TEST_UAF_BIN": str(self.guest),
                "DEMO08_TEST_UAF_SEED": "0",
                "DEMO08_TEST_ARGS_FILE": str(self.state / "hermit-args"),
            }
        )
        return subprocess.run(
            [str(PREPARE)],
            env=environment,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            timeout=300,
        )

    def _assert_calibrated_with(self, result, assets, expected):
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("Demo 8 crash seed calibrated: 0", result.stdout)
        self.assertTrue((assets / ".crash-seed").exists(), result.stdout)
        self.assertEqual(
            (self.state / "hermit-args").read_text().splitlines(), expected
        )

    def test_a_checkout_under_tmp_calibrates_with_the_binds(self):
        assets, artifacts = self.inside / "assets", self.inside / "artifacts"
        result = self._prepare(assets, artifacts)
        # The seed's first run, its replay, and the fixed control.
        self._assert_calibrated_with(
            result,
            assets,
            [
                "--log=error run --chaos --sched-seed 0 --no-virtualize-cpuid "
                "--base-env=minimal --epoch=2026-01-01T00:00:00Z "
                "--bind {a}/{v} --bind {o}/chaos-{v}.img "
                "-- {a}/{v}/btrfs-convert {o}/chaos-{v}.img".format(
                    a=assets, o=artifacts, v=variant
                )
                for variant in ("buggy", "buggy", "fixed")
            ],
        )

    def test_a_checkout_outside_tmp_calibrates_with_its_command_line(self):
        """Positive control: no --bind at all, as before the binds existed."""
        assets, artifacts = self.outside / "assets", self.outside / "artifacts"
        result = self._prepare(assets, artifacts)
        self._assert_calibrated_with(
            result,
            assets,
            [
                "--log=error run --chaos --sched-seed 0 --no-virtualize-cpuid "
                "--base-env=minimal --epoch=2026-01-01T00:00:00Z "
                "-- {a}/{v}/btrfs-convert {o}/chaos-{v}.img".format(
                    a=assets, o=artifacts, v=variant
                )
                for variant in ("buggy", "buggy", "fixed")
            ],
        )

    def test_a_path_it_cannot_bind_stops_it_before_any_run(self):
        assets = self.inside / "as:sets"
        result = self._prepare(assets, self.inside / "artifacts")
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn(
            TMP_REFUSAL.format(
                "{}/buggy/btrfs-convert".format(assets),
                "it contains ':', which --bind reads as SOURCE:TARGET",
            )
            + TMP_HINT,
            result.stdout,
        )
        self.assertNotIn("Searching for a crashing seed", result.stdout)
        self.assertFalse((self.state / "hermit-args").exists(), result.stdout)
        self.assertFalse((assets / ".crash-seed").exists())


if __name__ == "__main__":
    unittest.main()
