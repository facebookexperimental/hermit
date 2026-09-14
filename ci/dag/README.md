# CI validation lanes as dagrun DAGs

This directory holds a declarative migration of Hermit's CI validation lanes
onto [`dagrun`](../../agent-utils/common/docs/dagrun/README.md)
(from the `agent-utils` submodule). Each validation *gate* becomes a DAG node
with explicit dependencies and resource limits, so the scheduler can run
independent gates concurrently. On hosts with delegated cgroup v2 support, it
can also box each node for memory limits and full process-subtree teardown.

- [`validate.json`](validate.json) — the one committed superset. Steps declare
  `quick`, `portable`, `hosted-portable`, `full`, `super`, `privileged`, and
  `hosted-privileged` labels; dagrun selects the requested label and its
  dependency ancestry without rewriting the graph.

Run a lane with the wrapper:

```sh
ci/run-dag.sh portable   --max-mem 32G          # memory-aware -j
ci/run-dag.sh privileged -j 2                    # PMU lane, one gate at a time
agent-utils/py/bin/dagrun ascii --dag ci/dag/validate.json  # inspect the superset
```

## Status: active local lanes and manual hosted diagnostics

`scripts/validate.rs` reads `validate.json` and selects local labels.
`ci/run-dag.sh portable` and `ci/run-dag.sh privileged` map to the explicit
`hosted-portable` and `hosted-privileged` labels in that same file. The hosted
nodes retain host commands and typed host dependencies; local labels select
their pinned-root counterparts. Standard profile execution does not merge lane
files, regenerate nodes, or rewrite commands and resource caps.

`generate-validation-dag --check` is a maintenance check, not part of runtime
plan construction. Static step definitions live as private typed generator
input in `ci/manifest-plan/src/validation_dag_static.rs`; generated manifest,
compatibility, stress, local pinned-root, and hosted variants are composed with
them to emit the entire file deterministically. `--check` regenerates from that
independent source and refuses any command, dependency, or cap drift in the
committed artifact. `--write` updates this same file; there is no secondary
runnable DAG.

The local privileged selection contains 19 nodes with a 4500-second critical
path. The separately labelled hosted privileged smoke preserves its historical
12-node population and has a 2100-second critical path, so the manual workflow
uses an audited 2160-second launcher bound. The stale hosted file omitted CPU
budgets and therefore inherited dagrun's 10-second fallback; every hosted node
now carries the corresponding current-plan CPU budget explicitly. This is a
correctness repair, not a claim that the obsolete fallback was equivalent.
Commands, dependencies, wall bounds, and the absence of hosted resource demands
remain checked against the hosted selection. The 139-program record/replay
ratchet remains a separate step in the manually dispatched full validation
job.

The `mem_race` family and three nonblocking post-DAG diagnostics run in the
manually dispatched `super` tier so a known host-sensitive hang cannot consume
the serialized capability lane unexpectedly.

The `Validation Levels` workflow does not launch for pull requests, `main`, or a
schedule. Its quick, privileged, and super levels remain available by manual
dispatch. The manual [`ci-dag.yml`](../../.github/workflows/ci-dag.yml)
workflow selects either the `hosted-portable` or `hosted-privileged` label from
the same committed superset on demand.

### Prepared Nextest executables

The workspace producers prepare each distinct Cargo test selection before its
Nextest consumers run. The committed graph records the exact Cargo selectors
for every counted runner, the embedded KVM inventory commands, and the direct
CPUID `tests_misc` lookup. The generator
checks those declarations against the command arguments and requires a producer
in each consumer's dependency ancestry. The full privileged barrier verifies
its three selections without rebuilding shared test executables.

`ci/nextest-binaries.rs prepare PROFILE` writes one atomic record for all of that
profile's selections. The record binds source trees, compiler identity, Cargo
configuration and build settings, actual Cargo target/package identities,
metadata, and the contents of test executables and recorded runtime files.
The `hermit_modes` preparation also builds all 21 Cargo guest fixtures together
and supplies their verified Cargo-reported paths to the test process. Missing,
stale, ambiguous, or wrong-target artifacts cause refusal; a consumer never
falls back to compiling them. The actual Cargo target directory is retained,
including a configured `CARGO_TARGET_DIR`; no guessed target remap is applied.

