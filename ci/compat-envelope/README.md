# Compatibility scorecard

The compatibility scorecard reports two separate facts from a Hermit checkout:
which manifest-declared test, mode, and backend combinations are selected for
ordinary validation, and what canonical results have been imported for them.

Start at [`SCORECARD.md`](../../SCORECARD.md). It is intentionally a small,
versioned table. The stable per-cell identities behind the totals live in
[`cells.json`](cells.json). Raw results, logs, durations, timestamps, and host
data are not versioned; each validate run retains those under `ignored/`.

The denominator is the complete comparable manifest matrix, not just the
combinations selected by one validate path. For `N` manifest tests, verify,
replay, and chaos span five Hermit backends, while native contributes one
naked-execution control: `N × (5 × 3 + 1)` cells. Native is shown as a sixth
backend in the table, but it does not have replay or chaos cells, so the formula
is not `N × 6 × 3`. Explicit `custom` commands still run when selected by
ordinary validation, but they are not multiplied across every test/backend
pair: unlike the three common modes, they do not define a uniform product-wide
denominator.

For one dated example only: on 2026-08-13, `N = 336`, so the comparable matrix
has `336 × (5 × 3 + 1) = 5,376` cells. The checked-in table is generated from
the live manifest and changes automatically when a manifest test is added.

`hermit-manifest-plan --format matrix-json` emits every cell in the manifest,
including applicable and not-applicable combinations. It also emits the
validated per-test timeout and the number of
`execute_attempt` calls the existing harness makes for each mode. A seedless
chaos mode has `attempts: null`: it remains red but has no command to run. The
pressure-test entry point consumes this output instead of parsing the manifest
a second way. A disabled combination is not applicable. The existing `--format
json` and text views remain execution-only because
they are execution plans rather than scorecards.

## Ordinary validation

Run:

```console
./validate.sh
```

The path is deliberately direct:

1. `hermit-manifest-plan` validates the complete matrix and emits the execution
   plan selected by the requested path.
2. `ci/expected-e2e-plan.json` identifies the cells ordinary validation runs.
3. Each manifest bucket appends schema-4 `results.jsonl` rows to a unique
   durable result directory. Every row includes the validate attempt number,
   cell `duration_ms`, the `timeout_seconds` used for that attempt, literal
   argv, explicit environment, working directory, and pasteable shell command.
   The framework also writes the existing pressure `result` value and an
   explicit `failure_class`; `error_kind` remains the narrower mechanism. A
   retry adds another row rather than replacing the earlier observation, so a
   change such as `determinism-failure` then `crash-error` is retained.
   `attempts[].cpu_usage_usec` records `wait4` user-plus-system CPU for that
   launched child and descendants it reaped. The row-level `cpu_usage_usec`
   adds preparation and every launched attempt with checked arithmetic; it is
   null when execution did not occur or any required measurement is incomplete.
   The bucket `summary.json` field `cell_cpu_usage_usec` likewise sums every
   emitted applicable row, including failed rows followed by retries.
4. The final `scorecard.compatibility` node requires a clean, exact-HEAD PASS
   row for every selected cell and prints the table.
5. The checked-in table and cell identities must still equal what the manifest
   and expected plan derive. After the ledger and receipt work is complete, a
   direct top-level local validate merges its per-cell rows and rewrites
   `SCORECARD.md` and `ci/compat-envelope/cells.json`. A ci-hub validate performs
   that write in its isolated checkout after the receipt, then applies the same
   results to the checkout that invoked it. The agent reviews the generated diff
   and decides whether to commit it; validation never commits it automatically.
6. A top-level full run retains `ignored/validate/artifacts/<run-id>/coverage.json`.
   It names the exact plan and outer nodes, every selected E2E cell, every
   cell in the manifest not selected by that path, with its recorded reason
   and observed
   pass/fail counts, and the complete integration-test-binary registration
   partition. The ledger row carries the same counts and binds the artifact by
   SHA-256.

`./scripts/validate.rs --show-plan` lists outer DAG nodes only. It does not list
the Rust test IDs or E2E cells inside those nodes; `coverage.json` records the
E2E population after a full run, while exact per-plan Rust test-ID enumeration
remains unavailable.

`SCORECARD.md` reports the current regression-cell count and lists the exact
selected custom commands separately. Explicit custom commands remain required
validation checks even though they are outside this uniform comparable
denominator. `scorecard.rs check` refuses unless every selected row is accounted
for by either the comparable green cells or that custom-command list.

The Basic Sanity Milestone 1 `verify` cells run each selected backend twice against
itself. Bare `--verify` still uses the legacy Stripped comparator. These cells
therefore measure same-backend repeatability under the current contract; they
do not establish strict INFO-log determinism or cross-backend parity. The
scorecard says this directly and reports no cross-backend parity count until
the manifest has cells that really compare fresh ptrace and non-ptrace logs.

