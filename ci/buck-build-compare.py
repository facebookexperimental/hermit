#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

"""Compare two validation runs: Buck build cost, and every counted test's typed outcome.

Usage: ci/buck-build-compare.py RUN_A RUN_B

Each RUN is a run's durable artifact directory, as ci-hub keeps it
(ignored/validate/artifacts/<run>/). It reads, from each run's own records:
- constructed-plan.json: the run's DAG. Its counted steps are the steps that set
  HERMIT_NEXTEST_RESULTS_SCHEMA (ci/run-nextest-counted.sh); its Buck steps are the steps that
  hold the buck_daemon unit, the only ones that may run buck2;
- history-row.json: the steps that ran (gates), and each one's attempts (retries + 1); and
  build_mode and release_builder, which say whether the two Buck steps whose command switches on
  a mode (build.reverie_buck, build.buck_release_artifact) ran buck2 or the no-op cargo branch;
- dagrun-profiles/*.csv: each step's cgroup user and system CPU seconds;
- e2e/nextest-events/<step>.<attempt>.started and .results.json: each counted attempt's start
  marker and its typed per-test results, dagrun's structured test results as
  ci/nextest-test-results.rs wrote them (schema 5: runtime skips typed with their reasons,
  retries folded into each test's attempts);
- e2e/buck-measure/<step>.json: each Buck step's sidecar (ci/buck-e2e/buck-measure-summarize.py).

It prints:
- two cost tables, build steps (build.*, setup.*) and Buck E2E steps (e2e.buck_*), each step
  with its CPU-s: its cgroup's, plus its buck2 daemon's systemd scope where the daemon ran
  outside that cgroup (a daemon inside it is already counted), and for a Buck step its bootstrap
  CPU, actions by kind and cache hits. Each table has a total per run, and B/A. Cargo's E2E
  cells (e2e.manifest_*) are in neither table;
- every counted test whose final typed outcome differs between the runs, in both directions:
  passed, failed (with the last attempt's cause), diagnostic failure, skipped (with its reason),
  or absent (no row: not started, ignored or filtered out). A test with the same outcome in both
  runs but a different number of attempts (a pass that needed retries, or the same failure or
  skip after more retries) is listed apart as a retry difference; it is not a difference, since
  retries are routine on loaded hosts.

Limits: actions are counted only for buck2 invocations that keep an event log (the harvest's
build, the release build and the Buck E2E cell runs; buck-stock's `stage --from-buck` builds keep
none). buck-hybrid's remote (RE) CPU is in no step's row, so its totals are local CPU only.

Exit status:
- 0: every counted test has the same outcome in both runs, and every Buck step was measured;
- 1: a test is absent from one run, or its outcome changed; a counted step is in one run's plan
  only; or a step's recorded selection changed;
- 2: refused, naming the file: the runs cannot be compared as recorded. That includes a run
  with no counted step; a counted step that did not run, whose last attempt left no start
  marker, or whose last attempt published no results (killed or refused: no result) or executed
  zero tests (which the census never passes, so it is no result either); a Buck
  step that ran without a sidecar, with an unmeasured daemon or actions, or more than once (its
  cost is not one build's); a sidecar from a step that holds no buck_daemon unit; and any
  malformed or unreadable input.
Kept records of steps that are not in a run's plan are listed and not read.
"""

from __future__ import annotations

import csv
import json
import re
import sys
from pathlib import Path

SIDECAR_SCHEMA = "hermit-buck-measure/v2"
RESULTS_SCHEMA = 5
RESULT_FIELDS = {"id", "result", "attempts", "attempt_results", "diagnostic_reason", "skip_reason"}
ATTEMPT_OUTCOMES = {
    "passed",
    "failed",
    "cpu_timeout",
    "wall_timeout",
    "cancelled",
    "infrastructure_error",
    "no_result",
    "skipped",
}
# The Buck steps whose DAG command runs buck2 only in Buck mode (`cargo) : ;;` otherwise), and the
# history-row field that records the mode the run chose.
MODE_SWITCHED = {"build.reverie_buck": "build_mode", "build.buck_release_artifact": "release_builder"}
KEPT_RECORD = re.compile(r"^(?P<step>.+)\.(?P<attempt>[^.]+)\.(?P<kind>started|results\.json|jsonl)$")


class Refusal(Exception):
    pass


def load_json(path: Path) -> object:
    try:
        return json.loads(path.read_bytes().decode("utf-8"))
    except FileNotFoundError as error:
        raise Refusal(f"{path} is missing") from error
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        raise Refusal(f"{path}: unreadable or not JSON: {error}") from error


