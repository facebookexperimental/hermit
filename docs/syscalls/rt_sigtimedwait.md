---
schema: syscall-pages/v3
syscall: rt_sigtimedwait
nr_x86_64: 128
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+rt_sigtimedwait&body=Discussion+for+the+%60rt_sigtimedwait%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Frt_sigtimedwait.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60rt_sigtimedwait%60%2C+or+in+what+the+page+says%3F%0A
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

Natively, `rt_sigtimedwait` dequeues a pending signal or sleeps until one arrives, its timeout expires on host time, or another signal interrupts it, so which signal it returns and when follow host scheduling. Hermit instead turns the wait into zero-timeout probes at scheduler turns against a logical deadline and, where its signal-target model is in force, reconciles each take with its own pending-signal records, so a repeated schedule dequeues the same entry with the same siginfo while the dequeue order itself remains the serving backend's. Known gaps: 7.

## Determinism strategy

**What is nondeterministic natively.** `rt_sigtimedwait` synchronously takes one signal from the caller's private queue or its process's shared queue, first trying to dequeue at once and otherwise sleeping with the waited set unblocked until a waited signal arrives, the timeout expires, or an interrupting signal ends the sleep, so the returned signal, the siginfo written, and whether the call blocks at all depend on when senders, timers, and the host clock happen to run.[^linux] Which entry is taken also depends on queueing: a second ordinary send of a standard signal that is already pending adds no entry, while realtime sends queue in order behind one another.[^linux]

