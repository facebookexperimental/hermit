---
schema: syscall-pages/v3
syscall: timer_settime
nr_x86_64: 223
related:
  - https://github.com/facebookexperimental/hermit/blob/main/docs/syscalls/setitimer.md
  - https://github.com/facebookexperimental/hermit/blob/main/docs/syscalls/alarm.md
  - https://github.com/facebookexperimental/hermit/blob/main/docs/syscalls/getitimer.md
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+timer_settime&body=Discussion+for+the+%60timer_settime%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ftimer_settime.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60timer_settime%60%2C+or+in+what+the+page+says%3F%0A
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

Natively, an armed POSIX timer counts down on a host clock and its expiry races every other thread's work. Hermit arms the timer against the shared virtual clock instead, registers the deadline with its scheduler in the same turn, and reports the old arming only after that registration, so the same schedule expires the timer at the same logical moment. The interval siblings `setitimer`, `alarm`, and `getitimer` keep their own pages and are not checked here. Known gaps: 7.

## Determinism strategy

**What is nondeterministic natively.** `timer_settime` arms, rearms, or disarms a timer, and natively the time left on the old arming, the moment the new one expires, and how many periods pass before the signal is taken all follow host clocks and host scheduling.[^linux] Even a well-formed request can therefore expire at different host moments in two runs, and a periodic timer can miss a different number of periods each time.[^linux]

**What Hermit does.** Detcore determinizes `timer_settime` by making the virtual clock the only clock the arming counts down on: typed dispatch reaches a handler that reads the new setting, turns it into a deadline on the caller's observed virtual time (an absolute setting is read against that same virtual clock), stores the arming in the per-process timer store, and, for a timer that notifies with a standard signal, registers the deadline, target, and reload with the scheduler before the old value is copied out.[^c1][^c2][^c6][^c7][^c8][^c11] Registration bumps the timer's arming generation each time, so on the path that keeps signal entries an expiry queued before a rearm or disarm is known stale when it would be taken, and disarming registers no deadline at all.[^c16] Where the scheduler models signal targets on the ptrace path with its `e9patch` preprocessing, threads sequentialized, outside record and replay, and the pair observable, a periodic expiry then waits for its signal entry to be taken: the take re-arms it on its grid and counts the missed periods at that moment, an expiry of an ignored signal is held aside and queued again if the signal stops being ignored, and a one-shot expiry of an ignored signal is dropped (G5).[^c14][^c13][^c15][^c21] On KVM the expiry never reaches that machinery: a process-directed expiry is published with an overrun of zero and, if the leader ignores it, discarded at generation while the timer rearms on the recurrence the timed-waiter installed (the old deadline plus one interval), one publication per missed period as on DBT, SaBRe, and LiteInst, a copy still pending absorbing the next one, with nothing held aside or queued again on unignore, and the reported remaining time comes from the store (G4).[^c18][^c24] A thread-directed (`SIGEV_THREAD_ID`) expiry on KVM does reach the timer-firing path, but with coalescing off: overrun zero and the recurrence unchanged (G4).[^c26] An expiry queued before a rearm can still be delivered on KVM too, because the publication ledger keeps no arming generation (G4).[^c18] On DBT, SaBRe, and LiteInst, which do not model signal targets that way, each expiry is instead sent when the scheduler reaches it, once per missed period with no overrun ever counted, and an expiry queued before a rearm can still be delivered (G4).[^c19] No host timer is armed in any case, and the C sibling calls that share this store only as neighbours (`timer_gettime`, `timer_getoverrun`, and `timer_delete`) are not checked here; the interval siblings `setitimer`, `alarm`, and `getitimer` have their own handlers and pages.

**Interface fidelity.** A missing new-setting pointer fails with `EINVAL`, a fault reading it fails the call, and an unknown timer id fails with `EINVAL` from the store lookup, before any registration.[^c3][^c7] A zero value disarms and a zero interval makes the timer one-shot, and a deadline already in the past on an absolute setting expires at once rather than failing, as the kernel serves it; the one disarm exception is stated as G6.[^c6][^linux] The old arming is reported only after the store and the scheduler have both been told, in the kernel's own order, so a fault copying the old value out undoes neither the new arming nor its registration; where the scheduler keeps the timer's phase, the reported time left comes from that record rather than from the store's arithmetic.[^c7][^c8][^c9][^c12] Three fidelity limits are stated as gaps rather than smoothed over: an ill-formed setting is flattened instead of refused (G1), a real-time notification is armed but never delivered (G2), and a CPU clock counts down on the shared virtual clock rather than on its task's CPU time, with one arming form refused outright (G3).[^c4][^c10][^c5] The call itself never blocks or sleeps, so there is no interruption or restart path; a fault is the copy faults above.

