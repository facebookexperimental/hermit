---
schema: syscall-pages/v3
syscall: timer_create
nr_x86_64: 222
related:
  - https://github.com/facebookexperimental/hermit/blob/main/docs/syscalls/timer_settime.md
  - https://github.com/facebookexperimental/hermit/blob/main/docs/syscalls/setitimer.md
  - https://github.com/facebookexperimental/hermit/blob/main/docs/syscalls/alarm.md
  - https://github.com/facebookexperimental/hermit/blob/main/docs/syscalls/getitimer.md
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+timer_create&body=Discussion+for+the+%60timer_create%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ftimer_create.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60timer_create%60%2C+or+in+what+the+page+says%3F%0A
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
  reviewed_at: 2026-11-20
  based_on: 403e350e9e82ee540529f097daccae6a60832d07
---

## In short

Natively, a new POSIX timer gets the next id from a per-process counter, so ids run in creation order from the start of the process and only that order follows scheduling. In the default trapped subscription, outside a DBT copied (vfork) child, which runs it natively, Hermit creates no host timer at all: it validates the notification against its own thread model, checks a named CPU task where time is virtualized, and hands back the next id from the same kind of per-process counter that every thread of the process shares. The interval siblings `setitimer`, `alarm`, and `getitimer` keep their own pages and are not checked here; arming and expiry are on the `timer_settime` page, including how a CPU-clock timer counts. Known gaps: 8.

## Determinism strategy

**What is nondeterministic natively.** `timer_create` makes a per-process timer on a named clock and writes its id back. The id is not a host value: the kernel takes the next number from a counter held per process, starting at zero for each new process and kept across exec while the timers themselves are deleted, so a native process receives its ids in its own creation order.[^linux] What follows host state is which thread creates first, together with whether a CPU clock still names a live task and whether a named notification thread still exists when the kernel runs the call.[^linux] The call waits on no clock, but the clock it names decides how a later arming counts down, and that countdown is the host's unless something replaces it.[^linux]

**What Hermit does.** Detcore determinizes `timer_create` by keeping the whole decision in its own model: typed dispatch reaches a handler that checks the output pointer, copies the notification, validates it against the caller's own thread group, and only then allocates an id from the per-process store the process's threads share, writing that id back before it notes the creation for discarded-at-send accounting.[^c1][^c2][^c3][^c7][^c8] The store hands out ids in creation order from a counter that starts empty for each new process, is shared when a thread is cloned, and survives a successful exec while the timer table itself is cleared, so the same creation sequence in the same schedule always yields the same ids and no host timer exists to drift.[^c11][^c16] With no notification pointer the timer takes the default alarm signal and its own id as its value, as the kernel defaults it.[^c11][^linux] Timers notifying with a standard signal are recorded from their creation for the ignored-expiry bookkeeping, armed or not, but only where the scheduler models signal targets with threads sequentialized (the ptrace path, its `e9patch` preprocessing, and KVM); on DBT, SaBRe, and LiteInst no such record is kept and expiry takes the fallback described on the `timer_settime` page.[^c8][^c13][^c21] The sibling interval timers `setitimer`, `alarm`, and `getitimer` are separate calls with their own handlers and pages; nothing here checks them.

