#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

"""Re-target legacy backend-parity matrix additions into the shared schema-v2 manifest.

Background
----------
The legacy ``tests/backend-parity/matrix.tsv`` side-matrix (driven by the
standalone ``run_matrix.py`` harness) records parity per backend for freestanding
C guests, but only ptrace/DBI/KVM ever run it -- it is invisible to the
mode x backend x test symmetry enforced over ``tests/e2e/manifests/*.toml`` by
``ci/manifest-plan`` (PR #1518). #1498 removes ``matrix.tsv`` entirely and folds
its catalog back into ``run_matrix.py``. Dozens of open PRs still add a
``matrix.tsv`` row + a fixture + a ``run_matrix.py`` edit; none can land as-is.

This tool mechanizes the coverage migration for those PRs. For each source PR it:

  1. reads the added ``matrix.tsv`` row(s) (6-col L1 schema or 11-col L1+L2
     schema) and the added fixture ``.c`` file(s);
  2. writes the fixture into the working tree (so the manifest ``program`` path
     exists for the lint) if it is not already present;
  3. appends a symmetric ``[[test]]`` block to
     ``tests/e2e/manifests/backend-parity-c.toml`` -- ptrace established first,
     every backend x mode cell declared, DBI/KVM enabled only where the source
     row's ``--verify`` (L2) witness actually passed, everything else disabled
     with a concrete reason carried over from the matrix row;
  4. reclassifies the fixture in
     ``tests/e2e/manifests/inventory/test-files.json`` from the private
     ``guest-fixture`` disposition to ``manifest-test`` so it leaves the
     backend-private tripwire that ``ci/manifest-plan`` ratchets.

It deliberately does NOT touch ``matrix.tsv`` or ``run_matrix.py`` -- those are
retired by #1498; the source PR's edits to them are simply dropped.

The result passes the #1518 symmetry lint: the new test has a ptrace front-door
and never enables a backend without ptrace (so it stays out of
``asymmetric_manifest_tests``), and the fixture is no longer a private
guest-fixture (so ``backend_private_guest_files`` is unchanged from baseline).

This script is Python by design: it lives beside and coordinates with the
existing Python parity harness (``run_matrix.py`` / ``e9patch_corpus.py``) and
reuses that file's exact matrix-row validation semantics.

Usage
-----
    # Dry run (default): show the unified diff for one or more PRs, write nothing.
    tests/backend-parity/retarget_to_manifest.py --pr 1474 1383 1352

    # Apply the changes to the working tree.
    tests/backend-parity/retarget_to_manifest.py --pr 1474 --apply

    # Local mode: convert an explicit row + fixture already in the tree.
    tests/backend-parity/retarget_to_manifest.py \
        --row $'getpriority_identity\tpass\tpass\tpass\t-\t-\tdetlog\tdetlog\tguest\t-\t-' \
        --fixture tests/c/getpriority_identity.c

Idempotent: re-running is a no-op once a test id is present in the manifest and
its fixture is classified ``manifest-test``.
"""

from __future__ import annotations

import argparse
import difflib
import json
import subprocess
import sys
from dataclasses import dataclass, field
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent
REPO_ROOT = SCRIPT_DIR.parent.parent
MANIFEST = REPO_ROOT / "tests/e2e/manifests/backend-parity-c.toml"
INVENTORY = REPO_ROOT / "tests/e2e/manifests/inventory/test-files.json"
BUCKET = "backend-parity-c"
REPO = "rrnewton/hermit"

# Fixtures whose determinism contract needs a privileged lane (CPUID interception
# / /dev/kvm). Everything else is a portable syscall probe.
PRIVILEGED_HINTS = ("cpuid", "rdtsc", "rdseed", "rdrand")

L2_PASS = {"detlog", "guest"}  # a real --verify witness; "gap" means no witness


class ConvertError(Exception):
    """A source row/fixture that cannot be auto-converted; reported, not fatal."""


@dataclass
class MatrixRow:
    name: str
    # L1 pass/gap
    ptrace: str
    dbi: str
    kvm: str
    dbi_reason: str
    kvm_reason: str
    # L2 detlog/guest/gap (empty when the source row is the 6-col schema)
    ptrace_l2: str = ""
    dbi_l2: str = ""
    kvm_l2: str = ""
    dbi_l2_reason: str = ""
    kvm_l2_reason: str = ""
    has_l2: bool = False


