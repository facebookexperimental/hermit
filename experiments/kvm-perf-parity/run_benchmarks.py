#!/usr/bin/env python3
"""Benchmark strict Hermit execution through the ptrace and KVM backends."""

from __future__ import annotations

import argparse
import csv
from dataclasses import asdict, dataclass
from datetime import datetime, timezone
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import resource
import shutil
import statistics
import subprocess
import sys
import tempfile
import time


SCRIPT_DIR = Path(__file__).resolve().parent
REPOSITORY = SCRIPT_DIR.parent.parent
FIXTURE_DIR = SCRIPT_DIR / "fixtures"
BACKENDS = ("ptrace", "kvm")
MIB = 1024 * 1024


class BenchmarkError(Exception):
    """A benchmark prerequisite, workload, or parity check failed."""


@dataclass(frozen=True)
class Workload:
    name: str
    category: str
    command: tuple[str, ...]
    expected_stdout: bytes | None = None
    operations: int = 0
    byte_count: int = 0


@dataclass(frozen=True)
class Sample:
    workload: str
    category: str
    backend: str
    sample: int
    elapsed_ns: int
    user_cpu_ns: int
    system_cpu_ns: int
    voluntary_context_switches: int
    involuntary_context_switches: int
    operations: int
    byte_count: int

    @property
    def context_switches(self) -> int:
        return self.voluntary_context_switches + self.involuntary_context_switches


@dataclass(frozen=True)
class Summary:
    workload: str
    category: str
    backend: str
    samples: int
    median_ms: float
    p95_ms: float
    mean_ms: float
    stddev_ms: float
    median_user_cpu_ms: float
    median_system_cpu_ms: float
    median_context_switches: float


@dataclass(frozen=True)
class DerivedMetric:
    metric: str
    unit: str
    ptrace: float
    kvm: float
    kvm_advantage: float


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--hermit",
        type=Path,
        default=REPOSITORY / "target/release/hermit",
        help="optimized Hermit binary to benchmark",
    )
    parser.add_argument("--samples", type=positive_int, default=9)
    parser.add_argument("--warmups", type=nonnegative_int, default=2)
    parser.add_argument("--syscall-iterations", type=positive_int, default=10_000)
    parser.add_argument("--pipe-iterations", type=positive_int, default=1_000)
    parser.add_argument("--stream-bytes", type=positive_int, default=16 * MIB)
    parser.add_argument("--timeout", type=positive_float, default=120.0)
    parser.add_argument(
        "--cpu",
        type=nonnegative_int,
        help="logical CPU for all measured processes (default: least busy allowed CPU)",
    )
    parser.add_argument(
        "--output-dir",
        type=Path,
        help="write metadata, raw samples, summaries, and derived metrics here",
    )
    return parser.parse_args()


def positive_int(value: str) -> int:
    parsed = int(value)
    if parsed <= 0:
        raise argparse.ArgumentTypeError("must be positive")
    return parsed


def nonnegative_int(value: str) -> int:
    parsed = int(value)
    if parsed < 0:
        raise argparse.ArgumentTypeError("must be nonnegative")
    return parsed


def positive_float(value: str) -> float:
    parsed = float(value)
    if parsed <= 0:
        raise argparse.ArgumentTypeError("must be positive")
    return parsed


