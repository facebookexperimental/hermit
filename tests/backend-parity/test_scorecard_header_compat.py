#!/usr/bin/env python3
"""Regression test: the outer scorecard's schema is owned by the PARENT.

`run_matrix.py` writes live parity observations to a per-run artifact whose
schema belongs to the dev-hermit parent, and
the parent adds columns without touching Hermit.  The consumer used to demand
exact tuple equality with its own `SCORECARD_HEADER`, so when the parent added
`verify_compare` every Hermit validate that reached `test.dbt_parity` died --
with no Hermit-side change, AFTER running the whole matrix, and with a message
naming a header while every parity cell had actually passed.

Two things are asserted here, and the second is the one that bites quietly:

  1. a wider parent header is ACCEPTED (the reported outage), and
  2. rows are written at the FILE's width, so values stay in their columns.
     Merely relaxing the equality check while still writing the 19-name
     `SCORECARD_HEADER` would append short rows under a 20-column header and
     silently shift every field after `reason`.

Fail-closed is preserved and narrowed: a column this producer WRITES must
exist, and the refusal names it.

Run: python3 tests/backend-parity/test_scorecard_header_compat.py
Exit 0 = all assertions pass, 1 = a real failure.
"""

from __future__ import annotations

import csv
import importlib.util
import os
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("run_matrix", HERE / "run_matrix.py")
assert spec and spec.loader
run_matrix = importlib.util.module_from_spec(spec)
spec.loader.exec_module(run_matrix)

LEGACY_19 = (
    "run_id,run_utc,hermit_sha,reverie_sha,dirty,run_mode,lane,bucket,test_id,"
    "test_mode,backend,cell_state,outcome,deterministic,parity,output_hash,"
    "duration_ms,max_rss_kb,reason"
)
CURRENT_20 = LEGACY_19 + ",verify_compare"
RENAMED_20 = CURRENT_20.replace(",parity,", ",stdout_parity,")
OPERAND_AWARE_23 = RENAMED_20 + ",ref_output_hash,parity_comparator,parity_tier"

# A planted matching cell and a planted divergent cell.  Their parity comes from
# real byte operands, not from the enclosing PASS/FAIL status.  This is the
# mutation bracket: either hard-coding parity=1 or deriving it from status makes
# the divergent assertion below fail.
PLANTED = [
    {
        "result": "PASS",
        "backend": "dbt",
        "test_name": "planted-dbt-pass",
        "expectation": "pass",
        "seconds": "1.0",
        "detail": "planted genuine dbt parity",
        "evidence": run_matrix.stdout_parity_evidence(
            b"matching candidate\n", b"matching candidate\n"
        ),
    },
    {
        "result": "FAIL",
        "backend": "dbt",
        "test_name": "planted-dbt-diff",
        "expectation": "pass",
        "seconds": "2.0",
        "detail": "planted genuine dbt divergence",
        "evidence": run_matrix.stdout_parity_evidence(
            b"divergent candidate\n", b"ptrace reference\n"
        ),
    },
]

FAILURES: list[str] = []


def check(label: str, ok: bool, detail: str = "") -> None:
    if ok:
        print(f"  \033[32mok\033[0m    {label}")
    else:
        print(f"  \033[31mFAIL\033[0m  {label}{(' -- ' + detail) if detail else ''}")
        FAILURES.append(label)


def append(header: str | None, *, seed_row: str | None = None) -> tuple[Path, object]:
    """Write a scorecard with `header`, append the planted rows, return (path, err)."""
    tmp = Path(tempfile.mkdtemp(prefix="scorecard-compat-"))
    path = tmp / "scorecard.csv"
    if header is not None:
        body = header + "\n" + (seed_row + "\n" if seed_row else "")
        path.write_text(body, encoding="utf-8")
    err = None
    try:
        run_matrix.append_parent_scorecard(
            path,
            [dict(r) for r in PLANTED],
            strict=True,
            verify=True,
            probe_gaps=False,
        )
    except Exception as exc:  # noqa: BLE001 - the refusal is the thing under test
        err = exc
    return path, err


def read_planted(path: Path) -> dict[str, dict[str, str]]:
    with path.open(newline="", encoding="utf-8") as fh:
        rows = list(csv.DictReader(fh))
    return {r["test_id"].split("/")[-1]: r for r in rows if r.get("test_id")}