**Record & replay.** The call is subscribed in both run and record/replay modes (code facts in the Appendix).[^c2] Standard record and replay deliberately do not virtualize guest time,[^c22] so a recorded guest reads the real host clock while this handler still arms against virtual time and reads an absolute setting against that virtual clock, epoch included; the replay claim is therefore regeneration only, and absolute and CPU-clock settings in that mode are stated as G7.[^c6][^c8][^linux] Replay does not substitute a recorded expiry: it re-derives the deadline from the same virtual time and the same store at the same schedule point and registers it again, so the expiry is regenerated rather than replayed.[^c6][^c8] One narrowing is explicit: the coalescing described above is off in record and replay, which keep the older recurrence where each expiry is sent as the scheduler reaches it, so the checked coalescing claim covers ordinary runs and the replay claim is only that the deadline and delivery are regenerated in order.[^c15]

## Known gaps

- G1 (unimplemented): an ill-formed new setting (a negative field, or a nanosecond field at or past a whole second) is flattened field by field (a negative field reads as zero, an oversized nanosecond field is carried into the total) and the timer is armed or disarmed by the flattened value, where Linux refuses the call with `EINVAL` before it looks the timer up. No native measurement is needed to see the flattening in the conversion, but the guest-visible error Linux gives is never returned here, so a guest that validates its settings by this call is silently armed or disarmed instead. There is no fallback inside the call. This is a defect candidate for the timer handlers, not a documented Linux behaviour.[^c4][^c3][^linux]
- G2 (unimplemented): a timer notifying with a real-time signal is armed but, in a permissive run, registered nowhere: a permissive run logs that it will never fire and leaves it unregistered, so no expiry is ever delivered, while a fail-closed run registers it only so the scheduler can refuse the whole run if a deliverable expiry comes due; an expiry whose signal is ignored and left unblocked by some thread is discarded and the run goes on, one every thread blocks is kept pending only while nothing can observe it yet, and the run is refused later if that pending expiry becomes observable.[^c25] Linux delivers such expiries. A guest must notify with a standard signal to get a delivery; creation, reading, and deletion of such a timer still behave as on Linux.[^c10][^c8][^c2]
- G3 (unimplemented): a timer on a CPU clock counts down on the shared virtual clock, which every process advances, not on the CPU time of the task the clock names; a relative arming is served that way without telling the guest, and an absolute arming that would still have to wait for CPU time to come, or that would reload for more, is refused by name with `ENOSYS` where unsupported operations are allowed and stops a fail-closed run; an absolute arming on another process's CPU clock is refused even when its deadline has already passed, because reading that clock is refused. For the caller's own process — its own thread, the whole process, or a sibling thread — an absolute deadline already passed, with no reload, is instead served and fires at once. Once the named task is gone the call fails with `ESRCH` and arms nothing, as the kernel answers. A guest that needs true CPU-time expiry has no fallback inside the call.[^c5][^c17]
- G4 (unimplemented): the scheduler's remaining-time and overrun record for a timer is kept only where the scheduler coalesces, on the ptrace path with its `e9patch` preprocessing, threads sequentialized, and outside record and replay; the readout the old-value report consults returns nothing where coalescing is off. Where coalescing is on, an observable pair is counted at the take, and an unobservable pair is forwarded and counted at the send. On KVM a process-directed expiry is published with an overrun of zero and discarded at generation if the leader ignores it, with nothing held aside or queued again, so `timer_getoverrun` reads zero and the remaining time comes from the store; a thread-directed expiry there takes the coalescing-off fallback, overrun zero and the recurrence unchanged, and an expiry queued before a rearm can still be delivered, the publication ledger keeping no arming generation. On DBT, SaBRe, and LiteInst each expiry is sent once per missed period as the scheduler reaches it, no overrun is ever counted, and an expiry queued before a rearm can still be delivered. Coalescing itself is per (process, signal) pair and is lost for good once any thread enters a call outside the schedule, a host-timed source of that signal exists, an unreaped child's exit signal equals it, the real interval timer is armed on `SIGALRM`, the process leader is no longer live, or the signal is in the excluded set; the ptrace pair-level fallback then forwards to the next grid point and counts a missed period at the sending moment. No guest fallback recovers the count inside this call.[^c12][^c13][^c14][^c18][^c19]
- G5 (unimplemented): an expiry that arrives while its signal is ignored is held aside only where the scheduler keeps that record, on the coalescing path in G4; losing observability normally moves the pair to the pair-level fallback described in G4, and the run is refused by name only when a current held ignored expiry exists, because Linux queues the held expiry again when the signal stops being ignored and the fallback cannot follow that, or when the pair's lists hold more than a single own entry, because Linux would deliver those entries one by one. A periodic timer's held expiry is otherwise queued again on unignore and a one-shot's is dropped, as the strategy part states. A guest has no fallback inside the call when the refusal fires.[^c14][^c13][^c20]
- G6 (unimplemented): a disarming setting keeps its interval. Linux zeroes the interval whenever the value disarms the timer, so a later old-value report and `timer_gettime` then read a zero interval; Detcore stores the requested interval on a disarm and reports it back through both, on every backend. A guest that disarms with a nonzero interval and reads it back observes the stale interval here where Linux reads zero.[^c11][^c9][^linux]
- G7 (unimplemented): in standard record and replay the guest reads the real host clock (time is deliberately not virtualized there)[^c22] while this handler still judges an absolute setting against virtual time, epoch included, and a CPU-clock creation records no CPU clock at all in that mode,[^c23] so an absolute value computed from a host clock read means a different deadline here, and an absolute CPU-clock value becomes an absolute virtual-clock deadline. No refusal guards this mismatch. A guest in that mode has no fallback inside the call.[^c6][^c8][^c15]