For an explicit local prepared run, first prepare a graph profile, then invoke
`ci/nextest-binaries.rs run` or `list` with the same Cargo selectors and desired
Nextest filters. `--profile ci` remains a Nextest runtime setting. Unsupported
build settings such as `--cargo-profile` and `--release` are refused rather than
silently interpreted as runtime options. Official counted runs retain their
versioned test results, exact selected counts, retries and failure status.
The prebuilt Rust-script producer prepares the helper itself before official
consumers; ordinary standalone Rust-script invocations can still compile the
helper during command lookup.

The quick build now depends on Nextest setup because preparation needs it.
This adds the existing 600-second setup timeout to its worst-case dependency
path (8580 to 9180 seconds); individual node timeouts and CPU caps are unchanged.
These sums are scheduling bounds, not measured preparation or execution times.
The pressure runner's batch preparation similarly retains the Nextest setup
prerequisite: ten nodes including LiteInst, or nine without it. This adds 600
seconds to those declared preparation paths (6000 and 5100 seconds), while
preserving the existing 7200-second whole-run bound and the smaller exact-cell
preparation sets. The pinned image already includes Nextest 0.9.100; its
separate mounted Cargo target keeps image metadata distinct from host metadata.

### Runner dependency

This change pins `rrnewton/agent-utils` at v0.2.0 as an HTTPS submodule. Portable
CI initializes only `agent-utils` instead of all submodules, then executes the
dependency-free Python runner so per-node performance CSVs are available
without an install step. `ci/run-dag.sh` also accepts
`DAGRUN_BIN` for local or preinstalled binaries.

## Speed-to-signal audit

A successful PR run on 2026-07-26 provided the baseline:

- The blocking portable validation took 14 minutes after setup.
- Eight nonblocking diagnostics then ran serially for another 20 minutes,
  extending the required workflow from useful signal at minute 17 to completion
  at minute 37.
- `Validation Levels` independently repeated the same 14-minute portable suite,
  consuming another GitHub-managed portable runner.

The diagnostics are available in the manually dispatched `super` tier. The portable plan uses a 14 GiB memory budget, which the current model
maps to `-j 2` on the 16 GiB portable runner. Compile, lint, documentation, unit,
contract, and Hermit guest nodes may overlap when dependencies and memory allow.
Per-node performance reports are uploaded from every run so estimates can be
replaced with measurements.

### Relationship to local validation

For every step assigned by `ci/portable-shards.json`, the step command,
dependencies inside its selected group, timeouts, CPU timeouts, and declared
resource limits are the same values returned by the constructed local plan.
`ci/check-shard-coverage.sh` refuses a hosted grouping unless every constructed
portable step is assigned exactly once and the preflight group contains the
constructed dependencies it needs.

The current relationships are:

- **Same:** Clippy and rustdoc refuse warnings through the constructed step
  commands, and the hosted environment also exports `-D warnings`. Every
  constructed nextest step uses `ci/run-nextest-counted.sh`, so zero executed
  tests are refused in both places.
- **Same policy, different host capacity:** local full validation and hosted
  selected runs both use `validate`'s host-derived outer width (`host_cpus/8`,
  floored at 2 and capped at 16) unless an operator supplies an explicit
  override. The measured 316-CPU development host therefore selects 16 while a
  typical 4-CPU GitHub runner selects 2; that difference follows from the same
  committed policy rather than a second hosted default.
- **Deliberately different:** local validation fails closed unless it establishes
  its two-level cgroup-v2 boxing. GitHub-hosted runners do not provide the needed
  delegated systemd user scope, so selected diagnostic runs explicitly permit an
  unboxed execution. The constructed limits remain declared and audited but are
  not enforced there.
- **Deliberately different:** hosted selected runs are off the record. They do
  not write a local validation receipt, ledger row, scorecard, or pull-request
  label.
