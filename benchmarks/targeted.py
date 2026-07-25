#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

"""Measure targeted native, ptrace, DBI, and KVM backend workloads."""

from __future__ import annotations

import argparse
import datetime
import json
import os
import platform
import shutil
import signal
import statistics
import subprocess
import sys
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Sequence

ROOT = Path(__file__).resolve().parent.parent
BENCHMARK_DIR = ROOT / "benchmarks"
WORK_DIR = ROOT / "target" / "hermit-targeted-benchmarks"
DEFAULT_OUTPUT_DIR = BENCHMARK_DIR / "results" / "targeted"
BACKEND_NAMES = ("native", "ptrace", "dbi", "kvm")


class BenchmarkError(RuntimeError):
    """The benchmark matrix could not be prepared or measured."""


@dataclass(frozen=True)
class Benchmark:
    name: str
    description: str
    command: tuple[str, ...]
    binary_size_bytes: int


def comma_separated(
    parser: argparse.ArgumentParser,
    raw: str,
    allowed: Sequence[str],
    option: str,
) -> tuple[str, ...]:
    values = tuple(value.strip() for value in raw.split(",") if value.strip())
    invalid = sorted(set(values) - set(allowed))
    if not values or invalid:
        parser.error(
            f"{option} must be a comma-separated subset of {','.join(allowed)}"
        )
    if len(values) != len(set(values)):
        parser.error(f"{option} contains a duplicate value")
    return values


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Compare targeted workloads across Hermit execution backends."
    )
    parser.add_argument(
        "--iterations",
        type=int,
        default=5,
        help="measured runs per backend and benchmark (default: 5)",
    )
    parser.add_argument(
        "--warmups",
        type=int,
        default=1,
        help="unmeasured warmup runs per backend and benchmark (default: 1)",
    )
    parser.add_argument(
        "--timeout",
        type=float,
        default=180.0,
        help="seconds before one sample is terminated (default: 180)",
    )
    parser.add_argument(
        "--backends",
        default="native,ptrace,dbi,kvm",
        help="comma-separated backend set (default: native,ptrace,dbi,kvm)",
    )
    parser.add_argument(
        "--benchmarks",
        default="cpu_bound,syscall_heavy,large_startup,mixed_workload",
        help="comma-separated benchmark set",
    )
    parser.add_argument(
        "--output",
        type=Path,
        default=DEFAULT_OUTPUT_DIR,
        help="result directory (default: benchmarks/results/targeted)",
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
        help="use an existing Hermit release executable",
    )
    args = parser.parse_args()

    if args.iterations < 1:
        parser.error("--iterations must be at least 1")
    if args.warmups < 0:
        parser.error("--warmups cannot be negative")
    if args.timeout <= 0:
        parser.error("--timeout must be positive")

    args.backends = comma_separated(parser, args.backends, BACKEND_NAMES, "--backends")
    if "native" not in args.backends:
        parser.error("--backends must include native for overhead ratios")
    args.benchmarks = comma_separated(
        parser,
        args.benchmarks,
        ("cpu_bound", "syscall_heavy", "large_startup", "mixed_workload"),
        "--benchmarks",
    )
    return args


def resolve_from_root(path: Path) -> Path:
    return path if path.is_absolute() else ROOT / path


def command_environment() -> dict[str, str]:
    environment = os.environ.copy()
    environment["LC_ALL"] = "C"
    environment["LANG"] = "C"
    return environment


def run_setup(command: Sequence[str]) -> None:
    print("+ " + " ".join(command), file=sys.stderr)
    subprocess.run(command, cwd=ROOT, env=command_environment(), check=True)


def require_tool(name: str) -> str:
    path = shutil.which(name)
    if path is None:
        raise BenchmarkError(f"required tool is unavailable: {name}")
    return path


def build_hermit(hermit: Path, skip_build: bool) -> None:
    if not skip_build:
        run_setup(["cargo", "build", "--release", "-p", "hermit", "--bin", "hermit"])
    if not hermit.is_file() or not os.access(hermit, os.X_OK):
        raise BenchmarkError(f"Hermit executable is not executable: {hermit}")


def compile_fixture(cc: str, source: Path, output: Path) -> None:
    run_setup(
        [
            cc,
            "-O2",
            "-std=c11",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-fno-pie",
            "-no-pie",
            str(source),
            "-o",
            str(output),
        ]
    )


