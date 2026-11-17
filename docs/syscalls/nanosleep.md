---
schema: syscall-pages/v3
syscall: nanosleep
nr_x86_64: 35
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+nanosleep&body=Discussion+for+the+%60nanosleep%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fnanosleep.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60nanosleep%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 7ac736d54e14d3eb8e190ae1849481e5ff41ea1a
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
# prose: human/agent-authored, machine-checked (syscall-pages.rs check); v3 only
prose:
  status: checked
  reviewed_at: 2026-11-16
  based_on: 7ac736d54e14d3eb8e190ae1849481e5ff41ea1a
---

## In short

Natively, wake timing and interruption depend on the host clock and scheduler. In the deterministic configuration Hermit parks the caller in its own scheduler until a logical-time deadline. Known gaps: 2.

## Determinism strategy

**What is nondeterministic natively.** A native sleep ends when the host's monotonic clock and scheduler say so: the actual delay overshoots by host load, and whether a signal interrupts it — and how much time then remains — depends on host-timed signal delivery.[^linux]

**What Hermit does.** In the sequentialized deterministic configuration, Detcore determinizes the sleep: the caller parks in Detcore's own scheduler until a logical-time deadline, so the wake point is a schedule event, not a host wait, and other guest threads run meanwhile in deterministic order.[^c1][^c2][^c6] A configuration that does not sequentialize threads instead runs the sleep in the host kernel, keeping the kernel's own restart block; that path observes host time and is a relaxation, not the deterministic mode.[^c6]

**Interface fidelity.** The request is validated before any sleep: a null request pointer fails with `EFAULT`, and a malformed interval (negative seconds, or nanoseconds outside the legal range) fails with `EINVAL` rather than wrapping into an effectively endless sleep.[^c4] A signal that ends the sleep returns `EINTR` and, for a relative sleep, writes the unslept remainder to `rem` measured from when the interrupting signal was due.[^c5] One restart subtlety matches Linux: when a `SIGCHLD` that Detcore holds for delivery at the thread's resumption (its claimed-signal case) ends a relative sleep and no handler runs, the original deadline is kept for the kernel's `restart_syscall` resumption instead of starting the interval over.[^c5]

**Record & replay.** The call is subscribed in both run and record/replay modes (code facts in the Appendix).[^c2] The virtualized sleep is regenerated from its logical deadline at the same schedule point, including which signal interruption ends it and the remainder that follows.[^c5][^c6]

## Known gaps

- G1 (by design): the slept interval is logical time, so the overshoot profile is the virtual clock's granularity rather than a host high-resolution timer's.[^c6]
- G2 (by design): without thread sequentialization the sleep runs in the host kernel and observes host time; deterministic runs use the sequentialized path above.[^c6]

## Discussion

[Start a discussion about `nanosleep`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+nanosleep&body=Discussion+for+the+%60nanosleep%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fnanosleep.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60nanosleep%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Appendix

### Code facts

Detcore classifies `nanosleep` (35) as determinized via typed dispatch; deterministic refusal: false; run subscription: true; record/replay subscription: true.

- Reviewed list: `detcore/src/syscall_classification.rs:1805`
- Dispatch: `detcore/src/lib.rs:8156`

### Tests

Interruption remainder and zero-length sleeps: `detcore/src/syscalls/time.rs` (`interrupted_sleep_remaining_time_floors_at_zero`, `only_a_relative_sleep_of_zero_is_a_zero_length_sleep`) — interface fidelity, and what Hermit does.

### Citations

[^linux]: Linux `nanosleep(2)`:
  https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/time/hrtimer.c?h=v7.1.3
[^c1]: detcore/src/syscall_classification.rs:1805 sha256:bdb80887873e8a3d
[^c2]: detcore/src/lib.rs:8156 sha256:ed87abad08b828c8
[^c4]: detcore/src/syscalls/time.rs:1020-1040 sha256:780522b8b85931be
[^c5]: detcore/src/syscalls/time.rs:926-972 sha256:ccfb2f3a9f96e469
[^c6]: detcore/src/syscalls/time.rs:1105-1125 sha256:da2305f33c564d21
  https://github.com/facebookexperimental/hermit/blob/main/detcore/src/syscalls/time.rs#L1105-L1125

## Status

- 2026-11-15 · rev 54bd03547df0 · facts-only page generated from Detcore code facts at this revision.
- 2026-11-16 · rev 7ac736d54e14 · facts refreshed from Detcore at this revision.
- 2026-11-16 · rev 7ac736d54e14 · checked determinism-strategy prose added (rubric v3); citations verified at this revision.
- 2026-11-16 · rev 8e48c1c6605b · summary-first layout (In short, Known gaps, Appendix); claims unchanged.