def compile_fixture(source: Path, output: Path) -> None:
    compiler = shutil.which(os.environ.get("CC", "cc"))
    if compiler is None:
        raise BenchmarkError("C compiler unavailable (set CC or install cc)")
    command = [
        compiler,
        "-O2",
        "-std=c11",
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


def create_stream(path: Path, byte_count: int) -> None:
    chunk = bytes(range(256)) * 256
    remaining = byte_count
    with path.open("wb") as output:
        while remaining:
            written = min(remaining, len(chunk))
            output.write(chunk[:written])
            remaining -= written


def native_output(*command: str) -> bytes:
    result = subprocess.run(command, capture_output=True, check=False)
    if result.returncode != 0:
        raise BenchmarkError(f"native expected-output command failed: {command!r}")
    return result.stdout


def build_workloads(root: Path, args: argparse.Namespace) -> list[Workload]:
    syscall_loop = root / "syscall_loop"
    pipe_roundtrip = root / "pipe_roundtrip"
    compile_fixture(FIXTURE_DIR / "syscall_loop.c", syscall_loop)
    compile_fixture(FIXTURE_DIR / "pipe_roundtrip.c", pipe_roundtrip)

    stream = root / "stream.bin"
    listing = root / "listing"
    create_stream(stream, args.stream_bytes)
    listing.mkdir()
    listing_names = [f"entry-{index:03d}" for index in range(64)]
    for name in listing_names:
        (listing / name).write_text(f"{name}\n", encoding="ascii")

    readme = REPOSITORY / "README.md"
    head_output = b"".join(readme.read_bytes().splitlines(keepends=True)[:3])
    return [
        Workload("echo", "application", ("/bin/echo", "hello"), b"hello\n"),
        Workload(
            "ls-64",
            "application",
            ("/bin/ls", "-1", str(listing)),
            "".join(f"{name}\n" for name in listing_names).encode("ascii"),
        ),
        Workload("true", "application", ("/bin/true",), b""),
        Workload(
            "pwd",
            "application",
            ("/usr/bin/pwd",),
            f"{REPOSITORY}\n".encode(),
        ),
        Workload(
            "seq-10",
            "application",
            ("/usr/bin/seq", "10"),
            "".join(f"{value}\n" for value in range(1, 11)).encode("ascii"),
        ),
        Workload(
            "head-readme",
            "application",
            ("/usr/bin/head", "-n", "3", str(readme)),
            head_output,
        ),
        Workload(
            "base64-readme",
            "application",
            ("/usr/bin/base64", str(readme)),
            native_output("/usr/bin/base64", str(readme)),
        ),
        Workload("id-u", "application", ("/usr/bin/id", "-u"), b"0\n"),
        Workload(
            "printf",
            "application",
            ("/usr/bin/printf", "%s=%d\\n", "hermit", "42"),
            b"hermit=42\n",
        ),
        Workload(
            "cat-stream",
            "throughput",
            ("/bin/cat", str(stream)),
            stream.read_bytes(),
            byte_count=args.stream_bytes,
        ),
        Workload(
            "syscall-baseline",
            "microbaseline",
            (str(syscall_loop), "0"),
            b"",
        ),
        Workload(
            "syscall-loop",
            "syscall",
            (str(syscall_loop), str(args.syscall_iterations)),
            b"",
            operations=args.syscall_iterations,
        ),
        Workload(
            "pipe-baseline",
            "microbaseline",
            (str(pipe_roundtrip), "0"),
            b"",
        ),
        Workload(
            "pipe-roundtrip",
            "boundary",
            (str(pipe_roundtrip), str(args.pipe_iterations)),
            b"",
            operations=args.pipe_iterations * 2,
        ),
    ]


def benchmark_environment() -> dict[str, str]:
    environment = os.environ.copy()
    environment["LC_ALL"] = "C"
    environment["TZ"] = "UTC"
    environment.pop("RUST_LOG", None)
    return environment


def hermit_command(hermit: Path, backend: str, workload: Workload) -> list[str]:
    return [
        str(hermit),
        "--log=off",
        "run",
        "--backend",
        backend,
        "--strict",
        "--base-env=minimal",
        "--tmp=/tmp",
        "--",
        *workload.command,
    ]


def affinity_preexec(cpu: int):
    def pin_to_cpu() -> None:
        os.sched_setaffinity(0, {cpu})

    return pin_to_cpu


def preflight(
    hermit: Path,
    workloads: list[Workload],
    cpu: int,
    timeout: float,
) -> None:
    environment = benchmark_environment()
    for workload in workloads:
        outputs: dict[str, bytes] = {}
        for backend in BACKENDS:
            command = hermit_command(hermit, backend, workload)
            try:
                result = subprocess.run(
                    command,
                    cwd=REPOSITORY,
                    env=environment,
                    capture_output=True,
                    timeout=timeout,
                    check=False,
                    preexec_fn=affinity_preexec(cpu),
                )
            except subprocess.TimeoutExpired as error:
                raise BenchmarkError(
                    f"preflight timed out for {workload.name}/{backend}"
                ) from error
            if result.returncode != 0:
                diagnostic = result.stderr.decode(errors="replace").strip()
                raise BenchmarkError(
                    f"preflight {workload.name}/{backend} exited "
                    f"{result.returncode}: {diagnostic[-500:]}"
                )
            outputs[backend] = result.stdout
            if (
                workload.expected_stdout is not None
                and result.stdout != workload.expected_stdout
            ):
                raise BenchmarkError(
                    f"preflight {workload.name}/{backend} stdout digest "
                    f"{digest(result.stdout)} != expected {digest(workload.expected_stdout)}"
                )
        if outputs["ptrace"] != outputs["kvm"]:
            raise BenchmarkError(
                f"preflight output mismatch for {workload.name}: "
                f"ptrace={digest(outputs['ptrace'])} kvm={digest(outputs['kvm'])}"
            )
        print(f"PREFLIGHT {workload.name}", file=sys.stderr, flush=True)


def digest(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def run_sample(
    hermit: Path,
    workload: Workload,
    backend: str,
    sample_number: int,
    cpu: int,
    timeout: float,
) -> Sample:
    command = hermit_command(hermit, backend, workload)
    environment = benchmark_environment()
    before = resource.getrusage(resource.RUSAGE_CHILDREN)
    started = time.perf_counter_ns()
    try:
        result = subprocess.run(
            command,
            cwd=REPOSITORY,
            env=environment,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=timeout,
            check=False,
            preexec_fn=affinity_preexec(cpu),
        )
    except subprocess.TimeoutExpired as error:
        raise BenchmarkError(
            f"timed run timed out for {workload.name}/{backend}"
        ) from error
    elapsed_ns = time.perf_counter_ns() - started
    after = resource.getrusage(resource.RUSAGE_CHILDREN)
    if result.returncode != 0:
        raise BenchmarkError(
            f"timed run {workload.name}/{backend} exited {result.returncode}"
        )
    return Sample(
        workload=workload.name,
        category=workload.category,
        backend=backend,
        sample=sample_number,
        elapsed_ns=elapsed_ns,
        user_cpu_ns=round((after.ru_utime - before.ru_utime) * 1_000_000_000),
        system_cpu_ns=round((after.ru_stime - before.ru_stime) * 1_000_000_000),
        voluntary_context_switches=after.ru_nvcsw - before.ru_nvcsw,
        involuntary_context_switches=after.ru_nivcsw - before.ru_nivcsw,
        operations=workload.operations,
        byte_count=workload.byte_count,
    )


def collect_samples(
    hermit: Path,
    workloads: list[Workload],
    args: argparse.Namespace,
    cpu: int,
) -> list[Sample]:
    for warmup in range(args.warmups):
        for workload in workloads:
            for backend in BACKENDS if warmup % 2 == 0 else reversed(BACKENDS):
                run_sample(hermit, workload, backend, -1, cpu, args.timeout)
        print(f"WARMUP {warmup + 1}/{args.warmups}", file=sys.stderr, flush=True)

    samples: list[Sample] = []
    for sample_number in range(1, args.samples + 1):
        order = BACKENDS if sample_number % 2 == 1 else tuple(reversed(BACKENDS))
        for workload in workloads:
            for backend in order:
                samples.append(
                    run_sample(
                        hermit,
                        workload,
                        backend,
                        sample_number,
                        cpu,
                        args.timeout,
                    )
                )
        print(f"SAMPLE {sample_number}/{args.samples}", file=sys.stderr, flush=True)
    return samples


def percentile_95(values: list[float]) -> float:
    ordered = sorted(values)
    return ordered[max(0, math.ceil(len(ordered) * 0.95) - 1)]


def summarize(samples: list[Sample]) -> list[Summary]:
    groups: dict[tuple[str, str], list[Sample]] = {}
    for sample in samples:
        groups.setdefault((sample.workload, sample.backend), []).append(sample)
    summaries = []
    for (workload, backend), group in groups.items():
        elapsed = [sample.elapsed_ns / 1_000_000 for sample in group]
        summaries.append(
            Summary(
                workload=workload,
                category=group[0].category,
                backend=backend,
                samples=len(group),
                median_ms=statistics.median(elapsed),
                p95_ms=percentile_95(elapsed),
                mean_ms=statistics.mean(elapsed),
                stddev_ms=statistics.stdev(elapsed) if len(elapsed) > 1 else 0.0,
                median_user_cpu_ms=statistics.median(
                    sample.user_cpu_ns / 1_000_000 for sample in group
                ),
                median_system_cpu_ms=statistics.median(
                    sample.system_cpu_ns / 1_000_000 for sample in group
                ),
                median_context_switches=statistics.median(
                    sample.context_switches for sample in group
                ),
            )
        )
    return sorted(summaries, key=lambda value: (value.workload, value.backend))


def derive_metrics(
    summaries: list[Summary], args: argparse.Namespace
) -> list[DerivedMetric]:
    by_key = {(value.workload, value.backend): value for value in summaries}

    def incremental(
        active: str, baseline: str, backend: str, field: str, count: int
    ) -> float:
        active_value = getattr(by_key[(active, backend)], field)
        baseline_value = getattr(by_key[(baseline, backend)], field)
        return (active_value - baseline_value) / count

    syscall_latency = {
        backend: incremental(
            "syscall-loop",
            "syscall-baseline",
            backend,
            "median_ms",
            args.syscall_iterations,
        )
        * 1_000_000
        for backend in BACKENDS
    }
    syscall_latency = positive_measurements(syscall_latency)
    pipe_latency = {
        backend: incremental(
            "pipe-roundtrip",
            "pipe-baseline",
            backend,
            "median_ms",
            args.pipe_iterations * 2,
        )
        * 1_000_000
        for backend in BACKENDS
    }
    pipe_latency = positive_measurements(pipe_latency)
    syscall_switches = {
        backend: max(
            0.0,
            incremental(
                "syscall-loop",
                "syscall-baseline",
                backend,
                "median_context_switches",
                args.syscall_iterations,
            ),
        )
        for backend in BACKENDS
    }
    pipe_switches = {
        backend: max(
            0.0,
            incremental(
                "pipe-roundtrip",
                "pipe-baseline",
                backend,
                "median_context_switches",
                args.pipe_iterations * 2,
            ),
        )
        for backend in BACKENDS
    }
    throughput = {
        backend: args.stream_bytes
        / MIB
        / (by_key[("cat-stream", backend)].median_ms / 1_000)
        for backend in BACKENDS
    }
    return [
        latency_metric("syscall_interception", "ns/call", syscall_latency),
        latency_metric("pipe_boundary", "ns/boundary", pipe_latency),
        latency_metric(
            "syscall_host_context_switches", "switches/call", syscall_switches
        ),
        latency_metric(
            "pipe_host_context_switches", "switches/boundary", pipe_switches
        ),
        DerivedMetric(
            metric="cat_throughput",
            unit="MiB/s",
            ptrace=throughput["ptrace"],
            kvm=throughput["kvm"],
            kvm_advantage=safe_ratio(throughput["kvm"], throughput["ptrace"]),
        ),
    ]


def latency_metric(name: str, unit: str, values: dict[str, float]) -> DerivedMetric:
    return DerivedMetric(
        metric=name,
        unit=unit,
        ptrace=values["ptrace"],
        kvm=values["kvm"],
        kvm_advantage=safe_ratio(values["ptrace"], values["kvm"]),
    )


def positive_measurements(values: dict[str, float]) -> dict[str, float]:
    return {
        backend: value if value > 0 else float("nan")
        for backend, value in values.items()
    }


def safe_ratio(numerator: float, denominator: float) -> float:
    if not math.isfinite(numerator) or not math.isfinite(denominator):
        return float("nan")
    if numerator < 0 or denominator < 0:
        return float("nan")
    return numerator / denominator if denominator != 0 else float("inf")


def write_tsv(path: Path, values: list[object]) -> None:
    if not values:
        raise BenchmarkError(f"refusing to write empty results to {path}")
    rows = [asdict(value) for value in values]
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
    cpuinfo = Path("/proc/cpuinfo")
    if cpuinfo.exists():
        for line in cpuinfo.read_text(encoding="utf-8").splitlines():
            if line.startswith("model name"):
                return line.split(":", 1)[1].strip()
    return platform.processor() or "unknown"


def read_cpu_counters(allowed_cpus: set[int]) -> dict[int, tuple[int, int]]:
    counters = {}
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
        busy_fraction = 1.0 - idle_delta / total_delta if total_delta > 0 else 1.0
        candidates.append((busy_fraction, cpu))
    return min(candidates)[1]


def metadata(hermit: Path, args: argparse.Namespace, cpu: int) -> dict[str, object]:
    git = subprocess.run(
        ["git", "rev-parse", "HEAD"],
        cwd=REPOSITORY,
        capture_output=True,
        text=True,
        check=True,
    ).stdout.strip()
    governor_path = Path(f"/sys/devices/system/cpu/cpu{cpu}/cpufreq/scaling_governor")
    return {
        "timestamp_utc": datetime.now(timezone.utc).isoformat(),
        "git_commit": git,
        "hermit": str(hermit),
        "hermit_sha256": digest(hermit.read_bytes()),
        "kernel": platform.release(),
        "machine": platform.machine(),
        "cpu_model": cpu_model(),
        "cpu": cpu,
        "cpu_selection": "explicit" if args.cpu is not None else "least-busy-250ms",
        "load_average": os.getloadavg(),
        "cpu_governor": (
            governor_path.read_text().strip()
            if governor_path.exists()
            else "unavailable"
        ),
        "samples": args.samples,
        "warmups": args.warmups,
        "syscall_iterations": args.syscall_iterations,
        "pipe_iterations": args.pipe_iterations,
        "stream_bytes": args.stream_bytes,
        "timeout_seconds": args.timeout,
        "shared_hermit_flags": [
            "--log=off",
            "run",
            "--strict",
            "--base-env=minimal",
            "--tmp=/tmp",
        ],
        "context_switch_source": "getrusage(RUSAGE_CHILDREN): ru_nvcsw + ru_nivcsw",
    }


def print_tables(summaries: list[Summary], derived: list[DerivedMetric]) -> None:
    by_key = {(value.workload, value.backend): value for value in summaries}
    workloads = sorted(
        {value.workload for value in summaries if value.category != "microbaseline"}
    )
    print(
        "| Workload | Category | ptrace median ms | KVM median ms | KVM speedup | ptrace ctxsw | KVM ctxsw |"
    )
    print("| --- | --- | ---: | ---: | ---: | ---: | ---: |")
    for workload in workloads:
        ptrace = by_key[(workload, "ptrace")]
        kvm = by_key[(workload, "kvm")]
        print(
            f"| {workload} | {ptrace.category} | {ptrace.median_ms:.3f} | "
            f"{kvm.median_ms:.3f} | {ptrace.median_ms / kvm.median_ms:.2f}x | "
            f"{ptrace.median_context_switches:.1f} | {kvm.median_context_switches:.1f} |"
        )
    print("\n| Derived metric | ptrace | KVM | KVM advantage |")
    print("| --- | ---: | ---: | ---: |")
    for metric in derived:
        print(
            f"| {metric.metric} ({metric.unit}) | {metric.ptrace:.3f} | "
            f"{metric.kvm:.3f} | {metric.kvm_advantage:.2f}x |"
        )


def main() -> int:
    args = parse_args()
    hermit = args.hermit.resolve()
    if not hermit.is_file() or not os.access(hermit, os.X_OK):
        raise BenchmarkError(f"Hermit binary is not executable: {hermit}")
    if not Path("/dev/kvm").exists() or not os.access("/dev/kvm", os.R_OK | os.W_OK):
        raise BenchmarkError("/dev/kvm is not readable and writable")
    allowed_cpus = set(os.sched_getaffinity(0))
    cpu = args.cpu if args.cpu is not None else select_quiet_cpu(allowed_cpus)
    if cpu not in allowed_cpus:
        raise BenchmarkError(
            f"CPU {cpu} is outside allowed affinity {sorted(allowed_cpus)}"
        )

    output_dir = (
        args.output_dir.resolve()
        if args.output_dir
        else Path(tempfile.mkdtemp(prefix="kvm-perf-results-"))
    )
    output_dir.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(
        prefix="kvm-perf-fixtures-", dir="/tmp"
    ) as tempdir:
        workloads = build_workloads(Path(tempdir), args)
        preflight(hermit, workloads, cpu, args.timeout)
        samples = collect_samples(hermit, workloads, args, cpu)

    summaries = summarize(samples)
    derived = derive_metrics(summaries, args)
    write_tsv(output_dir / "raw.tsv", samples)
    write_tsv(output_dir / "summary.tsv", summaries)
    write_tsv(output_dir / "derived.tsv", derived)
    (output_dir / "metadata.json").write_text(
        json.dumps(metadata(hermit, args, cpu), indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )
    print_tables(summaries, derived)
    print(f"\nResults: {output_dir}")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except BenchmarkError as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(1) from error