def prepare_benchmarks(selected: Sequence[str]) -> list[Benchmark]:
    WORK_DIR.mkdir(parents=True, exist_ok=True)
    cc = require_tool("cc")
    descriptions = {
        "cpu_bound": "1,000,000 arithmetic iterations; no syscalls in the loop",
        "syscall_heavy": "100,000 raw getpid/clock_gettime syscalls",
        "large_startup": "execute a 4 MiB text path once after startup",
        "mixed_workload": "10,000 compute blocks followed by raw getpid",
    }

    benchmarks = []
    for name in selected:
        source = BENCHMARK_DIR / "fixtures" / f"{name}.c"
        output = WORK_DIR / name.replace("_", "-")
        compile_fixture(cc, source, output)
        benchmarks.append(
            Benchmark(
                name=name,
                description=descriptions[name],
                command=(str(output),),
                binary_size_bytes=output.stat().st_size,
            )
        )
    return benchmarks


def backend_command(
    hermit: Path, backend: str, benchmark: Benchmark
) -> tuple[str, ...]:
    if backend == "native":
        return benchmark.command
    return (
        str(hermit),
        "--log=error",
        "--backend",
        backend,
        "run",
        "--strict",
        "--",
        *benchmark.command,
    )


def run_bounded(
    command: Sequence[str], timeout: float, capture_output: bool, phase: str
) -> subprocess.CompletedProcess[bytes]:
    output = subprocess.PIPE if capture_output else subprocess.DEVNULL
    process = subprocess.Popen(
        command,
        cwd=ROOT,
        env=command_environment(),
        stdout=output,
        stderr=output,
        start_new_session=True,
    )
    try:
        stdout, stderr = process.communicate(timeout=timeout)
    except subprocess.TimeoutExpired as error:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.communicate()
        raise BenchmarkError(
            f"{phase} timed out after {timeout:g}s: {' '.join(command)}"
        ) from error
    return subprocess.CompletedProcess(
        command, process.returncode, stdout=stdout, stderr=stderr
    )


def run_precheck(command: Sequence[str], timeout: float) -> bytes:
    completed = run_bounded(command, timeout, True, "precheck")
    if completed.returncode != 0:
        assert completed.stderr is not None
        stderr = completed.stderr.decode("utf-8", errors="replace").strip()
        detail = f"; stderr: {stderr}" if stderr else ""
        raise BenchmarkError(
            f"precheck exited {completed.returncode}: {' '.join(command)}{detail}"
        )
    assert completed.stdout is not None
    return completed.stdout


def measure_once(command: Sequence[str], timeout: float) -> int:
    started = time.perf_counter_ns()
    completed = run_bounded(command, timeout, False, "sample")
    elapsed = time.perf_counter_ns() - started
    if completed.returncode != 0:
        raise BenchmarkError(
            f"sample exited {completed.returncode}: {' '.join(command)}"
        )
    return elapsed


def summarize_samples(samples_ns: Sequence[int]) -> dict[str, object]:
    return {
        "samples_seconds": [round(sample / 1_000_000_000, 9) for sample in samples_ns],
        "mean_seconds": round(statistics.fmean(samples_ns) / 1_000_000_000, 9),
        "median_seconds": round(statistics.median(samples_ns) / 1_000_000_000, 9),
    }


