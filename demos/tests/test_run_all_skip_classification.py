#!/usr/bin/env python3
"""Tests for how run-all.sh classifies a demo that declined to run.

Inside the sweep a demo exits 0 when it cannot run -- demo 8 does this when its
ASAN assets are absent. Run on its own it exits 77, but make turns that into 2,
so the sweep sets DEMO_SKIP_EXIT_STATUS=0, and the exit code alone cannot
separate a pass from a skip. run-all.sh reads the demo's SKIPPED line instead,
records the demo as SKIP, and never reports a skipped demo as passed. A sweep
with a skip and no failure exits 3, so a caller that reads only the exit status
does not take it for a success. There is no option that accepts a skip: a
caller that accepts one checks for exit status 3 itself.

Because the SKIPPED line is read from the demo's log, a log directory, log, or
summary that cannot be created, written, or read makes the sweep exit 4. An
unread log must never let a skip count as a pass.

Demos 5 and 6 also exit 0 after a run that only saved itself as their
reference run and compared nothing; their last result line then says FIRST RUN
SAVED. The sweep records such a demo as UNCOMPARED, exits 3 as for a skip, and
never reports it as passed. It also sets QEMU_BOOT_REPEAT=1 and
QEMU_RESUME_REPEAT=1, so that a demo with no reference run runs a second time
and compares.

Exit 0 alone is not a pass either: a demo passes only when its own last result
line, `=== Demo N: <title>: SUCCESS ===` with N its own number, says SUCCESS. A
demo that exits 0 without that line is recorded as FAIL.
"""

import collections
import os
import re
import shlex
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
RUN_ALL = ROOT / "demos/run-all.sh"

# run-all.sh invokes "$MAKE -C <demo dir> --no-print-directory <target>". These
# stubs stand in for that call, so the tests exercise the classification rather
# than a real demo.
SKIPPING_DEMO = """#!/usr/bin/env bash
echo "=== Demo 8: SKIPPED -- missing asset: /nonexistent/btrfs-convert ==="
exit 0
"""

PASSING_DEMO = """#!/usr/bin/env bash
echo "=== Demo 3: Chaos Concurrency Testing: SUCCESS ==="
exit 0
"""

FAILING_DEMO = """#!/usr/bin/env bash
echo "=== Demo 3: Chaos Concurrency Testing: FAILURE (exit 1) -- see errors above ==="
exit 1
"""

# Chooses by the make target, the last argument: demo 8 skips, demo 3 fails,
# and any other demo passes.
MIXED_DEMOS = """#!/usr/bin/env bash
case "${@: -1}" in
  demo8)
    echo "=== Demo 8: SKIPPED -- missing asset: /nonexistent/btrfs-convert ==="
    exit 0
    ;;
  demo3)
    echo "=== Demo 3: Chaos Concurrency Testing: FAILURE (exit 1) -- see errors above ==="
    exit 1
    ;;
esac
echo "=== Demo 1: Deterministic Execution: SUCCESS ==="
exit 0
"""

# Skips, and removes its own log while tee still has it open, so the log
# cannot be read once the demo has finished. tee creates the log when it
# starts, which can be a moment after this stub starts, so wait for it. If the
# log never appears, or survives the removal, exit 99: the sweep then records a
# failure, and a test that expects exit 4 fails rather than passing by accident.
SKIPPING_DEMO_WITH_AN_UNREADABLE_LOG = """#!/usr/bin/env bash
log="$DEMO_SWEEP_LOG_DIR/${@: -1}.log"
echo "=== Demo 8: SKIPPED -- missing asset: /nonexistent/btrfs-convert ==="
for _ in $(seq 600); do
  [ -e "$log" ] && break
  sleep 0.1
done
[ -e "$log" ] || exit 99
rm -f -- "$log"
[ ! -e "$log" ] || exit 99
exit 0
"""

# Passes, and replaces summary.tsv with a directory, so that the sweep cannot
# append this demo's row.
PASSING_DEMO_THAT_BLOCKS_THE_SUMMARY = """#!/usr/bin/env bash
summary="$DEMO_SWEEP_LOG_DIR/summary.tsv"
rm -f -- "$summary"
mkdir -- "$summary" || exit 99
echo "=== Demo 3: Chaos Concurrency Testing: SUCCESS ==="
exit 0
"""

# Saves its run as the reference run and stops, so it compared nothing. Demo 5
# prints these lines when it has no reference run and QEMU_BOOT_REPEAT=0.
FIRST_RUN_ONLY_DEMO = """#!/usr/bin/env bash
echo
echo "=== Automatic repeat verification ==="
echo "Saved this run as the reference run at ignored/qemu-linux/boot-anchor"
echo
echo "=== Demo 5: QEMU Linux Snapshot: FIRST RUN SAVED ==="
exit 0
"""

