# Checking Detcore's signal-state copy

Detcore can keep its own copy of every guest thread's signal dispositions and
blocked mask, and check it against the kernel. This is step 1 of audit step A5
(https://github.com/rrnewton/hermit/issues/4485): a diagnostic that no
decision reads yet. It is off by default. Turn it on with:

```bash
HERMIT_SIGNAL_STATE_CHECK=1 hermit run --strict --epoch=2026-01-01T00:00:00Z -- ./program
```

It runs only where the kernel's signal state for a guest thread is the
guest's own, which means the ptrace backend (and e9patch under ptrace), with
serialized threads. Elsewhere, and whenever the variable is not `1`, nothing
is recorded or checked, and runs are exactly as they would be without it.

What it reports:

- A WARN line starting `SIGNAL_STATE_COPY_DIVERGED` when the copy and the
  kernel disagree in a way no known mechanism explains. That thread's copy is
  then dropped for good. Treat such a line as a defect in the copy, or in
  Detcore's understanding of the guest's signal state.
- At DEBUG, each copy given up quietly, with its reason. These are cases the
  check cannot decide, for example a signal the backend passed on without
  reporting it, a SIGTRAP state changed by a ptrace single-step, or a handler
  return that mask equality cannot confirm.
- At TRACE, each seed, exec reset, clone hand-off, recorded delivery and
  matched check, so a trace-level run shows how much was actually checked.

It is opt-in because ptrace reports neither handler returns (`rt_sigreturn`
is never trapped) nor every delivery. In rare valid cases the copy can
therefore still report a divergence. Step 3 of A5, which adds those backend
reports, is the step that makes the copy fit to be relied on.
