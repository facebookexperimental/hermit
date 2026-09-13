#!/usr/bin/env python3
"""Exercise the Rust validate driver's signal traps and ledger writer without a build."""

from __future__ import annotations

import json
import os
from collections.abc import Callable
from pathlib import Path
import re
import signal
import socket
import subprocess
import sys
import tempfile
import time


ROOT = Path(__file__).resolve().parents[1]

# The machine-readable prefix ci/lint-checks-node.sh greps for. Text, not an exit
# code, because make collapses any nonzero recipe status into its own error.
NO_RESULT_MARKER = "NO-RESULT-CASE:"

# EX_TEMPFAIL. scripts/validate.rs:5365 reserves 75 as "the only nonzero code that
# is not a product failure", outcome_is_no_result() classifies it `no_result`, and
# ci/lint-checks-node.sh already exits 75 for the sibling case (uninitialized
# submodules).
#
# ⚠️ SHARING A CODE NEEDS AN ARGUMENT, not just a precedent -- a code shared by two
# conditions is a collapsed value when they want different reactions. That rule is
# stated in ci-hub/bin/gh-merge-verified IN THE DEV-HERMIT PARENT REPOSITORY; it is
# not in this repository, which has no ci-hub/ directory. Naming the repo because a
# citation a reader cannot resolve from here is worse than no citation. Here a new code
# is not merely unnecessary, it is UNAVAILABLE: validate.rs recognises exactly one
# no-result value, so any other number is classified a FAILURE and would reintroduce
# the false main-red this exists to remove. The two conditions also want the same
# reaction -- nothing was evaluated, fix the environment, re-run -- and differ only in
# which fix, which the message carries. The full argument is in ci/lint-checks-node.sh.
NO_RESULT_EXIT_CODE = 75
FINAL_VALIDATE_STATUS_PREFIX = "FINAL_VALIDATE_STATUS: "
FINAL_VALIDATE_STATUSES = frozenset(("PASSED", "FAILED", "COULD_NOT_RUN"))


class ValidateChildRefused(RuntimeError):
    """The child hit the re-entrancy guard before the stop-test seam."""


def final_validate_status(output: str) -> str | None:
    """Return the last complete status line; absence remains unknown."""
    matches = [
        line.removeprefix(FINAL_VALIDATE_STATUS_PREFIX)
        for line in output.splitlines()
        if line.startswith(FINAL_VALIDATE_STATUS_PREFIX)
    ]
    if not matches:
        return None
    status = matches[-1]
    if status not in FINAL_VALIDATE_STATUSES:
        raise AssertionError(f"validate emitted unknown final status {status!r}")
    return status


_REFUSAL_SUMMARY = re.compile(
    r"🚫 validate REFUSED \(exit 75\) — profile .+ @ (?:[0-9a-f]{40}|unknown)"
)
_REENTRANCY_REFUSAL = "   refused by: the re-entrancy guard"


def child_hit_reentrancy_refusal(output: str, returncode: int) -> bool:
    """Did this child report the refusal that kept it from reaching the seam?"""
    if (
        returncode != NO_RESULT_EXIT_CODE
        or final_validate_status(output) != "COULD_NOT_RUN"
    ):
        return False

    lines = output.splitlines()
    status_index = max(
        index
        for index, line in enumerate(lines)
        if line.startswith(FINAL_VALIDATE_STATUS_PREFIX)
    )
    summary_indices = [
        index
        for index, line in enumerate(lines[:status_index])
        if _REFUSAL_SUMMARY.fullmatch(line)
    ]
    if not summary_indices:
        return False
    summary_index = summary_indices[-1]
    return lines[summary_index + 1 : summary_index + 2] == [_REENTRANCY_REFUSAL]


class NoParentAdapter(RuntimeError):
    """The dev-hermit parent adapter is not reachable from this checkout.

    ⚠️ THIS IS A SETUP CONDITION, NOT A FINDING, AND THE DISTINCTION IS EXPENSIVE
    HERE. `run_canonical_adapter_contract(refuse=False)` deliberately exercises the
    REAL parent adapter -- that is the whole point of the non-refusing arm -- so
    when the parent is absent the contract cannot be tested at all. It has not
    failed; it has not passed; it was not evaluated.

    Reported as a failure it manufactures a MAIN-RED, which is a standing P0 here,
    and it does so from the DEFAULT working layout: agents are told to land from a
    detached worktree under /tmp, whose ancestors are only /tmp and /. So the false
    reading is the common one and the true one is the exception. Measured
    2026-08-25 on clean main from /tmp: the old `next(...)` raised a BARE
    StopIteration -- no message, no path, nothing naming the directory -- and
    `make lint-checks` surfaced it as `Error 1`. Telling a precondition from a
    finding required reading this file's source.

    ⚠️ AND DO NOT "FIX" THIS BY INVENTING A PARENT. Planting a shadow adapter, or
    falling back to the refusing arm's temporary one, would make the non-refusing
    arm pass while testing nothing -- a green that means less than the red it
    replaced. Absence of the parent is a no_result, and only a no_result.
    """


