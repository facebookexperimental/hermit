#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

"""Benchmark native, ptrace, DBI, and KVM execution on common workloads."""

from __future__ import annotations

import argparse
import csv
import hashlib
import json
import math
import os
import platform
import shutil
import signal
import statistics
import subprocess
import sys
import tempfile
import time
from dataclasses import asdict, dataclass
from datetime import datetime, timezone
from pathlib import Path


SCRIPT_DIR = Path(__file__).resolve().parent
REPOSITORY = SCRIPT_DIR.parent.parent
FIXTURE_DIR = SCRIPT_DIR / "fixtures"
MODES = ("native", "ptrace", "dbi", "kvm")
TIME_FORMAT = "\n".join(
    (
        "user_seconds=%U",
        "system_seconds=%S",
        "max_rss_kb=%M",
        "voluntary_context_switches=%w",
        "involuntary_context_switches=%c",
    )
)


class BenchmarkError(Exception):
    """A setup, execution, or result-validation failure."""


@dataclass(frozen=True)
class Workload:
    """One benchmark command and its semantic validation contract."""

    name: str
    category: str
    command: tuple[str, ...]
    output_policy: str = "exact"
    output_marker: bytes | None = None
    operations: int = 0
    reset_kind: str = "none"
    state_path: Path | None = None
    expected_artifacts: int = 0


@dataclass(frozen=True)
class CommandOutcome:
    """Bounded child-process outcome."""

    returncode: int | None
    stdout: bytes
    stderr: bytes
    elapsed_ns: int
    timed_out: bool


@dataclass(frozen=True)
class Compatibility:
    """Preflight result for one workload and execution mode."""

    workload: str
    category: str
    mode: str
    status: str
    exit_code: int | None
    wall_ms: float
    stdout_sha256: str
    detail: str


@dataclass(frozen=True)
class Sample:
    """One timed observation."""

    workload: str
    category: str
    mode: str
    sample: int
    elapsed_ns: int
    user_cpu_ns: int
    system_cpu_ns: int
    max_rss_kb: int
    voluntary_context_switches: int
    involuntary_context_switches: int

    @property
    def context_switches(self) -> int:
        """Return total voluntary plus involuntary context switches."""

        return self.voluntary_context_switches + self.involuntary_context_switches


@dataclass(frozen=True)
class Summary:
    """Descriptive statistics for one passing workload/mode pair."""

    workload: str
    category: str
    mode: str
    samples: int
    median_wall_ms: float
    p95_wall_ms: float
    mean_wall_ms: float
    stddev_wall_ms: float
    median_user_cpu_ms: float
    median_system_cpu_ms: float
    median_cpu_ms: float
    median_max_rss_kb: float
    median_context_switches: float


@dataclass(frozen=True)
class DerivedMetric:
    """Cross-mode latency comparison."""

    metric: str
    category: str
    unit: str
    native: float | None
    ptrace: float | None
    dbi: float | None
    kvm: float | None
    ptrace_over_native: float | None
    dbi_speedup_vs_ptrace: float | None
    kvm_speedup_vs_ptrace: float | None


def positive_integer(value: str) -> int:
    parsed = int(value)
    if parsed <= 0:
        raise argparse.ArgumentTypeError("must be positive")
    return parsed


def nonnegative_integer(value: str) -> int:
    parsed = int(value)
    if parsed < 0:
        raise argparse.ArgumentTypeError("must be nonnegative")
    return parsed


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--hermit",
        type=Path,
        default=REPOSITORY / "target/release/hermit",
        help="release Hermit executable",
    )
    parser.add_argument(
        "--ninja",
        type=Path,
        default=(REPOSITORY / "target/backend-benchmarks/deps/ninja/ninja"),
        help="Ninja executable prepared by prepare_dependencies.sh",
    )
    parser.add_argument(
        "--leveldb-bench",
        type=Path,
        default=(
            REPOSITORY / "target/backend-benchmarks/deps/leveldb-build-bench/db_bench"
        ),
        help="LevelDB db_bench executable prepared by prepare_dependencies.sh",
    )
    parser.add_argument(
        "--mode",
        action="append",
        choices=MODES,
        dest="modes",
        help="execution mode to benchmark (repeatable; default: all)",
    )
    parser.add_argument("--samples", type=positive_integer, default=5)
    parser.add_argument("--warmups", type=nonnegative_integer, default=1)
    parser.add_argument("--timeout", type=float, default=90.0)
    parser.add_argument("--cpu", type=nonnegative_integer)
    parser.add_argument("--output-dir", type=Path)
    parser.add_argument("--syscall-iterations", type=positive_integer, default=2_000)
    parser.add_argument("--fork-iterations", type=positive_integer, default=16)
    parser.add_argument("--bzip2-bytes", type=positive_integer, default=2 * 1024 * 1024)
    parser.add_argument("--ninja-jobs", type=positive_integer, default=32)
    parser.add_argument("--leveldb-operations", type=positive_integer, default=2_000)
    parser.add_argument("--sqlite-rows", type=positive_integer, default=20_000)
    args = parser.parse_args()
    if args.timeout <= 0:
        parser.error("--timeout must be positive")
    return args