def measure_benchmark(
    benchmark: Benchmark,
    hermit: Path,
    backends: Sequence[str],
    iterations: int,
    warmups: int,
    timeout: float,
) -> dict[str, object]:
    commands = {
        backend: backend_command(hermit, backend, benchmark) for backend in backends
    }
    native_output = run_precheck(commands["native"], timeout)
    available = []
    modes: dict[str, object] = {}

    for backend in backends:
        try:
            output = run_precheck(commands[backend], timeout)
            if output != native_output:
                raise BenchmarkError(
                    f"stdout differs from native for backend {backend}: "
                    f"{output!r} != {native_output!r}"
                )
            available.append(backend)
        except BenchmarkError as error:
            if backend == "native":
                raise
            modes[backend] = {
                "status": "unavailable",
                "command": list(commands[backend]),
                "reason": str(error),
            }

    samples: dict[str, list[int]] = {backend: [] for backend in available}
    for _ in range(warmups):
        for backend in available:
            measure_once(commands[backend], timeout)

    for iteration in range(iterations):
        offset = iteration % len(available)
        order = available[offset:] + available[:offset]
        for backend in order:
            try:
                samples[backend].append(measure_once(commands[backend], timeout))
            except BenchmarkError as error:
                modes[backend] = {
                    "status": "failed",
                    "command": list(commands[backend]),
                    "reason": str(error),
                    "completed_samples": len(samples[backend]),
                }
                samples.pop(backend)
                available.remove(backend)
                continue

    for backend, backend_samples in samples.items():
        summary = summarize_samples(backend_samples)
        summary.update(
            {
                "status": "ok",
                "command": list(commands[backend]),
            }
        )
        modes[backend] = summary

    native = modes["native"]
    assert isinstance(native, dict)
    native_median = float(native["median_seconds"])
    for _backend, mode in modes.items():
        assert isinstance(mode, dict)
        if mode["status"] != "ok":
            continue
        median = float(mode["median_seconds"])
        mode["ratio_vs_native"] = round(median / native_median, 3)

    return {
        "name": benchmark.name,
        "description": benchmark.description,
        "binary_size_bytes": benchmark.binary_size_bytes,
        "stdout": native_output.decode("utf-8", errors="replace"),
        "modes": modes,
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


def host_cpu() -> str:
    try:
        for line in Path("/proc/cpuinfo").read_text(encoding="utf-8").splitlines():
            if line.startswith("model name"):
                return line.partition(":")[2].strip()
    except OSError:
        pass
    return "unknown"


def perf_event_paranoid() -> str:
    try:
        return (
            Path("/proc/sys/kernel/perf_event_paranoid")
            .read_text(encoding="ascii")
            .strip()
        )
    except OSError:
        return "unknown"


def format_mode(mode: dict[str, object], native: bool) -> str:
    if mode["status"] != "ok":
        return str(mode["status"])
    milliseconds = float(mode["median_seconds"]) * 1000.0
    if native:
        return f"{milliseconds:.3f} ms"
    return f"{milliseconds:.3f} ms ({float(mode['ratio_vs_native']):.2f}x)"


def render_summary(results: dict[str, object]) -> str:
    configuration = results["configuration"]
    assert isinstance(configuration, dict)
    rows = [
        "# Targeted Hermit backend benchmark results",
        "",
        f"Generated: {results['generated_at']}",
        f"Hermit revision: {results['hermit_revision']}",
        f"Measured samples: {configuration['iterations']} "
        f"(+ {configuration['warmups']} warmup per available backend)",
        "Hermit modes: strict, log=error, relaxations=none",
        "",
        "| Benchmark | Native median | Ptrace median | DBI median | KVM median |",
        "| --- | ---: | ---: | ---: | ---: |",
    ]
    benchmarks = results["benchmarks"]
    assert isinstance(benchmarks, list)
    failures = []
    for benchmark in benchmarks:
        assert isinstance(benchmark, dict)
        modes = benchmark["modes"]
        assert isinstance(modes, dict)
        cells = []
        for backend in BACKEND_NAMES:
            mode = modes.get(backend)
            if not isinstance(mode, dict):
                cells.append("not requested")
                continue
            cells.append(format_mode(mode, backend == "native"))
            if mode["status"] != "ok":
                failures.append(f"- {benchmark['name']} / {backend}: {mode['reason']}")
        rows.append(
            f"| {benchmark['name']} | {cells[0]} | {cells[1]} | "
            f"{cells[2]} | {cells[3]} |"
        )

    rows.extend(
        [
            "",
            "Ratios in parentheses use each benchmark's native median.",
            "Individual samples and exact commands are in results.json.",
        ]
    )
    if failures:
        rows.extend(["", "## Unavailable or failed rows", "", *failures])
    rows.append("")
    return "\n".join(rows)


def main() -> int:
    args = parse_args()
    hermit = resolve_from_root(args.hermit).resolve()
    output_dir = resolve_from_root(args.output).resolve()
    output_dir.mkdir(parents=True, exist_ok=True)

    build_hermit(hermit, args.skip_build)
    benchmarks = prepare_benchmarks(args.benchmarks)

    measured = []
    for index, benchmark in enumerate(benchmarks, start=1):
        print(f"[{index}/{len(benchmarks)}] {benchmark.name}", file=sys.stderr)
        result = measure_benchmark(
            benchmark,
            hermit,
            args.backends,
            args.iterations,
            args.warmups,
            args.timeout,
        )
        measured.append(result)
        modes = result["modes"]
        assert isinstance(modes, dict)
        status = []
        for backend in args.backends:
            mode = modes[backend]
            assert isinstance(mode, dict)
            status.append(f"{backend}={format_mode(mode, backend == 'native')}")
        print("  " + " ".join(status), file=sys.stderr)

    results: dict[str, object] = {
        "schema_version": 1,
        "generated_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "hermit_revision": git_revision(),
        "hermit_binary": str(hermit),
        "system": {
            "platform": platform.platform(),
            "kernel": platform.release(),
            "machine": platform.machine(),
            "cpu": host_cpu(),
            "processor_count": os.cpu_count(),
            "perf_event_paranoid": perf_event_paranoid(),
            "python_version": platform.python_version(),
            "load_average_at_completion": list(os.getloadavg()),
        },
        "configuration": {
            "iterations": args.iterations,
            "warmups": args.warmups,
            "timeout_seconds": args.timeout,
            "backends": list(args.backends),
            "benchmarks": list(args.benchmarks),
        },
        "benchmarks": measured,
    }

    results_path = output_dir / "results.json"
    summary_path = output_dir / "summary.md"
    results_path.write_text(json.dumps(results, indent=2) + "\n", encoding="utf-8")
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