def find_parent_adapter() -> Path | None:
    """The nearest ancestor carrying the parent ledger adapter, or None.

    Returns rather than raises so the caller decides what absence means. The
    previous spelling put that decision inside `next()`, which can only raise.
    """
    for candidate in ROOT.parents:
        if (candidate / "ci-hub" / "ledger" / "validate_rows.py").is_file():
            return candidate
    return None
VALIDATE = ROOT / "scripts" / "validate.rs"
TEST_ROOTS: list[Path] = []
OUTER_VALIDATE_ENV = (
    "DEV_HERMIT_PARENT",
    "DEV_HERMIT_TOOL_ROOT",
    "CI_HUB_APPLY_LOCAL_LABEL",
    "CI_HUB_VALIDATE_PRODUCER",
    "CI_HUB_VALIDATE_CONCURRENT",
    "CI_HUB_VALIDATE_LOCK_OWNER_PID",
    "CI_HUB_VALIDATE_LOCK_OWNER_FILE",
    "CI_HUB_VALIDATE_RUN_NUMBER",
    "E2E_RUN_ID",
    "VALIDATE_STOP_TEST_AUTHORITY_STATUS_JSON",
)


def test_env_without_outer_validate() -> dict[str, str]:
    env = os.environ.copy()
    # These children exercise validate's stop and cleanup paths, not the outer
    # launcher's split-root or admission contract. Each case supplies the exact
    # parent/ledger/authority inputs it owns.
    for name in OUTER_VALIDATE_ENV:
        env.pop(name, None)
    return env


def stop_test_env(
    tmpdir: Path,
    ledger: Path,
    *,
    lock_proven: bool = False,
    forged_owner: bool = False,
) -> dict[str, str]:
    TEST_ROOTS.append(tmpdir)
    env = test_env_without_outer_validate()
    env.update(
        HERMIT_VALIDATE_STOP_TEST_MODE="1",
        HERMIT_VALIDATE_LEDGER=str(ledger),
        DEV_HERMIT_PARENT=str(ROOT.parent),
        VALIDATE_SKIP_INNER_DIRTY_WORKING_TREE_AND_REBASE_FRESHNESS_CHECKS="1",
        VALIDATE_STOP_TEST_TMP_ROOT=str(tmpdir / "validation"),
        DAGRUN_LOG_DIR=str(tmpdir / "dagrun-evidence"),
        TMPDIR=str(tmpdir),
    )
    if lock_proven:
        stat_fields = Path(f"/proc/{os.getpid()}/stat").read_text().rsplit(")", 1)[1].split()
        start_ticks = int(stat_fields[19])
        host = socket.gethostname().split(".", 1)[0]
        commit = subprocess.check_output(
            ["git", "rev-parse", "HEAD"], cwd=ROOT, text=True
        ).strip()
        authority = {
            "schema_version": 1,
            "admissible": True,
            "state": "held",
            "reason_code": None,
            "canonical_anchor_held": True,
            "cleanup_state": "none",
            "holder": {
                "kind": "validate",
                "target": commit,
                "host": host,
            },
            "owner": {
                "host": host,
                "liveness": "alive",
                "pid": os.getpid(),
                "start_ticks": start_ticks,
                "boot_id": Path("/proc/sys/kernel/random/boot_id").read_text().strip(),
            },
        }
        env.update(
            VALIDATE_STOP_TEST_AUTHORITY_STATUS_JSON=json.dumps(authority),
            # Plant the weaker legacy observation too. Canonical owner ancestry
            # must win; otherwise parked stop-test fixtures make every genuine
            # admitted schema-5 receipt unqualifiable.
            CI_HUB_VALIDATE_CONCURRENT="true",
        )
    if forged_owner:
        owner_file = tmpdir / "caller-chosen.owner"
        owner_file.write_text(f"pid={os.getpid()}\n")
        env.update(
            CI_HUB_VALIDATE_LOCK_OWNER_PID=str(os.getpid()),
            CI_HUB_VALIDATE_LOCK_OWNER_FILE=str(owner_file),
        )
    return env


def assert_stop_test_never_started_dagrun(tmpdir: Path, output: str) -> None:
    assert not (tmpdir / "dagrun-evidence").exists(), output
    assert "[dagrun]" not in output and "[scheduler]" not in output, output


def check_stop_test_env_does_not_inherit_outer_validate() -> None:
    saved = os.environ.copy()
    try:
        os.environ.update({name: "/outer/value" for name in OUTER_VALIDATE_ENV})
        os.environ.update(E2E_RUN_ID="outer-run-id", CI_HUB_VALIDATE_RUN_NUMBER="4242")
        with tempfile.TemporaryDirectory(prefix="validate-stop-env-") as tmp:
            tmpdir = Path(tmp)
            env = stop_test_env(tmpdir, tmpdir / "ledger.jsonl")
    finally:
        os.environ.clear()
        os.environ.update(saved)

    assert env["DEV_HERMIT_PARENT"] == str(ROOT.parent), env
    for name in set(OUTER_VALIDATE_ENV) - {"DEV_HERMIT_PARENT"}:
        assert name not in env, (name, env[name])


