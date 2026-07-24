#!/usr/bin/env python3
"""Measure native versus Hermit wall-clock time for representative workloads."""

from __future__ import annotations

import argparse
import datetime
import json
import os
import platform
import shutil
import statistics
import subprocess
import sys
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Sequence

ROOT = Path(__file__).resolve().parent.parent
BENCHMARK_DIR = ROOT / "benchmarks"
WORK_DIR = ROOT / "target" / "hermit-benchmarks"
DEFAULT_OUTPUT_DIR = BENCHMARK_DIR / "results"

THREAD_COUNT = 4
COUNTER_ITERATIONS = 1_000_000
FORK_EXEC_DEPTH = 25


class BenchmarkError(RuntimeError):
    """A benchmark could not be prepared or completed."""


@dataclass(frozen=True)
class Benchmark:
    name: str
    description: str
    command: tuple[str, ...]


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Compare native and deterministic Hermit wall-clock performance."
    )
    parser.add_argument(
        "--iterations",
        type=int,
        default=5,
        help="measured runs per mode and benchmark (default: 5)",
    )
    parser.add_argument(
        "--warmups",
        type=int,
        default=1,
        help="unmeasured warmup runs per mode and benchmark (default: 1)",
    )
    parser.add_argument(
        "--sort-lines",
        type=int,
        default=1_000_000,
        help="lines in the shared sort/grep input (default: 1000000)",
    )
    parser.add_argument(
        "--timeout",
        type=float,
        default=120.0,
        help="seconds before one workload run is terminated (default: 120)",
    )
    parser.add_argument(
        "--output",
        type=Path,
        default=DEFAULT_OUTPUT_DIR,
        help="result directory (default: benchmarks/results)",
    )
    parser.add_argument(
        "--hermit",
        type=Path,
        default=ROOT / "target" / "release" / "hermit",
        help="Hermit executable (default: target/release/hermit)",
    )
    parser.add_argument(
        "--skip-build",
        action="store_true",
        help="use the existing Hermit executable without a release build",
    )
    args = parser.parse_args()

    if args.iterations < 1:
        parser.error("--iterations must be at least 1")
    if args.warmups < 0:
        parser.error("--warmups cannot be negative")
    if args.sort_lines < 1:
        parser.error("--sort-lines must be at least 1")
    if args.timeout <= 0:
        parser.error("--timeout must be positive")
    return args


def resolve_from_root(path: Path) -> Path:
    if path.is_absolute():
        return path
    return ROOT / path


def require_tool(name: str) -> str:
    path = shutil.which(name)
    if path is None:
        raise BenchmarkError(f"required tool is unavailable: {name}")
    return path


def command_environment() -> dict[str, str]:
    environment = os.environ.copy()
    environment["LC_ALL"] = "C"
    environment["LANG"] = "C"
    return environment


def run_setup(command: Sequence[str]) -> None:
    print("+ " + " ".join(command), file=sys.stderr)
    subprocess.run(command, cwd=ROOT, env=command_environment(), check=True)


def build_hermit(hermit: Path, skip_build: bool) -> None:
    if not skip_build:
        run_setup(["cargo", "build", "--release", "-p", "hermit", "--bin", "hermit"])
    if not hermit.is_file() or not os.access(hermit, os.X_OK):
        raise BenchmarkError(
            f"Hermit executable is missing or not executable: {hermit}"
        )


def compile_fixture(cc: str, source: Path, output: Path, pthread: bool = False) -> None:
    command = [cc, "-O2", "-std=c11", "-Wall", "-Wextra", "-Werror"]
    if pthread:
        command.append("-pthread")
    command.extend([str(source), "-o", str(output)])
    run_setup(command)


def generate_input(path: Path, line_count: int) -> None:
    print(
        f"Generating {line_count:,} deterministic input lines at {path}",
        file=sys.stderr,
    )
    with path.open("w", encoding="ascii", buffering=1024 * 1024) as output:
        for value in range(line_count, 0, -1):
            marker = "needle" if value % 1000 == 0 else "haystack"
            output.write(f"{value:07d} {marker} benchmark-payload-{value % 97:02d}\n")


