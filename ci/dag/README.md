# CI validation lanes as safe-ci-dag-runner DAGs

This directory holds a declarative migration of Hermit's CI validation lanes
onto [`safe-ci-dag-runner`](../../agent-utils/common/docs/safe-ci-dag-runner/README.md)
(from the `agent-utils` submodule). Each validation *gate* becomes a DAG node
with explicit dependencies and resource limits, so the scheduler can run
independent gates concurrently. On hosts with delegated cgroup v2 support, it
can also box each node for memory limits and full process-subtree teardown.

- [`portable.json`](portable.json) — drives `validate.sh`'s **`--portable-only`**
  lane and the GitHub-managed portable `regular` job in
  [`.github/workflows/ci-portable.yml`](../../.github/workflows/ci-portable.yml).
  No PMU / CPUID interception required.
- [`privileged.json`](privileged.json) — implements the focused capability
  contract for the privileged job in
  [`.github/workflows/ci-privileged.yml`](../../.github/workflows/ci-privileged.yml).
  Requires PMU + `/dev/kvm`.

Run a lane with the wrapper:

```sh
ci/run-dag.sh portable   --max-mem 32G          # memory-aware -j
ci/run-dag.sh privileged -j 2                    # PMU lane, one gate at a time
ci/run-dag.sh portable   ascii                   # visualize instead of run
```

## Status: active validation lanes

`portable.json` drives the required `Regular tests (GitHub-managed portable)` job, and
`privileged.json` drives both privileged PMU entrypoints. Existing job names,
the merge-gate contract, and the outer PMU `flock` stay unchanged; only the
internal scheduler changes. The DAG files are the load-bearing source of truth
for individual gate commands; `validate.sh` delegates to them.

The privileged DAG is limited to the focused build, CPUID faulting, PMU skid,
manifest validation, and KVM E2E cells so the required self-hosted smoke stays
within its 270-second workflow bound. Each sequential build/KVM segment is
capped at 120 seconds, yielding a 240-second maximum DAG timeout path; the
manifest audit recomputes and enforces that bound. The 139-program
record/replay ratchet is preserved as a separate step in the long merge-group
validation job.

Closed pull requests trigger a skipped workflow in the same concurrency group,
which cancels their queued or in-progress PMU run. The `mem_race` family and
three nonblocking post-DAG diagnostics run in the scheduled `super` tier so a
known host-sensitive hang cannot consume the required serialized lane.

The `Validation Levels` workflow no longer launches a second copy of
`--portable-only` for every pull request. Its quick lane remains available by
manual dispatch, while merge-group privileged and scheduled super validation are
unchanged. The manual [`ci-dag.yml`](../../.github/workflows/ci-dag.yml)
workflow runs either DAG on demand.

### Runner dependency

This change pins `rrnewton/agent-utils` at v0.2.0 as an HTTPS submodule. Portable
CI initializes only `agent-utils` instead of all submodules, then executes the
dependency-free Python runner so per-node performance CSVs are available
without an install step. `ci/run-dag.sh` also accepts
`SAFE_CI_DAG_RUNNER` for local or preinstalled binaries.

## Speed-to-signal audit

A successful PR run on 2026-07-26 provided the baseline:

- The blocking portable validation took 14 minutes after setup.
- Eight nonblocking diagnostics then ran serially for another 20 minutes,
  extending the required workflow from useful signal at minute 17 to completion
  at minute 37.
- `Validation Levels` independently repeated the same 14-minute portable suite,
  consuming another GitHub-managed portable runner.

The diagnostics now run in the scheduled `super` tier. The required lane uses a 14 GiB memory budget, which the current model
maps to `-j 2` on the 16 GiB portable runner. Compile, lint, documentation, unit,
and contract nodes may overlap when dependencies allow, while Hermit guest
executions retain the
`hermit_guest: 1` exclusion. Per-node performance reports are uploaded from
every required run so estimates can be replaced with measurements.

## How gates map onto the DAG

`validate.sh` already encodes a hand-rolled DAG:

| `validate.sh` construct        | DAG equivalent                                   |
| ------------------------------ | ------------------------------------------------ |
| `run_check NAME cmd…`          | one node (serial via a shared scarce resource)   |
| `start_check NAME cmd…`        | one node with no scarce resource (parallelizes)  |
| `wait_for_background_checks`   | implicit — the scheduler joins on all nodes      |
| ordering "build, then the rest"| `deps: ["build.workspace"]`                       |

