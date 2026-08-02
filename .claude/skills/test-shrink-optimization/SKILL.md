---
name: test-shrink-optimization
description: "Optimize or review slow Hermit tests and manifest workloads. Use when reducing inputs, iterations, or CI time while preserving real syscall, scheduler, JIT, runtime, and Hermit code-coverage surface, or when deciding whether an irreducibly slow test belongs in occasional validation."
---

# Test Shrink Optimization

Make a test lighter without making it shallower. Preserve the real program path
and its Hermit-observable behavior; do not replace it with a cheap launcher,
`--help`, `--version`, or no-op probe.

Read the [Hermit code-coverage guide](../../../docs/HERMIT_CODE_COVERAGE.md)
and the [end-to-end manifest policy](../../../tests/e2e/manifests/README.md)
before changing a test.

## Define Power And Weight

Treat test power as a vector. Do not collapse unlike evidence into one score.
Measure each component for every backend and mode the test claims to cover:

| Component | Preservation evidence |
| --- | --- |
| Syscall surface | Exact set of syscall types plus relevant result or event classes |
| Hermit state surface | Scheduler, resource, event, thread, process, signal, and race classes reached |
| Hermit code coverage | Exact normalized covered line and region sets |
| Runtime surface | Workload-specific milestones such as JIT tiers, GC, threads, subprocesses, file I/O, or sockets |
| Schedule exploration | Declared seeds and schedule diversity for chaos or race tests |

Raw syscall, turn, allocation, or loop counts may fall when repetition is
removed. The class sets and semantic milestones must not. Define the state
surface as the observable event and resource classes exercised; do not claim
that a finite trace proves an exhaustive mathematical state space.

Measure weight with an ordinary, uninstrumented release build on the same
commit, backend, mode, host class, and command line. Report the median of
repeated warm runs and record environmental limitations. Coverage builds are
too slow to use as timing evidence.

When rates are useful, report them separately, for example syscall types per
second, Hermit event classes per second, covered lines per second, covered
regions per second, and JIT-tier milestones per second. Never add these units
into a synthetic total.

## Shrink Workflow

### 1. Freeze A Baseline

Record:

- The exact Hermit SHA, test source, manifest entry, backend, mode, flags, seed,
  host, runtime/toolchain version, and command.
- Median uninstrumented runtime from repeated warm runs.
- Raw logs and sorted syscall, scheduler/event, and runtime-milestone sets.
- A named Hermit coverage report.

Keep determinism settings fixed. In particular, do not change preemption,
verification, chaos, or backend flags merely to gain speed; that changes the
test surface rather than shrinking the workload.

Collect the baseline through the repository harness:

```bash
scripts/hermit-code-coverage.rs collect --name <test>-baseline -- \
  --backend <backend> <hermit-args> -- <program> <baseline-args>
```

For wrapper scripts, use `--command`; the harness supplies `HERMIT_BIN` and
`HERMIT_COVERAGE_BIN`. Preserve command output and logs under a named evidence
directory when the task requires a durable audit trail.

### 2. Shrink One Axis At A Time

Try the least semantic change first:

1. Reduce the input size or data volume.
2. Reduce repeated iterations after the behavior has activated.
3. Reduce worker count only when the same concurrency and synchronization
   classes remain exercised.
4. Replace a broad workload with a narrower real workload that reaches the same
   required runtime and Hermit surface.

Binary-search an activation threshold when behavior is tiered or delayed, such
as JIT compilation, GC, thread contention, or a queue transition. Keep a clear
margin above that threshold so normal run-to-run variation cannot turn the test
into a startup-only probe.

Do not shrink all dimensions together. A one-axis change makes any lost surface
attributable and makes the next candidate easier to reason about.

### 3. Measure The Candidate

Use the same Hermit source and baseline conditions. If only the guest workload
changed, reuse the instrumented build with `--no-build`:

```bash
scripts/hermit-code-coverage.rs collect --name <test>-candidate --no-build -- \
  --backend <backend> <hermit-args> -- <program> <candidate-args>

scripts/hermit-code-coverage.rs diff \
  --baseline <test>-baseline \
  --candidate <test>-candidate \
  --fail-on-loss
```

Compare exact normalized covered line and region sets. Equal percentages are
not evidence of equal coverage. Repeat collection for every claimed backend;
coverage under ptrace does not prove coverage under KVM, DBI, SaBRe, or
LiteInst.