def need(condition: bool, message: str) -> None:
    if not condition:
        raise Refusal(message)


def whole(value: object) -> bool:
    return isinstance(value, int) and not isinstance(value, bool)


class Run:
    def __init__(self, root: Path) -> None:
        self.root = root
        plan_path = root / "constructed-plan.json"
        plan = load_json(plan_path)
        need(isinstance(plan, dict), f"{plan_path}: not a JSON object")
        dag = plan.get("dag_json")
        if isinstance(dag, str):
            try:
                dag = json.loads(dag)
            except json.JSONDecodeError as error:
                raise Refusal(f"{plan_path}: dag_json is not JSON: {error}") from error
        need(isinstance(dag, dict) and isinstance(dag.get("steps"), list), f"{plan_path}: dag_json has no steps")
        self.steps: dict[str, dict] = {}
        for step in dag["steps"]:
            need(isinstance(step, dict) and isinstance(step.get("group"), str) and isinstance(step.get("job"), str),
                 f"{plan_path}: a step without a group and a job")
            self.steps[f"{step['group']}.{step['job']}"] = step
        row_path = root / "history-row.json"
        row = load_json(row_path)
        need(isinstance(row, dict) and isinstance(row.get("gates"), list), f"{row_path}: no gates")
        self.attempts: dict[str, int] = {}
        for gate in row["gates"]:
            need(isinstance(gate, dict) and isinstance(gate.get("name"), str) and whole(gate.get("retries"))
                 and gate["retries"] >= 0, f"{row_path}: a gate without a name and a retry count")
            self.attempts[gate["name"]] = gate["retries"] + 1
        self.modes = {field: row.get(field) for field in MODE_SWITCHED.values()}
        self.counted = sorted(tag for tag, step in self.steps.items()
                              if isinstance(step.get("env"), dict) and "HERMIT_NEXTEST_RESULTS_SCHEMA" in step["env"])
        self.holders = sorted(tag for tag, step in self.steps.items()
                              if ((step.get("hint") or {}).get("resources") or {}).get("buck_daemon"))
        self.records: dict[str, dict[str, set[str]]] = {}
        self.foreign: set[str] = set()
        events = root / "e2e" / "nextest-events"
        if events.is_dir():
            for file in sorted(events.iterdir()):
                match = KEPT_RECORD.match(file.name)
                if match is None or match["step"] not in self.steps:
                    self.foreign.add(match["step"] if match else file.name)
                    continue
                need(match["attempt"].isdigit(),
                     f"{file}: attempt {match['attempt']!r} is not DAGRUN_STEP_STARTED_MONOTONIC_NS, so attempts cannot be ordered")
                self.records.setdefault(match["step"], {}).setdefault(match["attempt"], set()).add(match["kind"])
        self.profiles: dict[str, list[dict]] = {}
        files = sorted((root / "dagrun-profiles").glob("*.csv"))
        need(bool(files), f"{root}: no dagrun-profiles/*.csv, so its steps' CPU is unknown")
        for file in files:
            try:
                with file.open(newline="", encoding="utf-8") as handle:
                    for row_number, profile in enumerate(csv.DictReader(handle), 2):
                        need(bool(profile.get("step")), f"{file}:{row_number}: a row without a step")
                        profile["_where"] = f"{file}:{row_number}"
                        self.profiles.setdefault(profile["step"], []).append(profile)
            except (OSError, UnicodeDecodeError, csv.Error) as error:
                raise Refusal(f"{file}: unreadable CSV: {error}") from error

    def ran(self, tag: str) -> bool:
        return self.attempts.get(tag, 0) > 0

    def cpu(self, tag: str) -> float:
        rows = self.profiles.get(tag)
        need(bool(rows), f"{self.root}: {tag} ran but has no dagrun profile row, so its CPU is unknown")
        total = 0.0
        for row in rows:
            try:
                total += float(row["user_s"]) + float(row["sys_s"])
            except (KeyError, TypeError, ValueError) as error:
                raise Refusal(f"{row['_where']}: {tag}'s user_s and sys_s are not numbers: {error}") from error
        return total


