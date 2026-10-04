# The timeout ladder

Several independent mechanisms can stop a Hermit run. They are nested, they do
**not** bound the same thing, and they report differently. This file says what
each one bounds, how to tell which one fired, and what has to stay true between
them.

**If a run was killed rather than failing, this is the page for it.** The same
number means different things at different rungs, so two rungs carrying "15" are
not agreeing with each other. Five rungs exit **124**, which is why the exit code
cannot say which one fired and the stderr class line is the discriminator
instead. And a bound at an inner rung that is not strictly smaller than its outer
rung can never fire: it is dead configuration that reads as protection, and it
presents as an intermittently killed test rather than as the configuration error
it is.

`ci/validate-timeout-layers-test.sh` exercises both a named DAG-step timeout and
the outer validate scope. This is the same idea widened to every rung,
including the ones that script does not cover.

The individual-test values below were calibrated from retained results through
2026-09-03 02:18:30 UTC. Re-measure before quoting them; the point of this file
is the *structure*, and the numbers move.

## What each rung bounds

⚠️ **THE UNITS DIFFER, AND THAT IS THE WHOLE TRAP.** "Fifteen seconds" means a
different quantity at each rung, so two rungs carrying the same number are not
agreeing with each other.

| Rung | Bounds | Value comes from | How it stops the run | Reports |
| --- | --- | --- | --- | --- |
| `hermit run --timeout N` | **one hermit invocation** = one guest execution, **ptrace and liteinst only** | the caller's argument | ptrace publishes termination and polls the retained original operation; unresolved cleanup retains its owner and guards. LiteInst keeps its existing timeout/unwind path. | exit 124, `HERMIT_RUN_TIMEOUT class=run-timeout` |
| hermit's unwind fallback | the same invocation, `N + 10s` | `RUN_TIMEOUT_UNWIND_GRACE` in `hermit-cli/src/bin/hermit/run_timeout.rs` | `_exit(124)` from a `SIGALRM` handler; no destructors | exit 124, `HERMIT_RUN_TIMEOUT_FALLBACK` |
| `hermit record --record-timeout N` | one recording | the caller's argument | `_exit(124)` from a `SIGALRM` handler | exit 124 |
| container finalize budget | **the teardown after a run has published its result**: the container child's own exit, and reaping what is left in its namespace | `FINALIZE_BUDGET` in `hermit-cli/src/bin/hermit/owned_container.rs`: 20s, a margin of 13x over the largest measured teardown (1.52s on a saturated 316-CPU host; the Buck-parallel tail that exceeded the old 2s was not measured); `HERMIT_FINALIZE_BUDGET_MS` overrides it | the parent cancels the child (SIGKILL) and waits up to the same budget again, so a child that never exits costs up to **2 x budget** (40s) | exit 125, "container child published its result but did not exit within the ... finalize budget" |
| nextest per-test CPU limit | one cargo test process and its descendants | 22s base, scaled by the machine CPU multiplier | owned attempt cgroup: `SIGTERM`, 2s grace, then `cgroup.kill`; retain typed `cpu_timeout` | nextest fails the named test; CPU report identifies the inner limit |
| nextest `slow-timeout` | **one cargo test process**, which may invoke hermit zero or many times | `.config/nextest.toml`: 57s base, scaled by the machine wall multiplier | `SIGTERM` to the test binary, 2s grace, then `SIGKILL` | wrapper exit 100, test named by nextest |
| manifest cell CPU limit | all process-group CPU consumed by a cell's executions, aggregated across attempts or seeds | `cpu_timeout_seconds`: 22s default plus measured cell overrides, scaled by the machine CPU multiplier | the harness stops the process group and retains `error_kind=cpu-timeout` | typed cell `ERROR` |
| manifest cell wall limit | fixture preparation and, separately, the complete execution phase | `timeout_seconds`: 57s default plus measured cell overrides, scaled by the machine wall multiplier | the harness stops the process group and retains `error_kind=wall-timeout` | typed cell `ERROR` |
| dagrun step wall/CPU limits | **one DAG node**, i.e. a whole batch of cells or tests | explicit `timeout` and `cpu_timeout` on each node in the single `ci/dag/validate.json`, selected by profile labels | dagrun stops the step | node failure |
| validate run budget | the whole outer validate graph | `HERMIT_VALIDATE_RUN_TIMEOUT_SECONDS` or `--run-timeout` | dagrun stops admitting work and records unfinished nodes | incomplete validation, with named unfinished nodes |
| validate systemd scope | the same outer run plus teardown grace | validate's safe-ci scope | systemd stops the whole process tree | outer-scope timeout |
| `safehermit --sh-deadline` | **the whole wrapped process tree** | `bin/safehermit`, default 3600s | `systemd-run --user RuntimeMaxSec`, a **cgroup kill** | exit 124, `safehermit: bound.wall=` |

