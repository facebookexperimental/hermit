---
name: deadlock-debugging
description: "Debug Hermit hangs, deadlocks, futex stalls, timed-wait failures, external-I/O stalls, and scheduler no-progress conditions from logs first. Use whenever a guest stops making progress under hermit, times out only under strict mode, or leaves threads blocked after a wait or wake."
---

# Debugging Hermit Deadlocks

Start with Hermit's logs, not `scheduler.rs`. A deadlock report is useful only
after you can name the last productive scheduler turn, every blocked thread,
and the event that should make one of them runnable.

This playbook specializes
`.llms/skills/hermit-debugging/SKILL.md`. Use that broader skill for log-diff,
nondeterministic output, and assurance levels. Use this document when the main
symptom is no forward progress.

All commands assume the Hermit repository root and the ptrace backend unless
stated otherwise. Replace `COMMAND [ARGS...]` with the guest invocation.

## 1. Capture a bounded first run

Never let a suspected hang run without a deadline. Keep Hermit's stderr log
separate from guest stdout, and preserve the exit code from `timeout`.

```bash
timeout --signal=TERM --kill-after=5s 30s \
  target/release/hermit --log info run --strict -- COMMAND [ARGS...] \
  > /tmp/guest.out 2> /tmp/hermit-info.log
status=$?
printf 'exit=%s\n' "$status"
wc -l /tmp/hermit-info.log
tail -n 80 /tmp/hermit-info.log
```

Exit `124` means the outer timeout fired; it does not identify the cause. Check
for orphaned guest or Hermit processes before rerunning:

```bash
pgrep -af 'target/(debug|release)/hermit|COMMAND'
```

Run the workload natively with the same bound. If native execution also hangs,
debug the guest first. If only Hermit hangs, continue here.

```bash
timeout --signal=TERM --kill-after=5s 30s COMMAND [ARGS...] \
  > /tmp/native.out 2> /tmp/native.err
printf 'native exit=%s\n' "$?"
```

Do not call a timeout a determinism pass. Record backend, log level,
relaxations, timeout duration, and the last observed progress in the report.

## 2. Choose the log level deliberately

`--log` is a global option and goes before `run`.

| Level | What it reveals | Use |
| --- | --- | --- |
| `info` | `DETLOG` syscall facts, scheduler `COMMIT` turns, virtual-time jumps, explicit deadlock panic | Always start here; it usually identifies the last productive turn. |
| `debug` | Step 3 queue length/current turn, `NONCOMMIT` decisions, deadlock avoidance, scheduler internals | Use when INFO shows where progress stopped but not why a thread could not run. |
| `trace` | Full queue/`next_turns`, futex actions and IDs, wait/wake transitions, quiescence waits, external-I/O spins | Use on the smallest reproduction; full TRACE logs are large. |

Prefer target-specific TRACE over global TRACE:

```bash
RUST_LOG='detcore::scheduler=trace,detcore::syscalls::threads=trace,detcore::tool_global=trace' \
timeout --signal=TERM --kill-after=5s 30s \
  target/release/hermit --log info run --strict -- COMMAND [ARGS...] \
  > /tmp/guest.out 2> /tmp/hermit-live.log
```

If `--log-file` writes inside a container namespace on the current setup,
redirect host stderr as above so the file remains available after guest exit.

## 3. Read scheduler progress

Three message classes define the liveness story.

### `COMMIT` is committed scheduler history

```text
DETLOG SCHEDRAND: seeding scheduler runqueue with seed 0
[sched-step3] Stepping scheduler, queue len 2, current turn 20, committed_time 1_640_995_199.508_831_230s
COMMIT turn 21, dettid 5 using resources {FutexWait: R}, on previously committed 1_640_995_199.508_831_230s
```

Read a commit as: scheduler turn, deterministic thread ID (`dettid`), granted
resources, and committed virtual time. The last changing `COMMIT turn` is the
last scheduler progress. A COMMIT proves a scheduler grant, not guest-visible
completion; `InternalIOPolling` can commit repeatedly without completing the
blocked syscall.

```bash
rg -n ' COMMIT turn ' /tmp/hermit-live.log | tail -n 40
rg -c ' COMMIT turn ' /tmp/hermit-live.log
rg -o 'COMMIT turn [0-9]+, dettid [0-9]+' /tmp/hermit-live.log | tail -n 40
```

### `NONCOMMIT` explains why a candidate did not run

```text
NONCOMMIT turn 9, SKIP dettid 3 which wanted resource SleepUntil(LogicalTime(1640995199506270050)) (blocking)
NONCOMMIT turn 20, DEFER dettid 3 after yield
```