**Interface fidelity.** A null output pointer fails with `EFAULT` before anything is copied, and a fault copying the notification fails the call with no timer made.[^c3] The notification forms are then judged in an order that keeps the kernel's clock answer ahead of Detcore's own checks only on one path, stated in G1: no notification delivers nothing and keeps the caller's value, the plain signal form needs a signal number in the valid range or the call fails with `EINVAL`, a raw `SIGEV_THREAD` value is treated as that same plain signal form, and any other notification value fails with `EINVAL` at once.[^c4][^linux] The C library's `SIGEV_THREAD` reaches the kernel as the thread-id form with an internal real-time signal, so a library thread-notification timer is created here and then never fires in a permissive run and refuses a fail-closed run (G5); there is no fallback inside that form, and a program must use `SIGEV_SIGNAL` with a standard signal instead.[^c4][^c18] The thread-id form first needs the named thread to be a thread of the caller's own process, found through the scheduler's thread tree; another process's thread, no such thread, or one of Hermit's own threads fails with `EINVAL`, and a caller inside a guest-made pid namespace is refused by name instead (G2).[^c4][^c10] A CPU clock is then checked where time is virtualized: the task the clock names must still be a live task, kept by its own identity so a later task with the same number cannot stand in for it (G3).[^c5][^c12] Two arming gates can still refuse a creation the kernel would allow, each stated as a gap: a notification signal the in-guest route reserves for itself fails with `EPERM`, and a guest alarm signal can be refused with `EPERM` on in-guest LiteInst in either order (G4).[^c6][^c14] A timer armed to kill the process still runs as on Linux; where the run tracks chosen inputs the creation is also noted for that accounting, which never changes the guest operation itself.[^c6] A real-time notification signal is accepted here and at arming, and the timer can be read and deleted as on Linux, but its expiry is never delivered in a permissive run; a fail-closed run is refused when that expiry could be observed, including whenever Detcore cannot decide whether it could be, or later if an expiry kept pending becomes observable, while, where the scheduler models signal targets, an expiry whose signal is ignored and left unblocked by some thread, or whose process has no live thread, is discarded, and on the other backends the arming thread's state decides instead (G5).[^c17][^c18] The call never blocks, so there is no interruption or restart path to model. Where Detcore's checks fall relative to the kernel's id write is itself a divergence, stated as G7.

**Record & replay.** The call is subscribed in both run and record/replay modes (code facts in the Appendix).[^c2] Replay creates no host timer either: it re-derives the same id from the same per-process store at the same schedule point, and re-runs the same notification and CPU-task checks against the replayed thread model.[^c7][^c11] What record and replay change is only how a later expiry coalesces, which the `timer_settime` page states; creation itself is identical in both modes.[^c15]

## Known gaps

