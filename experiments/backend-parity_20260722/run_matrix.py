#!/usr/bin/env python3
"""Run and ratchet Hermit's cross-backend compatibility matrix."""

from __future__ import annotations

import argparse
import csv
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time


SCRIPT_DIR = Path(__file__).resolve().parent
REPOSITORY = SCRIPT_DIR.parent.parent
MATRIX_PATH = SCRIPT_DIR / "matrix.tsv"
BACKENDS = ("ptrace", "dbi", "kvm")
RUNS = 3


class MatrixError(Exception):
    """An invalid matrix or failed regression contract."""


def read_matrix() -> list[dict[str, str]]:
    with MATRIX_PATH.open(newline="", encoding="utf-8") as matrix_file:
        rows = list(csv.DictReader(matrix_file, delimiter="\t"))

    required = {
        "test_name",
        "ptrace",
        "dbi",
        "kvm",
        "dbi_reason",
        "kvm_reason",
    }
    if not rows or set(rows[0]) != required:
        raise MatrixError(f"{MATRIX_PATH} must contain columns {sorted(required)}")

    names: set[str] = set()
    for row in rows:
        name = row["test_name"]
        if not name or name in names:
            raise MatrixError(f"duplicate or empty test name: {name!r}")
        names.add(name)
        if row["ptrace"] != "pass":
            raise MatrixError(f"{name}: ptrace is the baseline and must be pass")
        for backend in BACKENDS:
            if row[backend] not in {"pass", "gap"}:
                raise MatrixError(f"{name}/{backend}: expected pass or gap")
            if backend != "ptrace":
                reason = row[f"{backend}_reason"]
                if row[backend] == "gap" and reason in {"", "-"}:
                    raise MatrixError(f"{name}/{backend}: gap needs a reason")
                if row[backend] == "pass" and reason != "-":
                    raise MatrixError(
                        f"{name}/{backend}: passing pair reason must be -"
                    )
    return rows


def compile_fixture(source: Path, output: Path, *flags: str) -> Path:
    compiler = shutil.which(os.environ.get("CC", "cc"))
    if compiler is None:
        raise MatrixError("C compiler unavailable (set CC or install cc)")
    command = [
        compiler,
        "-O2",
        "-g",
        "-std=c11",
        "-Wall",
        "-Wextra",
        "-Werror",
        *flags,
        str(source),
        "-o",
        str(output),
    ]
    result = subprocess.run(command, capture_output=True, text=True, check=False)
    if result.returncode != 0:
        raise MatrixError(
            f"fixture compilation failed: {command!r}\n{result.stdout}{result.stderr}"
        )
    return output


class Fixtures:
    def __init__(self, root: Path) -> None:
        self.root = root
        self._binaries: dict[str, Path] = {}

    def binary(self, name: str) -> Path:
        if name in self._binaries:
            return self._binaries[name]

        local = SCRIPT_DIR / "fixtures"
        sources: dict[str, tuple[Path, tuple[str, ...]]] = {
            "pthread_lifecycle": (local / "pthread_lifecycle.c", ("-pthread",)),
            "cpuid_probe": (local / "cpuid_probe.c", ()),
            "clock_determinism": (
                REPOSITORY / "tests/c/clock_determinism.c",
                ("-D_GNU_SOURCE",),
            ),
            "random_sources": (
                REPOSITORY / "tests/c/random_sources.c",
                ("-pthread",),
            ),
            "pid_probe": (local / "pid_probe.c", ()),
        }
        source, flags = sources[name]
        binary = compile_fixture(source, self.root / name, *flags)
        self._binaries[name] = binary
        return binary


