---
schema: syscall-pages/v3
syscall: clock_nanosleep
nr_x86_64: 230
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+clock_nanosleep&body=Discussion+for+the+%60clock_nanosleep%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fclock_nanosleep.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60clock_nanosleep%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 403e350e9e82ee540529f097daccae6a60832d07
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

Natively, wake timing rides on host time and host CPU accounting, for whichever clock is named. In the sequentialized deterministic configuration, Hermit determinizes ordinary-clock sleeps on logical deadlines and refuses CPU-clock sleeps that would have to wait. Known gaps: 3.

## Determinism strategy

**What is nondeterministic natively.** Like `nanosleep`, but the guest chooses the clock — including process and thread CPU clocks — and may sleep until an absolute deadline, so wake timing, interruption points, and any remainder all ride on host time and host CPU accounting.[^linux]

**What Hermit does.** In the sequentialized deterministic configuration, Detcore determinizes sleeps on the ordinary clocks through the same family handler as `nanosleep`: the caller parks in Detcore's scheduler until a logical-time deadline, with `TIMER_ABSTIME` naming that deadline directly on the virtual clock.[^c1][^c2][^c7] A past absolute deadline does not wrap or sleep: the sequentialized path builds its wait at that already-reached instant, and the conversion the non-sequentialized path uses clamps a past target to a zero interval.[^c11][^c9] CPU-clock sleeps are the exception below: Detcore has no wait that only the named task's CPU time can end.[^c6][^c8]

**Interface fidelity.** The request is validated before any sleep: a null request pointer fails with `EFAULT`, and a malformed interval fails with `EINVAL`.[^c4] Only `TIMER_ABSTIME` is honored among the flags; any other flag bit sends the call to run in the kernel, exactly because Linux itself reads only that one bit (see gaps).[^c3] `CLOCK_THREAD_CPUTIME_ID` fails with `EOPNOTSUPP`, matching the kernel clock that has no sleep operation.[^c5] For other CPU clocks: the caller's own thread clock and a clock naming no live task fail with `EINVAL`; a zero request, or an absolute deadline the CPU clock has already reached, returns at once.[^c6][^c12] When a signal ends an ordinary-clock sleep, a relative sleep writes its remainder to `rem` from the signal's due time, while an absolute sleep never touches `rem`, as on Linux.[^c10]

**Record & replay.** The call is subscribed in both run and record/replay modes (code facts in the Appendix).[^c2] Virtualized sleeps regenerate from their logical deadlines, including the interruption point and remainder; the kernel-run flag path instead keeps the kernel's restart block.[^c3][^c10]

## Known gaps

- G1 (unimplemented): a CPU-clock sleep that would actually have to wait is refused by the unsupported-operation policy (`ENOSYS` under `--allow-unsupported-syscalls`; a fail-closed run ends naming the reason), because Detcore's sleeps count down on the shared virtual clock that every process advances.[^c6][^c8]
- G2 (by design): flag bits other than `TIMER_ABSTIME` run the sleep in the host kernel — Linux-faithful, but that path observes host time, so it sits outside the deterministic guarantee.[^c3]
- G3 (unimplemented): sequentialized threads without time virtualization cannot honour an absolute deadline and degrade the call to a yield; the deterministic configuration does not take that path.[^c7]

## Discussion

[Start a discussion about `clock_nanosleep`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+clock_nanosleep&body=Discussion+for+the+%60clock_nanosleep%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fclock_nanosleep.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60clock_nanosleep%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Appendix

### Code facts

Detcore classifies `clock_nanosleep` (230) as determinized via typed dispatch; deterministic refusal: false; run subscription: true; record/replay subscription: true.

- Reviewed list: `detcore/src/syscall_classification.rs:1635`
- Dispatch: `detcore/src/lib.rs:8409`

### Tests

Absolute CPU deadlines and refused CPU waits: `detcore/src/syscalls/time.rs` (`an_absolute_cpu_deadline_waits_for_the_rest`) and `detcore/src/cpu_clock/handler_tests.rs` (`a_cpu_clock_sleep_that_would_wait_is_refused`) — interface fidelity, and known gaps. Interruption remainder: `detcore/src/syscalls/time.rs` (`interrupted_sleep_remaining_time_floors_at_zero`) — interface fidelity.

### Citations

[^linux]: Linux `clock_nanosleep(2)`:
  https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/time/posix-timers.c?h=v7.1.3
[^c1]: detcore/src/syscall_classification.rs:1635 sha256:4a4c9cd61f298909
[^c2]: detcore/src/lib.rs:8409-8411 sha256:a8cc88fc6dd9178b
[^c3]: detcore/src/syscalls/time.rs:988-1003 sha256:e136235491938171
[^c4]: detcore/src/syscalls/time.rs:1020-1040 sha256:780522b8b85931be
[^c5]: detcore/src/syscalls/time.rs:1005-1018 sha256:a227c07e2df918b8
[^c6]: detcore/src/syscalls/time.rs:1182-1192 sha256:e48dfcc7f1dd0ef2
[^c7]: detcore/src/syscalls/time.rs:1057-1102 sha256:d54f7113d8299803
[^c8]: detcore/src/cpu_clock.rs:419-434 sha256:8aa80945903cb0be
[^c9]: detcore/src/syscalls/time.rs:1194-1222 sha256:64b20d04fefb079b
[^c10]: detcore/src/syscalls/time.rs:926-972 sha256:ccfb2f3a9f96e469
  https://github.com/facebookexperimental/hermit/blob/main/detcore/src/syscalls/time.rs#L926-L972
[^c11]: detcore/src/syscalls/time.rs:641-645 sha256:9f7ee2f871c456b8
[^c12]: detcore/src/syscalls/time.rs:1149-1192 sha256:97e3cab71e4960c1

## Status

- 2026-11-15 · rev 54bd03547df0 · facts-only page generated from Detcore code facts at this revision.
- 2026-11-16 · rev 7ac736d54e14 · facts refreshed from Detcore at this revision.
- 2026-11-16 · rev 7ac736d54e14 · checked determinism-strategy prose added (rubric v3); citations verified at this revision.
- 2026-11-16 · rev 8e48c1c6605b · summary-first layout (In short, Known gaps, Appendix); claims unchanged.
- 2026-11-18 · rev 403e350e9e82 · facts refreshed from Detcore at this revision.