# HOW LONG A HOOK MAY TAKE TO SIGNAL READY, AND WHY IT IS NOT 10 SECONDS.
#
# Both readiness waits below used a hard 10s. Measured on a quiet moment: the
# hook fires in 0.4s, 3 of 3 -- so 10s looks like ten times the headroom anyone
# could need. It is not, on this box. With four validates competing for the
# machine (load average 22-41 while this was measured) the full test failed 1 run
# in 3, alternating between the two deadlines: once on
# VALIDATE_STOP_TEST_READY, once on the cleanup hook. Neither is a stop-path
# defect and neither is a cold cache -- warm_validate_binary() already removed
# that -- they are a scheduling delay on a shared box.
#
# RAISING THIS COSTS NOTHING ON A REAL FAILURE, which is the point. Both loops
# poll `process.poll()` every iteration and abort the instant the child exits, so
# a crash, a refusal or a nonzero exit is still detected in milliseconds. The
# only case that now waits longer is a genuine HANG -- a child that is alive and
# silent -- and for that case waiting two minutes to be sure beats failing a
# whole pre-flight gate on someone else's build.
#
# The earlier version of this reasoning argued a large window would "hide a
# wedged startup behind the build". That was true while the compile happened
# inside the window. It no longer does, so the objection no longer applies.
#
# RAISED 120 -> 600 on 2026-08-25, at main a5fef7ff7623, because 120 was still not
# enough just above the load band measured above. With a full validate holding the
# box at load average 42.8, this script failed on the VALIDATE_STOP_TEST_READY
# deadline with the validate log at 0 bytes -- the child was alive and had not yet
# been scheduled enough to emit the marker. Two more runs at load 35-43 on the same
# commit then PASSED, taking 8m54s and 31s respectively. A spread of 31s to 8m54s
# with one hard timeout, same commit and same box within an hour, is a scheduling
# delay, not a stop-path defect.
#
# This matters more than it did: check.lint_checks now schedules this script in CI
# for the first time, so an intermittent deadline here becomes an intermittent red
# in the DAG -- the standing red everyone learns to ignore. The argument above is
# unchanged and still carries the raise: `process.poll()` is checked every 50ms in
# both loops, so a crash, refusal or nonzero exit is still caught in milliseconds
# and only a genuine silent hang waits the full window.
READY_TIMEOUT_SECONDS = 600


# HOW LONG THE CHILD MAY TAKE TO EXIT ONCE SIGNALLED.
#
# The two readiness deadlines above were raised to READY_TIMEOUT_SECONDS for
# scheduling delay on a shared box. The two `process.wait()` calls below were
# left at a hard 10s, and they are the SAME quantity measured from the other
# end: how long a loaded machine takes to get round to a process.
#
# MEASURED HERE, 21 observations at load ~56: SIGTERM/INT/HUP exit in
# 0.364-0.466s and SIGKILL in 0.001s. That is the SAME ~0.4s baseline this file
# already records for the readiness hook ("the hook fires in 0.4s, 3 of 3"), so
# 10s was already 21x the observed maximum -- and it still expired: one run in
# four failed at `process.wait(timeout=10)` in run_signal with load ~96, while
# the three passes sat at 55-61. The distribution has a long tail under
# contention; the median was never the problem.
#
# ⚠️ THIS IS NOT A TIMEOUT RELAXED TO MAKE A FAILURE GO AWAY. It is the value
# already derived above, applied to the case it missed. The cost is bounded and
# one-sided: unlike the readiness loops, `process.wait()` does not poll, so a
# genuine HANG now costs the full bound instead of 10s -- paid once, against a
# one-in-four false failure of a pre-flight gate. A crash or nonzero exit is
# still observed the instant it happens, because `wait()` returns then.
EXIT_TIMEOUT_SECONDS = READY_TIMEOUT_SECONDS


def warm_validate_binary() -> None:
    """Compile validate.rs ONCE, before anything is on the clock.

    validate.rs is a rust-script, so the first invocation COMPILES it, and the
    readiness waits below are not built to sit through a build: measured on this
    box with an isolated cache, 14.9s to produce 54 MB of artifacts; the pull
    request that removed this test from the pre-flight measured 36s under load.
    Against the original 10s window that meant the first `run_signal` failed on
    any cold cache with "did not emit 'VALIDATE_STOP_TEST_READY'" -- a message
    about readiness for what was really a build. That is why the failure looked
    intermittent: it was tracking cache warmth, not anything about the stop path.
    Reproduced both ways here, with XDG_CACHE_HOME pointed at an empty directory
    so the shared cache was never disturbed: cold and unwarmed it fails, cold and
    warmed it passes.

    Warming and READY_TIMEOUT_SECONDS fix two DIFFERENT causes and both are
    needed. This one takes the build out of the measurement, so a readiness
    timeout can no longer mean "it was compiling". The timeout constant covers
    what remains, which is scheduling delay on a shared box.

    `--show-plan` is the probe because validate.rs executes NOTHING under it and
    explicitly does not take the per-checkout invocation lock, so warming cannot
    contend with a real validate or write a ledger row. Its exit code is ignored
    on purpose: the point is the build, and a refusal (dirty tree, for one) still
    compiles the script.
    """
    subprocess.run(
        [str(VALIDATE), "full", "--show-plan"],
        cwd=ROOT,
        env={
            **os.environ,
            "VALIDATE_SKIP_INNER_DIRTY_WORKING_TREE_AND_REBASE_FRESHNESS_CHECKS": "1",
        },
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        timeout=900,
        check=False,
    )