Scorecard selection records whether a cell in the manifest is selected by full.
Measurement is separate: importing a pass or divergence records what happened
without changing which cells validation selects. Moving a cell out of the
selected plan is not a fix, and `scorecard.rs update` refuses that plan removal
unless an explicit compatibility-standard transition requests it.

See every command and the exact green definition with:

```console
./ci/compat-envelope/scorecard.rs --help
```

## Updating the checked-in table

After deliberately adding a cell to `ci/expected-e2e-plan.json`, run:

```console
./ci/compat-envelope/scorecard.rs update
git diff -- SCORECARD.md ci/compat-envelope/cells.json
./validate.sh
```

Review the table delta and the exact cell identity. The update command does not
run a test and cannot change measurement by itself. The following validate
automatically records any canonical per-cell results after its ledger and
receipt work; selection alone is not evidence.

## Cells not selected by full and the periodic full-matrix run

Every applicable manifest cell outside the full plan is not selected by full.
Other cells are not applicable. A cell not selected by full can have a passing
measurement, and a selected cell can have a divergence; selection and
measurement answer different questions.

To measure a not-applicable backend population before changing its manifest
applicability, name the backend explicitly. The backend requirement prevents an
unfiltered probe from attempting every unsupported combination:

```console
./ci/compat-envelope/pressure-test.rs run \
  --probe-disabled --backend kvm --mode verify --repetitions 1 \
  --cell-timeout 60 --run-timeout 7200
```

These probes use the harness's existing `--probe-disabled` execution path and
retain the backend filter in `run.json`. They do not change cell status or
manifest selection; a later reviewed change may select only cells supported by
the measured evidence.

## Divergence positions: where a cell diverged, and how well you know it

A red cell should say WHERE it diverged, not only that it did. Each observation
carries the position as a range per coordinate:

```json
"first_divergent_scheduler_turn": { "earliest": 1, "latest": 4, "samples": 2 }
```

**`samples` is the number of runs that LOCATED a position**, and it is stored
rather than derived because the plausible denominators disagree. In the
scorecard's own self-test bracket, five folded rows collapse to four distinct
invocations of which only three located anything: a passing run and a timing-out
run contribute no bound. "Earliest 80, latest 500" is a different claim over two
runs than over fifty, so the bounds are not interpretable without it. A range
with `samples: 1` is a POINT, not a distribution.

**`provenance` says which mechanism produced the bounds**, and the two are never
merged:

| provenance | what it runs | what its bounds mean |
|---|---|---|
| `hermit-repeat` | a cell repeatedly at one tree | the runs requested directly through `hermit-repeat` |
| `pressure-test` | a cell repeatedly at one tree | the flake distribution — what a yellow-cell floor should be derived from |
| `validate` | a cell once per commit | a point; the regression signal a floor is checked against |

Merging them would give one number that moves for two unrelated causes — "the
code changed" and "this varies run to run". Observations are therefore keyed by
`(detcore_tree, provenance)`. Keying by tree already stopped bounds mixing
across code changes; provenance closes the remaining axis.

Both coordinates above are the position of the PRECEDING scheduler COMMIT, so
they **bound** the divergence rather than locating it: in a 131-line log with
six COMMIT records, every divergence between two of them reports the same turn.

### Writing observations

These scorecard commands write the arrays. `update-observations`,
`import-results`, and `project-observations` remain explicit. `observe-results`
is also available directly, and validation invokes it automatically for each
completed direct top-level run.

Run these commands from the Hermit repository root. In the standard
`dev-hermit/hermit` checkout, the parent series directory is `../series`.
Because `--series-root` is resolved from the current directory, a checkout under
`dev-hermit/worktrees/NAME` uses `../../series` instead.

```console
./ci/compat-envelope/scorecard.rs update-observations --summary FILE   # pressure test
./ci/compat-envelope/scorecard.rs observe-results --results DIR        # validate
./ci/compat-envelope/scorecard.rs import-results \
  --results DIR --current-summary FILE [--current-summary FILE ...]
./ci/compat-envelope/scorecard.rs project-observations \
  --series-root ../series --refreshed-at STAMP                         # series
```

