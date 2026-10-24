#!/usr/bin/env python3
"""Wrap `test-harness parity export` rows as parity-ledger/v1 fixture rows.

Usage: fixture_wrap.py <export.jsonl> <e2e-root> <rows.jsonl out> <report.json out>

Each exported source row becomes one line: compact JSON of schema
"parity-ledger/v1", event_type "parity.record",
event_id = sha256("validate\\0<run_id>\\0<cell>\\0<lane>\\0<node>"), team,
host, emitted_at, producer "validate", run_id, hermit_sha,
source_tree_dirty false, then the source row's cell, test_id, backend,
verdict, unavailable_class, operand, reason and source (the
ParityLedgerRow field order), then ,"record": followed by the post-pass
record text spliced byte-for-byte from <e2e-root>/<lane>/<node>/parity.jsonl.

Every record line of every parity.jsonl under <e2e-root> must be used exactly
once, and every exported record must equal (as JSON) the line spliced for it.
"""

import hashlib
import json
import sys
from collections import Counter
from pathlib import Path

TEAM = "hermit"
HOST = "fixture-host-b"
EMITTED_AT = "2026-09-29T04:10:00Z"
PRODUCER = "validate"
SOURCE_KEYS = ["cell", "test_id", "backend", "verdict", "unavailable_class",
               "operand", "reason", "run_id", "hermit_sha", "source", "record"]
ORIGIN_KEYS = ["lane", "node", "post_pass_state", "status_sha256",
               "records_sha256", "hermit_bin_sha256", "scope_source"]


def sha256_hex(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def compact(value) -> str:
    return json.dumps(value, separators=(",", ":"), ensure_ascii=False)


def main():
    export_path, root, rows_out, report_out = map(Path, sys.argv[1:5])
    export_lines = [l for l in export_path.read_text().splitlines() if l.strip()]
    records = {}   # (lane, node) -> list of (line, parsed, used)
    for status in sorted(root.glob("*/*/parity.status.json")):
        lane, node = status.parent.parent.name, status.parent.name
        lines = [l for l in (status.parent / "parity.jsonl").read_text().splitlines() if l.strip()]
        records[(lane, node)] = [[l, json.loads(l), 0] for l in lines]
    out = []
    per_source = Counter()
    for n, text in enumerate(export_lines, 1):
        row = json.loads(text)
        assert list(row) == SOURCE_KEYS, f"export row {n}: keys {list(row)}"
        assert list(row["source"]) == ORIGIN_KEYS, f"export row {n}: source keys {list(row['source'])}"
        assert row["record"] is not None, f"export row {n}: record-missing row {row['cell']}"
        assert row["run_id"] and row["hermit_sha"], f"export row {n}: no run_id/hermit_sha"
        lane, node = row["source"]["lane"], row["source"]["node"]
        assert row["cell"] == f"{row['test_id']}@{row['backend']}", f"export row {n}: cell {row['cell']}"
        matches = [r for r in records[(lane, node)] if r[1] == row["record"]]
        assert len(matches) == 1, f"export row {n}: {len(matches)} matching record lines"
        match = matches[0]
        match[2] += 1
        line = match[0]
        for field in ("test_id", "backend", "verdict", "unavailable_class", "operand", "reason",
                      "run_id", "hermit_sha"):
            assert match[1][field] == row[field], f"export row {n}: {field} differs from its record"
        envelope = {
            "schema": "parity-ledger/v1",
            "event_type": "parity.record",
            "event_id": sha256_hex(
                f"{PRODUCER}\0{row['run_id']}\0{row['cell']}\0{lane}\0{node}".encode()),
            "team": TEAM,
            "host": HOST,
            "emitted_at": EMITTED_AT,
            "producer": PRODUCER,
            "run_id": row["run_id"],
            "hermit_sha": row["hermit_sha"],
            "source_tree_dirty": False,
            "cell": row["cell"],
            "test_id": row["test_id"],
            "backend": row["backend"],
            "verdict": row["verdict"],
            "unavailable_class": row["unavailable_class"],
            "operand": row["operand"],
            "reason": row["reason"],
            "source": row["source"],
        }
        head = compact(envelope)
        assert head.endswith("}")
        wrapped = head[:-1] + ',"record":' + line + "}"
        parsed = json.loads(wrapped)
        assert parsed["record"] == row["record"]
        assert list(parsed) == list(envelope) + ["record"]
        out.append(wrapped)
        per_source[(lane, node)] += 1
    unused = {k: sum(1 for r in v if r[2] == 0) for k, v in records.items()}
    reused = {k: sum(1 for r in v if r[2] > 1) for k, v in records.items()}
    assert all(v == 0 for v in unused.values()), f"unused record lines: {unused}"
    assert all(v == 0 for v in reused.values()), f"reused record lines: {reused}"
    data = "".join(l + "\n" for l in out).encode()
    rows_out.write_bytes(data)
    report = {
        "script_sha256": sha256_hex(Path(__file__).read_bytes()),
        "export": str(export_path),
        "export_sha256": sha256_hex(export_path.read_bytes()),
        "export_rows": len(export_lines),
        "rows": len(out),
        "rows_jsonl_sha256": sha256_hex(data),
        "per_source": {f"{l}/{n}": c for (l, n), c in sorted(per_source.items())},
        "sources": {
            f"{l}/{n}": {
                "records": len(v),
                "records_sha256": sha256_hex((root / l / n / "parity.jsonl").read_bytes()),
                "status_sha256": sha256_hex((root / l / n / "parity.status.json").read_bytes()),
            }
            for (l, n), v in sorted(records.items())
        },
        "verdicts": dict(Counter(json.loads(l)["verdict"] for l in out)),
        "unavailable_classes": dict(Counter(str(json.loads(l)["unavailable_class"]) for l in out)),
        "test_id_prefixes": dict(Counter(json.loads(l)["test_id"].split("/")[0] for l in out)),
    }
    Path(report_out).write_text(json.dumps(report, indent=1) + "\n")
    print(json.dumps(report, indent=1))
    return 0


if __name__ == "__main__":
    sys.exit(main())
