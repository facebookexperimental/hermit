#!/usr/bin/env python3
"""Both directions of the transport-versus-verdict distinction in the adapter.

The defect this covers was measured 2026-09-04 on the host recorded for this
measurement in ``docs/TESTING_ENVIRONMENTS.md`` under "Named measurement
hosts": a GitHub ``HTTP 504`` while fetching the pinned check-status authority
made ``make lint-checks`` exit 2, which the validation DAG recorded as
``check.lint_checks`` FAILED. An outage was reported as a code defect on every
lane consulting that authority.

⚠️ BOTH DIRECTIONS ARE REQUIRED AND THE SECOND IS THE ONE THAT GETS SKIPPED. A
checker that stops saying no is worse than one that says no wrongly, because the
first failure is silent.
"""

from __future__ import annotations

import os
from pathlib import Path
import subprocess
import sys
import unittest


SCRIPTS = Path(__file__).resolve().parent
ADAPTER = SCRIPTS / "check_outcome_adapter.py"
AUTHORITY_ORACLE = SCRIPTS / "test-authority-obtained-once.sh"
sys.path.insert(0, str(SCRIPTS))

import check_outcome_adapter as adapter  # noqa: E402


def _run(args: list[str], *, env: dict[str, str] | None = None):
    merged = dict(os.environ)
    if env:
        merged.update(env)
    return subprocess.run(
        [sys.executable, str(ADAPTER), *args],
        capture_output=True,
        text=True,
        check=False,
        env=merged,
    )


def _unreachable_env(tmp: Path) -> dict[str, str]:
    """Make every authority source unavailable without breaking anything else.

    ``DEV_HERMIT_PARENT`` pointing at an empty directory removes the local
    candidate (``_candidate_authorities`` returns exactly that one path when the
    variable is set), and an empty ``PATH`` removes ``gh`` and ``with-proxy`` so
    the fetch cannot start. That is a simulated unreachable authority, not a
    simulated wrong answer.
    """
    return {"DEV_HERMIT_PARENT": str(tmp), "PATH": ""}


def _run_oracle_with_authority_response(tmp: Path, response: str):
    """Run the oracle through its initial fetch with one controlled response."""
    response_bin = tmp / "bin"
    response_bin.mkdir()
    proxy = response_bin / "with-proxy"
    proxy.write_text(
        "#!/usr/bin/env bash\nprintf '%s\\n' \"$AUTHORITY_RESPONSE\" >&2\nexit 1\n"
    )
    proxy.chmod(0o755)
    empty_parent = tmp / "empty"
    empty_parent.mkdir()
    return subprocess.run(
        [str(AUTHORITY_ORACLE)],
        capture_output=True,
        text=True,
        check=False,
        env=dict(
            os.environ,
            DEV_HERMIT_PARENT=str(empty_parent),
            AUTHORITY_RESPONSE=response,
            PATH=f"{response_bin}:{os.environ['PATH']}",
        ),
    )


class AuthorityUnavailableIsNotAVerdict(unittest.TestCase):
    def test_unreachable_authority_yields_could_not_determine(self) -> None:
        import tempfile

        with tempfile.TemporaryDirectory() as tmp:
            result = _run(
                ["--status", "completed", "--conclusion", "failure"],
                env=_unreachable_env(Path(tmp)),
            )

        self.assertEqual(
            result.returncode,
            adapter.EXIT_AUTHORITY_UNAVAILABLE,
            f"stdout={result.stdout!r} stderr={result.stderr!r}",
        )
        self.assertIn("COULD-NOT-DETERMINE", result.stderr)
        self.assertIn("NO SIGNAL", result.stderr)

    def test_unreachable_authority_writes_no_verdict_to_stdout(self) -> None:
        """The distinction dies if a caller can read a token off stdout."""
        import tempfile

        with tempfile.TemporaryDirectory() as tmp:
            result = _run(
                ["--status", "completed", "--conclusion", "failure"],
                env=_unreachable_env(Path(tmp)),
            )

        self.assertEqual(result.stdout, "")
        for verdict in ("PASSED", "FAILED", "NO_RESULT"):
            self.assertNotIn(verdict, result.stdout)

    def test_the_two_failure_kinds_are_different_types(self) -> None:
        """A digest mismatch is a refusal and must not be caught as an outage."""
        self.assertTrue(issubclass(adapter.AuthorityUnavailable, RuntimeError))
        self.assertTrue(issubclass(adapter.AuthorityIntegrityError, RuntimeError))
        self.assertFalse(
            issubclass(adapter.AuthorityIntegrityError, adapter.AuthorityUnavailable)
        )
        self.assertFalse(
            issubclass(adapter.AuthorityUnavailable, adapter.AuthorityIntegrityError)
        )

    def test_exit_status_does_not_collide_with_usage_or_ordinary_error(self) -> None:
        self.assertNotIn(adapter.EXIT_AUTHORITY_UNAVAILABLE, (0, 1, 2))