def parity_of(row: dict[str, str]) -> str | None:
    # getattr, not attribute access: this test must also be runnable against the
    # PRE-FIX run_matrix.py (which has no PARITY_COLUMNS) so the not-inert
    # comparison is a like-for-like run rather than an import error.
    for name in getattr(run_matrix, "PARITY_COLUMNS", ("parity", "stdout_parity")):
        if name in row and row[name] is not None:
            return row[name]
    return None


print("case OPERANDS — verdict is derived from real bytes in both directions")
matching = run_matrix.stdout_parity_evidence(b"same\n", b"same\n")
check("matching bytes populate parity=1", matching.get("stdout_parity") == "1", repr(matching))
check(
    "matching bytes populate two equal SHA-256 operands",
    len(matching.get("output_hash", "")) == 64
    and matching.get("output_hash") == matching.get("ref_output_hash"),
    repr(matching),
)
divergent = run_matrix.stdout_parity_evidence(b"candidate\n", b"reference\n")
check("divergent bytes populate parity=0", divergent.get("stdout_parity") == "0", repr(divergent))
check(
    "divergent bytes populate two unequal SHA-256 operands",
    len(divergent.get("output_hash", "")) == 64
    and len(divergent.get("ref_output_hash", "")) == 64
    and divergent.get("output_hash") != divergent.get("ref_output_hash"),
    repr(divergent),
)
empty = run_matrix.stdout_parity_evidence(b"", b"")
check(
    "empty stdout is a measured matching operand, not missing",
    empty.get("stdout_parity") == "1"
    and empty.get("output_hash")
    == "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    and empty.get("ref_output_hash") == empty.get("output_hash"),
    repr(empty),
)
missing = run_matrix.stdout_parity_evidence(b"candidate\n", None)
check(
    "missing reference stays unmeasured",
    len(missing.get("output_hash", "")) == 64
    and "ref_output_hash" not in missing
    and "stdout_parity" not in missing,
    repr(missing),
)


print("case PRODUCER-PATH — live run results drive both sides of the mutation")


def run_producer(candidate_stdout: bytes):
    responses = [
        run_matrix.subprocess.CompletedProcess([], 0, b"hello world\n", b""),
        run_matrix.subprocess.CompletedProcess([], 0, candidate_stdout, b""),
    ]
    if candidate_stdout == b"hello world\n":
        responses.extend(
            run_matrix.subprocess.CompletedProcess([], 0, candidate_stdout, b"")
            for _ in range(run_matrix.RUNS - 1)
        )
    original = run_matrix.run_with_timeout

    def planted_run(_command):
        return responses.pop(0)

    evidence: dict[str, str] = {}
    try:
        run_matrix.run_with_timeout = planted_run
        result = run_matrix.run_case(
            Path("/planted/hermit"),
            "dbt",
            "hello_stdout",
            run_matrix.CatalogFixtures(),
            strict=True,
            evidence=evidence,
        )
    finally:
        run_matrix.run_with_timeout = original
    return result, evidence, responses


result, producer_match, remaining = run_producer(b"hello world\n")
check("matching producer cell passes", result[0] == "PASS", repr(result))
check(
    "matching producer cell emits equal operands and parity=1",
    producer_match.get("stdout_parity") == "1"
    and producer_match.get("output_hash") == producer_match.get("ref_output_hash"),
    repr(producer_match),
)
check("matching producer consumed 1 reference + 3 candidate runs", not remaining, repr(remaining))

result, producer_diff, remaining = run_producer(b"not the reference\n")
check("divergent producer cell fails", result[0] == "FAIL", repr(result))
check(
    "divergent producer cell emits unequal operands and parity=0",
    producer_diff.get("stdout_parity") == "0"
    and producer_diff.get("output_hash") != producer_diff.get("ref_output_hash"),
    repr(producer_diff),
)
check("divergent producer consumed 1 reference + 1 candidate run", not remaining, repr(remaining))