## Discussion

[Start a discussion about `timer_settime`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+timer_settime&body=Discussion+for+the+%60timer_settime%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ftimer_settime.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60timer_settime%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Appendix

### Code facts

Detcore classifies `timer_settime` (223) as determinized via typed dispatch; deterministic refusal: false; run subscription: true; record/replay subscription: true.

- Reviewed list: `detcore/src/syscall_classification.rs:1956`
- Dispatch: `detcore/src/lib.rs:8700`

### Tests

The previous arming and remaining time on the virtual clock: `detcore/src/tool_local.rs` (`settime_reports_previous_arming_and_remaining_uses_virtual_clock`, `periodic_remaining_advances_past_each_deadline`) — what Hermit does, and interface fidelity; those two tests exercise store arithmetic only. An absolute CPU expiry served only once passed, and a gone CPU task answered at arming: `detcore/src/cpu_clock/handler_tests.rs` (`an_absolute_cpu_timer_expiration_is_served_only_once_passed`, `timer_settime_on_a_timer_whose_thread_exited_is_esrch`) — interface fidelity. A periodic timer waiting for its take and reporting its overrun: `detcore/src/scheduler/posix_timer_coalesce_tests.rs` (`a_periodic_timer_waits_for_its_dequeue_and_reports_the_overrun`) — what Hermit does. Guest programs posix_timer_test, timer_coalesce, timer_coalesce_signals, guest_discard_at_send, and timer_exec_flush under tests exercise arming, coalescing, an expiry while its signal is ignored, and the exec flush end to end on the default ptrace backend — what Hermit does, and interface fidelity. There is no KVM, DBT, SaBRe, or LiteInst evidence for this page.

### Citations

[^linux]: Linux `timer_settime(2)`:
  syscall entry: https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/time/posix-timers.c?h=v7.1.3#n947
  setting validity judged before the timer lookup: https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/time/posix-timers.c?h=v7.1.3#n912
  what a valid timespec is: https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/include/linux/time64.h?h=v7.1.3#n97
  disarm zeroes the interval: https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/time/posix-timers.c?h=v7.1.3#n863
  expiry queues the signal and stops the timer: https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/time/posix-timers.c?h=v7.1.3#n349
  dequeue re-arms on the grid and counts overrun: https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/time/posix-timers.c?h=v7.1.3#n291
  ignored periodic timer held and queued again on unignore: https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/signal.c?h=v7.1.3#n2086
  CPU-clock arming and its gone-task answer: https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/time/posix-cpu-timers.c?h=v7.1.3#n622
[^c1]: detcore/src/syscall_classification.rs:1956 sha256:f0890d9ce8779d24
[^c2]: detcore/src/lib.rs:8700 sha256:3b24ee012c54f25c
  https://github.com/facebookexperimental/hermit/blob/main/detcore/src/lib.rs#L8700
[^c3]: detcore/src/syscalls/time.rs:1388-1392 sha256:73ccee1fa4223cf9
[^c4]: detcore/src/syscalls/time.rs:133-137 sha256:d55b41da7c0bbbf2
[^c5]: detcore/src/syscalls/time.rs:1419-1436 sha256:072d0aa847365316
[^c6]: detcore/src/syscalls/time.rs:1438-1448 sha256:c1493767e96d2337
[^c7]: detcore/src/syscalls/time.rs:1450-1468 sha256:4a4a1baa288ad740
[^c8]: detcore/src/syscalls/time.rs:1474-1497 sha256:8b1e7bd8cfd081f5
[^c9]: detcore/src/syscalls/time.rs:1499-1505 sha256:c9d47cd2cb283130
[^c10]: detcore/src/syscalls/time.rs:1224-1235 sha256:9bcb2fdbd4eff3f3
[^c11]: detcore/src/tool_local.rs:717-732 sha256:26defb17f17452ee; detcore/src/tool_local.rs:736-742 sha256:9e8f79cee9d1b354
[^c12]: detcore/src/syscalls/time.rs:1623-1642 sha256:d21867875bd5bc5b
[^c13]: detcore/src/scheduler/discarded_at_send.rs:65-67 sha256:31f653c9c9e6d869
[^c14]: detcore/src/scheduler/posix_timer_coalesce.rs:608-628 sha256:11c870b1cb77cd38
[^c15]: detcore/src/scheduler/itimer_coalesce.rs:99-101 sha256:4c81d5eab1a828b8
[^c16]: detcore/src/scheduler.rs:24914-24939 sha256:f4c6815d34473f28
[^c17]: detcore/src/cpu_clock.rs:423-432 sha256:3e04ec0fef678004; detcore/src/cpu_clock.rs:342-352 sha256:780fced58222ccbb
[^c18]: detcore/src/scheduler/parked.rs:1382-1436 sha256:eac2043e985fc94c; detcore/src/scheduler/parked.rs:415-446 sha256:1628e125640b56e5; detcore/src/scheduler/parked.rs:1454-1457 sha256:3213d9e40927465b
[^c19]: detcore/src/scheduler/posix_timer_coalesce.rs:551-560 sha256:f6327a1bc66229dd; detcore/src/scheduler/timed_waiters.rs:681-695 sha256:2f357e9dcb372f34
[^c20]: detcore/src/scheduler/posix_timer_coalesce.rs:280-334 sha256:65dba1a676f80f0a
[^c21]: detcore/src/scheduler/posix_timer_coalesce_tests.rs:283-307 sha256:6d54096ed57c04d0
[^c22]: detcore/src/syscalls/time.rs:546-549 sha256:d5cf774da6e9164b
[^c23]: detcore/src/syscalls/time.rs:1312-1323 sha256:d19ee7b908c7958d
[^c24]: detcore/src/scheduler/posix_timer_coalesce.rs:1434-1436 sha256:802183c317ae38cd
[^c25]: detcore/src/scheduler/parked.rs:976-993 sha256:556774bd1dbee847; detcore/src/scheduler/parked.rs:1130-1137 sha256:5b579e88b2f0f693; detcore/src/scheduler.rs:2462-2474 sha256:346cac95193e8a4e
[^c26]: detcore/src/scheduler/posix_timer_coalesce.rs:563-566 sha256:b3dfbb5a06df8daf; detcore/src/scheduler/posix_timer_coalesce.rs:673-684 sha256:bd3e5fa4ff6502d7

## Status

- 2026-11-15 · rev 54bd03547df0 · facts-only page generated from Detcore code facts at this revision.
- 2026-11-16 · rev 7ac736d54e14 · facts refreshed from Detcore at this revision.
- 2026-11-18 · rev 403e350e9e82 · facts refreshed from Detcore at this revision.
- 2026-11-20 · rev 403e350e9e82 · checked determinism-strategy prose added (rubric v3); citations verified at this revision.