# The same, after guest serial output that holds a NUL byte. grep takes such a
# log for binary and, without -a, prints none of its matching lines.
FIRST_RUN_ONLY_DEMO_WITH_BINARY_OUTPUT = """#!/usr/bin/env bash
printf 'serial: \\000\\001\\n'
echo "=== Demo 5: QEMU Linux Snapshot: FIRST RUN SAVED ==="
exit 0
"""

# Saves its run as the reference run, runs again, and compares the second run
# with it, as demo 5 does by default when it has no reference run.
FIRST_RUN_THEN_COMPARED_DEMO = """#!/usr/bin/env bash
echo
echo "=== Automatic repeat verification ==="
echo "Saved this run as the reference run at ignored/qemu-linux/boot-anchor"
echo
echo "=== Demo 5: QEMU Linux Snapshot: FIRST RUN SAVED ==="
echo
echo "=== Boot again and compare with the reference run just saved ==="
echo "Comparing with the reference run."
echo
echo "=== Demo 5: QEMU Linux Snapshot: SUCCESS ==="
exit 0
"""

# Chooses by the make target: demo 5 saves its first run and compares nothing,
# demo 6 saves its first run and then compares, demo 8 skips, demo 3 fails,
# and any other demo passes.
MIXED_DEMOS_WITH_FIRST_RUNS = """#!/usr/bin/env bash
case "${@: -1}" in
  demo5)
    echo "=== Demo 5: QEMU Linux Snapshot: FIRST RUN SAVED ==="
    exit 0
    ;;
  demo6)
    echo "=== Demo 6: QEMU Snapshot Resume: FIRST RUN SAVED ==="
    echo "=== Demo 6: QEMU Snapshot Resume: SUCCESS ==="
    exit 0
    ;;
  demo8)
    echo "=== Demo 8: SKIPPED -- missing asset: /nonexistent/btrfs-convert ==="
    exit 0
    ;;
  demo3)
    echo "=== Demo 3: Chaos Concurrency Testing: FAILURE (exit 1) -- see errors above ==="
    exit 1
    ;;
esac
echo "=== Demo 1: Deterministic Execution: SUCCESS ==="
exit 0
"""

# Prints the repeat variables it was started with, then passes.
DEMO_THAT_PRINTS_ITS_REPEAT_VARIABLES = """#!/usr/bin/env bash
echo "QEMU_BOOT_REPEAT=${QEMU_BOOT_REPEAT-unset}"
echo "QEMU_RESUME_REPEAT=${QEMU_RESUME_REPEAT-unset}"
echo "=== Demo 5: QEMU Linux Snapshot: SUCCESS ==="
exit 0
"""

# Exits 0 and prints nothing: a demo that stopped before it reached a result.
SILENT_DEMO = """#!/usr/bin/env bash
exit 0
"""

# Exits 0 after printing ordinary output but no result line.
DEMO_WITH_OUTPUT_BUT_NO_RESULT_LINE = """#!/usr/bin/env bash
echo "=== Demo 3: Chaos Concurrency Testing ==="
echo "PASS: something that is not the demo's result"
exit 0
"""

# Exits 0 after printing a PARTIAL result line: it did not succeed.
PARTIAL_DEMO_THAT_EXITS_0 = """#!/usr/bin/env bash
echo "=== Demo 6: QEMU Snapshot Resume: PARTIAL ==="
exit 0
"""

# Exits 0 after only demo 5's result lines. make runs demo 5 first when demos 6
# and 7 need its boot snapshot, so their logs can hold demo 5's lines.
DEMO_THAT_PRINTS_ONLY_ANOTHER_DEMOS_SUCCESS = """#!/usr/bin/env bash
echo "=== Demo 5: QEMU Linux Snapshot: SUCCESS ==="
exit 0
"""

# Exits 0 after only demo 5's FIRST RUN SAVED line, run first for its boot
# snapshot: the demo itself printed no result, so it did not succeed, and the
# first run it reports is demo 5's, not its own.
DEMO_THAT_PRINTS_ONLY_ANOTHER_DEMOS_FIRST_RUN = """#!/usr/bin/env bash
echo "=== Demo 5: QEMU Linux Snapshot: FIRST RUN SAVED ==="
exit 0
"""

# Demo 5 runs first and saves, then compares; then the demo itself succeeds.
DEMO_THAT_SUCCEEDS_AFTER_DEMO_5_RAN_FIRST = """#!/usr/bin/env bash
echo "=== Demo 5: QEMU Linux Snapshot: FIRST RUN SAVED ==="
echo "=== Demo 5: QEMU Linux Snapshot: SUCCESS ==="
case "${@: -1}" in
  demo6) echo "=== Demo 6: QEMU Snapshot Resume: SUCCESS ===" ;;
  demo7) echo "=== Demo 7: drgn Kernel Task Evolution: SUCCESS ===" ;;
esac
exit 0
"""

# Prints the skip status the sweep asked for, then passes.
DEMO_THAT_PRINTS_ITS_SKIP_EXIT_STATUS = """#!/usr/bin/env bash
echo "DEMO_SKIP_EXIT_STATUS=${DEMO_SKIP_EXIT_STATUS-unset}"
echo "=== Demo 3: Chaos Concurrency Testing: SUCCESS ==="
exit 0
"""