def label(row: object, where: str) -> tuple[str, str, int]:
    """(kind, detail, attempts) of one schema-5 result row; kind is passed|failed|diagnostic|skipped."""
    need(isinstance(row, dict) and set(row) == RESULT_FIELDS, f"{where}: a result row without exactly {sorted(RESULT_FIELDS)}")
    need(whole(row["attempts"]) and row["attempts"] >= 1, f"{where}: attempts is not a positive integer")
    attempts = row["attempt_results"]
    need(isinstance(attempts, list) and len(attempts) == row["attempts"], f"{where}: attempt_results does not hold one row per attempt")
    for attempt in attempts:
        need(isinstance(attempt, dict) and set(attempt) == {"attempt", "outcome", "detail"}
             and attempt["outcome"] in ATTEMPT_OUTCOMES, f"{where}: a malformed attempt row {attempt!r}")
    last = attempts[-1]["outcome"]
    result, skip, diagnostic = row["result"], row["skip_reason"], row["diagnostic_reason"]
    need(skip is None or (isinstance(skip, str) and skip), f"{where}: skip_reason is neither null nor a reason")
    need(diagnostic is None or (isinstance(diagnostic, str) and diagnostic), f"{where}: diagnostic_reason is neither null nor a reason")
    need((result == "skip") == (skip is not None), f"{where}: result {result!r} with skip_reason {skip!r}")
    need((result == "diagnostic_fail") == (diagnostic is not None), f"{where}: result {result!r} with diagnostic_reason {diagnostic!r}")
    if result == "pass":
        need(last == "passed", f"{where}: a pass whose last attempt is {last!r}")
        return "passed", "", row["attempts"]
    if result == "fail":
        need(last != "passed", f"{where}: a failure whose last attempt passed")
        return "failed", last, row["attempts"]
    if result == "diagnostic_fail":
        return "diagnostic", f"{last}; {diagnostic}", row["attempts"]
    if result == "skip":
        need(last == "skipped", f"{where}: a skip whose last attempt is {last!r}")
        return "skipped", skip, row["attempts"]
    raise Refusal(f"{where}: unknown result {result!r}")


def describe(outcome: tuple[str, str, int] | None) -> str:
    if outcome is None:
        return "absent"
    kind, detail, attempts = outcome
    if kind == "passed":
        return "passed" if attempts == 1 else f"passed on attempt {attempts}"
    text = {"failed": "failed", "diagnostic": "diagnostic failure", "skipped": "skipped"}[kind]
    return f"{text} ({detail})"


def counted_results(run: Run, tag: str) -> tuple[dict[str, tuple[str, str, int]], object]:
    """Each test's outcome in the last attempt of a counted step, and the attempt's selection."""
    expected = run.attempts.get(tag, 0)
    need(expected > 0, f"{run.root}: counted step {tag} did not run (no gate in history-row.json): no result")
    attempts = sorted(run.records.get(tag, {}), key=int)
    started = [attempt for attempt in attempts if "started" in run.records[tag][attempt]]
    need(len(started) == expected,
         f"{run.root}: counted step {tag} made {expected} attempt(s) (history-row.json) and {len(started)} left a "
         "start marker in e2e/nextest-events/: the last attempt's result is unknown")
    last = started[-1]
    need(attempts[-1] == last, f"{run.root}: {tag} kept records of attempt {attempts[-1]} with no start marker")
    path = run.root / "e2e" / "nextest-events" / f"{tag}.{last}.results.json"
    need("results.json" in run.records[tag][last],
         f"{path} is missing: counted step {tag}'s last attempt published no typed results (killed or refused): no result")
    document = load_json(path)
    need(isinstance(document, dict), f"{path}: not a JSON object")
    need(document.get("schema") == RESULTS_SCHEMA,
         f"{path}: schema {document.get('schema')!r}; only schema {RESULTS_SCHEMA} types a runtime skip")
    allowed = {"schema", "executed_tests", "filtered_tests", "results", "selection"}
    need(set(document) - {"selection"} == allowed - {"selection"} and set(document) <= allowed,
         f"{path}: fields {sorted(document)}, not {sorted(allowed)}")
    rows = document["results"]
    need(whole(document["executed_tests"]) and isinstance(rows, list) and len(rows) == document["executed_tests"],
         f"{path}: results does not hold one row per executed test")
    # The census never lets a counted node that executed nothing pass, so zero is a no-result.
    need(document["executed_tests"] > 0,
         f"{path}: counted step {tag}'s last attempt executed zero tests: no result")
    outcomes: dict[str, tuple[str, str, int]] = {}
    for index, row in enumerate(rows):
        where = f"{path}: results[{index}]"
        need(isinstance(row, dict) and isinstance(row.get("id"), str) and row["id"] and row["id"].strip() == row["id"],
             f"{where}: no trimmed test id")
        need(row["id"] not in outcomes, f"{where}: test {row['id']!r} appears twice")
        outcomes[row["id"]] = label(row, where)
    return outcomes, document.get("selection")


