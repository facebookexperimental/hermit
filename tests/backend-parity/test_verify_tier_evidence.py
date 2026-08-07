#!/usr/bin/env python3
"""Bracket the tier a `--verify` run is allowed to claim.

The bug this guards: `run_matrix.py` decided the assurance kind by scraping
hermit's stderr for `"Determinism verified"` and then labelled the row
`"L2 DETLOG-bitwise"`.  That banner is printed by a plain `--verify` run whose
own `--verify-json` reports `bitwise_parity: false`, so the label asserted
bitwise identity for a comparison that had merely normalised-and-compared.
Mutation testing measured the consequence: 3 of 5 planted defects (a differing
read() return length, a differing pointer argument, a differing openat path) pass
that comparison undetected.

So the acceptance rule under test is narrow and one-directional: `bitwise` is
claimable ONLY from a typed verdict that says `bitwise_parity` AND carries a
nonzero compared-message count on both sides.  Everything else must degrade to
`stripped`, `guest` or `gap` -- never upward.

Both sides are bracketed: each positive plants a record that MUST reach its tier,
and each negative plants a record that MUST NOT reach `bitwise`.
"""

from __future__ import annotations

import json
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from run_matrix import (  # noqa: E402
    EVIDENCE_COLUMNS,
    L2_RANK,
    SCORECARD_HEADER,
    expectation,
    verify_tier_from_json,
)

FAILURES: list[str] = []


def check(label: str, condition: bool, detail: str = "") -> None:
    if condition:
        print(f"  \033[32mok\033[0m    {label}")
    else:
        FAILURES.append(label)
        print(f"  \033[31mFAIL\033[0m  {label}" + (f" -- {detail}" if detail else ""))


def tier_of(record) -> dict[str, str] | None:
    with tempfile.TemporaryDirectory(prefix="verify-tier-") as tmp:
        path = Path(tmp) / "verdict.json"
        if record is not None:
            path.write_text(json.dumps(record), encoding="utf-8")
            return verify_tier_from_json(path)
        return verify_tier_from_json(path)


def spec(strictness, compare_logs=True, **over):
    base = {
        "strictness": strictness,
        "compare_logs": compare_logs,
        "strip_lines": strictness == "stripped",
        "full_trace": strictness == "canonical",
        "canonicalize_addresses": strictness == "canonical",
        "exact_remainder": strictness == "canonical",
    }
    base.update(over)
    return base


def record(verified=True, bitwise=False, left=239, right=239, strictness="stripped",
           verdict="matched", compare_logs=True):
    counts = None if left is None else {"left": left, "right": right}
    return {
        "verified": verified,
        "bitwise_parity": bitwise,
        "verdict": verdict,
        "comparison": spec(strictness, compare_logs),
        "compared_log_messages": counts,
        "guest_exit_code": 0,
        "guest_signal": None,
    }


# --------------------------------------------------------------------------
print("case STRIPPED — the exact shape the scorecard producer emits today")
# Verbatim from a live probe run: rc=0, banner ":: Success: deterministic.
# Determinism verified.", and bitwise_parity false in the same record.
got = tier_of(record(bitwise=False, strictness="stripped"))
check("tier is 'stripped', NOT 'bitwise'", got and got["tier"] == "stripped", repr(got))
check("bitwise_parity records 0", got and got["bitwise_parity"] == "0", repr(got))
check("strictness is carried", got and got["verify_compare"] == "stripped", repr(got))
check("counts travel with the verdict (#319)",
      got and got["compared_log_messages"] == "239|239", repr(got))

print("case BITWISE — a genuine canonical match may claim the top tier")
got = tier_of(record(bitwise=True, strictness="canonical", left=348, right=348))
check("tier is 'bitwise'", got and got["tier"] == "bitwise", repr(got))
check("bitwise_parity records 1", got and got["bitwise_parity"] == "1", repr(got))

print("case VACUOUS — bitwise_parity with a ZERO compared count is NOT bitwise")
# Two empty selections 'match' under the strictest possible spec.  Without the
# count conjunct a run that produced no DETLOG at all would certify as parity.
for left, right, why in ((0, 0, "0|0"), (0, 239, "left 0"), (239, 0, "right 0")):
    got = tier_of(record(bitwise=True, strictness="canonical", left=left, right=right))
    check(f"zero-count record ({why}) is refused the bitwise tier",
          got and got["tier"] != "bitwise", repr(got))
    check(f"zero-count record ({why}) reports bitwise_parity 0",
          got and got["bitwise_parity"] == "0", repr(got))

print("case GUEST — verified without comparing the log stream is guest-visible")
got = tier_of(record(bitwise=False, compare_logs=False, left=None))
check("tier is 'guest'", got and got["tier"] == "guest", repr(got))