# Runs the real demo 8 run.sh, which skips when its assets are absent.
REAL_DEMO_8 = """#!/usr/bin/env bash
exec REAL_RUN_SH
"""

# Stands in for grep on PATH. It fails as grep does when it cannot read a file,
# but only when searching for result lines (the pattern names FIRST RUN SAVED);
# every other call goes to the real grep, whose path replaces REAL_GREP.
GREP_THAT_CANNOT_READ_RESULT_LINES = """#!/usr/bin/env bash
for argument in "$@"; do
  case "$argument" in
    *"FIRST RUN SAVED"*)
      echo "grep: simulated read error" >&2
      exit 2
      ;;
  esac
done
exec REAL_GREP "$@"
"""

Sweep = collections.namedtuple("Sweep", "result summary calls")


def real_success_lines():
    """Return {N: the success line demo N prints}, read from the demo's source.

    Demos 1-4 set DEMO_LABEL in run.sh and print it with common.sh's
    demo_success; demos 5 and 6 set DEMO_LABEL in run.py and print
    `=== {label}: {result} ===`; demos 7-9 echo the line literally. The value
    is None when the line cannot be found, so a caller fails rather than check
    fewer demos.
    """
    lines = {}
    for directory in sorted((ROOT / "demos").glob("[0-9][0-9]-*")):
        number = int(directory.name[:2])
        run_sh = directory / "run.sh"
        run_py = directory / "run.py"
        shell = run_sh.read_text() if run_sh.is_file() else ""
        python = run_py.read_text() if run_py.is_file() else ""
        label_pattern = r'^DEMO_LABEL ?= ?"(Demo {}: [^"\n]+)"$'.format(number)
        shell_label = re.search(label_pattern, shell, re.M)
        python_label = re.search(label_pattern, python, re.M)
        literal = re.search(
            r'^echo "(=== Demo {}: [^"\n]+: SUCCESS ===)"$'.format(number),
            shell,
            re.M,
        )
        line = None
        if shell_label is not None and re.search(r"^demo_success$", shell, re.M):
            line = "=== {}: SUCCESS ===".format(shell_label.group(1))
        elif python_label is not None and (
            '=== {}: {} ===".format(DEMO_LABEL, result)' in python
        ):
            line = "=== {}: SUCCESS ===".format(python_label.group(1))
        elif literal is not None:
            line = literal.group(1)
        lines[number] = line
    return lines


