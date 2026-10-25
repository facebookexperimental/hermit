# The timeout ladder

Several independent mechanisms can stop a Hermit run. They are nested, they do
**not** bound the same thing, and they report differently. This file says what
each one bounds, how to tell which one fired, and what has to stay true between
them.

**If a run was killed rather than failing, this is the page for it.** The same
number means different things at different rungs, so two rungs carrying "15" are
not agreeing with each other. Six rungs exit **124**, which is why the exit code
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
| `hermit run --timeout N` | **one hermit invocation** = one guest execution, **ptrace only** (in-guest LiteInst refuses it) | the caller's argument | ptrace publishes termination and polls the retained original operation; unresolved cleanup retains its owner and guards. | exit 124, `HERMIT_RUN_TIMEOUT class=run-timeout`; a requested summary is kept, marked partial (see below) |
| `hermit run --virtual-timeout N` | **one hermit invocation's virtual time**: Detcore's global virtual time since the run began, the `virttime_elapsed` of its summary, in virtual nanoseconds, checked as each scheduler turn commits it | the caller's argument | the scheduler grants no further turn; ptrace unwinds the guest as for `--timeout` and finishes the run; other backends exit 124 from the scheduler (see below) | exit 124, `HERMIT_RUN_TIMEOUT class=virtual-timeout bound_ns=N` |
| hermit's unwind fallback | the same invocation, `N + 10s` | `RUN_TIMEOUT_UNWIND_GRACE` in `hermit-cli/src/bin/hermit/run_timeout.rs` | `_exit(124)` from a `SIGALRM` handler; no destructors | exit 124, `HERMIT_RUN_TIMEOUT_FALLBACK` |
| `hermit record --record-timeout N` | one recording | the caller's argument | `_exit(124)` from a `SIGALRM` handler | exit 124 |
| container finalize budget | **the teardown after a run has published its result**: the container child's own exit, and reaping what is left in its namespace | `FINALIZE_BUDGET` in `hermit-cli/src/bin/hermit/owned_container.rs`: 20s, a margin of 13x over the largest measured teardown (1.52s on a saturated 316-CPU host; the Buck-parallel tail that exceeded the old 2s was not measured); `HERMIT_FINALIZE_BUDGET_MS` overrides it | the parent cancels the child (SIGKILL) and waits up to the same budget again, so a child that never exits costs up to **2 x budget** (40s) | exit 125, "container child published its result but did not exit within the ... finalize budget" |
| nextest per-test CPU limit | one cargo test process and its descendants | 22s base, scaled by the machine CPU multiplier | owned attempt cgroup: `SIGTERM`, 2s grace, then `cgroup.kill`; retain typed `cpu_timeout` | nextest fails the named test; CPU report identifies the inner limit |
| nextest `slow-timeout` | **one cargo test process**, which may invoke hermit zero or many times | `.config/nextest.toml`: 57s base, scaled by the machine wall multiplier | `SIGTERM` to the test binary, 2s grace, then `SIGKILL` | wrapper exit 100, test named by nextest |
| manifest cell CPU limit | all CPU consumed by a cell's executions, aggregated across attempts or seeds; each execution is measured live in a cgroup of its own (see below) | `cpu_timeout_seconds`: 22s default plus measured cell overrides, scaled by the machine CPU multiplier | the harness stops the process group, kills what is left in the execution's cgroup, and retains `error_kind=cpu-timeout` | typed cell `ERROR` |
| manifest cell wall limit | fixture preparation and, separately, the complete execution phase | `timeout_seconds`: 57s default plus measured cell overrides, scaled by the machine wall multiplier | the harness stops the process group, and the execution's cgroup when it has one, and retains `error_kind=wall-timeout` | typed cell `ERROR` |
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

