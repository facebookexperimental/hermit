#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

"""Tests a node's Buck measurement sidecar: ci/buck-e2e/buck-measure-summarize.py from planted inputs, and
buck-measure.sh, measure-release-build and the harvest's wiring with a stand-in buck2."""

from __future__ import annotations

import gzip
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import unittest
import zlib
from decimal import Decimal
from pathlib import Path

HERE = Path(__file__).resolve().parent
SUMMARIZE = HERE / "buck-e2e" / "buck-measure-summarize.py"
MEASURE = HERE / "buck-e2e" / "buck-measure.sh"
RELEASE = HERE / "buck-e2e" / "measure-release-build"
HARVEST = HERE / "harvest-reverie-buck.sh"
RESULT = json.dumps({"Result": {"build_response": {}}})


def action(kind) -> str:
    return json.dumps({"Event": {"data": {"SpanEnd": {"data": {"ActionExecution": {"execution_kind": kind}}}}}})


def own_cgroup() -> str:
    return next(line[3:] for line in Path("/proc/self/cgroup").read_text().splitlines() if line.startswith("0::"))


def clean_environment(**extra: str) -> dict[str, str]:
    environment = {key: value for key, value in os.environ.items()
                   if key not in ("E2E_RESULT_ROOT", "DAGRUN_OUTER_RUN", "DAGRUN_STEP_STARTED_MONOTONIC_NS")}
    environment.update(extra)
    return environment


