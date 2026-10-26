# Checking Detcore's signal-state copy

Detcore can keep its own copy of every guest thread's signal dispositions and
blocked mask, and check it against the kernel. This is audit step A5
(https://github.com/rrnewton/hermit/issues/4485). It is on by default since
A5 step 2d. Turn it off with:

```bash
hermit run --no-signal-state-copy --strict --epoch=2026-01-01T00:00:00Z -- ./program
```

The environment variable `HERMIT_SIGNAL_STATE_CHECK`, which turned the copy
on while it was opt-in, is no longer read: `hermit run` refuses a run that
sets it, whatever the value.

A schedule recorded with `--record-preemptions`, or written by `hermit
analyze`, minimize, bisect or `hermit-verify interpolate`, states the setting
it ran under. Replaying it under the other setting (`--replay-preemptions-from`,
`--replay-schedule-from`, or as an analysis input) is refused. A schedule
written before the setting was stored states nothing and is replayed under
either. The setting is not the only input to the handler-return reports:
`--imprecise-timers`, `--gdbserver` and an inherited seccomp filter turn them
off too, so `hermit analyze --imprecise-search`, whose search runs without
them and whose precise replay runs with them, can differ in the timer cases
below. Record and replay (`hermit record`) run with the copy off; the copy
never asks for handler-return reports there, so its own decisions read what
`/proc` reads either way.

It runs only where the kernel's signal state for a guest thread is the
guest's own, which means the ptrace backend (and e9patch preprocessing,
which Hermit runs on ptrace), with serialized threads. Elsewhere, and
whenever it is turned off, nothing is recorded or checked, and runs are
exactly as they would be without it.

Where the copy is kept, some decisions read it in place of the kernel's
`/proc` view of the thread:

- a thread's own decisions that need only its dispositions (which signals it
  ignores and which it catches);
- whether a signal sent to another thread is discarded at the send, from the
  dispositions each process publishes to the scheduler;
- where the scheduler reads another thread's mask for one signal: whether a
  send discards it, which thread a process-directed signal the scheduler
  sends goes to (a child's SIGCHLD, a timer's signal) and whether every
  thread blocks it, which thread takes a SIGCHLD first, whether a real-time
  timer's expiration is kept, and whether a restartable poll admits the
  signal. Each thread publishes its mask, except inside a temporary-mask
  wait or after one was interrupted while its mask is still to be put back,
  before a reported handler return or a handler entry has been checked, and
  with a handler frame open whose return the backend would not report.
  While Detcore holds a thread under a mask of its own, the published mask
  is not read, and each reader reads what it read before: whether a send
  discards the signal, whether a restartable poll admits it and whether a
  real-time timer's expiration is kept read the guest's mask that Detcore
  saved; the choice of the thread a process-directed signal goes to
  (including whether every thread blocks it) and of the first SIGCHLD taker
  read the kernel's mask.

A thread publishes what changed before its next scheduler request, in at
most three extra messages (its process's dispositions, at most twice, and its
mask). They carry no logical time.

The copy answers only for signals whose kernel state it holds exactly: not
for the signals Linux can force-deliver (SIGTRAP, SIGSEGV, SIGBUS, SIGILL,
SIGFPE, SIGSYS), not for Reverie's preemption signal, and not while a signal
the backend passed on without reporting it may have changed the actions or
the mask unseen. Since the copy is checked against the kernel and given up on any
mismatch, where it answers it gives what `/proc` shows, except between a
defect in the copy and the check that finds it (which logs
`SIGNAL_STATE_COPY_DIVERGED`, below). One more exception: when
the target's `/proc` file cannot be read (for example a thread the host has
already removed), the published state still decides, where the `/proc` read
would have decided as if nothing were known.

On ptrace, the copy also asks the backend to report each guest
`rt_sigreturn` before it runs, so the copy follows handler returns exactly
instead of guessing them. It asks only when Hermit measured that the guest
inherits no seccomp filter and no gdbserver is attached (`--gdbserver`),
and not under `--imprecise-timers`, whose preemption events a reported
return would cancel past their period without requesting them again.
Without the reports the copy guesses returns from the kernel's mask, as it
always did. The reports cost something, and in a few cases they change
the run:

