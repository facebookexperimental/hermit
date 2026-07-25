#!/usr/bin/env python3
"""Compare raw gVisor platform exits with Reverie's ptrace counter."""

from __future__ import annotations

import argparse
import csv
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import signal
import statistics
import subprocess
import sys
import time
from typing import Any


SCRIPT_DIR = Path(__file__).resolve().parent
REVERIE_COUNT = re.compile(r"Total system calls in process tree: ([0-9]+)")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--gvisor-counter", type=Path, required=True)
    parser.add_argument("--reverie-counter", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--iterations", type=int, default=100_000)
    parser.add_argument("--runs", type=int, default=5)
    parser.add_argument("--warmups", type=int, default=1)
    parser.add_argument("--timeout", type=float, default=30.0)
    parser.add_argument("--cpu", type=int)
    parser.add_argument("--cc", default=os.environ.get("CC", "gcc"))
    parser.add_argument(
        "--backend",
        action="append",
        choices=("systrap", "kvm", "reverie"),
        dest="backends",
        help="backend to include; repeatable (default: all three)",
    )
    parser.add_argument("--gvisor-profile", default="opt")
    parser.add_argument("--reverie-profile", default="unknown")
    args = parser.parse_args()

    if args.iterations <= 0:
        parser.error("--iterations must be greater than zero")
    if args.runs <= 0:
        parser.error("--runs must be greater than zero")
    if args.warmups < 0:
        parser.error("--warmups must not be negative")
    if args.timeout <= 0:
        parser.error("--timeout must be greater than zero")
    if args.backends is None:
        args.backends = ["systrap", "kvm", "reverie"]
    if len(set(args.backends)) != len(args.backends):
        parser.error("--backend values must be unique")
    return args


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def require_executable(path: Path, label: str) -> Path:
    resolved = path.expanduser().resolve()
    if not resolved.is_file() or not os.access(resolved, os.X_OK):
        raise RuntimeError(f"{label} is not executable: {resolved}")
    return resolved


def invoke(command: list[str], timeout: float) -> tuple[int, int, str, str]:
    started = time.perf_counter_ns()
    process = subprocess.Popen(
        command,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        start_new_session=True,
        env={**os.environ, "LC_ALL": "C"},
    )
    try:
        stdout, stderr = process.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGTERM)
        try:
            stdout, stderr = process.communicate(timeout=1.0)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            stdout, stderr = process.communicate()
        raise RuntimeError(
            f"command timed out after {timeout}s: {shlex.join(command)}\n{stderr}"
        )
    elapsed = time.perf_counter_ns() - started
    return process.returncode, elapsed, stdout, stderr


def pinned(command: list[str], cpu: int | None) -> list[str]:
    if cpu is None:
        return command
    return ["taskset", "-c", str(cpu), *command]


def compile_fixture(args: argparse.Namespace, output_dir: Path) -> Path:
    fixture = output_dir / "getpid_loop"
    command = [
        args.cc,
        f"-DITERATIONS={args.iterations}",
        "-nostdlib",
        "-static",
        "-Wl,--build-id=none",
        "-o",
        str(fixture),
        str(SCRIPT_DIR / "fixtures/getpid_loop.S"),
    ]
    completed = subprocess.run(command, text=True, capture_output=True, check=False)
    if completed.returncode != 0:
        raise RuntimeError(
            f"fixture compile failed: {shlex.join(command)}\n{completed.stderr}"
        )
    status, _, _, stderr = invoke(pinned([str(fixture)], args.cpu), args.timeout)
    if status != 0:
        raise RuntimeError(f"native fixture preflight exited {status}: {stderr}")
    return fixture


def parse_gvisor(stdout: str, iterations: int) -> tuple[int, int]:
    lines = [line for line in stdout.splitlines() if line.strip()]
    if not lines:
        raise RuntimeError("gVisor counter produced no JSON")
    data = json.loads(lines[-1])
    getpid = int(data["getpid_syscalls"])
    total = int(data["total_syscalls"])
    if getpid != iterations or total != iterations + 1:
        raise RuntimeError(
            f"gVisor count mismatch: getpid={getpid}, total={total}, "
            f"expected {iterations}/{iterations + 1}"
        )
    if data.get("syscall_patching") is not False:
        raise RuntimeError("gVisor result did not identify unpatched syscall handling")
    return total, int(data["elapsed_ns"])


def parse_reverie(stderr: str, iterations: int) -> int:
    match = REVERIE_COUNT.search(stderr)
    if match is None:
        raise RuntimeError(f"Reverie counter output did not contain a total: {stderr}")
    total = int(match.group(1))
    if total != iterations + 2:
        raise RuntimeError(
            f"Reverie count mismatch: total={total}, expected {iterations + 2}"
        )
    return total


def command_for(
    backend: str,
    args: argparse.Namespace,
    fixture: Path,
) -> list[str]:
    if backend == "reverie":
        command = [str(args.reverie_counter), str(fixture)]
    else:
        command = [
            str(args.gvisor_counter),
            f"--backend={backend}",
            f"--syscalls={args.iterations}",
            "--runs=1",
        ]
    return pinned(command, args.cpu)


