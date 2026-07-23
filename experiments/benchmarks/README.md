# Hermit determinization overhead — baseline

This directory records a first baseline of Hermit's runtime overhead so future
optimization work has something to measure against. It compares, for each
workload, running the program three ways:

- **native** — the program directly, no Hermit;
- **`--strict`** — `hermit run -- …` (strict determinism is the default);
- **non-strict** — `hermit run --no-sequentialize-threads --no-deterministic-io -- …`
  (the documented determinism opt-outs; there is no literal `--no-strict` flag).

## How to reproduce

```bash
# from the repo root, with a release build at target/release/hermit
./experiments/benchmarks/bench.sh | tee experiments/benchmarks/results.txt
```

`bench.sh` has no external dependencies (no `hyperfine` required). For each
workload it runs every mode `N` times, discards nothing, and reports the
**minimum** wall-clock time (the most stable statistic for short commands) and
the overhead ratio versus native. It first runs a one-shot `precheck` of every
mode and aborts if any mode does not exit 0, so a silently failing guest is
never recorded as a bogus "fast" result. Override the binary with
`HERMIT=/path/to/hermit` and the bzip2 input with `BENCH_DATA=/path/to/file`.

## Environment

| | |
| --- | --- |
| Host | `devbig030` |
| CPU | AMD EPYC 9D85 (Zen 5) |
| Kernel | 6.13.2 |
| Hermit | `target/release/hermit` (release build) |
| Date | 2026-07-22 |

This is a shared developer machine, so treat the absolute numbers as indicative
and the **ratios** as the takeaway.

## Results

Minimum wall-clock seconds over `N` runs; `(x)` is overhead versus native.

| Workload | N | native (s) | `--strict` (s) | strict × | non-strict (s) | non-strict × |
| --- | --: | --: | --: | --: | --: | --: |
| `true` | 20 | 0.0027 | 0.0098 | **3.6×** | 0.0092 | 3.3× |
| `echo hello` | 20 | 0.0031 | 0.0131 | **4.2×** | 0.0141 | 4.5× |
| `ls -la /usr/bin` | 20 | 0.0103 | 0.0837 | **8.1×** | 0.0755 | 7.3× |
| `bzip2 -c` (2 MB) | 3 | 0.1283 | 6.2549 | **48.7×** | 5.1290 | 39.9× |
| `sh` spawn ×200 | 5 | 0.1039 | 1.1008 | **10.5×** | 0.6652 | 6.3× |

## Analysis

- **Fixed cost ≈ 7–10 ms.** `true` and `echo` are dominated by Hermit's
  process startup/teardown (namespace setup, tracer/scheduler spin-up). In
  ratio terms that is a large multiplier (3–5×) but a tiny absolute cost, so it
  is negligible for anything that runs longer than a few tens of milliseconds.

- **Per-syscall cost shows up around 8×.** `ls -la` does a directory read plus
  many `stat`/`write` syscalls; each subscribed syscall is a seccomp stop, a
  context switch to the out-of-process tracer, and a scheduler turn.

- **CPU-bound code is the worst case: ~49×.** `bzip2` is nearly pure
  computation with few syscalls, so the cost is not per-syscall — it is
  RCB-based preemption. With sequentialized scheduling, Hermit arms a
  retired-conditional-branch timer and takes a tracer turn every timeslice even
  though the guest never enters the kernel. A tight compute loop therefore pays
  continuously. **This is the headline result for optimization: compute-heavy,
  syscall-light workloads dominate overhead.**

- **fork/exec/wait is expensive, and this is where non-strict helps most.** The
  200-process spawn loop is ~10.5× under strict but ~6.3× non-strict — dropping
  sequentialized scheduling nearly halves it, because serializing every
  process/thread transition is the dominant cost for multi-process workloads.
  For CPU-bound `bzip2`, non-strict also helps (39.9× vs 48.7×) but does not
  remove the RCB-preemption tax. For sub-10 ms workloads, strict and non-strict
  are within run-to-run noise.

### Worst-case workloads

1. **CPU-bound / syscall-light (`bzip2`, ~49×)** — RCB preemption tax.
2. **fork/exec/wait-heavy (process spawning, ~10×)** — scheduler serialization.
3. **syscall-heavy small tools (`ls`, ~8×)** — per-syscall tracer round-trips.

## Caveats and notes

- **LULESH and `ninja_test` are not in this checkout.** They are referenced by
  in-flight PR branches but were not buildable here, so this baseline uses
  clearly-labeled stand-ins: `bzip2` for the CPU-bound (LULESH-like) category
  and a 200-iteration process-spawn loop for the many-small-processes
  (`ninja_test`-like) category. Re-run `bench.sh` with the real workloads once
  they land to replace these rows.
- **`/tmp` is overlaid.** Hermit hides the guest's `/tmp` behind a private
  tmpfs, so a host `/tmp/...` input path is invisible to the guest. The bzip2
  input therefore lives under `$HOME`; an early version of this benchmark
  silently mismeasured bzip2 because the guest could not open a `/tmp` input.
  The harness now fails closed on any non-zero exit.
- **CPUID interception is unavailable on this host.** `devbig030` is AMD on
  kernel 6.13, which predates AMD user-space CPUID-faulting support (Linux
  6.17), so `--strict` prints `Unable to intercept CPUID` and runs without CPUID
  virtualization. This is a *determinism* caveat, not a performance one — CPUID
  faulting is essentially free — but it means the `--strict` rows here are not
  paying for CPUID emulation.
- Timing uses `date +%s.%N` around each run with guest stdout and Hermit stderr
  discarded; `min` is reported to reduce scheduler noise on a shared host.
