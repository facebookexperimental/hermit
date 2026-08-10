#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

"""Merge ``tests/e2e/manifests/inventory/test-files.json`` safely.

Every test-corpus PR adds an entry to the inventory, so the file is a constant
source of merge conflicts. Those conflicts are textual, not semantic: the gate
in ``ci/test_harness.sh`` sorts both sides before comparing, so entry ORDER
carries no meaning and two PRs registering different files never actually
disagree. The mechanical resolution is therefore a union keyed by ``path``.

A NAIVE UNION IS UNSAFE, which is the reason this script exists rather than a
line in a runbook. Renames make it destructive. When the DBI backend was
renamed to DBT (``e565b1ab``) the FILES were renamed too -- ``tests/c/dbi_*.c``
became ``tests/c/dbt_*.c`` -- but a branch that predates the rename still lists
the old paths. Unioning that branch's inventory into current main silently
resurrects nine entries for files that no longer exist, and the next
``audit-inventory`` fails with a stale-inventory diff that names the phantom
paths without explaining where they came from. Union alone reintroduces
whatever the other side has forgotten to delete.

So the union here is always filtered against the files that actually exist,
enumerated exactly the way the gate enumerates them::

    git ls-files --cached --others --exclude-standard -- tests

which is tracked files plus genuinely new untracked ones, minus ignored build
output. Pruning is on by default and ``--no-prune`` exists only to make the
hazard visible in tests.

When both sides register the SAME path, the first input wins and later ones are
reported, not merged. That precedence is deliberate and is the same lesson as
pruning. A stale branch's copy of an entry that already exists is, by
definition, the older description of it; letting it win reverts whatever main
has since changed. The DBI/DBT rename shows this concretely -- merging one real
pre-rename branch into current main produces 25 same-path collisions whose only
difference is the rationale prose::

    -  ... calibrated under strict verification for ptrace and DBT ...
    +  ... calibrated under strict verification for ptrace and DBI ...

Taking "theirs" there would quietly undo the rename in 25 rationales. So put
the authoritative inventory first (normally the one in your working tree) and
the branch being folded second: the branch contributes its NEW paths and
nothing else. Use ``--strict`` to refuse on any such collision instead, when
you would rather review them than absorb them.

Because pruning asks the filesystem what exists, ORDER OF OPERATIONS MATTERS:
put the branch's new fixture files in place first, then merge the inventory.
Merging first would prune the very entries you are trying to add, since their
files are not there yet.

Usage::

    ci/merge-test-inventory.py OURS THEIRS [MORE ...] -o OUT
    ci/merge-test-inventory.py --strict OURS THEIRS -o OUT
    ci/merge-test-inventory.py --check FILE
    ci/merge-test-inventory.py --self-test
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
from pathlib import Path
from typing import Dict, Iterable, List, Optional, Sequence, Tuple

ENTRY_KEYS = ("disposition", "path", "runner", "why")


def _load(path: Path) -> dict:
    with path.open() as handle:
        doc = json.load(handle)
    if not isinstance(doc, dict) or not isinstance(doc.get("files"), list):
        raise SystemExit(f"{path}: not a test inventory (missing .files array)")
    return doc


def default_repo_root() -> Path:
    """Locate the repository to enumerate.

    Prefer the checkout the caller is standing in, so the script keeps working
    when it is invoked through a copy outside the tree (a rebase helper, a
    scratch checkout). Fall back to the location of the script itself.
    """
    try:
        result = subprocess.run(
            ["git", "rev-parse", "--show-toplevel"],
            check=True,
            capture_output=True,
            text=True,
        )
        return Path(result.stdout.strip())
    except (subprocess.CalledProcessError, FileNotFoundError):
        return Path(__file__).resolve().parent.parent


def existing_test_files(repo_root: Path) -> set:
    """Enumerate files under tests/ the same way audit_inventory does."""
    try:
        result = subprocess.run(
            [
                "git",
                "-C",
                str(repo_root),
                "ls-files",
                "--cached",
                "--others",
                "--exclude-standard",
                "--",
                "tests",
            ],
            check=True,
            capture_output=True,
            text=True,
        )
    except subprocess.CalledProcessError as exc:
        raise SystemExit(
            f"test-inventory: cannot enumerate tests/ under {repo_root}: "
            f"{exc.stderr.strip() or 'git failed'}\n"
            "Pruning needs a real checkout; pass --repo-root, or run from inside one."
        ) from exc
    return {line for line in result.stdout.splitlines() if line}


def merge(
    docs: Sequence[dict],
    present: Optional[set],
) -> Tuple[List[dict], List[str], List[str]]:
    """Union entries by path, prune phantoms, and sort.

    Earlier inputs take precedence: a later document's version of a path that
    already exists is recorded as an override and discarded, never applied.

    Returns (entries, pruned_paths, overridden_paths).
    """
    chosen: Dict[str, dict] = {}
    overrides: List[str] = []
    for doc in docs:
        for entry in doc["files"]:
            path = entry.get("path")
            if not isinstance(path, str):
                raise SystemExit(f"inventory entry without a string path: {entry!r}")
            previous = chosen.get(path)
            if previous is None:
                chosen[path] = entry
            elif previous != entry and path not in overrides:
                overrides.append(path)

    pruned: List[str] = []
    if present is not None:
        for path in sorted(chosen):
            if path not in present:
                pruned.append(path)
                del chosen[path]

    entries = sorted(chosen.values(), key=lambda e: e["path"])
    return entries, pruned, sorted(overrides)


def check(path: Path, repo_root: Path) -> int:
    """Verify a committed inventory is canonical: sorted, and free of phantoms."""
    doc = _load(path)
    paths = [entry["path"] for entry in doc["files"]]
    problems = []
    if paths != sorted(paths):
        problems.append(
            "files[] is not sorted by path; run ci/merge-test-inventory.py to canonicalize"
        )
    duplicates = sorted({p for p in paths if paths.count(p) > 1})
    if duplicates:
        problems.append(f"duplicate paths: {', '.join(duplicates)}")
    phantoms = sorted(set(paths) - existing_test_files(repo_root))
    if phantoms:
        problems.append(
            "entries for files that do not exist (a rename was merged without "
            f"dropping the old paths): {', '.join(phantoms)}"
        )
    for problem in problems:
        print(f"test-inventory: {problem}", file=sys.stderr)
    return 1 if problems else 0


def _self_test() -> int:
    """Bracket the merge from both sides: it must add, and it must refuse."""

    def doc(*entries: dict) -> dict:
        return {"files": list(entries), "schema": 2}

    def entry(path: str, disposition: str = "manifest-test") -> dict:
        runner = "ci/test_harness.sh"
        return {
            "disposition": disposition,
            "path": path,
            "runner": runner,
            "why": f"{path} is owned by {runner}: fixture used by the self-test.",
        }

    failures = []

    def expect(name: str, condition: bool) -> None:
        status = "ok  " if condition else "FAIL"
        print(f"  {status} {name}")
        if not condition:
            failures.append(name)

    present = {"tests/a.c", "tests/b.c", "tests/c.c"}

    # POSITIVE: a union must actually combine disjoint registrations. Without
    # this case a merge that dropped everything would satisfy every check below.
    ours = doc(entry("tests/a.c"))
    theirs = doc(entry("tests/b.c"))
    entries, pruned, overrides = merge([ours, theirs], present)
    paths = [e["path"] for e in entries]
    expect("union combines disjoint entries", paths == ["tests/a.c", "tests/b.c"])
    expect("union reports no false override", overrides == [])
    expect("union prunes nothing when all files exist", pruned == [])

    # Output is sorted regardless of input order, so append position stops mattering.
    entries, _, _ = merge([doc(entry("tests/c.c")), doc(entry("tests/a.c"))], present)
    expect(
        "output is sorted by path",
        [e["path"] for e in entries] == ["tests/a.c", "tests/c.c"],
    )

    # NEGATIVE, the whole reason this script exists: a stale side still lists a
    # path the rename deleted, and the union must NOT resurrect it.
    stale = doc(entry("tests/a.c"), entry("tests/dbi_gone.c"))
    entries, pruned, _ = merge([doc(entry("tests/a.c")), stale], present)
    expect(
        "renamed-away path is pruned, not resurrected",
        [e["path"] for e in entries] == ["tests/a.c"] and pruned == ["tests/dbi_gone.c"],
    )

    # And the hazard is real: without pruning the phantom survives. This proves
    # the filter is load-bearing rather than decorative.
    entries, pruned, _ = merge([doc(entry("tests/a.c")), stale], None)
    expect(
        "without --prune the phantom would survive (hazard is real)",
        [e["path"] for e in entries] == ["tests/a.c", "tests/dbi_gone.c"],
    )

    # NEGATIVE: a stale side's version of an EXISTING path must not win. This
    # is the DBI/DBT rationale-prose case; letting "theirs" through there
    # reverts the rename in every colliding entry.
    mine = doc(entry("tests/a.c", "manifest-test"))
    yours = doc(entry("tests/a.c", "guest-fixture"))
    entries, _, overrides = merge([mine, yours], present)
    expect(
        "first input wins for an existing path",
        [e["disposition"] for e in entries] == ["manifest-test"],
    )
    expect("the discarded override is reported", overrides == ["tests/a.c"])

    # ...and precedence follows argument order, not entry content.
    entries, _, _ = merge([yours, mine], present)
    expect(
        "precedence follows argument order",
        [e["disposition"] for e in entries] == ["guest-fixture"],
    )

    # Identical duplicates are not an override at all.
    _, _, overrides = merge([mine, doc(entry("tests/a.c", "manifest-test"))], present)
    expect("identical duplicate is not an override", overrides == [])

    if failures:
        print(f"\nself-test FAILED: {len(failures)} case(s)", file=sys.stderr)
        return 1
    print("\nall merge-test-inventory self-test cases passed")
    return 0


def main(argv: Optional[Iterable[str]] = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("inputs", nargs="*", type=Path, help="inventories to merge")
    parser.add_argument("-o", "--output", type=Path, help="destination (default: stdout)")
    parser.add_argument("--check", type=Path, help="verify an inventory is canonical")
    parser.add_argument("--self-test", action="store_true", help="run the self-test")
    parser.add_argument(
        "--repo-root",
        type=Path,
        default=None,
        help="repository root used to enumerate existing test files "
        "(default: the checkout containing the current directory)",
    )
    parser.add_argument(
        "--no-prune",
        action="store_true",
        help="keep entries whose file does not exist (unsafe; for tests only)",
    )
    parser.add_argument(
        "--strict",
        action="store_true",
        help="refuse when a later input redefines an existing path, instead of "
        "keeping the first and reporting it",
    )
    args = parser.parse_args(list(argv) if argv is not None else None)
    repo_root = args.repo_root if args.repo_root is not None else default_repo_root()

    if args.self_test:
        return _self_test()
    if args.check is not None:
        return check(args.check, repo_root)
    if not args.inputs:
        parser.error("give at least one inventory, or --check / --self-test")

    docs = [_load(path) for path in args.inputs]
    present = None if args.no_prune else existing_test_files(repo_root)
    entries, pruned, overrides = merge(docs, present)

    if overrides and args.strict:
        for path in overrides:
            print(
                f"test-inventory: {path} is redefined by a later input; "
                "resolve by hand (--strict)",
                file=sys.stderr,
            )
        return 2

    merged = {"files": entries, "schema": docs[0].get("schema", 2)}
    text = json.dumps(merged, indent=2, sort_keys=True) + "\n"
    if args.output is None:
        sys.stdout.write(text)
    else:
        args.output.write_text(text)

    for path in pruned:
        print(f"test-inventory: pruned entry for missing file {path}", file=sys.stderr)
    if overrides:
        print(
            f"test-inventory: kept {args.inputs[0]}'s version of {len(overrides)} "
            "path(s) redefined by a later input (a stale branch usually predates "
            "a rename; re-run with --strict to review them)",
            file=sys.stderr,
        )
    print(
        f"test-inventory: merged {len(args.inputs)} inventories -> "
        f"{len(entries)} entries ({len(pruned)} pruned)",
        file=sys.stderr,
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