def parse_matrix_row(raw: str) -> MatrixRow:
    """Parse a single matrix.tsv data row (6 or 11 tab-separated columns)."""
    cols = raw.rstrip("\n").split("\t")
    if cols and cols[0] == "test_name":
        raise ConvertError("header row, not a data row")
    if len(cols) == 6:
        name, ptrace, dbi, kvm, dbi_r, kvm_r = cols
        row = MatrixRow(name, ptrace, dbi, kvm, dbi_r, kvm_r)
    elif len(cols) == 11:
        (
            name,
            ptrace,
            dbi,
            kvm,
            dbi_r,
            kvm_r,
            p_l2,
            dbi_l2,
            kvm_l2,
            dbi_l2_r,
            kvm_l2_r,
        ) = cols
        row = MatrixRow(
            name,
            ptrace,
            dbi,
            kvm,
            dbi_r,
            kvm_r,
            p_l2,
            dbi_l2,
            kvm_l2,
            dbi_l2_r,
            kvm_l2_r,
            has_l2=True,
        )
    else:
        raise ConvertError(f"unexpected column count {len(cols)} (want 6 or 11)")

    for backend in ("ptrace", "dbi", "kvm"):
        if getattr(row, backend) not in ("pass", "gap"):
            raise ConvertError(f"{row.name}/{backend}: expected pass|gap")
    if row.ptrace != "pass":
        raise ConvertError(
            f"{row.name}: ptrace baseline is not pass; cannot establish front door"
        )
    return row


@dataclass
class Plan:
    name: str
    slug: str
    program: str  # repo-relative fixture path
    lane: str
    enabled: list  # verify-mode enabled backends (always includes ptrace)
    disabled: dict = field(default_factory=dict)  # verify-mode backend -> reason
    fixture_content: str | None = None  # written when the fixture is not in-tree


def build_plan(row: MatrixRow, program: str, fixture_content: str | None) -> Plan:
    slug = row.name.replace("_", "-")
    enabled = ["ptrace"]
    disabled: dict = {}

    def classify(backend: str, l1: str, l1_reason: str, l2: str, l2_reason: str):
        if row.has_l2:
            if l2 in L2_PASS:
                enabled.append(backend)
            elif l2 == "gap":
                disabled[backend] = (
                    l2_reason
                    if l2_reason not in ("", "-")
                    else f"{backend.upper()} has no recorded --verify witness in the source matrix row"
                )
            else:
                raise ConvertError(f"{row.name}/{backend}_l2: expected detlog|guest|gap")
        else:
            # 6-col source: only L1 evidence, no --verify witness -> stay ptrace-first.
            if l1 == "pass":
                disabled[backend] = (
                    f"L1 parity established in the source matrix row; the L2 --verify "
                    f"witness was not recorded, so qualify {backend.upper()} separately"
                )
            else:
                disabled[backend] = (
                    l1_reason
                    if l1_reason not in ("", "-")
                    else f"{backend.upper()} L1 gap in the source matrix row"
                )

    classify("dbi", row.dbi, row.dbi_reason, row.dbi_l2, row.dbi_l2_reason)
    classify("kvm", row.kvm, row.kvm_reason, row.kvm_l2, row.kvm_l2_reason)
    disabled["sabre"] = (
        "Not evaluated in the source backend-parity matrix; qualify SaBRe separately"
    )
    disabled["liteinst"] = (
        "Not evaluated in the source backend-parity matrix; qualify LiteInst separately"
    )

    lane = "privileged" if any(h in row.name.lower() for h in PRIVILEGED_HINTS) else "portable"
    return Plan(row.name, slug, program, lane, enabled, disabled, fixture_content)


def render_test_block(plan: Plan) -> str:
    requires = ["linux", "x86_64", "userns", "ptrace", "cc"]
    if plan.lane == "privileged":
        requires.insert(4, "cpuid")
    requires_toml = ", ".join(f'"{r}"' for r in requires)
    enabled_toml = ", ".join(f'"{b}"' for b in plan.enabled)

    lines = [
        "",
        "[[test]]",
        f'id = "{BUCKET}/{plan.slug}"',
        f'description = "Strict verification for {plan.program}"',
        f'lane = "{plan.lane}"',
        f"requires = [{requires_toml}]",
        "timeout_seconds = 90",
        "occasional = false",
        f'program = "{plan.program}"',
        "observation = { status = true, stdout = true, stderr = false, artifacts = [] }",
        "",
        "[test.modes.verify]",
        "ci = false",
        f"backends_enabled = [{enabled_toml}]",
        "[test.modes.verify.backends_disabled]",
    ]
    for backend in ("dbi", "kvm", "sabre", "liteinst"):
        if backend in plan.disabled:
            lines.append(f'{backend} = "{_escape(plan.disabled[backend])}"')
    lines += [
        "",
        "[test.modes.naked]",
        "ci = false",
        "backends_enabled = []",
        "[test.modes.naked.backends_disabled]",
        'native = "This migration inventories strict verification; it does not assert native nondeterminism"',
        "",
        "[test.modes.replay]",
        "ci = false",
        "backends_enabled = []",
        "[test.modes.replay.backends_disabled]",
        'ptrace = "Record/replay qualification is separate from the initial strict-verification migration"',
        'dbi = "Record/replay is unsupported by DBI"',
        'kvm = "Record/replay is unsupported by KVM"',
        'sabre = "Record/replay is unsupported by SaBRe"',
        'liteinst = "Record/replay is unsupported by LiteInst"',
        "",
        "[test.modes.chaos]",
        "ci = false",
        "backends_enabled = []",
        "[test.modes.chaos.backends_disabled]",
        'ptrace = "Chaos scheduling is only meaningful for guests with an explicit schedule-diversity oracle"',
        'dbi = "Chaos scheduling is unsupported by DBI"',
        'kvm = "Chaos scheduling is unsupported by KVM"',
        'sabre = "Chaos scheduling is unsupported by SaBRe"',
        'liteinst = "Chaos scheduling is unsupported by LiteInst"',
        "",
        "[test.modes.custom]",
        "ci = false",
        "backends_enabled = []",
        "[test.modes.custom.backends_disabled]",
        'ptrace = "No custom edge-case arguments have been calibrated for this C guest"',
        'dbi = "No custom edge-case arguments have been calibrated for this C guest"',
        'kvm = "No custom edge-case arguments have been calibrated for this C guest"',
        'sabre = "No custom edge-case arguments have been calibrated for this C guest"',
        'liteinst = "No custom edge-case arguments have been calibrated for this C guest"',
    ]
    return "\n".join(lines) + "\n"