def wait_for_text(log: Path, text: str, process: subprocess.Popen[bytes]) -> None:
    deadline = time.monotonic() + READY_TIMEOUT_SECONDS
    while time.monotonic() < deadline:
        if log.exists() and text in log.read_text(errors="replace"):
            return
        returncode = process.poll()
        if returncode is not None:
            seen = log.read_text(errors="replace") if log.exists() else "<log never created>"
            status = final_validate_status(seen)
            if child_hit_reentrancy_refusal(seen, returncode):
                raise ValidateChildRefused(
                    f"the spawned validate hit the re-entrancy guard before ready "
                    f"(rc={returncode}); the stop path was never exercised.\n"
                    f"--- validate output ({len(seen)} bytes) ---\n{seen[-2000:]}"
                )
            if status is not None:
                expected = {
                    "PASSED": 0,
                    "FAILED": 1,
                    "COULD_NOT_RUN": NO_RESULT_EXIT_CODE,
                }[status]
                if returncode != expected:
                    raise AssertionError(
                        f"validate final status {status} disagrees with "
                        f"rc={returncode}; expected {expected}\n"
                        f"--- validate output ({len(seen)} bytes) ---\n{seen[-2000:]}"
                    )
            raise AssertionError(
                f"validate exited before ready: rc={returncode}; "
                f"final_status={status or 'absent'}\n"
                f"--- validate output ({len(seen)} bytes) ---\n{seen[-2000:]}"
            )
        time.sleep(0.05)
    # SHOW WHAT IT ACTUALLY SAW. The bare form of this message cost real time:
    # it names readiness, so it was read as a slow start, when the log underneath
    # was saying something else entirely. A timeout assertion that hides the
    # output it timed out on sends every reader to reproduce it by hand.
    seen = log.read_text(errors="replace") if log.exists() else "<log never created>"
    raise AssertionError(
        f"validate stop-test hook did not emit {text!r} within {READY_TIMEOUT_SECONDS}s.\n"
        f"warm_validate_binary() pays the rust-script compile before this window "
        f"opens, so a cold cache is NOT the explanation.\n"
        f"--- validate output ({len(seen)} bytes) ---\n{seen[-2000:]}"
    )


def run_final_validate_status_contract() -> None:
    """Pin the final status, child status, and refusal-reason contract."""

    class Exited:
        def __init__(self, returncode: int):
            self.returncode = returncode

        def poll(self) -> int:
            return self.returncode

    sample_commit = "0123456789abcdef0123456789abcdef01234567"
    genuine = (
        f"🚫 validate REFUSED (exit 75) — profile full @ {sample_commit}\n"
        f"{_REENTRANCY_REFUSAL}\n"
        "   nodes: none executed (stopped before the DAG ran)\n"
        "FINAL_VALIDATE_STATUS: COULD_NOT_RUN\n"
    )
    cases = (
        ("re-entrancy refusal", genuine, NO_RESULT_EXIT_CODE, True, None),
        (
            "other output before the final refusal",
            "wrapper mentioned FINAL_VALIDATE_STATUS: FAILED\n" + genuine,
            NO_RESULT_EXIT_CODE,
            True,
            None,
        ),
        (
            "unknown commit fallback",
            "🚫 validate REFUSED (exit 75) — profile full @ unknown\n"
            f"{_REENTRANCY_REFUSAL}\n"
            "FINAL_VALIDATE_STATUS: COULD_NOT_RUN\n",
            NO_RESULT_EXIT_CODE,
            True,
            None,
        ),
        (
            "line that only starts like a refusal",
            "🚫 validate REFUSED is quoted documentation, not a final summary\n"
            f"{_REENTRANCY_REFUSAL}\n"
            "FINAL_VALIDATE_STATUS: COULD_NOT_RUN\n",
            NO_RESULT_EXIT_CODE,
            False,
            "validate exited before ready: rc=75; final_status=COULD_NOT_RUN",
        ),
        (
            "argument parsing refusal",
            f"🚫 validate REFUSED (exit 75) — profile full @ {sample_commit}\n"
            "   refused by: argument parsing\n"
            "FINAL_VALIDATE_STATUS: COULD_NOT_RUN\n",
            NO_RESULT_EXIT_CODE,
            False,
            "validate exited before ready: rc=75; final_status=COULD_NOT_RUN",
        ),
        (
            "child-status mismatch",
            genuine,
            1,
            False,
            "validate final status COULD_NOT_RUN disagrees with rc=1; expected 75",
        ),
        (
            "earlier quoted status",
            "FINAL_VALIDATE_STATUS: COULD_NOT_RUN\n"
            "quoted documentation above must not win\n"
            "FINAL_VALIDATE_STATUS: FAILED\n",
            1,
            False,
            "validate exited before ready: rc=1; final_status=FAILED",
        ),
        (
            "status without a refusal reason",
            "FINAL_VALIDATE_STATUS: COULD_NOT_RUN\n",
            NO_RESULT_EXIT_CODE,
            False,
            "validate exited before ready: rc=75; final_status=COULD_NOT_RUN",
        ),
        (
            "status absence",
            "ordinary output with no status\n",
            NO_RESULT_EXIT_CODE,
            False,
            "validate exited before ready: rc=75; final_status=absent",
        ),
    )

    with tempfile.TemporaryDirectory(prefix="validate-final-status-") as tmp:
        log = Path(tmp) / "validate.log"
        for name, output, returncode, should_refuse, expected_error in cases:
            log.write_text(output, encoding="utf-8")
            try:
                wait_for_text(log, "never-written", Exited(returncode))
            except ValidateChildRefused as exc:
                refused = True
                if not should_refuse:
                    raise AssertionError(
                        f"{name}: ordinary failure was classified as a refusal: {exc}"
                    ) from exc
            except AssertionError as exc:
                refused = False
                if should_refuse:
                    raise AssertionError(
                        f"{name}: genuine refusal was not classified by name: {exc}"
                    ) from exc
                first_line = str(exc).splitlines()[0]
                if first_line != expected_error:
                    raise AssertionError(
                        f"{name}: unexpected failure path: {first_line!r}; "
                        f"expected {expected_error!r}"
                    ) from exc
            else:
                raise AssertionError(f"{name}: an exited child reached readiness")
            assert refused is should_refuse, (name, refused, should_refuse, output)


