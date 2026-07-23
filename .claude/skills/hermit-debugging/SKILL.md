---
name: hermit-debugging
description: "Debug hermit/detcore runs (nondeterminism, hangs, syscall gaps, scheduling) using hermit's built-in logging and log-diff FIRST, before reading source. Use whenever a guest program behaves unexpectedly, --verify fails, or a run hangs under hermit."
---

# Debugging Hermit Runs

**Thesis: reach for hermit's logs before you reach for the source.** Detcore
emits a rich, structured trace of every scheduling decision, syscall, and
virtual-time advance. Most "why did this diverge / hang / behave oddly?"
questions are answered by reading that trace or diffing two of them — *not* by
reading `scheduler.rs`. Read the code only once the log has told you *where* to
look.

All commands below assume the repo root and the release binary
`target/release/hermit` (use `target/debug/hermit` if that is what you built).
On Meta devservers prefix network-touching commands with `with-proxy`.

## 0. First move, always

```bash
# Separate hermit's log (stderr) from the guest's own output (stdout):
hermit --log info run -- <program> [args...] 2>/tmp/h.log
#   ^ global flag, BEFORE the subcommand.   guest stdout stays on your terminal
wc -l /tmp/h.log      # a trivial `echo hello` produces ~350 INFO lines
```

Do **not** interleave hermit logs into guest stdout. Either redirect stderr as
above, or use the dedicated flag:

```bash
hermit --log info --log-file /tmp/h.log run -- <program>
```

`--log-file` (env `HERMIT_LOG_FILE`) writes the trace to a file and leaves the
guest's stdout/stderr untouched — the cleanest way to keep the two streams
apart.

### Log levels (`-l/--log`, env `HERMIT_LOG`)

| Level | Use it for |
| --- | --- |
| `error` / `warn` | Quiet; only when you want the guest to run near-normally (used for QEMU boots). |
| `info` | **Default debugging level.** Every COMMIT (scheduling turn), every DETLOG syscall, every virtual-time advance. Start here. |
| `debug` | Adds `reverie_ptrace::task` events, per-step scheduler internals, `tracee` lines. ~2-3x the volume. Use when INFO isn't enough. |
| `trace` | Everything, including `[sched-step*]` micro-steps and quiescence waits. Very large; scope it (see per-target filtering). |

Per-target filtering uses `tracing`/`RUST_LOG` syntax, so you can crank up one
module without drowning in the rest:

```bash
HERMIT_LOG='info,detcore::scheduler=trace' hermit run -- <program> 2>/tmp/h.log
```

## 1. How to read the trace

Every line is `TIMESTAMP LEVEL target: message`. The `target` tells you the
subsystem: `detcore`, `detcore::scheduler`, `detcore::scheduler::runqueue`,
`detcore::syscalls::files`, `detcore::tool_global`, `detcore::tool_local`,
`reverie_ptrace::task`. Grep by target to isolate a subsystem.

The two message classes that matter most both live in the deterministic trace:

**COMMIT lines** — one per scheduler *turn*. This is the serialized schedule.

```
[sched-step5] >>>>>>>
 COMMIT turn 0, dettid 3 using resources {ParentContinue { parent: DetPid(3), child: DetPid(3) }: W}, on previously committed 1_640_995_199.000_000_000s
 COMMIT turn 1, dettid 3 using resources {MemAddrSpace(DetPid(3)): RW}, on previously committed 1_640_995_199.000_500_000s
```

Read it as: *turn N, thread `dettid`, acquired these resources (R/W), at this
committed virtual time.* The **sequence of `(turn, dettid)` pairs is the
schedule** — the single most important thing to diff between two runs.

**DETLOG lines** — deterministic facts: syscalls, their results, RNG seeds, etc.

```
DETLOG [syscall][detcore, dtid 3] inbound syscall: openat(-100, ... "/etc/ld.so.cache", OFlag(O_CLOEXEC)) = ?
DETLOG [syscall][detcore, dtid 3] finish syscall #3: openat(...) = Ok(3)
DETLOG SCHEDRAND: seeding scheduler runqueue with seed 0
DETLOG USER RAND: seeding PRNG for root thread with seed 0
```

