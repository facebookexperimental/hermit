# Concurrency stress suite

`concurrency.rs` is a parameterized guest for Hermit chaos testing. It covers:

- lost atomic updates;
- incorrect publication ordering;
- producer/consumer completion races;
- missing barrier synchronization;
- lost condition-variable wakeups;
- mutex correctness under contention;
- bounded `RwLock` writer fairness; and
- the Relaxed-atomic store-buffer litmus.

The guest exits with status 1 when it observes the target race and status 0 when
the invariant holds. The store-buffer category documents a current limitation:
Hermit serializes guest threads and therefore does not explore weak-memory
outcomes that can occur during native parallel execution.

The integration harness is `hermit-cli/tests/stress_suite.rs`. Its fast tier
runs ten fixed chaos seeds with the random scheduling heuristic at 2, 4, 8, and
16 threads. The slow tier runs 100 seeds for the sparse 16-thread
producer/consumer and condition-variable races. A separate PMU-dependent tier
searches the existing CAS handoff race with imprecise timers, records a failing
preemption schedule, and requires a precise replay of that failure.

Run the tiers with:

```bash
cargo test -p hermit --test stress_suite fast_chaos_matrix -- --ignored --exact
cargo test -p hermit --test stress_suite slow_race_matrix -- --ignored --exact
cargo test -p hermit --test stress_suite slow_cas_search_and_replay -- --ignored --exact
```

## Default scheduling fairness

`scheduling_fairness.rs` measures the non-chaos scheduler with four runnable
threads. It records the largest number of other worker progress events between
turns, completion of a bounded producer/consumer queue, and read acquisitions
while a writer is waiting on an `RwLock`. The integration test runs each fixed
workload five times and requires identical metrics:

```bash
cargo test -p hermit --test thread_scheduling_fairness -- --test-threads=1
```

The current default-scheduler baseline is:

| Workload | Five-run result |
| --- | --- |
| Four counters | `64,64,64,64` turns; maximum gaps `3,3,3,3` |
| Bounded buffer | 256 produced and consumed; maximum consumer streak 1 |
| Reader/writer lock | 32 writes, 93 reads; 0 reads while the writer waited |

The measurements cover fairness at intercepted synchronization and
`sched_yield` boundaries. They do not replace PMU preemption for a CPU-bound
thread that never enters the kernel. The test asserts the documented bounds
rather than the exact total reader count so it remains portable across
standard-library lock implementations.