def assert_schema5_contract(row: dict, *, admitted: bool = False) -> None:
    """A current writer never escapes strict evidence by downgrading schema."""
    assert row["schema_version"] == 5, row
    assert row["repo"] == "hermit", row
    assert row["producer"] == "hermit-validate-rs", row
    assert row["run_id"] is None, row
    assert "run_number" not in row, row
    # Exercise write_ledger itself, not only validate.rs's synthetic helper.
    # A real outer failure positively knows there are no failed lane substeps;
    # gates that established no failure must omit the collection entirely.
    for gate in row["gates"]:
        origin = gate["failure_origin"]
        if origin is None:
            assert gate["failure_origin"] is None, gate
            assert "failed_substeps" not in gate, gate
        else:
            assert not gate["aborted"], gate
            assert gate["result"] == "fail", gate
            assert origin == "outer_gate", gate
            assert isinstance(gate["failed_substeps"], list), gate
            assert gate["failed_substeps"] == [], gate
    expected_depth = int(
        subprocess.check_output(
            ["git", "rev-list", "--count", row["commit"]], cwd=ROOT, text=True
        ).strip()
    )
    assert row["git_depth"] == expected_depth > 0, row
    if admitted:
        assert row["admission"] == "ci-hub-validate-lock", row
        assert row["concurrent_validates"] == 0, row
        assert row["concurrency_proof"] == "validate_lock_owner_ancestry", row
    else:
        # These direct stop-path tests do not run through validate-lock, so their
        # honest admission result is unknown. A schema-4 fallback would incorrectly
        # grandfather that absence; schema 5 plus null is correctly unqualifiable.
        assert row["admission"] is None, row


def run_signal(
    sig: signal.Signals,
    expect_record: bool,
    *,
    prior_failure: bool = False,
    lock_proven: bool = False,
    forged_owner: bool = False,
) -> None:
    with tempfile.TemporaryDirectory(prefix=f"validate-stop-{sig.name.lower()}-") as tmp:
        tmpdir = Path(tmp)
        ledger = tmpdir / "ledger.jsonl"
        log = tmpdir / "validate.log"
        env = stop_test_env(
            tmpdir, ledger, lock_proven=lock_proven, forged_owner=forged_owner
        )
        env.update(
            VALIDATE_STOP_TEST_PRIOR_FAILURE="1" if prior_failure else "0",
        )
        with log.open("wb") as output:
            process = subprocess.Popen(
                [str(VALIDATE), "full"],
                cwd=ROOT,
                env=env,
                stdout=output,
                stderr=subprocess.STDOUT,
                start_new_session=True,
            )
            wait_for_text(log, "VALIDATE_STOP_TEST_READY", process)
            process.send_signal(sig)
            rc = process.wait(timeout=EXIT_TIMEOUT_SECONDS)

        rendered = log.read_text(errors="replace")
        assert_stop_test_never_started_dagrun(tmpdir, rendered)

        rows = [json.loads(line) for line in ledger.read_text().splitlines()] if ledger.exists() else []
        if not expect_record:
            assert not rows, (sig.name, rows)
            assert rc == -sig.value, (sig.name, rc)
            return

        assert rc == NO_RESULT_EXIT_CODE, (sig.name, rc, rendered)
        assert len(rows) == 1, (sig.name, rows)
        row = rows[0]
        assert_schema5_contract(row, admitted=lock_proven)
        assert row["result"] == ("fail" if prior_failure else "no_result"), row
        assert row["raw_result"] == "fail", row
        assert row["gates_run"] == row["checks"] == 2, row
        assert row["gates_expected"] == 2, row
        assert row["validation_complete"] is False, row
        assert row["product_result_node_count"] == 2, row
        assert len(row["product_result_nodes"]) == 2, row
        assert row["understood_infrastructure_failure_nodes"] == {}, row
        assert row["understood_prerequisite_failure_nodes"] == [], row
        assert row["no_result_nodes"] == [], row
        assert row["failures"] == (1 if prior_failure else 0), row
        assert row["interruption_signal"] == sig.name.removeprefix("SIG"), row


