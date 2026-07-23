# Ruby thread-scheduling determinism

Date: 2026-07-22

Runtime coverage for the Ruby interpreter's thread scheduler.

- **Ruby version:** `ruby 3.0.7p220 (2024-04-23 revision 724a071175) [x86_64-linux]`
  (system package). The host's RubyGems install is broken
  (`did_you_mean`/`RbConfig` `NameError` in `gem_prelude`), so the programs are
  run with `ruby --disable-gems`; the test uses no gems.
- **Program under test:** `thread_order.rb` (minimal repro: `mini.rb`).
- **Runner:** `run.sh` (dual assertion). Regenerates `results.csv`.

## Program

`thread_order.rb` spawns N threads that each do an equal amount of CPU work,
`Thread.pass`, then print their id. The order the lines appear in depends on how
the Ruby VM interleaves the threads (GVL hand-off + OS scheduling).

## Dual assertion

1. **Native runs must DIFFER** — nondeterminism from thread scheduling.
2. **`hermit run --strict` runs must be IDENTICAL** — determinism achieved.

## Result

| Mode | Outcome |
| --- | --- |
| native (5×, 24 threads) | **nondeterministic** — 4 distinct outputs / 5 runs (ASSERT-1 PASS) |
| `hermit run` (`--strict`, default) | **DEADLOCK / TIMEOUT** — does not complete (ASSERT-2 blocked) |
| `hermit run --no-sequentialize-threads --no-deterministic-io` | completes, exit 0 (control) |

Assertion 1 holds. **Assertion 2 cannot be satisfied today: Ruby 3.0
multithreading livelocks under hermit's strict (sequentialized) scheduler.**

- 1 Ruby thread completes under `--strict` in ~1s.
- 2+ Ruby threads never complete (observed >60s, no output), with or without
  `--preemption-timeout=disabled`.
- The same program completes instantly under hermit **non-strict**, isolating the
  problem to the sequentialized deterministic scheduler (not ptrace/exec).

### Root cause

A `detcore=debug` trace of the 2-thread case shows a worker thread spinning
forever on a non-blocking read of Ruby's internal thread-wakeup pipe:

```text
[syscall][detcore, dtid 3] inbound syscall: read(3, 0x7fffffffa790, 8) = ?
[syscall][detcore, dtid 3] finish syscall #217: read(3, ...) = Err(Errno(EAGAIN))
```

Ruby's VM hands the GVL between threads using a self-pipe/eventfd: a waiting
thread blocks reading an 8-byte token that the running thread writes on hand-off.
Under sequentialized deterministic scheduling the reader is turned into a
deterministic non-blocking poll, but the counterpart thread that would write the
wakeup token is never scheduled, so the reader livelocks on `read = EAGAIN`. This
is the same *internal pipe-based blocking* class of issue that shared-futex work
has been chipping away at, now surfaced through Ruby's threading runtime.

## How to run

```bash
# from the repo root, with a release build at target/release/hermit
./experiments/ruby-threads/run.sh
# knobs: NTHREADS=24 NRUNS=5 STRICT_TIMEOUT=60 RUBY=$(command -v ruby) HERMIT=...
```

## Status / follow-up

This is Ruby runtime coverage **and** a determinism-scheduler bug report: to make
`hermit --strict` deterministic for Ruby, the scheduler needs to make progress on
Ruby's self-pipe GVL hand-off (schedule the pipe writer when a reader is blocked
on that pipe), rather than livelocking the reader's non-blocking poll.