`project-observations` records the selected source as a canonical
repository-relative path together with the exact commit and Git tree object
whose series rows it projected. Thus `../series`, `../../series`, and a symlink
to that directory all record the same checkout-independent `series` identity.
It snapshots the committed JSONL shard population and bytes, then refuses a
worktree with changed, missing, or untracked shards. Git replacement refs are
ignored so the recorded object IDs and read tree cannot disagree.
Malformed rows or shards without their final newline, read-invalid rows, and
conflicting bodies under one `event_id` are also refusals. Semantically
identical repeats of an event collapse to one canonical row, which is what
`rows_read` counts. Projection blocks written before `source_commit` or
`source_tree` existed remain readable, but consumers cannot qualify them until
they are regenerated with the immutable source identity.

`observe-results` walks every `results.jsonl` under `DIR`, so several runs fold
in one invocation — which is how a validate-side range widens beyond a point.
Local validation runs it only after ledger and receipt publication, so the
generated-file write cannot change the tree named by an in-flight receipt.
Nested validates do not write separately. ci-hub also applies the completed
results from its isolated checkout to the checkout that invoked it.
`observe-results` serializes writes for each checkout and merges onto existing
unstaged changes to these two generated files. It refuses staged changes and
unstaged changes to any other tracked file.
No-result directories are an explicit unchanged success. A refused write is
reported separately from the validation verdict and makes the local command
nonzero; neither path commits the generated files.

An off-the-record local validate still adds any clean exact-HEAD per-cell
observations. Those observations cannot qualify a receipt, and the scorecard
writer cannot change which cells are selected or move their green/red state.
`import-results` walks retained history without executing a guest, keeps only
clean schema-4 `BitwiseInfoV1` terminal comparisons from commits on `HEAD`'s
history, and selects the newest such commit independently for every applicable
cell. If several retained runs at that commit disagree, it imports every result
instead of resolving the conflict by file order.

A retained comparison without a divergence position is imported as historical
evidence with its own SHA. A retained position is handled only after a current
pressure summary classifies it: FRESH imports the matching retained position;
DRIFTED replaces it with the current position; WRONG discards it because the
current comparison matches; UNCHECKABLE withholds it because the current row
did not establish a trustworthy result. Each outcome is printed per cell.
One matching run is UNCHECKABLE rather than WRONG because these cells can match
once and diverge on another run. WRONG requires at least two distinct current
runs and no divergence; every classification prints the run count it used.

The writers refuse unrelated tracked changes. `import-results` may replace its
own two generated outputs so the same retained corpus can be imported again.
`observe-results` additionally refuses rows that are not clean at `HEAD`;
`import-results` preserves each historical row's SHA and Detcore tree instead
of relabelling it as current.
`ERROR` rows are reported but not recorded as product behaviour. Neither
pressure-test nor validate observations change scorecard colour.

Current pressure summaries may come from separate clean worktrees. The import
requires each named Hermit commit to exist, requires its recorded Detcore tree
to match that commit, and requires the same Detcore tree as the one being
classified. One DBT case is narrower than the ordinary pressure writer: when
the sole evidence error is that the raw run1/run2 logs were not retained, a
typed canonical, non-vacuous `verdict=diverged` report can supply the current
divergence position. The ordinary pressure writer still refuses that row, and
the import refuses any additional evidence error, matched report, or weaker
comparison.

The historical `never-measured` value in `cells.json` means no observation was
imported. It is not proof that the cell was never run; retained results can
exist before this projection is refreshed.

The series cell name is the exact `test/mode/backend` identity. Passing and
diverging comparisons create observations and update `last_tested`; compressed
`num_runs` rows contribute their full sample count to any recorded divergence
position. `no_result`, `timeout`, `errored`, and `skipped` rows are named but do
not become evidence. Historical rows that predate the `detcore_tree` field use
their exact recorded Hermit commit as a distinct internal grouping key and
never consult the projection checkout's refs or object store. Scorecard schema
7 represents that legacy case as an observation with no `detcore_tree` and
exactly one recorded `hermit_shas` entry. It cannot alias an explicit
Detcore-tree observation, and it does not update `last_tested`, whose staleness
check requires a real Detcore tree. Older scorecards remain readable. A row is
skipped for identity only when it records neither a valid explicit Detcore tree
nor a valid Hermit commit.

This projection does not import retained validate result directories. That is
a separate input path even though both ultimately update the same observation
fields.

During investigation, probe one exact red cell with a tight wall-clock cap:

```console
./ci/compat-envelope/pressure-test.rs run \
  --test applications/example-timed-progress-bar \
  --mode verify --backend ptrace --cell-timeout 60
```

For a reproducible bounded sample across verify, replay, and chaos, run:

```console
./ci/compat-envelope/pressure-test.rs run \
  --sample 10 --seed 42 --cell-timeout 60
```

Add `--mode verify` to sample only the first improvement target. Custom commands
and native naked controls are not part of an unqualified random sample. The
seed and every selected identity are retained in `run.json`.
Chaos cells whose manifests declare no seeds remain red but are not executable.
An exact request refuses before creating a plan; a batch reports and omits
those cells instead of inventing a default seed or recording a zero-execution
failure. The scorecard denominator is unchanged.