def run_dynamic_producer(reference_stdout: bytes, candidate_stdout: bytes):
    """Exercise a marker-only case whose output is not fixed by the catalog."""
    responses = [
        run_matrix.subprocess.CompletedProcess([], 0, reference_stdout, b""),
        *(
            run_matrix.subprocess.CompletedProcess([], 0, candidate_stdout, b"")
            for _ in range(run_matrix.RUNS)
        ),
    ]
    original = run_matrix.run_with_timeout

    def planted_run(_command):
        return responses.pop(0)

    evidence: dict[str, str] = {}
    try:
        run_matrix.run_with_timeout = planted_run
        result = run_matrix.run_case(
            Path("/planted/hermit"),
            "dbt",
            "virtual_pid",
            run_matrix.CatalogFixtures(),
            strict=True,
            evidence=evidence,
        )
    finally:
        run_matrix.run_with_timeout = original
    return result, evidence, responses


result, dynamic_diff, remaining = run_dynamic_producer(b"pid=111\n", b"pid=222\n")
check(
    "dynamic-output divergence is RED even when all candidate runs are stable",
    result[0] == "FAIL"
    and result[1] == "run 1 stdout differed from ptrace reference",
    repr(result),
)
check(
    "dynamic-output divergence emits unequal operands and parity=0",
    dynamic_diff.get("stdout_parity") == "0"
    and dynamic_diff.get("output_hash") != dynamic_diff.get("ref_output_hash"),
    repr(dynamic_diff),
)
check(
    "dynamic-output divergence stops after 1 reference + 1 candidate run",
    len(remaining) == run_matrix.RUNS - 1,
    repr(remaining),
)


def run_backend_local_layout(candidate_stdout: bytes):
    """Exercise a dynamic row whose raw addresses are not a parity contract."""
    responses = [
        run_matrix.subprocess.CompletedProcess([], 0, candidate_stdout, b"")
        for _ in range(run_matrix.RUNS)
    ]
    original = run_matrix.run_with_timeout

    def planted_run(_command):
        return responses.pop(0)

    evidence: dict[str, str] = {}
    try:
        run_matrix.run_with_timeout = planted_run
        result = run_matrix.run_case(
            Path("/planted/hermit"),
            "dbt",
            "anonymous_mmap_layout",
            run_matrix.CatalogFixtures(),
            strict=True,
            evidence=evidence,
        )
    finally:
        run_matrix.run_with_timeout = original
    return result, evidence, responses


result, backend_local, remaining = run_backend_local_layout(
    b"multiple 0x1000 0x2000 0x3000\n"
)
check(
    "backend-local layout remains a within-backend repeatability contract",
    result[0] == "PASS" and not remaining,
    repr((result, remaining)),
)
check(
    "backend-local layout does not invent cross-backend operands",
    backend_local.get("comparison_tier") == "unqualified-no-comparison"
    and "output_hash" not in backend_local
    and "ref_output_hash" not in backend_local
    and "stdout_parity" not in backend_local,
    repr(backend_local),
)
check(
    "named dynamic virtual PID remains exact while layout addresses do not",
    run_matrix.exact_stdout_parity_contract("dbt", "virtual_pid", None)
    and not run_matrix.exact_stdout_parity_contract(
        "dbt", "anonymous_mmap_layout", None
    ),
)

original = run_matrix.run_with_timeout
try:
    run_matrix.run_with_timeout = lambda _command: None
    missing_reference = run_matrix.run_case(
        Path("/planted/hermit"),
        "dbt",
        "hello_stdout",
        run_matrix.CatalogFixtures(),
        strict=True,
        evidence={},
    )
    preserved = run_matrix.run_case(
        Path("/planted/hermit"),
        "dbt",
        "random_sources",
        run_matrix.CatalogFixtures(),
        strict=True,
    )
finally:
    run_matrix.run_with_timeout = original
check(
    "a requested stdout comparison with no reference is RED",
    missing_reference[0] == "FAIL"
    and missing_reference[1] == "ptrace reference timed out",
    repr(missing_reference),
)
check(
    "DBT random_sources still requires its pre-existing ptrace reference "
    "regardless of artifact routing",
    preserved[0] == "FAIL" and preserved[1] == "ptrace reference timed out",
    repr(preserved),
)