- **Deliberately different:** `check.lint_checks` and
  `check.check_outcome_consumers` load their pinned authority from the private
  parent repository. The repository-scoped hosted token cannot read that other
  repository, and no cross-repository read secret is configured, so those two
  checks remain visible as red diagnostics. They run unchanged locally, and
  their hosted job does not prevent the remaining selected steps from running.
- **Unknown:** none after the current command and assignment audit. A future
  difference remains a defect until it is either removed or explained here.

## How gates map onto the DAG

`scripts/validate.rs` already encodes a hand-rolled DAG:

| `scripts/validate.rs` construct        | DAG equivalent                                   |
| ------------------------------ | ------------------------------------------------ |
| `run_check NAME cmd…`          | one node (serial via a shared scarce resource)   |
| `start_check NAME cmd…`        | one node with no scarce resource (parallelizes)  |
| `wait_for_background_checks`   | implicit — the scheduler joins on all nodes      |
| ordering "build, then the rest"| `deps: ["build.workspace"]`                       |

Each node's tag is `group.job` (e.g. `build.workspace`, `lint.clippy`).

### Manifest bucket fan-out

The centralized manifests use an explicit build barrier before execution:

1. `e2e.metadata` validates schema, inventory, generated test-footprint freshness,
   and CI correspondence.
2. `build.e2e_artifact` waits for both initial Cargo producers, verifies and
   hash-binds the debug Hermit plus the dereferenced `install_pkg` resource
   tree, then atomically publishes a content-addressed bundle. Every later
   shared-target Cargo writer waits on this barrier.
3. `build.manifest_guests` prepares every `ci=true` program once. One
   `e2e.manifest_<bucket>` node per YAML bucket declares both producers and runs
   through `run-with-hermit-e2e-artifact.sh`, which re-verifies identity before
   exporting exact `HERMIT_BIN` and `HERMIT_INSTALL_DIR` paths. Parallel Cargo
   tests may then relink `target/debug/hermit` or restage `target/install_pkg`
   without invalidating a running bucket.
4. `e2e.audit_compile_backend_parity_c` compiles every C guest that bucket
   declares, `ci=false` cells included. Nothing else in the DAG ever builds a
   disabled cell, so without this node a disabled fixture rots invisibly — it
   never reaches `-Werror`, and "the file is in the repo" quietly stops meaning
   "the file builds". It fails closed: zero guests compiled, or a filter that
   selects nothing, is a failure rather than a vacuous pass.

Every run node carries a structured `manifest` selector as well as its command.
`target/debug/test-harness audit-ci` derives the expected bucket set from the YAML manifests,
requires the command to be the canonical rendering of that selector, and
compares the aggregate selected cells with `ci/expected-e2e-plan.json`. Buckets
whose entries are still manual execute as explicit empty nodes; `--allow-empty`
cannot hide a blocking cell because the aggregate comparison is independent of
runtime output.

### Command fidelity

Node `cmd`s are the **verbatim** commands `scripts/validate.rs` runs. Portable
strict compatibility is committed as one fixture producer plus direct
`compat.*` steps. The stable `test.strict_compat` shard/selection alias expands
only to those already-committed steps; it never constructs or rewrites them.
- **The DBT stderr-isolation CLI case is a separate 120-second node** so a
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
  `scripts/validate.rs` flag. It mirrors `run_portable_envelope_levels` in `scripts/validate.rs`.
  If `ENVELOPE_PROBES` changes in `scripts/validate.rs`, update this node.

## Before you add a gate: scope is where gates go blind

Eight gates in this repository have been found reporting nothing while looking
like they passed. **Eight different causes, one outcome.** Not one of them was
disabled, and every one looked reasonable when it was written — which is the
reason to read this list before adding the ninth.

