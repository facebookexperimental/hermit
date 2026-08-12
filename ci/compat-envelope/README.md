# Compatibility scorecard

The compatibility scorecard answers one question from a Hermit checkout: how
many manifest-declared test, mode, and backend combinations are inside the
known-green regression envelope at this commit?

Start at [`SCORECARD.md`](../../SCORECARD.md). It is intentionally a small,
versioned table. The stable per-cell identities behind the totals live in
[`cells.json`](cells.json). Raw results, logs, durations, timestamps, and host
data are not versioned; each validate run retains those under `ignored/`.

The denominator is the complete manifest matrix, not just the combinations
that happen to be enabled today. Every one of the 336 tests declares four
Hermit modes across five backends, plus naked execution on native:
`336 × (4 × 5 + 1) = 7,056` cells. `hermit-manifest-plan --format
matrix-json` emits both sides of each manifest's required enabled/disabled
partition. A disabled combination is red; a cell that cannot run is not green.
The existing `--format json` and text views remain enabled-only because they
are execution plans rather than scorecards.

## Ordinary validation

Run:

```console
./validate.sh
```

The path is deliberately direct:

1. `hermit-manifest-plan` validates the complete matrix and emits the enabled
   execution plan.
2. `ci/expected-e2e-plan.json` identifies the cells ordinary validation runs.
3. Each manifest bucket writes schema-3 `results.jsonl` rows to a unique durable
   result directory.
4. The final `scorecard.compatibility` node requires a clean, exact-HEAD PASS
   row for every selected cell and prints the table.
5. The checked-in table and cell identities must still equal what the manifest
   and expected plan derive. Normal validation changes no tracked scorecard
   file.

The selected plan currently has 172 regression cells. Two are chaos-mode
race-exposure checks rather than deterministic/parity claims, so the
compatibility table reports 170 green cells. Both chaos checks still have to
pass validate.

A green cell turning red makes validate fail. The normal response is to fix the
regression. Moving the cell out of the selected plan is not a fix, and
`scorecard.rs update` refuses green-to-red movement unless an explicit
compatibility-standard transition requests it.

See every command and the exact green definition with:

```console
./ci/compat-envelope/scorecard.rs --help
```

## Updating the checked-in table

After deliberately adding a newly proven cell to `ci/expected-e2e-plan.json`,
run:

```console
./ci/compat-envelope/scorecard.rs update
git diff -- SCORECARD.md ci/compat-envelope/cells.json
./validate.sh
```

Review the table delta and the exact cell identity. The update command does not
run a test and cannot turn a red cell green by itself; the subsequent validate
must execute the newly selected cell.

## Red cells and the periodic full-matrix run

Every manifest cell outside the green set is red, including cells that have not
run and cells that cannot currently run. That conservative classification is
intentional: absence of evidence is not green.

The per-cell `observations` arrays are reserved for periodic full-matrix runs.
They are empty in the initial baseline. Generate and inspect the boxed graph
without running it:

```console
run_dir=ignored/compat-envelope/pressure-review
./ci/compat-envelope/pressure-test.rs plan --results "$run_dir"
RUN_DAG_FILE_OVERRIDE="$run_dir/dag.json" ./ci/run-dag.sh portable ascii
```

Run the complete red population from a clean committed checkout with:

```console
./ci/compat-envelope/pressure-test.rs run
```

The command reuses the canonical Hermit/resource build nodes, serializes
fixture preparation, and gives every red cell its own cgroup-boxed node.
Enabled red cells use the ordinary exact-cell selector; disabled red cells use
the harness's explicit `--probe-disabled` selector. Each cell gets at most the
shipped portable DAG's existing 600-second bucket allowance; the manifest's
smaller timeout still applies inside it. Expected FAIL, ERROR, and no-result
outcomes stay red but do not stop later cells. A missing attempt marker makes
the overall pressure run fail rather than claim a complete population.

The ignored run directory retains `dag.json`, `run.json`, per-cell rows/logs,
and `summary.json`. A one-time PASS is printed as a candidate for repeated
confirmation; it never edits the tracked green set automatically. See the
complete command contract with:

```console
./ci/compat-envelope/pressure-test.rs --help
```

This ports the useful one-box-per-red-cell shape from the old parent-workspace
`compat-envelope/expansion-dag.rs`. It deliberately does not port the parent
CSV dependency, invented fallback backend multipliers, or evidence-directory
deletion.

When the periodic run records divergence progress, it will use the first
divergent scheduler turn and virtual nanosecond from Hermit's verification
report. A failure with no measurable divergence point records null values; it
does not get a guessed category. Ordinary green regression validation never
updates those observations.
