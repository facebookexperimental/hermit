#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

"""Read a Buck E2E cell subset (ci/buck-e2e/subsets/NAME.json) against the plan.

usage: subset.py SUBSET_JSON PLAN_JSON plan OUT     write the plan narrowed to the subset
       subset.py SUBSET_JSON PLAN_JSON buck-config  print the -c flags that define it in Buck
       subset.py SUBSET_JSON PLAN_JSON categories   print its categories, one per line
       subset.py SUBSET_JSON PLAN_JSON lanes        print its lanes, one per line
       subset.py SUBSET_JSON PLAN_JSON excluded-backends
                                                    print the plan's backends it leaves out
       subset.py SUBSET_JSON PLAN_JSON counts       print how many plan cells it keeps, by
                                                    backend, and leaves out, by the first of
                                                    lane, category and backend it fails
       subset.py -h | --help                         print this help

A subset names lanes, backends and categories; its cells are the plan cells in all three.
Every name must occur in the plan (a typo would otherwise define an empty or smaller
subset silently), and the subset must hold at least one cell. ci/buck-e2e/subset.bzl reads
the Buck flags where ci/buck-e2e/BUCK defines the cells, the import twins of
scripts/validate.rs --buck-s0 select the same cells with --lane, --category and
--exclude-backend, and ci/buck-e2e/ingest.py refuses unless the cells Buck ran are exactly
this plan's, so ingest, Buck and the judge agree on one set.
"""
import collections
import json
import sys


def refuse(message):
    print(f"subset.py: {message}", file=sys.stderr)
    sys.exit(2)


def load(subset_path, plan_path):
    try:
        with open(subset_path) as f:
            subset = json.load(f)
        with open(plan_path) as f:
            plan = json.load(f)
    except (OSError, ValueError) as error:
        refuse(f"cannot read {subset_path} or {plan_path}: {error}")
    for key in ("lanes", "backends", "categories"):
        values = subset.get(key)
        if not isinstance(values, list) or not values or not all(isinstance(v, str) and v for v in values):
            refuse(f"{subset_path}: {key} must be a non-empty list of names")
        known = {cell[key[:-1] if key != "categories" else "category"] for cell in plan["cells"]}
        unknown = sorted(set(values) - known)
        if unknown:
            refuse(f"{subset_path}: {key} {unknown} do not occur in {plan_path}")
    cells = [
        cell
        for cell in plan["cells"]
        if cell["lane"] in subset["lanes"]
        and cell["backend"] in subset["backends"]
        and cell["category"] in subset["categories"]
    ]
    if not cells:
        refuse(f"{subset_path} selects no cell of {plan_path}")
    return subset, plan, cells


def by_count(counter):
    return ", ".join(f"{name} {n}" for name, n in sorted(counter.items(), key=lambda kv: (-kv[1], kv[0])))


def counts(subset, plan, cells):
    """Two lines: the cells kept, by backend; the cells left out, by the first of lane,
    category and backend that the subset does not name."""
    kept = collections.Counter(cell["backend"] for cell in cells)
    left_out = {"lane": collections.Counter(), "category": collections.Counter(), "backend": collections.Counter()}
    for cell in plan["cells"]:
        for key, names in (("lane", "lanes"), ("category", "categories"), ("backend", "backends")):
            if cell[key] not in subset[names]:
                left_out[key][cell[key]] += 1
                break
    total = len(plan["cells"])
    print(f"{subset.get('name', 'subset')}: {len(cells)} of the plan's {total} cells, by backend: {by_count(kept)}")
    parts = [f"{key} {by_count(left_out[key])}" for key in ("lane", "category", "backend") if left_out[key]]
    print(f"left out: {total - len(cells)}: " + "; ".join(parts))


def main(argv):
    if len(argv) == 2 and argv[1] in ("-h", "--help"):
        print(__doc__.strip())
        return
    if len(argv) < 4:
        refuse(__doc__.strip().splitlines()[2])
    subset, plan, cells = load(argv[1], argv[2])
    action = argv[3]
    if action == "plan" and len(argv) == 5:
        narrowed = dict(plan, cells=cells)
        with open(argv[4], "w") as f:
            json.dump(narrowed, f)
        print(len(cells))
    elif action == "buck-config" and len(argv) == 4:
        for key in ("lanes", "backends", "categories"):
            print(f"-c\nhermit_e2e.subset_{key}={','.join(sorted(subset[key]))}")
    elif action == "categories" and len(argv) == 4:
        print("\n".join(sorted(subset["categories"])))
    elif action == "lanes" and len(argv) == 4:
        print("\n".join(sorted(subset["lanes"])))
    elif action == "excluded-backends" and len(argv) == 4:
        backends = {cell["backend"] for cell in plan["cells"]}
        print("\n".join(sorted(backends - set(subset["backends"]))))
    elif action == "counts" and len(argv) == 4:
        counts(subset, plan, cells)
    else:
        refuse(f"unknown action or arguments: {argv[3:]}")


if __name__ == "__main__":
    main(sys.argv)