Each node's tag is `group.job` (e.g. `build.workspace`, `lint.clippy`).

### Command fidelity

Node `cmd`s are the **verbatim** commands `validate.sh` runs, with three
deliberate exceptions, chosen to avoid duplicating script logic that has many
moving parts:

- **Composite envelope gates reuse `validate.sh`'s own standalone entrypoints**
  so there is one source of truth: `test.strict_compat` runs
  `./validate.sh --portable-strict-compat-only`, and (privileged) `rr.compat_baseline`
  runs `./validate.sh --rr-compat-only`. The privileged selector builds release;
  portable strict compatibility reuses `STRICT_COMPAT_HERMIT_BIN` from the
  preceding workspace build. Without that override, the strict flag builds
  release as before.
- **The DBI stderr-isolation CLI case is a separate 120-second node** so a
  backend hang fails quickly without consuming the aggregate CLI budget. The
  aggregate node skips that case, so the test set remains unchanged.
- **Portable strict compatibility starts after every non-guest Cargo node** so
  its `shell-build` run1/run2 comparison cannot observe concurrent target or
  cache mutation. Those short nodes still run in parallel before the barrier.
- **Hermit integration targets use one Cargo invocation** with repeated
  `--test` selectors (`test.hermit_integration` and `hw.integration`). Cargo
  plans and links the selected targets together, then executes their separate
  test binaries serially. The `pmu.*` exact-case gates retain their `for` loops
  and per-case `timeout`s to preserve fail-fast hardware isolation.
- **The portable `envelope_levels` gate is inlined** (L1–L4 over the three
  `ENVELOPE_PROBES`: `true`, `echo`, `date`) because it has no standalone
  `validate.sh` flag. It mirrors `run_portable_envelope_levels` in `validate.sh`.
  If `ENVELOPE_PROBES` changes in `validate.sh`, update this node.

## Resource model (outer + inner limits)

The task's "outer + inner resource limits" map onto the runner's two knobs:

**Outer** — how many gates may co-run:

- `resource_caps` gates *scarce* resources. `portable.json` sets
  `{"hermit_guest": 1}`; every gate that executes Hermit on guest programs
  carries `resources: {"hermit_guest": 1}`, so they run **one at a time**
  (they share the working filesystem, are mutually nondeterministic, and on a
  PMU host contend for the counter). Non-guest gates — `build`, `clippy`,
  `rustfmt`, doctests, `rustdoc`, nextest of non-Hermit crates — carry no
  scarce resource and parallelize freely. `privileged.json` uses `{"pmu": 1}`
  the same way; the PMU is genuinely exclusive, so that lane is essentially
  serial after the initial builds.
- `--max-mem SPEC` (or `-j N`) bounds total concurrency. With `--max-mem`, the
  runner picks the largest `-j` whose modeled worst-case footprint (summed
  `rss_baseline_bytes` of a schedulable set) fits the budget.

**Inner** — each gate's own box:

- `rss_baseline_bytes` — estimated peak RSS, the input to `-j` sizing.
- `hard_mem_max_bytes` — explicit inner cgroup `MemoryMax` (applied only under
  `--cgroups`); a gate that exceeds it is OOM-killed **in isolation** rather
  than taking down the run.
- `est_duration_s` — orders ready gates longest-first (packing only; never a
  correctness contract).
- `classification` — `cpu-bound` (compiles, PMU compute), `latency-bound`
  (guest execution / I/O), or `light` (fmt, contract checks).

> **The `rss_baseline_bytes` / `hard_mem_max_bytes` / `est_duration_s` values
> are hand-estimated, not measured.** They are safe starting points for
> `--max-mem` sizing and inner caps, not benchmarks. Refine them from a real
> run's `--perf-dir` CSVs (`ci/run-dag.sh portable --perf-dir ./perf`) before
> relying on tight memory budgets.

## Conservatism and how to relax it

The `hermit_guest: 1` / `pmu: 1` serialization faithfully reproduces
`validate.sh`, which ran these gates strictly one-after-another. It is
intentionally conservative: as individual guest gates are shown to be safe to
co-run (e.g. distinct scratch directories, no shared fixture), drop their
`resources` hint (or raise the cap) to unlock more parallelism. The DAG shape
and dependencies stay the same.
