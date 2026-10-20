# Demo 1: Deterministic run

Many values a program reads change from one run to the next: bytes from
`/dev/urandom`, the wall clock, Python's per-process hash seed, heap addresses,
and the order in which two processes write to the same terminal. Under
`hermit run`, each of these comes from a virtual source that depends only on the
program, its inputs, and a few settings you can pin on the command line, so
running the same command twice gives the same output. `hermit run --verify`
checks this for you by running the command twice and comparing the results.

## Prerequisites

- `hermit` on your `PATH`, built from this checkout (see
  [Setup](../README.md#setup)).
- `cargo` and a C compiler. The demo builds two small test programs from this
  checkout: `rustbin_heap_ptrs` and `hello_race`.
- `python3`.
- For the last step (`--verify`), user-space access to the CPU's hardware
  performance counters. Hermit uses them to interrupt threads at repeatable
  points. Most bare-metal Linux hosts allow this; many virtual machines and
  containers do not.

## Run it

From the repository root:

```bash
demos/01-deterministic-run/run.sh
```

The same demo runs with `make -C demos demo1`. Once the two test programs are
built, the demo takes about 11 seconds; the first run also compiles them (about
20 more seconds on the 2026-09-30 test host).

To try the individual steps yourself, run each command twice and compare:

```bash
hermit --log=error run --base-env=minimal --no-virtualize-cpuid -- /bin/sh -c 'od -An -N8 -tx1 /dev/urandom'
hermit --log=error run --base-env=minimal --no-virtualize-cpuid --epoch=2026-01-01T00:00:00Z -- /bin/date +%s.%N
hermit --log=info run --verify --no-virtualize-cpuid -- /bin/bash examples/race.sh
```

`--base-env=minimal` gives the program a small, fixed set of environment
variables instead of yours. `--no-virtualize-cpuid` lets the commands run on
CPUs that cannot trap the `CPUID` instruction. With it, the program sees the
host's real `CPUID` results on every host, CPU model and features included, so
`CPUID` becomes an input from the host and a different CPU can change what the
program does. `--epoch` sets the instant at which the virtual
clock starts (see [How it works](#how-it-works)).

Every `hermit run` prints one line on standard error that names the clock's
starting instant, for example:

```text
hermit: virtual-time epoch=2026-01-01T00:00:00+00:00 source=explicit; reproduce with --epoch=2026-01-01T00:00:00+00:00
```

Without `--epoch` the line says `source=host-now` and shows the host time at
which the run started.

## What you will see

Output of `demos/01-deterministic-run/run.sh` on 2026-09-30, with Hermit built
from commit `dc92644f96f4`. Cargo's build lines are left out, as are the
starting instants on the `hermit: virtual-time epoch=` lines, which are the
host's clock (shown as `...`). The native Python values and native checksums
are different on every run.

```text
==========================================
===     Demo 1: Deterministic Run      ===
==========================================

Hermit preserves the guest exit status and output while making random bytes,
wall-clock time, Python hash seeding, and heap address layout stable across
runs. hermit run --verify runs the guest twice and compares exit status, output,
and Hermit's deterministic execution log. The guest must be idempotent: a first
run that changes a file, database, cache, or external service can legitimately
change the second run.

==========================================
Using hermit 0.2.0 (2026-09-30, gdc92644f96f4-dirty) (...)

=== Basic execution ===
hermit: virtual-time epoch=... source=host-now; reproduce with --epoch=...
hello

=== Virtual random bytes are stable across runs ===
hermit: virtual-time epoch=... source=host-now; reproduce with --epoch=...
 29 72 bb 04 4d 96 df 28
hermit: virtual-time epoch=... source=host-now; reproduce with --epoch=...
 29 72 bb 04 4d 96 df 28

=== Virtual wall-clock time is stable across runs ===
-- both runs use --epoch=2026-01-01T00:00:00Z --
hermit: virtual-time epoch=2026-01-01T00:00:00+00:00 source=explicit; reproduce with --epoch=2026-01-01T00:00:00+00:00
1767225600.002351680
hermit: virtual-time epoch=2026-01-01T00:00:00+00:00 source=explicit; reproduce with --epoch=2026-01-01T00:00:00+00:00
1767225600.002351680

=== Python entropy and hash ordering match under Hermit ===
-- native (normally differs) --
random=314598b20db7d94b1126b9db1b931c2a
hash=1110224776714749765
set=gamma,beta,epsilon,delta,alpha
random=095331f186705cb4bdc82fd496c2f680
hash=1590100102423033345
set=epsilon,delta,beta,alpha,gamma
-- hermit (matches exactly) --
hermit: virtual-time epoch=... source=host-now; reproduce with --epoch=...
random=1d68e174656284908af35beb75ac9099
hash=8910831638551545767
set=gamma,epsilon,alpha,delta,beta
hermit: virtual-time epoch=... source=host-now; reproduce with --epoch=...
random=1d68e174656284908af35beb75ac9099
hash=8910831638551545767
set=gamma,epsilon,alpha,delta,beta

=== Address layout is stable across runs ===
hermit: virtual-time epoch=... source=host-now; reproduce with --epoch=...
alloc 1:          0x00005555555aed60
alloc 10:         0x00005555555aed80
alloc 100:        0x00005555555aeda0
alloc 1000:       0x00005555555aee10
alloc 10,000:     0x00005555555af200
alloc 100,000:    0x00005555555b1920
alloc 1,000,000:  0x00007ffff7e9e010
alloc 10,000,000: 0x00007ffff7276010
hermit: virtual-time epoch=... source=host-now; reproduce with --epoch=...
alloc 1:          0x00005555555aed60
alloc 10:         0x00005555555aed80
alloc 100:        0x00005555555aeda0
alloc 1000:       0x00005555555aee10
alloc 10,000:     0x00005555555af200
alloc 100,000:    0x00005555555b1920
alloc 1,000,000:  0x00007ffff7e9e010
alloc 10,000,000: 0x00007ffff7276010

=== Built-in --verify determinizes a racy multi-process guest ===
-- native race: output interleaving differs each run (checksum of output) --
native run 1: cksum=3769752461
native run 2: cksum=3107637923
-- hermit --verify (identical output + verified execution log) --
hermit: virtual-time epoch=... source=host-now; reproduce with --epoch=...
:: Run1...
:: Run2...
:: Comparing captured verification logs...
Logs contain 3828 | 3828 messages total
Logs contain 3826 | 3826 detcore-specific messages
Logs contain 3828 | 3828 INFO messages
Logs contain 3778 | 3778 DETLOG & scheduler COMMIT messages
Normalizing known nondeterministic numerical data before comparison...
  Comparing DETLOG messages...

Done processing logs, no substantive differences found (3778 | 3778 DETLOG messages compared).
Logs contain 1 | 1 scheduler empty-run-queue kick messages
Logs contain 0 | 0 scheduler COMMIT records reading /proc/self/maps
bababababababababa...bababa


:: comparison=Stripped relaxations=unsafe-numeric-address-and-path-normalization/v1
:: Success: deterministic. Determinism verified.

=== Demo 1: Deterministic Run: SUCCESS ===
```

The `Using` line shows your build's date, commit, and path (the path is left
out here). The `bababa...`
line is 400 characters long and is shortened here. The message counts in the
`--verify` step are the same on both sides of each `|`; the counts themselves
depend on the directory you run from and on your environment, because this
step does not use `--base-env=minimal` (for example, `make -C demos demo1`
runs from `demos/` and reports 3830 messages).

## What to notice

- The random bytes, the Python lines, and the heap addresses are identical
  between the two Hermit runs, and the demo checks each pair with `cmp`. They
  are also the same on every later run of the demo.
- The two native Python runs print different `random=` bytes, and usually a
  different `hash=` value and `set=` order.
- With the same `--epoch`, `date` prints the same time in both runs, down to
  the nanosecond: 2.351680 milliseconds of virtual time after the epoch. Leave
  out `--epoch` and each run starts its clock at the host's current time, so the
  printed times differ by the real time between the runs.
- The heap addresses printed by `rustbin_heap_ptrs` are identical across Hermit
  runs, including the large allocations that the allocator serves with `mmap`.
- `examples/race.sh` starts two shells that each print 200 `a` or `b`
  characters at the same time. Natively the interleaving, and so the checksum,
  varies. Under `--verify`, Hermit runs the script twice and compares the exit
  status, the output, and its own log of scheduling decisions. The two shells
  take strict turns, so the output is `baba...` in the same order every time.

## How it works

Hermit runs the program under `ptrace` and intercepts its system calls. Calls
that read time or randomness are answered from virtual sources. The clock
starts at an epoch and then advances with the program's own progress rather
than with real time. `hermit run` takes the epoch from the host clock once, at
startup, unless you give `--epoch`, and prints it so that you can repeat a run
exactly. Random bytes come from a fixed seed (`--seed`, 0 unless you change
it), so they are the same whether or not you pin the clock. The program starts
with address-space randomization turned off, so with the same inputs and the
same order of events the kernel places memory at the same addresses on every
run. All threads and processes in the container run one at a time on a single
virtual CPU, in an order Hermit chooses deterministically, so a race between
two processes has the same winner on every run.

`--verify` runs the program twice with the same epoch and compares the two
runs. It needs `--log=info` because it compares Hermit's log of scheduling
decisions; at `--log=error` that log is empty and the comparison would prove
nothing.

Controls: `HERMIT_DEMO_MAX_TIMESLICE=disabled` turns off counter-based
preemption in the steps before `--verify`, for hosts without performance
counters (the Python step may then be slow). It does not reach the `--verify`
step, which keeps counter-based preemption because the race in `race.sh` is
only reliably deterministic with it, so that step still needs the counters.
`DEMO_SKIP_BUILD=1` reuses the already built test programs.