def _escape(text: str) -> str:
    return text.replace("\\", "\\\\").replace('"', '\\"')


def manifest_has_id(manifest_text: str, plan: Plan) -> bool:
    return f'id = "{BUCKET}/{plan.slug}"' in manifest_text


def inventory_entry(program: str) -> dict:
    return {
        "path": program,
        "disposition": "manifest-test",
        "runner": (
            "ci/test_harness.sh via tests/e2e/manifests/backend-parity-c.toml "
            "(explicit mode selection; ci=false)"
        ),
        "why": (
            f"{program} is owned by ci/test_harness.sh via "
            "tests/e2e/manifests/backend-parity-c.toml (explicit mode selection; "
            "ci=false): Direct C guest is centrally discoverable with ptrace "
            "verification enabled for explicit runs; it remains outside blocking "
            "CI until its standalone build and output contract are calibrated"
        ),
    }


def apply_inventory(inv: dict, program: str) -> bool:
    """Return True if the inventory changed (add or reclassify)."""
    for entry in inv["files"]:
        if entry.get("path") == program:
            if entry.get("disposition") == "manifest-test":
                return False
            entry.update(inventory_entry(program))
            return True
    # Append only -- the on-disk inventory is not globally sorted, so reordering
    # would produce spurious churn. The lint does not depend on entry order.
    inv["files"].append(inventory_entry(program))
    return True


# --------------------------------------------------------------------------- #
# Source acquisition
# --------------------------------------------------------------------------- #
def _gh(args: list) -> str:
    proc = subprocess.run(
        ["with-proxy", "gh", *args],
        capture_output=True,
        text=True,
    )
    if proc.returncode != 0:
        raise ConvertError(f"gh {' '.join(args)} failed: {proc.stderr.strip()}")
    return proc.stdout


def pr_added_matrix_rows(diff: str) -> list:
    rows, in_matrix = [], False
    for line in diff.splitlines():
        if line.startswith("diff --git"):
            in_matrix = "matrix.tsv" in line
            continue
        if in_matrix and line.startswith("+") and not line.startswith("+++"):
            body = line[1:]
            if body.strip() and not body.startswith("test_name\t"):
                rows.append(body)
    return rows


def pr_added_fixtures(diff: str) -> list:
    fixtures = []
    for line in diff.splitlines():
        if line.startswith("+++ b/") and line.endswith(".c"):
            path = line[len("+++ b/"):]
            if path.startswith("tests/backend-parity/fixtures/") or path.startswith("tests/c/"):
                fixtures.append(path)
    return fixtures


def pr_file_content(pr_head: str, path: str) -> str:
    raw = _gh(
        [
            "api",
            f"repos/{REPO}/contents/{path}?ref={pr_head}",
            "--jq",
            ".content",
        ]
    )
    import base64

    return base64.b64decode(raw).decode("utf-8", "replace")