print("case CURRENT-20 — parent added verify_compare (THE REPORTED OUTAGE)")
path, err = append(CURRENT_20)
check("append is accepted, not refused", err is None, repr(err))
if err is None:
    got = read_planted(path)
    check("planted dbt PASS reads outcome=pass", got["planted-dbt-pass"]["outcome"] == "pass")
    check(
        "legacy row with no reference column withholds PASS parity",
        parity_of(got["planted-dbt-pass"]) == "",
    )
    check("planted dbt FAIL reads outcome=fail", got["planted-dbt-diff"]["outcome"] == "fail")
    check(
        "legacy row with no reference column withholds FAIL parity",
        parity_of(got["planted-dbt-diff"]) == "",
    )
    check("backend column says dbt", got["planted-dbt-pass"]["backend"] == "dbt")
    # Alignment: the latent short-write bug.
    widths = {
        len(r) for r in csv.reader(path.read_text(encoding="utf-8").splitlines()) if r
    }
    check("every row is 20 fields wide (no short write)", widths == {20}, str(widths))
    check(
        "reason did not shift into verify_compare",
        got["planted-dbt-pass"]["verify_compare"] == "",
        repr(got["planted-dbt-pass"].get("verify_compare")),
    )
    check(
        "reason still holds the detail",
        "planted genuine dbt parity" in got["planted-dbt-pass"]["reason"],
    )

print("case LEGACY-19 — a parent file predating verify_compare still works")
path, err = append(LEGACY_19)
check("append is accepted", err is None, repr(err))
if err is None:
    got = read_planted(path)
    check("PASS still reads pass", got["planted-dbt-pass"]["outcome"] == "pass")
    check("FAIL still reads fail", got["planted-dbt-diff"]["outcome"] == "fail")
    widths = {
        len(r) for r in csv.reader(path.read_text(encoding="utf-8").splitlines()) if r
    }
    check("rows are 19 fields wide", widths == {19}, str(widths))

print("case RENAMED — forward-compat with parity -> stdout_parity")
path, err = append(RENAMED_20)
check("append is accepted", err is None, repr(err))
if err is None:
    got = read_planted(path)
    check(
        "missing operand column keeps PASS unmeasured",
        got["planted-dbt-pass"].get("stdout_parity") == "",
    )
    check(
        "missing operand column keeps FAIL unmeasured",
        got["planted-dbt-diff"].get("stdout_parity") == "",
    )

print("case OPERAND-AWARE — both hashes and their derived verdict survive the row")
path, err = append(OPERAND_AWARE_23)
check("append is accepted", err is None, repr(err))
if err is None:
    got = read_planted(path)
    held = got["planted-dbt-pass"]
    differed = got["planted-dbt-diff"]
    check("matching row carries parity=1", parity_of(held) == "1", repr(held))
    check(
        "matching row verdict re-derives from equal operands",
        len(held["output_hash"]) == 64
        and held["output_hash"] == held["ref_output_hash"],
        repr(held),
    )
    check("divergent row carries parity=0", parity_of(differed) == "0", repr(differed))
    check(
        "divergent row verdict re-derives from unequal operands",
        len(differed["output_hash"]) == 64
        and len(differed["ref_output_hash"]) == 64
        and differed["output_hash"] != differed["ref_output_hash"],
        repr(differed),
    )
    check(
        "comparison contract travels with both measured rows",
        all(row["parity_comparator"] == "stdout-sha256-exact-v1" for row in (held, differed))
        and all(row["parity_tier"] == "stdout-exact" for row in (held, differed)),
    )

print("case PRESERVE — an existing parent row keeps its verify_compare value")
seed = (
    "prior-run,@1,abc,unknown,false,regression,portable,backend-parity,"
    "backend-parity/prior,verify,ptrace,enabled,pass,1,1,,10,,prior detail,BITWISE"
)
path, err = append(CURRENT_20, seed_row=seed)
check("append is accepted", err is None, repr(err))
if err is None:
    with path.open(newline="", encoding="utf-8") as fh:
        rows = list(csv.DictReader(fh))
    prior = next(r for r in rows if r["run_id"] == "prior-run")
    check("pre-existing verify_compare survives", prior["verify_compare"] == "BITWISE")

print("case REFUSAL — a column this producer WRITES is missing (fail-closed)")
path, err = append(CURRENT_20.replace(",outcome,", ","))
check("refused", isinstance(err, run_matrix.MatrixError), repr(err))
check("names the missing column", "outcome" in str(err), str(err))
check("carries the header size (#319)", "column(s):" in str(err), str(err))