class SummarizeTest(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory(prefix="test-buck-measure-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.buck2 = self.root / "buck2"
        self.buck2.write_bytes(b"#!/bin/sh\n")
        self.buck2.chmod(0o755)
        (self.root / "start.stat").write_text("usage_usec 1000000\nuser_usec 600000\nsystem_usec 400000\nnr_periods 0\n")
        (self.root / "end.stat").write_text("usage_usec 5500000\nuser_usec 3600000\nsystem_usec 1900000\nnr_periods 0\n")
        (self.root / "peak").write_text("4294967296\n")
        self.output = self.root / "buck-out" / "hermit"
        self.output.parent.mkdir()
        self.output.write_bytes(b"binary bytes")
        (self.root / "show.txt").write_text(f"root//hermit-cli:hermit {self.output}\n")
        (self.root / "boot.tsv").write_text("regenerate-rust-deps\t0\t2.50\t0.75\nstage-pinned-sysroot\t0\t0.10\t0.05\n")
        self.events = self.write_events("events.json-lines.gz",
                                        [action(1), action(1), action(3), action(10), action(12), action(13), "{}", action(42), RESULT])

    def write_events(self, name: str, lines: list[str]) -> Path:
        path = self.root / name
        with gzip.open(path, "wt", encoding="utf-8") as handle:
            handle.write("\n".join(lines) + "\n")
        return path

    def run_summarize(self, *extra: str, daemon: tuple[str, ...] | None = None, events: list[Path] | None = None):
        out = self.root / "measure" / "build.reverie_buck.json"
        argv = [sys.executable, str(SUMMARIZE), "--node", "build.reverie_buck", "--out", str(out), "--root", str(self.root),
                "--attempt", "123456789", "--buck2", str(self.buck2), "--buck2-version", "buck2 2026-08-01",
                "--buck2-chosen-by", "HERMIT_VALIDATE_BUCK2", "--show-output", str(self.root / "show.txt"),
                "--bootstraps", str(self.root / "boot.tsv"), "--buck-out", str(self.root / "buck-out")]
        for log in (self.events,) if events is None else events:
            argv += ["--event-log", str(log)]
        if daemon is None:
            daemon = ("--daemon-scope", "/sys/fs/cgroup/user.slice/buck2-daemon.scope", "--scope-cpu-start",
                      str(self.root / "start.stat"), "--scope-cpu-end", str(self.root / "end.stat"),
                      "--scope-memory-peak", str(self.root / "peak"))
        process = subprocess.run(argv + list(daemon) + list(extra), capture_output=True, text=True, timeout=60)
        self.assertEqual(process.returncode, 0, process.stderr)
        return json.loads(out.read_text())

    def facts(self, cgroup: str, node: str, **extra) -> tuple[str, ...]:
        path = self.root / "daemon.json"
        path.write_text(json.dumps({"pid": 4242, "cgroup": cgroup, "node_cgroup": node, **extra}))
        return ("--daemon-facts", str(path))

    def test_a_complete_node_records_every_fact(self):
        record = self.run_summarize()
        self.assertEqual(record["schema"], "hermit-buck-measure/v2")
        self.assertEqual((record["node"], record["attempt"]), ("build.reverie_buck", "123456789"))
        self.assertEqual(record["buck2"]["sha256"], hashlib.sha256(b"#!/bin/sh\n").hexdigest())
        self.assertEqual(record["buck2"]["file"], "executable")
        self.assertEqual(record["buck2"]["chosen_by"], "HERMIT_VALIDATE_BUCK2")
        # 5.5 - 1.0 = 4.5 CPU-s over the node: 3.0 user, 1.5 system.
        self.assertEqual(record["daemon"], {"placement": "scope", "scope": "/sys/fs/cgroup/user.slice/buck2-daemon.scope",
                                            "cpu_usec": 4500000, "user_usec": 3000000, "system_usec": 1500000,
                                            "memory_peak_bytes": 4294967296})
        # Two local; action_cache, local_action_cache and remote_served_from_cache are cache hits;
        # remote_deduplicated is not; an unknown kind is kept by number; the record with no
        # ActionExecution and the final Result count nothing.
        actions = record["actions"]
        self.assertEqual((actions["measured"], actions["event_logs"], actions["total"], actions["cached"]), (True, 1, 7, 3))
        self.assertEqual(actions["by_kind"], {"action_cache": 1, "local": 2, "local_action_cache": 1,
                                              "remote_deduplicated": 1, "remote_served_from_cache": 1, "unknown_42": 1})
        with gzip.open(self.events, "rb") as handle:
            self.assertEqual(actions["decoded_bytes"], len(handle.read()))
        self.assertEqual(record["outputs"], {"measured": True, "files": [
            {"target": "root//hermit-cli:hermit", "path": str(self.output), "sha256": hashlib.sha256(b"binary bytes").hexdigest()}]})
        self.assertEqual([(row["name"], row["measured"]) for row in record["bootstraps"]],
                         [("regenerate-rust-deps", True), ("stage-pinned-sysroot", True)])
        self.assertEqual(record["bootstraps"][0]["user_s"], 2.5)
        self.assertEqual(record["disk"]["buck_out_apparent_bytes"], len(b"binary bytes"))
        self.assertEqual(record["cache"], {"configured": False})

    def test_a_dotslash_launcher_is_labelled_as_one(self):
        self.buck2.write_bytes(b'#!/usr/bin/env dotslash\n{"name": "buck2"}\n')
        self.assertEqual(self.run_summarize()["buck2"]["file"], "dotslash_launcher")

    def test_a_daemon_in_the_node_cgroup_is_already_the_step_cpu(self):
        record = self.run_summarize(daemon=self.facts("/user.slice/dagrun/step.x", "/user.slice/dagrun/step.x"))
        self.assertEqual(record["daemon"], {"placement": "step_cgroup", "cgroup": "/user.slice/dagrun/step.x",
                                            "node_cgroup": "/user.slice/dagrun/step.x", "pid": 4242})
        record = self.run_summarize(daemon=self.facts("/user.slice/dagrun/step.x/leaf", "/user.slice/dagrun/step.x"))
        self.assertEqual(record["daemon"]["placement"], "step_cgroup")

    def test_a_daemon_outside_the_node_cgroup_is_unmeasured(self):
        for daemon, node in (("/user.slice/other.scope", "/user.slice/dagrun/step.x"),
                             ("/user.slice/dagrun/step.xy", "/user.slice/dagrun/step.x")):
            record = self.run_summarize(daemon=self.facts(daemon, node))
            self.assertEqual(record["daemon"]["placement"], "unmeasured")
            self.assertIn(f"ran in cgroup {daemon}, outside this node's cgroup", record["daemon"]["reason"])
        record = self.run_summarize(daemon=self.facts("/a/step", "/a/step", forkserver_cgroup="/elsewhere"))
        self.assertIn("forkserver ran in cgroup /elsewhere", record["daemon"]["reason"])

    def test_an_unmeasured_daemon_keeps_its_reason(self):
        record = self.run_summarize(daemon=("--daemon-unmeasured-reason", "`buck2 status` exited 124 (124: it ran past 5 s)"))
        self.assertEqual(record["daemon"], {"placement": "unmeasured", "reason": "`buck2 status` exited 124 (124: it ran past 5 s)"})
        record = self.run_summarize(daemon=())
        self.assertEqual(record["daemon"], {"placement": "unmeasured", "reason": "where the daemon ran was not recorded"})

    def test_two_event_logs_are_counted_together_and_a_plain_jsonl_log_is_read(self):
        second = self.root / "second.event-log.jsonl"
        second.write_text("\n".join([json.dumps({"command_line_args": []}), action(2), action(11), RESULT]) + "\n")
        record = self.run_summarize("--expected-event-logs", "2", events=[self.events, second])
        self.assertEqual(record["actions"]["event_logs"], 2)
        self.assertEqual((record["actions"]["by_kind"]["remote"], record["actions"]["by_kind"]["remote_worker"]), (1, 1))
        self.assertEqual(record["actions"]["total"], 9)

    def assert_actions_unmeasured(self, record: dict, needle: str) -> None:
        self.assertEqual(record["actions"]["measured"], False)
        self.assertIn(needle, record["actions"]["reason"])
        self.assertNotIn("total", record["actions"], "an unmeasured count writes no partial number")

    def test_fewer_logs_than_invocations_leave_the_actions_unmeasured(self):
        self.assert_actions_unmeasured(self.run_summarize("--expected-event-logs", "2"),
                                       "ran 2 buck2 invocation(s) with event logs, and 1 were decoded")
        self.assert_actions_unmeasured(self.run_summarize(events=[]), "no buck2 event log was kept")

    def test_a_cut_log_leaves_the_actions_unmeasured(self):
        for lines in ([action(1), action(1)], [action(1), RESULT[:12]]):
            self.events = self.write_events("events.json-lines.gz", lines)
            self.assert_actions_unmeasured(self.run_summarize(), "does not end with buck2's Result record")

    def test_a_malformed_log_leaves_the_actions_unmeasured(self):
        self.events = self.write_events("events.json-lines.gz", [action(1), "{not json", RESULT])
        self.assert_actions_unmeasured(self.run_summarize(), "invalid JSON before its last line")
        self.events = self.write_events("events.json-lines.gz", [action("local"), RESULT])
        self.assert_actions_unmeasured(self.run_summarize(), "without an integer execution_kind")
        self.events.write_text(action(1))
        self.assert_actions_unmeasured(self.run_summarize(), "not a readable buck2 event log")

    def test_a_corrupt_deflate_block_leaves_the_actions_unmeasured_not_the_sidecar_lost(self):
        # A review finding on the first landed version: zlib.error must be typed as an unreadable
        # log, so the daemon, outputs and bootstrap facts are still written.
        self.events = self.write_events("events.json-lines.gz", [action(1)] * 400 + [RESULT])
        data = bytearray(self.events.read_bytes())
        data[40:80] = b"\xff" * 40
        self.events.write_bytes(bytes(data))
        with self.assertRaises(zlib.error):
            gzip.decompress(bytes(data))
        record = self.run_summarize()
        self.assert_actions_unmeasured(record, "not a readable buck2 event log")
        self.assertEqual(record["daemon"]["placement"], "scope")
        self.assertEqual(record["outputs"]["measured"], True)

    def test_bytes_that_are_not_utf8_leave_the_actions_unmeasured_not_the_sidecar_lost(self):
        # The same class as the corrupt deflate block: json.loads decodes bytes first, and invalid
        # UTF-8 raises UnicodeDecodeError, a ValueError but not a JSONDecodeError. A bad byte in
        # a middle line, and a copy cut inside a multibyte character (keep_invocations' time
        # budget can end the copy anywhere), each leave only the actions unmeasured.
        result = json.loads(RESULT)
        cut = json.dumps({**result, "note": "café"}, ensure_ascii=False).encode()
        for lines, needle in (([action(1).encode(), b'{"note": "\xff"}', RESULT.encode()], "invalid JSON before its last line"),
                              ([action(1).encode(), cut[:cut.index("é".encode()) + 1]], "does not end with buck2's Result record")):
            with self.subTest(needle):
                self.events = self.root / "events.event-log.jsonl"
                self.events.write_bytes(b"\n".join(lines) + b"\n")
                with self.assertRaises(UnicodeDecodeError):
                    json.loads(lines[-1] if needle.startswith("does not") else lines[1])
                record = self.run_summarize()
                self.assert_actions_unmeasured(record, needle)
                self.assertEqual(record["daemon"]["placement"], "scope")
                self.assertEqual(record["outputs"]["measured"], True)

    def test_bad_scope_readings_leave_the_daemon_unmeasured(self):
        (self.root / "end.stat").write_text("usage_usec 10\nuser_usec 5\nsystem_usec 5\n")
        self.assertIn("went backwards", self.run_summarize()["daemon"]["reason"])
        (self.root / "end.stat").write_text("user_usec 5\nsystem_usec 5\n")
        self.assertIn("lacks usage_usec", self.run_summarize()["daemon"]["reason"])
        (self.root / "end.stat").write_text("usage_usec\n")
        self.assertIn("not a `key value` cpu.stat line", self.run_summarize()["daemon"]["reason"])

    def test_a_missing_output_and_bad_bootstrap_rows_are_unmeasured_by_name(self):
        self.output.unlink()
        (self.root / "boot.tsv").write_text("regenerate-rust-deps\t0\t2.5\nstage-pinned-sysroot\t1\tunmeasured\t/usr/bin/time is not installed\n")
        record = self.run_summarize()
        self.assertEqual(record["outputs"]["measured"], False)
        self.assertIn("is not a file", record["outputs"]["reason"])
        first, second = record["bootstraps"]
        self.assertEqual((first["measured"], second["measured"], second["rc"]), (False, False, 1))
        self.assertIn("not a `name rc user_s sys_s` row", first["reason"])
        self.assertEqual(second["reason"], "/usr/bin/time is not installed")

    def test_the_daemon_subcommand_keeps_pids_and_cgroups_and_never_the_token(self):
        status = self.root / "status.json"
        status.write_text(json.dumps({"process_info": {"pid": os.getpid(), "endpoint": "tcp:1", "auth_token": "SECRET-TOKEN"},
                                      "forkserver_pid": os.getpid(), "allprocs_cgroup_path": None}))
        out = self.root / "facts.json"
        process = subprocess.run([sys.executable, str(SUMMARIZE), "daemon", "--status", str(status), "--out", str(out)],
                                 capture_output=True, text=True, timeout=60)
        self.assertEqual(process.returncode, 0, process.stderr)
        self.assertNotIn("SECRET-TOKEN", out.read_text() + process.stdout + process.stderr)
        facts = json.loads(out.read_text())
        self.assertEqual((facts["pid"], facts["cgroup"], facts["node_cgroup"]), (os.getpid(), own_cgroup(), own_cgroup()))
        self.assertEqual(facts["forkserver_cgroup"], own_cgroup())
        for text, needle in (("", "no buck2 daemon was running"), ("not json", "printed no JSON"),
                             (json.dumps({"process_info": {}}), "named no daemon pid")):
            status.write_text(text)
            subprocess.run([sys.executable, str(SUMMARIZE), "daemon", "--status", str(status), "--out", str(out)],
                           check=True, timeout=60)
            self.assertIn(needle, json.loads(out.read_text())["unmeasured"])


STUB_BUCK2 = r"""#!/bin/bash
# A buck2 stand-in. `status` names $STUB_DAEMON_PID as the daemon, after $STUB_STATUS_SLEEP seconds
# when set; `build` requires the client metadata, writes a two-action event log ending with buck2's
# Result record, and one output.
case $1 in
  --version) echo "buck2 stub 1.0" ;;
  status)
    sleep "${STUB_STATUS_SLEEP:-0}"
    printf '{"process_info": {"pid": %s, "auth_token": "SECRET-TOKEN"}}\n' "$STUB_DAEMON_PID" ;;
  build)
    shift
    log='' metadata=''
    while (($#)); do
      case $1 in
        --event-log) log=$2; shift 2 ;;
        --client-metadata) metadata=$2; shift 2 ;;
        --show-output) shift ;;
        *) target=$1; shift ;;
      esac
    done
    [[ $metadata == id=hermit-buck-measure ]] || { echo "stub: no --client-metadata id=hermit-buck-measure" >&2; exit 3; }
    if [[ -n $log ]]; then
      action='{"Event": {"data": {"SpanEnd": {"data": {"ActionExecution": {"execution_kind": KIND}}}}}}'
      printf '%s\n%s\n%s\n' "${action/KIND/1}" "${action/KIND/10}" '{"Result": {}}' | gzip >"$log"
    fi
    mkdir -p buck-out/v2/gen
    printf 'built\n' >buck-out/v2/gen/out
    echo "$target buck-out/v2/gen/out"
    ;;
  *) exit 4 ;;
esac
"""


class MeasureShellTest(unittest.TestCase):
    """ci/buck-e2e/buck-measure.sh end to end, with a stand-in buck2."""

    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory(prefix="test-buck-measure-sh-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.stub = self.root / "buck2"
        self.stub.write_text(STUB_BUCK2)
        self.stub.chmod(0o755)

    def run_script(self, body: str, **environment: str) -> subprocess.CompletedProcess:
        # A stand-in daemon in this node's cgroup, alive while the measurement reads it.
        script = (f'set -euo pipefail; sleep 300 & export STUB_DAEMON_PID=$!; trap "kill $STUB_DAEMON_PID" EXIT; '
                  f'source {MEASURE}; {body}')
        return subprocess.run(["bash", "-c", script], cwd=self.root, capture_output=True, text=True, timeout=120,
                              env=clean_environment(**environment))

    def test_a_node_records_its_build_and_its_daemon_in_the_step_cgroup(self):
        process = self.run_script(f'buck_measure_start build.example {self.stub} HERMIT_VALIDATE_BUCK2; '
                                  'buck_measure_bootstrap prepare true; '
                                  'buck_measure_build show.txt root//example:out; cat show.txt; '
                                  'buck_measure_daemon; buck_measure_write; echo after')
        self.assertEqual(process.returncode, 0, process.stderr)
        self.assertIn("root//example:out buck-out/v2/gen/out\nafter\n", process.stdout)
        record = json.loads((self.root / "target/validate/buck-measure/build.example.json").read_text())
        self.assertEqual(record["daemon"]["placement"], "step_cgroup", record["daemon"])
        self.assertEqual(record["daemon"]["cgroup"], own_cgroup())
        self.assertEqual(record["actions"]["by_kind"], {"local": 1, "local_action_cache": 1})
        self.assertEqual(record["outputs"]["files"][0]["sha256"], hashlib.sha256(b"built\n").hexdigest())
        self.assertEqual([(row["name"], row["rc"]) for row in record["bootstraps"]], [("prepare", 0)])
        self.assertEqual(record["buck2"]["version"], "buck2 stub 1.0")
        self.assertEqual(record["attempt"], None)
        self.assertNotIn("SECRET-TOKEN", json.dumps(record) + process.stderr)

    def test_dagrun_names_the_sidecar_and_an_earlier_attempts_sidecar_is_removed(self):
        results = self.root / "results"
        stale = results / "buck-measure" / "e2e.buck_s0.json"
        stale.parent.mkdir(parents=True)
        stale.write_text('{"stale": true}\n')
        process = self.run_script(f'buck_measure_start e2e.buck_all {self.stub} HERMIT_VALIDATE_BUCK2; '
                                  '[[ ! -e $E2E_RESULT_ROOT/buck-measure/e2e.buck_s0.json ]]; echo removed',
                                  E2E_RESULT_ROOT=str(results), DAGRUN_OUTER_RUN="e2e.buck_s0",
                                  DAGRUN_STEP_STARTED_MONOTONIC_NS="987654321")
        self.assertEqual(process.returncode, 0, process.stderr)
        self.assertIn("removed", process.stdout)
        process = self.run_script(f'buck_measure_start e2e.buck_all {self.stub} HERMIT_VALIDATE_BUCK2; buck_measure_write',
                                  E2E_RESULT_ROOT=str(results), DAGRUN_OUTER_RUN="e2e.buck_s0",
                                  DAGRUN_STEP_STARTED_MONOTONIC_NS="987654321")
        record = json.loads(stale.read_text())
        self.assertEqual((record["node"], record["attempt"]), ("e2e.buck_s0", "987654321"))
        self.assertEqual(record["daemon"]["reason"], "where the daemon ran was not recorded")
        self.assertIn("no buck2 event log was kept", record["actions"]["reason"])

    def test_a_measurement_that_cannot_start_costs_the_sidecar_never_the_node(self):
        (self.root / "not-a-directory").write_text("")
        process = self.run_script(f'buck_measure_start build.example {self.stub} HERMIT_VALIDATE_BUCK2; '
                                  'buck_measure_bootstrap prepare true; buck_measure_build show.txt root//example:out; '
                                  'buck_measure_daemon; buck_measure_write; echo after',
                                  E2E_RESULT_ROOT=str(self.root / "not-a-directory"))
        self.assertEqual(process.returncode, 0, process.stderr)
        self.assertIn("root//example:out", (self.root / "show.txt").read_text())
        self.assertIn("after", process.stdout)
        self.assertIn("writes no sidecar", process.stderr)

    def daemon_read(self, limit: str, status_sleep: str) -> tuple[subprocess.CompletedProcess, list[tuple[Decimal, str]], Decimal]:
        """Run buck_measure_daemon LIMIT against a `buck2 status` that takes STATUS_SLEEP seconds,
        with every `timeout` it runs recorded (its duration and command); return the process, those
        records and the read's own wall time."""
        bin_dir = self.root / "bin"
        bin_dir.mkdir(exist_ok=True)
        log = self.root / "timeouts.tsv"
        recorder = bin_dir / "timeout"
        recorder.write_text(f'#!/bin/bash\nprintf \'%s\\t%s\\n\' "$4" "${{5##*/}}" >>"{log}"\nexec {shutil.which("timeout")} "$@"\n')
        recorder.chmod(0o755)
        process = self.run_script(f'buck_measure_start build.example {self.stub} HERMIT_VALIDATE_BUCK2; : >"{log}"; '
                                  f'start=$EPOCHREALTIME; buck_measure_daemon {limit}; end=$EPOCHREALTIME; '
                                  'echo "read $start $end"; echo "reason: $bm_daemon_reason"',
                                  PATH=f"{bin_dir}:{os.environ['PATH']}", STUB_STATUS_SLEEP=status_sleep)
        records = [(Decimal(duration), command) for duration, command in
                   (line.split("\t") for line in log.read_text().splitlines())]
        start, end = re.search(r"^read (\S+) (\S+)$", process.stdout, re.M).groups()
        return process, records, Decimal(end) - Decimal(start)

    def test_the_daemon_read_spends_one_budget_for_status_and_the_summarizer(self):
        # A `buck2 status` that takes 2 of the 6 s leaves the summarizer only the rest (at most
        # 4 s): never the whole 6 s again. The read stays within its 6 s plus one 1 s kill grace.
        # The margins are wide so a loaded host cannot push status past its budget.
        process, records, elapsed = self.daemon_read("6", "2")
        self.assertEqual(process.returncode, 0, process.stderr)
        self.assertEqual([command for _duration, command in records], ["buck2", "python3"], records)
        self.assertEqual(records[0][0], Decimal(6), records)
        self.assertGreater(records[1][0], 0, records)
        self.assertLessEqual(records[1][0], Decimal(4), f"the summarizer got more than what status left: {records}")
        self.assertLessEqual(elapsed, 6 + 1, f"the read took {elapsed} s of a 6 s budget")

    def test_a_status_that_uses_the_whole_budget_starts_no_summarizer(self):
        # A `buck2 status` still running at the bound is stopped there; nothing starts after it,
        # and the daemon is unmeasured with the reason.
        process, records, elapsed = self.daemon_read("1", "5")
        self.assertEqual(process.returncode, 0, process.stderr)
        self.assertEqual(records, [(Decimal(1), "buck2")], records)
        self.assertIn("reason: `buck2 status` exited 124 (124: it ran past 1 s)", process.stdout)
        self.assertLessEqual(elapsed, 1 + 1, f"the read took {elapsed} s of a 1 s budget")

    def test_a_discarded_measurement_leaves_no_work_directory_and_no_sidecar(self):
        # validate-node's exit with no time left for the sidecar discards the measurement: its work
        # directory (with any decoded event logs) is removed, and no sidecar is written.
        scratch = self.root / "scratch"
        scratch.mkdir()
        process = self.run_script(f'buck_measure_start build.example {self.stub} HERMIT_VALIDATE_BUCK2; '
                                  'buck_measure_build show.txt root//example:out; '
                                  'printf "decoded\\n" >"$bm_work/re.event-log.jsonl"; '
                                  'ls -d "$TMPDIR"/buck-measure.* >/dev/null; echo "had a work dir"; '
                                  'buck_measure_discard; [[ -z $bm_work ]]; buck_measure_write; echo after',
                                  TMPDIR=str(scratch))
        self.assertEqual(process.returncode, 0, process.stderr)
        self.assertIn("had a work dir\nafter\n", process.stdout)
        self.assertEqual(sorted(path.name for path in scratch.iterdir()), [], "the work directory was left behind")
        self.assertFalse((self.root / "target/validate/buck-measure/build.example.json").exists(),
                         "a discarded measurement wrote a sidecar")

    def test_a_failing_bootstrap_returns_its_status_and_is_recorded(self):
        process = self.run_script(f'buck_measure_start build.example {self.stub} HERMIT_VALIDATE_BUCK2; '
                                  'rc=0; buck_measure_bootstrap fails false || rc=$?; echo "rc=$rc"; '
                                  'cat "$bm_work/bootstraps.tsv"')
        self.assertEqual(process.returncode, 0, process.stderr)
        self.assertIn("rc=1\n", process.stdout)
        self.assertRegex(process.stdout, r"fails\t1\t(unmeasured\t.+|[0-9.]+\t[0-9.]+)\n")


class ReleaseMeasureTest(unittest.TestCase):
    """ci/buck-e2e/measure-release-build: the release build's own evidence, its status kept."""

    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory(prefix="test-buck-measure-release-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        (self.root / "stub-buck2").write_text(STUB_BUCK2)
        (self.root / "stub-buck2").chmod(0o755)
        (self.root / "ignored/buck2-phase2/builds/build-old-1").mkdir(parents=True)
        # A stand-in daemon in this test's cgroup, which outlives the build as a real one does.
        daemon = subprocess.Popen(["sleep", "300"])
        self.addCleanup(daemon.wait)
        self.addCleanup(daemon.kill)
        self.daemon_pid = str(daemon.pid)

    def run_release(self, command: str) -> subprocess.CompletedProcess:
        return subprocess.run([str(RELEASE), "bash", "-c", command], cwd=self.root, capture_output=True, text=True,
                              timeout=120, env=clean_environment(DAGRUN_OUTER_RUN="build.buck_release_artifact",
                                                                 STUB_DAEMON_PID=self.daemon_pid))

    def test_the_release_builds_evidence_is_measured_and_its_status_kept(self):
        process = self.run_release(
            'set -e; evidence=ignored/buck2-phase2/builds/build-abc-2; mkdir -p "$evidence/buck2-tool-snapshot"; '
            'cp stub-buck2 "$evidence/buck2-tool-snapshot/buck2"; '
            '"$evidence/buck2-tool-snapshot/buck2" build --event-log "$evidence/buck-build.json-lines.gz" '
            '--client-metadata id=hermit-buck-measure --show-output root//hermit-cli:hermit-release >"$evidence/buck-build.stdout"; '
            'exit 7')
        self.assertEqual(process.returncode, 7, process.stderr)
        record = json.loads((self.root / "target/validate/buck-measure/build.buck_release_artifact.json").read_text())
        self.assertEqual(record["actions"]["by_kind"], {"local": 1, "local_action_cache": 1})
        self.assertEqual(record["outputs"]["files"][0]["target"], "root//hermit-cli:hermit-release")
        self.assertTrue(record["buck2"]["path"].endswith("build-abc-2/buck2-tool-snapshot/buck2"))
        self.assertEqual(record["buck2"]["version"], "buck2 stub 1.0")
        self.assertEqual(record["daemon"]["placement"], "step_cgroup", record["daemon"])

    def test_a_run_without_one_new_evidence_directory_is_unmeasured(self):
        process = self.run_release("exit 0")
        self.assertEqual(process.returncode, 0, process.stderr)
        record = json.loads((self.root / "target/validate/buck-measure/build.buck_release_artifact.json").read_text())
        self.assertIn("created 0 evidence directories", record["daemon"]["reason"])
        self.assertIn("ran 1 buck2 invocation(s) with event logs, and 0 were decoded", record["actions"]["reason"])


class HarvestWiringTest(unittest.TestCase):
    """The harvest measures its build without starting or stopping a daemon, and reads its outputs from its own file."""

    def test_the_harvest_measures_in_order_and_owns_its_show_output(self):
        text = HARVEST.read_text()
        calls = [match.group(0) for match in re.finditer(
            r"buck_measure_(start build\.reverie_buck|bootstrap regenerate-rust-deps|bootstrap stage-pinned-sysroot|"
            r"build \"\$SHOW_OUTPUT\"|daemon \d+|write \d+)", text)]
        self.assertEqual(calls, ["buck_measure_start build.reverie_buck", "buck_measure_bootstrap regenerate-rust-deps",
                                 "buck_measure_bootstrap stage-pinned-sysroot", 'buck_measure_build "$SHOW_OUTPUT"',
                                 "buck_measure_daemon 10", "buck_measure_write 60"])
        self.assertIn('cat -- "$SHOW_OUTPUT" >>"$BUILD_LOG"', text)
        self.assertNotRegex(text, r"\bkill\b", "the harvest leaves the daemon's lifetime to dagrun")
        self.assertNotIn("bm_build_out", text)
        # The measurement fits inside build.reverie_buck's own 600 s wall and 600 CPU-s: no
        # buck-out walk (bm_buck_out is never set), and the two calls the harvest bounds itself
        # are bounded by 10 + 60 s. With buck_measure_start's 30 s
        # version read and the 1 s grace that `timeout -k 1` gives each bounded command, the
        # worst case is about 103 s of the 600.
        self.assertNotIn("bm_buck_out", text, "the harvest must not walk buck-out")
        bounds = [int(seconds) for seconds in re.findall(r"^buck_measure_(?:daemon|write) (\d+)$", text, re.M)]
        self.assertLessEqual(sum(bounds), 70, bounds)


if __name__ == "__main__":
    unittest.main()