def plans_from_pr(pr: int) -> tuple:
    """Return (plans, skips) for a PR number."""
    diff = _gh(["pr", "diff", str(pr), "-R", REPO])
    head = _gh(
        ["pr", "view", str(pr), "-R", REPO, "--json", "headRefName", "--jq", ".headRefName"]
    ).strip()
    rows = pr_added_matrix_rows(diff)
    fixtures = {Path(p).stem: p for p in pr_added_fixtures(diff)}
    plans, skips = [], []
    for raw in rows:
        try:
            row = parse_matrix_row(raw)
        except ConvertError as err:
            skips.append(f"#{pr}: {err}")
            continue
        program = fixtures.get(row.name)
        content = None
        if program is None:
            # Fixture already in-tree on main (some rows reuse an existing guest).
            for candidate in (
                f"tests/backend-parity/fixtures/{row.name}.c",
                f"tests/c/{row.name}.c",
            ):
                if (REPO_ROOT / candidate).exists():
                    program = candidate
                    break
            if program is None:
                skips.append(f"#{pr}: no fixture found for row '{row.name}'")
                continue
        else:
            content = pr_file_content(head, program)
        plans.append(build_plan(row, program, content))
    return plans, skips


def plans_from_local(row_str: str, fixture: str) -> tuple:
    row = parse_matrix_row(row_str)
    program = fixture
    content = None
    if not (REPO_ROOT / program).exists():
        raise ConvertError(f"fixture not found in tree: {program}")
    return [build_plan(row, program, content)], []


# --------------------------------------------------------------------------- #
# Emit
# --------------------------------------------------------------------------- #
def run(plans: list, apply: bool) -> int:
    manifest_text = MANIFEST.read_text()
    inv = json.loads(INVENTORY.read_text())
    new_manifest = manifest_text
    changed_fixtures = []
    converted, skipped_existing = [], []

    for plan in plans:
        if manifest_has_id(new_manifest, plan):
            skipped_existing.append(plan.slug)
            continue
        new_manifest = new_manifest.rstrip("\n") + "\n" + render_test_block(plan)
        converted.append(plan)

    inv_after = json.loads(json.dumps(inv))
    inv_changed = False
    for plan in plans:
        if apply_inventory(inv_after, plan.program):
            inv_changed = True
    new_inv_text = json.dumps(inv_after, indent=2) + "\n"

    # Report
    print(f"== retarget: {len(plans)} candidate(s) ==")
    for plan in converted:
        print(
            f"  CONVERT  {plan.name:32s} -> {BUCKET}/{plan.slug}  "
            f"verify.enabled={plan.enabled}  lane={plan.lane}"
        )
    for slug in skipped_existing:
        print(f"  SKIP     already present: {BUCKET}/{slug}")

    if not apply:
        print("\n--- DRY RUN (no files written) ---")
        _print_diff(MANIFEST, manifest_text, new_manifest)
        if inv_changed:
            _print_diff(INVENTORY, INVENTORY.read_text(), new_inv_text)
        for plan in converted:
            if plan.fixture_content is not None and not (REPO_ROOT / plan.program).exists():
                print(f"\n+++ NEW FIXTURE {plan.program} ({len(plan.fixture_content)} bytes)")
        return 0

    if converted:
        MANIFEST.write_text(new_manifest)
    if inv_changed:
        INVENTORY.write_text(new_inv_text)
    for plan in converted:
        dest = REPO_ROOT / plan.program
        if plan.fixture_content is not None and not dest.exists():
            dest.parent.mkdir(parents=True, exist_ok=True)
            dest.write_text(plan.fixture_content)
            changed_fixtures.append(plan.program)
    print(f"\nAPPLIED: {len(converted)} test(s), inventory_changed={inv_changed}, "
          f"fixtures_written={len(changed_fixtures)}")
    return 0


def _print_diff(path: Path, before: str, after: str) -> None:
    rel = path.relative_to(REPO_ROOT)
    diff = difflib.unified_diff(
        before.splitlines(keepends=True),
        after.splitlines(keepends=True),
        fromfile=f"a/{rel}",
        tofile=f"b/{rel}",
    )
    sys.stdout.writelines(diff)
    print()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--pr", type=int, nargs="+", help="source PR number(s)")
    parser.add_argument("--row", help="explicit matrix.tsv data row (local mode)")
    parser.add_argument("--fixture", help="fixture path for --row (local mode)")
    parser.add_argument("--apply", action="store_true", help="write changes (default: dry run)")
    args = parser.parse_args()

    plans, skips = [], []
    if args.pr:
        for pr in args.pr:
            try:
                p, s = plans_from_pr(pr)
                plans += p
                skips += s
            except ConvertError as err:
                skips.append(f"#{pr}: {err}")
    elif args.row and args.fixture:
        try:
            plans, skips = plans_from_local(args.row, args.fixture)
        except ConvertError as err:
            skips.append(str(err))
    else:
        parser.error("provide --pr N [N ...] or --row ROW --fixture PATH")

    rc = run(plans, args.apply) if plans else 0
    if skips:
        print("\n== CANNOT AUTO-CONVERT ==")
        for s in skips:
            print(f"  {s}")
    return rc


if __name__ == "__main__":
    sys.exit(main())