def run_incomplete_exit() -> None:
    with tempfile.TemporaryDirectory(prefix="validate-stop-incomplete-") as tmp:
        tmpdir = Path(tmp)
        # A fixture path named exactly `ledger` must remain an explicit file;
        # basename matching alone must never redirect a caller-selected path to
        # a sibling adapter.
        ledger = tmpdir / "ledger"
        env = stop_test_env(tmpdir, ledger)
        env.update(
            VALIDATE_STOP_TEST_EXIT_EARLY="1",
        )
        process = subprocess.run(
            [str(VALIDATE), "full"],
            cwd=ROOT,
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            check=False,
        )
        assert process.returncode == 1, process.stdout.decode(errors="replace")
        rows = [json.loads(line) for line in ledger.read_text().splitlines()]
        assert len(rows) == 1, rows
        row = rows[0]
        assert_schema5_contract(row)
        # An ordinary early exit is not an operator stop. It remains a raw
        # failure unless the producer carries an explicit interruption signal.
        assert row["result"] == "fail", row
        assert row["gates_run"] == row["gates_expected"] == 2, row
        assert row["validation_complete"] is False, row
        assert row["product_result_node_count"] == 2, row
        assert row["interruption_signal"] is None, row


def run_off_record_incomplete_exit() -> None:
    """The early stop-test seam must preserve off-record evidence isolation."""
    with tempfile.TemporaryDirectory(prefix="validate-stop-off-record-") as tmp:
        tmpdir = Path(tmp)
        ledger = tmpdir / "ledger.jsonl"
        env = stop_test_env(tmpdir, ledger)
        env.update(VALIDATE_STOP_TEST_EXIT_EARLY="1")
        process = subprocess.run(
            [str(VALIDATE), "--allow-local-off-the-record-run", "quick"],
            cwd=ROOT,
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            check=False,
        )
        output = process.stdout.decode(errors="replace")
        assert process.returncode == 1, output
        assert not ledger.exists() or not ledger.read_bytes(), output
        assert "OFF THE RECORD" in output, output
        assert "ledger:" not in output, output


def run_canonical_adapter_contract(*, refuse: bool) -> None:
    """Production-shaped writes use the parent adapter, never a raw shadow."""
    with tempfile.TemporaryDirectory(prefix="validate-canonical-adapter-") as tmp:
        tmpdir = Path(tmp)
        canonical_root = tmpdir / "canonical-root"
        if refuse:
            parent = tmpdir / "parent"
            adapter = parent / "ci-hub" / "ledger" / "validate_rows.py"
            adapter.parent.mkdir(parents=True)
            adapter.write_text(
                "import sys\n"
                "sys.stdin.read()\n"
                "print('planted refusal', file=sys.stderr)\n"
                "raise SystemExit(2)\n"
            )
        else:
            # Exercise the REAL parent adapter, redirected only through its
            # explicit stop-test root. This proves the producer/consumer
            # contract without appending the machine's authoritative ledger.
            parent = find_parent_adapter()
            if parent is None:
                raise NoParentAdapter(
                    "the dev-hermit parent adapter "
                    "(ci-hub/ledger/validate_rows.py, in the dev-hermit PARENT "
                    "repository -- this repository has no ci-hub/ directory) is "
                    f"not on any ancestor of {ROOT}"
                )
        raw_shadow = parent / "ignored" / "validate-run-ledger.jsonl"
        raw_before = raw_shadow.read_bytes() if raw_shadow.exists() else None
        env = test_env_without_outer_validate()
        env.update(
            HERMIT_VALIDATE_STOP_TEST_MODE="1",
            DEV_HERMIT_PARENT=str(parent),
            CI_HUB_VALIDATE_LEDGER_TEST_ROOT=str(canonical_root),
            VALIDATE_SKIP_INNER_DIRTY_WORKING_TREE_AND_REBASE_FRESHNESS_CHECKS="1",
            VALIDATE_STOP_TEST_TMP_ROOT=str(tmpdir / "validation"),
            VALIDATE_STOP_TEST_EXIT_EARLY="1",
            TMPDIR=str(tmpdir),
        )
        env.pop("HERMIT_VALIDATE_LEDGER", None)
        process = subprocess.run(
            [str(VALIDATE), "full"],
            cwd=ROOT,
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            check=False,
        )
        output = process.stdout.decode(errors="replace")
        assert process.returncode == 1, output
        raw_after = raw_shadow.read_bytes() if raw_shadow.exists() else None
        assert raw_after == raw_before, "canonical write touched the retired raw shadow"
        if refuse:
            assert not list(canonical_root.glob("ledger/**/*.jsonl")), output
            assert "canonical ledger writer" in output and "refused" in output, output
        else:
            # WHERE THE PRODUCER'S BYTES LAND, AND WHY THIS MOVED.
            #
            # This used to glob `ledger/hermit/*/*.jsonl` -- the PUBLISHED shard.
            # The producer does not write that path and has not for some time:
            # publication into `ledger/<team>/<host>/<YYYY-MM>.jsonl` is a
            # separate later step, and what this run produces is one append to
            # the live spool. Measured against the real adapter: the only file
            # created anywhere under CI_HUB_VALIDATE_LEDGER_TEST_ROOT is
            #   ignored/ci-hub/validate-ledger-spool/<team>__<host>__<YYYY-MM>__<uuid>.jsonl
            # so the old assertion could only ever see an empty list. It was not
            # a flake and it was not wrong about the CONTENT -- it was looking
            # one stage downstream of the thing under test.
            #
            # THE PATH IS NOT HARDCODED AGAIN. The producer prints the shard it
            # says it wrote; this reads that self-report and then proves the file
            # is really there with the expected content. That makes the next
            # relocation a test failure that NAMES the new path rather than an
            # empty glob, and it adds a contract the old form did not have: a
            # producer that reports a write it did not perform now fails here.
            appended = [
                line for line in output.splitlines()
                if "canonical ledger record appended" in line
            ]
            assert len(appended) == 1, (appended, output)
            report = json.loads(appended[0][appended[0].index("{"):])
            spool_rel = report["spool"]
            assert spool_rel.startswith("ignored/"), (spool_rel, output)
            shard = canonical_root / spool_rel
            assert shard.is_file(), (spool_rel, sorted(
                str(p.relative_to(canonical_root))
                for p in canonical_root.rglob("*") if p.is_file()
            ))
            # The producer must NOT reach past the spool and write the published
            # shard itself. That separation is the whole reason the spool exists,
            # so assert the published location stays empty rather than merely
            # not looking at it.
            published = list(canonical_root.glob("ledger/*/*/*.jsonl"))
            assert not published, (published, output)

            events = [json.loads(line) for line in shard.read_text().splitlines()]
            assert len(events) == 1, events
            assert events[0]["schema"] == "validate-ledger/v1", events[0]
            assert_schema5_contract(events[0]["legacy_row"])


