---
schema: syscall-pages/v3
syscall: rt_sigprocmask
nr_x86_64: 14
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+rt_sigprocmask&body=Discussion+for+the+%60rt_sigprocmask%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Frt_sigprocmask.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60rt_sigprocmask%60%2C+or+in+what+the+page+says%3F%0A
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

Natively, whether a signal is delivered at once or held pending depends on the mask in force at the host-chosen moment it arrives, so a mask change racing a send changes the execution. Hermit runs each mask change in the calling thread's own scheduled turn and, where the kernel's signal state is the guest's own with serialized threads, mirrors it in a per-thread copy that the scheduler consults in place of the kernel's mask for some delivery decisions, so the mask at every point of a repeated schedule is the same. That copy is checked against the kernel after every change and given up, never repaired, if the two ever disagree. Known gaps: 5.

## Determinism strategy

**What is nondeterministic natively.** `rt_sigprocmask` reads and changes the calling thread's blocked-signal mask, the gate that decides for every later send whether its signal runs a handler at once, kills or stops the thread, is discarded on arrival because it is ignored and unblocked, or waits pending until unblocked, so the mask's value at each instant perturbs delivery order, interruption of blocking calls, and which thread of a process takes a process-directed signal.[^linux] Natively that value races every concurrent sender: the same send is delivered or held pending according to host timing of the mask change against the send, and a signal unblocked while pending becomes deliverable at a host-chosen point in the receiver's execution.[^linux]

**What Hermit does.** Detcore determinizes `rt_sigprocmask` by keeping the kernel's mask and its own copy in lockstep at scheduler turns: typed dispatch reaches a handler that validates the set size before touching either user pointer, runs the change itself in the caller's turn, and then records exactly that committed change in the thread's signal-state copy, which is seeded from the kernel at exec, published to the scheduler before another thread may run, and withdrawn whenever its value is uncertain.[^c1][^c2][^c4][^c15][^c14] For the two operations that add or replace blocked signals, the caller's set is first read through the kernel itself — a probe call with an invalid operation, which copies the set and changes nothing, so an unreadable set faults exactly as the real call would — then stripped of Hermit's reserved preemption signal (G1), staged on the guest stack, and injected with tracer preemptions retried out of the guest's sight; an unblock, a query with no set, and any other operation value are injected as written and their set, where the shadow still needs it, is read back afterward from the memory the kernel just read.[^c6][^c5][^c17] The copy applies the change under the same rules the kernel does, clearing the two unblockable signals from every set and from the result, predicts the old mask, and compares that prediction with the old mask the kernel actually wrote back; a difference the copy cannot explain drops the copy rather than repairing it from the kernel, because the check exists to expose exactly such a defect, and marks that thread's copy never to be seeded again (G2).[^c10][^c9][^c18] The recorded mask is the time-dependent gate the rest of the signal machinery reads: a change that may unblock signals makes observable any pending real-time timer expiration every thread had blocked that the new mask unblocks, which ends the run with a named refusal rather than delivering it, while an ignored expiration is discarded as Linux discards it (G4), and where the backend virtualizes the guest's alarm signal and the process handles that signal the scheduler's blocked bit is republished from the new mask whatever the call returned, since a fault writing the old mask still comes after the change — all still inside the caller's turn, before the schedule moves on.[^c11][^c19]

**Interface fidelity.** The size argument must equal the kernel's raw mask size and is refused with `EINVAL` before either pointer is read, matching the kernel's first check.[^c3][^c4][^linux] With a set, block adds its signals and set-mask replaces the mask, each honoring the caller's set as read through the kernel probe above (less the reserved signal, G1), while unblock removes its signals and runs as written; without a set the operation value is ignored and the call only reports the old mask, while an unknown operation value with a set is answered by the kernel's own `EINVAL` on the injected call, the same answer the copy gives for it.[^c5][^c10][^linux] The unblockable signals `SIGKILL` and `SIGSTOP` are silently dropped from any set, by the kernel on the real mask, by the installed-mask rule for temporary sleep masks, and by the copy on its mask, so no readback ever shows either as blocked.[^c13][^c10][^linux] The old mask written back is the mask from before the change, and the fault split keeps the kernel's order: a set that cannot be copied in fails with `EFAULT` and changes nothing, while a fault copying the old mask out fails with `EFAULT` after the change has committed, which the copy records as a committed change for a set Detcore validated itself and otherwise treats as unknowable (G2).[^c8][^linux] The call never blocks and there is no `EINTR` result: when the entry probe is stopped before it runs by a signal held at entry, the call instead restarts after that signal, as if it had arrived just before the call, rather than failing a call Linux never fails that way.[^c12]