def sidecar(run: Run, tag: str) -> dict:
    path = run.root / "e2e" / "buck-measure" / f"{tag}.json"
    need(path.exists(), f"{path} is missing: {tag} holds the buck_daemon unit and ran, but left no measurement sidecar")
    record = load_json(path)
    need(isinstance(record, dict) and record.get("schema") == SIDECAR_SCHEMA,
         f"{path}: not a {SIDECAR_SCHEMA} sidecar")
    need(record.get("node") == tag, f"{path}: names node {record.get('node')!r}, not {tag}")
    daemon, actions = record.get("daemon"), record.get("actions")
    need(isinstance(daemon, dict) and isinstance(actions, dict) and isinstance(record.get("bootstraps"), list),
         f"{path}: no daemon, actions and bootstraps")
    need(daemon.get("placement") in ("step_cgroup", "scope"),
         f"{path}: {tag}'s buck2 daemon is unmeasured: {daemon.get('reason')}")
    if daemon["placement"] == "scope":
        need(whole(daemon.get("cpu_usec")) and daemon["cpu_usec"] >= 0, f"{path}: the daemon scope's cpu_usec is not a count")
    need(actions.get("measured") is True, f"{path}: {tag}'s actions are unmeasured: {actions.get('reason')}")
    need(whole(actions.get("total")) and whole(actions.get("cached")) and isinstance(actions.get("by_kind"), dict),
         f"{path}: actions without total, cached and by_kind counts")
    for row in record["bootstraps"]:
        need(isinstance(row, dict) and isinstance(row.get("name"), str) and isinstance(row.get("measured"), bool),
             f"{path}: a malformed bootstrap row {row!r}")
    return record


def buck_steps(run: Run) -> tuple[dict[str, dict], dict[str, str]]:
    """Each Buck step that ran buck2, with its sidecar; and each that ran its no-op cargo branch."""
    found, cargo = {}, {}
    for tag in run.holders:
        if not run.ran(tag):
            continue
        field = MODE_SWITCHED.get(tag)
        if field is not None:
            mode = run.modes[field]
            need(mode in ("cargo", "buck"),
                 f"{run.root}/history-row.json: {field} is {mode!r}, so whether {tag} ran buck2 is unknown")
            if mode == "cargo":
                cargo[tag] = f"{field} cargo"
                continue
        need(run.attempts[tag] == 1 and len(run.profiles.get(tag, [])) <= 1,
             f"{run.root}: Buck step {tag} ran {max(run.attempts[tag], len(run.profiles.get(tag, [])))} times, so its cost is not one build's")
        found[tag] = sidecar(run, tag)
    directory = run.root / "e2e" / "buck-measure"
    for file in sorted(directory.glob("*.json")) if directory.is_dir() else []:
        if file.stem in found:
            continue
        why = ("holds no buck_daemon unit" if file.stem not in run.holders
               else f"ran no buck2 ({cargo[file.stem]})" if file.stem in cargo else "did not run")
        raise Refusal(f"{file}: a sidecar from {file.stem}, which {why}")
    return found, cargo


def cost_table(name: str, run: Run, tags: list[str], buck: tuple[dict[str, dict], dict[str, str]]) -> tuple[list[str], float]:
    measured, cargo = buck
    lines, total = [], 0.0
    for tag in tags:
        cgroup = run.cpu(tag)
        record = measured.get(tag)
        extra, daemon = "", 0.0
        if tag in cargo:
            extra = f"  ran no buck2 ({cargo[tag]})"
        elif record is not None:
            placement = record["daemon"]["placement"]
            if placement == "scope":
                daemon = record["daemon"]["cpu_usec"] / 1e6
            boot = sum(row["user_s"] + row["sys_s"] for row in record["bootstraps"] if row["measured"])
            unmeasured_boot = [row["name"] for row in record["bootstraps"] if not row["measured"]]
            actions = record["actions"]
            hits = f"{100 * actions['cached'] / actions['total']:.1f}%" if actions["total"] else "n/a"
            disk = record.get("disk") or {}
            extra = (f"  daemon {'in cgroup' if placement == 'step_cgroup' else f'scope {daemon:.1f}'}"
                     f"  bootstraps {boot:.1f}{' (unmeasured: ' + ', '.join(unmeasured_boot) + ')' if unmeasured_boot else ''}"
                     f"  actions {actions['total']}, cache hits {actions['cached']} ({hits}) "
                     f"{json.dumps(actions['by_kind'], sort_keys=True)}")
            if disk.get("measured"):
                extra += f"  buck-out {disk['buck_out_allocated_bytes'] / 2**30:.2f} GiB"
        total += cgroup + daemon
        lines.append(f"  {name} {tag:48s} CPU-s {cgroup + daemon:9.1f}{extra}")
    return lines, total