| # | Mechanism | What it looked like | What it actually covered |
|---|-----------|---------------------|--------------------------|
| 1 | **Trigger** | a portability workflow in `.github/workflows/` | `on: workflow_dispatch:` only, so it never fired |
| 2 | **Coverage** | the DAG "runs the backends" | it never ran `third-party-backends` |
| 3 | **Execution** | a budget wrapper around a command | the wrapped command never executed, for want of calibration |
| 4 | **Feature** | `cargo clippy --workspace --all-targets` | default features, so it never linted `dbt`/`sabre`/`e9patch` — every feature in the workspace |
| 5 | **Severity** | `cargo doc --workspace --no-deps` | exited 0 while emitting warnings; it rendered docs and could not fail on doc defects |
| 6 | **File type** | a repository-wide source check | it skipped whole extensions, so files outside its allowlist were invisible |
| 7 | **Repository boundary** | a pin checker in the parent tree | it saw the gitlink SHA but could not establish the pinned repository's content |
| 8 | **Ownership without execution** | an inventory entry naming a suite's maintainer | the file was accounted for, but no DAG node invoked it |

The shape they share: **a gate's scope is narrower than the claim its name
makes.** "Documentation", "Clippy", "portability" all sound total. Each was
partial, and nothing in the output said so.

### Four questions to answer for any new node

1. **Does it run?** Check the trigger and that the command is actually reached —
   not that the script containing it exists.
2. **Does it cover everything its name implies?** Features (`--all-features`),
   targets (`--all-targets`), workspace members, lanes. Note that
   `cargo --workspace` does **not** reach nested workspaces or crates outside
   `[workspace] members`, and no workspace cargo gate reaches `scripts/*.rs`
   rust-scripts — `build.rust_scripts` discovers and compiles that population,
   while `scripts/check-script-sigpipe.sh` audits its entrypoint contract.
3. **Can it fail?** A gate that only ever emits warnings, or whose exit status
   comes from the last element of a pipeline, cannot refuse anything. Prefer
   `-D warnings`; if you pipe, capture the status before the pipe.
4. **Have you watched it fail?** Introduce a real defect — ideally one that
   genuinely existed, recovered from history — and confirm the gate refuses it
   **by name**. A gate demonstrated only against invented input is the fixture
   trap, and a gate never demonstrated failing is indistinguishable from one that
   cannot.

### And two ordering rules, learned the expensive way

- **Do not widen a gate and fix what it finds in the same change.** Enumerate
  what turning it on surfaces, fix or waive each item and land that, then switch
  the gate on. Switching first converts a silent gap into a standing red, and
  main going red is a P0.
- **Say plainly which half of a change closes a live gap and which is
  precautionary.** When `--all-features` was added to `doc.rustdoc` it changed
  nothing measurable — 13 crates and 920 pages either way. Recording that stopped
  it being read as a fix for a gap that was not there.
### Declaring a machine facility a node cannot run without

A node may add one optional field:

```json
"requires_host_capability": "cpuid-faulting"
```

It means: this node can only observe what it exists to observe on a machine that
has that facility. When the facility is present, the node runs exactly as it
always did and every assertion inside it keeps full force. When it is provably
absent, `scripts/validate.rs` withholds the node before anything spawns and
records a **third outcome, host-inapplicable** — neither a pass nor a failure.

