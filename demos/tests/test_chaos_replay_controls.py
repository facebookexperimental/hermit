#!/usr/bin/env python3
"""Tests for the checks in demo 3's last step, the schedule replay.

Demo 3 records a failing chaos run of hello_race (seed 0) with
`--record-preemptions-to=FILE` and replays FILE with
`--replay-preemptions-from=FILE`. The replay runs under seed 1, which passes
without the file, so a failing replay shows that the file chose the failing
order. The demo also checks that the replay started its virtual clock at the
instant stored in the file: Hermit prints
`hermit: virtual-time epoch=... source=recording` for such a run.

Each test puts a stub `hermit` first on PATH and runs the real
demos/03-chaos-concurrency/run.sh. The stub fails for the seeds the demo
expects to fail (0, 5, 6, 12 and 15), stores the recorded seed and clock start
in FILE, and replays FILE in one of three ways: faithfully, ignoring FILE (the
outcome then follows the replay's own seed), or ignoring the stored clock
start. It records every call's arguments and whether HERMIT_EPOCH and
HERMIT_LOG_FILE were set. No real Hermit, guest build, or performance counters
are involved. Run directly (`python3 demos/tests/test_chaos_replay_controls.py`)
or via `make -C demos test`.
"""

import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

DEMO_DIR = Path(__file__).resolve().parent.parent
RUN = DEMO_DIR / "03-chaos-concurrency" / "run.sh"
SUCCESS_LINE = "=== Demo 3: Chaos Concurrency Testing: SUCCESS ==="

STUB_HERMIT = r'''#!{python}
import json, os, sys
from pathlib import Path

FAILING = {{0, 5, 6, 12, 15}}
args = sys.argv[1:]
if args == ["--version"]:
    print("hermit 0.0.0 (stub)")
    sys.exit(0)
log = Path(os.environ["STUB_LOG"])
with log.open("a") as handle:
    handle.write(json.dumps({{
        "argv": args,
        "HERMIT_EPOCH": os.environ.get("HERMIT_EPOCH"),
        "HERMIT_LOG_FILE": os.environ.get("HERMIT_LOG_FILE"),
    }}) + "\n")
calls = len(log.read_text().splitlines())


def option(name):
    prefix = "--" + name + "="
    found = [arg[len(prefix):] for arg in args if arg.startswith(prefix)]
    return found[-1] if found else None


# Real Hermit: --seed defaults to HERMIT_PRNG, else 0.
seed = int(option("seed") or os.environ.get("HERMIT_PRNG") or 0)
record = option("record-preemptions-to")
replay = option("replay-preemptions-from")
explicit = os.environ.get("HERMIT_EPOCH")
host_now = "2026-10-03T12:00:{{:02d}}.000000000+00:00".format(calls)
outcome_seed = seed
if replay is None:
    epoch, source = (explicit, "explicit") if explicit else (host_now, "host-now")
else:
    stored = json.loads(Path(replay).read_text())
    mode = os.environ["STUB_REPLAY"]
    if explicit and explicit != stored["epoch"]:
        print("Error: the explicit virtual-time epoch differs from the recorded one", file=sys.stderr)
        sys.exit(2)
    epoch, source = (explicit, "explicit") if explicit else (stored["epoch"], "recording")
    if mode == "faithful":
        outcome_seed = stored["seed"]
    elif mode == "ignores-file":
        pass
    elif mode == "host-clock":
        outcome_seed = stored["seed"]
        epoch, source = host_now, "host-now"
    else:
        sys.exit("unknown STUB_REPLAY " + mode)
line = "hermit: virtual-time epoch={{0}} source={{1}}; reproduce with --epoch={{0}}".format(epoch, source)
log_file = os.environ.get("HERMIT_LOG_FILE")
if log_file:
    with open(log_file, "a") as handle:
        handle.write("2026-10-03T12:00:00.000000Z DEBUG hermit::controller: " + line + "\n")
else:
    print(line, file=sys.stderr)
if record is not None:
    Path(record).write_text(json.dumps({{"seed": seed, "epoch": epoch}}))
if outcome_seed in FAILING:
    print("Final value: 1")
    print("Antagonistic schedule reached, failing.")
    sys.exit(1)
print("Final value: 2")
print("Did not find antagonistic schedule. Succeeding.")
'''


