#!/usr/bin/env python3
"""Mutation tests for audit-test-binary-registration.py."""

from __future__ import annotations

import subprocess
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("audit-test-binary-registration.py")


class RegistrationAuditTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary_directory = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary_directory.name)
        (self.root / "hermit-cli/tests/common").mkdir(parents=True)
        (self.root / "ci/dag").mkdir(parents=True)
        (self.root / "hermit-cli/tests/registered.rs").write_text(
            "#[test]\nfn registered() {}\n"
        )
        (self.root / "hermit-cli/tests/unknown.rs").write_text(
            "#[test]\nfn unknown() {}\n"
        )
        # Nested helpers are tracked source, but not top-level Cargo test targets.
        (self.root / "hermit-cli/tests/common/mod.rs").write_text("pub fn helper() {}\n")
        (self.root / "ci/dag/portable.json").write_text(
            '{"steps":[{"cmd":"cargo test -p hermit --test registered"}]}\n'
        )
        (self.root / "ci/undeclared-test-binaries.tsv").write_text(
            "unknown\tnone-recorded\tNo omission reason was recorded.\n"
        )
        subprocess.run(["git", "init", "-q", str(self.root)], check=True)
        subprocess.run(["git", "-C", str(self.root), "add", "."], check=True)

    def tearDown(self) -> None:
        self.temporary_directory.cleanup()

    def audit(self) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            ["python3", str(SCRIPT), "--root", str(self.root)],
            capture_output=True,
            text=True,
            check=False,
        )

    def test_none_recorded_is_a_distinct_unknown_not_a_pass(self) -> None:
        result = self.audit()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(
            "present=2 ci-registered=1 reason-recorded=0 none-recorded=1 undeclared=0",
            result.stdout,
        )
        self.assertIn("ACCOUNTED-WITH-UNKNOWN", result.stdout)
        self.assertNotIn("PASS", result.stdout)

    def test_new_tracked_top_level_binary_is_refused_and_named(self) -> None:
        probe = self.root / "hermit-cli/tests/zz_unregistered_probe.rs"
        probe.write_text("#[test]\nfn probe() {}\n")
        subprocess.run(
            ["git", "-C", str(self.root), "add", str(probe.relative_to(self.root))],
            check=True,
        )

        result = self.audit()

        self.assertEqual(result.returncode, 2)
        self.assertIn("hermit-cli/tests/zz_unregistered_probe.rs", result.stderr)

    def test_concrete_declaration_accounts_for_planted_binary(self) -> None:
        probe = self.root / "hermit-cli/tests/zz_unregistered_probe.rs"
        probe.write_text("#[test]\nfn probe() {}\n")
        with (self.root / "ci/undeclared-test-binaries.tsv").open("a") as ledger:
            ledger.write("zz_unregistered_probe\tmanual-only\tFixture declaration.\n")
        subprocess.run(["git", "-C", str(self.root), "add", "."], check=True)

        result = self.audit()

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("reason-recorded=1 none-recorded=1 undeclared=0", result.stdout)

    # ------------------------------------------------------------------
    # FALSE REGISTRATION. Every case above plants a binary that is WHOLLY absent
    # from the DAG, so all of them pass against an auditor that accepts any text
    # resembling an invocation. These plant the text WITHOUT the execution: the
    # binary must still be reported undeclared, or the ledger can be satisfied by
    # a command that never runs.
    # ------------------------------------------------------------------

    def _plant_probe_with_dag_command(self, command: str) -> subprocess.CompletedProcess[str]:
        probe = self.root / "hermit-cli/tests/zz_probe.rs"
        probe.write_text("#[test]\nfn probe() {}\n")
        (self.root / "ci/dag/portable.json").write_text(
            '{"steps":[{"cmd":"cargo test -p hermit --test registered"},'
            f'{{"cmd":{command!r}}}]}}\n'.replace("'", '"')
        )
        subprocess.run(["git", "-C", str(self.root), "add", "."], check=True)
        return self.audit()

    def test_echoed_invocation_does_not_register_a_binary(self) -> None:
        result = self._plant_probe_with_dag_command(
            "echo cargo test -p hermit --test zz_probe"
        )
        self.assertEqual(result.returncode, 2, result.stdout)
        self.assertIn("zz_probe", result.stderr)

    def test_no_run_invocation_does_not_register_a_binary(self) -> None:
        result = self._plant_probe_with_dag_command(
            "cargo test -p hermit --test zz_probe --no-run"
        )
        self.assertEqual(result.returncode, 2, result.stdout)
        self.assertIn("zz_probe", result.stderr)

    def test_nextest_run_registers_a_binary(self) -> None:
        result = self._plant_probe_with_dag_command(
            "CARGO_BUILD_JOBS=8 ./ci/run-with-reverie-dbt-budget.sh "
            "./ci/run-nextest-counted.sh ${CI:+--profile ci} -p hermit --test zz_probe -j 1"
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("ci-registered=2", result.stdout)

    def test_nextest_no_run_does_not_register_a_binary(self) -> None:
        result = self._plant_probe_with_dag_command(
            "cargo nextest run -p hermit --test zz_probe --no-run"
        )
        self.assertEqual(result.returncode, 2, result.stdout)
        self.assertIn("zz_probe", result.stderr)

    def test_invocation_named_only_in_a_description_does_not_register(self) -> None:
        probe = self.root / "hermit-cli/tests/zz_probe.rs"
        probe.write_text("#[test]\nfn probe() {}\n")
        (self.root / "ci/dag/portable.json").write_text(
            '{"steps":[{"cmd":"cargo test -p hermit --test registered",'
            '"desc":"unlike cargo test -p hermit --test zz_probe, which we skip"}]}\n'
        )
        subprocess.run(["git", "-C", str(self.root), "add", "."], check=True)

        result = self.audit()

        self.assertEqual(result.returncode, 2, result.stdout)
        self.assertIn("zz_probe", result.stderr)

    def test_wrapper_and_env_prefixed_invocation_still_registers(self) -> None:
        """The positive leg: the tightening must not reject Hermit's real shapes."""
        result = self._plant_probe_with_dag_command(
            "CARGO_BUILD_JOBS=8 ./ci/run-with-reverie-dbt-budget.sh "
            "cargo test -p hermit --features third-party-backends --test zz_probe"
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("ci-registered=2", result.stdout)


if __name__ == "__main__":
    unittest.main()