def digest(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def executable(path: Path, description: str) -> Path:
    resolved = path.expanduser().resolve()
    if not resolved.is_file() or not os.access(resolved, os.X_OK):
        raise BenchmarkError(
            f"{description} is not executable: {resolved}; "
            "run experiments/backend-benchmarks/prepare_dependencies.sh"
        )
    return resolved


def compile_fixture(source: Path, output: Path) -> None:
    compiler = shutil.which(os.environ.get("CC", "cc"))
    if compiler is None:
        raise BenchmarkError("C compiler unavailable (set CC or install cc)")
    command = [
        compiler,
        "-O2",
        "-std=c11",
        "-D_GNU_SOURCE",
        "-Wall",
        "-Wextra",
        "-Werror",
        str(source),
        "-o",
        str(output),
    ]
    result = subprocess.run(command, capture_output=True, text=True, check=False)
    if result.returncode != 0:
        raise BenchmarkError(
            f"fixture compilation failed: {command!r}\n{result.stdout}{result.stderr}"
        )


def create_deterministic_bytes(path: Path, byte_count: int) -> None:
    state = 0x6D2B79F5
    data = bytearray(byte_count)
    for index in range(byte_count):
        state ^= (state << 13) & 0xFFFFFFFF
        state ^= state >> 17
        state ^= (state << 5) & 0xFFFFFFFF
        data[index] = state & 0xFF
    path.write_bytes(data)


def create_ninja_graph(root: Path, job_count: int) -> None:
    root.mkdir()
    (root / "out").mkdir()
    outputs = [f"out/job-{index:03d}" for index in range(job_count)]
    manifest = [
        "rule generate",
        "  command = /bin/sh -c 'printf benchmark > \"$out\"'",
        "  description = generate $out",
        "",
    ]
    manifest.extend(f"build {output}: generate" for output in outputs)
    manifest.extend(("", f"build all: phony {' '.join(outputs)}", "default all", ""))
    (root / "build.ninja").write_text("\n".join(manifest), encoding="ascii")


def build_workloads(root: Path, args: argparse.Namespace) -> list[Workload]:
    syscall_loop = root / "syscall_loop"
    fork_exec_loop = root / "fork_exec_loop"
    compile_fixture(FIXTURE_DIR / "syscall_loop.c", syscall_loop)
    compile_fixture(FIXTURE_DIR / "fork_exec_loop.c", fork_exec_loop)

    bzip2_input = root / "bzip2-input.bin"
    create_deterministic_bytes(bzip2_input, args.bzip2_bytes)

    ninja_root = root / "ninja-graph"
    create_ninja_graph(ninja_root, args.ninja_jobs)

    leveldb_root = root / "leveldb-data"
    sqlite_database = root / "sqlite-benchmark.db"
    sqlite_sql = " ".join(
        (
            "PRAGMA journal_mode=DELETE;",
            "PRAGMA synchronous=FULL;",
            "CREATE TABLE bench(id INTEGER PRIMARY KEY, payload TEXT);",
            "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL",
            f"SELECT x+1 FROM n WHERE x < {args.sqlite_rows})",
            "INSERT INTO bench SELECT x, printf('%08d-payload', x) FROM n;",
            "CREATE INDEX bench_payload ON bench(payload);",
            "SELECT count(*), sum(length(payload)) FROM bench;",
        )
    )

    return [
        Workload("true", "startup", ("/bin/true",)),
        Workload(
            "syscall-baseline",
            "microbaseline",
            (str(syscall_loop), "0"),
        ),
        Workload(
            "syscall-loop",
            "micro",
            (str(syscall_loop), str(args.syscall_iterations)),
            operations=args.syscall_iterations,
        ),
        Workload(
            "fork-baseline",
            "microbaseline",
            (str(fork_exec_loop), "0"),
        ),
        Workload(
            "fork-exec",
            "micro",
            (str(fork_exec_loop), str(args.fork_iterations)),
            operations=args.fork_iterations,
        ),
        Workload(
            "bzip2-2m",
            "macro",
            ("/usr/bin/bzip2", "-9", "-c", str(bzip2_input)),
        ),
        Workload(
            "ninja-graph",
            "macro",
            (
                str(args.ninja),
                "--quiet",
                "-j1",
                "-C",
                str(ninja_root),
            ),
            reset_kind="ninja",
            state_path=ninja_root / "out",
            expected_artifacts=args.ninja_jobs,
        ),
        Workload(
            "leveldb-fillread",
            "macro",
            (
                str(args.leveldb_bench),
                "--benchmarks=fillseq,readrandom",
                f"--num={args.leveldb_operations}",
                f"--reads={args.leveldb_operations}",
                "--threads=1",
                "--value_size=100",
                "--compression_ratio=0.5",
                f"--db={leveldb_root}",
                "--use_existing_db=0",
            ),
            output_policy="marker",
            output_marker=b"fillseq",
            reset_kind="leveldb",
            state_path=leveldb_root,
        ),
        Workload(
            "sqlite-insert-index",
            "macro",
            ("/usr/bin/sqlite3", str(sqlite_database), sqlite_sql),
            reset_kind="sqlite",
            state_path=sqlite_database,
        ),
    ]


def prepare_state(workload: Workload) -> None:
    path = workload.state_path
    if workload.reset_kind == "none":
        return
    if path is None:
        raise BenchmarkError(f"{workload.name}: reset contract has no state path")
    if workload.reset_kind == "ninja":
        shutil.rmtree(path, ignore_errors=True)
        path.mkdir()
    elif workload.reset_kind == "leveldb":
        shutil.rmtree(path, ignore_errors=True)
    elif workload.reset_kind == "sqlite":
        path.unlink(missing_ok=True)
    else:
        raise BenchmarkError(
            f"{workload.name}: unknown reset kind {workload.reset_kind}"
        )


def validate_artifacts(workload: Workload) -> None:
    path = workload.state_path
    if workload.reset_kind == "ninja":
        assert path is not None
        outputs = list(path.glob("job-*"))
        if len(outputs) != workload.expected_artifacts:
            raise BenchmarkError(
                f"created {len(outputs)} Ninja outputs; expected {workload.expected_artifacts}"
            )
        if any(output.read_bytes() != b"benchmark" for output in outputs):
            raise BenchmarkError("Ninja output content was incorrect")
    elif workload.reset_kind == "leveldb":
        assert path is not None
        if not (path / "CURRENT").is_file():
            raise BenchmarkError("LevelDB did not create a CURRENT manifest")
    elif workload.reset_kind == "sqlite":
        assert path is not None
        if not path.is_file() or path.stat().st_size == 0:
            raise BenchmarkError("SQLite did not create a nonempty database")


def benchmark_environment() -> dict[str, str]:
    environment = {
        "HOME": os.environ.get("HOME", "/tmp"),
        "LANG": "C",
        "LC_ALL": "C",
        "LOGNAME": os.environ.get("LOGNAME", "hermit-benchmark"),
        "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
        "SHELL": os.environ.get("SHELL", "/bin/sh"),
        "TZ": "UTC",
        "USER": os.environ.get("USER", "hermit-benchmark"),
    }
    for variable in ("DYNAMORIO_HOME", "DynamoRIO_DIR", "LD_LIBRARY_PATH"):
        if variable in os.environ:
            environment[variable] = os.environ[variable]
    return environment


def mode_command(hermit: Path, mode: str, workload: Workload) -> list[str]:
    if mode == "native":
        return list(workload.command)
    return [
        str(hermit),
        "--log=off",
        "run",
        "--backend",
        mode,
        "--strict",
        "--base-env=minimal",
        "--tmp=/tmp",
        "--",
        *workload.command,
    ]


def child_setup(cpu: int) -> None:
    os.setsid()
    os.sched_setaffinity(0, {cpu})


def terminate_process_group(process: subprocess.Popen[bytes]) -> tuple[bytes, bytes]:
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        pass

    try:
        output = process.communicate(timeout=1.0)
    except subprocess.TimeoutExpired:
        output = None

    # The direct wrapper can exit on SIGTERM before DynamoRIO-managed guests.
    # Always kill the original process group after the grace period, even when
    # Popen has already reaped the wrapper.
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass

    return output if output is not None else process.communicate()


def execute(
    command: list[str],
    environment: dict[str, str],
    cpu: int,
    timeout: float,
    capture: bool,
) -> CommandOutcome:
    stdout = subprocess.PIPE if capture else subprocess.DEVNULL
    stderr = subprocess.PIPE if capture else subprocess.DEVNULL
    started = time.perf_counter_ns()
    process = subprocess.Popen(
        command,
        cwd=REPOSITORY,
        env=environment,
        stdin=subprocess.DEVNULL,
        stdout=stdout,
        stderr=stderr,
        preexec_fn=lambda: child_setup(cpu),
    )
    try:
        captured_stdout, captured_stderr = process.communicate(timeout=timeout)
        timed_out = False
    except subprocess.TimeoutExpired:
        captured_stdout, captured_stderr = terminate_process_group(process)
        timed_out = True
    elapsed_ns = time.perf_counter_ns() - started
    return CommandOutcome(
        returncode=None if timed_out else process.returncode,
        stdout=captured_stdout or b"",
        stderr=captured_stderr or b"",
        elapsed_ns=elapsed_ns,
        timed_out=timed_out,
    )


def diagnostic(stderr: bytes) -> str:
    text = " ".join(stderr.decode(errors="replace").split())
    return text[-500:] or "no stderr diagnostic"


def compatibility_failure(
    workload: Workload,
    mode: str,
    outcome: CommandOutcome,
    status: str,
    detail: str,
) -> Compatibility:
    return Compatibility(
        workload=workload.name,
        category=workload.category,
        mode=mode,
        status=status,
        exit_code=outcome.returncode,
        wall_ms=outcome.elapsed_ns / 1_000_000,
        stdout_sha256=digest(outcome.stdout),
        detail=detail,
    )


def preflight(
    hermit: Path,
    workloads: list[Workload],
    modes: tuple[str, ...],
    cpu: int,
    timeout: float,
) -> tuple[list[Compatibility], set[tuple[str, str]]]:
    environment = benchmark_environment()
    results: list[Compatibility] = []
    passing: set[tuple[str, str]] = set()

    kvm_blocked = not Path("/dev/kvm").exists() or not os.access(
        "/dev/kvm", os.R_OK | os.W_OK
    )
    for workload in workloads:
        baseline_stdout = b""
        for mode in modes:
            if mode == "kvm" and kvm_blocked:
                result = Compatibility(
                    workload.name,
                    workload.category,
                    mode,
                    "blocked",
                    None,
                    0.0,
                    digest(b""),
                    "/dev/kvm is not readable and writable",
                )
                results.append(result)
                print(
                    f"BLOCKED {mode}/{workload.name}: {result.detail}", file=sys.stderr
                )
                continue

            prepare_state(workload)
            outcome = execute(
                mode_command(hermit, mode, workload),
                environment,
                cpu,
                timeout,
                capture=True,
            )
            if outcome.timed_out:
                result = compatibility_failure(
                    workload,
                    mode,
                    outcome,
                    "timeout",
                    f"exceeded {timeout:.1f}s and process group was terminated",
                )
            elif outcome.returncode != 0:
                result = compatibility_failure(
                    workload,
                    mode,
                    outcome,
                    "failed",
                    f"exit {outcome.returncode}: {diagnostic(outcome.stderr)}",
                )
            else:
                try:
                    validate_artifacts(workload)
                    if workload.output_policy == "exact":
                        if mode == "native":
                            baseline_stdout = outcome.stdout
                        elif outcome.stdout != baseline_stdout:
                            raise BenchmarkError(
                                "stdout mismatch: "
                                f"{digest(outcome.stdout)} != native {digest(baseline_stdout)}"
                            )
                    elif workload.output_policy == "marker":
                        marker = workload.output_marker
                        if (
                            marker is None
                            or marker not in outcome.stdout + outcome.stderr
                        ):
                            raise BenchmarkError(f"output omitted marker {marker!r}")
                    else:
                        raise BenchmarkError(
                            f"unknown output policy {workload.output_policy}"
                        )
                except BenchmarkError as error:
                    result = compatibility_failure(
                        workload, mode, outcome, "failed", str(error)
                    )
                else:
                    result = Compatibility(
                        workload.name,
                        workload.category,
                        mode,
                        "pass",
                        outcome.returncode,
                        outcome.elapsed_ns / 1_000_000,
                        digest(outcome.stdout),
                        "exit and semantic output checks passed",
                    )
                    passing.add((workload.name, mode))
            results.append(result)
            print(
                f"{result.status.upper()} {mode}/{workload.name}: {result.detail}",
                file=sys.stderr,
                flush=True,
            )

        if (workload.name, "native") not in passing:
            raise BenchmarkError(f"native preflight failed for {workload.name}")

    for backend in ("ptrace", "dbi", "kvm"):
        if backend in modes and not any(mode == backend for _, mode in passing):
            raise BenchmarkError(f"{backend} passed no workload preflights")
    return results, passing


def parse_time_file(path: Path) -> dict[str, str]:
    values: dict[str, str] = {}
    for line in path.read_text(encoding="ascii").splitlines():
        key, separator, value = line.partition("=")
        if not separator:
            raise BenchmarkError(f"invalid GNU time output: {line!r}")
        values[key] = value
    required = {
        "user_seconds",
        "system_seconds",
        "max_rss_kb",
        "voluntary_context_switches",
        "involuntary_context_switches",
    }
    if set(values) != required:
        raise BenchmarkError(
            f"GNU time output columns {sorted(values)} != {sorted(required)}"
        )
    return values


def run_sample(
    hermit: Path,
    time_binary: Path,
    time_path: Path,
    workload: Workload,
    mode: str,
    sample_number: int,
    environment: dict[str, str],
    cpu: int,
    timeout: float,
) -> Sample:
    prepare_state(workload)
    time_path.unlink(missing_ok=True)
    command = [
        str(time_binary),
        "-q",
        "-f",
        TIME_FORMAT,
        "-o",
        str(time_path),
        "--",
        *mode_command(hermit, mode, workload),
    ]
    outcome = execute(command, environment, cpu, timeout, capture=False)
    if outcome.timed_out:
        raise BenchmarkError(f"timed sample {mode}/{workload.name} exceeded {timeout}s")
    if outcome.returncode != 0:
        raise BenchmarkError(
            f"timed sample {mode}/{workload.name} exited {outcome.returncode}"
        )
    validate_artifacts(workload)
    timing = parse_time_file(time_path)
    return Sample(
        workload=workload.name,
        category=workload.category,
        mode=mode,
        sample=sample_number,
        elapsed_ns=outcome.elapsed_ns,
        user_cpu_ns=round(float(timing["user_seconds"]) * 1_000_000_000),
        system_cpu_ns=round(float(timing["system_seconds"]) * 1_000_000_000),
        max_rss_kb=int(timing["max_rss_kb"]),
        voluntary_context_switches=int(timing["voluntary_context_switches"]),
        involuntary_context_switches=int(timing["involuntary_context_switches"]),
    )


def rotated(values: tuple[str, ...], offset: int) -> tuple[str, ...]:
    index = offset % len(values)
    return values[index:] + values[:index]


def collect_samples(
    hermit: Path,
    workloads: list[Workload],
    modes: tuple[str, ...],
    passing: set[tuple[str, str]],
    args: argparse.Namespace,
    cpu: int,
    fixture_root: Path,
) -> list[Sample]:
    time_binary_text = shutil.which("/usr/bin/time")
    if time_binary_text is None:
        raise BenchmarkError("GNU /usr/bin/time is required")
    time_binary = Path(time_binary_text)
    environment = benchmark_environment()
    sequence = 0

    for warmup in range(args.warmups):
        order = rotated(modes, warmup)
        for workload in workloads:
            for mode in order:
                if (workload.name, mode) not in passing:
                    continue
                run_sample(
                    hermit,
                    time_binary,
                    fixture_root / f"time-warm-{sequence}.txt",
                    workload,
                    mode,
                    -1,
                    environment,
                    cpu,
                    args.timeout,
                )
                sequence += 1
        print(f"WARMUP {warmup + 1}/{args.warmups}", file=sys.stderr, flush=True)

    samples: list[Sample] = []
    for sample_number in range(1, args.samples + 1):
        order = rotated(modes, sample_number - 1)
        for workload in workloads:
            for mode in order:
                if (workload.name, mode) not in passing:
                    continue
                samples.append(
                    run_sample(
                        hermit,
                        time_binary,
                        fixture_root / f"time-sample-{sequence}.txt",
                        workload,
                        mode,
                        sample_number,
                        environment,
                        cpu,
                        args.timeout,
                    )
                )
                sequence += 1
        print(f"SAMPLE {sample_number}/{args.samples}", file=sys.stderr, flush=True)
    return samples


def percentile_95(values: list[float]) -> float:
    ordered = sorted(values)
    return ordered[max(0, math.ceil(len(ordered) * 0.95) - 1)]


def summarize(samples: list[Sample]) -> list[Summary]:
    groups: dict[tuple[str, str], list[Sample]] = {}
    for sample in samples:
        groups.setdefault((sample.workload, sample.mode), []).append(sample)

    summaries: list[Summary] = []
    for (workload, mode), group in groups.items():
        wall = [sample.elapsed_ns / 1_000_000 for sample in group]
        user = [sample.user_cpu_ns / 1_000_000 for sample in group]
        system = [sample.system_cpu_ns / 1_000_000 for sample in group]
        cpu = [left + right for left, right in zip(user, system)]
        summaries.append(
            Summary(
                workload=workload,
                category=group[0].category,
                mode=mode,
                samples=len(group),
                median_wall_ms=statistics.median(wall),
                p95_wall_ms=percentile_95(wall),
                mean_wall_ms=statistics.mean(wall),
                stddev_wall_ms=(statistics.stdev(wall) if len(wall) > 1 else 0.0),
                median_user_cpu_ms=statistics.median(user),
                median_system_cpu_ms=statistics.median(system),
                median_cpu_ms=statistics.median(cpu),
                median_max_rss_kb=statistics.median(
                    sample.max_rss_kb for sample in group
                ),
                median_context_switches=statistics.median(
                    sample.context_switches for sample in group
                ),
            )
        )
    return sorted(summaries, key=lambda value: (value.workload, value.mode))


def safe_ratio(numerator: float | None, denominator: float | None) -> float | None:
    if numerator is None or denominator is None or denominator <= 0:
        return None
    if not math.isfinite(numerator) or not math.isfinite(denominator):
        return None
    return numerator / denominator


def derive_metrics(
    summaries: list[Summary], args: argparse.Namespace
) -> list[DerivedMetric]:
    by_key = {(summary.workload, summary.mode): summary for summary in summaries}

    def wall(workload: str, mode: str) -> float | None:
        summary = by_key.get((workload, mode))
        return summary.median_wall_ms if summary else None

    def incremental(
        active: str, baseline: str, mode: str, divisor: int, scale: float
    ) -> float | None:
        active_value = wall(active, mode)
        baseline_value = wall(baseline, mode)
        if active_value is None or baseline_value is None:
            return None
        value = (active_value - baseline_value) * scale / divisor
        return value if value > 0 else None

    definitions: list[tuple[str, str, str, dict[str, float | None]]] = []
    definitions.append(
        (
            "true_end_to_end",
            "startup",
            "ms/run",
            {mode: wall("true", mode) for mode in MODES},
        )
    )
    definitions.append(
        (
            "syscall_interception",
            "micro",
            "us/call",
            {
                mode: incremental(
                    "syscall-loop",
                    "syscall-baseline",
                    mode,
                    args.syscall_iterations,
                    1_000.0,
                )
                for mode in MODES
            },
        )
    )
    definitions.append(
        (
            "syscall_loop_gross",
            "micro",
            "ms/run",
            {mode: wall("syscall-loop", mode) for mode in MODES},
        )
    )
    definitions.append(
        (
            "fork_exec_wait",
            "micro",
            "ms/iteration",
            {
                mode: incremental(
                    "fork-exec",
                    "fork-baseline",
                    mode,
                    args.fork_iterations,
                    1.0,
                )
                for mode in MODES
            },
        )
    )
    for workload in (
        "bzip2-2m",
        "ninja-graph",
        "leveldb-fillread",
        "sqlite-insert-index",
    ):
        definitions.append(
            (
                f"{workload}_wall",
                "macro",
                "ms/run",
                {mode: wall(workload, mode) for mode in MODES},
            )
        )

    derived = []
    for metric, category, unit, values in definitions:
        derived.append(
            DerivedMetric(
                metric=metric,
                category=category,
                unit=unit,
                native=values["native"],
                ptrace=values["ptrace"],
                dbi=values["dbi"],
                kvm=values["kvm"],
                ptrace_over_native=safe_ratio(values["ptrace"], values["native"]),
                dbi_speedup_vs_ptrace=safe_ratio(values["ptrace"], values["dbi"]),
                kvm_speedup_vs_ptrace=safe_ratio(values["ptrace"], values["kvm"]),
            )
        )
    return derived


def write_tsv(path: Path, values: list[object]) -> None:
    if not values:
        raise BenchmarkError(f"refusing to write empty results to {path}")
    rows = [
        {key: "NA" if item is None else item for key, item in asdict(value).items()}
        for value in values
    ]
    with path.open("w", newline="", encoding="utf-8") as output:
        writer = csv.DictWriter(
            output,
            fieldnames=rows[0].keys(),
            delimiter="\t",
            lineterminator="\n",
        )
        writer.writeheader()
        writer.writerows(rows)


def cpu_model() -> str:
    for line in Path("/proc/cpuinfo").read_text(encoding="ascii").splitlines():
        if line.startswith("model name"):
            return line.split(":", 1)[1].strip()
    return platform.processor() or "unknown"


def read_cpu_counters(allowed_cpus: set[int]) -> dict[int, tuple[int, int]]:
    counters: dict[int, tuple[int, int]] = {}
    for line in Path("/proc/stat").read_text(encoding="ascii").splitlines():
        fields = line.split()
        if len(fields) < 5 or not fields[0].startswith("cpu") or fields[0] == "cpu":
            continue
        cpu = int(fields[0][3:])
        if cpu not in allowed_cpus:
            continue
        values = [int(value) for value in fields[1:]]
        idle = values[3] + (values[4] if len(values) > 4 else 0)
        counters[cpu] = (sum(values), idle)
    return counters


def select_quiet_cpu(allowed_cpus: set[int]) -> int:
    first = read_cpu_counters(allowed_cpus)
    time.sleep(0.25)
    second = read_cpu_counters(allowed_cpus)
    candidates = []
    for cpu in allowed_cpus:
        total_delta = second[cpu][0] - first[cpu][0]
        idle_delta = second[cpu][1] - first[cpu][1]
        busy = 1.0 - idle_delta / total_delta if total_delta > 0 else 1.0
        candidates.append((busy, cpu))
    return min(candidates)[1]


def file_sha256(path: Path) -> str:
    with path.open("rb") as source:
        hasher = hashlib.sha256()
        for block in iter(lambda: source.read(1024 * 1024), b""):
            hasher.update(block)
    return hasher.hexdigest()


def metadata(
    hermit: Path,
    workloads: list[Workload],
    args: argparse.Namespace,
    modes: tuple[str, ...],
    cpu: int,
) -> dict[str, object]:
    commit = subprocess.run(
        ["git", "rev-parse", "HEAD"],
        cwd=REPOSITORY,
        capture_output=True,
        text=True,
        check=True,
    ).stdout.strip()
    dirty = bool(
        subprocess.run(
            ["git", "status", "--porcelain"],
            cwd=REPOSITORY,
            capture_output=True,
            text=True,
            check=True,
        ).stdout
    )
    governor = Path(f"/sys/devices/system/cpu/cpu{cpu}/cpufreq/scaling_governor")
    return {
        "timestamp_utc": datetime.now(timezone.utc).isoformat(),
        "git_commit": commit,
        "git_worktree_dirty": dirty,
        "runner_sha256": file_sha256(Path(__file__)),
        "hermit": str(hermit),
        "hermit_sha256": file_sha256(hermit),
        "kernel": platform.release(),
        "machine": platform.machine(),
        "cpu_model": cpu_model(),
        "cpu": cpu,
        "cpu_selection": "explicit" if args.cpu is not None else "least-busy-250ms",
        "cpu_governor": (
            governor.read_text().strip() if governor.exists() else "unavailable"
        ),
        "load_average": os.getloadavg(),
        "samples": args.samples,
        "warmups": args.warmups,
        "timeout_seconds": args.timeout,
        "modes": modes,
        "syscall_iterations": args.syscall_iterations,
        "fork_iterations": args.fork_iterations,
        "bzip2_bytes": args.bzip2_bytes,
        "ninja_jobs": args.ninja_jobs,
        "leveldb_operations": args.leveldb_operations,
        "sqlite_rows": args.sqlite_rows,
        "ninja": str(args.ninja),
        "ninja_sha256": file_sha256(args.ninja),
        "ninja_source_commit": "79feac0f3e3bc9da9effc586cd5fea41e7550051",
        "leveldb_bench": str(args.leveldb_bench),
        "leveldb_bench_sha256": file_sha256(args.leveldb_bench),
        "leveldb_source_commit": "99b3c03b3284f5886f9ef9a4ef703d57373e61be",
        "measurement": "GNU time per process group plus perf_counter_ns wall clock",
        "shared_hermit_flags": [
            "--log=off",
            "run",
            "--backend=MODE",
            "--strict",
            "--base-env=minimal",
            "--tmp=/tmp",
        ],
        "workloads": {workload.name: list(workload.command) for workload in workloads},
    }


def format_value(value: float | None) -> str:
    return "-" if value is None else f"{value:.3f}"


def print_tables(
    summaries: list[Summary],
    compatibility: list[Compatibility],
    derived: list[DerivedMetric],
) -> None:
    summary_by_key = {(value.workload, value.mode): value for value in summaries}
    status_by_key = {
        (value.workload, value.mode): value.status.upper() for value in compatibility
    }
    workload_order = [
        "true",
        "syscall-loop",
        "fork-exec",
        "bzip2-2m",
        "ninja-graph",
        "leveldb-fillread",
        "sqlite-insert-index",
    ]

    def cell(workload: str, mode: str) -> str:
        summary = summary_by_key.get((workload, mode))
        if summary is not None:
            return f"{summary.median_wall_ms:.3f}"
        return status_by_key.get((workload, mode), "-")

    print("| Workload | native ms | ptrace ms | DBI ms | KVM ms |")
    print("| --- | ---: | ---: | ---: | ---: |")
    for workload in workload_order:
        print(
            f"| {workload} | {cell(workload, 'native')} | "
            f"{cell(workload, 'ptrace')} | {cell(workload, 'dbi')} | "
            f"{cell(workload, 'kvm')} |"
        )

    print(
        "\n| Derived metric | unit | native | ptrace | DBI | KVM | "
        "DBI speedup vs ptrace | KVM speedup vs ptrace |"
    )
    print("| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |")
    for metric in derived:
        print(
            f"| {metric.metric} | {metric.unit} | {format_value(metric.native)} | "
            f"{format_value(metric.ptrace)} | {format_value(metric.dbi)} | "
            f"{format_value(metric.kvm)} | "
            f"{format_value(metric.dbi_speedup_vs_ptrace)} | "
            f"{format_value(metric.kvm_speedup_vs_ptrace)} |"
        )


def main() -> int:
    args = parse_args()
    hermit = executable(args.hermit, "Hermit")
    args.ninja = executable(args.ninja, "Ninja")
    args.leveldb_bench = executable(args.leveldb_bench, "LevelDB db_bench")

    selected = tuple(args.modes or MODES)
    modes = selected if "native" in selected else ("native", *selected)
    modes = tuple(mode for mode in MODES if mode in modes)
    allowed_cpus = set(os.sched_getaffinity(0))
    cpu = args.cpu if args.cpu is not None else select_quiet_cpu(allowed_cpus)
    if cpu not in allowed_cpus:
        raise BenchmarkError(
            f"CPU {cpu} is outside allowed affinity {sorted(allowed_cpus)}"
        )

    output_dir = (
        args.output_dir.expanduser().resolve()
        if args.output_dir
        else Path(tempfile.mkdtemp(prefix="backend-benchmark-results-"))
    )
    if args.output_dir:
        output_dir.mkdir(parents=True, exist_ok=False)

    target_root = REPOSITORY / "target"
    target_root.mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(
        prefix="backend-benchmark-fixtures-", dir=target_root
    ) as tempdir:
        fixture_root = Path(tempdir)
        workloads = build_workloads(fixture_root, args)
        compatibility, passing = preflight(hermit, workloads, modes, cpu, args.timeout)
        samples = collect_samples(
            hermit,
            workloads,
            modes,
            passing,
            args,
            cpu,
            fixture_root,
        )

        summaries = summarize(samples)
        derived = derive_metrics(summaries, args)
        write_tsv(output_dir / "compatibility.tsv", compatibility)
        write_tsv(output_dir / "raw.tsv", samples)
        write_tsv(output_dir / "summary.tsv", summaries)
        write_tsv(output_dir / "derived.tsv", derived)
        (output_dir / "metadata.json").write_text(
            json.dumps(
                metadata(hermit, workloads, args, modes, cpu),
                indent=2,
                sort_keys=True,
            )
            + "\n",
            encoding="utf-8",
        )

    print_tables(summaries, compatibility, derived)
    print(f"\nResults: {output_dir}")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except BenchmarkError as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(1) from error