class RunAllSkipClassificationTest(unittest.TestCase):
    def _run(self, stub_body, target="demo8", args=(), prepare=None):
        """Run the sweep against a stub make.

        prepare(scratch, environment), if given, runs before the sweep and may
        change the scratch directory or the environment. Returns the completed
        process, the text of summary.tsv (None if it is not a readable file),
        and the make targets that the stub was called with, in order.
        """
        with tempfile.TemporaryDirectory() as td:
            scratch = Path(td)
            body = scratch / "demo-stub"
            body.write_text(stub_body)
            body.chmod(0o755)
            calls = scratch / "make-calls"
            stub = scratch / "fake-make"
            stub.write_text(
                "#!/usr/bin/env bash\n"
                f"printf '%s\\n' \"${{@: -1}}\" >>{shlex.quote(str(calls))}\n"
                f'exec {shlex.quote(str(body))} "$@"\n'
            )
            stub.chmod(0o755)
            log_dir = scratch / "logs"
            environment = os.environ.copy()
            environment.update(
                {
                    "MAKE": str(stub),
                    "DEMO_SWEEP_TARGETS": target,
                    "DEMO_SWEEP_LOG_DIR": str(log_dir),
                    "DEMO_TMP": str(scratch / "demo-tmp"),
                }
            )
            environment.pop("GITHUB_STEP_SUMMARY", None)
            environment.pop("GITHUB_ACTIONS", None)
            if prepare is not None:
                prepare(scratch, environment)
            result = subprocess.run(
                [str(RUN_ALL), *args],
                cwd=str(ROOT),
                env=environment,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
                timeout=120,
            )
            summary_path = Path(environment["DEMO_SWEEP_LOG_DIR"]) / "summary.tsv"
            summary = summary_path.read_text() if summary_path.is_file() else None
            made = calls.read_text().split() if calls.exists() else []
            return Sweep(result, summary, made)

    def _sweep(self, stub_body, target="demo8", args=()):
        sweep = self._run(stub_body, target=target, args=args)
        self.assertIsNotNone(sweep.summary, sweep.result.stdout)
        return sweep.result, sweep.summary

    @staticmethod
    def _statuses(summary):
        rows = [line.split("\t") for line in summary.strip().splitlines()[1:]]
        return [row[1] for row in rows]

    def assertNeverAPassOrSuccess(self, sweep):
        output = sweep.result.stdout
        self.assertNotIn(": PASS", output)
        self.assertNotIn("Demo suite: SUCCESS", output)
        if sweep.summary is not None:
            self.assertNotIn("PASS", self._statuses(sweep.summary))

    def test_a_skipped_demo_is_not_recorded_as_a_pass(self):
        result, summary = self._sweep(SKIPPING_DEMO)
        self.assertEqual(self._statuses(summary), ["SKIP"])
        # A sweep with a skipped demo is incomplete. It exits 3, which is
        # neither a pass (0) nor a failure (1), so a caller that reads only the
        # exit status does not take it for a success.
        self.assertEqual(result.returncode, 3, result.stdout)
        self.assertNotIn("PASS", summary)
        self.assertIn("SKIPPED", result.stdout)

    def test_the_headline_never_counts_a_skip_as_passed(self):
        result, _ = self._sweep(SKIPPING_DEMO)
        self.assertIn(
            "Demo suite: INCOMPLETE — 0 of 1 requested demos passed, "
            "1 skipped and unmeasured",
            result.stdout,
        )
        self.assertNotIn("all 1 requested demos passed", result.stdout)

    def test_the_skipped_demos_are_named(self):
        result, summary = self._sweep(MIXED_DEMOS, target="demo1 demo8")
        self.assertEqual(self._statuses(summary), ["PASS", "SKIP"])
        self.assertEqual(result.returncode, 3, result.stdout)
        self.assertIn(
            "Demo suite: INCOMPLETE — 1 of 2 requested demos passed, "
            "1 skipped and unmeasured",
            result.stdout,
        )
        self.assertIn("Skipped, with no result: demo8\n", result.stdout)
        self.assertNotIn("all 2 requested demos passed", result.stdout)

    def test_there_is_no_option_that_accepts_a_skip(self):
        """--allow-skips was removed: it is refused as a usage error."""
        sweep = self._run(SKIPPING_DEMO, args=("--allow-skips",))
        self.assertEqual(sweep.result.returncode, 2, sweep.result.stdout)
        self.assertIn("Usage: demos/run-all.sh", sweep.result.stdout)
        self.assertEqual(sweep.calls, [], "no demo may run after a usage error")
        self.assertNotIn("INCOMPLETE", sweep.result.stdout)

    def test_the_removed_option_is_refused_even_with_a_failure(self):
        sweep = self._run(
            MIXED_DEMOS, target="demo3 demo8", args=("--allow-skips",)
        )
        self.assertEqual(sweep.result.returncode, 2, sweep.result.stdout)
        self.assertIn("Usage: demos/run-all.sh", sweep.result.stdout)
        self.assertEqual(sweep.calls, [], "no demo may run after a usage error")

    def test_a_real_pass_is_still_a_pass(self):
        """Positive control: the classifier must not label every demo SKIP."""
        result, summary = self._sweep(PASSING_DEMO, target="demo3")
        self.assertEqual(self._statuses(summary), ["PASS"])
        self.assertEqual(result.returncode, 0)
        self.assertIn("all 1 requested demos passed", result.stdout)
        self.assertNotIn("INCOMPLETE", result.stdout)

    def test_a_genuine_failure_is_still_a_failure(self):
        """A skip classification must not swallow a real red."""
        result, summary = self._sweep(FAILING_DEMO, target="demo3")
        self.assertEqual(self._statuses(summary), ["FAIL"])
        self.assertEqual(result.returncode, 1)
        self.assertIn("1 demo(s) failed", result.stdout)

    def assertExitZeroWithoutSuccessFails(self, sweep, demo):
        self.assertEqual(sweep.calls, [demo])
        self.assertIsNotNone(sweep.summary, sweep.result.stdout)
        self.assertEqual(self._statuses(sweep.summary), ["FAIL"])
        # The row keeps the demo's real exit status, 0.
        row = sweep.summary.strip().splitlines()[1].split("\t")
        self.assertEqual(row[2], "0", sweep.summary)
        self.assertEqual(sweep.result.returncode, 1, sweep.result.stdout)
        self.assertIn(
            '=== {}: FAIL (exit 0 without its own "=== Demo {}: <title>: '
            'SUCCESS ===" line'.format(demo, demo[len("demo") :]),
            sweep.result.stdout,
        )
        self.assertIn(
            "Demo suite: FAILURE — 1 demo(s) failed, 0 passed, 0 skipped",
            sweep.result.stdout,
        )
        self.assertNeverAPassOrSuccess(sweep)
        self.assertNotIn("INCOMPLETE", sweep.result.stdout)

    def test_a_demo_that_exits_0_silently_fails(self):
        sweep = self._run(SILENT_DEMO, target="demo3")
        self.assertExitZeroWithoutSuccessFails(sweep, "demo3")

    def test_output_without_a_result_line_is_not_a_pass(self):
        sweep = self._run(DEMO_WITH_OUTPUT_BUT_NO_RESULT_LINE, target="demo3")
        self.assertExitZeroWithoutSuccessFails(sweep, "demo3")

    def test_a_partial_result_that_exits_0_is_not_a_pass(self):
        sweep = self._run(PARTIAL_DEMO_THAT_EXITS_0, target="demo6")
        self.assertExitZeroWithoutSuccessFails(sweep, "demo6")

    def test_another_demos_success_line_is_not_a_pass(self):
        sweep = self._run(DEMO_THAT_PRINTS_ONLY_ANOTHER_DEMOS_SUCCESS, target="demo7")
        self.assertExitZeroWithoutSuccessFails(sweep, "demo7")

    def test_another_demos_first_run_is_not_the_demos_result(self):
        sweep = self._run(
            DEMO_THAT_PRINTS_ONLY_ANOTHER_DEMOS_FIRST_RUN, target="demo7"
        )
        self.assertExitZeroWithoutSuccessFails(sweep, "demo7")

    def test_a_silent_demo_still_outranks_a_skip(self):
        silent_demo3_and_skipping_demo8 = (
            "#!/usr/bin/env bash\n"
            'case "${@: -1}" in\n'
            '  demo8) echo "=== Demo 8: SKIPPED -- missing asset: /x ===" ;;\n'
            "esac\n"
            "exit 0\n"
        )
        sweep = self._run(silent_demo3_and_skipping_demo8, target="demo3 demo8")
        self.assertEqual(sweep.calls, ["demo3", "demo8"])
        self.assertEqual(self._statuses(sweep.summary), ["FAIL", "SKIP"])
        self.assertEqual(sweep.result.returncode, 1, sweep.result.stdout)
        self.assertIn("1 demo(s) failed, 0 passed, 1 skipped", sweep.result.stdout)

    def test_a_success_after_demo_5_ran_first_is_a_pass(self):
        """Positive control: demo 5's result lines, from make running demo 5
        first for its boot snapshot, do not hide the demo's own SUCCESS."""
        result, summary = self._sweep(
            DEMO_THAT_SUCCEEDS_AFTER_DEMO_5_RAN_FIRST, target="demo6 demo7"
        )
        self.assertEqual(self._statuses(summary), ["PASS", "PASS"])
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("Demo suite: SUCCESS — all 2 requested demos passed", result.stdout)

    def test_every_demos_real_success_line_is_a_pass(self):
        """Positive control: no demo that passes today turns red. Each demo's
        own success line, read from its source, is a pass for its target."""
        lines = real_success_lines()
        self.assertEqual(sorted(lines), list(range(1, 10)))
        self.assertEqual(
            [number for number, line in lines.items() if line is None],
            [],
            "a demo's success line was not found in its source",
        )
        common = (ROOT / "demos/lib/common.sh").read_text()
        self.assertIn(
            "demo_success() { printf '\\n=== %s: SUCCESS ===\\n' "
            '"${DEMO_LABEL:-demo}"; }',
            common,
        )
        stub = '#!/usr/bin/env bash\ncase "${@: -1}" in\n'
        for number, line in sorted(lines.items()):
            stub += "  demo{}) echo; echo {} ;;\n".format(number, shlex.quote(line))
        stub += "esac\nexit 0\n"
        targets = " ".join("demo{}".format(number) for number in sorted(lines))
        result, summary = self._sweep(stub, target=targets)
        self.assertEqual(self._statuses(summary), ["PASS"] * 9, result.stdout)
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("Demo suite: SUCCESS — all 9 requested demos passed", result.stdout)

    def test_a_target_that_is_not_a_demo_is_refused(self):
        for target in ("demo3 lint", "demo3 demo", "demo3 demo08", "demo3 demo3x"):
            with self.subTest(target=target):
                sweep = self._run(PASSING_DEMO, target=target)
                self.assertEqual(sweep.result.returncode, 2, sweep.result.stdout)
                self.assertEqual(sweep.calls, [], "no demo may run after a usage error")
                self.assertIn("is not a demo target", sweep.result.stdout)
                self.assertNeverAPassOrSuccess(sweep)

    def test_the_sweep_asks_demos_for_exit_0_on_a_skip(self):
        """Run on its own, a demo that skips exits 77. make would turn that
        into 2, which the sweep could not tell from a failure, so the sweep
        asks for 0, whatever the caller set, and reads the SKIPPED line."""

        def caller_asked_for_77(scratch, environment):
            environment["DEMO_SKIP_EXIT_STATUS"] = "77"

        sweep = self._run(
            DEMO_THAT_PRINTS_ITS_SKIP_EXIT_STATUS,
            target="demo3",
            prepare=caller_asked_for_77,
        )
        self.assertIn("\nDEMO_SKIP_EXIT_STATUS=0\n", sweep.result.stdout)
        self.assertEqual(sweep.result.returncode, 0, sweep.result.stdout)

    def test_demo_8s_real_skip_is_a_skip_in_the_sweep(self):
        """Demo 8's own run.sh, without its assets, is recorded as SKIP and
        the sweep is INCOMPLETE with exit 3, not a failure and not a pass."""

        def without_demo_8_assets(scratch, environment):
            for key in list(environment):
                if key.startswith("DEMO08_"):
                    del environment[key]
            environment["DEMO08_DIR"] = str(scratch / "no-demo8-assets")
            environment["DEMO08_ARTIFACTS"] = str(scratch / "demo8-artifacts")

        stub = REAL_DEMO_8.replace(
            "REAL_RUN_SH", shlex.quote(str(ROOT / "demos/08-btrfs-convert-uaf/run.sh"))
        )
        sweep = self._run(stub, target="demo8", prepare=without_demo_8_assets)
        self.assertIn("=== Demo 8: SKIPPED -- missing asset:", sweep.result.stdout)
        self.assertIsNotNone(sweep.summary, sweep.result.stdout)
        self.assertEqual(self._statuses(sweep.summary), ["SKIP"])
        self.assertEqual(sweep.result.returncode, 3, sweep.result.stdout)
        self.assertIn(
            "Demo suite: INCOMPLETE — 0 of 1 requested demos passed, "
            "1 skipped and unmeasured",
            sweep.result.stdout,
        )
        self.assertNeverAPassOrSuccess(sweep)

    def test_a_log_directory_that_cannot_be_created_stops_the_sweep(self):
        def log_dir_below_a_file(scratch, environment):
            blocker = scratch / "a-regular-file"
            blocker.write_text("")
            environment["DEMO_SWEEP_LOG_DIR"] = str(blocker / "logs")

        sweep = self._run(SKIPPING_DEMO, prepare=log_dir_below_a_file)
        self.assertEqual(sweep.result.returncode, 4, sweep.result.stdout)
        self.assertEqual(sweep.calls, [], "no demo may run without its log")
        self.assertIn("cannot create the log directory", sweep.result.stdout)
        self.assertIn("Set DEMO_SWEEP_LOG_DIR", sweep.result.stdout)
        self.assertIn("Demo suite: ERROR — no demo was run", sweep.result.stdout)
        self.assertNeverAPassOrSuccess(sweep)

    def test_a_summary_that_cannot_be_written_stops_the_sweep(self):
        def summary_is_a_directory(scratch, environment):
            (Path(environment["DEMO_SWEEP_LOG_DIR"]) / "summary.tsv").mkdir(
                parents=True
            )

        sweep = self._run(SKIPPING_DEMO, prepare=summary_is_a_directory)
        self.assertEqual(sweep.result.returncode, 4, sweep.result.stdout)
        self.assertEqual(sweep.calls, [], "no demo may run without a summary")
        self.assertIn("cannot write the summary", sweep.result.stdout)
        self.assertIn("Demo suite: ERROR — no demo was run", sweep.result.stdout)
        self.assertNeverAPassOrSuccess(sweep)

    def test_a_log_that_cannot_be_written_is_an_error_not_a_pass(self):
        def log_is_a_directory(scratch, environment):
            (Path(environment["DEMO_SWEEP_LOG_DIR"]) / "demo8.log").mkdir(
                parents=True
            )

        sweep = self._run(SKIPPING_DEMO, prepare=log_is_a_directory)
        self.assertEqual(sweep.calls, ["demo8"])
        self.assertEqual(sweep.result.returncode, 4, sweep.result.stdout)
        self.assertEqual(self._statuses(sweep.summary), ["ERROR"])
        self.assertIn("tee could not write the log", sweep.result.stdout)
        self.assertIn(
            "Demo suite: ERROR — 0 of 1 requested demos passed, 0 skipped, "
            "1 unknown",
            sweep.result.stdout,
        )
        self.assertIn(
            "Unknown, because the log could not be read: demo8\n",
            sweep.result.stdout,
        )
        self.assertNeverAPassOrSuccess(sweep)
        self.assertNotIn("INCOMPLETE", sweep.result.stdout)

    def test_a_skip_whose_log_cannot_be_read_is_never_a_pass(self):
        sweep = self._run(SKIPPING_DEMO_WITH_AN_UNREADABLE_LOG)
        self.assertEqual(sweep.calls, ["demo8"])
        # The demo really did print its SKIPPED line (tee copied it here), so
        # a PASS would hide the skip.
        self.assertIn("=== Demo 8: SKIPPED", sweep.result.stdout)
        self.assertEqual(sweep.result.returncode, 4, sweep.result.stdout)
        self.assertEqual(self._statuses(sweep.summary), ["ERROR"])
        self.assertIn("grep could not read the log", sweep.result.stdout)
        self.assertIn(
            "Unknown, because the log could not be read: demo8\n",
            sweep.result.stdout,
        )
        self.assertNeverAPassOrSuccess(sweep)

    def test_an_unreadable_log_does_not_hide_a_failure(self):
        def log_is_a_directory(scratch, environment):
            (Path(environment["DEMO_SWEEP_LOG_DIR"]) / "demo3.log").mkdir(
                parents=True
            )

        sweep = self._run(
            MIXED_DEMOS, target="demo3 demo8", prepare=log_is_a_directory
        )
        self.assertEqual(sweep.result.returncode, 1, sweep.result.stdout)
        self.assertEqual(self._statuses(sweep.summary), ["FAIL", "SKIP"])
        self.assertIn("1 demo(s) failed, 0 passed, 1 skipped", sweep.result.stdout)
        self.assertIn("tee could not write the log", sweep.result.stdout)

    def test_a_summary_row_that_cannot_be_appended_is_an_error(self):
        sweep = self._run(PASSING_DEMO_THAT_BLOCKS_THE_SUMMARY, target="demo3")
        self.assertEqual(sweep.calls, ["demo3"])
        self.assertEqual(sweep.result.returncode, 4, sweep.result.stdout)
        self.assertIn("could not append its row to the summary", sweep.result.stdout)
        self.assertIn(
            "Demo suite: ERROR — 1 of 1 requested demos passed, 0 skipped, "
            "0 unknown",
            sweep.result.stdout,
        )
        self.assertNotIn("Demo suite: SUCCESS", sweep.result.stdout)

    def test_a_github_step_summary_that_cannot_be_written_is_an_error(self):
        def step_summary_is_a_directory(scratch, environment):
            step_summary = scratch / "step-summary"
            step_summary.mkdir()
            environment["GITHUB_STEP_SUMMARY"] = str(step_summary)

        sweep = self._run(
            PASSING_DEMO, target="demo3", prepare=step_summary_is_a_directory
        )
        self.assertEqual(sweep.result.returncode, 4, sweep.result.stdout)
        self.assertEqual(self._statuses(sweep.summary), ["PASS"])
        self.assertIn("could not write the GitHub step summary", sweep.result.stdout)
        self.assertNotIn("Demo suite: SUCCESS", sweep.result.stdout)

    def test_a_demo_that_only_saved_its_first_run_is_not_a_pass(self):
        sweep = self._run(FIRST_RUN_ONLY_DEMO, target="demo5")
        self.assertEqual(sweep.calls, ["demo5"])
        self.assertIsNotNone(sweep.summary, sweep.result.stdout)
        self.assertEqual(self._statuses(sweep.summary), ["UNCOMPARED"])
        # It compared nothing, so the sweep is incomplete and exits 3, as it
        # does for a skip.
        self.assertEqual(sweep.result.returncode, 3, sweep.result.stdout)
        self.assertIn(
            "Demo suite: INCOMPLETE — 0 of 1 requested demos passed, "
            "0 skipped and unmeasured, 1 saved a first run and compared nothing ===",
            sweep.result.stdout,
        )
        self.assertIn(
            "Saved a first run and compared nothing: demo5\n", sweep.result.stdout
        )
        self.assertNeverAPassOrSuccess(sweep)

    def test_a_first_run_after_binary_output_is_still_found(self):
        sweep = self._run(FIRST_RUN_ONLY_DEMO_WITH_BINARY_OUTPUT, target="demo5")
        self.assertIsNotNone(sweep.summary, sweep.result.stdout)
        self.assertEqual(self._statuses(sweep.summary), ["UNCOMPARED"])
        self.assertEqual(sweep.result.returncode, 3, sweep.result.stdout)
        self.assertNeverAPassOrSuccess(sweep)

    def test_a_first_run_followed_by_a_comparison_is_a_pass(self):
        """Positive control: only the last result line counts."""
        result, summary = self._sweep(FIRST_RUN_THEN_COMPARED_DEMO, target="demo5")
        self.assertEqual(self._statuses(summary), ["PASS"])
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("Demo suite: SUCCESS — all 1 requested demos passed", result.stdout)
        self.assertNotIn("INCOMPLETE", result.stdout)

    def test_skips_and_first_runs_are_counted_and_named(self):
        result, summary = self._sweep(
            MIXED_DEMOS_WITH_FIRST_RUNS, target="demo1 demo5 demo6 demo8"
        )
        self.assertEqual(
            self._statuses(summary), ["PASS", "UNCOMPARED", "PASS", "SKIP"]
        )
        self.assertEqual(result.returncode, 3, result.stdout)
        self.assertIn(
            "Demo suite: INCOMPLETE — 2 of 4 requested demos passed, "
            "1 skipped and unmeasured, 1 saved a first run and compared nothing ===",
            result.stdout,
        )
        self.assertIn("Skipped, with no result: demo8\n", result.stdout)
        self.assertIn("Saved a first run and compared nothing: demo5\n", result.stdout)
        self.assertNotIn("Demo suite: SUCCESS", result.stdout)

    def test_a_failure_still_outranks_a_first_run(self):
        result, summary = self._sweep(
            MIXED_DEMOS_WITH_FIRST_RUNS, target="demo3 demo5"
        )
        self.assertEqual(self._statuses(summary), ["FAIL", "UNCOMPARED"])
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn(
            "Demo suite: FAILURE — 1 demo(s) failed, 0 passed, 0 skipped, "
            "1 saved a first run and compared nothing ===",
            result.stdout,
        )
        self.assertIn("Saved a first run and compared nothing: demo5\n", result.stdout)

    def _put_grep_that_cannot_read_result_lines_on_path(self, scratch, environment):
        real_grep = shutil.which("grep", path=environment["PATH"])
        self.assertIsNotNone(real_grep, "no grep on PATH")
        bin_dir = scratch / "bin"
        bin_dir.mkdir()
        fake_grep = bin_dir / "grep"
        fake_grep.write_text(
            GREP_THAT_CANNOT_READ_RESULT_LINES.replace(
                "REAL_GREP", shlex.quote(real_grep)
            )
        )
        fake_grep.chmod(0o755)
        environment["PATH"] = f"{bin_dir}{os.pathsep}{environment['PATH']}"

    def test_a_log_whose_result_lines_cannot_be_read_is_an_error(self):
        sweep = self._run(
            FIRST_RUN_ONLY_DEMO,
            target="demo5",
            prepare=self._put_grep_that_cannot_read_result_lines_on_path,
        )
        self.assertEqual(sweep.calls, ["demo5"])
        self.assertEqual(sweep.result.returncode, 4, sweep.result.stdout)
        self.assertEqual(self._statuses(sweep.summary), ["ERROR"])
        self.assertIn("grep could not read the log", sweep.result.stdout)
        self.assertIn(
            "Unknown, because the log could not be read: demo5\n",
            sweep.result.stdout,
        )
        self.assertNeverAPassOrSuccess(sweep)
        self.assertNotIn("INCOMPLETE", sweep.result.stdout)

    def test_the_failing_grep_leaves_the_skipped_search_alone(self):
        """Positive control for the test above: the stand-in grep fails only
        the search for result lines. The search for a SKIPPED line still
        works, so the error in that test comes from the later search."""
        sweep = self._run(
            SKIPPING_DEMO, prepare=self._put_grep_that_cannot_read_result_lines_on_path
        )
        self.assertEqual(sweep.result.returncode, 3, sweep.result.stdout)
        self.assertEqual(self._statuses(sweep.summary), ["SKIP"])

    def test_demos_are_always_asked_to_compare_a_first_run(self):
        def caller_turned_the_repeat_off(scratch, environment):
            environment["QEMU_BOOT_REPEAT"] = "0"
            environment["QEMU_RESUME_REPEAT"] = "0"

        sweep = self._run(
            DEMO_THAT_PRINTS_ITS_REPEAT_VARIABLES,
            target="demo5",
            prepare=caller_turned_the_repeat_off,
        )
        self.assertIn("\nQEMU_BOOT_REPEAT=1\n", sweep.result.stdout)
        self.assertIn("\nQEMU_RESUME_REPEAT=1\n", sweep.result.stdout)
        self.assertEqual(sweep.result.returncode, 0, sweep.result.stdout)

    def test_the_usage_documents_a_demo_that_compared_nothing(self):
        result = subprocess.run(
            [str(RUN_ALL), "--help"],
            cwd=str(ROOT),
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            timeout=60,
        )
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn(
            "\n  3  no selected demo failed, but at least one was skipped\n"
            "     or saved its first run and compared nothing\n",
            result.stdout,
        )
        prose = " ".join(result.stdout.split())
        for phrase in (
            "sets QEMU_BOOT_REPEAT=1 and QEMU_RESUME_REPEAT=1",
            "A demo whose last result line still says FIRST RUN SAVED compared "
            "nothing and is recorded as UNCOMPARED.",
        ):
            with self.subTest(phrase=phrase):
                self.assertIn(phrase, prose)

    def test_the_usage_documents_that_exit_0_alone_is_not_a_pass(self):
        result = subprocess.run(
            [str(RUN_ALL), "--help"],
            cwd=str(ROOT),
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            timeout=60,
        )
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn(
            "\n  1  at least one selected demo failed\n"
            "     (including one that exited 0 without its own SUCCESS line)\n",
            result.stdout,
        )
        prose = " ".join(result.stdout.split())
        self.assertIn(
            "A demo passes only when it exits 0 and its own last result line, "
            "`=== Demo N: <title>: SUCCESS ===` with N its own number, says "
            "SUCCESS. A demo that exits 0 without that line",
            prose,
        )
        self.assertIn("is recorded as FAIL.", prose)

    def test_the_usage_documents_the_exit_statuses(self):
        result = subprocess.run(
            [str(RUN_ALL), "--help"],
            cwd=str(ROOT),
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            timeout=60,
        )
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertNotIn("--allow-skips", result.stdout)
        for line in (
            "  0  every selected demo passed\n",
            "  1  at least one selected demo failed\n",
            "  2  usage error\n",
            "  3  no selected demo failed, but at least one was skipped\n",
            "  4  no selected demo failed, but the sweep could not create, write, or read\n",
            "When more than one applies, 1 comes before 4, and 4 before 3.",
        ):
            with self.subTest(line=line):
                self.assertIn("\n" + line, result.stdout)


if __name__ == "__main__":
    unittest.main()
