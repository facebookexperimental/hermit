<!--
Copyright (c) Meta Platforms, Inc. and affiliates.
All rights reserved.

This source code is licensed under the BSD-style license found in the
LICENSE file in the root directory of this source tree.
-->

# Centralized e2e test manifests (schema v3)

These YAML files are the load-bearing policy source for Hermit's executable
end-to-end tests. Test programs contain behavior only; lane, mode, backend,
timeout, build flags, observation policy, and exclusion reasons belong here.
`target/debug/test-harness` loads them through the structured Rust parser in
`ci/manifest-plan`.

`defaults.yaml` declares the global per-cell timeout. The 13 bucket manifests
(12 until the strict compatibility corpus became `compat.yaml`,
https://github.com/rrnewton/hermit/issues/3448, and 13 before that until the
backend-parity-c bucket was folded into c-programs,
https://github.com/rrnewton/hermit/issues/3301)
separate calibrated blocking cells from discoverable migration inventory. CI
creates one independently schedulable run node for every bucket.
Seven buckets currently contain calibrated blocking workloads:

- `system-utils.yaml`
- `data-handling.yaml`
- `determinism-stress.yaml`
- `language-runtimes.yaml`
- `applications.yaml`
- `c-programs.yaml` (eight calibrated Buck-derived C probes)
- `compat.yaml` (the 189-program strict compatibility corpus, written as a
  `corpus` section; five of its rows are diagnostics; 63 of its rows also
  have a verify cell on liteinst and on in-guest-trap that the full run type
  runs (`additional`); it also holds the
  `sabre-compat-only` run type's SaBRe cells and its 27 extra rows, the
  `rr-compat-only` run type's 139 replay cells, and the
  `strict-compat-only` run type's 219 strict variant tests)

Eight additional `*-c.yaml`/`c-programs.yaml` buckets make 180 more C guests
centrally discoverable. Eight `c-programs.yaml` entries have calibrated
standalone build and output contracts and run in blocking CI; the remaining
172 C guests keep `ci = false` until they are calibrated. Buckets without a
calibrated cell still have a CI node that intentionally reports zero cells,
and the correspondence audit proves that this cannot hide a calibrated cell.
Every entry still declares all five modes and every backend exclusion, so
inventory does not silently imply support.

## Matrix symmetry and the test front door

Compatibility coverage enters through these shared schema-v3 manifests, not
through a backend-owned guest list. Every test declares all five modes, and
every non-naked mode partitions the complete `ptrace`, `dbt`, `kvm`, `sabre`,
`liteinst` and `in-guest-trap` axis into enabled cells and explicit gaps. Any active mode must
include ptrace so the reference behavior is established before another backend
ratchets it.

`ci/matrix-symmetry-baseline.json` records the small amount of older policy
debt: ptrace-less manifest rows and guest fixtures owned by a driver for one
backend or for e9patch preprocessing. `hermit-manifest-plan` requires that
baseline to match exactly, so private corpora cannot grow. Migrating a baseline
entry to a shared manifest is allowed, but the same change must remove it from
the baseline. This makes the shared test identity the row axis; backend support
or gaps remain cells of that one row rather than creating backend-private rows.

## Schema contract

`defaults.yaml` supplies independent global bounds: 22 CPU seconds and 57 wall
seconds. A bucket may override the wall value with top-level `timeout_seconds`
plus a non-empty `slow_reason`. An exact `(test, mode, backend)` exception must
declare `cpu_timeout_seconds`, `timeout_seconds`, and `slow_reason` for the same
backend. Cell wall values win over bucket wall values, and bucket values win
over the global wall default. Runners consume the resolved values and do not
carry fallback timeout literals.

Nextest still requires its native TOML syntax at execution time, so validation
requires `.config/nextest.toml` to carry the same 57-second base wall bound.
The counted wrapper supplies a temporary parsed TOML configuration with the
`HERMIT_TEST_WALL_TIMEOUT_MULTIPLIER` value applied; it never rewrites the
checked-in file. The manifest runner independently applies
`HERMIT_TEST_CPU_TIMEOUT_MULTIPLIER` and
`HERMIT_TEST_WALL_TIMEOUT_MULTIPLIER`; unset values mean `1`, and invalid values
refuse before a cell starts.

```yaml
schema: 3
bucket: applications
test:
  - id: applications/timed-progress-bar
    modes:
      verify:
        timeout_seconds:
          ptrace: 91
        cpu_timeout_seconds:
          ptrace: 32
        slow_reason:
          ptrace: Retained p90 measurements require the explicit CPU/wall pair.
```

Every entry under `test` names either a repo-relative `program` or a `direct` shell
command. Program extensions select the runner:

- `.sh`: execute the existing `--prepare`/`--run` protocol directly;
- `.c`: compile implicitly with `cc` plus optional `build.cflags`;
- `.rs`: compile implicitly with `rustc` plus optional `build.rustflags`.

`MODE` is always the outer axis. Every entry declares exactly these five
tables: `verify`, `chaos`, `replay`, `naked`, and `custom`. Each table has a
`backends_enabled` list and a `backends_disabled` table. The two must form a
complete, disjoint partition and every disabled backend needs a nonempty WHY.
For non-naked modes the axis is `ptrace`, `dbt`, `kvm`, `sabre`, `liteinst`,
and `in-guest-trap` (in-guest LiteInst with syscall site patching off, a
column of its own); naked partitions only `native`.

```yaml
test:
  - id: example/test
    modes:
      verify:
        ci: true
        backends_enabled: [ptrace]
        backends_disabled:
          dbt: DBT coverage is owned by its backend parity partition
          kvm: KVM requires the privileged runner
          sabre: SaBRe requires its external runtime
          liteinst: LiteInst coverage is owned by its compatibility partition
          in-guest-trap: in-guest-trap coverage is tracked apart from liteinst
```


### Host requirements are a closed, evidence-bearing gate

Every test declares `requires`. The vocabulary is closed in
`ci/manifest-plan/src/runner.rs`; an unknown token refuses manifest loading.
Most tokens are descriptive prerequisites only and can never suppress a cell.
The sole current host capability mapping is `cpuid` to `cpuid-faulting`.
Independently of `requires`, a `kvm` backend cell needs the `kvm` capability:
a missing `/dev/kvm` (ENOENT) is proof of absence even when the CPU advertises
vmx or svm, which is what RE workers and containers show. Likewise a
`liteinst` or `in-guest-trap` backend cell needs `cpuid-faulting`: the in-guest
LiteInst runtime arms CPUID faulting before the guest's first instruction and
refuses to start without it, and RE workers and GitHub-hosted runners may lack
it. Each such cell's plan row names the capability, which also keeps the Buck
cell generator from routing it to RE (`ci/buck-e2e/defs.bzl`).
One descriptive token is enforced at load time: a verify golden that prints
the vDSO getrandom leg (`vdso-getrandom[`) can only be produced on a host
kernel that exports `__vdso_getrandom` (Linux 6.11+ on x86-64), so its test
must declare `vdso-getrandom`. The token is not probed; on an older kernel the
cell runs and fails with that prerequisite named in its manifest.

When a needed capability is provably absent, the harness records each selected cell
as `HOST-INAPPLICABLE` with the probe evidence. The cell stays in the selected
denominator, has no invented command or attempt, and appears as JUnit `skipped`;
it is never a pass. Probe failure or disagreement runs the cell. If every
selected cell is inapplicable, the direct harness invocation refuses rather
than returning a vacuous green. `ci/expected-e2e-plan.json` carries each
selected cell's generated `requires_host_capabilities` metadata, and `audit-ci`
checks it against the live manifest; the validation planner reads that bounded,
checked-in population without compiling or running a second driver. It computes whether a
DAG bucket would contain no runnable cells and records that node with the same
typed outcome instead of spawning a known-empty bucket.

The mode contracts are:

| Mode | Contract |
| --- | --- |
| `verify` | Run each enabled backend with `hermit run --strict --verify` |
| `chaos` | Search declared seeds and require cross-seed diversity plus exact within-seed reproduction |
| `replay` | Run ptrace `record start --strict --verify` in an isolated recording directory |
| `naked` | Opt-in meta-CI only; run natively three to five times and require declared variation |
| `custom` | Run declared edge-case Hermit arguments and require three to five identical observations |

`verify`, `chaos`, and `custom` may set an absolute `workdir` path. The harness
passes it to `hermit run` before the guest-command separator, so it is resolved
inside the guest after mounts are applied. Use this when a guest's interpreter
or toolchain inspects its inherited working directory before the program can
change directories itself. `naked` and `replay` do not accept this field.
The checked-in use is currently ptrace-only. A mode that enables DBT is
rejected because the DBT launcher does not yet preserve the requested guest
working directory; qualify that backend behavior before using `workdir` there.

An enabled `verify` cell is green only when its typed report records canonical
strictness, log comparison, positive INFO counts on both runs, bitwise parity,
and a matched verdict. Output-only, stripped, empty-log, malformed, or
contradictory reports are infrastructure errors rather than product results.
The one exception is a cell that declares `comparator: stripped` (below): it
requires a stripped report instead, and is never L2.

A `verify` guest on any backend except dbt finds its home, XDG configuration
and fixture directories at `/tmp/e2e/home`, `/tmp/e2e/xdg-config` and
`/tmp/e2e/fixtures`, bound from the cell directory; `HOME`, `XDG_CONFIG_HOME`
and `E2E_FIXTURE_DIR` name those paths and a prepared program runs as
`/tmp/e2e/fixtures/program`. Every backend's guest therefore sees the same
strings, which the parity post-pass requires before it reports a
comparison's `credit` rather than its `unequalized_credit`
(<https://github.com/rrnewton/hermit/issues/3301>). The parity population is
the verify matrix itself, with no selection file: every test whose ptrace
`verify` cell full validation selects, on every candidate backend (dbt,
in-guest-trap, kvm, liteinst and sabre). A candidate whose `verify` cell is disabled, not selected,
red or missing scores 0 and is counted; only a test whose ptrace run left no
golden is left out, and counted apart. `ci/compat-envelope/parity-cells.json`
snapshots it. A guest in any other mode
sees the host paths. Read these directories through the variables rather
than assuming either form.

`ci` may also be a mapping when enabled backends have different validation
status. The mapping must name every enabled backend and no disabled backend.
Each `false` backend requires its own structured reason; a `true` backend must
not carry one. The reason records an existing result class, retained evidence,
and explanatory text. Placeholder text is rejected.

```yaml
test:
  - id: example/mixed-backends
    modes:
      verify:
        ci:
          ptrace: true
          liteinst: false
        ci_disabled_reason:
          liteinst:
            result: determinism-failure
            evidence: ignored/results/liteinst.jsonl
            reason: canonical comparison diverged at scheduler turn 10
        backends_enabled: [ptrace, liteinst]
        backends_disabled:
          dbt: DBT coverage is owned by its backend parity partition
          kvm: KVM requires the privileged runner
          sabre: SaBRe requires its external runtime
```

The false backend remains enabled, red, and available to pressure/manual
measurement. Its reason is copied into `ci/compat-envelope/cells.json`; it is
not made invisible by being omitted from ordinary validation.

Use `unavailable` when the cell is executable but its required canonical
evidence or positive verification contract is not available from the current
product path. Reserve `infrastructure-error` for an identified host or harness
infrastructure failure, such as a runner resource or artifact-publication fault.

An enabled SaBRe cell has an additional execution-path contract. Every E2E
Hermit execution writes structured evidence into the cell capture: the
in-guest tool must have issued a coordinator RPC, and both
`ptrace_fallback_sites` and `trusted_shared_object_sites` must be zero. A
ptrace-installed SaBRe marker is
classified as fallback; a raw syscall observed in a trusted shared object is
classified as native execution outside the measured SaBRe path. Either makes
the cell fail even when status and stdout match. The JSONL result retains the
per-execution records and aggregate eligibility under `execution_path`.

Any mode may declare backend-specific guest arguments. The harness appends
these after the guest executable, separately from Hermit's own arguments:

```yaml
test:
  - id: example/test
    modes:
      verify:
        ci: false
        ci_disabled_reason: Not selected by ordinary validation yet
        backends_enabled: [ptrace, kvm]
        guest_args:
          ptrace: [multi]
          kvm: [multi]
```

Every `guest_args` key must name a backend listed in either `backends_enabled`
or `backends_disabled`. This lets an explicit `--probe-disabled` run give an
unselected backend its own scenario arguments without selecting that backend
for ordinary validation. The test harness, manifest CLI, and `--guest-args`
exporter look up the requested backend's arguments exactly, without inheriting
another backend's arguments; an omitted backend receives no guest arguments.
The exporter uses JSON Lines so empty strings, tabs, newlines, and explicitly
empty vectors retain their exact argument boundaries.
The only valid backend for `naked` is `native`; other modes accept only the six
Hermit backends.

A verify cell whose guest ends unsuccessfully on purpose declares exactly how it
ends. `expected_guest_exit` names exactly one of a nonzero `code` (1-255) or a
`signal` (1-64), plus a substantive `reason`:

```yaml
test:
  - id: example/exits-seven
    modes:
      verify:
        expected_guest_exit:
          code: 7
          reason: The guest returns 7 so the test can prove Hermit reports it exactly.
```

The harness then passes `--verify-allow=failure`, so Hermit compares both runs
instead of refusing the first. The cell passes only when the canonical report
matches, the report's guest status is exactly the declared one, and Hermit's own
process status reports it: the same exit code, or for a signal either death by
that signal or exit status 128 plus the signal number. Any other ending,
including a successful one, fails. The key is rejected outside `verify` mode.

A verify cell may also name the exact bytes its guest prints on stdout, per
enabled backend, with `expected_stdout`:

```yaml
      verify:
        expected_stdout:
          ptrace: "madvise-ok\n"
          dbt: "madvise-ok\n"
```

The cell then passes only when, in addition to every check above, both runs
the report compared printed exactly those bytes: the report's per-run stdout
length and SHA-256 must equal the declared string's. Two runs agreeing with
each other on different bytes fail. A report without per-run outputs is an
incomplete-evidence error, never a pass. The same string declared for two
backends makes their stdout equal by construction, which the report-only
parity post-pass cannot enforce. An empty string asserts empty stdout. A key
must name a backend in `backends_enabled`, and the table is rejected outside
`verify` mode.

Some output repeats exactly on one host but is not the same across hosts:
addresses depend on the compiler and C library that built the guest, and
virtual-time deltas depend on the guest's instruction counts. For such a guest,
`expected_stdout_contains` names the success marker it prints instead of the
whole stream:

```yaml
      verify:
        expected_stdout_contains:
          ptrace: "heap "
          dbt: "heap "
```

The runner reads the captured stdout of the attempt and uses it only when its
length and SHA-256 equal both compared runs' stdout; the cell then passes only
when that stream contains the text. A capture that does not match both runs,
or a report without per-run outputs, is an incomplete-evidence error. The run
comparison already requires the two runs to agree, so the marker adds the
guarantee that what they agreed on is the guest's success output. The text
must be non-empty, keys must name backends in `backends_enabled`, and the
table is rejected outside `verify` mode.

These keys carry the DBT cases of the retired strict parity matrix
(`run_matrix.py --backend dbt --strict`). That driver was deleted with
`tests/backend-parity/` under https://github.com/rrnewton/hermit/issues/3301;
its last version is
https://github.com/rrnewton/hermit/blob/82e24cc0e6fac0f7f9a8dac4b4b25d9ad8e3231d/tests/backend-parity/run_matrix.py.
The matrix compared the
stdout of three `--strict` runs with one attempt. A verify cell compares two
runs, their stdout and their recorded event streams, and like every manifest
cell but a replay cell a failed attempt is retried once; a passing retry passes
the cell and the failed attempt's row is kept in the results.

A `verify` mode may also declare, for a test whose recorded policy runs one
specific configuration:

```yaml
      verify:
        hermit_args:
          ptrace: [--no-virtualize-cpuid, --max-timeslice=disabled]
        hermit_args_reason: The corpus records this configuration
        env: {TMPDIR: /tmp}
        comparator: stripped
        comparator_reason: The corpus verdict policy is Hermit's default --verify
        diagnostic:
          ptrace: A bounded probe; its failure is reported, not blocking
```

- `hermit_args` are Hermit `run` flags added after the runner's own, per
  enabled backend: a backend not named gets none. Only
  `--no-virtualize-cpuid` and `--max-timeslice=VALUE` are accepted. Each
  relaxes determinism, so a non-empty `hermit_args_reason` is required and
  every flag is recorded, with that reason, in the result row's
  `relaxations`. `hermit_args_reason` is one string for every backend's
  flags, or a mapping that names exactly the backends of `hermit_args`, each
  with its own reason, so each cell records only its own.
- `env` adds guest variables as `--env NAME=VALUE` after the runner's fixed
  guest environment; a name the runner sets (`HOME`, `TZ`, `LC_ALL`, ...) is
  refused. Only a verify or a replay mode accepts it. A replay cell that does
  not declare `env` inherits its verify cell's; one that does, an empty
  mapping included, records with exactly the variables it declares.
- `env_by_backend` adds guest variables only on its named enabled backend,
  merged with common `env`. Only verify recipes may declare it. Empty backend
  maps, names already in common `env`, invalid names or NUL values, and
  runner-owned names are refused. Replay inherits the effective verify
  environment unless it declares its own `env`, including an empty mapping.
  The effective `--env` arguments are retained in invocation evidence; exact
  manifest bytes, including these settings, remain covered by the manifest
  digest. The guest source/direct-command digest keeps its existing scope.
- `comparator: stripped` runs Hermit's default `--verify` comparison instead of
  `--verify-strict`. It is below L2. The cell passes only on a verified,
  matched report that compared a non-empty stripped event stream on both
  runs, never counts as bitwise parity, and cannot anchor a backend-parity
  verdict. It requires a non-empty `comparator_reason`, is recorded in
  `relaxations` as `comparator=stripped`, and is refused together with
  `assert.bitwise_parity: true`. `strict` is the default. The scorecard's
  verify-results gate admits a stripped PASS only from a cell that declared it,
  by the same report rule, and counts it apart from canonical matches. The
  ledger records it as `compared-and-matched` (or `compared-and-diverged`) at
  tier `exit-and-stream-equality` with `bitwise_parity: false`, which no
  qualification counts as canonical evidence.
- `diagnostic` names, per enabled backend, why that cell is a diagnostic. Only
  a `comparator: stripped` cell may declare one: a canonical cell exists to
  supply L2 evidence, so its failures always block. The result row's
  `classification` is `diagnostic` and the reason is recorded in
  `relaxations`. When the cell's last attempt is a measured failure (a FAIL
  that is a product failure or the cell exceeding its time budget), the
  harness prints it as `DIAGNOSTIC`, counts it in `summary.json`
  (`diagnostic_failures`), writes it to dagrun as a `diagnostic_fail` row
  (structured-result schema 4), and does not fail the run; the scorecard and
  the validation ledger count it separately as well. Only a bucket listed in
  `DIAGNOSTIC_MANIFEST_BUCKETS` (ci/manifest-plan/src/validation_dag.rs) may
  hold diagnostic cells: its node runs `test-harness run --diagnostic-results`
  and declares schema 4. Every other node writes schema 2, and a run that
  reports to dagrun without the flag refuses to select a diagnostic cell. A
  standalone run without the flag (one pressure-test sample, for example) may
  run it, and excuses nothing: its failure fails the run. An ERROR, any other
  no-result cause, or a FAIL retried into an ERROR still fails the run.
  Because the ledger keeps the failure out of `passed_tests`, a run with a
  diagnostic failure is not a qualifying receipt.

### Strictness level: one run per test

Every test runs once per validation, at the deepest strictness level the host
and the cell allow:

- **strict** on a host with CPUID faulting whose Hermit reports an exact
  retired-branch counter (`hermit host-capabilities`): no relaxation, so CPUID
  virtualization and the preemption timer stay on.
- **relaxed** on any other host, or anywhere when `E2E_STRICTNESS_LEVEL=relaxed`
  is set: a verify cell on ptrace or e9patch adds `--no-virtualize-cpuid
  --max-timeslice=disabled`, which is what lets `hermit run --strict` run there
  (without the preemption timer it does not refuse an inexact counter). Nothing
  can make a host without the capabilities run the strict level.

Only a verify cell on ptrace or e9patch, the backends whose virtual clock reads
the host's branch counter, has a relaxed form. Every other cell runs as
declared, and a chaos or replay cell that needs the counter is still
`HOST-INAPPLICABLE` on a host without it. `test-harness run` prints the level it
chose and why.

A verify mode that does not depend on preemption or CPUID virtualization may cap
itself at the relaxed level, so it never has a strict form on any host:

```yaml
      verify:
        max_level: relaxed
        max_level_reason: Prints a constant; no schedule or CPUID value reaches it
```

`max_level` accepts only `relaxed`, needs a non-empty `max_level_reason`, and
needs a ptrace or e9patch backend in `backends_enabled`. Each flag the relaxed
level adds is recorded in the row's `relaxations` with its reason (the cap's, or
the host's), and every row records how its cell ran in `strictness`:
`{"level": "strict"|"relaxed", "virtualize_cpuid": bool, "max_timeslice":
"disabled"|<nanoseconds>, "capped": bool}`. A row without `strictness` is a row
written before it existed; readers must not infer the level from the host.

A bucket whose tests share one recipe may state it once in a `corpus` section
instead of a `test` list (`compat.yaml` is the one such bucket).
`ci/manifest-plan/src/manifest_corpus.rs` expands every row into an ordinary
recipe before validation, so the harness, the front door and every audit see
ordinary tests: id `<bucket>/<id or label>`, one CI verify cell on the
section's `backend` carrying its `verify` settings, and every other mode and
backend disabled with a stated reason. `diagnostic.rows` names, per row label,
why that row's cell is a diagnostic, with its own shortened budget, and
`heavy.rows` why that row's cell gets a longer one. `focused` adds a verify cell
on one more backend to every row except those its `except` names (each with
the reason that backend is disabled there), labelled with a run type (below),
so the default run type does not run it; it shares the section's verify
settings except `hermit_args`, which stay on the section's backend. Each
`additional` entry adds a verify cell on one more backend to the rows its
`rows` names, which the default (full) run type does run: it carries none of
the row's run-type labels, has the global default budget, shares the
section's environment, comparator and `no_retry_reason`, is never a
diagnostic, and takes the entry's own `hermit_args` and `hermit_args_reason`,
which join the mode's per-backend `hermit_args` (and, when the flagged cells'
reasons differ, a per-backend `hermit_args_reason`). Every other row disables
that backend with the entry's `disabled_reason`, or with its own reason in the
entry's `disabled`. An additional backend may not be the section's backend or
a focused one. An entry's optional `row_env` maps row labels from its enabled
`rows` to nonempty environment additions for that row on that backend alone.
It expands to `env_by_backend` without changing common `env`, sibling rows,
other backends or generated strict variants; unknown or disabled rows refuse.
`replay` gives every row except those its `except` groups name (each group
with its reason) a replay cell on ptrace in the row's own test (`hermit record
start` then replay, compared by the harness), labelled with the section's run
type so the default run type does not run it, with the section's budget and
`slow_reason`, an empty `env`, so it does not inherit the verify cell's
`TMPDIR`, and none of the verify settings: no `hermit_args`, comparator,
`no_retry_reason` or diagnostic. Its `unselected` lists the replay cells
measured red. Each `variants` entry adds one more test per row except those its `except` groups
name (each group with its reason): id `<bucket>/<id_prefix><id or label>`,
labelled with the entry's run type, with one verify cell on the section's
backend that keeps the section's environment and comparator but takes its
`hermit_args` (none unless stated), budget, `slow_reason` and
`no_retry_reason` from the entry and is never a diagnostic; the entry's own
`unselected` lists its cells measured red. A row's own `labels` put its
verify cells in those run types, and not its replay cell, which carries only
the `replay` section's run type. `unselected` lists cells
measured red: each stays enabled with `ci: false` and a `ci_disabled_reason`
carrying the class's `result`, `evidence` (an issue) and `reason`. A row's
`argv` is a `direct` argv list, run without a shell; in it `{{ROOT_DIR}}` is
the repository root and `{{VALIDATE_RUN_STATE}}` the validation's per-run state
directory (`$VALIDATE_RUN_STATE`). A row that names the latter is refused when
that variable is unset. `{{XDG_CONFIG_HOME}}` names the cell's prepared copy of
`tests/e2e/xdg-config`; verify equalizes it to `/tmp/e2e/xdg-config`, while replay
keeps the cell's host path. A caller without cell context refuses that token.
`readlink -f` and `realpath` canonicalize its existing nested `git/config` file,
retaining ancestor stat coverage without shared Buck or run-state ancestors.
Rerunnable shell commands assign their prepared cell path before expanding this
token, so an unset or unrelated ambient XDG directory cannot change the operand.
Any other `{{...}}` token is refused outright.

A verify mode may declare `no_retry_reason`: the cell then gets one attempt
instead of the runner's retry after a product failure, so a first-attempt
failure stays a failure. `compat.yaml` declares it because each of its programs
ran once per validation as its own node before the corpus moved here.
A replay cell never gets that retry and takes no `no_retry_reason`: its
product failure is a replay that diverged from its recording, or a recording
that did not complete, and a fresh recording on a second attempt does not
answer for the first.

A test may carry `labels` (lowercase words joined by `-`, unique), naming the
run types it belongs to, and a non-naked mode may carry `labels` per enabled
backend, adding run types to that one cell. A cell's run types are its test's
labels plus its mode's labels for its backend, or `full` (the default run type)
when both are empty. `test-harness run --label LABEL[,LABEL...]` keeps only the
cells carrying at least one named run type, and a selection without `--label`
selects `full`, so a cell labelled only with a focused run type is required by
that run type's bucket node and by nothing else; ci/expected-e2e-plan.json and
every full, portable, hosted and quick node select `full`. A bucket node that
selects a run type declares it in its static source (`ManifestSpec::label` in
ci/manifest-plan/src/validation_dag_static.rs), carries only that DAG label, and
owns that run type's required cells. A label that no cell carries is refused,
so a typo cannot select nothing and pass.

`naked` must set `ci = false`; it runs only when explicitly selected. A mode
with no enabled backend remains visible with `ci = false` and a reason for
every disabled backend. Regular CI executes only cells with `ci = true`;
run one enabled manual cell with explicit test and mode filters:

```sh
target/debug/test-harness run --include-manual --mode verify \
  --test c-programs/add-key-enosys
```

`--include-manual` requires both exact filters so a broad CI command cannot
accidentally pull the uncalibrated corpus into its run plan.

To measure one documented backend gap without first promoting it into the
known-green envelope, use all three exact cell filters:

```bash
target/debug/test-harness run --probe-disabled --test c-programs/example \
  --mode verify --backend sabre --results target/e2e/probe/results.jsonl
```

`--probe-disabled` selects from `backends_disabled`, is accepted only by
`run`, and cannot be combined with `--ci-only` or `--include-manual`. This is
the bounded expansion path: a passing probe is evidence for a later manifest
ratchet, not an implicit promotion into the regression envelope.
Callers that combine explicit mode/backend filters with CI policy must add
`--ci-only`. This is how `scripts/validate.rs quick` avoids expanding the manual C
inventory.

## Running one cell

`test-harness` is the canonical manifest cell runner. Select all three parts of
the cell identity explicitly when reproducing CI behavior:

```sh
target/debug/test-harness run \
  --test system-utils/example-devrand --mode verify --backend ptrace
```

Add `--lane portable --ci-only --prebuilt` when reproducing a portable CI node
against fixtures from `test-harness build`. The runner owns manifest selection,
host-capability checks, CPU and wall limits, retries, and JSONL/JUnit results.

`tests/manifest-cli.rs` is the interactive inventory and command renderer. Use
`list` to find a test and `get` to inspect the direct Hermit command:

```sh
./tests/manifest-cli.rs list --bucket system-utils
./tests/manifest-cli.rs get system-utils/example-devrand \
  --mode verify --backend ptrace --lane portable
```

Its `run` subcommand can inject extra Hermit flags after `--`, which is useful
when debugging Hermit itself, but it does not provide the test harness's typed
results, retry policy, or aggregate CPU accounting. Therefore it is not the
canonical reproduction command even though it can launch the same guest.

To run the cell through one existing validation node, keeping that node's
boxing and limits, use the node wrapper. This path assumes its build artifacts
already exist and reports iteration evidence only:

```sh
./ci/run-node.sh portable e2e.manifest_system_utils -- \
  --test system-utils/example-devrand --mode verify --backend ptrace
```

For the heavier validation-owned requalification path, including its declared
preparation and evidence checks, use:

```sh
./scripts/validate.rs --requalify-cell \
  system-utils/example-devrand verify ptrace \
  --allow-local-off-the-record-run --no-label-pr
```

That focused validation is intentionally not suite-complete and cannot publish
a whole-suite validation receipt.

## Inventory and validation

`inventory/test-files.json` classifies every regular file and symlink below
`tests/` with a disposition, owning runner, and per-file justification. The
audit compares the inventory byte-for-byte with filesystem discovery, then
confirms that every manifest program is classified as `manifest-test`. Tests
retained under Cargo, Buck, integration, QEMU, or suite drivers explain the
build flags, arguments, expected results, hardware, or shared setup that their
owner supplies. Each exception names its exact owning runner and the file's
specific role; generic category-only justifications fail review even when the
inventory is mechanically complete.

`ci/expected-e2e-plan.json` ratchets the exact blocking cells. Adding, removing,
or reclassifying a `ci=true` cell fails validation until the expected plan is
updated in the same review.

To flip a cell (make an existing manifest cell required, or stop requiring it),
edit only the manifest, then run `ci/sync-cell-config.sh`. It regenerates every
file derived from the manifests: `ci/expected-e2e-plan.json`,
`ci/optional-e2e-cells.txt` (the enabled `ci = false` cells),
`ci/compat-envelope/parity-cells.json`, `SCORECARD.md`,
`ci/compat-envelope/cells.json` and `ci/dag/validate.json`.
Commit them with the manifest; the diff of the expected plan, and of the
optional-cell inventory for a `ci = false` cell, is the record of the change. `ci/sync-cell-config.sh --check` writes nothing and fails on any
drift, and validation runs the same checks
(https://github.com/rrnewton/hermit/issues/3606).

A `ci = false` cell is never executed **and never compiled** by ordinary
validation, so its guest can
rot without any node noticing. Two mechanisms bound that. `manifest-plan`
rejects every enabled mode with boolean `ci = false` unless it has a shared
`ci_disabled_reason` carrying explanatory text: at least sixteen characters and
at least three words, and not placeholder text. A per-backend mapping is held to
that same requirement, and additionally requires the retained evidence described
above, which the shared string has no field for. It rejects a stale reason left
behind on a selected backend. Separately, `target/debug/test-harness audit-compile --category <bucket>` compiles every C guest
the bucket declares regardless of its `ci` flag; it is wired into the portable
DAG for `c-programs` (which absorbed the former `backend-parity-c` bucket) and
fails closed on zero compiled.

Use the load-bearing entrypoints:

```sh
cargo run -p hermit-manifest-plan -- --format text
target/debug/test-harness validate
target/debug/test-harness plan --format json
target/debug/test-harness expected-plan > ci/expected-e2e-plan.json
target/debug/test-harness audit-gaps --format json
target/debug/test-harness build --lane portable --ci-only
target/debug/test-harness run --lane portable
target/debug/test-harness run --lane portable --category system-utils --ci-only --prebuilt
target/debug/test-harness run --mode naked --test system-utils/random-device
```

Both GitHub workflows and `scripts/validate.rs` execute the same portable and
privileged DAG files. Each DAG has a manifest guest-build barrier followed by
one structured selector per bucket. `audit-ci` fails if either caller stops
delegating to the shared plans, a bucket node disappears, a command diverges
from its selector, or the aggregate selected cells differ from the ratchet.

## Adding a test

1. Put behavior in a focused shell, C, or Rust source file.
2. Add it to exactly one bucket and declare all five modes.
3. Enable only combinations proven locally; justify every exclusion.
4. Add or update its exact entry in `inventory/test-files.json`.
5. Run `target/debug/test-harness validate` and the affected cells.
6. Add a structured DAG node when adding a bucket; validation fails until each
   lane has exactly one node per bucket.

Do not replace a semantic workload with `--help`, `--version`, or a no-op
launcher probe.
