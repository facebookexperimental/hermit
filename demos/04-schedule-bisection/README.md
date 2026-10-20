# Demo 4: Schedule bisection

Knowing that some seed fails is useful; knowing which two operations race is
better. `hermit analyze` searches for a passing and a failing thread schedule of
the same program, then bisects the difference between them until it finds the
one pair of adjacent events, on different threads, whose order decides the
outcome. It reports those two events with their stack traces. It runs the
program many times: on a 316-thread AMD EPYC host the whole demo took about 5
seconds in the default mode and about 22 seconds with
`ANALYZE_MAX_TIMESLICE=400000` (measured 2026-09-30 with Hermit commit
`dc92644f96f4`). The script gives up after 10 minutes.

## Prerequisites

- `hermit` on your `PATH`, built from this checkout (see
  [Setup](../README.md#setup)).
- `cargo` and a C compiler. The demo builds `hello_race` (the same program as
  [demo 3](../03-chaos-concurrency/README.md)) as a debug build, so the report
  can name source lines.
- `python3` to print the report summary, and `timeout` from coreutils.

## Run it

From the repository root:

```bash
demos/04-schedule-bisection/run.sh
```

The same demo runs with `make -C demos demo4`. Set `DEMO_VERBOSE=1` to see the
full, unfiltered `hermit analyze` output.

To run the analysis yourself:

```bash
cargo build --locked -p hermetic_infra_hermit_flaky-tests --bin hello_race
hermit --log=error analyze --run-arg=--base-env=host --run-arg=--no-virtualize-cpuid --report-file=/tmp/hello-race-analysis.json --analyze-seed=0 --search -- --chaos --summary --max-timeslice=disabled -- target/debug/hello_race
```

The options after `--search --` are the `hermit run` options used for each
trial run; each `--run-arg=` adds one more. Drop
`--run-arg=--no-virtualize-cpuid` on a host whose CPU can trap `CPUID`.
Run directly, `hermit analyze` prints all of its diagnostics (about 850 lines
here), including many `WARN ... Expected match before pop` lines from the
spliced trial schedules; those are part of the search, not a failure. The
report comes last and is also written to `/tmp/hello-race-analysis.json`. On
the test host, with `--no-virtualize-cpuid`, it named events 182 and 183 in
about 4 seconds, the same on two runs.

## What you will see

Captured from `demos/04-schedule-bisection/run.sh` on 2026-09-30 with Hermit
commit `dc92644f96f4`, on a host with CPUID faulting (so the script did not add
`--no-virtualize-cpuid`). The introduction, the `Using hermit ...` line, and
`cargo`'s build lines come first and are left out here.

```text
=== Search and bisect schedules (gives up after 10 minutes) ===
:: Event-Level Search Pass 0 => EditDistance = 697991, Swap Distance = 697975 (100% matched, midpoint sched len = 1861)
:: Event-Level Search Pass 1 => EditDistance = 348996, Swap Distance = 348988 (100% matched, midpoint sched len = 1861)
...
:: Event-Level Search Pass 18 => EditDistance = 2, Swap Distance = 2 (100% matched, midpoint sched len = 1861)
:: Event-Level Search Pass 19 => EditDistance = 1, Swap Distance = 1 (100% matched, midpoint sched len = 1861)
:: Critical events found which exercise race bug.
Critical event index 213
:: Completed analysis successfully.

------------------------------ hermit analyze report ------------------------------
These two operations, on different threads, are RACING with eachother.
The order of events (#212 and #213 in the schedule) determines the program outcome.
You must add synchronization to prevent these operations from racing, or give them a different order.
Attached are the stacktraces from a PASSING run, but flipping the order of the two events makes the program FAIL (nonzero exit).

Stack trace for thread 5 ("hello_race"):
   0: 0x007ffff7d08db6: mmap64 + 0x25
   1: 0x007ffff7c977b7: new_heap + 0xa6
...

Stack trace for thread 7 ("hello_race"):
   0: 0x007ffff7d08db6: mmap64 + 0x25
   1: 0x0055555559f9e4: std::sys::pal::unix::stack_overflow::imp::make_handler + 0x143
...

Execution context for the critical events (*):
    (tid5 end=0x7ffff7c8c830 Syscall(sched_getaffinity, Posthook))
...
  * (tid5 end=0x7ffff7d08db6 Syscall(mmap, Prehook))
  * (tid7 end=0x7ffff7d08db6 Syscall(mmap, Prehook))
...

=== Report the two critical adjacent events ===
These two operations, on different threads, are RACING with eachother.
The order of events (#212 and #213 in the schedule) determines the program outcome.
You must add synchronization to prevent these operations from racing, or give them a different order.
Attached are the stacktraces from a PASSING run, but flipping the order of the two events makes the program FAIL (nonzero exit).

critical events: 212 213

Event numbers can vary with the binary and Hermit revision. The default
localizes the race to the order of two system calls; on a host with hardware
performance counters, ANALYZE_MAX_TIMESLICE=400000 localizes it more finely.

=== Demo 4: Schedule Bisection: SUCCESS ===
```

Search passes 2 to 17, the rest of both stack traces, and the rest of the
execution context are elided (`...`). Addresses, event numbers, and pass counts
depend on the binary, the C library, and the Hermit revision; your numbers may
differ. Six runs on the test host produced byte-identical JSON reports, and
two runs with `ANALYZE_MAX_TIMESLICE=400000` produced a second, also
byte-identical, report (events 2880 and 2881, after 24 search passes).

## What to notice

- Each search pass line shows the two schedules getting closer: the edit
  distance and swap distance between the passing and failing schedules shrink
  until only one adjacent swap separates them.
- The report names two events on different threads. Running them in one order
  passes and in the other order fails. In the default mode Hermit switches
  threads only at system calls, so the pair is two system calls: above, two
  `mmap` calls the threads make while starting up, and the stack traces show
  thread start-up code rather than lines of `hello_race`.
- With `ANALYZE_MAX_TIMESLICE=400000` the pair moves into the program itself:
  the stack traces point at lines 56 and 73 of `flaky-tests/hello_race.rs`,
  the `do_work` calls right after the two racing `var.store` writes.
- The full report, including both stack traces, is saved as JSON under
  `target/demos/`.

## How it works

Every run under Hermit is a deterministic function of its inputs and its thread
schedule, and Hermit can record a schedule and replay it exactly (see
[demo 3](../03-chaos-concurrency/README.md)). `hermit analyze` uses chaos mode
to find one schedule that passes and one that fails. It then builds schedules
that follow the passing one for a prefix and the failing one after it, and
bisects on the length of that prefix, replaying each candidate to see whether it
passes or fails. The search ends at two adjacent events whose order flips the
result.

By default the demo switches threads only at system calls, which works on any
host. On a host with user-space access to hardware performance counters, set
`ANALYZE_MAX_TIMESLICE=400000` to also switch threads in the middle of
computation, which lets the analysis point at finer-grained source locations.