`SKIP ... (blocking)` means the thread must later be restored by a wake,
timeout, signal, or I/O completion. `DEFER` means another runnable thread should
be selected. Repeated NONCOMMITs at a fixed turn with no later COMMIT are a
no-progress signature.

```bash
rg -n 'NONCOMMIT|SKIP dettid|DEFER dettid|changed priority' \
  /tmp/hermit-live.log | tail -n 80
```

### `SCHEDRAND` explains randomized choices

```text
DETLOG SCHEDRAND: seeding scheduler runqueue with seed 0
DETLOG SCHEDRAND: [0,3) => 1
```

The seed line appears when the run queue is created. Range-selection lines
matter under random or sticky-random scheduling.

```bash
rg -n 'SCHEDRAND|CHAOSRAND|fuzz-futexes' /tmp/hermit-live.log
```

## 4. Build a thread-state table

At TRACE, classify every `dettid` seen near the end. Hermit tracks runnable,
futex-blocked, timed, and external-I/O-blocked threads separately.

| Evidence | State | Required next event |
| --- | --- | --- |
| In `[sched-step3] queue ...` or selected for COMMIT | Runnable or tentatively runnable | Scheduler grants or rejects its request. |
| `Waiter blocking on futex ...` | Precise futex-blocked | Matching wake mask/identity, timeout, or signal. |
| `NONCOMMIT ... SleepUntil(...) (blocking)` or `Timed events:` | Timed-blocked | `step2d` advances time or normal virtual time reaches the deadline. |
| `io-blocked {...}` or `external IO ... SPINNING` | External-I/O-blocked | Host operation completes and checks in with `BlockedExternalContinue`. |
| `waiting for next thread ... to park` or repeated quiescence wait | Outside the scheduler/check-in gap | Thread reaches a Hermit event and fills its request IVar. |
| `thread deregistered, removed from sched structures` | Gone | No wake should target it. |

```bash
rg -n -i \
  'queue len|\[sched-step3\] queue|Waiter blocking|Timed events|SleepUntil|io-blocked|external IO|quiescen|to park|Woke one thread|thread deregistered' \
  /tmp/hermit-live.log | tail -n 200
```

Sample progress five times instead of judging one quiet instant:

```bash
for sample in 1 2 3 4 5; do
  printf 'sample=%s lines=' "$sample"
  wc -l < /tmp/hermit-live.log
  rg -c ' COMMIT turn ' /tmp/hermit-live.log || true
  sleep 2
done
```

If lines grow but COMMITs do not, classify the repeated message. External-I/O
polling and quiescence waits are distinct from a silent guest busy loop.

## 5. Trace futex wait to wake

Use precise futex mode first; it is the default strict model. Search for both
raw syscall interception and modeled actions:

The regex covers `FUTEX_WAIT`, `FUTEX_WAIT_BITSET`, `FUTEX_WAKE`, and
`FUTEX_WAKE_BITSET` spellings.

```bash
rg -n \
  'syscall=futex|inbound syscall: futex|FUTEX_(WAIT|WAKE)|Futex action:|Waiter blocking on futex|Waking up to|Woke one thread|Unblocked from futex_wait|FIZZLED|futex wait timed out' \
  /tmp/hermit-live.log
```

This complete chain was captured from `rustbin_futex_wait_child`. Addresses
and IVar pointers vary per run.

```text
[detcore, dtid 5] Futex action: WaitRequest(None)
[dtid 5] Waiter blocking on futex Private { mm: MmId { creator: DetPid(3), generation: 1 }, address: 93824992615776 }, now 1 waiters, on <ivar ...>
[detcore, dtid 3] Futex action: WakeRequest(1)
Waking up to 1 Futex waiters, out of 1 waiting.
[detcore] Woke one thread, dtid: 5, ... scheduled at position (p: 1000, t: 22)
COMMIT turn 21, dettid 5 using resources {FutexWait: R}, ...
[detcore, dtid 5] Unblocked from futex_wait! (<ivar ... Go(None)>)
```

For every final `WaitRequest`, find a wake for the same futex identity and
compatible bitset, a timed event followed by `Go(Some(TimeOut))`, or a signal.
The modeled identity is more useful than the raw pointer: private futexes
include `MmId`, and shared mappings need mapping identity.

Common findings:

- `Futex wake ... FIZZLED -- none waiting` followed by a permanent wait can be
  an application lost wake, value-update ordering bug, or wrong identity.