**Record & replay.** The call is subscribed in both run and record/replay modes (code facts in the Appendix).[^c2] Nothing about the answer needs a recorded mask value: the size refusal is regenerated from the argument alone, and the copy is updated only from the set Detcore validated or read back and from the call's own result, so replay reaching the same call at the same schedule point rebuilds the same mask and the same published gate from the same inputs.[^c4][^c17][^c8] The signal-state copy's bookkeeping adds no guest-visible step, because the shadow update runs after the guest operation commits and reads only the written old mask and the thread's kernel signal state read from `/proc`, and issues no injection and no scheduler request of its own.[^c18]

## Known gaps

- G1 (by design): the guest cannot block Hermit's reserved preemption signal through this call (or through a handler's mask): its bit is removed from the set of every block or set-mask call before the kernel sees it, and left out of every comparison, so a guest that asks to block it reads back a mask without it and the signal stays deliverable to the instrumentation. There is no guest fallback; the preemption the signal drives is what makes repeated schedules repeat.[^c7][^c5]
- G2 (unimplemented): the mask copy is given up whenever the change or its check cannot be completed — a set the old mask overwrote or that cannot be read back, an old mask that cannot be read back, an `EFAULT` that may belong to either pointer, a failure without an errno, kernel state that cannot be read, or a difference only a forced delivery explains — and a copy dropped that quietly is seeded again only at the thread's next exec, where the new image's mask describes the whole state, while a difference that neither the change nor a forced delivery explains, in the written old mask or in the post-call check, marks it diverged and it is never seeded again for that thread. The guest's real mask is unaffected and this call keeps working through the kernel; until any reseeding the published mask is withdrawn and later decisions read the kernel's own blocked mask as they did before the copy existed, refusing only if that read itself fails, and a guest's own process-directed send never reads the copy at all.[^c8][^c9][^c15][^c20][^c21]
- G3 (unimplemented): the copy is seeded from the kernel and checked against it only where the backend's kernel signal state is the guest's own and threads are serialized; on any other backend, or with the copy switched off, mask changes still run in turn through the kernel, the in-guest runtime, or on KVM its own syscall implementation, but no shadow follows them, because those backends do not yet report a first image's inherited state, and a divergence between what Detcore believes and what the kernel holds cannot be detected there.[^c16][^c15]
- G4 (unimplemented): Detcore does not deliver a real-time signal from a timer: a mask change that lets the guest observe a pending real-time timer expiration ends the run with a named refusal rather than dropping the expiration, while an ignored expiration is discarded as Linux discards it; a guest that blocks such a signal, lets its timer expire, and unblocks it has no fallback inside the call.[^c19]
- G5 (unimplemented): on the block and set-mask path, if the staged set cannot be committed to the guest stack the call returns that failure's errno without running, a failure that leaves the mask unchanged, which Linux never gives for a readable set; this happens only when the guest stack has no writable room below the red zone.[^c22]

## Discussion