class ChaosReplayControlsTest(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.directory = Path(directory.name)
        self.bin = self.directory / "bin"
        self.bin.mkdir()
        hermit = self.bin / "hermit"
        hermit.write_text(STUB_HERMIT.format(python=sys.executable))
        hermit.chmod(0o755)
        # common.sh requires cargo and cc on PATH; DEMO_SKIP_BUILD=1 keeps it
        # from running them.
        for name in ("cargo", "cc"):
            tool = self.bin / name
            tool.write_text("#!/bin/sh\necho 'unexpected {} call' >&2\nexit 99\n".format(name))
            tool.chmod(0o755)
        self.guest = self.directory / "hello_race"
        self.guest.write_text("#!/bin/sh\nexit 99\n")
        self.guest.chmod(0o755)
        self.stub_log = self.directory / "hermit-calls.jsonl"
        self.artifacts = self.directory / "artifacts"
        self.schedule = self.artifacts / "hello-race-schedule.json"

    def _run(self, replay_mode, **extra_env):
        env = dict(os.environ)
        for name in ("HERMIT_EPOCH", "HERMIT_LOG_FILE", "HERMIT_PRNG", "HERMIT_SCHED_SEED"):
            env.pop(name, None)
        env.update(
            PATH="{}:{}".format(self.bin, os.environ.get("PATH", "/usr/bin:/bin")),
            DEMO_SKIP_BUILD="1",
            HELLO_RACE=str(self.guest),
            HEAP_PTRS=str(self.guest),
            DEMO_TMP=str(self.directory / "demo-tmp"),
            DEMO_ARTIFACTS=str(self.artifacts),
            STUB_LOG=str(self.stub_log),
            STUB_REPLAY=replay_mode,
        )
        env.update(extra_env)
        return subprocess.run(
            ["bash", str(RUN)],
            env=env,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            universal_newlines=True,
            timeout=120,
        )

    def _calls(self):
        return [json.loads(line) for line in self.stub_log.read_text().splitlines()]

    def _call_with(self, option):
        matches = [call for call in self._calls() if any(arg.startswith(option) for arg in call["argv"])]
        self.assertEqual(len(matches), 1, "expected one call with {}".format(option))
        return matches[0]

    def _assert_success(self, result):
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(result.stdout.rstrip("\n").splitlines()[-1], SUCCESS_LINE)

    # Positive controls: a replay that follows the file passes.

    def test_a_replay_that_follows_the_file_passes(self):
        self._assert_success(self._run("faithful"))

    def test_a_replay_that_follows_the_file_passes_with_hermit_epoch_and_log_file_set(self):
        result = self._run(
            "faithful",
            HERMIT_EPOCH="2031-07-15T12:34:56+00:00",
            HERMIT_LOG_FILE=str(self.directory / "hermit.log"),
        )
        self._assert_success(result)

    # Regression tests: each fails on a demo whose replay ran without a seed
    # (so under seed 0, which fails by itself) and checked no clock start.

    def test_the_replay_runs_the_file_under_seed_1_with_epoch_and_log_file_unset(self):
        self._assert_success(
            self._run(
                "faithful",
                HERMIT_EPOCH="2031-07-15T12:34:56+00:00",
                HERMIT_LOG_FILE=str(self.directory / "hermit.log"),
            )
        )
        recording = self._call_with("--record-preemptions-to=")
        replay = self._call_with("--replay-preemptions-from=")
        self.assertIn("--seed=0", recording["argv"])
        self.assertIn("--record-preemptions-to={}".format(self.schedule), recording["argv"])
        self.assertEqual([arg for arg in replay["argv"] if arg.startswith("--seed")], ["--seed=1"])
        self.assertIn("--replay-preemptions-from={}".format(self.schedule), replay["argv"])
        for call in (recording, replay):
            self.assertIsNone(call["HERMIT_EPOCH"])
            self.assertIsNone(call["HERMIT_LOG_FILE"])
        # Apart from the seed and the file option, the two runs use the same
        # flags as each other.
        def without(argv, *prefixes):
            return [arg for arg in argv if not arg.startswith(prefixes)]

        self.assertEqual(
            without(recording["argv"], "--seed=", "--record-preemptions-to="),
            without(replay["argv"], "--seed=", "--replay-preemptions-from="),
        )

    def test_a_replay_that_ignores_the_file_fails(self):
        result = self._run("ignores-file")
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn(
            "replaying the failing schedule under seed 1: exit status 0, expected 1", result.stderr
        )
        self.assertNotIn(SUCCESS_LINE, result.stdout)

    def test_a_replay_that_does_not_start_at_the_stored_clock_fails(self):
        result = self._run("host-clock")
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn(
            "the replay did not start its virtual clock at the instant stored in {}".format(self.schedule),
            result.stderr,
        )
        recording = json.loads(self.schedule.read_text())
        self.assertIn(
            "recording: {} host-now; replay: 2026-10-03T12:00:".format(recording["epoch"]), result.stderr
        )
        self.assertNotIn(SUCCESS_LINE, result.stdout)


if __name__ == "__main__":
    unittest.main()