def run_cleanup_signal_race() -> None:
    with tempfile.TemporaryDirectory(prefix="validate-stop-cleanup-race-") as tmp:
        tmpdir = Path(tmp)
        ledger = tmpdir / "ledger.jsonl"
        log = tmpdir / "validate.log"
        cleanup_ready = tmpdir / "cleanup-ready"
        env = stop_test_env(tmpdir, ledger)
        env.update(
            VALIDATE_STOP_TEST_EXIT_EARLY="1",
            VALIDATE_STOP_TEST_CLEANUP_READY_FILE=str(cleanup_ready),
            VALIDATE_STOP_TEST_CLEANUP_DELAY_SECONDS="1",
        )
        with log.open("wb") as output:
            process = subprocess.Popen(
                [str(VALIDATE), "full"],
                cwd=ROOT,
                env=env,
                stdout=output,
                stderr=subprocess.STDOUT,
                start_new_session=True,
            )
            deadline = time.monotonic() + READY_TIMEOUT_SECONDS
            while time.monotonic() < deadline and not cleanup_ready.exists():
                if process.poll() is not None:
                    raise AssertionError(
                        f"validate exited before cleanup hook: rc={process.returncode}"
                    )
                time.sleep(0.01)
            assert cleanup_ready.exists(), (
                f"cleanup hook did not become ready within {READY_TIMEOUT_SECONDS}s; "
                f"validate output: {log.read_text(errors='replace')[-1500:]}"
            )
            for _ in range(20):
                process.send_signal(signal.SIGTERM)
                time.sleep(0.01)
            rc = process.wait(timeout=EXIT_TIMEOUT_SECONDS)

        rows = [json.loads(line) for line in ledger.read_text().splitlines()]
        assert rc == 1, (rc, log.read_text(errors="replace"))
        assert len(rows) == 1, rows
        assert_schema5_contract(rows[0])
        assert rows[0]["result"] == "fail", rows[0]
        assert rows[0]["interruption_signal"] is None, rows[0]


def run_signal_cases() -> None:
    warm_validate_binary()
    for sig in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
        run_signal(sig, expect_record=True)
    run_signal(signal.SIGKILL, expect_record=False)
    run_signal(signal.SIGTERM, expect_record=True, prior_failure=True)
    run_signal(signal.SIGTERM, expect_record=True, lock_proven=True)
    # The retired shell contract trusted these caller-selected values. Rust must
    # ignore them: only the canonical authority query can establish admission.
    run_signal(signal.SIGTERM, expect_record=True, forged_owner=True)


def run_signal_cases_then_incomplete_exit(
    signal_cases: Callable[[], None], incomplete_exit: Callable[[], None]
) -> str | None:
    """Keep the incomplete-exit check independent of a refused signal child."""
    unevaluated: str | None = None
    try:
        signal_cases()
    except ValidateChildRefused as exc:
        unevaluated = f"signal stop paths (validate declined to start): {exc}"
    incomplete_exit()
    return unevaluated


