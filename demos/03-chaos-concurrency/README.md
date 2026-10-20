# Demo 3: Chaos concurrency testing

A data race may fail once in a thousand runs and then refuse to fail while you
debug it. Hermit's chaos mode makes thread-scheduling decisions from a seeded
random number generator: different seeds explore different thread orders, and
the same seed always reproduces the same order. The demo shows one seed that
passes and one that fails, surveys 16 seeds, then saves the failing seed's
schedule to a file and replays the file under the passing seed: the replay
fails the same way, so the file, not the seed, decides the outcome.

## Prerequisites

- `hermit` on your `PATH`, built from this checkout (see
  [Setup](../README.md#setup)).
- `cargo` and a C compiler. The demo builds the test program `hello_race`
  from `flaky-tests/hello_race.rs`.

## Run it

From the repository root:

```bash
demos/03-chaos-concurrency/run.sh
```

The same demo runs with `make -C demos demo3`. Once `hello_race` is built, it
took 18 and 23 seconds in two runs on 2026-10-03, on a host with a load average
of about 190 and 260; most of that is the 20 Hermit runs, about a second each.

To try one seed yourself:

```bash
cargo build --locked -p hermetic_infra_hermit_flaky-tests --bin hello_race
hermit --log=error run --chaos --seed=0 --base-env=minimal --no-virtualize-cpuid --max-timeslice=disabled --env=HERMIT_MODE=chaos -- target/debug/hello_race
```

With `--seed=0` the program prints `Final value: 1`, then
`Antagonistic schedule reached, failing.`, and exits with status 1. Change
`--seed=0` to `--seed=1` and it prints `Final value: 2`, then
`Did not find antagonistic schedule. Succeeding.`, and exits with status 0. Run
either command again and you get the same result.

## What you will see

Output of `demos/03-chaos-concurrency/run.sh` on 2026-10-03, with Hermit built
from commit `b82e83b510a6`. Cargo's build lines and the warning that this Hermit
was not built from the demo's checkout are left out. Every Hermit run also
prints a `hermit: virtual-time epoch=... source=host-now` line on standard
error (see [demo 1](../01-deterministic-run/README.md#how-it-works)); those
lines are left out of the first two sections. In the last section their
host-clock instants are shown as `...`.

```text
==========================================
=== Demo 3: Chaos Concurrency Testing  ===
==========================================

hello_race contains an intentional data race. Chaos mode makes scheduler
choices with a seeded PRNG, so different seeds explore different interleavings
and the same seed reproduces the same result. Seed 1 passes; seed 0 reaches the
antagonistic schedule and returns the guest's expected failure status. The demo
surveys seeds 0-15, then records seed 0's schedule to a file and replays the
file under seed 1: the replay fails like the recording, with identical output.

==========================================
Using hermit 0.4.0 (2026-10-03, gb82e83b510a6) (...)

=== Seed 1 passes; seed 0 reproduces the expected failure ===
Final value: 2
Did not find antagonistic schedule. Succeeding.
Final value: 1
Antagonistic schedule reached, failing.
seed 0 reproduced the expected concurrency failure

=== Survey seeds 0..15, retaining each run's output ===
seed=0 result=fail
seed=1 result=pass
seed=2 result=pass
seed=3 result=pass
seed=4 result=pass
seed=5 result=fail
seed=6 result=fail
seed=7 result=pass
seed=8 result=pass
seed=9 result=pass
seed=10 result=pass
seed=11 result=pass
seed=12 result=fail
seed=13 result=pass
seed=14 result=pass
seed=15 result=fail
failing seeds: 0 5 6 12 15; seeds 0 and 1 repeated their first output byte for byte

=== Save and replay the failing schedule ===
hermit: virtual-time epoch=... source=host-now; reproduce with --epoch=...
hermit: virtual-time epoch=... source=recording; reproduce with --epoch=...
Seed 1 passes without the file. Replayed under seed 1, the file reproduced the
recording's failure with identical output, starting the virtual clock at the
recording's instant (...):
Final value: 1
Antagonistic schedule reached, failing.

=== Demo 3: Chaos Concurrency Testing: SUCCESS ===
```

The `Using` line shows your build's date, commit, and path (the path is left
out here).

## What to notice

- `hello_race` starts two threads that each store a different value into one
  shared variable: the first thread stores 1 (or 11), the second 2 (or 22). If
  the first thread's store lands last, the program prints
  `Antagonistic schedule reached, failing.` and exits with status 1.
- Seed 0 fails and seed 1 passes on every run. A failure you found with a seed
  is a failure you can hand to someone else as one command.
- The survey shows how often the failing order appears: 5 of the 16 seeds
  (0, 5, 6, 12, and 15) fail. The script checks that the failing seeds are
  exactly these five and stops with an error otherwise. It also checks that
  seeds 0 and 1 print byte for byte what they printed in the first step.
- Every run must end in one of the program's two outcomes: exit status 0 with
  `Did not find antagonistic schedule. Succeeding.`, or exit status 1 with
  `Antagonistic schedule reached, failing.`. Any other exit status, or a run
  that prints nothing, stops the demo, so a Hermit error cannot pass for the
  expected failure.
- The last step records seed 0's failing run with
  `--record-preemptions-to=FILE` and replays FILE with
  `--replay-preemptions-from=FILE` under seed 1. It requires both runs to reach
  the failing outcome and checks with `cmp` that the two outputs are identical.
  Seed 1 passes without the file (first step), so the failing replay shows
  that the file, not the seed, chose the failing order. The schedule file is
  kept under `target/demos/` (for this run,
  `target/demos/hermit-demo.<random>/hello-race-schedule.json`, 187 KB listing
  1,839 scheduling events). The file also stores the virtual clock's starting
  instant, and the replay reuses it: the script checks that the second
  `hermit: virtual-time` line says `source=recording` and shows the same
  instant as the first, and stops with an error otherwise. For these two runs
  the script unsets `HERMIT_EPOCH`, which would give both runs that instant, so
  the replay would not take it from the file, and `HERMIT_LOG_FILE`, which
  would move that line from standard error into the log file.

## How it works

Hermit runs all threads of the program one at a time. Each time a thread makes a
system call or executes an instruction that Hermit intercepts (here, the
`rdtsc` time-stamp reads in the program's work loops), Hermit decides which
thread runs next. In chaos mode it makes those choices randomly, from a
generator seeded with `--seed`, and it can also stretch or shorten how long a
thread runs before it is switched out. Because every other input is already
deterministic (see [demo 1](../01-deterministic-run/README.md)), the seed alone
determines the order of events, so a seed that fails once fails every time. The
result does not depend on the virtual clock's starting instant: seeds 0, 1, and
5 give the same result with `--epoch=2026-01-01T00:00:00Z` and with
`--epoch=2031-07-15T12:34:56Z`. `--max-timeslice=disabled` turns off
preemption by hardware performance counters, so thread switches happen only at
those intercepted events and this demo does not need performance counters.