- A zero wake count is valid if no waiter matches. Check guest expectations.
- `FUTEX_WAIT_BITSET` has an absolute deadline; plain `FUTEX_WAIT` uses a
  relative timeout. `FUTEX_PRIVATE_FLAG` does not change that distinction.
- A timed waiter appears in both futex and timed pools. Wake removes its timed
  entry; timeout removes its futex entry.
- Wake and wait bitsets must intersect. Inspect raw args when addresses match
  but the waiter stays blocked.

Use alternate modes only for differential diagnosis:

```bash
timeout 30s target/release/hermit --log debug run --strict \
  --debug-futex-mode polling -- COMMAND [ARGS...] 2> /tmp/futex-polling.log

timeout 30s target/release/hermit --log debug run --strict \
  --debug-futex-mode external -- COMMAND [ARGS...] 2> /tmp/futex-external.log
```

If precise hangs but polling progresses, inspect registration, identity,
bitsets, timeout removal, and scheduler requeue. If only external progresses,
inspect sequentialization and host blocking. Polling/external results are
diagnostic relaxations, not strict determinism evidence.

## 6. Trace timers and `step2d`

```bash
rg -n -i \
  'timer_create|timer_settime|timer_gettime|timerfd_create|timerfd_settime|timerfd_gettime|SleepUntil|Timed events|Time-based event|Skipping global time|Alarm fired|futex wait timed out' \
  /tmp/hermit-live.log
```

Distinguish three mechanisms:

1. Scheduler timed waits (`nanosleep`, timed futexes, alarms) enter
   `timed_waiters` and can make a blocked thread runnable.
2. POSIX `timer_create`/`timer_settime` use deterministic IDs and virtual-clock
   deadlines, but current handlers explicitly say signal delivery is **not
   emulated**. A guest waiting only for that signal can stall after arming.
3. `timerfd_create` is tracked as `FdType::Timerfd`;
   `timerfd_settime`/`timerfd_gettime` are serialized through record/replay.
   Trace the later `read`/`poll`/`epoll_wait`, not only setup.

Current POSIX timer message templates expose the limitation:

```text
DETLOG [dtid 3] timer_create(clockid=...) => deterministic timer id 0 (arming tracked; signal delivery not emulated)
DETLOG [dtid 3] timer_settime(id=0, interval_ns=0, value_ns=1000000) armed against virtual clock (not delivered)
```

### What `step2d` should do

When the run queue is empty but a timed event exists,
`step2d_handle_empty_queue` jumps global logical time to the earliest event,
wakes/fires it, and restarts scheduler selection.

This sequence was captured from `rustbin_futex_timeout`:

```text
[detcore, dtid 3] Futex action: WaitRequest(Some(LogicalTime(1640995199015849030)))
[scheduler] Deadlock avoidance! Empty run-queue, so waking next timed event.
[scheduler] Skipping global time ahead to 1_640_995_199.015_849_030s.
[sched-step2] Time-based event on thread 3 (...) - futex wait timed out!
COMMIT turn 10, dettid 3 using resources {FutexWait: R}, ...
[detcore, dtid 3] Unblocked from futex_wait! (<ivar ... Go(Some(TimeOut))>)
```

If a timed waiter exists but no time jump appears, check whether quiescence is
blocking step 2. If time jumps but its `dettid` never commits, inspect
`next_turns`, timeout removal, and requeue. If the deadline is far away, check
absolute/relative semantics, clock ID, `TIMER_ABSTIME`, and guest `timespec`.

Repeated `InternalIOPolling` turns with a finite timeout must still advance
virtual time. Retry-count-sensitive DETLOG/COMMIT lines are filtered during
verification, so confirm the deadline comparison eventually fires.

## 7. Recognize no-progress signatures

| Final pattern | Meaning | Next check |
| --- | --- | --- |
| `Deadlock detected: thread(s) waiting on futex, but no runnable threads left` | Futex waiters exist, with no runnable, timed, or external event. | Pair every wait with its expected wake; compare native. |
| `Deadlock avoidance!` then `Skipping global time ahead` | Not itself a deadlock; step2d advances to a timer. | Require a time-based wake and later COMMIT. |
| `zero threads left anywhere, fizzling` near exit | No runnable or blocked threads remain. | Check guest exit/lifecycle, not futex logic. |
| `external IO ... SPINNING` with fixed dtids | Only host-driven blocking remains. | Identify syscall/fd and compare native readiness. |
| Repeated `Scheduler wait for full quiescense, on <same ivar>` | A thread has not parked/checkpointed. | Find its last event; distinguish host block from busy loop. |
| Repeated `InternalIOPolling`, no syscall completion | Polling livelock/readiness never arrives. | Trace fd owner, producer, and deadline. |
| No log growth, high CPU | Busy loop or slow precise RCB single-stepping. | Check PMU access, CPU use, preemption logs. |
| No log growth, sleeping process | Kernel/external block or stuck RPC. | Inspect `strace`, thread stacks, last park message. |