class AReachableAuthorityStillSaysNo(unittest.TestCase):
    """⚠️ THE DIRECTION THAT GETS SKIPPED. Without this the fix could be a
    checker that never says no, which is strictly worse than the bug."""

    def test_reachable_authority_still_returns_failed(self) -> None:
        result = _run(["--status", "completed", "--conclusion", "failure"])
        if result.returncode == adapter.EXIT_AUTHORITY_UNAVAILABLE:
            self.skipTest(
                "authority genuinely unreachable here; this direction needs a "
                "reachable authority and must not be asserted without one"
            )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), "FAILED")

    def test_reachable_authority_still_returns_passed_and_no_result(self) -> None:
        passed = _run(["--status", "completed", "--conclusion", "success"])
        if passed.returncode == adapter.EXIT_AUTHORITY_UNAVAILABLE:
            self.skipTest("authority genuinely unreachable here")
        self.assertEqual(passed.stdout.strip(), "PASSED")

        cancelled = _run(["--status", "completed", "--conclusion", "cancelled"])
        self.assertEqual(cancelled.returncode, 0, cancelled.stderr)
        self.assertEqual(cancelled.stdout.strip(), "NO_RESULT")




class TheSecondPinnedAuthorityAdapterUsesTheSameSpelling(unittest.TestCase):
    """review_contract_adapter.py fetches a different pinned contract and had
    the identical defect. One spelling across both, so a caller learns it once.
    """

    def test_review_contract_adapter_declares_the_same_two_kinds(self) -> None:
        import review_contract_adapter as review

        self.assertEqual(
            review.EXIT_AUTHORITY_UNAVAILABLE, adapter.EXIT_AUTHORITY_UNAVAILABLE
        )
        self.assertFalse(
            issubclass(review.AuthorityIntegrityError, review.AuthorityUnavailable)
        )

    def test_review_contract_adapter_reports_unreachable_as_exit_three(self) -> None:
        import tempfile

        with tempfile.TemporaryDirectory() as tmp:
            result = subprocess.run(
                [sys.executable, str(SCRIPTS / "review_contract_adapter.py")],
                capture_output=True,
                text=True,
                check=False,
                env=dict(os.environ, DEV_HERMIT_PARENT=tmp, PATH=""),
            )
        self.assertEqual(result.returncode, adapter.EXIT_AUTHORITY_UNAVAILABLE)
        self.assertIn("COULD-NOT-DETERMINE", result.stderr)
        self.assertEqual(result.stdout, "")


class TheProbeHelperReportsOnlyTheOutage(unittest.TestCase):
    """⚠️ The probe is what lets five checkers skip. If it ever returned 3 for
    anything other than an outage, those checkers would skip real failures."""

    def test_probe_says_reachable_when_it_is(self) -> None:
        result = subprocess.run(
            [str(SCRIPTS / "authority-available.sh")],
            capture_output=True, text=True, check=False,
        )
        if result.returncode == 3:
            self.skipTest("authority genuinely unreachable here")
        self.assertEqual(result.returncode, 0)

    def test_probe_says_unreachable_only_on_exit_three(self) -> None:
        """An adapter failing for any OTHER reason must not be read as an outage.

        The helper takes adapter PATHS, so the stub is an executable file rather
        than an interpreter plus a script.
        """
        import stat as stat_module
        import tempfile

        with tempfile.TemporaryDirectory() as tmp:
            stub = Path(tmp) / "ordinary-error-adapter"
            stub.write_text("#!/usr/bin/env python3\nimport sys\nsys.exit(1)\n")
            stub.chmod(stub.stat().st_mode | stat_module.S_IEXEC)
            result = subprocess.run(
                [str(SCRIPTS / "authority-available.sh"), str(stub)],
                capture_output=True, text=True, check=False,
            )

        self.assertNotEqual(
            result.returncode, 3,
            "an ordinary adapter error must not be reported as unreachable, or a "
            "broken adapter would make every guarded checker skip silently",
        )
        self.assertNotEqual(result.returncode, 0, "and it must not be reported as success")

    def test_oracle_startup_reports_transport_failure_as_no_result(self) -> None:
        """The oracle's own authority fetch must preserve the outage direction."""
        import tempfile

        with tempfile.TemporaryDirectory() as tmp:
            result = _run_oracle_with_authority_response(
                Path(tmp), "gh: HTTP 504 Gateway Timeout"
            )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.count("NO-RESULT-CASE:"), 1)

    def test_oracle_startup_does_not_skip_a_reachable_refusal(self) -> None:
        """A reachable authority refusal must remain a lint failure."""
        import tempfile

        with tempfile.TemporaryDirectory() as tmp:
            result = _run_oracle_with_authority_response(
                Path(tmp), "gh: HTTP 403: Resource not accessible by integration"
            )

        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn("NO-RESULT-CASE:", result.stdout)
        self.assertIn("not an outage and is not being skipped", result.stderr)

    def test_one_obtained_authority_serves_every_later_invocation(self) -> None:
        """⚠️ THE RACE THE CODEX LANE FOUND. Obtaining must remove later fetches,
        not merely answer a question about the first one."""
        import tempfile

        probe = subprocess.run(
            [str(SCRIPTS / "authority-available.sh")],
            capture_output=True, text=True, check=False,
        )
        if probe.returncode == 3:
            self.skipTest("authority genuinely unreachable here")
        self.assertEqual(probe.returncode, 0, probe.stderr)
        materialized = Path(probe.stdout.strip())
        try:
            # The directory the caller exports must actually satisfy the adapter
            # with no network available at all -- that is what closes the window.
            result = subprocess.run(
                [sys.executable, str(ADAPTER), "--status", "completed",
                 "--conclusion", "failure"],
                capture_output=True, text=True, check=False,
                env=dict(os.environ, DEV_HERMIT_PARENT=str(materialized), PATH=""),
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout.strip(), "FAILED")
        finally:
            import shutil as shutil_module
            shutil_module.rmtree(materialized, ignore_errors=True)



if __name__ == "__main__":
    unittest.main()
