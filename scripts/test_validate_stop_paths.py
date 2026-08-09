#!/usr/bin/env python3
"""Exercise the Rust validate driver's signal traps and ledger writer without a build."""

from __future__ import annotations

import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import tempfile
import time


ROOT = Path(__file__).resolve().parents[1]
VALIDATE = ROOT / "scripts" / "validate.rs"
TEST_ROOTS: list[Path] = []


def stop_test_env(
    tmpdir: Path,
    ledger: Path,
    *,
    lock_proven: bool = False,
    forged_owner: bool = False,
) -> dict[str, str]:
    TEST_ROOTS.append(tmpdir)
    env = os.environ.copy()
    env.update(
        HERMIT_VALIDATE_STOP_TEST_MODE="1",
        HERMIT_VALIDATE_LEDGER=str(ledger),
        DEV_HERMIT_PARENT=str(ROOT.parent),
        VALIDATE_RUN_ON_DIRTY_TREE="1",
        VALIDATE_STOP_TEST_TMP_ROOT=str(tmpdir / "validation"),
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


def wait_for_text(log: Path, text: str, process: subprocess.Popen[bytes]) -> None:
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        if log.exists() and text in log.read_text(errors="replace"):
            return
        if process.poll() is not None:
            raise AssertionError(f"validate exited before ready: rc={process.returncode}")
        time.sleep(0.05)
    raise AssertionError(f"validate stop-test hook did not emit {text!r}")


def assert_schema5_contract(row: dict, *, admitted: bool = False) -> None:
    """A current writer never escapes strict evidence by downgrading schema."""
    assert row["schema_version"] == 5, row
    assert row["repo"] == "hermit", row
    assert row["producer"] == "hermit-validate-rs", row
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
            rc = process.wait(timeout=10)

        rows = [json.loads(line) for line in ledger.read_text().splitlines()] if ledger.exists() else []
        if not expect_record:
            assert not rows, (sig.name, rows)
            assert rc == -sig.value, (sig.name, rc)
            return

        assert rc == 130, (sig.name, rc, log.read_text(errors="replace"))
        assert len(rows) == 1, (sig.name, rows)
        row = rows[0]
        assert_schema5_contract(row, admitted=lock_proven)
        assert row["result"] == ("fail" if prior_failure else "no_result"), row
        assert row["raw_result"] == "fail", row
        assert row["gates_run"] == row["checks"] == 2, row
        assert row["failures"] == (1 if prior_failure else 0), row
        assert row["interruption_signal"] == sig.name.removeprefix("SIG"), row


def run_incomplete_exit() -> None:
    with tempfile.TemporaryDirectory(prefix="validate-stop-incomplete-") as tmp:
        tmpdir = Path(tmp)
        ledger = tmpdir / "ledger.jsonl"
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
        assert row["gates_run"] == 2 and row["gates_expected"] is None, row
        assert row["interruption_signal"] is None, row


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
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline and not cleanup_ready.exists():
                if process.poll() is not None:
                    raise AssertionError(
                        f"validate exited before cleanup hook: rc={process.returncode}"
                    )
                time.sleep(0.01)
            assert cleanup_ready.exists(), "cleanup hook did not become ready"
            for _ in range(20):
                process.send_signal(signal.SIGTERM)
                time.sleep(0.01)
            rc = process.wait(timeout=10)

        rows = [json.loads(line) for line in ledger.read_text().splitlines()]
        assert rc == 1, (rc, log.read_text(errors="replace"))
        assert len(rows) == 1, rows
        assert_schema5_contract(rows[0])
        assert rows[0]["result"] == "fail", rows[0]
        assert rows[0]["interruption_signal"] is None, rows[0]


def main() -> None:
    for sig in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
        run_signal(sig, expect_record=True)
    run_signal(signal.SIGKILL, expect_record=False)
    run_signal(signal.SIGTERM, expect_record=True, prior_failure=True)
    run_signal(signal.SIGTERM, expect_record=True, lock_proven=True)
    # The retired shell contract trusted these caller-selected values. Rust must
    # ignore them: only the canonical authority query can establish admission.
    run_signal(signal.SIGTERM, expect_record=True, forged_owner=True)
    run_incomplete_exit()
    run_cleanup_signal_race()
    leaked = [path for path in TEST_ROOTS if path.exists()]
    assert not leaked, f"stop-path test residue: {leaked}"
    print(
        "PASS: TERM/INT/HUP => NO-RESULT; KILL => no record; "
        "prior failure remains fail; forged owner path is unadmitted; cleanup is signal-atomic"
    )


if __name__ == "__main__":
    main()
