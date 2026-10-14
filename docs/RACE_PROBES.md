# Race probes: targeted delays on Hermit's own host-timed edges

Hermit determinizes the guest, but Hermit itself is a multithreaded host program.
Some of its decisions depend on host timing that it cannot control, and treats as
nondeterministic input on purpose. The main example is when a backgrounded
(outside-the-schedule) blocking system call returns, and when the scheduler next
samples that it has. A defect in how Hermit handles one of those orderings can show
up once in a thousand runs under load and never on demand.

Race probes let a developer push those orderings on purpose. They are env-gated
perturbation points compiled into every build, and they are **off unless
`HERMIT_RACE_PROBE` is set**.

## Using them

```console
HERMIT_RACE_PROBE=bg_harvest_skip=0@0.5 HERMIT_RACE_PROBE_SEED=7 \
  hermit run --strict -- ./guest
```

`HERMIT_RACE_PROBE` is a comma-separated list of `point=LO[-HI][@P]` items:

- `LO[-HI]` is a wait in milliseconds, drawn uniformly from `LO..=HI`. Synchronous
  points ignore it; write `0`.
- `@P` is the probability, in `[0, 1]`, that one visit to the point is perturbed. It
  defaults to 1.
- An item naming an unknown point, or one that does not parse, is reported on stderr
  and skipped.

`HERMIT_RACE_PROBE_SEED` (a `u64`) fixes the choices. Without it a seed is drawn
from the host clock. Either way Hermit prints one line to its stderr on the first
visit to a configured point, so a failing run names its seed:

```text
race_probe: HERMIT_RACE_PROBE=bg_harvest_skip=0@0.5 HERMIT_RACE_PROBE_SEED=7
```

Each perturbed visit is also logged at DEBUG under the `race_probe` target.

Neither variable reaches the guest. Under the default `--base-env=host`, Hermit
removes both from the guest's environment, as it does for its own
`HERMIT_VERIFY_TMPDIR`. So turning probes on changes no guest-visible input.

## The points

| point | kind | where | effect |
|---|---|---|---|
| `bg_pre_call` | async wait | `detcore/src/syscalls/helpers.rs`, `record_or_replay_blocking_resource_then`, after the grant and before the backgrounded call runs | the call starts late, as if its host thread were slow to run |
| `bg_post_call` | async wait | the same function, after the call returns and before it asks to rejoin (`BlockedExternalContinue`) | the result is posted late |
| `bg_harvest_skip` | synchronous | `detcore/src/scheduler.rs`, `step2c_process_io_blockers`, per finished blocker in the scheduler's snapshot | the scheduler treats a finished call as still running for this one check |

The waits are `tokio::time::sleep`. They delay only the task that owns the call;
the scheduler keeps running. A blocking sleep at these points would stall the
scheduler as well and hide the race.

## Why an outcome under a probe is a real outcome

Each point only moves host timing that Hermit already accepts as nondeterministic.
A backgrounded call can start late, finish late, or be noticed one scheduler check
late without any probe. On a loaded host its reporting thread can wait hundreds of
microseconds for a CPU. So anything a probe produces, host timing alone can produce.
A failure under a probe is a reproduction of a Hermit defect, not an artefact of
the probe. The probes do not change Hermit's guest-visible decisions on their own;
they change which of the already-possible host orders occurs.

What the rates mean:

- Under a probe, the failure rate measures the probe as much as the race. For
  example, `bg_harvest_skip` at `@0.5` and `@0.9` gave 301/600 and 544/600 on the
  case below.
- **Seed replay is exact only when the visit count before the deciding visit is
  stable.** Decisions are drawn in visit order from one process-wide counter.
  `bg_harvest_skip` seeds replayed almost exactly in measurement: one seed failed
  200/200 and another passed 200/200. `bg_pre_call` seeds replayed less well.

## Cost when off

Unset (or set to nothing valid), each point reads an initialized `OnceLock` and
returns. It draws no random number, writes nothing, and emits no tracing event.
The unit test `race_probes_are_off_unless_configured` in `detcore/src/race_probe.rs`
checks that an unset, empty or invalid configuration yields no probes and that no
point perturbs.

## A worked example

The race behind https://github.com/rrnewton/hermit/issues/4341: case
`sfdsib-nano late rt` of `every_sigchld_copy_is_accounted_for_as_on_linux` in
`hermit-cli/tests/cli.rs`. It failed about 0.15% of the time on a loaded host
(11 of 7324 runs). Under probes it failed:

| `HERMIT_RACE_PROBE` | failing runs |
|---|---|
| `bg_pre_call=300` | 600/600 |
| `bg_pre_call=0-50@0.5` | 272/600 |
| `bg_harvest_skip=0@0.5` | 301/600 |

These numbers were measured on 2026-10-14 at hermit c9bbfbd2, at about 50 runs in
flight and host load average around 300, with the prototype these points were taken
from. The fix, e3bf3e41 ("End a woken sleep for a pending SIGCHLD where the send
window is unusable"), passed the same probes with this facility: 0/600 under
`bg_pre_call=300` and 0/600 under `bg_harvest_skip=0@0.5`.

That is the shape of a fix check: the probe setting (and, for `bg_harvest_skip`, the
seed) that failed before the fix must pass after it.

## Choosing a point for a new race

1. Run the test many ways in parallel at `--log debug` until it fails naturally.
2. In the failing run's log, find the decision that went the wrong way. For
   background calls, look for `Nondeterministic status of backgrounded operations`
   just before a `Skipping global time ahead`.
3. If one of the points above sits on that edge, probe it.
4. If not, add a point there with `crate::race_probe::delay` (async) or
   `crate::race_probe::perturb` (synchronous), add its name to `POINTS`, and
   document it in the table above.

A new point must keep the rule above: it may only delay or defer something whose
timing Hermit already treats as host input.
