#!/usr/bin/env python3
"""S0: direct single-run Hermit timing harness (no safehermit wrapper floor).

Why this exists
---------------
`bin/safehermit` boxes every run in a fresh `systemd-run --user` unit. That
provisioning costs ~1-1.5 s of wall time per invocation on a loaded host, which
swamps Hermit's own 20-200 ms startup and makes short workloads unmeasurable.
This harness times the Hermit binary as a *direct child*, so the measured wall
and CPU are Hermit's, not the wrapper's.

How the safehermit rule is honoured
------------------------------------
The rule's purpose is bounding, not the wrapper itself. This harness applies
equivalent bounds to every timed sample and records each as APPLIED or
NOT_APPLIED in its output metadata (the same fail-loud discipline):

  bound.wall   APPLIED  per-sample wall timeout; the whole process group is
                        SIGKILLed on expiry.
  bound.cpu    APPLIED  RLIMIT_CPU set to the sample timeout, and the child
                        is confined to a K=1 CPU set (sched_setaffinity to one
                        chosen CPU; no per-thread pinning inside the guest).
  bound.memory APPLIED  RLIMIT_AS from --memory-mb (default 16384 MiB).
  bound.output APPLIED  stdout/stderr go to capped temp files; a sample that
                        exceeds --output-cap bytes is killed and failed.
  bound.cgroup NOT_APPLIED  /sys/fs/cgroup is not writable by the invoking
                        user on typical dev hosts, so no dedicated cgroup is
                        created. The rlimit+affinity bounds above are the
                        replacement; on a host with delegated cgroup v2 this
                        line must be revisited before publish-grade runs.

This harness is for *timing only*. Exploratory, adversarial, or long-running
Hermit invocations still go through `bin/safehermit`.

Output: results.json (raw samples, medians, IQR, getpid slopes/intercepts,
metadata with full Hermit SHA, chosen CPU, load averages) under --output-dir
(default benchmarks/results/direct). Statistics: median primary, IQR shown.
A sample counts only on exit 0 and exact expected stdout; failures are kept.
"""

from __future__ import annotations

import argparse
import json
import math
import os
import platform
import resource
import signal
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
EPOCH = "2026-01-01T00:00:00Z"
BACKENDS = ("native", "ptrace", "kvm", "liteinst", "dbt")