- G1 (unimplemented): an unsupported clock is refused with the kernel's own answer (`EINVAL` for an id with no clock, `EOPNOTSUPP` for a clock with no timers) only when the notification names a signal the in-guest route reserves; for an ordinary signal the clock is never checked at creation, so a clock Linux refuses at once is accepted and runs on the one virtual clock, and no later call fails. The wake-alarm clocks are likewise always accepted, where Linux answers `EOPNOTSUPP` without an RTC device and `EPERM` without the wake-alarm capability, which a containerized guest normally lacks. Linux judges the clock first, before it validates the notification. A guest that probes clocks by creating timers sees success where Linux fails, and has no fallback inside the call.[^c3][^c9][^linux]
- G2 (unimplemented): the thread-id notification form from a caller inside a pid namespace the guest made itself is refused by name with `ENOSYS` where unsupported operations are allowed, and stops a fail-closed run, because the named id is a pid of that namespace and Detcore does not translate it; Linux accepts a thread of the caller's own group named that way. A creation through the C library's thread notification inside such a namespace gets the same refusal. A guest in such a namespace must notify its whole process with `SIGEV_SIGNAL` and a standard signal instead.[^c4][^c10]
- G3 (unimplemented): creation never reaches the host, whatever the time setting: with time unvirtualized no named CPU task is checked at all, so a CPU clock naming no live task is accepted where Linux answers `EINVAL`, and the timer is created unconditionally rather than following the host. The timer then counts down on Detcore's logical clock while the guest's own clock reads go to the host, the opposite of host clocks serving the timer; an absolute arming in that state therefore mixes a host-clock value with logical time (see the `timer_settime` page). The virtualized path above is the checked one.[^c5][^c19]
- G4 (unimplemented): a creation whose notification signal the in-guest route reserves is refused with `EPERM` where Linux creates the timer, and on in-guest LiteInst a POSIX timer cannot deliver `SIGALRM` to a handler in either order: creating it first blocks the later handler install with `EPERM`, and installing the handler first blocks creation, the default no-notification form included.[^c20] Other backends do not take these refusals. A guest on those routes has no fallback inside the call and must notify with another signal.[^c6][^c14]
- G5 (unimplemented): a timer notifying with a real-time signal is created, and can be armed, read, and deleted as on Linux, but no expiry of it is ever delivered in a permissive run, and a fail-closed run is refused at expiry when that expiry could be observed, including whenever Detcore cannot decide, or later if an expiry kept pending becomes observable; where the scheduler models signal targets, an expiry is discarded only when its signal is ignored and some thread leaves it unblocked, or when the process has no live thread, while on DBT, SaBRe, and LiteInst the arming or named thread's own state decides instead, and a process-directed expiry is discarded once that thread has exited, even while the process lives.[^c17][^c22] The C library's thread-notification form rides on this same real-time mechanism, so those timers likewise never fire in a permissive run. Detcore's signal model carries no real-time signal to deliver. The `timer_settime` page states the arming side. A guest must use a standard signal to get a delivery.[^c18]
- G6 (unimplemented): the id sequence after failed creations departs from Linux. Linux takes the next per-process id before it validates the notification, writes the id back, and runs the clock's create hook, so any failure after that point still consumes the id, and for CPU-clock and alarm-clock failures the would-be id was already written to the caller's memory. Detcore otherwise runs every check before it allocates, returns before taking an id, and leaves that memory untouched, so after k creations that fail after Linux has taken the id (the notification check, a null id pointer, or the clock's create hook where Detcore also refuses, which is the CPU-clock task check when time is virtualized), together with Detcore-only refusals Linux does not make (the nested-namespace `ENOSYS` of G2 and the reserved-signal and `SIGALRM` `EPERM` of G4), every later Detcore id is k lower than Linux's, not counting an unwritable id pointer, which uses up an id here as in Linux (G7). For the wake-alarm clocks (G1) and for CPU clocks when time is unvirtualized (G3), Detcore accepts the creation and takes and writes an id just as Linux does, so those cases shift nothing. A guest that creates timers after probing failures observes different ids than natively.[^c3][^c11][^linux]
- G7 (unimplemented): the order of the checks and of the id write differs from the kernel's. A null id pointer returns `EFAULT` before the clock and notification answers where Linux gives those answers first and writes the id only after both; a CPU-clock `EINVAL` comes before the id is written here but after it in Linux; and with an unwritable non-null id pointer Detcore creates the timer before the write, leaving a live timer after the `EFAULT` that `timer_settime`, `timer_gettime`, and `timer_delete` then accept, where Linux frees it and those calls return `EINVAL`.[^c3][^c7][^linux]
- G8 (unimplemented): `EAGAIN` is never returned. Linux charges each new timer against the caller's pending-signal limit, which counts that user's pending signals across all host processes, and refuses creation with `EAGAIN` at the limit; Detcore has no such charge, so a guest at the native limit creates timers here without limit.[^c3][^linux]

## Discussion

[Start a discussion about `timer_create`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+timer_create&body=Discussion+for+the+%60timer_create%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ftimer_create.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60timer_create%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Appendix

### Code facts

Detcore classifies `timer_create` (222) as determinized via typed dispatch; deterministic refusal: false; run subscription: true; record/replay subscription: true.

- Reviewed list: `detcore/src/syscall_classification.rs:1950`
- Dispatch: `detcore/src/lib.rs:8699`

### Tests

The clock-refusal table against the kernel's: `detcore/src/syscalls/time.rs` (`timer_create_clock_refusal_matches_linuxs_clock_table`) — interface fidelity. The CPU-clock task check at creation: `detcore/src/cpu_clock/handler_tests.rs` (`timer_create_checks_the_task_a_cpu_clock_names`) — what Hermit does, and interface fidelity; that same test also pins G6, since its failed creations leave the next id at the start where Linux would have consumed those ids. Deterministic ids in creation order: `detcore/src/tool_local.rs` (`ids_are_deterministic_and_sequential`) — what Hermit does. Guest programs posix_timer_test, timer_create_determinism, timer_sigev_thread_id, timer_create_rt_signal_refused, and timer_family_identity under tests exercise creation, the thread-id form, and the refused real-time expiry end to end — what Hermit does, and interface fidelity.

### Citations

[^linux]: Linux `timer_create(2)`:
  syscall entry: https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/time/posix-timers.c?h=v7.1.3#n566
  clock judged first, then the notification: https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/time/posix-timers.c?h=v7.1.3#n458
  id taken from the per-process counter: https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/time/posix-timers.c?h=v7.1.3#n157
  id taken before the later checks, written back, then the clock hook: https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/time/posix-timers.c?h=v7.1.3#n490
  pending-signal limit checked: https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/signal.c?h=v7.1.3#n418
  creation refused at the pending-signal limit: https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/time/posix-timers.c?h=v7.1.3#n480
  per-process counter zeroed for each new process: https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/fork.c?h=v7.1.3#n1703
  timers deleted on exec, counter kept: https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/fs/exec.c?h=v7.1.3#n1158
  raw SIGEV_THREAD treated as the signal form: https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/time/posix-timers.c?h=v7.1.3#n403
  notification validity: https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/time/posix-timers.c?h=v7.1.3#n391
  CPU-clock creation task check: https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/time/posix-cpu-timers.c?h=v7.1.3#n386
  wake-alarm clock creation check: https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/time/alarmtimer.c?h=v7.1.3#n667
[^c1]: detcore/src/syscall_classification.rs:1950 sha256:70a45d9bd52644fc
[^c2]: detcore/src/lib.rs:8699 sha256:ee577679df5ad921
  https://github.com/facebookexperimental/hermit/blob/main/detcore/src/lib.rs#L8699
[^c3]: detcore/src/syscalls/time.rs:1252-1277 sha256:88c89f02f6e66c8f
[^c4]: detcore/src/syscalls/time.rs:1278-1311 sha256:1f574223b34aa78b
[^c5]: detcore/src/syscalls/time.rs:1317-1323 sha256:57473cddc6157838
[^c6]: detcore/src/syscalls/time.rs:1329-1345 sha256:ef9c5c1f171bbf4a
[^c7]: detcore/src/syscalls/time.rs:1346-1352 sha256:e66d6c465c485c23
[^c8]: detcore/src/syscalls/time.rs:1356-1367 sha256:d42f5e549e99e28a
[^c9]: detcore/src/syscalls/time.rs:98-114 sha256:3e04a29cb95b771a
[^c10]: detcore/src/scheduler.rs:15253-15266 sha256:3c49ffa3f5160d88
[^c11]: detcore/src/tool_local.rs:679-700 sha256:ed099ae5fe96ec77
[^c12]: detcore/src/cpu_clock.rs:303-329 sha256:9b1397d27cb41441
[^c13]: detcore/src/scheduler/discarded_at_send.rs:65-67 sha256:31f653c9c9e6d869
[^c14]: detcore/src/syscalls/signal.rs:4692-4709 sha256:bbb4e384cf9d57c7; detcore/src/tool_global.rs:13675-13688 sha256:0c67a1be57c9a924
[^c15]: detcore/src/scheduler/itimer_coalesce.rs:99-101 sha256:4c81d5eab1a828b8
[^c16]: detcore/src/lib.rs:6319-6327 sha256:b1ccc906a01b3862; detcore/src/lib.rs:6798-6807 sha256:a1fdd7b6fcc4aa05; detcore/src/tool_local.rs:767-769 sha256:7c11f2515495f32b
[^c17]: detcore/src/scheduler/parked.rs:1114-1214 sha256:23e73efddeeebc79
[^c18]: detcore/src/syscalls/time.rs:1224-1235 sha256:9bcb2fdbd4eff3f3; detcore/src/syscalls/time.rs:1474-1497 sha256:8b1e7bd8cfd081f5
[^c19]: detcore/src/syscalls/time.rs:1438-1448 sha256:c1493767e96d2337; detcore/src/lib.rs:8248-8253 sha256:39a81cea9bf11c60
[^c20]: detcore/src/scheduler/sigalrm.rs:389-393 sha256:43dead366d82aee3; detcore/src/syscalls/signal.rs:1979 sha256:97d007ff86f1af6b
[^c21]: detcore/src/scheduler.rs:1861-1870 sha256:9eea619c6b34b578
[^c22]: detcore/src/scheduler/timed_waiters.rs:159-162 sha256:d83754d307dc6a2d

## Status

- 2026-11-15 · rev 54bd03547df0 · facts-only page generated from Detcore code facts at this revision.
- 2026-11-16 · rev 7ac736d54e14 · facts refreshed from Detcore at this revision.
- 2026-11-18 · rev 403e350e9e82 · facts refreshed from Detcore at this revision.
- 2026-11-20 · rev 403e350e9e82 · checked determinism-strategy prose added (rubric v3); citations verified at this revision.