`inbound syscall: ... = ?` is interception; `finish syscall #N: ... = Ok(..)` is
the sanitized result handed back to the guest. A syscall that appears inbound
but whose result looks like passthrough of a host value is a determinism
suspect.

**Virtual time (DetTime / LogicalTime).** Time in hermit is *logical*, not wall
clock. `detcore-model/src/time.rs` defines:

- `LogicalTime(u64)` — absolute nanoseconds since a fixed epoch
  (`starting_micros`, default `1640995199000000` = 2021-12-31T23:59:59). This is
  why guest timestamps are identical across runs.
- `DetTime { syscalls, rcbs, nondet_instrs, starting_micros, multiplier }` —
  virtual time is a deterministic *function of work done*, not of the host
  clock. It advances by counting **syscalls**, **RCBs** (retired conditional
  branches, from the PMU — the preemption clock), and **nondet_instrs**
  (`rdtsc`/`cpuid`).

You see it advance in the trace:

```
[dtid 3] inbound rdtsc, new logical time: DetTime { syscalls: 1, rcbs: 49, nondet_instrs: 1, starting_micros: 1640995199000000, multiplier: 1.0 }
```

and summarized at shutdown:

```
Internally, the hermit scheduler ran 34 turns, recorded 0 events, replayed 0 events (0 desynced)
Final virtual global (cpu) time: 1_640_995_199.019_160_055s
```

If the RCB counts for the same logical point differ between two runs, the guest
executed a different number of branches — a real divergence, not a clock
artifact.

### Quick grep cookbook

```bash
grep ' COMMIT turn '            /tmp/h.log   # the schedule (turn, dettid) sequence
grep ' DETLOG '                 /tmp/h.log   # deterministic facts
grep 'inbound syscall'          /tmp/h.log   # syscalls intercepted, in order
grep 'new logical time'         /tmp/h.log   # virtual-time advances (DetTime)
grep -iE 'park|unpark|go-ahead|New thread|run queue|quiescen' /tmp/h.log  # thread lifecycle
grep -oE 'detcore[a-z_:]*'      /tmp/h.log | sort | uniq -c | sort -rn    # subsystem histogram
```

## 2. Finding a nondeterminism / divergence point

When `hermit run --strict --verify` reports "nondeterministic", hermit already
ran twice and compared the deterministic trace. To localize the divergence
yourself, capture two runs and use the **built-in log differ** — it is far
smarter than plain `diff` because it normalizes known-nondeterministic noise
(hex pointers, tmp paths, `/proc/<pid>/`, elapsed-time fields).

```bash
hermit --log info run -- <program> 2>/tmp/a.log
hermit --log info run -- <program> 2>/tmp/b.log
hermit log-diff /tmp/a.log /tmp/b.log        # compares COMMIT + DETLOG only
```

Useful `log-diff` flags (`detcore/src/logdiff.rs`):

| Flag | Effect |
| --- | --- |
| `--strip-lines` | Normalize numbers and tmp paths before comparing — tolerates limited nondeterminism to find the *structural* divergence. |
| `--syscall-history <N>` | Print the N completed syscalls *before* each divergence — the context that tells you what led up to it. |
| `--ignore-lines <substr>` | Drop lines containing a substring before comparing (repeatable). |
| `--skip-commit` / `--skip-detlog` | Compare only DETLOG, or only COMMIT, to tell a *scheduling* divergence from a *syscall/data* divergence. |
| `--include-detlogs syscall,syscallresult,other` | Narrow which DETLOG classes count. |
| `--limit 0` | Don't elide after 20 diffs; show all. |

**Interpretation:** the *first* divergence is the one that matters; everything
after it is downstream noise. If the first diff is a **COMMIT** line
(`(turn, dettid)` differs), the *schedule* diverged — a thread-interleaving
problem. If COMMITs match but a **DETLOG** line differs, the schedule is stable
but a syscall returned different data — an unvirtualized source.

## 3. Common root causes, and their log signatures