Before this existed, `privileged-cpuid.faulting` failed in 0.11 s with exit 101
and an empty detail block on a machine without CPUID faulting, which reads like a
broken build, and its eager-exit aborted twelve other in-flight nodes and
filtered twenty-seven more (hermit#2135, hermit#2148, hermit#2205).

This is **not** a way to get a node out of the way. Every one of these holds:

- The judgement never reads the node. It comes from an out-of-band probe of the
  machine during plan construction, so a node's exit code, stderr or panic
  message can never produce it. A node that is merely broken has no declaration,
  so it runs, fails, and is refused exactly as before.
- The capability vocabulary is closed in
  `scripts/lib/validate_plan.rs::HostCapability`. A name that is not in that enum
  refuses the whole run rather than omitting anything.
- The probe fails closed toward running: absence requires two independent
  sources to agree (for `cpuid-faulting`: `arch_prctl(ARCH_SET_CPUID, 0)`
  returning `ENODEV` **and** `/proc/cpuinfo` not advertising `cpuid_fault`). A
  probe error, a different errno, an unreadable `/proc/cpuinfo`, or the two
  sources disagreeing all mean PRESENT.
- `HERMIT_VALIDATE_HOST_CAPABILITY_PRESENT` can only force a capability
  *present*. There is no override in the other direction.
- Withholding a node that a retained node depends on is a refusal, not a
  cascade.
- The omission is written to the ledger as a typed intentional skip with reason
  `host-inapplicable`, is never added to `gates`, and is added back into
  `gates_expected`. The parent's separately-reviewed consumer allowlist
  (`ci-hub/validate/gate_completeness.py`, `ci-hub/lib/qualifying_receipt.rs`)
  admits only `empty-manifest-bucket`, so a run carrying a host-inapplicable node
  is **not** a qualifying receipt. Recording it honestly is what costs the
  receipt; the mechanism cannot buy one.

## Resource model (outer + inner limits)

The task's "outer + inner resource limits" map onto the runner's two knobs:

**Outer** — how many gates may co-run:

- `resource_caps` gates *scarce* resources. The `portable` label keeps only
  `{"manifest_guest": 8}`. Ordinary manifest buckets use disjoint cell trees
  and request one slot after the shared build barrier. The two high-width
  buckets, `backend-parity-c` and `c-programs`, request all eight slots and pass
  `--jobs 8`, so they do not overlap another manifest bucket while retaining
  the measured worker width. Legacy Hermit guest gates and direct strict
  compatibility probes have no shared scarce-resource demand; they may overlap
  when dependencies, the outer scheduler width, and memory allow.
  The `privileged` label declares no resource cap: `/dev/kvm` supports concurrent
  guests, so its three consumers may overlap. The PMU is
  **not** a scarce resource and carries no cap: reverie
  measures retired conditional branches with per-task (`cpu = -1`) counters that
  the kernel context-switches, so only the running task's counters need be
  resident; 32 concurrent `run --strict --verify` processes pinned to one core
  all pass with zero `perf_event_open` failures (measured in the dev-hermit
  parent, `experiments/pmu-concurrency-ceiling-measured_20260803`). PMU
  exhaustion, if it ever occurred, fails **loudly** (`perf_event_open` error or
  a `set_pinned(1)` panic), never as silent miscounting, so leaving PMU uncapped
  cannot corrupt results invisibly.
  The canonical `ci-hub validate-run` launcher supplies one shared
  `DAGRUN_RESOURCE_CAPS_PATH`, so these same counts cover overlapping top-level
  validates rather than restarting at full capacity in each scheduler process.
  A queued gate has not started: its wall and CPU bounds begin only after its
  resource demand is granted. Direct developer invocations that do not supply
  that path retain the process-local behavior.
- `--max-mem SPEC` (or `-j N`) bounds total concurrency. With `--max-mem`, the
  runner picks the largest `-j` whose modeled worst-case footprint (summed
  `rss_baseline_bytes` of a schedulable set) fits the budget.

**Inner** — each gate's own box:

- `rss_baseline_bytes` — estimated peak RSS, the input to `-j` sizing.
- `hard_mem_max_bytes` — explicit inner cgroup `MemoryMax`; a gate that exceeds
  it is OOM-killed **in isolation** rather
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

The remaining `manifest_guest` cap limits one real worker population. Ordinary
manifest buckets consume one slot; the two explicit eight-worker buckets consume
all eight so their own worker count, rather than overlapping sibling buckets, is
the pressure under test. Legacy guest gates, direct strict compatibility probes,
and KVM consumers are bounded by their existing dependencies, CPU and memory
declarations rather than a synthetic exclusive resource.

The former `pmu: 1` cap (and the matching `flock /tmp/hermit-privileged-pmu.lock`
in the workflows, plus the `pmu-serial` runner label) were retired: they guarded
a counter-exhaustion limit that measurement showed does not exist, and PMU skid
is an instruction-space hardware property independent of concurrency, so no
determinism argument justifies serializing PMU work. If uncapping ever surfaces
a real ceiling it does so loudly (see above), which is a finding to record, not
a reason to restore a silent serialization.