Manifest cells measure live CPU from a cgroup as well. Before exec, each
budgeted execution joins a cgroup v2 child that the harness creates below its
own cgroup, and the harness reads that child's `cpu.stat` `usage_usec` every
100 ms, so a descendant that leaves the process group, for example through
`setsid`, stays charged. The final charge is still the leader's `wait4` usage,
raised to the triggering live sample when the CPU budget stopped it. Once the
leader is reaped, whatever is left in the cgroup is killed there and the cgroup
is removed, together with any cgroups the command made below it (as a nested
runner does), deepest first. A wall or CPU stop sends `SIGTERM` to the process
group and waits up to 10 s for the leader to exit; it then sends `SIGKILL` to
the process group,
kills the whole cgroup through `cgroup.kill`, and sends `SIGKILL` to the leader
through a pidfd before it waits for the leader, so a leader that moved to
another process group cannot hold the stop open. A process can move itself out
of that cgroup into the harness's own, where the counter no longer sees its
CPU, so before every live sample, before a wall stop, and once the leader has
been waited for, the harness reads the cgroup v2 line of `/proc/<pid>/cgroup`
for the leader and for every live member of its process group. Any of them
outside the execution's cgroup, or a check that cannot be made, stops the
execution through the same sequence (only `SIGKILL` to the process group when
the leader has already exited), charges no CPU, records the termination
`cgroup_membership_stop`, and makes the cell an `ERROR`. A process that leaves
both the process group and the cgroup, or leaves the cgroup and exits between
two checks, is not seen; the process-group scan the cgroup replaced could not
charge it either. A command that empties the cgroup, makes it threaded through
`cgroup.type` and moves only its main thread back is not refused either, and
its other threads' CPU then goes to the harness's cgroup; the process-group
scan did count those threads. A sample that cannot be read,
or that is lower than one already read, is a failed sample. CPU that stays
unmeasurable for 1 s, timed from when
the first failed sample returned, stops the execution rather than letting it run
without its budget. An execution whose cgroup cannot be created, for example
because the harness's own cgroup is not writable, has no CPU source at all, so
it is refused before it starts and the cell is an `ERROR`; that grace is only
for a meter that was established and then failed. The one exception is a
launcher that ran without cgroups on purpose and set
`HERMIT_E2E_ALLOW_PROCESS_GROUP_CPU_SCAN=1`: the agent-utils process-group
scan then measures the execution, and its rows name
`agent_utils_paired_pidfd_stat_v1` instead of `cgroup_v2_invocation_cpu_stat_v1`
as their CPU source. `scripts/validate.rs` sets the marker only for a run that
`--allow-cgroup-failure` left unboxed, and refuses to start a boxed run whose
caller set it. `ci/run-dag.sh` sets it only for `--unsafe-no-cgroups`, the
privileged workflow only for its unboxed occasional KVM probe step,
`ci/buck-e2e/cell.sh` only for cells on the RE route (an RE worker's cgroup is
not writable by the test; local and pinned-root cells drop a caller's marker),
and `ci/compat-envelope/pressure-test.rs` never. The boxed-run refusal therefore
covers only cells executed on the validating host: a boxed
`--e2e-runner buck-hybrid` run still imports RE rows whose CPU source is
`agent_utils_paired_pidfd_stat_v1`.

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

⚠️ **THE EXIT CODE CANNOT ANSWER THIS AND MUST NOT BE ASKED.** Six different
rungs exit **124** — hermit's own bound, its virtual bound, hermit's fallback,
`hermit record`'s bound, the manifest cell's `timeout(1)`, and `safehermit`'s
wall deadline. GNU `timeout` uses 124 for the same event. A consumer that
branches on `$?` alone is guessing which mechanism stopped the run, and will
attribute a slow guest to the wrong layer.

**The contract is the class line on stderr.** Each rung emits a distinct,
greppable marker, and that marker — not the status — is what a caller keys on:

| Marker | Meaning | What to change |
| --- | --- | --- |
| `HERMIT_RUN_TIMEOUT class=run-timeout` | hermit's own bound expired and hermit unwound the container. The line also carries the bound in seconds. | the guest is genuinely slow, or the bound is too tight |
| `HERMIT_RUN_TIMEOUT class=virtual-timeout bound_ns=N` | the run's virtual clock passed `--virtual-timeout` (see below). For a guest whose input all comes from inside the container, the stop is at the same turn of the same schedule on every host and in every run. | the guest really runs that long in virtual time, or the bound is too tight |
| `HERMIT_RUN_TIMEOUT_FALLBACK` | the bound expired and **the unwind itself did not finish** within the grace. This is a hermit defect, not a slow guest. | investigate the wedged teardown |
| `HERMIT_INTERNAL_FAILURE class=cli-error` with "did not exit within the ... finalize budget" | the run had published its result, but the container child did not exit within the finalize budget and hermit cancelled it; the published result was discarded | a teardown stall: raise `HERMIT_FINALIZE_BUDGET_MS` on an overloaded host, or investigate the child's teardown |
| `safehermit: bound.wall=APPLIED` … then a kill | the outermost cgroup deadline reaped the tree | the run escaped every inner bound |
| a nextest-named test with wrapper exit 100 | the test **process** exceeded its per-test cap | `.config/nextest.toml` |
| exit 124 with **no marker at all** | something outside hermit killed it — `timeout(1)` on the cell, or a harness | the cell's `timeout_seconds`, or a missing inner bound |
| `HERMIT_MODEL_DEADLOCK class=deadlock-under-hermit-model`, exit **121** | **no rung fired**: no wake Hermit models can reach any remaining thread, and the scheduler ended the run (see below) | the guest's synchronization, or a gap in Hermit's model; the report above the line names every blocked thread |

That last row is the useful one. **A cell that times out with no class line means
no inner bound fired**, which is either a missing `--timeout` or an inner bound
set larger than the outer one. Both are configuration errors, and the marker's
absence is what distinguishes them from a genuinely slow guest.

## A deadlock under Hermit's model is not a timeout

Under Hermit every remaining thread can be blocked on something no later
scheduler turn can supply, so "no thread can be woken now" is also "no thread
can ever be woken" by anything Hermit models. The scheduler prints its
`Deadlock detected: ...` report and the run ends at once, in virtual time and
without waiting for any bound.

⚠️ **THAT IS A VERDICT OF HERMIT'S MODEL, NOT THE GUEST'S RESULT.** When the
model covers every wake the program relies on, Linux would leave it hung
forever. When it does not, the deadlock is Hermit's bug: robust-futex owner
death, dash's `sigsuspend` after `wait4(WNOHANG)`, a raw exit, KVM's `kill`
and happens-before re-admission each once produced this report (their
regression tests in `hermit-cli/tests/cli.rs` and
`hermit-cli/tests/robust_futex_owner_death.rs` say so). So it is neither a
guest status nor `HERMIT_INTERNAL_FAILURE`: it has its own status and class
line, `HERMIT_MODEL_DEADLOCK class=deadlock-under-hermit-model`, exit **121**
(allocated in `hermit-cli/src/lib.rs` beside 122-127). To tell the two cases
apart, run the program natively (or under `--strace-only`) and see whether it
hangs.

On ptrace the run is unwound the way `--timeout` unwinds one, by publishing a
termination that tears the parked guest down, and then finished: its output so
far is kept, `--summary` is printed and names the deadlock, and a recording is
committed and replays to the same deadlock. Nothing takes it as a guest
result by default. `--verify` applies `--verify-allow` to it as to any status
that is not success: the default `success` refuses run 1 (`no_result`,
`first_run_rejected`, `verified: false`), and only an explicit `failure` or
`both` compares the runs, in which case `verified` says the deadlock repeats
at the same point, not that it is native. The report records
`guest_exit_code: 121`. `hermit-verify chaos-stress` stops on it as on any
other outcome of Hermit's own, and an `analyze` trial that ends on it fails
with it rather than counting as on or off target. A
teardown that records a failure of its own (a capture drain or cleanup error)
is Hermit's failure instead, `HERMIT_INTERNAL_FAILURE`, because the kept output
may then be incomplete. Library callers receive `hermit::SchedulerEndedRun`,
carrying the outcome and the output when the run captured it. Reverie still
logs each tracee the unwind kills (`guest terminated by signal ... SIGKILL`),
as it does for `--timeout`.

Only ptrace unwinds the guest, and only ptrace is measured. On the other
backends the scheduler still exits with 121 from the process it runs in. From
a container child the parent reports that exit the same way, without a
summary. A scheduler that runs elsewhere hands 121 to Hermit as an ordinary
exit status, without the class line: DBT drives its scheduler in Hermit's own
process (see `TerminalDeadlockRecording` in `hermit-cli/src/bin/hermit/verify.rs`),
whose `--verify` records `run_failed` with failure `deadlock` as before.

The scheduler's own `HERMIT_DEADLOCK: ...` verdicts are not this. They say
Hermit cannot go on (a broken invariant, or a death the backend never
reported), and stay `HERMIT_INTERNAL_FAILURE class=container-child-exit`,
exit 125.

## A virtual timeout bounds virtual time, not the host's

`--virtual-timeout` is refused with `--gdbserver` or `--dap-listen`, before
starting the guest or debugger. A debugger can suppress a fatal signal after
Detcore has marked the process doomed; the surviving guest would then be
excluded from the virtual bound. Until that final-delivery state is reconciled,
Hermit fails closed with `HERMIT_VIRTUAL_TIMEOUT_WITH_DEBUGGER`. Remove either
the debugger option or the virtual bound, or use `--timeout` for a wall-clock
bound. Debugger runs without a virtual bound and virtual bounds without a
debugger retain their existing admission.

`hermit run --virtual-timeout=N` bounds the run in **virtual** nanoseconds:
Detcore's global virtual time since the run began, the same quantity
`--summary` reports as elapsed (`virttime_elapsed` in `--summary-json`). The
scheduler reads it each time it commits the global clock at the start of a
turn, before the turn's thread runs, and the first commit more than `N` past
the run's start ends the run, provided some thread could still be granted a
turn: it prints `Virtual timeout: at scheduler turn T, the run's virtual clock
is E ns past its start, beyond --virtual-timeout=N; no further turn is
granted.`, and the run exits 124 with
`HERMIT_RUN_TIMEOUT class=virtual-timeout bound_ns=N`. A commit past `N` after
the guest's last thread has exited, or after committed decisions doomed every
remaining process (a granted exit, a SIGKILL, an exec in flight, a delivered
fatal signal, or the exit of its PID namespace's init), stops nothing: the
guest ended on its own, and its own status stands. The marks err toward too
much, so a process that a committed decision marked doomed but that survives (a
process-group SIGKILL that failed) is not stopped by the bound either, the
same way in every run. A failed exec clears its in-flight mark before the
old image resumes. After a SIGKILL whose victim Detcore
could not name, that victim counts until the host reports it gone, so a stop
decided after one is recorded as a determinism loss. The check reads the clock
and never moves, rounds or resets it, and it does not change how threads are
scheduled before the stop.

`--timeout` fires wherever the guest has got to when the host's clock runs out,
so where it stops depends on how fast the host is. For a guest whose input all
comes from inside the container, the virtual bound stops the same schedule at
the same turn on every host and in every run, so the output written before it,
`--summary` (which says the bound fired) and `--verify` are reproducible: under
an explicit `--verify-allow=failure` or `both`, two runs that stop at the same
turn verify equal at bitwise parity. A thread that
polls something outside the container (retries a nonblocking call on a pipe or
socket a host process feeds) advances virtual time with each retry, and how
many retries it takes depends on the host, so such a run can stop at another
turn; `--verify --verify-strict` reports that as a divergence, because the
committed time in every compared COMMIT record moves with it.

It is still not a wall-clock bound, and the wall-clock invariant below does
not order it against the rungs above it: a large virtual bound can take any
amount of host time to reach, and while every thread is blocked in a call
outside the container virtual time does not pass at all, so the wall-clock
rungs stay in place.

⚠️ **A VIRTUAL TIMEOUT IS NOT A GUEST RESULT EITHER.** The bound is Hermit's,
imposed for the caller, and the guest had not finished when it fired; a gap in
Hermit's model (a missed wake that turns into a spin-poll, as with the
deadlocks above) can also be what ran the clock past it. So Hermit treats it
the way it treats a deadlock under Hermit's model. `--verify` applies
`--verify-allow` to it: the default `success` refuses run 1 (`no_result`,
`first_run_rejected`, `verified: false`, still ending with the class line and
124), and only an explicit `failure` or `both` compares the runs, in which case
`verified` says both runs stopped at the same turn. `--verify-run` pairs
compared by `log-diff --verify-json` follow the same rule. `hermit-verify
chaos-stress` stops on it as on any other outcome of Hermit's own instead of
counting it as the guest failing under that schedule, and an `analyze` trial
that it stops fails with it rather than counting as on or off target. These
three choices follow the deadlock's by consistency; a guest that exits 124 by
itself, with no class line, is still judged by its status.

On ptrace the scheduler hands the stop to the run's owner, which tears the
parked guest down and finishes the run as it does for a deadlock under Hermit's
model (above).
KVM's scheduler exits the container child 124 directly. That status is also
`--timeout`'s fallback's, so a run with the bound maps a note the child shares
with its parent (a `MAP_SHARED` cell mapped before the clone, and cleared
before every launch), the scheduler writes the bound there just before it
exits, and the parent reports `class=virtual-timeout` only when the note says
so. Such a run has no summary, and two of them cannot be compared, so
`--verify` (or `--verify-run`) with the bound is refused on KVM.

The option needs Detcore's sequentializing scheduler and virtual time: it is
refused with `--no-sequentialize-threads`, `--namespace-only`, `--strace-only`
or `--no-virtualize-time`. Like `--timeout`, it is qualified per backend and
fail-closed: accepted on ptrace (and e9patch preprocessing, which runs on it)
and KVM, where it was measured; refused on LiteInst, in-guest-trap and SaBRe
until a run there qualifies it; and refused on DBT, which drives Detcore's
scheduler in Hermit's own process, so the exit that ends a stopped run would
end Hermit itself without its class line. `hermit record` takes no virtual
bound.

## A run `--timeout` stopped keeps its summary

A timed-out run is when its statistics are most needed, so `--summary` and
`--summary-json` still report it. The summary prints before the class line, and
its first line says it is partial:

```text
PARTIAL: --timeout=2 (seconds of wall-clock time) stopped the run before the guest exited; these statistics describe the run up to then.
```

The JSON carries the bound as `"run_timeout_secs": 2`. A run whose guest
exited first keeps its complete summary without the bound, even when the
deadline fires during Hermit's own cleanup after that.
The scheduler's turns, recorded events and virtual time are the run's as it
stood at the deadline, and the syscall and timeslice counts include the threads
the deadline killed, from their exit reports. A `--record-preemptions-to`
recording is flushed after the summary, as for any failed run: the summary
names the file even if that write then fails, and the write error is the
run's, as before. Without `--summary`, the summary goes to the INFO log, as a
finished run's does. The partial summary is withheld, with a warning naming
why, when the scheduler task did not finish, a handler panicked while holding
the scheduler or clock lock, or the summary could not be written.

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
other than `ptrace`, rather than accepting a bound it cannot enforce. (The
`liteinst` row above measured a ptrace-hosted hybrid that no longer exists;
in-guest LiteInst refuses the flag in
`refuse_unqualified_liteinst_in_guest_options`.) `kvm` is refused for a softer
reason than the other two: it does bound the run, but every KVM timeout would
take the hard `_exit` path and emit the marker that is supposed to mean *the
unwind failed*, destroying the signal that marker carries. Qualify a backend by finding out why the runtime never reaches
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