The futex-only panic is high-confidence classification, not proof Hermit is at
fault. A guest can deadlock. Native progress with identical inputs is minimum
evidence for a Detcore bug.

## 8. Compare native `strace` with Hermit

```bash
rm -f /tmp/native.strace.*
timeout --signal=TERM --kill-after=5s 30s \
  strace -ff -tt -T -yy -s 256 -o /tmp/native.strace \
  COMMAND [ARGS...]

timeout --signal=TERM --kill-after=5s 30s \
  target/release/hermit --log info run --strace-only -- COMMAND [ARGS...] \
  > /tmp/hermit-strace.out 2> /tmp/hermit-strace.log

for file in /tmp/native.strace.*; do
  printf '\n== %s ==\n' "$file"
  tail -n 30 "$file"
done
rg -n 'inbound syscall|finish syscall' /tmp/hermit-strace.log | tail -n 100
rg -n 'inbound syscall|finish syscall' /tmp/hermit-live.log | tail -n 100
```

Find the first structural difference: a native wake/timer Hermit never reaches,
a thread Hermit parks before native completion, polling/external blocking with
no completion, a futex argument mismatch, or native timer delivery after
Hermit only arms it. `strace` changes timing and `--strace-only` disables
determinization; compare order, arguments, blocking, and completion, not
timestamps or PIDs.

## 9. Common Hermit-specific causes

1. **Futex model gap:** unsupported op, private/shared identity, untracked shared-mapping
   aliasing, bitset mismatch, stale timed waiter, or missed requeue.
2. **Timer interaction:** absolute/relative confusion, missing step2d jump,
   wrong timeout removal, or undelivered POSIX timer signal.
3. **Thread sequentialization:** a thread blocks in the host while owning the
   deterministic turn, preventing its producer from running.
4. **External I/O:** pipe/socket/child readiness depends on an actor outside
   the deterministic scheduler.
5. **Polling livelock:** retries continue but producer, fd mapping, or timeout
   never makes the operation ready.
6. **Lifecycle cleanup:** exited thread remains in `next_turns`, futex/timed
   pools, priorities, or parent/child state and prevents quiescence.
7. **Preemption environment:** PMU/RCB restriction or syscall-free loop makes
   progress extremely slow rather than cyclically blocked.

Use `--no-sequentialize-threads` only to test whether deterministic scheduling
is implicated. It weakens the guarantee and cannot be reported as L1/L2.

## 10. Bisect and report

Once the last productive turn is known, stop near it:

```bash
target/release/hermit --log trace run --strict \
  --stop-after-turn 123 -- COMMAND [ARGS...] 2> /tmp/turn-123.log
target/release/hermit --log trace run --strict \
  --stop-after-iter 500 -- COMMAND [ARGS...] 2> /tmp/iter-500.log
```

Both options require thread sequentialization. A useful report includes the
minimized command/timeout, native result and strace tail, Hermit commit/backend/
log level/relaxations, last COMMIT and `dettid`, final state of every thread,
futex identity/op/bitset and missing wake, timer deadline and step2d outcome,
external fd/syscall, alternate futex-mode result, and a focused excerpt.

## Source map after logs localize the fault

- `detcore/src/scheduler.rs`: blocked pools, scheduler steps, futex queues,
  `step2d_handle_empty_queue`, time jumps, summaries.
- `detcore/src/scheduler/runqueue.rs`: priorities, polling backoff,
  `SCHEDRAND`.
- `detcore/src/scheduler/timed_waiters.rs`: timed-event ordering/removal.
- `detcore/src/syscalls/threads.rs`: futex ops, timeout semantics, modes.
- `detcore/src/syscalls/time.rs`: nanosleep and POSIX timers.
- `detcore/src/syscalls/files.rs`: timerfd and notification-fd handling.
- `detcore/src/tool_global.rs`: futex RPCs and blocked response IVars.
- `detcore/src/syscalls/helpers.rs`: nonblocking retry/polling.
- `docs/ARCHITECTURE.md`: scheduler, futex, internal/external blocking model.
- `docs/ERROR_CATALOG.md`: user-facing deadlock/environment signatures.