print("case FRESH — an absent file is created at the canonical schema")
path, err = append(None)
check("append is accepted", err is None, repr(err))
if err is None:
    hdr = path.read_text(encoding="utf-8").splitlines()[0]
    # The canonical schema grew from 20 to 23 when the tier-evidence columns
    # landed: a bare `deterministic=1` cannot say WHICH comparison earned it, so
    # the verdict now travels with its strictness, its parity boolean and the
    # counts that make the boolean falsifiable.
    check(
        "created header carries the tier-evidence columns",
        hdr.endswith(",verify_compare,bitwise_parity,compared_log_messages,tier"),
        hdr,
    )
    check("created header is 29 columns", len(hdr.split(",")) == 29, hdr)
    check(
        "fresh schema uses the parent publisher's canonical stdout_parity name",
        "stdout_parity" in hdr.split(",") and "parity" not in hdr.split(","),
        hdr,
    )

print("case ROUTING — validation writes an ignored observation, never current scorecard")
with tempfile.TemporaryDirectory(prefix="scorecard-routing-") as td:
    root = Path(td)
    compat = root / "compat-envelope"
    compat.mkdir()
    current = compat / "scorecard.csv"
    current.write_text("sentinel-current-view\n", encoding="utf-8")
    old_root = os.environ.get("DEV_HERMIT_ROOT")
    os.environ["DEV_HERMIT_ROOT"] = str(root)
    try:
        destination = run_matrix.record_parent_observations(
            PLANTED,
            requested_path=None,
            disabled=False,
            strict=True,
            verify=False,
            probe_gaps=False,
        )
        try:
            run_matrix.record_parent_observations(
                PLANTED,
                requested_path=current,
                disabled=False,
                strict=True,
                verify=False,
                probe_gaps=False,
            )
            canonical_refused = False
        except run_matrix.MatrixError as error:
            canonical_refused = "refusing to append" in str(error)
        original_discover = run_matrix.discover_compat_envelope
        run_matrix.discover_compat_envelope = lambda: None
        try:
            try:
                run_matrix.record_parent_observations(
                    PLANTED,
                    requested_path=current,
                    disabled=False,
                    strict=True,
                    verify=False,
                    probe_gaps=False,
                )
                unavailable_parent_refused = False
            except run_matrix.MatrixError as error:
                unavailable_parent_refused = "refusing to append" in str(error)
        finally:
            run_matrix.discover_compat_envelope = original_discover
        disabled_destination = run_matrix.record_parent_observations(
            PLANTED,
            requested_path=current,
            disabled=True,
            strict=True,
            verify=False,
            probe_gaps=False,
        )
    finally:
        if old_root is None:
            os.environ.pop("DEV_HERMIT_ROOT", None)
        else:
            os.environ["DEV_HERMIT_ROOT"] = old_root
    check("default observation was written", destination is not None and destination.is_file())
    check(
        "default observation is under the ignored per-run directory",
        destination is not None
        and destination.parent == compat / "ignored" / "backend-parity",
        str(destination),
    )
    check("tracked current view stayed byte-identical", current.read_text() == "sentinel-current-view\n")
    check("direct append to tracked current view is refused", canonical_refused)
    check(
        "tracked current view is refused even when parent discovery is unavailable",
        unavailable_parent_refused,
    )
    check(
        "disabled observation routing is side-effect-only",
        disabled_destination is None
        and current.read_text() == "sentinel-current-view\n",
    )
    if destination is not None:
        with destination.open(newline="", encoding="utf-8") as handle:
            reader = csv.DictReader(handle)
            routed = list(reader)
            routed_header = reader.fieldnames or []
        check(
            "default observation header is directly publisher-compatible",
            "stdout_parity" in routed_header and "parity" not in routed_header,
            repr(routed_header),
        )
        check(
            "new rows without L3 flags record false, not historical blank",
            all(row["stack_parity"] == "0" and row["heap_parity"] == "0" for row in routed),
        )

print()
if FAILURES:
    print(f"FAIL ({len(FAILURES)} assertions)")
    sys.exit(1)
print("PASS")