print("case DIVERGED — an unverified record never claims a positive tier")
got = tier_of(record(verified=False, verdict="diverged"))
check("tier is 'gap'", got and got["tier"] == "gap", repr(got))

print("case NO-RECORD — absent / no_result / malformed fall back, never upward")
check("absent file yields None", tier_of(None) is None)
check("no_result yields None",
      tier_of({"verdict": "no_result", "verified": False}) is None)
with tempfile.TemporaryDirectory(prefix="verify-tier-") as tmp:
    bad = Path(tmp) / "verdict.json"
    bad.write_text("not json{", encoding="utf-8")
    check("malformed JSON yields None", verify_tier_from_json(bad) is None)

print("case RANK — the ladder orders the tiers and 'bitwise' is the ceiling")
check("guest < stripped < bitwise",
      L2_RANK["guest"] < L2_RANK["stripped"] < L2_RANK["bitwise"], repr(L2_RANK))
check("'detlog' is no longer a tier name", "detlog" not in L2_RANK, repr(L2_RANK))

print("case CONTRACT — today's contracts demand 'stripped', not 'bitwise'")
# Asserting bitwise before an INFO-tier comparator exists would red every
# ptrace/DBT cell for a comparator limitation, not a guest defect.
check("ptrace verify contract is 'stripped'",
      expectation("ptrace", "exit_status", True)[0] == "stripped")
# `exit_status` is a declared dbt L2 gap, so it would report "gap" regardless of
# tiering; use a case dbt is actually contracted for.
check("dbt verify contract is 'stripped'",
      expectation("dbt", "hello_stdout", True)[0] == "stripped")
check("a declared dbt L2 gap still reports 'gap'",
      expectation("dbt", "exit_status", True)[0] == "gap")
check("kvm verify contract stays 'guest'",
      expectation("kvm", "exit_status", True)[0] == "guest")

print("case FALLBACK — a run with no typed verdict must NOT issue a determinism positive")
# DBT accepts --verify-json and writes nothing (measured: rc=0, no file). The old
# behaviour emitted deterministic=1 beside a blank comparator and blank counts --
# a positive whose required fields are empty, which a wired verifier must refuse.
# Producing rows designed to be refused is not a contract, so the row is published
# UNMEASURED instead.
import tempfile as _tf, csv as _csv  # noqa: E402
from run_matrix import (  # noqa: E402
    VERIFY_COMPARE_UNAVAILABLE, BITWISE_CAPABLE_COMPARATORS, append_parent_scorecard,
)


def emitted_row(evidence):
    with _tf.TemporaryDirectory(prefix="fallback-") as tmp:
        path = Path(tmp) / "sc.csv"
        path.write_text(",".join(SCORECARD_HEADER) + "\n", encoding="utf-8")
        append_parent_scorecard(
            path,
            [{"test_name": "t", "backend": "dbt", "expectation": "stripped",
              "result": "PASS", "seconds": "1.0", "detail": "d", "evidence": evidence}],
            strict=True, verify=True, probe_gaps=False)
        return list(_csv.DictReader(path.open(encoding="utf-8")))[-1]


fallback = emitted_row({"tier": "stripped", "verify_compare": VERIFY_COMPARE_UNAVAILABLE,
                        "bitwise_parity": "0", "compared_log_messages": "",
                        "determinism_unmeasured": "1"})
check("fallback row does NOT claim deterministic=1",
      fallback["deterministic"] == "", repr(fallback["deterministic"]))
check("fallback row names why no verdict exists, rather than leaving it blank",
      fallback["verify_compare"] == VERIFY_COMPARE_UNAVAILABLE, repr(fallback["verify_compare"]))
check("fallback outcome is still a PASS (the guest ran and the compare succeeded)",
      fallback["outcome"] == "pass", repr(fallback["outcome"]))
check("the no-verdict sentinel is not a bitwise-capable comparator",
      VERIFY_COMPARE_UNAVAILABLE not in BITWISE_CAPABLE_COMPARATORS)

typed = emitted_row({"tier": "bitwise", "verify_compare": "canonical",
                     "bitwise_parity": "1", "compared_log_messages": "348|348"})
check("a typed verdict DOES still claim deterministic=1 (not inert)",
      typed["deterministic"] == "1", repr(typed["deterministic"]))
check("typed row carries its counts into the row",
      typed["compared_log_messages"] == "348|348", repr(typed["compared_log_messages"]))

print("case SCHEMA — the evidence columns exist and sit in the canonical header")
for column in EVIDENCE_COLUMNS:
    check(f"{column} is in SCORECARD_HEADER", column in SCORECARD_HEADER)
check("evidence columns are the last four",
      SCORECARD_HEADER[-4:] == EVIDENCE_COLUMNS, repr(SCORECARD_HEADER[-4:]))

print()
if FAILURES:
    print(f"FAIL ({len(FAILURES)} assertions)")
    sys.exit(1)
print("PASS")
