#!/usr/bin/env python3
"""Mutation tests for audit-test-binary-registration.py."""

from __future__ import annotations

import subprocess
import tempfile
import unittest
import json
import os
import shutil
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
        (self.root / "ci/dag/validate.json").write_text(
            '{"steps":[{"group":"test","job":"registered",'
            '"cmd":"cargo test -p hermit --test registered",'
            '"integration_test_binaries":["registered"]}]}\n'
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

    def audit_json(self) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            ["python3", str(SCRIPT), "--root", str(self.root), "--json"],
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

    def test_json_names_every_member_of_the_partition(self) -> None:
        result = self.audit_json()
        self.assertEqual(result.returncode, 0, result.stderr)
        evidence = json.loads(result.stdout)
        self.assertEqual(evidence["schema"], 1)
        self.assertEqual(evidence["present"], ["registered", "unknown"])
        self.assertEqual(evidence["ci_registered"], ["registered"])
        self.assertEqual(evidence["reason_recorded"], [])
        self.assertEqual(evidence["none_recorded"], ["unknown"])
        self.assertEqual(evidence["undeclared"], [])

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

    def _plant_probe_with_dag_command(
        self, command: str, *, declared: list[str] | None = None
    ) -> subprocess.CompletedProcess[str]:
        probe = self.root / "hermit-cli/tests/zz_probe.rs"
        probe.write_text("#[test]\nfn probe() {}\n")
        probe_step: dict[str, object] = {
            "group": "test",
            "job": "probe",
            "cmd": command,
        }
        if declared is not None:
            probe_step["integration_test_binaries"] = declared
        (self.root / "ci/dag/validate.json").write_text(
            json.dumps(
                {
                    "steps": [
                        {
                            "group": "test",
                            "job": "registered",
                            "cmd": "cargo test -p hermit --test registered",
                            "integration_test_binaries": ["registered"],
                        },
                        probe_step,
                    ]
                }
            )
            + "\n"
        )
        subprocess.run(["git", "-C", str(self.root), "add", "."], check=True)
        return self.audit()

    def test_echoed_invocation_does_not_register_a_binary(self) -> None:
        result = self._plant_probe_with_dag_command(
            "echo cargo test -p hermit --test zz_probe", declared=["zz_probe"]
        )
        self.assertEqual(result.returncode, 2, result.stdout)
        self.assertIn("integration_test_binaries", result.stderr)

    def test_no_run_invocation_does_not_register_a_binary(self) -> None:
        result = self._plant_probe_with_dag_command(
            "cargo test -p hermit --test zz_probe --no-run", declared=["zz_probe"]
        )
        self.assertEqual(result.returncode, 2, result.stdout)
        self.assertIn("integration_test_binaries", result.stderr)

    def test_nextest_run_registers_a_binary(self) -> None:
        result = self._plant_probe_with_dag_command(
            "CARGO_BUILD_JOBS=8 ./ci/run-with-reverie-dbt-budget.sh "
            "./ci/run-nextest-counted.sh ${CI:+--profile ci} -p hermit --test zz_probe -j 1",
            declared=["zz_probe"],
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("ci-registered=2", result.stdout)

    def test_nextest_no_run_does_not_register_a_binary(self) -> None:
        result = self._plant_probe_with_dag_command(
            "cargo nextest run -p hermit --test zz_probe --no-run",
            declared=["zz_probe"],
        )
        self.assertEqual(result.returncode, 2, result.stdout)
        self.assertIn("integration_test_binaries", result.stderr)

    def test_invocation_named_only_in_a_description_does_not_register(self) -> None:
        probe = self.root / "hermit-cli/tests/zz_probe.rs"
        probe.write_text("#[test]\nfn probe() {}\n")
        (self.root / "ci/dag/validate.json").write_text(
            '{"steps":[{"group":"test","job":"registered",'
            '"cmd":"cargo test -p hermit --test registered",'
            '"integration_test_binaries":["registered"],'
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
            "cargo test -p hermit --features third-party-backends --test zz_probe",
            declared=["zz_probe"],
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("ci-registered=2", result.stdout)

    def test_prlimit_command_registers_the_wrapped_test_binary(self) -> None:
        result = self._plant_probe_with_dag_command(
            'test "${HERMIT_E2E_EMPTY_WORKDIR:-}" = /test && '
            "prlimit --fsize=67108864:67108864 -- "
            "./ci/run-with-reverie-dbt-budget.sh ./ci/run-nextest-counted.sh "
            "${CI:+--profile ci} -p hermit --features third-party-backends "
            "--test zz_probe -E 'test(=positive) | test(=negative)' -- --include-ignored",
            declared=["zz_probe"],
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("ci-registered=2", result.stdout)


    def test_prlimit_does_not_register_nonexecuting_or_malformed_commands(self) -> None:
        invocation = "./ci/run-nextest-counted.sh -p hermit --test zz_probe"
        for command in (
            f"echo prlimit --fsize=67108864:67108864 -- {invocation}",
            f"unrelated prlimit --fsize=67108864:67108864 -- {invocation}",
            f"prlimit --fsize=67108864:67108864 -- echo {invocation}",
            f"prlimit --fsize=67108864:67108864 -- {invocation} --no-run",
            f"prlimit --fsize=67108864:67108864 {invocation}",
            f"prlimit -- {invocation}",
            f"prlimit --pid=1 -- {invocation}",
            f"prlimit --help -- {invocation}",
            f"prlimit --fsize=malformed -- {invocation}",
            f"prlimit --fsize=67108865:67108864 -- {invocation}",
            f"prlimit --fsize=18446744073709551616:18446744073709551616 -- {invocation}",
            f"prlimit --fsize=67108864:67108864 -- --help {invocation}",
            f"prlimit --fsize=67108864:67108864 -- HERMIT_REVIEW_ASSIGNMENT=1 {invocation}",
            f"prlimit --fsize=67108864:67108864 -- 30 {invocation}",
            f"prlimit --fsize=67108864:67108864 -- env --help {invocation}",
        ):
            with self.subTest(command=command):
                result = self._plant_probe_with_dag_command(
                    command, declared=["zz_probe"]
                )
                self.assertEqual(result.returncode, 2, result.stdout)
                self.assertIn("integration_test_binaries", result.stderr)


    def test_prlimit_assignments_require_shell_position_or_explicit_env(self) -> None:
        invocation = "./ci/run-nextest-counted.sh -p hermit --test zz_probe"
        for command in (
            f"CARGO_BUILD_JOBS=1 prlimit --fsize=67108864:67108864 -- {invocation}",
            f"prlimit --fsize=67108864:67108864 -- env CARGO_BUILD_JOBS=1 {invocation}",
        ):
            with self.subTest(command=command):
                result = self._plant_probe_with_dag_command(
                    command, declared=["zz_probe"]
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("ci-registered=2", result.stdout)


    def test_prlimit_refuses_unsupported_nested_runner_forms(self) -> None:
        invocation = "./ci/run-nextest-counted.sh -p hermit --test zz_probe"
        for prefix in ("timeout 300 FOO=1", "nice FOO=1", "timeout 300", "nice"):
            with self.subTest(prefix=prefix):
                # The first two never execute the test command. The latter two
                # are valid wrappers whose operand grammar is not supported by
                # this audit, so they must not silently gain coverage credit.
                result = self._plant_probe_with_dag_command(
                    f"prlimit --fsize=67108864:67108864 -- {prefix} {invocation}",
                    declared=["zz_probe"],
                )
                self.assertEqual(result.returncode, 2, result.stdout)
                self.assertIn("integration_test_binaries", result.stderr)


    def test_prlimit_cannot_execute_a_shell_exec_builtin(self) -> None:
        invocation = "./ci/run-nextest-counted.sh -p hermit --test zz_probe"
        prlimit = shutil.which("prlimit")
        env_command = shutil.which("env")
        self.assertIsNotNone(prlimit)
        self.assertIsNotNone(env_command)
        environment = dict(os.environ, PATH=str(self.root))
        for prefix, argv in (
            ("exec", ["exec"]),
            ("env CARGO_BUILD_JOBS=1 exec", [env_command, "CARGO_BUILD_JOBS=1", "exec"]),
        ):
            with self.subTest(prefix=prefix):
                result = self._plant_probe_with_dag_command(
                    f"prlimit --fsize=67108864:67108864 -- {prefix} {invocation}",
                    declared=["zz_probe"],
                )
                self.assertEqual(result.returncode, 2, result.stdout)
                self.assertIn("integration_test_binaries", result.stderr)
                real = subprocess.run(
                    [prlimit, "--fsize=67108864:67108864", "--", *argv, "/bin/true"],
                    env=environment,
                    capture_output=True,
                    text=True,
                    check=False,
                )
                self.assertEqual(real.returncode, 127, real.stderr)


    def test_shell_exec_before_prlimit_still_executes_the_test_command(self) -> None:
        result = self._plant_probe_with_dag_command(
            "exec prlimit --fsize=67108864:67108864 -- "
            "./ci/run-nextest-counted.sh -p hermit --test zz_probe",
            declared=["zz_probe"],
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("ci-registered=2", result.stdout)
        real = subprocess.run(
            [
                "/bin/sh", "-c", 'exec "$1" --fsize=67108864:67108864 -- /bin/true',
                "fixture", shutil.which("prlimit"),
            ],
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(real.returncode, 0, real.stderr)


    def test_prlimit_budget_wrapper_registers_a_binary(self) -> None:
        result = self._plant_probe_with_dag_command(
            "prlimit --fsize=67108864:67108864 -- "
            "./ci/run-with-reverie-dbt-budget.sh ./ci/run-nextest-counted.sh "
            "${CI:+--profile ci} -p hermit --features third-party-backends --test zz_probe",
            declared=["zz_probe"],
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("ci-registered=2", result.stdout)

    def test_prlimit_wrapped_no_run_does_not_register_a_binary(self) -> None:
        result = self._plant_probe_with_dag_command(
            "prlimit --fsize=67108864:67108864 -- "
            "./ci/run-with-reverie-dbt-budget.sh ./ci/run-nextest-counted.sh "
            "-p hermit --test zz_probe --no-run",
            declared=["zz_probe"],
        )
        self.assertEqual(result.returncode, 2, result.stdout)
        self.assertIn("integration_test_binaries", result.stderr)

    def test_prlimit_nonexecuting_modes_do_not_register_a_binary(self) -> None:
        for options in (
            "--help",
            "--version",
            "--fsize=67108864:67108864 --help",
            "--fsize=67108864:67108864 --version",
            "--pid=1",
        ):
            with self.subTest(options=options):
                result = self._plant_probe_with_dag_command(
                    f"prlimit {options} -- ./ci/run-nextest-counted.sh "
                    "-p hermit --test zz_probe",
                    declared=["zz_probe"],
                )
                self.assertEqual(result.returncode, 2, result.stdout)
                self.assertIn("integration_test_binaries", result.stderr)

    def test_prlimit_wrapped_echo_does_not_register_a_binary(self) -> None:
        result = self._plant_probe_with_dag_command(
            "prlimit --fsize=67108864:67108864 -- "
            "./ci/run-with-reverie-dbt-budget.sh echo "
            "cargo test -p hermit --test zz_probe",
            declared=["zz_probe"],
        )
        self.assertEqual(result.returncode, 2, result.stdout)
        self.assertIn("integration_test_binaries", result.stderr)

    def test_executed_target_without_typed_declaration_is_refused_by_name(self) -> None:
        result = self._plant_probe_with_dag_command(
            "cargo test -p hermit --test zz_probe"
        )
        self.assertEqual(result.returncode, 2, result.stdout)
        self.assertIn("step test.probe", result.stderr)
        self.assertIn("omits integration_test_binaries", result.stderr)

    def test_command_and_typed_declaration_must_name_the_same_targets(self) -> None:
        result = self._plant_probe_with_dag_command(
            "cargo test -p hermit --test zz_probe", declared=["registered"]
        )
        self.assertEqual(result.returncode, 2, result.stdout)
        self.assertIn("step test.probe integration_test_binaries", result.stderr)
        self.assertIn("do not match executed targets ['zz_probe']", result.stderr)


if __name__ == "__main__":
    unittest.main()