def case_command(name: str, fixtures: Fixtures) -> tuple[list[str], int, bytes | None]:
    fixture_input = SCRIPT_DIR / "fixtures/input.txt"
    cases: dict[str, tuple[list[str], int, bytes | None]] = {
        "hello_stdout": (["/bin/echo", "hello world"], 0, b"hello world\n"),
        "argument_forwarding": (
            ["/usr/bin/printf", "%s|%s\n", "alpha", "two words"],
            0,
            b"alpha|two words\n",
        ),
        "exit_zero": (["/bin/true"], 0, b""),
        "exit_status": (["/bin/sh", "-c", "exit 23"], 23, b""),
        "file_read": (["/bin/cat", str(fixture_input)], 0, fixture_input.read_bytes()),
        "pthread_lifecycle": (
            [str(fixtures.binary("pthread_lifecycle"))],
            0,
            b"threads=4 total=10\n",
        ),
        "cpuid_policy": (
            [str(fixtures.binary("cpuid_probe"))],
            0,
            b"CPUID-SUCCESS vendor=GenuineIntel signature=00000663\n",
        ),
        "virtual_clock": ([str(fixtures.binary("clock_determinism"))], 0, None),
        "random_sources": ([str(fixtures.binary("random_sources"))], 0, None),
        "virtual_pid": ([str(fixtures.binary("pid_probe"))], 0, None),
    }
    try:
        return cases[name]
    except KeyError as error:
        raise MatrixError(f"matrix has no implementation for {name}") from error


def backend_block(backend: str) -> str | None:
    if backend == "dbi":
        missing = [
            name
            for name in ("DYNAMORIO_HOME", "HERMIT_DRRUN", "HERMIT_DBI_CLIENT")
            if not os.environ.get(name)
        ]
        if missing:
            return "missing " + ", ".join(missing)
        for name in ("HERMIT_DRRUN", "HERMIT_DBI_CLIENT"):
            if not Path(os.environ[name]).is_file():
                return f"{name} does not name a file: {os.environ[name]}"
    elif backend == "kvm":
        kvm = Path("/dev/kvm")
        if not kvm.exists() or not os.access(kvm, os.R_OK | os.W_OK):
            return "/dev/kvm is not readable and writable"
    return None


def hermit_command(
    hermit: Path, backend: str, guest: list[str], name: str
) -> list[str]:
    command = [str(hermit), "run"]
    if backend != "ptrace":
        command.extend(["--backend", backend])
    command.extend(
        [
            "--base-env=minimal",
            "--preemption-timeout=disabled",
            "--tmp=/tmp",
        ]
    )
    if backend == "ptrace" and name != "cpuid_policy":
        command.append("--no-virtualize-cpuid")
    command.extend(["--", *guest])
    return command


def run_case(
    hermit: Path, backend: str, name: str, fixtures: Fixtures
) -> tuple[str, str, float]:
    guest, expected_status, expected_stdout = case_command(name, fixtures)
    baseline: bytes | None = None
    started = time.monotonic()
    for iteration in range(RUNS):
        command = hermit_command(hermit, backend, guest, name)
        try:
            result = subprocess.run(
                command, capture_output=True, timeout=30, check=False
            )
        except subprocess.TimeoutExpired:
            return "FAIL", f"run {iteration + 1} timed out", time.monotonic() - started

        if result.returncode != expected_status:
            diagnostic = result.stderr.decode(errors="replace").strip()
            if (
                backend == "ptrace"
                and name == "cpuid_policy"
                and (
                    "continuing without CPUID interception" in diagnostic
                    or "CPUID faulting is unavailable" in diagnostic
                )
            ):
                return (
                    "BLOCKED",
                    "host kernel/CPU lacks CPUID faulting",
                    time.monotonic() - started,
                )
            return (
                "FAIL",
                f"run {iteration + 1} exited {result.returncode}, expected "
                f"{expected_status}: {diagnostic[-300:]}",
                time.monotonic() - started,
            )
        if expected_stdout is not None and result.stdout != expected_stdout:
            return (
                "FAIL",
                f"run {iteration + 1} stdout={result.stdout!r}, expected={expected_stdout!r}",
                time.monotonic() - started,
            )
        if expected_stdout is None:
            required_markers = {
                "virtual_clock": b"clock matrix success\n",
                "random_sources": b"getrandom[0]=",
                "virtual_pid": b"pid=",
            }
            marker = required_markers[name]
            if marker not in result.stdout:
                return (
                    "FAIL",
                    f"run {iteration + 1} omitted marker {marker!r}",
                    time.monotonic() - started,
                )
            if baseline is None:
                baseline = result.stdout
            elif result.stdout != baseline:
                return (
                    "FAIL",
                    f"run {iteration + 1} output differed from run 1",
                    time.monotonic() - started,
                )
    return "PASS", f"{RUNS}/{RUNS} runs matched", time.monotonic() - started