[Start a discussion about `rt_sigprocmask`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+rt_sigprocmask&body=Discussion+for+the+%60rt_sigprocmask%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Frt_sigprocmask.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60rt_sigprocmask%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Appendix

### Code facts

Detcore classifies `rt_sigprocmask` (14) as determinized via typed dispatch; deterministic refusal: false; run subscription: true; record/replay subscription: true.

- Reviewed list: `detcore/src/syscall_classification.rs:1869`
- Dispatch: `detcore/src/lib.rs:8662`

### Tests

The size rule itself, and the reserved bit leaving only the kernel-sized mask: `detcore/src/syscalls/signal.rs` (`raw_signal_size_validation_rejects_before_pointer_processing`, `reserved_signal_is_removed_from_the_kernel_sized_mask_only`) — what Hermit does, and interface fidelity. The installed mask never blocking the unblockable pair: `detcore/src/syscalls/signal.rs` (`the_installed_mask_cannot_block_kill_or_stop`) — interface fidelity. The copy returning the old mask and never holding the kill signal: `detcore/src/signal_state.rs` (`sigprocmask_returns_the_old_mask_and_never_blocks_sigkill`) — what Hermit does, and interface fidelity. Recording and checking a committed change, the old-mask fault split, and divergence on a differently written old mask: `detcore/src/syscalls/signal_shadow.rs` (`a_matching_mask_change_is_recorded_and_checked`, `an_efault_counts_only_when_the_kernel_ran_the_call_and_the_fault_was_the_old_masks`, `an_old_mask_the_kernel_wrote_differently_is_a_divergence`) — what Hermit does, and interface fidelity. The guest program signal_delivery_check under tests/c exercises masked delivery end to end — what Hermit does.

### Citations

[^linux]: Linux `rt_sigprocmask(2)`:
  syscall entry, size check first: https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/signal.c?h=v7.1.3#n3320
  mask update rules: https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/signal.c?h=v7.1.3#n3237
  unblockable signals removed in the syscall body: https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/signal.c?h=v7.1.3#n3335
  installed temporary masks (`set_current_blocked`): https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/signal.c?h=v7.1.3#n3207
  discarded on arrival because ignored and unblocked (`sig_ignored`): https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/signal.c?h=v7.1.3#n106
  receiving-thread choice: https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/signal.c?h=v7.1.3#n963
[^c1]: detcore/src/syscall_classification.rs:1869 sha256:6daacb4c1e64a08a
[^c2]: detcore/src/lib.rs:8662 sha256:3ec47a3ede9f5600
  https://github.com/facebookexperimental/hermit/blob/main/detcore/src/lib.rs#L8662
[^c3]: detcore/src/syscalls/signal.rs:152-162 sha256:fd146252f5e53e8d
[^c4]: detcore/src/syscalls/signal.rs:2013-2024 sha256:8d5467c2f30b9abb
[^c5]: detcore/src/syscalls/signal.rs:2104-2143 sha256:1b1873f69f1e1a52
[^c6]: detcore/src/syscalls/signal.rs:210-218 sha256:c2df5b1e71ff993d; detcore/src/syscalls/signal.rs:227-250 sha256:5e45ad9c25259098
[^c7]: detcore/src/syscalls/signal.rs:172-175 sha256:e7879b9c32d27293; detcore/src/syscalls/signal.rs:1796 sha256:1db835762da3e746; detcore/src/syscalls/signal.rs:1810 sha256:1af3e5cdcded8d70
[^c8]: detcore/src/syscalls/signal_shadow.rs:654-687 sha256:c9563ebbbbe7897f
[^c9]: detcore/src/syscalls/signal_shadow.rs:687-728 sha256:4bb01f08e27b0506; detcore/src/syscalls/signal.rs:2031-2036 sha256:8a74b3a0604c5e70; detcore/src/syscalls/signal_shadow.rs:2010-2025 sha256:a86631ae1d03f8b4
[^c10]: detcore/src/signal_state.rs:107 sha256:3c48f2ddc88bd13d; detcore/src/signal_state.rs:326-341 sha256:12d56a227925ffb0
[^c11]: detcore/src/syscalls/signal.rs:2079-2101 sha256:35c0f3eb1c1a0384
[^c12]: detcore/src/syscalls/signal.rs:177-189 sha256:15eee60a81dfc35c
[^c13]: detcore/src/syscalls/signal.rs:164-170 sha256:75e9258912cf387d
[^c14]: detcore/src/syscalls/signal_shadow.rs:1722-1733 sha256:545468a9047ed8d5; detcore/src/syscalls/signal_shadow.rs:1734-1765 sha256:cd0387794b7116c7
[^c15]: detcore/src/syscalls/signal_shadow.rs:523-537 sha256:11fc88988f3fe6a5
[^c16]: detcore/src/syscalls/signal_shadow.rs:142-160 sha256:4d1f875965bb819f; detcore/src/signal_state.rs:24-27 sha256:0fdd05359290c5a5
[^c17]: detcore/src/syscalls/signal_shadow.rs:624-640 sha256:17b1b841a88bb25c
[^c18]: detcore/src/syscalls/signal_shadow.rs:9-28 sha256:dd50e08daf7b9dfd
[^c19]: detcore/src/syscalls/helpers.rs:6645-6672 sha256:d06fa2d60a11641b; detcore/src/scheduler.rs:4908-4912 sha256:39dbf507f464b30e
[^c20]: detcore/src/scheduler.rs:10494-10506 sha256:ae4b76726c344b57; detcore/src/scheduler.rs:17342-17355 sha256:bc26427073627fca; detcore/src/scheduler.rs:10611-10636 sha256:18b6463252208521
[^c21]: detcore/src/syscalls/signal_shadow.rs:104-129 sha256:aa4e1ccc12f9d572; detcore/src/syscalls/signal_shadow.rs:1960-1975 sha256:7cdfe3cbdb7641ab
[^c22]: detcore/src/syscalls/signal.rs:2121-2126 sha256:1d4caa2a4882a06e

## Status

- 2026-11-15 · rev 54bd03547df0 · facts-only page generated from Detcore code facts at this revision.
- 2026-11-16 · rev 7ac736d54e14 · facts refreshed from Detcore at this revision.
- 2026-11-18 · rev 403e350e9e82 · facts refreshed from Detcore at this revision.
- 2026-11-20 · rev 403e350e9e82 · checked determinism-strategy prose added (rubric v3); citations verified at this revision.
- 2026-11-20 · rev 403e350e9e82 · prose corrected after seat review; citations re-verified at this revision.