| Symptom in the log | Likely cause | Where to look |
| --- | --- | --- |
| First diff is a `COMMIT` line — different `(turn, dettid)` order between runs | **Thread-interleaving nondeterminism.** Often from `--no-sequentialize-threads`, or a futex/blocking-IO race. | `detcore/src/scheduler.rs`; check for relaxation flags. |
| DETLOG syscall result differs; value looks like a live host reading (time, meminfo, rand) | **Unvirtualized time / entropy source** falling through to the host. | `detcore/src/time.rs`, the relevant `detcore/src/syscalls/` handler. |
| `WARN`/`ERROR` "unsupported syscall" or a syscall returning `ENOSYS` unexpectedly | **Unhandled syscall falling through.** Add `--panic-on-unsupported-syscalls` to make it fatal + get a backtrace. | `detcore/src/syscalls/`. |
| `cpuid` in the trace and behavior varies by host | **CPUID leaking real hardware.** Try `--no-virtualize-cpuid` to confirm it's CPUID-related; the host may lack CPUID faulting. | `detcore/src/cpuid.rs`. |
| Run *hangs* with no forward progress; last lines are `[sched-step*]` / quiescence waits | Scheduler waiting on a wakeup that never causally pairs (e.g. FIFO open rendezvous), **or** a long syscall-free loop being precise-preemption single-stepped (slow, not hung). | `detcore/src/scheduler.rs`; try `--debug-futex-mode polling`. |
| `--verify` aborts before run 2 | Run 1 exited via a **signal** (verify needs a clean exit to compare two runs). | Use plain `--strict` x3 instead. |

## 4. Debugging-specific CLI flags

Global (before the subcommand): `--log`, `--log-file`, `--backend <ptrace|dbi|kvm>`.

On `run` (see `hermit run --help`), the internal/debug flags:

- `--panic-on-unsupported-syscalls` — turn a silent fallthrough into a fatal
  error with a backtrace (debugging detcore itself; do not use in production).
- `--stacktrace-event <index[,path]>` — print the guest stack at a given
  schedule event; pairs with record/replay.
- `--preemption-stacktrace[-log-file <f>]` — dump a stack at each preemption
  (chaos mode).
- `--debug-futex-mode <precise|polling|external>` — switch the futex model when
  diagnosing a futex-related hang.
- `--debug-externalize-sockets` — treat all sockets as external/nondeterministic
  to isolate socket-driven nondeterminism.
- `--detlog-heap` / `--detlog-stack` — log hashes of heap/stack maps for
  memory-determinism (L3) checking.
- `--stop-after-turn <N>` / `--stop-after-iter <N>` — halt after a scheduler
  turn/loop iteration (requires `--sequentialize-threads`) to bisect a schedule.
- `--imprecise-timers` / RCB-count knobs — change how logical time is derived
  when the PMU is unavailable or noisy.
- `--gdbserver` — start a gdbserver for remote debugging.

Higher-level analysis subcommands: `hermit log-diff` (above),
`hermit analyze` (analyze passing vs failing runs), and `hermit bisect`
(`--good <schedule> --bad <schedule>` to localize a race between two recorded
schedules).

## 5. Assurance ladder (name the level you reached)

Per `AGENTS.md`, never say "works". State the level, backend, log level, and
relaxations:

- **L1** deterministic: `hermit run --strict` completes.
- **L2** bitwise-identical repeat: `hermit run --strict --verify`.
- **L3** memory determinism: add `--detlog-heap --detlog-stack`.
- **L4** stress-hardened: L2/L3 repeated ~20x with no divergence.

Example of a correct report: "passes at L2 (ptrace backend, `--log` default,
relaxations: none)".

## 6. Source-code map (read *after* the log points you here)

- `detcore/src/scheduler.rs` — the sched loop; `[scheduler]`, `[sched-step*]`,
  and COMMIT emission (`info!`/`debug!`/`trace!`). The COMMIT point is step 4.
- `detcore/src/logdiff.rs` — the log comparator: `strip_log_entry`
  normalization, `is_commit`/`is_detlog`, `LogComparisonMode`, `LogDiffOpts`.
- `detcore-model/src/time.rs` — `LogicalTime`, `DetTime`, `GlobalTime`, and the
  RCB↔nanosecond conversions.
- `detcore/src/syscalls/` — per-syscall handlers (`files.rs`, etc.).
- `detcore/src/cpuid.rs`, `detcore/src/time.rs` — CPUID and time virtualization.
- `detcore/src/tool_local.rs` / `tool_global.rs` — per-task events vs shared
  deterministic state (they talk over RPC).
- `docs/Developers/Architecture.md` — architecture overview.