**What Hermit does.** Detcore determinizes `rt_sigtimedwait` by never letting one host sleep decide it: typed dispatch reaches a handler that converts the call into repeated zero-timeout probes issued at scheduler turns, under a deadline read from logical time when the guest gave a timeout and no deadline when it gave none.[^c1][^c2][^c3][^c6][^c7] Each probe is the guest's own call as the backend serves it. On a backend that serves the probe in the host kernel, the dequeue itself — the private queue before the shared one and, within a queue, Linux's own signal order (synchronous signals first), with entries of one signal in the order they were queued — is performed by Linux rather than modeled; on KVM the backend's own implementation performs the dequeue, documented as following Linux's order with the private queue first, and Detcore neither models nor checks it.[^c6][^linux] What Detcore models around that dequeue, only where its gates are in force, is which physical copy may stand for which logical send for standard signals: take reconciliation needs the discarded-at-send record, which exists only where signal targets are modeled (ptrace, `e9patch` and KVM with sequentialized threads; the cited comment in `[^c15]` predates KVM's predictable-target capability), and the logical lists further need timer coalescing, which excludes record and replay and any backend with installed signal control, plus an observable check for the process and signal concerned, other pairs taking the fallback (G6).[^c15][^c16] Where those gates hold, ordinary sends into a non-empty logical list add nothing, timers queue behind an ordinary front, and taking the front promotes and re-sends the next entry, so a merged physical copy cannot deliver two logical sends or lose the one behind it.[^c8][^c9][^c12] The handler also publishes the waited set to the scheduler for the duration of the wait and withdraws it when the call returns, so that where the scheduler itself chooses which thread takes a kernel-generated process-directed signal this waiter counts as wanting a waited signal even where its ordinary mask blocks it, as Linux unblocks the waited set for the wait.[^c4][^c11][^linux] For a guest's own process-directed send the published set is only an input to the sender-side admission decision: on ptrace and `e9patch` a waiter whose set covers the signal makes that send refused by name rather than delivered to the waiter, except for a signal left at its default disposition whose default action ends the process — the refusal the kill page numbers as its G2.[^c17] Where reconciliation applies, a take the scheduler's records say Linux would never have queued — for a standard signal a copy discarded when it was sent (G7), a child-exit copy a delivered notification already stood for (only on a backend that reports child-exit publication), or a stale timer entry whose timer was set again or deleted after it queued — is not returned: the bytes the probe wrote into the siginfo buffer are put back and the wait continues to the same deadline.[^c5][^c13][^c18]

**Interface fidelity.** The mask width is rejected with `EINVAL` before any guest pointer is inspected, matching the kernel entry order, and an unreadable waited set is left for the first kernel probe to fail as Linux fails its copy-in.[^c14][^c3][^c6][^linux] The timeout keeps its Linux shapes except as noted in the gaps: no timeout pointer waits without a deadline, a zero timeout probes until a take stands or a probe would block (a skipped take probes again, with the answer a guest sees matching Linux), and expiry returns `EAGAIN` from the probe that would have blocked.[^c3][^c6] A signal the caller does not block, outside the waited set that the wait may not consume, ends it with `EINTR` and the call is never restarted after a handler, while signals in the waited set are consumed by the wait rather than interrupting it.[^c6][^c7][^linux] The siginfo returned is the kernel's own copy-out from the successful probe, including its partial-write behaviour when the buffer faults, except in three cases where Detcore writes a siginfo of its own over the buffer: when the logical entry taken stands on another sender's physical record or is a timer entry, in the kernel's layout with the bytes past the kernel-sized prefix zeroed, and, on a backend that reports child-exit publication, for a consumed child-exit signal whose own copy stands, where the siginfo Linux gives the parent is written instead.[^c5][^c10][^c19][^linux] When that copy-out fails after a dequeue, the kernel has still consumed the signal, so the failure stands as Linux's does unless the scheduler's records say the consumed copy was one Linux had discarded at send time, or a stale timer entry, in which case the wait resumes instead of reporting it; where that fault leaves two or more listed entries out of their queues the whole run is refused by name instead.[^c13][^c18][^linux]

**Record & replay.** The call is subscribed in both run and record/replay modes (code facts in the Appendix).[^c2] No recorded answer is substituted for it: signals are not recorded, the handler is not passed to the record/replay layer, and replay reaches the same zero-timeout probes at the same schedule points against the same logical deadline, reconciling each take with the discarded-at-send record and, on a backend that reports child-exit publication, the child-exit ledger, but keeping no timer-entry lists (G5).[^c3][^c7][^c20][^c19][^c18]

## Known gaps

- G1 (unimplemented): a timeout whose second count is negative or whose nanosecond count is out of range is not rejected with `EINVAL` as Linux rejects it before the first dequeue: the handler reads both fields as unsigned durations and adds them to logical time, so such a call waits for a converted interval (a negative second count becomes an effectively unbounded one) instead of failing at once — except where the two converted durations overflow when added, as with `{tv_sec=-1, tv_nsec=1000000000}`, where the handler panics instead of waiting — and no guest fallback recovers the error.[^c3][^linux]
- G2 (unimplemented): the waited set is not snapshotted at entry as Linux snapshots it: each retry injects the call from the caller's pointer again, so another thread that rewrites the set while the call waits changes which signals later probes may take, although the set the scheduler routes by still reflects the entry read.[^c3][^c6]
- G3 (unimplemented): when a taken entry's own siginfo cannot be written over the physical record the probe returned — the timer and proxy overwrites, and likewise the child-exit overwrite for a consumed child-exit signal — the failure is only logged and the guest keeps the physical sender's siginfo for a signal whose logical sender was another; this needs the buffer to become unwritable between the kernel's copy-out and the rewrite, which the handler treats as not expected.[^c5][^c19]
- G4 (by design): a signal sent from outside the guest is not made deterministic: Hermit fixes the turns at which probes run, but not when an outside sender acts, so which probe dequeues an externally sent signal can still follow its host arrival.[^c7]
- G5 (unimplemented): in record and replay no timer-entry lists are kept, so a timer's expiry can merge into a pending ordinary copy of the same standard signal and a take sees one entry where Linux holds two, without the timer's own siginfo; the logical-list guarantee in the strategy above therefore holds only outside record and replay.[^c20][^c16]
- G6 (unimplemented): where a reconciliation gate is off a take is only partly reconciled: with no modeled signal targets there is no discarded-at-send reconciliation and no logical lists; with no timer coalescing, or an unobservable process and signal pair, a timer entry can merge and be lost, while discarded-at-send reconciliation still applies where the record exists; and with no child-exit publication a duplicate child-exit copy is returned as the backend gives it.[^c15][^c16][^c18]
- G7 (unimplemented): the discarded-at-send record tracks standard signals only, so an unblocked ignored real-time signal sent to a waiter can be queued under a tracer and returned by a probe where native Linux discards it at send and the wait would time out with `EAGAIN`.[^c21]

## Discussion

[Start a discussion about `rt_sigtimedwait`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+rt_sigtimedwait&body=Discussion+for+the+%60rt_sigtimedwait%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Frt_sigtimedwait.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60rt_sigtimedwait%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Appendix

### Code facts

Detcore classifies `rt_sigtimedwait` (128) as determinized via typed dispatch; deterministic refusal: false; run subscription: true; record/replay subscription: true.

- Reviewed list: `detcore/src/syscall_classification.rs:1872`
- Dispatch: `detcore/src/lib.rs:8569`

### Tests

Restart behaviour for interrupted waits: `detcore/src/syscalls/helpers.rs` (`kernel_restart_errno_matches_linux_restart_policy`) — interface fidelity. Logical pending entries, their coalescing, promotion, and timer siginfo layout: `detcore/src/scheduler/signal_lists.rs` (`an_ordinary_send_into_a_non_empty_list_adds_nothing`, `a_timer_goes_behind_an_ordinary_front_and_is_promoted_at_its_dequeue`, `the_timer_siginfo_matches_the_kernel_layout`) — what Hermit does, and interface fidelity. A thread waiting for a signal counting as wanting it in the scheduler's receiver choice for kernel-generated signals, and the own-process admission inputs reading each sibling (including publish and withdraw of the waited set): `detcore/src/scheduler.rs` (`a_thread_waiting_for_the_signal_in_sigtimedwait_wants_it`, `own_process_signal_inputs_read_each_sibling_from_the_schedule`) — what Hermit does. Guest programs sigtimedwait-no-timeout, timer_coalesce_signals, guest_discard_at_send, and record_replay_sigtimedwait_child_exit under tests/c exercise the probed wait, its deadline, and dropped-copy takes end to end — what Hermit does; of these programs only record_replay_sigtimedwait_child_exit exercises record and replay (its deadline, timeout, and interruption by a caught child-exit signal) — record & replay.

### Citations

[^linux]: Linux `rt_sigtimedwait(2)`:
  entry and siginfo copy-out: https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/signal.c?h=v7.1.3#n3806
  the wait itself: https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/signal.c?h=v7.1.3#n3747
  dequeue order: https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/signal.c?h=v7.1.3#n618
  standard-signal coalescing: https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/signal.c?h=v7.1.3#n1035
  siginfo copy-out layout: https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/signal.c?h=v7.1.3#n3502
[^c1]: detcore/src/syscall_classification.rs:1872 sha256:a1738b27bc194362
[^c2]: detcore/src/lib.rs:8569 sha256:71727a8625cc4caf
[^c3]: detcore/src/syscalls/signal.rs:2145-2172 sha256:71c083590e6d5f21
[^c4]: detcore/src/syscalls/signal.rs:2173-2201 sha256:02d063baa2a194e5
[^c5]: detcore/src/syscalls/signal.rs:2196-2268 sha256:0080e98c8169b9ae
[^c6]: detcore/src/syscalls/helpers.rs:2604-2640 sha256:be37a29b5b51e409; detcore/src/syscalls/helpers.rs:8920-8923 sha256:eeba0761ea81ca22
[^c7]: detcore/src/syscalls/helpers.rs:3313-3342 sha256:fe8bc74a51d14add
[^c8]: detcore/src/scheduler/signal_lists.rs:9-19 sha256:fde13877699cb4ce
[^c9]: detcore/src/scheduler/signal_lists.rs:164-201 sha256:9d8fe2fec4d27389; detcore/src/scheduler/signal_lists.rs:276-300 sha256:25574adcd9506ffa
[^c10]: detcore/src/scheduler/signal_lists.rs:480-507 sha256:7158e797758965b3
[^c11]: detcore/src/scheduler.rs:10526-10547 sha256:4d002cff84b5e8ea; detcore/src/scheduler.rs:17034-17059 sha256:7d0108cf9d5f6e4c; detcore/src/scheduler.rs:10611-10636 sha256:18b6463252208521
[^c12]: detcore/src/syscalls/helpers.rs:2471-2500 sha256:06f40f30e5306c50
[^c13]: detcore/src/syscalls/signal.rs:2270-2313 sha256:f665b8671f73cac5
[^c14]: detcore/src/syscalls/signal.rs:156-162 sha256:f9137ce7661dfcd0
[^c15]: detcore/src/syscalls/signal.rs:2198-2207 sha256:b4b6960c65267ed5; detcore/src/scheduler/discarded_at_send.rs:61-67 sha256:d8cb357a8b1cd7c9
[^c16]: detcore/src/scheduler/posix_timer_coalesce.rs:934-944 sha256:9ca582697b9de197; detcore/src/scheduler/itimer_coalesce.rs:94-101 sha256:2ace4e69b224a523; detcore/src/scheduler/posix_timer_coalesce.rs:236-257 sha256:76c452c2b5de54be
[^c17]: detcore/src/syscalls/signal.rs:3081-3083 sha256:fdec0851aecc83bf; detcore/src/scheduler/process_signal_receiver.rs:327-341 sha256:b001f15edc809572; detcore/src/syscalls/signal.rs:3581-3585 sha256:77be11ee0d10e522; detcore/src/scheduler/process_signal_receiver.rs:272-315 sha256:f00694db1956697d
[^c18]: detcore/src/scheduler/posix_timer_coalesce.rs:971-977 sha256:967374b3ed11a4f9; detcore/src/scheduler/posix_timer_coalesce.rs:1040-1054 sha256:98859cc2c78bc543; detcore/src/scheduler.rs:4896-4906 sha256:60afe1b58ffb25ed
[^c19]: detcore/src/syscalls/signal.rs:2431-2437 sha256:c32659c802f8e48a; detcore/src/syscalls/signal.rs:2506-2536 sha256:f975f1f9cefd425a
[^c20]: detcore/src/scheduler/posix_timer_coalesce.rs:939-944 sha256:6ad89571ad17382f; detcore/src/scheduler/posix_timer_coalesce.rs:18-30 sha256:f50f5afe9136f119
[^c21]: detcore/src/scheduler/discarded_at_send.rs:40-42 sha256:de2b22decc0c39b5; detcore/src/syscalls/signal.rs:2282 sha256:ef326ab6c328829f

## Status

- 2026-11-15 · rev 54bd03547df0 · facts-only page generated from Detcore code facts at this revision.
- 2026-11-16 · rev 7ac736d54e14 · facts refreshed from Detcore at this revision.
- 2026-11-18 · rev 403e350e9e82 · facts refreshed from Detcore at this revision.
- 2026-11-20 · rev 403e350e9e82 · checked determinism-strategy prose added (rubric v3); citations verified at this revision.
- 2026-11-20 · rev 403e350e9e82 · prose corrected after seat review; citations re-verified at this revision.