Derive sorted syscall and event-class sets from the same logging mode for both
runs. Prefer structured trace or repository parsers. If none exists, retain the
raw logs beside the documented extraction command so a reviewer can reproduce
the comparison.

### 4. Apply The Acceptance Gates

Accept a shrink only when all of these hold:

- The candidate has a clear uninstrumented runtime reduction.
- The backend, Hermit mode, determinism flags, seed policy, and runtime version
  are unchanged.
- No syscall type or required result/event class disappears.
- `hermit-code-coverage diff --fail-on-loss` reports no normalized line or
  region loss.
- Every declared runtime milestone still occurs with threshold headroom.
- Concurrency, signal, process, I/O, or race behavior named by the test still
  executes rather than merely initializes.
- Chaos and race tests retain their declared schedule-search contract.

Reject the candidate when startup happens to produce the same syscall set but
the program no longer executes its meaningful bytecode, native code, JIT path,
thread interaction, I/O, or failure mode. Real execution is part of coverage.

### 5. Choose The Validation Tier

Keep a power-preserving candidate in the regular manifest when it meets the CI
budget. If meaningful shrink attempts still leave it too slow, keep the real
workload and mark the manifest entry `occasional = true` with a nonempty
`slow_reason`. Leave its relevant mode `ci = true` so it runs when occasional
validation is requested.

Validate and exercise that placement:

```bash
./ci/test_harness.sh validate
./ci/test_harness.sh plan --lane <portable-or-privileged>
./ci/test_harness.sh run --mode <mode> --backend <backend> \
  --include-occasional --test <category/test>
```

Occasional status is not permission to weaken the workload. It records that a
high-value, irreducibly expensive test runs at a lower cadence.

### 6. Record The Score

Add a dated score comment next to the test's manifest entry. Include the Hermit
SHA, backend/mode, host class, baseline and candidate inputs, median runtimes,
speedup, exact set comparison, coverage set comparison, semantic milestones,
and evidence names or commands.

```toml
# Power/weight 2026-08-02 @ Hermit <sha>, ptrace verify, <host>:
# input 10000 -> 1200; median 42.1s -> 7.4s (5.7x); syscall types
# 51 -> 51 (same set); Hermit lines/regions 812/1160 -> 812/1160
# (no losses); JIT C1/C2 plus thread and GC milestones preserved.
```

Use actual measured values. Update the date and score when later changes alter
the workload, coverage, or runtime materially. Do not commit generated coverage
or build output from `target/`.

## JVM Worked Example

Use the workloads in
[app_strict_verify.rs](../../../hermit-cli/tests/app_strict_verify.rs) and the
evidence from [Hermit PR #1163](https://github.com/rrnewton/hermit/pull/1163)
as the `small-jvm-jit-runtime-compat` model.

The investigation found that `java -version` was cheap and reached roughly the
same unique syscall-type set as real programs, but executed no application
bytecode and triggered no JIT compilation. It was therefore not a valid
replacement for a JVM compatibility test.

Small real programs separated the required surfaces:

- `Hello` proved bytecode execution and basic JVM startup.
- `JitHotLoop` was the cheapest workload that reliably crossed tiered JIT
  thresholds, including C2.
- `ThreadCounter` preserved application-level threads and synchronization.
- Larger hash-map, GC, NIO, socket, and `javac` workloads added useful surface
  but were much heavier and belonged in targeted or occasional validation when
  their unique behavior was required.

The method is to reduce each program's loop bound or data size, confirm its JIT,
GC, and thread milestones still occur with margin, then compare syscall and
Hermit coverage sets. Do not infer runtime coverage from the syscall set alone.
Keep deterministic RCB preemption enabled: disabling the timeslice changed the
execution behavior and could livelock a JIT-enabled JVM, so it was not a valid
test optimization.

## Handoff Checklist

Before claiming a shrink is complete, provide:

- The baseline and candidate commands and exact Hermit SHA.
- Per-backend uninstrumented median runtimes and speedup.
- Exact syscall/event set comparison and retained semantic milestones.
- Named coverage reports and a successful `--fail-on-loss` diff.
- The dated manifest score comment.
- Manifest validation, including an `--include-occasional` run when applicable.

If any preservation gate is unmeasured, report the shrink as a hypothesis, not
as an optimization ready to land.