def prepare_benchmarks(args: argparse.Namespace) -> list[Benchmark]:
    WORK_DIR.mkdir(parents=True, exist_ok=True)
    dataset = WORK_DIR / f"input-{args.sort_lines}.txt"
    generate_input(dataset, args.sort_lines)

    cc = require_tool("cc")
    thread_counter = WORK_DIR / "thread-counter"
    fork_exec_chain = WORK_DIR / "fork-exec-chain"
    compile_fixture(
        cc,
        BENCHMARK_DIR / "fixtures" / "thread_counter.c",
        thread_counter,
        pthread=True,
    )
    compile_fixture(
        cc,
        BENCHMARK_DIR / "fixtures" / "fork_exec_chain.c",
        fork_exec_chain,
    )

    return [
        Benchmark(
            "echo",
            "single-process launch baseline",
            (require_tool("echo"), "hermit-benchmark"),
        ),
        Benchmark(
            "sort_1m_lines",
            f"sort {args.sort_lines:,} deterministic lines",
            (require_tool("sort"), str(dataset)),
        ),
        Benchmark(
            "grep_large_file",
            f"grep the shared {args.sort_lines:,}-line input",
            (require_tool("grep"), "-c", "needle", str(dataset)),
        ),
        Benchmark(
            "multithread_counter",
            f"{THREAD_COUNT} threads perform {COUNTER_ITERATIONS:,} atomic increments each",
            (str(thread_counter), str(THREAD_COUNT), str(COUNTER_ITERATIONS)),
        ),
        Benchmark(
            "fork_exec_chain",
            f"serial fork+exec chain with depth {FORK_EXEC_DEPTH}",
            (str(fork_exec_chain), str(FORK_EXEC_DEPTH)),
        ),
    ]


def hermit_command(hermit: Path, benchmark: Benchmark) -> tuple[str, ...]:
    return (
        str(hermit),
        "--log=error",
        "run",
        "--base-env=minimal",
        "--env=LC_ALL=C",
        "--no-virtualize-cpuid",
        "--max-timeslice=disabled",
        "--",
        *benchmark.command,
    )


def measure_once(command: Sequence[str], timeout: float) -> int:
    started = time.perf_counter_ns()
    try:
        completed = subprocess.run(
            command,
            cwd=ROOT,
            env=command_environment(),
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            timeout=timeout,
            check=False,
        )
    except subprocess.TimeoutExpired as error:
        raise BenchmarkError(
            f"command timed out after {timeout:g}s: {' '.join(command)}"
        ) from error
    elapsed = time.perf_counter_ns() - started

    if completed.returncode != 0:
        stderr = completed.stderr.decode("utf-8", errors="replace").strip()
        detail = f"\nstderr:\n{stderr}" if stderr else ""
        raise BenchmarkError(
            f"command exited {completed.returncode}: {' '.join(command)}{detail}"
        )
    return elapsed


def summarize_samples(samples_ns: list[int]) -> dict[str, object]:
    mean_ns = statistics.fmean(samples_ns)
    median_ns = statistics.median(samples_ns)
    return {
        "samples_seconds": [round(sample / 1_000_000_000, 9) for sample in samples_ns],
        "mean_seconds": round(mean_ns / 1_000_000_000, 9),
        "median_seconds": round(median_ns / 1_000_000_000, 9),
    }


def measure_benchmark(
    benchmark: Benchmark,
    hermit: Path,
    iterations: int,
    warmups: int,
    timeout: float,
) -> dict[str, object]:
    native_command = benchmark.command
    wrapped_command = hermit_command(hermit, benchmark)

    for _ in range(warmups):
        measure_once(native_command, timeout)
        measure_once(wrapped_command, timeout)

    native_samples: list[int] = []
    hermit_samples: list[int] = []
    for iteration in range(iterations):
        modes = [
            ("native", native_command, native_samples),
            ("hermit", wrapped_command, hermit_samples),
        ]
        if iteration % 2 == 1:
            modes.reverse()
        for _, command, samples in modes:
            samples.append(measure_once(command, timeout))

    native = summarize_samples(native_samples)
    wrapped = summarize_samples(hermit_samples)
    native_mean = statistics.fmean(native_samples)
    hermit_mean = statistics.fmean(hermit_samples)
    overhead = ((hermit_mean / native_mean) - 1.0) * 100.0

    return {
        "name": benchmark.name,
        "description": benchmark.description,
        "native_command": list(native_command),
        "hermit_command": list(wrapped_command),
        "native": native,
        "hermit": wrapped,
        "overhead_percent": round(overhead, 3),
    }