Distribution of the selected CI-cell values calibrated at that cutoff:

- manifest CPU: 22s ×488, 25s ×1, 32s ×1, 46s ×1, 56s ×1.
- manifest wall: 57s ×487, 58s ×1, 74s ×1, 91s ×1, 105s ×1, 118s ×1.
- historical dagrun step `timeout`: 600s ×15, 900s ×15, 120s ×11, 180s ×6,
  60s ×6, 720s ×5, 1200s ×4, 300s ×3, 2400s ×1, 40s ×1, 30s ×1.

Those distributions describe the snapshot at the cutoff above. Current node wall
and CPU budgets are explicit in `ci/dag/validate.json`; the generator's `--check`
and undeclared-node audit check its current labelled populations.

## Individual-test CPU and wall policy

CPU and wall time are independent measurements. For each retained passing
population, the CPU bound is `ceil(1.5 * p90 CPU)`. The wall bound is
`ceil(4 * p90 wall)` unless that exceeds 120 seconds, in which case it is
`ceil(3 * p90 wall)`. Whole-second rounding is always upward.

The ordinary 22-second CPU and 57-second wall defaults are anchored by the
completing `data-handling/dd-partial-transfers` sample that prompted the policy
(14.298 CPU seconds and 14.019 wall seconds). The current retained p90 census
requires five explicit pairs: `kvm-python-examples` 25/74,
`timed-progress-bar` 32/91, `fp-reduction-nondeterminism` chaos 46/105,
`dd-partial-transfers` 22/58, and `zstd-multithread` 56/118 (CPU/wall seconds).
The other 487 selected cells require at most 12/49. The 188 runnable but
`ci:false` cells had no retained passing samples and are reported as unsampled,
not treated as calibrated.

Machine-specific CPU and wall multipliers are deliberately separate:
`HERMIT_TEST_CPU_TIMEOUT_MULTIPLIER` and
`HERMIT_TEST_WALL_TIMEOUT_MULTIPLIER`. Each defaults to `1`, must be a positive
finite number, and scales its configured component with ceiling rounding. The
nextest wrapper generates a temporary TOML config with the scaled 22-second CPU
budget and every `slow-timeout.period` scaled by the wall multiplier, passes it
with `--config-file`, and removes it on all exits. The CPU budget plus its
two-second cleanup grace must remain below every finite wall termination bound
(`period × terminate-after`); invalid or
overflowing values refuse before tests start. Each test attempt runs in an owned
cgroup so CPU used by descendants remains charged after their parent exits. A
CPU stop is retained separately from a native failure, external signal, or
nextest wall timeout; the first observed cause is preserved. Budgeted Nextest
requires a writable delegated cgroup v2 subtree; normal boxed dagrun supplies
`Delegate=yes`. A standalone invocation without delegation refuses rather than
running without its CPU limit. This does not establish container-route support
without an actual run in that container. Validation refuses a wall multiplier whose scaled inner bounds
would outgrow a committed outer DAG backup. Current result rows carry both
effective bounds as
`execution_cpu_timeout_seconds` and `execution_wall_timeout_seconds`; readers
accept older rows with neither field but refuse current publication unless both
are present and consistent.

