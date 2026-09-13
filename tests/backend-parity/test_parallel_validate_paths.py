#!/usr/bin/env python3
"""Collision controls for backend-parity host temporary directories."""

from __future__ import annotations

import importlib.util
import json
import os
import select
import subprocess
import sys
import tempfile
import threading
import unittest
from importlib.machinery import SourceFileLoader
from pathlib import Path


HERE = Path(__file__).resolve().parent


def load(name: str, filename: str):
    loader = SourceFileLoader(name, str(HERE / filename))
    spec = importlib.util.spec_from_loader(name, loader)
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    loader.exec_module(module)
    return module


class BackendParityTemporaryPathTest(unittest.TestCase):
    def test_infrastructure_receipt_keeps_its_cause_beside_the_gap_tier(self) -> None:
        run_matrix = load("infrastructure_receipt_run_matrix", "run_matrix.py")
        with tempfile.TemporaryDirectory() as raw:
            path = Path(raw) / "verdict.json"
            path.write_text(
                json.dumps(
                    {
                        "verified": False,
                        "bitwise_parity": False,
                        "verdict": "infrastructure_error",
                        "infrastructure_error": {
                            "kind": "skid_overshoot",
                            "count": 2,
                        },
                        "comparison": None,
                        "compared_log_messages": None,
                        "guest_exit_code": None,
                        "guest_signal": None,
                        "first_divergent_scheduler_turn": None,
                        "first_divergent_virtual_nanoseconds": None,
                        "first_divergent_record": None,
                        "first_divergent_syscall": None,
                        "first_divergent_left_message": None,
                        "first_divergent_right_message": None,
                    }
                )
            )
            receipt = run_matrix.verify_tier_from_json(path)
        self.assertIsNotNone(receipt)
        self.assertEqual(receipt["tier"], "gap")
        self.assertEqual(
            receipt["infrastructure_error"],
            "verification recorded 2 HERMIT_SKID_OVERSHOOT report(s)",
        )

    def test_old_host_tmp_overwrites_and_all_commands_accept_private_roots(self) -> None:
        run_matrix = load("parallel_run_matrix", "run_matrix.py")
        e9patch = load("parallel_e9patch_corpus", "e9patch_corpus.py")
        mutation = load("parallel_parity_mutation", "parity_mutation.py")

        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            old_tmp = root / "old-host-tmp"
            old_tmp.mkdir()
            old_child = old_tmp / "hermit-file-io-fixed"

            # Removed behavior: both top-level validates bound host /tmp.  The
            # same guest-relative name therefore denotes one host file, and the
            # second run replaces the first result.
            first_written = threading.Event()
            second_written = threading.Event()

            def first_run() -> None:
                old_child.write_text("run-a")
                first_written.set()
                second_written.wait(timeout=2)

            def second_run() -> None:
                if not first_written.wait(timeout=2):
                    return
                old_child.write_text("run-b")
                second_written.set()

            first = threading.Thread(target=first_run)
            second = threading.Thread(target=second_run)
            first.start()
            second.start()
            first.join(timeout=2)
            second.join(timeout=2)
            self.assertFalse(first.is_alive())
            self.assertFalse(second.is_alive())
            self.assertTrue(second_written.is_set())
            self.assertEqual(old_child.read_text(), "run-b")

            run_a = root / "run-a"
            run_b = root / "run-b"
            fixtures_a = run_matrix.Fixtures(run_a)
            fixtures_b = run_matrix.Fixtures(run_b)
            tmp_a = fixtures_a.host_tmp("ptrace", "file-io")
            tmp_b = fixtures_b.host_tmp("ptrace", "file-io")
            self.assertNotEqual(tmp_a, tmp_b)
            child_a = tmp_a / old_child.name
            child_b = tmp_b / old_child.name
            child_a.write_text("run-a")
            child_b.write_text("run-b")
            self.assertEqual(child_a.read_text(), "run-a")
            self.assertEqual(child_b.read_text(), "run-b")

            fixture = run_a / "compiled-fixture"
            fixture.write_bytes(b"fixture-bytes")
            fixture.chmod(0o755)
            input_file = run_a / "input.txt"
            input_file.write_bytes(b"input-bytes")
            guest_argv = fixtures_a.expose_tmp_paths(
                "ptrace", [str(fixture), str(input_file), "argument"], tmp_a
            )
            staged = tmp_a / fixture.relative_to("/tmp")
            staged_input = tmp_a / input_file.relative_to("/tmp")
            self.assertEqual(
                guest_argv,
                [str(fixture), str(input_file), "argument"],
            )
            self.assertEqual(staged.read_bytes(), b"fixture-bytes")
            self.assertTrue(os.access(staged, os.X_OK))
            self.assertEqual(staged_input.read_bytes(), b"input-bytes")
            self.assertEqual(
                fixtures_a.expose_tmp_paths("ptrace", ["/bin/echo", "hello"], tmp_b),
                ["/bin/echo", "hello"],
            )

            dbt_tmp = fixtures_a.host_tmp("dbt", "compiled-fixture")
            self.assertEqual(
                fixtures_a.expose_tmp_paths(
                    "dbt", [str(fixture), str(input_file)], dbt_tmp
                ),
                [str(fixture), str(input_file)],
            )
            self.assertEqual(
                (dbt_tmp / fixture.relative_to("/tmp")).read_bytes(),
                b"fixture-bytes",
            )

            escaped = tmp_a.parent / "etc" / "hosts"
            self.assertEqual(
                fixtures_a.expose_tmp_paths(
                    "ptrace", ["/tmp/../etc/hosts"], tmp_a
                ),
                ["/tmp/../etc/hosts"],
            )
            self.assertFalse(escaped.exists())

            guarded_tmp = fixtures_a.host_tmp("ptrace", "symlink-escape")
            outside = root / "outside-staging-root"
            outside.mkdir()
            (guarded_tmp / root.name).symlink_to(outside, target_is_directory=True)
            with self.assertRaisesRegex(run_matrix.MatrixError, "outside"):
                fixtures_a.expose_tmp_paths("ptrace", [str(fixture)], guarded_tmp)
            self.assertFalse((outside / "run-a" / fixture.name).exists())

            hermit = Path("/hermit")
            guest = Path("/guest")
            matrix_a = run_matrix.hermit_command(
                hermit, "ptrace", [str(guest)], "file-io", True, tmp_a
            )
            matrix_b = run_matrix.hermit_command(
                hermit, "ptrace", [str(guest)], "file-io", True, tmp_b
            )
            e9_a = e9patch.hermit_command(hermit, False, False, guest, tmp_a)
            e9_b = e9patch.hermit_command(hermit, False, False, guest, tmp_b)
            mutation_a = mutation._hermit_command(hermit, "ptrace", guest, None, tmp_a)
            mutation_b = mutation._hermit_command(hermit, "ptrace", guest, None, tmp_b)
            for command_a, command_b in (
                (matrix_a, matrix_b),
                (e9_a, e9_b),
                (mutation_a, mutation_b),
            ):
                self.assertIn(f"--tmp={tmp_a}", command_a)
                self.assertIn(f"--tmp={tmp_b}", command_b)
                self.assertNotIn("--tmp=/tmp", command_a)
                self.assertNotIn("--tmp=/tmp", command_b)

            dbt = run_matrix.hermit_command(
                hermit, "dbt", [str(fixture)], "file-io", True, dbt_tmp
            )
            self.assertIn("--tmp=/tmp", dbt)
            self.assertIn("--env=TMPDIR=/tmp", dbt)
            self.assertIn(str(dbt_tmp), dbt)

            # Exercise the complete generated command with its Hermit path and
            # sibling resources beneath the host /tmp that the wrapper hides.
            # The verify output directory is separate and must remain visible
            # too. This is the layout used by a /tmp validation worktree.
            release = root / "target" / "release"
            install = root / "target" / "install_pkg"
            release.mkdir(parents=True)
            resources = install / "rsrcs"
            resources.mkdir(parents=True)
            (resources / "marker").write_text("resource-ok\n")
            fake_hermit = release / "hermit"
            fake_hermit.write_text(
                "#!/bin/sh\n"
                "set -eu\n"
                "test \"$TMPDIR\" = /tmp\n"
                "test \"$(cat \"$(dirname \"$0\")/../install_pkg/rsrcs/marker\")\" "
                "= resource-ok\n"
                "for arg do\n"
                "  case $arg in\n"
                "    --verify-json=*) printf '{}\\n' > \"${arg#*=}\" ;;\n"
                "  esac\n"
                "done\n"
                "printf 'fake-hermit-ok\\n'\n"
            )
            fake_hermit.chmod(0o755)
            verify_dir = root / "verify-output"
            verify_dir.mkdir()
            verify_json = verify_dir / "verdict.json"
            under_tmp = run_matrix.hermit_command(
                fake_hermit,
                "dbt",
                [str(fixture)],
                "file-io",
                True,
                fixtures_a.host_tmp("dbt", "under-tmp-hermit"),
                verify=True,
                verify_json=verify_json,
            )
            executed = subprocess.run(
                under_tmp,
                stdin=subprocess.DEVNULL,
                capture_output=True,
                text=True,
                timeout=10,
                check=False,
            )
            self.assertEqual(executed.returncode, 0, executed.stderr)
            self.assertEqual(executed.stdout, "fake-hermit-ok\n")
            self.assertEqual(verify_json.read_text(), "{}\n")

            # Execute the wrapper twice in parallel. Both guests ignore TMPDIR
            # and use the same fixed /tmp name; without separate mount
            # namespaces, the second write deterministically corrupts one run.
            fixed_name = "hermit-backend-parity-fixed-collision"

            def collision_command(root: Path, value: str) -> subprocess.Popen[str]:
                command = run_matrix.command_in_private_tmp(
                    [
                        "/bin/sh",
                        "-ceu",
                        'test "$TMPDIR" = /tmp; '
                        f'printf "%s\\n" "$VALUE" > /tmp/{fixed_name}; '
                        'printf "ready\\n"; IFS= read -r release; '
                        f'test "$(cat /tmp/{fixed_name})" = "$VALUE"',
                    ],
                    root,
                )
                environment = os.environ.copy()
                environment["TMPDIR"] = "/tmp/ignored-by-fixture"
                environment["VALUE"] = value
                return subprocess.Popen(
                    command,
                    stdin=subprocess.PIPE,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    text=True,
                    env=environment,
                )

            process_a = collision_command(tmp_a, "run-a")
            process_b = collision_command(tmp_b, "run-b")
            processes = (process_a, process_b)
            try:
                for process in processes:
                    ready, _, _ = select.select([process.stdout], [], [], 5)
                    self.assertTrue(ready, "private /tmp command did not become ready")
                    self.assertEqual(process.stdout.readline(), "ready\n")
                for process in processes:
                    process.stdin.write("release\n")
                    process.stdin.flush()
                for process in processes:
                    stdout, stderr = process.communicate(timeout=10)
                    self.assertEqual(
                        process.returncode,
                        0,
                        f"private /tmp command failed: stdout={stdout!r} stderr={stderr!r}",
                    )
            finally:
                for process in processes:
                    if process.poll() is None:
                        process.kill()
                        process.wait(timeout=5)
            self.assertEqual((tmp_a / fixed_name).read_text(), "run-a\n")
            self.assertEqual((tmp_b / fixed_name).read_text(), "run-b\n")


if __name__ == "__main__":
    unittest.main()