def run_one(
    backend: str,
    run: int,
    args: argparse.Namespace,
    fixture: Path,
    diagnostics: Path,
) -> dict[str, Any]:
    command = command_for(backend, args, fixture)
    status, wall_ns, stdout, stderr = invoke(command, args.timeout)
    stem = diagnostics / f"{backend}-run-{run:02d}"
    stem.with_suffix(".stdout").write_text(stdout, encoding="utf-8")
    stem.with_suffix(".stderr").write_text(stderr, encoding="utf-8")
    if status != 0:
        raise RuntimeError(f"{backend} run {run} exited {status}: {stderr}")

    if backend == "reverie":
        total = parse_reverie(stderr, args.iterations)
        switch_ns: int | None = None
        profile = args.reverie_profile
    else:
        total, switch_ns = parse_gvisor(stdout, args.iterations)
        profile = args.gvisor_profile

    return {
        "backend": backend,
        "run": run,
        "profile": profile,
        "status": status,
        "counted_syscalls": total,
        "wall_elapsed_ns": wall_ns,
        "wall_ns_per_count": wall_ns / total,
        "platform_switch_ns": switch_ns,
        "switch_ns_per_count": None if switch_ns is None else switch_ns / total,
        "command": shlex.join(command),
    }


def write_results(
    args: argparse.Namespace,
    fixture: Path,
    rows: list[dict[str, Any]],
) -> None:
    raw_fields = [
        "backend",
        "run",
        "profile",
        "status",
        "counted_syscalls",
        "wall_elapsed_ns",
        "wall_ns_per_count",
        "platform_switch_ns",
        "switch_ns_per_count",
        "command",
    ]
    with (args.output_dir / "raw.tsv").open(
        "w", newline="", encoding="utf-8"
    ) as target:
        writer = csv.DictWriter(target, fieldnames=raw_fields, delimiter="\t")
        writer.writeheader()
        writer.writerows(rows)

    summary_fields = [
        "backend",
        "samples",
        "counted_syscalls",
        "median_wall_ns",
        "median_wall_ns_per_count",
        "median_platform_switch_ns",
        "median_switch_ns_per_count",
        "profile",
    ]
    summaries: list[dict[str, Any]] = []
    for backend in args.backends:
        selected = [row for row in rows if row["backend"] == backend]
        switch_values = [
            row["platform_switch_ns"]
            for row in selected
            if row["platform_switch_ns"] is not None
        ]
        count = selected[0]["counted_syscalls"]
        median_wall = statistics.median(row["wall_elapsed_ns"] for row in selected)
        median_switch = statistics.median(switch_values) if switch_values else None
        summaries.append(
            {
                "backend": backend,
                "samples": len(selected),
                "counted_syscalls": count,
                "median_wall_ns": median_wall,
                "median_wall_ns_per_count": median_wall / count,
                "median_platform_switch_ns": median_switch,
                "median_switch_ns_per_count": (
                    None if median_switch is None else median_switch / count
                ),
                "profile": selected[0]["profile"],
            }
        )
    with (args.output_dir / "summary.tsv").open(
        "w", newline="", encoding="utf-8"
    ) as target:
        writer = csv.DictWriter(target, fieldnames=summary_fields, delimiter="\t")
        writer.writeheader()
        writer.writerows(summaries)

    metadata = {
        "schema_version": 1,
        "created_unix_ns": time.time_ns(),
        "host": " ".join(os.uname()),
        "cpu": args.cpu,
        "iterations": args.iterations,
        "runs": args.runs,
        "warmups": args.warmups,
        "timeout_seconds": args.timeout,
        "backends": args.backends,
        "gvisor_counter": str(args.gvisor_counter),
        "gvisor_counter_sha256": sha256(args.gvisor_counter),
        "gvisor_profile": args.gvisor_profile,
        "reverie_counter": str(args.reverie_counter),
        "reverie_counter_sha256": sha256(args.reverie_counter),
        "reverie_profile": args.reverie_profile,
        "fixture": str(fixture),
        "fixture_sha256": sha256(fixture),
    }
    (args.output_dir / "metadata.json").write_text(
        json.dumps(metadata, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )


def main() -> int:
    args = parse_args()
    args.gvisor_counter = require_executable(args.gvisor_counter, "gVisor counter")
    args.reverie_counter = require_executable(args.reverie_counter, "Reverie counter")
    if "kvm" in args.backends and not os.access("/dev/kvm", os.R_OK | os.W_OK):
        raise RuntimeError("KVM backend requested but /dev/kvm is not read-write")
    if args.cpu is not None and args.cpu not in os.sched_getaffinity(0):
        raise RuntimeError(f"CPU {args.cpu} is outside this process's affinity set")
    if args.output_dir.exists():
        raise RuntimeError(f"refusing to overwrite output directory: {args.output_dir}")

    args.output_dir.mkdir(parents=True)
    diagnostics = args.output_dir / "diagnostics"
    diagnostics.mkdir()
    fixture = compile_fixture(args, args.output_dir)

    for backend in args.backends:
        for warmup in range(1, args.warmups + 1):
            command = command_for(backend, args, fixture)
            status, _, stdout, stderr = invoke(command, args.timeout)
            if status != 0:
                raise RuntimeError(
                    f"{backend} warmup {warmup} exited {status}: {stderr}"
                )
            if backend == "reverie":
                parse_reverie(stderr, args.iterations)
            else:
                parse_gvisor(stdout, args.iterations)

    rows: list[dict[str, Any]] = []
    for run in range(1, args.runs + 1):
        offset = (run - 1) % len(args.backends)
        order = args.backends[offset:] + args.backends[:offset]
        for backend in order:
            rows.append(run_one(backend, run, args, fixture, diagnostics))

    write_results(args, fixture, rows)
    print(f"wrote {len(rows)} observations to {args.output_dir}")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (
        KeyError,
        OSError,
        RuntimeError,
        TypeError,
        ValueError,
        json.JSONDecodeError,
    ) as error:
        print(f"gvisor-platform-counter: {error}", file=sys.stderr)
        raise SystemExit(2)