def write_results(path: Path, results: list[dict[str, str]]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", newline="", encoding="utf-8") as output:
        writer = csv.DictWriter(
            output,
            fieldnames=(
                "test_name",
                "backend",
                "expectation",
                "result",
                "seconds",
                "detail",
            ),
            delimiter="\t",
        )
        writer.writeheader()
        writer.writerows(results)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--backend",
        action="append",
        choices=BACKENDS,
        dest="backends",
        help="backend to run (repeatable; default: all)",
    )
    parser.add_argument(
        "--check",
        action="store_true",
        help="validate the matrix and print ratchet rates without running guests",
    )
    parser.add_argument(
        "--hermit",
        type=Path,
        default=REPOSITORY / "target/debug/hermit",
        help="Hermit executable",
    )
    parser.add_argument("--output", type=Path, help="write observed result TSV")
    parser.add_argument(
        "--probe-gaps",
        action="store_true",
        help="run documented gaps and report XPASS candidates",
    )
    parser.add_argument(
        "--require-backend",
        action="store_true",
        help="fail instead of reporting BLOCKED when a selected backend is unavailable",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    rows = read_matrix()
    backends = args.backends or list(BACKENDS)
    baseline = sum(row["ptrace"] == "pass" for row in rows)
    for backend in BACKENDS:
        passing = sum(row[backend] == "pass" for row in rows)
        print(f"RATCHET {backend}: {passing}/{baseline} ({passing / baseline:.1%})")
    if args.check:
        return 0

    hermit = args.hermit.resolve()
    if not hermit.is_file() or not os.access(hermit, os.X_OK):
        raise MatrixError(f"Hermit executable is unavailable: {hermit}")

    results: list[dict[str, str]] = []
    failures = 0
    with tempfile.TemporaryDirectory(prefix="hermit-backend-parity-") as tempdir:
        fixtures = Fixtures(Path(tempdir))
        for backend in backends:
            block = backend_block(backend)
            if block:
                print(f"BLOCKED {backend}: {block}")
                if args.require_backend:
                    failures += 1
                continue

            for row in rows:
                name = row["test_name"]
                expectation = row[backend]
                if expectation == "gap" and not args.probe_gaps:
                    reason = row[f"{backend}_reason"]
                    print(f"GAP {backend}/{name}: {reason}")
                    results.append(
                        {
                            "test_name": name,
                            "backend": backend,
                            "expectation": expectation,
                            "result": "GAP",
                            "seconds": "0.000",
                            "detail": reason,
                        }
                    )
                    continue

                status, detail, duration = run_case(hermit, backend, name, fixtures)
                if expectation == "gap" and status == "PASS":
                    status = "XPASS"
                    detail = "candidate for promotion from gap to pass"
                print(f"{status} {backend}/{name}: {detail}")
                results.append(
                    {
                        "test_name": name,
                        "backend": backend,
                        "expectation": expectation,
                        "result": status,
                        "seconds": f"{duration:.3f}",
                        "detail": detail,
                    }
                )
                if expectation == "pass" and status == "FAIL":
                    failures += 1

    if args.output:
        write_results(args.output, results)
    return 1 if failures else 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except MatrixError as error:
        print(f"ERROR: {error}", file=sys.stderr)
        sys.exit(2)
