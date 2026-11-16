#!/usr/bin/env python3
"""Check build.json against the published summary in a built site tree.

Reads <site>/compatibility/latest/build.json and
data/site-summary.json.gz and refuses (exit 1, naming every
problem) unless: exactly one artifact entry records the summary
with matching sha256 and byte count, the summary's cells list
exists, and the measured-cell count (latest_result in
{pass, product_failure, timeout}) equals build.json's
green_cell_count + red_cell_count. Any missing key is a refusal,
never a skip. Writes a short report to $GITHUB_STEP_SUMMARY when
set, else stdout. Usage: check-site-data-consistency.py SITE_DIR
where SITE_DIR holds compatibility/latest (the site artifact root).
"""
from __future__ import annotations

import gzip
import zlib
import hashlib
import json
import os
import pathlib
import sys

MEASURED_RESULTS = {"pass", "product_failure", "timeout"}
SUMMARY_PATH = "data/site-summary.json.gz"


def check(root: pathlib.Path) -> list[str]:
    problems: list[str] = []
    try:
        build = json.loads((root / "build.json").read_text())
    except (OSError, ValueError) as error:
        return [f"cannot read build.json: {error}"]
    summary_path = root / SUMMARY_PATH
    if not summary_path.is_file():
        return [f"{SUMMARY_PATH} missing"]
    body = summary_path.read_bytes()
    entries = [a for a in build.get("artifacts") or []
               if isinstance(a, dict) and a.get("path") == SUMMARY_PATH]
    if len(entries) != 1:
        problems.append(
            f"build.json records {len(entries)} artifact entries for "
            f"{SUMMARY_PATH} (want exactly 1)")
    elif entries[0].get("sha256") != hashlib.sha256(body).hexdigest() \
            or entries[0].get("bytes") != len(body):
        problems.append(
            "site-summary bytes do not match build.json "
            f"(recorded sha256 {entries[0].get('sha256')}, "
            f"{entries[0].get('bytes')} bytes)")
    try:
        summary = json.loads(gzip.decompress(body))
    except (OSError, EOFError, ValueError, zlib.error) as error:
        return problems + [f"summary unreadable: {error}"]
    cells = summary.get("cells")
    if not isinstance(cells, list):
        problems.append("site-summary.json has no cells list")
        measured = -1
    else:
        measured = sum(1 for c in cells
                       if isinstance(c, dict)
                       and c.get("latest_result") in MEASURED_RESULTS)
    counts = build.get("counts") or {}
    if "green_cell_count" not in counts or "red_cell_count" not in counts:
        problems.append(
            "build.json counts missing green_cell_count/red_cell_count")
    else:
        expect = counts["green_cell_count"] + counts["red_cell_count"]
        if measured != expect:
            problems.append(f"measured cells {measured} != green+red {expect}")
    return problems


def main(argv: list[str] | None = None) -> int:
    argv = sys.argv[1:] if argv is None else argv
    if len(argv) != 1:
        print("usage: check-site-data-consistency.py SITE_DIR", file=sys.stderr)
        return 2
    root = pathlib.Path(argv[0]) / "compatibility" / "latest"
    problems = check(root)
    report = "## site data consistency\n\n" + (
        "\n".join(f"- {p}" for p in problems) if problems else "current\n")
    summary_file = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary_file:
        with open(summary_file, "a") as handle:
            handle.write(report)
    else:
        print(report)
    return 1 if problems else 0


if __name__ == "__main__":
    raise SystemExit(main())