- Each handler return becomes one extra ptrace stop.
- A precise preemption timer event due within 10005 retired branches of a
  handler return (Reverie's keep margin) is cancelled at that stop and
  requested again at the same target, so it fires where it fires without
  the reports.
- If a late notification has already let the thread reach or pass the
  target, the event fires one branch after the return. Without the reports
  it would fire after the return too, at a point host timing decides.
  Either way the late notification is witnessed as a skid overshoot, which
  `--strict`, `--verify` and a replay refuse and any other run reports in
  one warning.
- In these cases the stop still cancels the event without requesting it
  again:
  - the mask the handler's frame restores blocks the timer's signal, or
    cannot be read;
  - single steps toward the event failed to clean up, and the thread has
    already reached the target (short of it, the event is requested again
    as above);
  - the event was requested before the guest's first exec and is still
    held;
  - an instruction-offset event whose offset is still ahead at its target
    (Hermit requests none).

  The thread is then preempted only at the next event Detcore sees, such as
  its next syscall or signal. A thread that runs without any after its
  handler returns is never preempted, which can hang a run whose progress
  depends on preemption. In these cases a run with the switch on can be
  scheduled differently from the same run with it off. The two are not the
  same run.

Each caught signal the backend reports is also checked at its delivery,
before the copy records it (A5 step 3.2). Detcore builds the delivery from
the copy, and the backend checks it against the kernel at the stop, reading
only, through `Guest::deliver_signal`. On ptrace:
- the mask the frame saves and the current mask are compared with the
  kernel's (`PTRACE_GETSIGMASK` and procfs `SigBlk`);
- where the thread is at the exit of a syscall Detcore finished, the
  syscall number and the result are compared with `orig_ax` and `rax`.
  Where `orig_ax` names a syscall Detcore cannot see (a number outside
  Reverie's syscall table, or a negative one), no callback updated the
  record, so the registers' own answer is used and counted, and no result
  is compared;
- the handler's mask is the copy's own transition, compared with Linux's
  rule over the same masks and action. The action itself is not read from
  the kernel; the copy checks it where the guest sets it.

Force-delivered signals (faults) the copy blocks are left out of the
comparison of each mask that holds them, and only that one, since a forced
delivery the copy did not see can only have unblocked them (A5 step 3.2b).
Inside a temporary-mask wait the two masks differ, so a forced signal the
copy blocks in only one of them stays checked in the other. The handler's
mask is the copy's own arithmetic and keeps every bit.

The check also runs under e9patch preprocessing, whose guest runs on
ptrace; the tests and the corpus measurement cover plain ptrace.

On ptrace the kernel still delivers the signal, with its own action and
frame. Checking changes the copy exactly as it would unchecked, and at the
same point, with one exception: when the backend reports that the kernel
will not deliver the signal at this resume, nothing is recorded until a later
delivery. On ptrace that is a signal the kernel's current mask blocks, which
the resume requeues, or a stop after an injection that is not a delivery
stop, where the resume queues the signal anew or drops it. A delivery the
backend cannot check at that point is counted by reason and goes on
unchanged, for example a thread with no copy. A force-delivered signal (a
fault) that the copy holds blocked or ignored gives the copy up quietly,
since the kernel unblocked it and reset its action to the default before
the stop. A stop for a signal the copy blocks always shows that, since the
kernel never dequeues a blocked signal. For an ignored one, only a signal
whose siginfo shows the kernel generated it (`si_code` > 0) counts; the same
number sent by a guest's `kill` keeps the copy.

A second signal taken at the same syscall exit, before any guest
instruction, stops at the first handler's first instruction, and the
syscall registers then describe that frame, so they are not compared. If
the first frame could not be written (a frame fault, whose SIGSEGV comes at
the same exit), the registers still describe the syscall, and the check
takes the interruption from them.

A delivery the kernel contradicts, where the copy and the kernel are both
certain, is a Detcore defect. It is logged as `SIGNAL_STATE_COPY_DIVERGED`
and drops the copy. Under `hermit run --strict` it instead ends the run as
an internal failure (exit 125). The log then holds the line
`HERMIT_INTERNAL_FAILURE class=signal-delivery-check`, naming the signal,
the thread and what differed. Hermit's own final class line reads
`container-child-panic` or `container-child-exit`, depending on which
thread failed. A run command that Hermit prints for a strict run, for
example a reproducer, carries `--strict`, so it ends the same way.

Each thread logs its counts when it exits, at TRACE under the target
`detcore::signal_delivery`, below the level that runs compare. The counts
are approvals (frame and current masks matched the kernel's), of those the
ones made at a held signal's report (`held`: a signal that stopped one of
Detcore's injections, reported after the callback that held it),
contradictions, deliveries whose interruption was taken from
the registers, approvals that compared Detcore's syscall result
(`result_checked`), and deliveries not checked, by reason. A copy that this
delivery's own reconciliation gave up is counted as "copy given up at this
delivery", apart from a thread that had no copy at the stop ("no copy").
Signals the backend passes on without reporting them, which no check sees,
are counted by the backend's reason (`unreported`, for example "during a
syscall skip" or "held, restore pending"). Each pass counts: the kernel
then delivers the signal, requeues it when the mask blocks it (a later stop
can pass the same instance again), queues a new instance with a kernel
siginfo (a syscall stop), or discards it (an ignored signal, or a ptrace
event stop or group stop). Reverie's preemption signal,
SIGKILL and SIGSTOP are not counted, since none reaches a guest handler.
Each approval is also logged at TRACE under the same target, with the
interruption it was checked against. To see them:

```bash
RUST_LOG=detcore::signal_delivery=trace hermit --log-file run.log run --strict --epoch=2026-01-01T00:00:00Z -- ./program
```

Reverie can now keep a guest's SIGTRAP state across its own single-steps,
as a Tool opt-in (Reverie c57b7a90, from
https://github.com/rrnewton/reverie/pull/1070). Until Detcore opts in (A5
step 3.0), a multithreaded glibc program loses its copy at its first
`pthread_create`, and its deliveries are counted as "no copy".

What it reports:

- A WARN line starting `SIGNAL_STATE_COPY_DIVERGED` when the copy and the
  kernel disagree in a way no known mechanism explains. That thread's copy is
  then dropped for good. Treat such a line as a defect in the copy, or in
  Detcore's understanding of the guest's signal state.
- At DEBUG, each copy given up quietly, with its reason. These are cases the
  check cannot decide, for example a signal the backend passed on without
  reporting it, a SIGTRAP state changed by a ptrace single-step, a reported
  handler return the kernel did not follow, or a return frame on a protected
  page.
- At TRACE, each seed, exec reset, clone hand-off, recorded delivery,
  reported handler return and matched check, so a trace-level run shows how
  much was actually checked.

It was opt-in until A5 step 2d. It is on by default now that a reported
return no longer loses a precise preemption event (step 2c), and no longer
loses an imprecise one, because the reports are off under
`--imprecise-timers`. The backend still does not report every delivery.
Where one goes unreported, the copy gives itself up for that thread, and
the decisions above read `/proc` as they do without it. The timer cases
above remain where the switch can still change a run.
