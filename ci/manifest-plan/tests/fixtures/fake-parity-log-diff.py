#!/usr/bin/python3
"""Test stand-in for the one `hermit log-diff` call the parity post-pass makes:

    log-diff <golden> <candidate> --json <report> --record-envelope cross-backend-detcore-v1

It selects the `INFO detcore` lines of each log as the compared messages and
writes a schema-2 report carrying every field the real comparator's
cross-backend evidence policy checks: input identities, record counts, the
matched prefix and the first divergence. It exits 0 on a match, 1 on a
divergence and 2 when there is nothing to compare, as `hermit log-diff` does.

Each call is appended to `log-diff-calls` beside this script. A
`logdiff-mode` file beside it plants one defect:
  lie-about-inputs  report a right-hand input that is not the file compared
  contradict-exit   exit with the status of the opposite verdict
  hang              never finish
  invalid-record    give a match a divergence position, which no parity
                    record can carry
"""

import hashlib
import json
import pathlib
import sys
import time

here = pathlib.Path(__file__).resolve().parent
argv = sys.argv[1:]
assert len(argv) == 7 and argv[0] == "log-diff", argv
assert argv[3] == "--json", argv
assert argv[5:] == ["--record-envelope", "cross-backend-detcore-v1"], argv
with (here / "log-diff-calls").open("a") as calls:
    calls.write(json.dumps(argv) + "\n")
mode_file = here / "logdiff-mode"
mode = mode_file.read_text().strip() if mode_file.exists() else ""
if mode == "hang":
    time.sleep(600)

left, right = (pathlib.Path(path).read_bytes() for path in argv[1:3])
raw_left, raw_right = (data.decode().splitlines() for data in (left, right))
selected_left, selected_right = (
    [line for line in raw if line.startswith("INFO detcore")]
    for raw in (raw_left, raw_right)
)
prefix = 0
while (
    prefix < min(len(selected_left), len(selected_right))
    and selected_left[prefix] == selected_right[prefix]
):
    prefix += 1


def identity(data):
    return {"sha256": hashlib.sha256(data).hexdigest(), "bytes": len(data)}


report = {
    "schema": 2,
    "selected_messages": {"left": len(selected_left), "right": len(selected_right)},
    "records": {
        "compared": min(len(raw_left), len(raw_right)),
        "available_left": len(raw_left),
        "available_right": len(raw_right),
        "withheld_incomplete_tail": False,
    },
    "inputs": {"left": identity(left), "right": identity(right)},
    "comparison": {
        "stream": "info",
        "record_envelope": "cross_backend_detcore_v1",
        "unsafe_strip_lines": False,
        "canonicalize_host_addresses": True,
        "require_structured_events": True,
        "ignored_line_substrings": [],
        "skip_commit": False,
        "skip_detlog": False,
        "included_detlog_kinds": ["syscall", "syscall_result", "other"],
        "git_diff": False,
    },
    "first_divergent_syscall": None,
    "first_divergent_scheduler_turn": None,
    "first_divergent_virtual_nanoseconds": None,
    "first_divergent_left_message": None,
    "first_divergent_right_message": None,
}
if not selected_left or not selected_right:
    report["verdict"] = "no_comparable_messages"
    code = 2
elif prefix == len(selected_left) == len(selected_right):
    report["verdict"] = "matched"
    report["matched_prefix_records"] = prefix
    code = 0
else:
    report["verdict"] = "diverged"
    report["matched_prefix_records"] = prefix
    report["first_divergent_record"] = prefix + 1
    report["first_divergent_syscall"] = prefix + 1
    if prefix < len(selected_left):
        report["first_divergent_left_message"] = selected_left[prefix]
    if prefix < len(selected_right):
        report["first_divergent_right_message"] = selected_right[prefix]
    code = 1
if mode == "lie-about-inputs":
    report["inputs"]["right"] = identity(b"INFO detcore: some other log\n")
if mode == "invalid-record" and report["verdict"] == "matched":
    report["first_divergent_record"] = 1
if mode == "contradict-exit" and code in (0, 1):
    code = 1 - code
pathlib.Path(argv[4]).write_text(json.dumps(report))
sys.exit(code)