def git_revision() -> str:
    completed = subprocess.run(
        ["git", "rev-parse", "HEAD"],
        cwd=ROOT,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        check=False,
    )
    return completed.stdout.strip() if completed.returncode == 0 else "unknown"


def render_summary(results: dict[str, object]) -> str:
    configuration = results["configuration"]
    assert isinstance(configuration, dict)
    rows = [
        "# Hermit benchmark results",
        "",
        f"Generated: {results['generated_at']}",
        f"Hermit revision: `{results['hermit_revision']}`",
        f"Measured iterations: {configuration['iterations']} "
        f"(+ {configuration['warmups']} warmup per mode)",
        "",
        "| Benchmark | Native mean | Hermit mean | Overhead |",
        "| --- | ---: | ---: | ---: |",
    ]
    benchmarks = results["benchmarks"]
    assert isinstance(benchmarks, list)
    for benchmark in benchmarks:
        assert isinstance(benchmark, dict)
        native = benchmark["native"]
        wrapped = benchmark["hermit"]
        assert isinstance(native, dict) and isinstance(wrapped, dict)
        rows.append(
            "| {name} | {native:.3f} ms | {hermit:.3f} ms | {overhead:+.1f}% |".format(
                name=benchmark["name"],
                native=float(native["mean_seconds"]) * 1000.0,
                hermit=float(wrapped["mean_seconds"]) * 1000.0,
                overhead=float(benchmark["overhead_percent"]),
            )
        )
    rows.extend(
        [
            "",
            "Overhead is `(Hermit mean / native mean - 1) * 100`.",
            "Individual wall-clock samples are available in `results.json`.",
            "",
        ]
    )
    return "\n".join(rows)


def main() -> int:
    args = parse_args()
    hermit = resolve_from_root(args.hermit).resolve()
    output_dir = resolve_from_root(args.output).resolve()
    output_dir.mkdir(parents=True, exist_ok=True)

    build_hermit(hermit, args.skip_build)
    benchmarks = prepare_benchmarks(args)

    measured: list[dict[str, object]] = []
    for index, benchmark in enumerate(benchmarks, start=1):
        print(f"[{index}/{len(benchmarks)}] {benchmark.name}", file=sys.stderr)
        result = measure_benchmark(
            benchmark,
            hermit,
            args.iterations,
            args.warmups,
            args.timeout,
        )
        measured.append(result)
        print(
            "  native={:.3f} ms hermit={:.3f} ms overhead={:+.1f}%".format(
                float(result["native"]["mean_seconds"]) * 1000.0,
                float(result["hermit"]["mean_seconds"]) * 1000.0,
                float(result["overhead_percent"]),
            ),
            file=sys.stderr,
        )

    results: dict[str, object] = {
        "schema_version": 1,
        "generated_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "hermit_revision": git_revision(),
        "hermit_binary": str(hermit),
        "system": {
            "platform": platform.platform(),
            "machine": platform.machine(),
            "processor_count": os.cpu_count(),
            "python_version": platform.python_version(),
        },
        "configuration": {
            "iterations": args.iterations,
            "warmups": args.warmups,
            "timeout_seconds": args.timeout,
            "sort_lines": args.sort_lines,
            "thread_count": THREAD_COUNT,
            "counter_iterations_per_thread": COUNTER_ITERATIONS,
            "fork_exec_depth": FORK_EXEC_DEPTH,
        },
        "benchmarks": measured,
    }

    results_path = output_dir / "results.json"
    summary_path = output_dir / "summary.md"
    with results_path.open("w", encoding="utf-8") as output:
        json.dump(results, output, indent=2)
        output.write("\n")
    summary = render_summary(results)
    summary_path.write_text(summary, encoding="utf-8")

    print()
    print(summary)
    print(f"Machine-readable results: {results_path}")
    print(f"Human-readable summary: {summary_path}")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (BenchmarkError, subprocess.CalledProcessError) as error:
        print(f"benchmark error: {error}", file=sys.stderr)
        raise SystemExit(1) from error