def check_signal_refusal_does_not_skip_incomplete_exit() -> None:
    calls: list[str] = []

    def refused_signal_cases() -> None:
        calls.append("signal cases")
        raise ValidateChildRefused("fixture refusal")

    def incomplete_exit() -> None:
        calls.append("incomplete exit")

    unevaluated = run_signal_cases_then_incomplete_exit(
        refused_signal_cases, incomplete_exit
    )
    assert calls == ["signal cases", "incomplete exit"], calls
    assert unevaluated is not None and unevaluated.startswith("signal stop paths"), (
        unevaluated
    )


def main(argv: list[str] | None = None) -> None:
    args = sys.argv[1:] if argv is None else argv
    if args == ["--final-status-self-test"]:
        run_final_validate_status_contract()
        print(
            "PASS: final validate status, child status, complete refusal summary, "
            "and re-entrancy reason are classified together"
        )
        return
    if args:
        raise SystemExit(
            "usage: test_validate_stop_paths.py [--final-status-self-test]"
        )

    # Every child this file spawns is a fixture, not a nested validation. The
    # re-entrancy guard runs before the stop-test seam, so inheriting the outer
    # run's marker would prevent the fixture from observing any stop path.
    os.environ.pop("HERMIT_VALIDATE_ACTIVE", None)

    run_final_validate_status_contract()
    check_stop_test_env_does_not_inherit_outer_validate()
    check_signal_refusal_does_not_skip_incomplete_exit()

    unevaluated: list[str] = []
    signal_unevaluated = run_signal_cases_then_incomplete_exit(
        run_signal_cases, run_incomplete_exit
    )
    if signal_unevaluated is not None:
        unevaluated.append(signal_unevaluated)
    run_off_record_incomplete_exit()
    # ⚠️ SKIP ONLY WHAT CANNOT BE EVALUATED, AND KEEP GOING.
    # An earlier version let NoParentAdapter propagate out of main(), which
    # abandoned the four steps below it -- refuse=True, the cleanup race and the
    # residue assertion -- while printing "Every other assertion in this file ran
    # and passed". Measured: all four PASS in a parentless tree, because the
    # refuse=True arm plants its own adapter and never needed a parent. Claiming
    # they ran was false, and abandoning them cost real coverage for a precondition
    # that affects exactly one arm of one case.
    try:
        run_canonical_adapter_contract(refuse=False)
    except NoParentAdapter as exc:
        unevaluated.append(f"canonical adapter contract, accept arm: {exc}")
    run_canonical_adapter_contract(refuse=True)
    run_cleanup_signal_race()
    leaked = [path for path in TEST_ROOTS if path.exists()]
    assert not leaked, f"stop-path test residue: {leaked}"
    if unevaluated:
        # Exit 0: everything evaluable here RAN AND PASSED, and saying otherwise
        # would report a failure that did not happen. The unevaluated arm is
        # announced on stderr with a machine-readable prefix so the CI node can
        # report no_result for the run as a whole -- see ci/lint-checks-node.sh.
        # make() cannot carry a no-result status (any nonzero recipe exit becomes
        # `make: *** Error N`), so the distinction has to leave this process as
        # text and be turned back into an exit code one layer out.
        # ⚠️ FLUSH STDOUT FIRST, so the marker is guaranteed to START A LINE.
        # ci/lint-checks-node.sh matches this marker ANCHORED (`grep -q "^..."`),
        # because the token is tracked text in this repo and an unanchored match
        # turns any checker that merely mentions it into a false no_result. The
        # cost of anchoring is that a marker which does NOT begin a line is missed
        # -- and that direction is the dangerous one, because a real no_result
        # would be silently reported as a pass. The node merges streams with
        # `2>&1`, and stdout is block-buffered when piped while stderr is not, so
        # an unterminated stdout line could otherwise still be open when this
        # writes and the marker would land mid-line. Flushing closes that window
        # here, at the source, rather than loosening the pattern there to
        # compensate.
        sys.stdout.flush()
        for item in unevaluated:
            print(f"{NO_RESULT_MARKER} {item}", file=sys.stderr)
        print(
            f"PARTIAL: every evaluable assertion passed; "
            f"{len(unevaluated)} case(s) could not be evaluated from {ROOT}. "
            "Run from a checkout nested under the dev-hermit parent to evaluate them.",
            file=sys.stderr,
        )
    signals_ran = not any(item.startswith("signal stop paths") for item in unevaluated)
    adapter_unevaluated = any(
        item.startswith("canonical adapter") for item in unevaluated
    )
    print(
        "PASS: "
        + (
            "TERM/INT/HUP => NO-RESULT; KILL => no record; "
            "prior failure remains fail; forged owner path is unadmitted; "
            if signals_ran
            else "signal stop paths NOT EVALUATED (validate declined to start); "
        )
        + (
            "canonical adapter REFUSE arm only (accept arm not evaluable here); "
            if adapter_unevaluated
            else "canonical adapter accept/refuse bracketed; "
        )
        + "cleanup is signal-atomic"
    )


if __name__ == "__main__":
    main()