def median_iqr(samples: list[float]) -> dict:
    ordered = sorted(samples)
    n = len(ordered)
    return {
        "samples": samples,
        "median": statistics.median(ordered),
        "q1": ordered[n // 4],
        "q3": ordered[(3 * n) // 4],
        "n": n,
    }


def fit_slope(xs: list[float], ys: list[float]) -> tuple[float, float]:
    """Least-squares (slope, intercept); slope is per-unit cost in y units."""
    xm = sum(xs) / len(xs)
    ym = sum(ys) / len(ys)
    den = sum((x - xm) ** 2 for x in xs)
    if den == 0:
        raise ValueError("fit needs at least two distinct x values")
    slope = sum((x - xm) * (y - ym) for x, y in zip(xs, ys)) / den
    return slope, ym - slope * xm


def pick_quiet_cpu(idle_fractions: dict[int, float], allowed: set[int]) -> int:
    """Choose the allowed CPU with the highest idle fraction (pure, tested)."""
    candidates = {c: f for c, f in idle_fractions.items() if c in allowed}
    if not candidates:
        raise ValueError("no allowed CPU has a measured idle fraction")
    return max(candidates, key=lambda c: candidates[c])


def measure_idle_fractions(sample_s: float = 0.5) -> dict[int, float]:
    def snapshot() -> dict[int, tuple[int, int]]:
        out = {}
        for line in Path("/proc/stat").read_text().splitlines():
            parts = line.split()
            if len(parts) > 4 and parts[0].startswith("cpu") and parts[0][3:].isdigit():
                vals = [int(v) for v in parts[1:]]
                out[int(parts[0][3:])] = (sum(vals), vals[3] + vals[4])
        return out

    first = snapshot()
    time.sleep(sample_s)
    second = snapshot()
    fractions = {}
    for cpu, (total2, idle2) in second.items():
        total1, idle1 = first[cpu]
        if total2 > total1:
            fractions[cpu] = (idle2 - idle1) / (total2 - total1)
    return fractions


def bounds_for(backend: str, cpu: int, timeout_s: float, memory_mb: int,
               kvm_memory_max: str, output_cap: int) -> "Bounds":
    """Per-backend bounds.

    KVM rows run with NO RLIMIT_AS (RLIM_INFINITY) and memory bounded by a
    cgroup instead: reverie#1225's AS1 fix makes Hermit refuse KVM when the
    host soft RLIMIT_AS is finite (exit 122), and the earlier 16 GiB limit
    intermittently failed guest execve (ENOMEM, exit 126; 0/88 unlimited).
    Other backends keep their 16 GiB RLIMIT_AS -- simpler, and no backend
    there has refused it. MemoryMax=16G size: measured KVM peak RSS was
    6,353,424 KiB (~6.2 GiB) on the jq seed cell and 28 MiB on /bin/true
    (GNU time maxrss), so 16G gives ~2.5x headroom over the worst seed."""
    if backend == "kvm":
        return Bounds(cpu, timeout_s, memory_mb, output_cap,
                      memory_mode="cgroup", cgroup_memory=kvm_memory_max)
    return Bounds(cpu, timeout_s, memory_mb, output_cap)


def launch_command(cmd: list[str], bounds: "Bounds") -> list[str]:
    """The argv actually exec'd for a sample (pure; tested)."""
    if bounds.memory_mode == "cgroup":
        return ["systemd-run", "--user", "--scope", "-p",
                f"MemoryMax={bounds.cgroup_memory}", "--", *cmd]
    return list(cmd)


class Bounds:
    def __init__(self, cpu: int, timeout_s: float, memory_mb: int, output_cap: int,
                 memory_mode: str = "rlimit", cgroup_memory: str = "16G"):
        self.cpu = cpu
        self.timeout_s = timeout_s
        self.memory_mb = memory_mb
        self.output_cap = output_cap
        self.memory_mode = memory_mode
        self.cgroup_memory = cgroup_memory

    def preexec(self) -> None:  # runs in the child before exec
        os.setsid()
        os.sched_setaffinity(0, {self.cpu})
        # No RLIMIT_CPU: LiteInst (correctly) refuses a finite physical CPU
        # limit, which would leak host timing into the deterministic guest.
        # The wall timeout above is the CPU-time backstop.
        if self.memory_mode == "rlimit":
            mem = self.memory_mb * 1024 * 1024
            resource.setrlimit(resource.RLIMIT_AS, (mem, mem))
        # cgroup mode: RLIMIT_AS stays RLIM_INFINITY; MemoryMax bounds memory.

    def describe(self) -> dict:
        if self.memory_mode == "cgroup":
            memory = (f"APPLIED:cgroup MemoryMax={self.cgroup_memory} via "
                      "systemd-run --user --scope (RLIMIT_AS=RLIM_INFINITY; reverie#1225 AS1)")
            cgroup = "APPLIED:systemd-run --user --scope for the sample"
        else:
            memory = f"APPLIED:RLIMIT_AS={self.memory_mb}MiB"
            cgroup = "NOT_APPLIED:/sys/fs/cgroup not writable by invoking user; rlimits+affinity are the replacement"
        return {
            "bound.wall": f"APPLIED:{self.timeout_s:g}s process-group SIGKILL",
            "bound.cpu": f"APPLIED:K=1 quietest-cpu={self.cpu} sched_setaffinity; RLIMIT_CPU NOT_APPLIED (LiteInst refuses finite physical RLIMIT_CPU; wall timeout is the backstop)",
            "bound.memory": memory,
            "bound.output": f"APPLIED:cap={self.output_cap}B per stream",
            "bound.cgroup": cgroup,
        }


def load_workloads(spec: dict) -> list[dict]:
    """Validate a workload-spec document (pure; tested).

    Format: {"workloads": [{"name": str, "argv": [str, ...],
                            "prepare_argv": [str, ...] (optional)}]}
    """
    entries = spec.get("workloads")
    if not isinstance(entries, list) or not entries:
        raise ValueError("spec needs a non-empty 'workloads' list")
    seen: set[str] = set()
    for entry in entries:
        if not isinstance(entry, dict) or not isinstance(entry.get("name"), str):
            raise ValueError(f"workload entry lacks a string name: {entry!r}")
        argv = entry.get("argv")
        if not isinstance(argv, list) or not argv or not all(
                isinstance(a, str) for a in argv):
            raise ValueError(f"workload {entry['name']}: argv must be a non-empty string list")
        if entry["name"] in seen:
            raise ValueError(f"duplicate workload name: {entry['name']}")
        seen.add(entry["name"])
    return entries


def prepare_command(entry: dict) -> list[str] | None:
    """The untimed fixture-prepare argv for a spec entry, if it declares one.

    Mirrors the e2e harness: a `--run` shell script is first invoked once
    with `--prepare` (same script, E2E_FIXTURE_DIR set) to build its guests.
    """
    if entry.get("prepare_script") and len(entry["argv"]) >= 2:
        return [entry["argv"][0], entry["argv"][1], "--prepare"]
    return entry.get("prepare_argv")


def run_sample(cmd: list[str], bounds: Bounds, expect_stdout: str | None,
               extra_env: dict[str, str] | None = None) -> dict:
    """One timed direct-child run. Returns wall/cpu, status, and stdout."""
    with tempfile.TemporaryFile() as out_f, tempfile.TemporaryFile() as err_f:
        env = {k: v for k, v in os.environ.items()
               if k not in ("RUST_LOG", "HERMIT_LOG", "HERMIT_LOG_FILE")}
        env["LC_ALL"] = "C"
        if extra_env:
            env.update(extra_env)
        rusage0 = resource.getrusage(resource.RUSAGE_CHILDREN)
        start = time.perf_counter_ns()
        proc = subprocess.Popen(launch_command(cmd, bounds),
                                stdout=out_f, stderr=err_f, env=env,
                                cwd=ROOT, preexec_fn=bounds.preexec)
        try:
            proc.wait(timeout=bounds.timeout_s)
            status = "ok" if proc.returncode == 0 else f"exit:{proc.returncode}"
        except subprocess.TimeoutExpired:
            os.killpg(proc.pid, signal.SIGKILL)
            proc.wait()
            status = "timeout"
        wall_s = (time.perf_counter_ns() - start) / 1e9
        rusage1 = resource.getrusage(resource.RUSAGE_CHILDREN)
        cpu_s = (rusage1.ru_utime + rusage1.ru_stime) - (rusage0.ru_utime + rusage0.ru_stime)
        out_f.seek(0)
        stdout = out_f.read(bounds.output_cap + 1)
        if len(stdout) > bounds.output_cap:
            status = "output_cap"
        text = stdout.decode(errors="replace").strip()
    if status == "ok" and expect_stdout is not None and text != expect_stdout:
        status = f"stdout_mismatch:{text[:80]!r}"
    return {"wall_s": wall_s, "cpu_s": cpu_s, "status": status, "stdout": text}


def hermit_cmd(hermit: Path, backend: str, prog: list[str]) -> list[str]:
    if backend == "native":
        return prog
    cmd = [str(hermit), "--log=error", "--backend", backend,
           "run", "--strict", "--epoch", EPOCH]
    if backend == "liteinst":
        cmd.append("--max-timeslice=disabled")
    return cmd + ["--"] + prog


def measure(cmd: list[str], bounds: Bounds, expect: str | None,
            iterations: int, warmups: int,
            extra_env: dict[str, str] | None = None) -> dict:
    for _ in range(warmups):
        run_sample(cmd, bounds, expect, extra_env)
    rows = [run_sample(cmd, bounds, expect, extra_env) for _ in range(iterations)]
    ok = [r for r in rows if r["status"] == "ok"]
    result: dict = {"command": cmd, "rows": rows, "status": "ok" if len(ok) == len(rows) else "failed"}
    if ok:
        result["wall"] = median_iqr([r["wall_s"] for r in ok])
        result["cpu"] = median_iqr([r["cpu_s"] for r in ok])
    return result


def git_sha() -> str:
    try:
        return subprocess.run(["git", "rev-parse", "HEAD"], cwd=ROOT,
                              capture_output=True, text=True, check=True).stdout.strip()
    except Exception:
        return "unknown"


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--hermit", type=Path, default=ROOT / "target/release/hermit")
    ap.add_argument("--backends", default="native,ptrace,kvm,liteinst")
    ap.add_argument("--iterations", type=int, default=7)
    ap.add_argument("--warmups", type=int, default=1)
    ap.add_argument("--timeout", type=float, default=120.0)
    ap.add_argument("--memory-mb", type=int, default=16384)
    ap.add_argument("--kvm-memory-max", default="16G",
                    help="cgroup MemoryMax for KVM rows (default 16G; see bounds_for)")
    ap.add_argument("--output-cap", type=int, default=8 * 1024 * 1024)
    ap.add_argument("--cpu", type=int, default=None)
    ap.add_argument("--getpid-counts", default="0,10000,50000,100000")
    ap.add_argument("--output-dir", type=Path, default=ROOT / "benchmarks/results/direct")
    ap.add_argument("--workloads", type=Path, default=None,
                    help="workload-spec JSON (see benchmarks/seed_workloads.json)")
    args = ap.parse_args()

    backends = tuple(b.strip() for b in args.backends.split(",") if b.strip())
    bad = set(backends) - set(BACKENDS)
    if bad:
        ap.error(f"unknown backends: {sorted(bad)}")
    cpu = args.cpu if args.cpu is not None else pick_quiet_cpu(
        measure_idle_fractions(), set(os.sched_getaffinity(0)))
    def make_bounds(backend: str) -> Bounds:
        return bounds_for(backend, cpu, args.timeout, args.memory_mb,
                          args.kvm_memory_max, args.output_cap)

    bounds = make_bounds(backends[0] if backends else "native")
    counts = [int(c) for c in args.getpid_counts.split(",")]

    fixture_src = ROOT / "benchmarks/fixtures/getpid_loop.c"
    args.output_dir.mkdir(parents=True, exist_ok=True)
    fixture = args.output_dir / "getpid_loop"
    subprocess.run(["cc", "-O2", "-std=c11", "-Wall", "-Wextra", "-Werror",
                    str(fixture_src), "-o", str(fixture)], check=True)

    load_start = os.getloadavg()
    results: dict = {
        "schema": "hermit-direct-run/v1",
        "hermit_sha": git_sha(),
        "hermit_binary": str(args.hermit),
        "host_short": platform.node().split(".")[0],
        "kernel": platform.release(),
        "cpu_chosen": cpu,
        "iterations": args.iterations,
        "warmups": args.warmups,
        "bounds": bounds.describe(),
        "bounds_kvm": make_bounds("kvm").describe(),
        "bounds_note": "KVM rows: cgroup MemoryMax (--kvm-memory-max), no RLIMIT_AS (reverie#1225 AS1); other backends: RLIMIT_AS --memory-mb",
        "relaxations": {"liteinst": "--max-timeslice=disabled", "others": "none (--strict)"},
        "loadavg_start": load_start,
        "workloads": {},
    }

    # Startup: hermit run -- /bin/true (native: direct /bin/true).
    startup = {}
    for b in backends:
        startup[b] = measure(hermit_cmd(args.hermit, b, ["/bin/true"]), make_bounds(b),
                             "", args.iterations, args.warmups)
        print(f"startup {b}: {startup[b].get('wall', {}).get('median')}", file=sys.stderr)
    results["workloads"]["startup_true"] = startup

    # Per-syscall slope from getpid_loop medians at positive counts.
    getpid: dict = {}
    for b in backends:
        per_count = {}
        for n in counts:
            per_count[str(n)] = measure(
                hermit_cmd(args.hermit, b, [str(fixture), str(n)]), make_bounds(b),
                f"calls={n}", args.iterations, args.warmups)
        xs = [float(n) for n in counts if n > 0]
        ys = [per_count[str(int(x))]["wall"]["median"] for x in xs
              if per_count[str(int(x))]["status"] == "ok"]
        entry: dict = {"per_count": per_count}
        if len(ys) == len(xs) and len(xs) >= 2:
            slope_s, intercept_s = fit_slope(xs, ys)
            entry["slope_us_per_call"] = slope_s * 1e6
            entry["intercept_s"] = intercept_s
        getpid[b] = entry
        print(f"getpid {b}: slope_us={entry.get('slope_us_per_call')}", file=sys.stderr)
    results["workloads"]["getpid"] = getpid

    # Optional workload spec (e.g. benchmarks/seed_workloads.json): full,
    # unshrunk cell programs timed per backend at K=1. Declared concurrency
    # is 1 (serial); per-sample containment is the Bounds above.
    if args.workloads:
        spec = load_workloads(json.loads(args.workloads.read_text()))
        e2e_tmp = str((args.output_dir / "e2e-tmp").resolve())
        Path(e2e_tmp).mkdir(parents=True, exist_ok=True)
        print(f"workload sweep: {len(spec)} workloads x {len(backends)} backends "
              f"x {args.iterations} reps, concurrency=1, K=1 cpu={cpu}, "
              f"load={os.getloadavg()}", file=sys.stderr)
        for entry in spec:
            entry = {**entry,
                     "argv": [a.replace("{OUTPUT_DIR}", str(args.output_dir))
                              for a in entry["argv"]],
                     **({"prepare_argv": [a.replace("{OUTPUT_DIR}", str(args.output_dir))
                                          for a in entry["prepare_argv"]]}
                        if entry.get("prepare_argv") else {})}
            fixture_dir = str((args.output_dir / "fixtures" /
                               entry["name"].replace("/", "_")).resolve())
            Path(fixture_dir).mkdir(parents=True, exist_ok=True)
            spec_env = {"E2E_TMPDIR": e2e_tmp, "E2E_FIXTURE_DIR": fixture_dir}
            prep = prepare_command(entry)
            if prep:
                subprocess.run(prep, cwd=ROOT, check=True,
                               env={**os.environ, **spec_env})
            per_backend = {}
            for b in backends:
                per_backend[b] = measure(
                    hermit_cmd(args.hermit, b, entry["argv"]), make_bounds(b), None,
                    args.iterations, args.warmups,
                    extra_env=spec_env)
                wall = per_backend[b].get("wall", {}).get("median")
                print(f"workload {entry['name']} {b}: {wall}", file=sys.stderr)
            results["workloads"][entry["name"]] = per_backend
    results["loadavg_end"] = os.getloadavg()

    out = args.output_dir / "results.json"
    out.write_text(json.dumps(results, indent=2) + "\n")
    print(f"wrote {out}")
    return 0 if all(v.get("status") == "ok" for w in results["workloads"].values()
                    for v in w.values()) else 1


if __name__ == "__main__":
    raise SystemExit(main())
