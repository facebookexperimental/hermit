#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

"""Run one Hermit e2e cell under Buck2's stock public test runner.

cell.sh speaks the TPX artifact protocol: it needs per-test artifact
directories, emits one JSON test_done record, and exits 0 for every
harness outcome. The stock runner provides none of that protocol, so
this wrapper supplies it around an unchanged cell.sh invocation:

* it creates a unique execution directory with `artifacts_directory`
  and `annotations_directory` children and points cell.sh at them, so
  cell.sh writes its usual ingest run-id marker, result.json,
  results.jsonl, summary.json and verify evidence there;
* it reads the cell's test_done claim from stdout and maps it to this
  process's exit code, because the stock sh_test runner decides pass
  and fail by exit code alone;
* it refuses a pass claim whose published result.json is missing,
  disagrees on cell or run id, or has no valid producer completion.

Exit codes: 0 pass, 1 cell failed, 3 host-inapplicable, 2 wrapper or
protocol error. The stock runner has no skip status for a shell test,
so a host-inapplicable claim fails closed (3) instead of reporting a
pass the harness never gave; the artifacts still classify it as
HOST-INAPPLICABLE for ingest.
"""

import argparse
import json
import os
import secrets
import subprocess
import sys
import time
from pathlib import Path


def locate_cell_script(bundle):
    """cell.sh beside this script, in the checkout, or in the bundle src."""
    candidates = []
    script_dir = Path(__file__).resolve().parent
    candidates.append(script_dir / "cell.sh")
    candidates.append(Path.cwd() / "ci" / "buck-e2e" / "cell.sh")
    candidates.append(
        Path.cwd() / "hermetic_infra" / "hermit" / "ci" / "buck-e2e" / "cell.sh"
    )
    if bundle:
        candidates.append(Path(bundle) / "src" / "ci" / "buck-e2e" / "cell.sh")
    for candidate in candidates:
        if candidate.is_file():
            return candidate
    return None


def project_root():
    """The checkout root: the first ancestor holding the ingest entry point."""
    for base in [Path.cwd(), *Path.cwd().parents]:
        if (base / "ci" / "buck-e2e" / "ingest.py").is_file():
            return base
        if (base / "hermetic_infra" / "hermit" / "ci" / "buck-e2e" / "ingest.py").is_file():
            return base
    return None


def test_done_records(stdout):
    records = []
    for line in stdout.splitlines():
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            record = json.loads(line)
        except (ValueError):
            continue
        if isinstance(record, dict) and record.get("op") == "test_done":
            records.append(record)
    return records


def details_of(record):
    raw = record.get("details")
    if not isinstance(raw, str):
        return None
    try:
        value = json.loads(raw)
        return value if isinstance(value, dict) else None
    except ValueError:
        pass
    # The published record leads its details with a human summary; the
    # harness's JSON object is the last JSON-looking line.
    for line in reversed(raw.splitlines()):
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            value = json.loads(line)
        except ValueError:
            continue
        if isinstance(value, dict):
            return value
    return None


def classify(stdout, cell_exit, artifacts):
    """(exit code, one-line verdict) for one cell.sh run's stdout and artifacts."""
    records = test_done_records(stdout)
    if len(records) != 1:
        return 2, f"ERROR: expected exactly one test_done record, saw {len(records)}"
    record = records[0]
    status = record.get("status")
    details = details_of(record)
    if status not in ("passed", "failed", "skipped") or details is None:
        return 2, f"ERROR: unusable test_done record (status {status!r})"
    outcome = details.get("outcome")
    if cell_exit != 0:
        return 2, f"ERROR: cell.sh exited {cell_exit} after reporting {status}"
    markers = sorted(p.name for p in artifacts.glob("run_id.*") if p.is_file())
    if status == "passed":
        problem = passed_evidence_problem(artifacts, record, markers)
        if outcome != "PASS" or problem is not None:
            return 2, f"ERROR: pass claim without complete evidence: {problem or outcome!r}"
        return 0, "PASS"
    if status == "skipped" and outcome == "HOST-INAPPLICABLE":
        reason = details.get("reason") or "host-inapplicable"
        return 3, f"HOST-INAPPLICABLE (stock runner has no skip status; failing closed): {reason}"
    if status == "failed":
        return 1, f"FAIL: outcome {outcome}"
    if outcome == "FAIL":
        return 1, f"FAIL: outcome {outcome}"
    return 2, f"ERROR: skipped record with outcome {outcome!r}"


def passed_evidence_problem(artifacts, record, markers):
    """Why a pass claim's published evidence is incomplete, or None."""
    result_path = artifacts / "result.json"
    try:
        result = json.loads(result_path.read_text())
    except (OSError, ValueError) as error:
        return f"result.json unreadable: {error}"
    if not isinstance(result, dict):
        return "result.json is not a JSON object"
    if result.get("cell") != record.get("test"):
        return f"result.json is for {result.get('cell')!r}"
    if result.get("outcome") != "PASS":
        return f"result.json outcome is {result.get('outcome')!r}"
    if result.get("evidence_complete") is not True:
        return "evidence_complete is not true"
    completion = result.get("producer_completion")
    if not isinstance(completion, dict) or completion.get("status") != "valid":
        return "no valid producer completion"
    run_id = result.get("run_id")
    if markers != [f"run_id.{run_id}"]:
        return f"run id markers {markers} do not name result run {run_id!r}"
    return None


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bundle", default=os.environ.get("HERMIT_E2E_BUNDLE"))
    parser.add_argument("--artifact-root", default="buck-out/v2/test/execution")
    parser.add_argument("--slug", required=True)
    parser.add_argument("test")
    parser.add_argument("mode")
    parser.add_argument("backend")
    args = parser.parse_args(argv)

    cell_script = locate_cell_script(args.bundle)
    if cell_script is None:
        print("cell-stock: cannot locate cell.sh", file=sys.stderr)
        return 2
    root = project_root()
    if root is None:
        print("cell-stock: cannot locate the project root", file=sys.stderr)
        return 2
    artifact_root = Path(args.artifact_root)
    if not artifact_root.is_absolute():
        artifact_root = root / artifact_root
    unique = f"{os.getpid()}-{time.time_ns()}-{secrets.token_hex(4)}"
    execution = artifact_root / "stock" / args.slug / unique
    artifacts = execution / "artifacts_directory"
    annotations = execution / "annotations_directory"
    artifacts.mkdir(parents=True)
    annotations.mkdir()

    env = dict(os.environ)
    if args.bundle:
        env["HERMIT_E2E_BUNDLE"] = str(Path(args.bundle).resolve())
    env["TEST_RESULT_ARTIFACTS_DIR"] = str(artifacts)
    env["TEST_RESULT_ARTIFACT_ANNOTATIONS_DIR"] = str(annotations)
    completed = subprocess.run(
        ["/bin/bash", str(cell_script), args.test, args.mode, args.backend],
        env=env,
        capture_output=True,
        text=True,
        errors="replace",
    )
    sys.stdout.write(completed.stdout)
    sys.stderr.write(completed.stderr)
    code, verdict = classify(completed.stdout, completed.returncode, artifacts)
    cell = f"{args.test}/{args.mode}@{args.backend}"
    print(
        f"cell-stock: {cell}: {verdict} (artifacts_directory {artifacts})",
        file=sys.stderr,
    )
    return code


if __name__ == "__main__":
    sys.exit(main())

