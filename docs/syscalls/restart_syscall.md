---
schema: syscall-pages/v3
syscall: restart_syscall
nr_x86_64: 219
related:
  - https://github.com/rrnewton/hermit/issues/404
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+restart_syscall&body=Discussion+for+the+%60restart_syscall%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Frestart_syscall.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60restart_syscall%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 403e350e9e82ee540529f097daccae6a60832d07
  detcore_class: unsupported
  dispatch: "unsupported"
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

Natively, the kernel resumes an interrupted wait through this call, and a bare call returns EINTR. Hermit intercepts it to resume waits from its own restart records, and treats a bare call as unsupported. Known gaps: 4.

## Determinism strategy

**What is nondeterministic natively.** `restart_syscall` is a real syscall in the table, but guests do not call it directly in normal operation: when a signal interrupts a wait with the restart-block code and no handler runs, the kernel rewrites the thread's registers so the thread re-issues this call itself from user space, and the kernel resumes the wait from the saved restart block (above all its original deadline).[^linux] Only that one restart code takes this route; the other restart codes re-execute the original call, and with a handler the interrupted wait returns `EINTR` instead.[^linux] Issued with nothing pending, the call itself returns `EINTR`.[^linux] Resuming the wrong wait, or with a fresh instead of the saved deadline, changes what the guest observes just as surely as answering the original wait wrongly.

**What Hermit does.** As a directly issued call this number is unsupported, and it sits in the unsupported class for exactly that reading.[^c1] In its real role, though, Detcore intercepts it ahead of the unsupported policy: when a wait Detcore emulates (or a poll it ran in the kernel) was interrupted under a restart code, Detcore keeps its own restart record for the thread, and this call resumes that wait from the record — original deadline included — instead of re-reading the guest's syscall arguments.[^c2][^c5][^c6] Waits Detcore itself ran in the kernel resume in the kernel from their own restart block.[^c6]

**Interface fidelity.** The record is taken from the thread at every syscall entry and resumption matches it by the instruction pointer of the interrupted call, so only the very next syscall, arriving as this call at that address, resumes the wait; any other entry has already consumed the record.[^c9][^c4] Resumption covers Detcore's emulated polls, kernel-run polls, futex waits, claimed relative sleeps, and waits left in the kernel.[^c6] With no matching record — including a bare guest-issued call — the unsupported-syscall policy decides: a fail-closed run ends (shutdown, tool error, or panic by configuration), while a permissive run reports the call and passes it through.[^c4][^c7] The restart errnos each wait keeps across a signal follow Linux's restart policy per operation (which waits restart, which keep a copied deadline, which always restart), rather than one rule for all waits.[^c10]

**Record & replay.** The call is subscribed in both run and record/replay modes (code facts in the Appendix).[^c2] The restart record is tool state rebuilt by the same interrupted wait in the same schedule, so replay reaches the same resumption at the same instruction pointer and consumes an identical record.[^c5][^c6]

## Known gaps

- G1 (by design): a woken poll's internal rerun record deliberately never serves this kernel restart entry; reaching it from here takes the unsupported policy instead.[^c6]
- G2 (unimplemented): a wait whose restart Detcore does not model has no record to resume from, so its restart takes the same unsupported policy as a bare call rather than resuming.[^c4][^c6]
- G3 (by design): a bare guest-issued call gets the unsupported policy (in a fail-closed run, the run ends), where Linux simply returns `EINTR`.[^linux][^c7]
- G4 (by design): the record's lifetime is stricter than Linux's restart block, which survives the handler's own syscalls and resumes from any address; Detcore's record lives only until the next syscall entry and must match its address.[^c8][^c9]

## Discussion

[Start a discussion about `restart_syscall`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+restart_syscall&body=Discussion+for+the+%60restart_syscall%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Frestart_syscall.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60restart_syscall%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Appendix

### Code facts

Detcore classifies `restart_syscall` (219) as unsupported via unsupported dispatch; deterministic refusal: false; run subscription: true; record/replay subscription: true.

- Reviewed list: `detcore/src/syscall_classification.rs:2094`
- Dispatch: `detcore/src/lib.rs:8796`

### Tests

Restart-errno policy per wait: `detcore/src/syscalls/helpers.rs` (`signal_interruption_errno_matches_linux_restart_policy`, `kernel_restart_errno_matches_linux_restart_policy`, `a_woken_poll_restarts_through_erestartsys_where_linux_restarts_it`) — interface fidelity, and what Hermit does.

### Citations

[^linux]: Linux `restart_syscall` and the restart block:
  https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/signal.c?h=v7.1.3#n3177
  and the register rewrite:
  https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/arch/x86/kernel/signal.c?h=v7.1.3#n346
[^c1]: detcore/src/syscall_classification.rs:2094 sha256:141bc2be34332b90
[^c2]: detcore/src/lib.rs:8792-8805 sha256:60fa791b3b39c5f3
[^c4]: detcore/src/lib.rs:2184-2201 sha256:a52a49e793018d72
[^c5]: detcore/src/lib.rs:2123-2130 sha256:9019875916d9fecc
[^c6]: detcore/src/lib.rs:2206-2243 sha256:11ce7bfdd0a59cb2
  https://github.com/facebookexperimental/hermit/blob/main/detcore/src/lib.rs#L2206-L2243
[^c7]: detcore/src/lib.rs:2094-2120 sha256:a956e173f03ed24f
[^c8]: detcore/src/syscalls/helpers.rs:3016-3045 sha256:4f7604dfddef1d52
[^c9]: detcore/src/lib.rs:7207-7215 sha256:9ae2050e058435fa
[^c10]: detcore/src/syscalls/helpers.rs:2260-2290 sha256:c25880d1866152ac

## Status

- 2026-11-15 · rev 54bd03547df0 · facts-only page generated from Detcore code facts at this revision.
- 2026-11-16 · rev 7ac736d54e14 · facts refreshed from Detcore at this revision.
- 2026-11-16 · rev 7ac736d54e14 · checked determinism-strategy prose added (rubric v3); citations verified at this revision.
- 2026-11-16 · rev 8e48c1c6605b · summary-first layout (In short, Known gaps, Appendix); claims unchanged.
- 2026-11-18 · rev 403e350e9e82 · facts refreshed from Detcore at this revision.
