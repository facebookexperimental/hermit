---
schema: syscall-pages/v3
syscall: wait4
nr_x86_64: 61
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+wait4&body=Discussion+for+the+%60wait4%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fwait4.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60wait4%60%2C+or+in+what+the+page+says%3F%0A
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

Natively, `wait4` reaps a child whose exit the host kernel has already processed or sleeps until host scheduling makes one ready, so the returned child, its status timing, and its resource usage follow the host. Hermit instead decides readiness for its modeled class from its own record of which children have logically exited at scheduler turns, then runs an exact-child kernel wait for the child it selected and zeroes the resource usage; within that class the per-turn probe or the re-issued DBT wait can still return a kernel-chosen child (G5, G7), and outside that class the answer comes from kernel probes or the backend, as the gaps state. Known gaps: 8.

## Determinism strategy

**What is nondeterministic natively.** `wait4` reports and usually reaps one child that has exited, stopped, or continued, selected by the pid argument and the option bits, and blocks when no eligible child has such an event yet, so which child it returns, whether it blocks, and the resource usage it reports all depend on when children happen to change state on the host.[^linux] A signal can also end the wait instead, at a moment host delivery chooses.[^linux]

**What Hermit does.** Detcore determinizes `wait4` for terminal child events by deciding readiness in its deterministic scheduler rather than sleeping in one host wait (the re-issued DBT wait in G5 excepted): typed dispatch reaches a handler that turns the pid argument into an exact, any-child, or process-group selector and asks the scheduler which matching child has logically exited, offering them in ascending pid order.[^c1][^c2][^c6][^c12] The selected child is then reaped by an exact-child form of the guest's own call, so the returned pid and status word are the kernel's for that child — or, on a backend that emulates child waits, the backend's, which first waits for that child's final accounting to be published and then consumes it exactly once.[^c7][^c17] Eligible children are partitioned by their recorded exit signal as Linux partitions them for children the caller does not trace — an ordinary wait matches only children whose exit signal is the usual child signal, the clone form only the others, and the all form both — with that signal following the creating clone, its creator for a parent-cloned child, and a successful exec, although the modeled selector is only fed the ordinary class and the other classes stay on the kernel-probed path below; the no-other-thread option is modeled inside that selector, restricting a wait to children whose parent thread (Linux's real parent, which moves to a surviving thread when the creator exits or execs, and which a parent-cloned child inherits from its creator) is the caller.[^c4][^c11][^c13][^c19][^linux] A blocking wait with no ready child parks as a scheduler child wait and retries at child-lifecycle turns, and on a backend that does not emulate child waits with sequentialized threads each such turn also injects a no-hang wait with the guest's own selector and returns any child the kernel reports, before the pending signal is considered; a modeled-class wait with no child known to the scheduler on a backend without complete child lineage skips parking and goes straight to those kernel probes, as does every wait outside the modeled class (G7).[^c8][^c10][^c20] Thread exits are not reaped by this call at all: a joined thread's exit clears its tid word and wakes one futex waiter on it, which Detcore models as an exit-to-join wake admitted at the next deterministic drain, in Linux's order after the dying owner's priority-inheritance releases.[^c16][^linux]

**Interface fidelity.** Argument validation follows the kernel's order: an unknown option bit fails with `EINVAL` before the pid selector is interpreted, and the smallest pid value, whose negation is not representable, then fails with `ESRCH` before any child is looked for.[^c3][^linux] With the no-hang option the handler takes one scheduler turn and then answers from its record — a ready child is reaped, an eligible child that has not exited gives a zero return without any kernel wait, and no eligible child gives `ECHILD` where the backend tracks every child, with a one-shot kernel wait left to answer where it does not.[^c7][^linux] A ready child wins over an interrupting signal, as on Linux, where the child scan runs before the pending-signal break: the managed wait latches the signal's disposition but reports the child first, and only when no child is ready — or remains ready, after a stale selection is dropped — does it return the restart form that a handler turns into `EINTR`, or a restart under `SA_RESTART`; on the backend that needs thread-directed process signals (DBT) that point instead inspects the signal's action — `EINTR` directly for an interrupting disposition, and for a restartable one the guest's own blocking call is re-issued to the host kernel, which then chooses the child and the moment and reports its real resource usage, Detcore's handler never resuming (G5).[^c8][^c14][^c15][^linux] Stop and continue reports, and the clone and all option classes, are not modeled selections on the kernel-probed path: they run as the guest's own call converted to its no-hang form at scheduler turns, so their event words and errors are the kernel's for each probe.[^c5][^c10] On a backend that emulates child waits with sequentialized threads the split is different: the terminal subset of a stop-reporting wait, with or without the no-other-thread option, does select a logical child through that same modeled selector, which enforces the no-other-thread restriction by its owner filter before the exact-child backend call, while a wait asking for continued children, the clone class, or the all class gets no selector, goes to the backend as a no-hang call, and there fails with `EINVAL` where Linux serves it (G6).[^c5][^c4][^c11][^c21] On success the status word is left exactly as the reaping call wrote it, while a requested resource-usage buffer is overwritten with zeros on every path that returns through Detcore's handler, the kernel-probed path included, because host CPU and scheduling counters are not deterministic (G1 and G8, with the re-issued DBT wait in G5 the exception); a caller in a guest-created pid namespace is translated for an exact child, while its process-group waits, and any emulated wait from such a namespace, are refused by name instead (G2).[^c9][^c6]

**Record & replay.** The call is subscribed in both run and record/replay modes (code facts in the Appendix).[^c2] It is handled by the scheduler and not passed to the record/replay layer, so on the managed path replay re-derives the same ready child from the same scheduled exits and runs the same exact-child reap rather than substituting a recorded result, except where the per-turn probe or the re-issued DBT wait returns a kernel-chosen child (G5, G7), which replay reruns live; on the kernel-probed path replay simply reruns the probes (G3).[^c6][^c9]

## Known gaps

- G1 (by design): on every path that returns through Detcore's handler (the re-issued DBT wait in G5 excepted), a successful wait's resource usage reads as zeros for every field other than the CPU times in G8 (the page-fault, context-switch, block-I/O and peak-resident-set counts), because the host counters Linux reports for them are not a function of the schedule and those fields have no logical counterpart to report in their place. A guest that needs real usage has no fallback inside this call.[^c9][^c22][^linux]
- G2 (unimplemented): a process-group wait from a guest-created pid namespace, and any wait from such a namespace on a backend that emulates child waits, is refused by name with `ENOSYS` where the run allows unsupported operations and no guest fallback, although Linux serves both forms.[^c6][^c18]
- G3 (unimplemented): stop, continue, clone-class, and all-class waits on the kernel-probed path are answered by kernel probes rather than the scheduler's lifecycle record, pending a backend waitability callback and a backend distinction between process and thread clones, so an event only the host kernel or tracer can produce is observed at the first probe after the host makes it, not at a modeled schedule point; on a backend that emulates child waits with sequentialized threads only the terminal subset of a stop-reporting wait is modeled instead, as the fidelity part describes.[^c5][^c10][^c23]
- G4 (unimplemented): when several modeled children are ready at once the scheduler offers the lowest pid first, while Linux scans the calling thread's own children first and then each other thread's, each list in the order its children were appended, with a dying thread's children spliced onto its heir's tail; the orders can therefore differ even while pids are assigned in creation order, for a multithreaded parent whose threads created different children and for adopted children, and no test named below separates them.[^c12][^linux]
- G5 (unimplemented): on DBT, after a restartable signal arrives with no selected child ready, the wait becomes the host kernel's own blocking wait with the guest's original arguments: the returned child and its timing are the host kernel's choice, the scheduler's lifecycle consumption never runs, and the resource usage is the kernel's real counters rather than the zeros in G1 and G8.[^c14]
- G6 (unimplemented): on the emulating backend (KVM), a wait asking for continued children, the clone class, or the all class fails with `EINVAL` where Linux serves it, because the backend's own wait accepts only the no-hang and stop-reporting bits; the guest sees an error Linux never gives for those bits.[^c21][^linux]
- G7 (unimplemented): in the modeled class itself, the per-turn no-hang kernel probe with the guest's own selector can return a kernel-chosen child, and on a backend without complete child lineage a blocking wait with no child known to the scheduler goes straight to such probes, so that child and the moment it is reported follow host timing rather than the scheduler's selection.[^c20]
- G8 (unimplemented): on every path that returns through Detcore's handler (the re-issued DBT wait in G5 excepted), a successful wait's `ru_utime` and `ru_stime` read as zeros, not the child's logical CPU time, although for a terminal report that time has a logical counterpart the same reap folds into the totals that `times` and `getrusage` for reaped children do report, where natively the two agree (Linux fills the same fields for stop and continue reports too, where nothing is reaped); a guest that needs the child's CPU time from this call has no fallback inside it.[^c9][^c22][^linux]

## Discussion

[Start a discussion about `wait4`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+wait4&body=Discussion+for+the+%60wait4%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fwait4.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60wait4%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Appendix

### Code facts

Detcore classifies `wait4` (61) as determinized via typed dispatch; deterministic refusal: false; run subscription: true; record/replay subscription: true.

- Reviewed list: `detcore/src/syscall_classification.rs:1991`
- Dispatch: `detcore/src/lib.rs:8219`

### Tests

Argument validation in Linux's precedence: `detcore/src/syscalls/threads.rs` (`wait4_argument_validation_follows_linux_precedence`) — interface fidelity. Keeping a pending interrupt behind another ready child: `detcore/src/syscalls/threads.rs` (`stale_any_child_preserves_interrupt_until_no_ready_child_remains`) — interface fidelity. Which option forms select a logical child on an emulating backend, and exact-child consumption: `detcore/src/syscalls/emulated_waitid.rs` (`wait4_terminal_selection_retains_raw_option_validation`, `wait4_completion_publishes_then_consumes_exact_child_once`) — what Hermit does, and interface fidelity. A thread exit's tid-clearing wake as a join edge: `detcore/src/scheduler.rs` (`a_clear_child_tid_wake_is_a_join_edge`) — what Hermit does. Guest programs child_exit_sigchld_wake, clone_exit_signal_effective, and child_wait_deadlock under tests/c exercise reaping, the exit-signal partition, and stuck child waits end to end — what Hermit does, and interface fidelity.

### Citations

[^linux]: Linux `wait4(2)`:
  entry and argument validation: https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/exit.c?h=v7.1.3#n1847
  child scan order: https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/exit.c?h=v7.1.3#n1664
  interrupt after the scan: https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/exit.c?h=v7.1.3#n1711
  clone partition: https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/exit.c?h=v7.1.3#n1142
  exec resetting the exit signal: https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/fs/exec.c?h=v7.1.3#n1021
  parent-cloned child's exit signal: https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/fork.c?h=v7.1.3#n2414
  zombie reaping and resource usage: https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/exit.c?h=v7.1.3#n1174
  stop-report resource usage: https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/exit.c?h=v7.1.3#n1380
  continued-report resource usage: https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/exit.c?h=v7.1.3#n1430
  thread-exit tid clearing: https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/fork.c?h=v7.1.3#n1463
  thread-exit PI release before tid clearing: https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/fork.c?h=v7.1.3#n1496
[^c1]: detcore/src/syscall_classification.rs:1991 sha256:bd41fb7f032509db
[^c2]: detcore/src/lib.rs:8219 sha256:ab9210156500ef72
[^c3]: detcore/src/syscalls/threads.rs:566-586 sha256:a0d560fe77e776fc
[^c4]: detcore/src/syscalls/threads.rs:547-565 sha256:ef259099538fd9e2
[^c5]: detcore/src/syscalls/threads.rs:588-611 sha256:3a225cc060f102d4
[^c6]: detcore/src/syscalls/threads.rs:5831-5903 sha256:e5a37c324c6c40d0
[^c7]: detcore/src/syscalls/threads.rs:5905-5960 sha256:f6ec44b4d98e35d9
[^c8]: detcore/src/syscalls/threads.rs:5976-6061 sha256:944dce7914c89fad
[^c9]: detcore/src/syscalls/threads.rs:6062-6112 sha256:72cea24a3dc48fac
[^c10]: detcore/src/syscalls/helpers.rs:2516-2545 sha256:527a0aea5dede456
[^c11]: detcore/src/scheduler.rs:14650-14666 sha256:0b43c5eec487a536
[^c12]: detcore/src/scheduler.rs:14805-14812 sha256:1993956b8ecae773; detcore/src/scheduler.rs:2269 sha256:19fe6f5fcdadf584
[^c13]: detcore/src/scheduler.rs:3195-3212 sha256:464135068a88eac7; detcore/src/scheduler.rs:3728-3741 sha256:918e3612e0269a65
[^c14]: detcore/src/syscalls/threads.rs:2066-2088 sha256:e9176fb2c1b3864a
[^c15]: detcore/src/syscalls/threads.rs:543-545 sha256:ca18396bb80e954b
[^c16]: detcore/src/scheduler.rs:8183-8191 sha256:0b4a36fb24a0df6b; detcore/src/scheduler.rs:11789-11817 sha256:b057ee6a0a29632f
[^c17]: detcore/src/syscalls/threads.rs:661-695 sha256:41a9f120df47db2f
[^c18]: detcore/src/syscalls/threads.rs:5808-5829 sha256:d235fedb22a01c29
[^c19]: detcore/src/scheduler.rs:3678-3681 sha256:b1ad854319cf8f35
[^c20]: detcore/src/syscalls/threads.rs:6045-6056 sha256:2a12832777f15ad1; detcore/src/syscalls/threads.rs:5897-5903 sha256:d6cb342e218c8273
[^c21]: detcore/src/syscalls/threads.rs:597-598 sha256:df348f129d387972; detcore/src/syscalls/threads.rs:566-586 sha256:a0d560fe77e776fc; detcore/src/syscalls/threads.rs:6062-6065 sha256:6c36a4fdca84ea9a
[^c22]: detcore/src/syscalls/sysinfo.rs:397-402 sha256:c7eb7e5a7a3a8cd2; detcore/src/syscalls/sysinfo.rs:418-419 sha256:9c1538362815803f; detcore/src/syscalls/sysinfo.rs:436-441 sha256:c589192e7b94c957; detcore/src/syscalls/sysinfo.rs:467-485 sha256:4c642081e708dfad
[^c23]: detcore/src/syscalls/threads.rs:5878-5881 sha256:89f8f4c8f4d2d8f6; detcore/src/syscalls/helpers.rs:2526-2531 sha256:ae20d6301c7b5ec1

## Status

- 2026-11-15 · rev 54bd03547df0 · facts-only page generated from Detcore code facts at this revision.
- 2026-11-16 · rev 7ac736d54e14 · facts refreshed from Detcore at this revision.
- 2026-11-18 · rev 403e350e9e82 · facts refreshed from Detcore at this revision.
- 2026-11-20 · rev 403e350e9e82 · checked determinism-strategy prose added (rubric v3); citations verified at this revision.
- 2026-11-20 · rev 403e350e9e82 · prose corrected after seat review; citations re-verified at this revision.