Generate the same graph without executing it by replacing `run` with `plan`
and supplying `--results DIR`. A request for every red cell is accepted only
when its declared worst-case occupancy fits `--run-timeout`; otherwise the tool
refuses and tells the caller to select a bounded sample or deliberately provide
a larger wall-clock bound. It does not pretend that thousands of cells fit in
the two-hour default.

The current improvement sequence starts with verify. A verify-only sample does
not change its denominator or green definition:

```console
./ci/compat-envelope/pressure-test.rs run \
  --mode verify --sample 10 --seed 42 --cell-timeout 60
```

The command reuses the canonical Hermit/resource build nodes, serializes
fixture preparation, and gives every red cell its own cgroup-boxed node.
The plan derives that build closure from the selected cells: a sample without
LiteInst does not build the LiteInst runtime, while any sample containing a
LiteInst cell retains the complete canonical LiteInst build chain.
`run` first materializes the exact committed SHA in a temporary local clone,
so ignored Cargo output in the primary checkout cannot change the experiment
and no shared worktree registry is touched. The generated clone is removed
afterward while the run directory remains retained.
Applicable cells not selected by full use the ordinary exact-cell selector;
not-applicable cells use the harness's explicit `--probe-disabled` selector.
Each cell gets at most the
shipped portable DAG's existing 600-second bucket allowance; the manifest's
smaller per-cell timeout still applies inside it. Expected nonzero exits,
timeouts, OOMs, and no-result outcomes stay red but do not stop later cells. If the
cgroup runner itself stops after a bounded cell is killed, the command keeps a
conservative attempt marker and starts another DAG pass; completed builds,
preparations, and cells are not repeated. KVM cells retain the canonical
privileged DAG's 16 GiB hard cap even when their manifest lane is portable. A
malformed published per-cell artifact, or a missing artifact without a
narrowly proven runner timeout or OOM, becomes an infrastructure-error row;
the tool finishes the table and writes `summary.json`, then returns nonzero
rather than claiming a complete population. The retained runner profile is
what distinguishes an OOM or boxed cell timeout from an ordinary nonzero
harness exit. A timeout requires either that exact runner row plus the attempt marker,
or the test harness's separate GNU-timeout signal report plus its named
per-cell timeout result; exit 124 alone is not timeout evidence. Cell results
are published from an `in-progress` path only after the harness returns; that
path is never terminal evidence, so an empty file created before a runner kill
cannot masquerade as a malformed terminal result. The combined `crash-error`
result contains remaining nonzero harness
exits, including signal-caused crashes when the shell reports a nonzero status;
the pressure runner does not currently distinguish the originating signal. A
missing result, verification report, or retained log is accepted only when an
exact-SHA, exact-step runner row records an OOM kill and the cell's numeric
attempt marker exists. Any artifact that does exist must still parse and match
the selected cell.

The ignored run directory retains `dag.json`, `run.json`, captured per-cell
stdout/stderr, result rows, runner profiles, and `summary.json`. Verify-mode
attempts also retain both raw INFO logs named by Hermit. A ptrace verify attempt
runs the same Hermit binary's one-input `log-diff` command and retains the
normalized first-run INFO stream for later cross-backend parity work. Retaining
that input is preparation, not a parity result.
Replay-mode raw-log retention is not implemented yet. A one-time PASS is
printed as a candidate for repeated confirmation; it never edits the tracked
green set automatically.
See the complete command contract with:

```console
./ci/compat-envelope/pressure-test.rs --help
```

This ports the useful one-box-per-red-cell shape from the old parent-workspace
`compat-envelope/expansion-dag.rs`. It deliberately does not port the parent
CSV dependency, invented fallback backend multipliers, or evidence-directory
deletion.

After a clean periodic run, deliberately merge its red-cell measurements with:

```console
./ci/compat-envelope/scorecard.rs update-observations \
  --summary ignored/compat-envelope/pressure-<SHA>-<time>/summary.json
git diff -- ci/compat-envelope/cells.json
```

The command requires the summary's Hermit commit and Detcore tree to equal the
clean checkout at `HEAD`, refuses infrastructure-error rows, and updates only
red-cell observations. For repeated measurements of the same Detcore tree, it
retains the exact Hermit commits measured, every observed result, and the
earliest and latest first-divergence scheduler turn and virtual nanosecond. A
determinism, replay, or parity failure with no measurable divergence point
keeps null fields; it does not get a guessed number. A new Detcore tree gets a
separate observation. Neither this command nor ordinary green regression
validation changes the green set.