## Regular Nextest calibration and enclosing budgets

The prepared regular-crates runner can use `.config/nextest-budgets.json`, a
derived configuration under the same CPU/wall formulas above. Raw observations
remain in the existing run evidence; this file is not a second history store.
It keys each row by the exact package, binary ID and test name from Nextest's
typed selected inventory. The resolver produces one CPU/wall pair and retains
the complete resolved map beside the CPU report as `.budgets.json`. Both the
generated wall configuration and every CPU-wrapper invocation bind those same
bytes; the wrapper refuses a changed digest or an absent executing identity.

Applicability requires the recorded source, verified build context, CPU model,
available CPU allocation and emitted Nextest concurrency to match. It also
requires the same verified execution domain (native host or exact pinned image
and config ID), ancestor CPU quota/period limits, affinity/cpuset and effective
memory/swap limits. The source
fingerprint includes every tracked tree entry and gitlink except this one
derived calibration file; dirty source cannot activate it. This avoids a
calibration invalidating its own measurements when its table is written.
Prepared-artifact checks still verify the actual source views, compiler,
configuration and executable bytes. Comparison context replaces only a
verified target directory and configuration placement with logical locations;
it does not equate executable hashes from different checkouts.

The maintained launcher observes host ancestors before entering a pinned root;
the inner verifier checks the actual image and visible cgroup anchor/controls
against the dedicated read-only proof mount. Hidden ancestors remain explicitly
launch-observed. Each invocation retains its proof bytes beside the CPU report
as `.launch.json`; its hash binds the context and final run but is not part of
the reusable cohort key. Missing or legacy cohort evidence keeps defaults,
while malformed supplied evidence refuses. Bootstrap wrapper calls do not
capture or compile an observer. Existing calibration tables are not activated
by adding this transport; new matching measurements are still required.

Current-cohort p90 values are not pooled across resource allocations. An
applicable historical positive maximum supplies a conservative floor before
the owner formula is evaluated. With three current samples, p90 is the maximum
of those three, not a statistically established tail. Bounds are positive whole
seconds, with base wall at least CPU+3. If either formula exceeds the existing
22/57 defaults, that row remains explicitly unresolved at the unchanged
defaults; a high observation is neither discarded nor called calibrated after
truncation. Each machine multiplier is applied once, followed by the strict
CPU+2 < wall check. A collision refuses rather than extending wall silently.

Missing or stale calibration keeps every selected case at its existing default
bounds, allowing a changed source tree to be measured without a circular gate.
Unsupported profiles, overrides and standalone consumers retain their existing
configuration. A measured calibration does not authorize case exclusions,
assertion changes, retries being ignored, or a serialized group being treated
as independent work.

The regular population calculation sums every attempt's CPU allowance. For
independent work-conserving slots of actual width J, its nominal wall allowance
is `ceil((sum(wall_i+2) + (J-1)*max(wall_i+2))/J)`, with sequential retries
included in each case. Nextest times the whole wrapper and sends process-group
SIGKILL after its two-second wall grace; the wrapper's own gentle shutdown and
five-second reap deadline run inside that outer lifetime. This model assumes a
responsive runner and scheduler. Setup, reporting, accounting delay and failed
cleanup remain separate infrastructure work under the unchanged containment.

The report distinguishes nominal work from available headroom under the regular
node's 900 wall/7200 CPU limits. Conservative defaults can exceed those totals;
that is uncertified aggregate headroom, not a promise that all cases can consume
their maximum simultaneously. Emergency expiry remains incomplete and
non-green. The ordinary selected eleven-node run retains its 4200-second
whole-work cap even though its nominal dependency critical path can total
6300 seconds. Neither this calibration nor healthy observed margins qualify
the separate unmeasured Detcore and full-profile populations.

## Gentle first, hard as fallback