def kept_bytes(run: Run, directory: str) -> int:
    path = run.root / "e2e" / directory
    return sum(file.stat().st_size for file in path.iterdir() if file.is_file()) if path.is_dir() else 0


def compare(run_a: Run, run_b: Run) -> int:
    measured_a, measured_b = buck_steps(run_a), buck_steps(run_b)
    differences: list[str] = []
    for tag in sorted(set(run_a.counted) ^ set(run_b.counted)):
        differences.append(f"  counted step {tag} is in {'A' if tag in run_a.counted else 'B'}'s plan only")
    shared = sorted(set(run_a.counted) & set(run_b.counted))
    need(bool(shared), "the runs share no counted step: there is nothing to compare")
    flaky: list[str] = []
    tests = 0
    for tag in shared:
        outcomes_a, selection_a = counted_results(run_a, tag)
        outcomes_b, selection_b = counted_results(run_b, tag)
        if selection_a != selection_b:
            differences.append(f"  {tag}: the selection changed: A {json.dumps(selection_a)}, B {json.dumps(selection_b)}")
        for test in sorted(set(outcomes_a) | set(outcomes_b)):
            tests += 1
            a, b = outcomes_a.get(test), outcomes_b.get(test)
            if a == b:
                continue
            # The same outcome (kind and cause) with a different number of attempts is a retry
            # difference, listed apart like a pass that needed retries; it is not a difference.
            if a is not None and b is not None and a[:2] == b[:2]:
                flaky.append(f"  {tag}: {test}: A {describe(a)} after {a[2]} attempt(s), B after {b[2]}")
            else:
                differences.append(f"  {tag}: {test}: A {describe(a)}, B {describe(b)}")

    for title, select in (("Build steps (build.*, setup.*)", lambda tag: tag.split(".")[0] in ("build", "setup")),
                          ("Buck E2E steps (e2e.buck_*)", lambda tag: tag.startswith("e2e.buck_"))):
        tags_a = sorted(tag for tag in run_a.steps if run_a.ran(tag) and select(tag))
        tags_b = sorted(tag for tag in run_b.steps if run_b.ran(tag) and select(tag))
        if not tags_a and not tags_b:
            continue
        lines_a, total_a = cost_table("A", run_a, tags_a, measured_a)
        lines_b, total_b = cost_table("B", run_b, tags_b, measured_b)
        print(f"{title}, CPU-s (the step's cgroup user+sys, plus a buck2 daemon's systemd scope outside it):")
        print("\n".join(lines_a + lines_b))
        ratio = f"{total_b / total_a:.3f}" if total_a else "n/a"
        print(f"  total: A {total_a:.1f}  B {total_b:.1f}  B/A {ratio}")

    print(f"Test outcomes: {len(shared)} counted steps in both plans, {tests} tests; {len(differences)} differences, "
          f"{len(flaky)} retry differences (the same outcome after a different number of attempts)")
    print("\n".join(differences))
    if flaky:
        print("Retry differences (not a difference):")
        print("\n".join(flaky))
    for name, run in (("A", run_a), ("B", run_b)):
        if run.foreign:
            print(f"Not read ({name}): kept records of steps not in the run's plan: {', '.join(sorted(run.foreign))}")
        print(f"Kept bytes ({name}): nextest-events {kept_bytes(run, 'nextest-events')}, "
              f"buck-measure {kept_bytes(run, 'buck-measure')}")
    return 1 if differences else 0


def main(argv: list[str]) -> int:
    if len(argv) != 2:
        print(__doc__.split("\n\n")[1], file=sys.stderr)
        return 2
    try:
        return compare(Run(Path(argv[0])), Run(Path(argv[1])))
    except Refusal as error:
        print(f"buck-build-compare: refused: {error}", file=sys.stderr)
    except Exception as error:  # Any other input fault is a refusal too, never a difference (1).
        print(f"buck-build-compare: refused: malformed input: {type(error).__name__}: {error}", file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
