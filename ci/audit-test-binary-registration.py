#!/usr/bin/env python3
"""Fail closed when a Hermit integration-test binary is absent from CI accounting.

Cargo discovers every top-level ``hermit-cli/tests/*.rs`` file as a test binary.
Hermit's CI DAG records the binaries a step executes in its shared typed
``integration_test_binaries`` field. A binary present in the former set but absent
from both the DAG and the declarations ledger would otherwise be invisible:
neither executed nor reported as not run.

The present set deliberately comes from ``git ls-files`` rather than the ledger.
The ledger therefore cannot certify its own completeness.  Nested helper modules
such as ``hermit-cli/tests/common/mod.rs`` are not Cargo test binaries and are
excluded by the explicit path-depth check.

``none-recorded`` is a first-class honest-unknown state.  It means only that no
reason for omitting the binary was recorded; it is never counted as CI coverage
or as a reason-recorded declaration.

The command parser below is not the source of registration. It verifies that a
typed declaration matches what the in-repository command executes, so a stale
declaration cannot claim coverage and an old DAG with no declaration cannot turn
missing evidence into a zero or a success.
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path


DEFAULT_ROOT = Path(__file__).resolve().parents[1]
AGENT_UTILS_PY = DEFAULT_ROOT / "agent-utils/py"
sys.path.insert(0, str(AGENT_UTILS_PY))

from dagrun import DagJsonError, dag_from_json  # noqa: E402


DECLARATIONS = Path("ci/undeclared-test-binaries.tsv")
DAG_GLOB = "ci/dag/*.json"

# Restrict registration to the hermit-cli package.  A similarly named target in
# hermit-detcore is not evidence that the hermit-cli binary ran.
_HERMIT_INVOCATION_RE = re.compile(
    r"(?:cargo (?:test|nextest run)|\./ci/run-nextest-counted\.sh)"
    r"[^\"\n]*?-p hermit(?!-)[^\"\n]*"
)
_TEST_FLAG_RE = re.compile(r"--test(?:=|\s+)([A-Za-z0-9_]+)")
_TOP_LEVEL_TEST_RE = re.compile(r"^hermit-cli/tests/([^/]+)\.rs$")

# REGISTRATION MEANS "CI EXECUTES IT", NOT "THE TEXT APPEARS SOMEWHERE".
#
# The audit used to regex the whole DAG document, so any string that merely looked
# like an invocation registered a binary: a `desc` field mentioning one, or the
# literal command `echo cargo test -p hermit --test zz_probe`, both counted. A
# binary excused by text that never runs is exactly the blindness this file exists
# to remove, one level up.
#
# So: read only each step's `cmd`, split it into shell command segments, and accept
# the invocation only when everything preceding `cargo` in its segment is a thing
# that still leads to cargo running -- an environment assignment or a real wrapper
# program. Hermit's DAG legitimately uses both (`CARGO_BUILD_JOBS=8 ...`,
# `./ci/run-with-reverie-dbt-budget.sh cargo test ...`), so a bare command-position
# rule would reject the real registrations. `echo` and `printf` match nothing in
# the allowlist and are refused.
_SEGMENT_SPLIT_RE = re.compile(r"&&|\|\||[;|\n]")
_ENV_ASSIGNMENT_RE = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*=")
_KNOWN_RUNNERS = frozenset({"timeout", "env", "nice", "nohup", "xargs", "exec", "command"})
_PRLIMIT_FSIZE_RE = re.compile(r"--fsize=(\d+):(\d+)")
_DURATION_RE = re.compile(r"^\d+[smhd]?$")
# `--no-run` compiles the binary and never executes it, so it is not coverage.
_NO_RUN_RE = re.compile(r"(?<![\w-])--no-run(?![\w-])")


def _prefix_still_runs_cargo(prefix: str) -> bool:
    """Do the tokens before `cargo` leave cargo actually being executed?"""
    tokens = prefix.split()
    position = 0
    while position < len(tokens):
        token = tokens[position]
        position += 1
        if token == "prlimit":
            # Recognize the limit-setting/exec form used by the DAG. Options
            # such as --help, --version and --pid do not execute the runner.
            if position + 1 >= len(tokens):
                return False
            limits = _PRLIMIT_FSIZE_RE.fullmatch(tokens[position])
            if (
                limits is None
                or int(limits[1]) > int(limits[2])
                or tokens[position + 1] != "--"
            ):
                return False
            position += 2
            # After --, prlimit expects an executable, not shell assignments,
            # shell builtins or another option. The matched cargo/runner may
            # immediately follow the prefix, leaving no tokens here.
            if position < len(tokens) and (
                tokens[position].startswith("-")
                or _ENV_ASSIGNMENT_RE.match(tokens[position])
                or tokens[position] in {"exec", "command"}
            ):
                return False
            continue
        if _ENV_ASSIGNMENT_RE.match(token):
            continue
        if "/" in token or token.endswith(".sh"):
            continue
        if token in _KNOWN_RUNNERS or token.startswith("-") or _DURATION_RE.match(token):
            continue
        return False
    return True


def executed_test_targets(command: str) -> set[str]:
    """`--test` targets of hermit-cli invocations this command really executes."""
    found: set[str] = set()
    for segment in _SEGMENT_SPLIT_RE.split(command):
        match = _HERMIT_INVOCATION_RE.search(segment)
        if match is None:
            continue
        if not _prefix_still_runs_cargo(segment[: match.start()]):
            continue
        invocation = match.group(0)
        if _NO_RUN_RE.search(invocation):
            continue
        found.update(_TEST_FLAG_RE.findall(invocation))
    return found


@dataclass(frozen=True)
class Declaration:
    disposition: str
    reason: str


def present_targets(root: Path) -> set[str]:
    """Return tracked top-level Cargo integration-test targets."""
    listed = subprocess.run(
        ["git", "-C", str(root), "ls-files", "--", "hermit-cli/tests/"],
        capture_output=True,
        text=True,
        check=False,
    )
    if listed.returncode != 0:
        raise ValueError(
            "git ls-files failed: " + (listed.stderr.strip() or "unknown error")[:200]
        )

    names = {
        match.group(1)
        for raw in listed.stdout.splitlines()
        if (match := _TOP_LEVEL_TEST_RE.fullmatch(raw.strip())) is not None
    }
    if not names:
        raise ValueError(
            "found NO tracked top-level hermit-cli test targets; refusing a vacuous audit"
        )
    return names


def registered_targets(root: Path) -> set[str]:
    """Return typed test targets whose DAG commands execute the same targets."""
    dag_paths = sorted(root.glob(DAG_GLOB))
    if not dag_paths:
        raise ValueError(f"found no DAG files matching {DAG_GLOB}")

    registered: set[str] = set()
    for path in dag_paths:
        try:
            config = dag_from_json(path.read_text())
        except (OSError, DagJsonError) as error:
            raise ValueError(f"cannot parse {path.relative_to(root)}: {error}") from error
        for step in config.steps:
            executed = executed_test_targets(step.cmd)
            declared = step.integration_test_binaries
            if declared is None:
                if executed:
                    raise ValueError(
                        f"{path.relative_to(root)} step {step.tag} executes "
                        f"{sorted(executed)!r} but omits integration_test_binaries"
                    )
                continue
            declared_set = set(declared)
            if declared_set != executed:
                raise ValueError(
                    f"{path.relative_to(root)} step {step.tag} integration_test_binaries "
                    f"{sorted(declared_set)!r} do not match executed targets "
                    f"{sorted(executed)!r}"
                )
            registered.update(declared)
    return registered


def declarations(root: Path) -> dict[str, Declaration]:
    """Return the versioned omission ledger, rejecting ambiguous rows."""
    path = root / DECLARATIONS
    if not path.is_file():
        return {}

    rows: dict[str, Declaration] = {}
    for lineno, raw in enumerate(path.read_text().splitlines(), 1):
        if not raw.strip() or raw.lstrip().startswith("#"):
            continue
        parts = raw.split("\t")
        if len(parts) != 3 or any(not part.strip() for part in parts):
            raise ValueError(
                f"{DECLARATIONS}:{lineno} must be "
                "<binary>\\t<disposition>\\t<nonempty reason>"
            )
        name, disposition, reason = (part.strip() for part in parts)
        if name in rows:
            raise ValueError(f"{DECLARATIONS}:{lineno} duplicates binary {name!r}")
        rows[name] = Declaration(disposition=disposition, reason=reason)
    return rows


def audit(root: Path, *, json_output: bool = False) -> int:
    try:
        present = present_targets(root)
        registered = registered_targets(root)
        rows = declarations(root)
    except ValueError as error:
        print(f"audit-test-binary-registration: REFUSED: {error}", file=sys.stderr)
        return 2

    ci_registered = present & registered
    declared_not_run = present & set(rows) - registered
    none_recorded = {
        name for name in declared_not_run if rows[name].disposition == "none-recorded"
    }
    reason_recorded = declared_not_run - none_recorded
    undeclared = present - registered - set(rows)

    # This equality is the accounting invariant.  Keep the unknown state
    # separate rather than folding it into either satisfied or failed.
    accounted = ci_registered | reason_recorded | none_recorded | undeclared
    if accounted != present:
        print(
            "audit-test-binary-registration: REFUSED: internal accounting partition "
            "does not cover the present set",
            file=sys.stderr,
        )
        return 2

    if json_output:
        print(
            json.dumps(
                {
                    "schema": 1,
                    "present": sorted(present),
                    "ci_registered": sorted(ci_registered),
                    "reason_recorded": [
                        {
                            "binary": name,
                            "disposition": rows[name].disposition,
                            "reason": rows[name].reason,
                        }
                        for name in sorted(reason_recorded)
                    ],
                    "none_recorded": sorted(none_recorded),
                    "undeclared": sorted(undeclared),
                },
                sort_keys=True,
                separators=(",", ":"),
            )
        )
        return 2 if undeclared else 0

    print(
        "test-binary registration: "
        f"present={len(present)} "
        f"ci-registered={len(ci_registered)} "
        f"reason-recorded={len(reason_recorded)} "
        f"none-recorded={len(none_recorded)} "
        f"undeclared={len(undeclared)}"
    )

    stale = sorted(set(rows) - present)
    superseded = sorted(set(rows) & registered)
    for name in stale:
        print(f"  NOTE: stale declaration has no tracked test binary: {name}")
    for name in superseded:
        print(f"  NOTE: declaration is superseded by CI registration: {name}")

    if undeclared:
        print(
            f"FAIL: {len(undeclared)} tracked hermit-cli test binar"
            f"{'y is' if len(undeclared) == 1 else 'ies are'} absent from both CI "
            "registration and the declarations ledger:",
            file=sys.stderr,
        )
        for name in sorted(undeclared):
            print(f"    hermit-cli/tests/{name}.rs", file=sys.stderr)
        print(
            "Either register each binary with `./ci/run-nextest-counted.sh -p hermit --test <name>` "
            "in ci/dag/*.json, or add a ledger row naming why it is not run. "
            "Use `none-recorded` only for an honest unknown; it is debt, not approval.",
            file=sys.stderr,
        )
        return 2

    if none_recorded:
        print(
            "ACCOUNTED-WITH-UNKNOWN: inventory is complete, but "
            f"{len(none_recorded)} test binaries remain `none-recorded`; they are "
            "NOT counted as CI-covered or reason-recorded"
        )
    else:
        print(
            "ACCOUNTING-COMPLETE: every tracked test binary is CI-registered or has "
            "a recorded omission reason"
        )
    return 0


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--root",
        type=Path,
        default=DEFAULT_ROOT,
        help="repository root (used by isolated mutation tests)",
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help="emit the complete registration partition as one JSON object",
    )
    return parser.parse_args()


if __name__ == "__main__":
    arguments = parse_args()
    raise SystemExit(audit(arguments.root.resolve(), json_output=arguments.json))
