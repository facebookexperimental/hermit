#!/usr/bin/env python3
"""Rewrite one copy of the s17 parity inputs so `test-harness parity compare`
can re-measure them at a commit where backend-parity-c is retired.

For each copied <root>/e2e/<lane>/manifest_backend_parity_c/results.jsonl it
changes exactly two fields of each row and nothing else:
  * "test": a retired id is replaced by its successor from retired-ids.json
    (backend-parity-c/X -> c-programs/X; pidfd-open-self -> pidfd-open-self-pair);
  * "argv": the one value after --verify-log-dir, "/results/..." (the run's
    container path), becomes "<root>/e2e/..." (the same directory in this copy).
Guest inputs (guest_argv, env, the --env/--workdir/--mount=/--bind= argv
entries), artifact_dir, category and every other field are left as recorded,
and the script asserts that. It also writes the --cell list per lane: every
cell the run's own post-pass measured (its parity.jsonl), mapped the same way.

Usage: map_s17.py <copy root> <retired-ids.json> <report.json>
"""
import copy
import hashlib
import json
import os
import sys

FLAG = "--verify-log-dir"
PREFIX = "/results/"
LANES = ("portable", "privileged")
NODE = "manifest_backend_parity_c"


def sha256_file(path):
    with open(path, "rb") as f:
        return hashlib.sha256(f.read()).hexdigest()


def main():
    root, retired_path, report_path = sys.argv[1:4]
    root = os.path.abspath(root)
    retired = json.load(open(retired_path))
    successor = {}
    for retirement in retired["retirements"]:
        for old, new in retirement["ids"].items():
            assert old not in successor, old
            successor[old] = new
    report = {
        "script": os.path.abspath(__file__),
        "script_sha256": sha256_file(__file__),
        "retired_ids": os.path.abspath(retired_path),
        "retired_ids_sha256": sha256_file(retired_path),
        "retired_ids_mapped": len(successor),
        "root": root,
        "lanes": {},
    }
    for lane in LANES:
        node = os.path.join(root, "e2e", lane, NODE)
        results = os.path.join(node, "results.jsonl")
        lines = open(results, encoding="utf-8").read().splitlines()
        counts = {
            "results_sha256_in": sha256_file(results),
            "rows_in": 0,
            "rows_mapped": 0,
            "rows_already_live": 0,
            "rows_retired_unmapped": 0,
            "argv_rewrites": 0,
            "rows_without_flag": 0,
            "rewritten_dirs_with_one_nonempty_run1_log": 0,
        }
        out = []
        for line in lines:
            if not line.strip():
                continue
            counts["rows_in"] += 1
            row = json.loads(line)
            original = copy.deepcopy(row)
            test = row["test"]
            if test in successor:
                row["test"] = successor[test]
                counts["rows_mapped"] += 1
            elif test.startswith("backend-parity-c/"):
                counts["rows_retired_unmapped"] += 1
            else:
                counts["rows_already_live"] += 1
            argv = row["argv"]
            positions = [i for i, arg in enumerate(argv) if arg == FLAG]
            assert len(positions) <= 1, (test, positions)
            if positions:
                i = positions[0] + 1
                old = argv[i]
                assert old.startswith(PREFIX), (test, old)
                new = os.path.join(root, "e2e", old[len(PREFIX):])
                assert os.path.isdir(new), new
                argv[i] = new
                counts["argv_rewrites"] += 1
                logs = [n for n in os.listdir(new) if n.startswith("run1_log_")]
                if len(logs) == 1 and os.path.getsize(os.path.join(new, logs[0])) > 0:
                    counts["rewritten_dirs_with_one_nonempty_run1_log"] += 1
            else:
                counts["rows_without_flag"] += 1
            # Nothing else changed: undo the two edits and compare.
            undone = copy.deepcopy(row)
            undone["test"] = original["test"]
            if positions:
                undone["argv"][positions[0] + 1] = original["argv"][positions[0] + 1]
            assert undone == original, test
            out.append(json.dumps(row, ensure_ascii=False, separators=(",", ":")))
        assert counts["rows_retired_unmapped"] == 0, counts
        with open(results, "w", encoding="utf-8") as f:
            f.write("\n".join(out) + "\n")
        counts["results_sha256_out"] = sha256_file(results)
        # The cells the run's own post-pass measured, mapped the same way.
        records = [json.loads(l) for l in open(os.path.join(node, "parity.jsonl")) if l.strip()]
        cells = []
        for record in records:
            test = successor.get(record["test_id"], record["test_id"])
            cells.append(f"{test}@{record['backend']}")
        assert len(cells) == len(set(cells)), "duplicate cell"
        cells_path = os.path.join(os.path.dirname(os.path.abspath(report_path)), f"cells-{lane}.txt")
        with open(cells_path, "w") as f:
            f.write("\n".join(cells) + "\n")
        counts["records_in_run_post_pass"] = len(records)
        counts["cells_file"] = cells_path
        counts["cells"] = len(cells)
        report["lanes"][lane] = counts
    with open(report_path, "w") as f:
        json.dump(report, f, indent=2)
        f.write("\n")
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