Owner ruling: a run is stopped by a **proper teardown with a gentle kill of
hermit itself**, and a hard kill is the fallback, never the first move. Hermit
made the container, so hermit unwinds it.

Only the innermost rung can do that, because it is the only one running inside
the process that owns the container and the log. The rungs above it stop hermit
from outside; the best they can do is stop it *cleanly*, which is not the same
as unwinding.

⚠️ **A CGROUP KILL CANNOT BE GENTLE, BY CONSTRUCTION.** `safehermit`'s deadline
is `SIGKILL` delivered to every cgroup member at once, with no `SIGTERM` phase
and no opportunity for any member to run a destructor. This is not a defect to
repair — it is what that rung is *for*: the runaway that does not answer. It
stays as the outermost backstop and should not be pressed into service as a
per-cell bound.

⚠️ **AN EXTERNAL KILL DOES NOT LEAVE PROCESS RESIDUE TODAY, AND THE ARGUMENT
THAT IT DOES IS OUT OF DATE.** Measured 2026-08-26: `SIGTERM` to the outer
`hermit` process left no surviving guest and no surviving hermit — the outer
exited 143 and the run was gone. `PR_SET_PDEATHSIG` and the container-init stop
handlers in `hermit-cli/src/bin/hermit/container.rs` already closed that hole,
which is why three runs once survived 45 hours and none does now. The case for
the innermost rung is **not** that the outside cannot clean up. It is that only
the inside can produce a *record* of what happened before it stops: an
externally killed run writes no verdict and a truncated log. The residue is
evidence, not processes.

## Which rung fired: read the class line, not `$?`

⚠️ **THE EXIT CODE CANNOT ANSWER THIS AND MUST NOT BE ASKED.** Five different
rungs exit **124** — hermit's own bound, hermit's fallback, `hermit record`'s
bound, the manifest cell's `timeout(1)`, and `safehermit`'s wall deadline. GNU
`timeout` uses 124 for the same event. A consumer that branches on `$?` alone is
guessing which mechanism stopped the run, and will attribute a slow guest to the
wrong layer.

**The contract is the class line on stderr.** Each rung emits a distinct,
greppable marker, and that marker — not the status — is what a caller keys on:

| Marker | Meaning | What to change |
| --- | --- | --- |
| `HERMIT_RUN_TIMEOUT class=run-timeout` | hermit's own bound expired and hermit unwound the container. The line also carries the bound in seconds. | the guest is genuinely slow, or the bound is too tight |
| `HERMIT_RUN_TIMEOUT_FALLBACK` | the bound expired and **the unwind itself did not finish** within the grace. This is a hermit defect, not a slow guest. | investigate the wedged teardown |
| `HERMIT_INTERNAL_FAILURE class=cli-error` with "did not exit within the ... finalize budget" | the run had published its result, but the container child did not exit within the finalize budget and hermit cancelled it; the published result was discarded | a teardown stall: raise `HERMIT_FINALIZE_BUDGET_MS` on an overloaded host, or investigate the child's teardown |
| `safehermit: bound.wall=APPLIED` … then a kill | the outermost cgroup deadline reaped the tree | the run escaped every inner bound |
| a nextest-named test with wrapper exit 100 | the test **process** exceeded its per-test cap | `.config/nextest.toml` |
| exit 124 with **no marker at all** | something outside hermit killed it — `timeout(1)` on the cell, or a harness | the cell's `timeout_seconds`, or a missing inner bound |

That last row is the useful one. **A cell that times out with no class line means
no inner bound fired**, which is either a missing `--timeout` or an inner bound
set larger than the outer one. Both are configuration errors, and the marker's
absence is what distinguishes them from a genuinely slow guest.

## `--timeout` is qualified per backend, and refuses elsewhere

⚠️ **A TIMEOUT FLAG READS AS BACKEND-AGNOSTIC AND THIS ONE IS NOT.** Measured
2026-08-26 with `--timeout 3` against a guest that never exits, two runs each:

| Backend | Elapsed | Marker | Meaning |
| --- | --- | --- | --- |
| `ptrace` | 3s | `class=run-timeout` | bound works, container unwound |
| `liteinst` | 3s | `class=run-timeout` | bound works, container unwound |
| `kvm` | 13s | `HERMIT_RUN_TIMEOUT_FALLBACK` | bound holds, but ONLY via the hard fallback |
| `sabre` | 40s | none | **did not bound the run at all** |
| `dbt` | 20s | none | **did not bound the run at all** |

⚠️ **READ THAT TABLE AS A DEFECT REPORT, NOT A SUPPORT MATRIX.** `sabre` and
`dbt` are not backends that merely lack testing — **they structurally cannot
honour the flag today.** The 40s and 20s are the *harness's* own deadline, not
hermit's, and the missing marker is exactly the "exit 124 with no marker"
reading above: the bound was accepted and then had no effect whatsoever. All
five backends run correctly **without** the flag, so it is the bound that fails,
not the backend.

The mechanism is visible on `sabre`, which additionally panicked in reverie's
blocking RPC transport after **69 seconds** — `reverie-rpc-transport`'s
`blocking_client`, reporting a broken pipe once the container went away. A
*blocking* call cannot yield to the single `current_thread` tokio runtime that
`tokio::time::timeout` needs in order to fire, so the primary path is never
reached. `dbt` shows the same absence of any bound and is a launch adapter
around DynamoRIO, so it is very likely the same class of problem; that has not
been traced and should not be asserted.

⚠️ **THE DISTINCTION MATTERS FOR WHOEVER FIXES IT.** "Unverified" invites
someone to run the flag once, see it accepted, and mark the backend supported.
What is actually required is finding out why the runtime never reaches the timer
and fixing that — and until it is fixed, the refusal below is the honest
behaviour rather than a placeholder.

So `--timeout` **refuses** (exit 122, `HERMIT_POLICY_REFUSAL`) on any backend
other than `ptrace` and `liteinst`, rather than accepting a bound it cannot
enforce. `kvm` is refused for a softer reason than the other two: it does bound
the run, but every KVM timeout would take the hard `_exit` path and emit the
marker that is supposed to mean *the unwind failed*, destroying the signal that
marker carries. Qualify a backend by finding out why the runtime never reaches
the timer — not by widening the list.

Fail-closed, so a new backend must be qualified deliberately instead of
inheriting a guarantee nobody measured for it.

## The invariant

**Each wall-clock rung must be strictly smaller than the rung outside it.**

If an inner bound is greater than or equal to its outer bound, the inner one can
never fire. It is then dead configuration that reads as protection — and the
run is stopped by the outer rung instead, hard and unnamed. The symptom is a
test that looks intermittently killed for no stated reason, which is why this
belongs in a configuration check and not in a debugging session.

CPU consumption is a separate axis: a multi-threaded process can consume more
than one CPU-second per wall second, while a wedged or descheduled process can
consume almost none. That is why neither bound replaces the other.

`ci/validate-timeout-layers-test.sh` exercises the named DAG-step and outer-scope
stops. `scripts/validate.rs --self-test` also proves the checked-in nextest and
manifest wall defaults agree and that representative multiplier scaling uses
the same ceiling rule; planted base and multiplier mismatches are rejected.

## Known gap

`hermit-cli/tests/container_init_deadline.rs` — which defends `PR_SET_PDEATHSIG`
and the container-init stop handlers, i.e. the guarantee that an external
deadline can end a hung run at all — **was in no DAG node at that cutoff**.
The historical enumeration of every `--test <target>` across
`ci/dag/portable.json` and `ci/dag/privileged.json` yielded 50 targets and omitted
that file, so those cells did not run in that validation snapshot. The regression cells for `hermit run --timeout` are in
`hermit-cli/tests/cli.rs` for that reason.

If that file is ever wired in, its own 12-second startup and 20-second teardown
budgets must remain below the enclosing 57-second nextest base bound.
