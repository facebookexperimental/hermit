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
LEGACY_OPERAND_AWARE_23 = (
    CURRENT_20 + ",ref_output_hash,parity_comparator,parity_tier"
)

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


def run_backend_local_dynamic(name: str, candidate_stdout: bytes):
    """Exercise a dynamic row whose raw output is not a parity contract."""
    commands: list[list[str]] = []
    original = run_matrix.run_with_timeout

    def planted_run(command):
        commands.append(command)
        return run_matrix.subprocess.CompletedProcess(
            command, 0, candidate_stdout, b""
        )

    evidence: dict[str, str] = {}
    try:
        run_matrix.run_with_timeout = planted_run
        result = run_matrix.run_case(
            Path("/planted/hermit"),
            "dbt",
            name,
            run_matrix.CatalogFixtures(),
            strict=True,
            evidence=evidence,
        )
    finally:
        run_matrix.run_with_timeout = original
    return result, evidence, commands


result, backend_local, backend_local_commands = run_backend_local_dynamic(
    "anonymous_mmap_layout", b"multiple 0x1000 0x2000 0x3000\n"
)
check(
    "backend-local layout remains a within-backend repeatability contract",
    result[0] == "PASS"
    and len(backend_local_commands) == run_matrix.RUNS
    and all("--backend" in command for command in backend_local_commands),
    repr((result, backend_local_commands)),
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

clock_result, clock_local, clock_commands = run_backend_local_dynamic(
    "virtual_clock", b"clock matrix success\n"
)
check(
    "virtual clock remains a within-backend repeatability contract",
    clock_result[0] == "PASS"
    and len(clock_commands) == run_matrix.RUNS
    and all("--backend" in command for command in clock_commands),
    repr((clock_result, clock_commands)),
)
check(
    "virtual clock performs no ptrace reference or cross-backend verdict",
    not run_matrix.exact_stdout_parity_contract("dbt", "virtual_clock", None)
    and clock_local.get("comparison_tier") == "unqualified-no-comparison"
    and "output_hash" not in clock_local
    and "ref_output_hash" not in clock_local
    and "stdout_parity" not in clock_local,
    repr(clock_local),
)


print("case BACKEND-ARGS — ptrace reference excludes KVM-only guest arguments")
kvm_commands: list[list[str]] = []
original = run_matrix.run_with_timeout


def planted_memory_advice(command):
    kvm_commands.append(command)
    if "--backend" not in command and "--kvm" in command:
        return run_matrix.subprocess.CompletedProcess(
            command,
            14,
            b"",
            b"ptrace fixture rejected KVM-only invocation\n",
        )
    return run_matrix.subprocess.CompletedProcess(command, 0, b"madvise-ok\n", b"")


kvm_evidence: dict[str, str] = {}
try:
    run_matrix.run_with_timeout = planted_memory_advice
    kvm_memory_advice = run_matrix.run_case(
        Path("/planted/hermit"),
        "kvm",
        "memory_advice",
        run_matrix.CatalogFixtures(),
        strict=True,
        evidence=kvm_evidence,
    )
finally:
    run_matrix.run_with_timeout = original
check(
    "KVM memory_advice keeps its fixed-output parity contract",
    kvm_memory_advice[0] == "PASS"
    and kvm_evidence.get("stdout_parity") == "1",
    repr((kvm_memory_advice, kvm_evidence)),
)
check(
    "ptrace reference receives the portable fixture invocation",
    len(kvm_commands) == run_matrix.RUNS + 1
    and "--backend" not in kvm_commands[0]
    and "--kvm" not in kvm_commands[0],
    repr(kvm_commands),
)
check(
    "all KVM candidates retain the required KVM-only fixture argument",
    len(kvm_commands) == run_matrix.RUNS + 1
    and all(
        "--backend" in command and "--kvm" in command
        for command in kvm_commands[1:]
    ),
    repr(kvm_commands),
)


print("case CPUID-BLOCKED — reference capture preserves capability semantics")


def run_cpuid_reference(reference_returncode: int, reference_stderr: bytes):
    commands: list[list[str]] = []

    def planted_cpuid(command):
        commands.append(command)
        if len(commands) == 1:
            return run_matrix.subprocess.CompletedProcess(
                command,
                reference_returncode,
                (
                    b"CPUID-SUCCESS vendor=GenuineIntel signature=00000663\n"
                    if reference_returncode == 0
                    else b""
                ),
                reference_stderr,
            )
        return run_matrix.subprocess.CompletedProcess(
            command,
            0,
            b"CPUID-SUCCESS vendor=GenuineIntel signature=00000663\n",
            b"",
        )

    evidence: dict[str, str] = {}
    original_run = run_matrix.run_with_timeout
    try:
        run_matrix.run_with_timeout = planted_cpuid
        result = run_matrix.run_case(
            Path("/planted/hermit"),
            "ptrace",
            "cpuid_policy",
            run_matrix.CatalogFixtures(),
            strict=True,
            evidence=evidence,
        )
    finally:
        run_matrix.run_with_timeout = original_run
    return result, evidence, commands


for marker in (
    b"continuing without CPUID interception\n",
    b"CPUID faulting is unavailable\n",
):
    blocked, blocked_evidence, cpuid_commands = run_cpuid_reference(14, marker)
    check(
        f"CPUID capability marker remains BLOCKED: {marker.decode().strip()}",
        blocked[0] == "BLOCKED"
        and blocked[1] == "host kernel/CPU lacks CPUID faulting"
        and len(cpuid_commands) == 1
        and "stdout_parity" not in blocked_evidence,
        repr((blocked, blocked_evidence, cpuid_commands)),
    )

generic_failure, _, generic_commands = run_cpuid_reference(
    14, b"unrelated ptrace reference failure\n"
)
check(
    "unrelated ptrace reference failure remains FAIL",
    generic_failure[0] == "FAIL"
    and "ptrace reference exited 14" in generic_failure[1]
    and len(generic_commands) == 1,
    repr((generic_failure, generic_commands)),
)

cpuid_match, cpuid_evidence, cpuid_commands = run_cpuid_reference(
    0, b""
)
check(
    "available CPUID path still executes 1 reference plus 3 candidates",
    cpuid_match[0] == "PASS"
    and len(cpuid_commands) == run_matrix.RUNS + 1
    and cpuid_evidence.get("stdout_parity") == "1"
    and cpuid_evidence.get("output_hash")
    == cpuid_evidence.get("ref_output_hash"),
    repr((cpuid_match, cpuid_evidence, cpuid_commands)),
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

print("case LEGACY-OPERAND-AWARE — measured verdict maps into parity")
path, err = append(LEGACY_OPERAND_AWARE_23)
check("append is accepted", err is None, repr(err))
if err is None:
    got = read_planted(path)
    held = got["planted-dbt-pass"]
    differed = got["planted-dbt-diff"]
    check(
        "legacy parity carries the measured matching verdict",
        held.get("parity") == "1"
        and held.get("output_hash") == held.get("ref_output_hash"),
        repr(held),
    )
    check(
        "legacy parity carries the measured divergent verdict",
        differed.get("parity") == "0"
        and differed.get("output_hash") != differed.get("ref_output_hash"),
        repr(differed),
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
    compat_alias = root / "compat-alias"
    compat_alias.symlink_to(compat, target_is_directory=True)
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
            try:
                run_matrix.record_parent_observations(
                    PLANTED,
                    requested_path=compat_alias / "scorecard.csv",
                    disabled=False,
                    strict=True,
                    verify=False,
                    probe_gaps=False,
                )
                alias_current_refused = False
            except run_matrix.MatrixError as error:
                alias_current_refused = "refusing to append" in str(error)
            alias_observation = compat_alias / "ignored" / "explicit-observation.csv"
            alias_destination = run_matrix.record_parent_observations(
                PLANTED,
                requested_path=alias_observation,
                disabled=False,
                strict=True,
                verify=False,
                probe_gaps=False,
            )
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
        "symlink alias to tracked current is refused without parent discovery",
        alias_current_refused,
    )
    check(
        "symlinked non-current observation remains writable",
        alias_destination == alias_observation
        and alias_observation.resolve().is_file()
        and alias_observation.resolve().parent == compat / "ignored",
        repr(alias_destination),
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
