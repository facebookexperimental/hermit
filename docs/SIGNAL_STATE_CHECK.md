# Checking Detcore's signal-state copy

Detcore can keep its own copy of every guest thread's signal dispositions and
blocked mask, and check it against the kernel. This is step 1 of audit step A5
(https://github.com/rrnewton/hermit/issues/4485): a diagnostic that no
decision reads yet. It is off by default. Turn it on with:

```bash
HERMIT_SIGNAL_STATE_CHECK=1 hermit run --strict --epoch=2026-01-01T00:00:00Z -- ./program
```

It runs only where the kernel's signal state for a guest thread is the
guest's own, which means the ptrace backend (and e9patch preprocessing,
which Hermit runs on ptrace), with serialized threads. Elsewhere, and
whenever the variable is not `1`, nothing is recorded or checked, and runs
are exactly as they would be without it.

On ptrace, turning it on also asks the backend to report each guest
`rt_sigreturn` before it runs, so the copy follows handler returns exactly
instead of guessing them. It asks only when Hermit measured that the guest
inherits no seccomp filter and no gdbserver is attached (`--gdbserver`);
without the reports the copy guesses returns from the kernel's mask, as it
always did. The reports are not free, and they change
the run:

- Each handler return becomes one extra ptrace stop.
- A preemption timer event due within 10005 retired branches of a handler
  return is cancelled at that stop (Reverie's keep margin). The thread is
  then preempted only at its next syscall. A thread that spins without
  syscalls after its handler returns is never preempted, which can hang a
  run whose progress depends on preemption.
- So a run with the switch on can be scheduled differently from the same
  run with it off. Each is deterministic, but the two are not the same run.

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

It stays opt-in because the backend does not report every delivery yet, and
because of the timer cost above, which must be resolved before step 2 of A5
lets decisions rely on the copy.
